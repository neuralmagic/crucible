//! Admission ledger: a durable, idempotent record of every external input into a run.
//! Nothing mutates the run until its `Admitted` line is fsync'd (except stop/abort);
//! same key + same payload converges, different payload conflicts; first `Settled` wins.

use anyhow::{Context, Result};
use crucible_contract::admission::{
    AdmissionEvent, AdmissionKey, AdmissionOutcome, AdmittedInput, SteerSource, decode, encode,
};
use forge::ndjson::{Durability, Ledger, Open};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

/// What [`AdmissionLedger::admit`] did with an input.
#[derive(Debug, PartialEq)]
pub(crate) enum Admitted {
    /// Recorded for the first time: the caller applies the effect.
    Fresh(AdmissionKey),
    /// Exact replay; nothing written, the effect must NOT be re-applied.
    Duplicate(AdmissionKey, Option<AdmissionOutcome>),
    /// Same key, different payload: rejected, nothing written.
    Conflict(AdmissionKey),
}

struct Record {
    key: AdmissionKey,
    input: AdmittedInput,
    /// Canonical JSON of `input`, the conflict comparand.
    payload: String,
    settled: Option<AdmissionOutcome>,
}

#[derive(Debug, thiserror::Error)]
#[error("admission ledger lock poisoned")]
struct LedgerLockPoisoned;

struct Inner {
    file: Ledger,
    next_seq: u64,
    log: Vec<Record>,
    index: HashMap<AdmissionKey, usize>,
    /// Torn lines skipped at open, and where an unreadable file was moved.
    skipped: usize,
    quarantined: Option<PathBuf>,
}

/// Shared between the bridge threads and the loop thread.
pub(crate) struct AdmissionLedger {
    inner: Mutex<Inner>,
}

impl AdmissionLedger {
    /// A fresh run truncates (it must not inherit the last run's un-drained inputs);
    /// a resumed run folds what is there.
    pub(crate) fn open(path: &Path, mode: Open) -> Result<Self> {
        let (file, folded) = Ledger::open(path, mode, Durability::Fsync, decode)
            .with_context(|| format!("opening admission ledger {}", path.display()))?;
        let mut inner = Inner {
            file,
            next_seq: 0,
            log: Vec::new(),
            index: HashMap::new(),
            skipped: folded.skipped,
            quarantined: folded.quarantined,
        };
        for ev in folded.records {
            inner.fold(ev);
        }
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    /// Durably record an input BEFORE it takes effect; a duplicate or conflict writes
    /// nothing.
    pub(crate) fn admit(
        &self,
        key: Option<AdmissionKey>,
        input: AdmittedInput,
    ) -> Result<Admitted> {
        let payload =
            serde_json::to_string(&input).context("encoding an admitted input's payload")?;
        let key = key.unwrap_or_else(generated_key);
        let mut g = self.lock()?;
        if let Some(&at) = g.index.get(&key) {
            let existing = &g.log[at];
            return Ok(if existing.payload == payload {
                Admitted::Duplicate(key, existing.settled)
            } else {
                Admitted::Conflict(key)
            });
        }
        let seq = g.next_seq;
        let line = encode(&AdmissionEvent::Admitted {
            key: key.clone(),
            seq,
            ts: now_secs(),
            input: input.clone(),
        });
        // Register in memory only after the append lands: a failed write must leave
        // no trace.
        g.file
            .append(&line)
            .with_context(|| format!("appending admission {key}"))?;
        g.next_seq = seq + 1;
        let at = g.log.len();
        g.index.insert(key.clone(), at);
        g.log.push(Record {
            key: key.clone(),
            input,
            payload,
            settled: None,
        });
        Ok(Admitted::Fresh(key))
    }

    /// First terminal wins: settling a settled key writes nothing and returns the
    /// outcome that stands. `None` means the key was never admitted.
    pub(crate) fn settle(
        &self,
        key: &AdmissionKey,
        outcome: AdmissionOutcome,
        note: &str,
    ) -> Result<Option<AdmissionOutcome>> {
        let mut g = self.lock()?;
        let Some(&at) = g.index.get(key) else {
            return Ok(None);
        };
        if let Some(existing) = g.log[at].settled {
            return Ok(Some(existing));
        }
        let line = encode(&AdmissionEvent::Settled {
            key: key.clone(),
            outcome,
            ts: now_secs(),
            note: note.to_string(),
        });
        g.file
            .append(&line)
            .with_context(|| format!("appending settlement of {key}"))?;
        g.log[at].settled = Some(outcome);
        Ok(Some(outcome))
    }

    /// Settle each key, swallowing IO errors: a lost settle only re-arms the input on
    /// resume, so it must never fail the loop.
    pub(crate) fn settle_all(&self, keys: &[AdmissionKey], outcome: AdmissionOutcome, note: &str) {
        for key in keys {
            let _ = self.settle(key, outcome, note);
        }
    }

    /// Unsettled steers in admission order, WITHOUT settling: the loop settles only once
    /// the turn that carried the text has run, so a death mid-turn re-delivers on resume.
    pub(crate) fn peek_steers(&self) -> Vec<(AdmissionKey, String)> {
        let Ok(g) = self.lock() else {
            return Vec::new();
        };
        g.log
            .iter()
            .filter(|r| r.settled.is_none())
            .filter_map(|r| match &r.input {
                AdmittedInput::Steer { text, .. } => Some((r.key.clone(), text.clone())),
                _ => None,
            })
            .collect()
    }

    /// Levels to re-establish, edges to re-arm, stale inputs a resume overrides.
    pub(crate) fn replay_for_resume(&self) -> ResumeReplay {
        let Ok(g) = self.lock() else {
            return ResumeReplay::default();
        };
        let mut replay = ResumeReplay {
            skipped_lines: g.skipped,
            quarantined: g.quarantined.clone(),
            ..ResumeReplay::default()
        };
        for r in &g.log {
            let unsettled = r.settled.is_none();
            match &r.input {
                // Levels: the last one admitted stands, settled or not.
                AdmittedInput::SetBudget { usd } => replay.last_budget = Some(*usd),
                AdmittedInput::Pause => replay.paused = true,
                AdmittedInput::Resume => replay.paused = false,
                // Edges: only an un-drained one is still owed.
                AdmittedInput::Steer { .. } if unsettled => replay.steers_pending += 1,
                AdmittedInput::Rescope { regime } if unsettled => {
                    replay.unsettled_rescope = Some((r.key.clone(), regime.clone()));
                }
                AdmittedInput::Deny { reason } if unsettled => {
                    replay.unsettled_deny = Some((r.key.clone(), reason.clone()));
                }
                // A resume overrides a stop; an un-granted approve must be re-sent.
                AdmittedInput::Stop | AdmittedInput::Abort if unsettled => {
                    replay.stale_stops.push(r.key.clone());
                }
                AdmittedInput::Approve if unsettled => replay.stale_approves.push(r.key.clone()),
                _ => {}
            }
        }
        replay
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        // A poisoned lock means a prior holder panicked mid-fold; the ledger's in-memory view
        // can no longer be trusted, so every later call fails the same way.
        self.inner.lock().map_err(|_| LedgerLockPoisoned.into())
    }
}

impl Inner {
    fn fold(&mut self, ev: AdmissionEvent) {
        match ev {
            AdmissionEvent::Admitted {
                key, seq, input, ..
            } => {
                self.next_seq = self.next_seq.max(seq + 1);
                // Two Admitted lines per key only happen in a hand-edited file; the
                // first wins.
                if self.index.contains_key(&key) {
                    return;
                }
                let payload = serde_json::to_string(&input).unwrap_or_default();
                self.index.insert(key.clone(), self.log.len());
                self.log.push(Record {
                    key,
                    input,
                    payload,
                    settled: None,
                });
            }
            // A settlement with no admission (truncated head) has nothing to close.
            AdmissionEvent::Settled { key, outcome, .. } => {
                if let Some(&at) = self.index.get(&key) {
                    self.log[at].settled.get_or_insert(outcome);
                }
            }
        }
    }
}

/// What a resumed run owes the operator, folded out of the ledger.
#[derive(Debug, Default)]
pub(crate) struct ResumeReplay {
    /// Live budget override in force at the death.
    pub last_budget: Option<f64>,
    pub paused: bool,
    /// Re-scope granted but never drained by an iteration head.
    pub unsettled_rescope: Option<(AdmissionKey, String)>,
    /// Denial that never reached a park or an iteration head.
    pub unsettled_deny: Option<(AdmissionKey, String)>,
    /// Steers admitted but never delivered into a turn.
    pub steers_pending: usize,
    /// Stops/aborts the resume overrides.
    pub stale_stops: Vec<AdmissionKey>,
    /// Approves that never converted into a grant; the operator re-approves.
    pub stale_approves: Vec<AdmissionKey>,
    /// Torn lines skipped when the file was folded.
    pub skipped_lines: usize,
    /// Where an unreadable ledger was moved.
    pub quarantined: Option<PathBuf>,
}

/// Steer text to render, plus the keys that settle once the turn it feeds has run.
#[derive(Debug, Default)]
pub(crate) struct SteerBatch {
    pub keys: Vec<AdmissionKey>,
    pub text: Option<String>,
}

/// Admit whatever sits in `STEER.md`, then return every un-delivered steer as one batch.
/// Without a ledger this is the historical read-and-blank.
pub(crate) fn drain_steer(ledger: Option<&AdmissionLedger>, steer_path: &Path) -> SteerBatch {
    let file_text = take_steer_file(steer_path);
    let Some(ledger) = ledger else {
        return SteerBatch {
            keys: Vec::new(),
            text: file_text,
        };
    };
    if let Some(text) = file_text {
        let admitted = ledger.admit(
            None,
            AdmittedInput::Steer {
                text: text.clone(),
                from: SteerSource::SteerFile,
            },
        );
        // The file is already blanked; deliver un-recorded rather than dropping the text.
        if admitted.is_err() {
            let mut batch = pending_batch(ledger);
            batch.text = Some(match batch.text {
                Some(pending) => format!("{pending}\n{text}"),
                None => text,
            });
            return batch;
        }
    }
    pending_batch(ledger)
}

fn pending_batch(ledger: &AdmissionLedger) -> SteerBatch {
    let pending = ledger.peek_steers();
    let text = (!pending.is_empty()).then(|| {
        pending
            .iter()
            .map(|(_, text)| text.trim())
            .collect::<Vec<_>>()
            .join("\n")
    });
    SteerBatch {
        keys: pending.into_iter().map(|(key, _)| key).collect(),
        text,
    }
}

fn take_steer_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    let _ = std::fs::write(path, "");
    Some(text)
}

/// v7 is timestamp-ordered, so a ledger read by eye stays in admission order.
fn generated_key() -> AdmissionKey {
    AdmissionKey::new(uuid::Uuid::now_v7().to_string())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmp(name: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "crucible-admission-{}-{n}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("admissions.jsonl")
    }

    fn steer(text: &str) -> AdmittedInput {
        AdmittedInput::Steer {
            text: text.into(),
            from: SteerSource::Operator,
        }
    }

    fn fresh(admitted: Admitted) -> AdmissionKey {
        match admitted {
            Admitted::Fresh(key) => key,
            other => panic!("expected a fresh admission, got {other:?}"),
        }
    }

    fn lines(path: &Path) -> Vec<String> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn a_fresh_admission_writes_one_line_and_survives_a_reopen() {
        let path = tmp("reopen");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        let key = fresh(l.admit(None, steer("cache first")).unwrap());
        assert_eq!(lines(&path).len(), 1);
        drop(l);

        let l = AdmissionLedger::open(&path, Open::Fold).unwrap();
        assert_eq!(
            l.peek_steers(),
            vec![(key.clone(), "cache first".to_string())]
        );
        // The fold restored the index too: the same key + payload converges.
        assert_eq!(
            l.admit(Some(key.clone()), steer("cache first")).unwrap(),
            Admitted::Duplicate(key, None)
        );
        assert_eq!(lines(&path).len(), 1, "a duplicate writes nothing");
    }

    #[test]
    fn a_truncating_open_drops_the_previous_runs_inputs() {
        let path = tmp("truncate");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        l.admit(None, steer("old")).unwrap();
        drop(l);
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        assert!(l.peek_steers().is_empty());
        assert!(lines(&path).is_empty());
    }

    #[test]
    fn same_key_different_payload_is_a_conflict_and_writes_nothing() {
        let path = tmp("conflict");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        let key = AdmissionKey::new("pr-comment:o/r#7:1");
        l.admit(Some(key.clone()), steer("first")).unwrap();
        assert_eq!(
            l.admit(Some(key.clone()), steer("different")).unwrap(),
            Admitted::Conflict(key)
        );
        assert_eq!(lines(&path).len(), 1);
        assert_eq!(l.peek_steers().len(), 1, "the original stands");
    }

    #[test]
    fn a_duplicate_reports_the_settled_outcome() {
        let path = tmp("dup-outcome");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        let key = AdmissionKey::new("k");
        l.admit(Some(key.clone()), steer("go")).unwrap();
        l.settle(&key, AdmissionOutcome::Applied, "delivered in iter 1")
            .unwrap();
        assert_eq!(
            l.admit(Some(key.clone()), steer("go")).unwrap(),
            Admitted::Duplicate(key, Some(AdmissionOutcome::Applied))
        );
    }

    #[test]
    fn the_first_terminal_outcome_wins() {
        let path = tmp("first-terminal");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        let key = AdmissionKey::new("k");
        l.admit(Some(key.clone()), steer("go")).unwrap();
        assert_eq!(
            l.settle(&key, AdmissionOutcome::Applied, "one").unwrap(),
            Some(AdmissionOutcome::Applied)
        );
        assert_eq!(
            l.settle(&key, AdmissionOutcome::Superseded, "two").unwrap(),
            Some(AdmissionOutcome::Applied),
            "the second settle is a no-op returning the first outcome"
        );
        assert_eq!(lines(&path).len(), 2, "one admitted + one settled");
        assert!(l.peek_steers().is_empty(), "settled steers stop pending");
    }

    #[test]
    fn settling_a_key_that_was_never_admitted_is_a_no_op() {
        let path = tmp("unknown-key");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        assert_eq!(
            l.settle(&AdmissionKey::new("ghost"), AdmissionOutcome::Applied, "no")
                .unwrap(),
            None
        );
        assert!(lines(&path).is_empty());
    }

    #[test]
    fn peek_preserves_admission_order_across_a_reopen() {
        let path = tmp("order");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        for t in ["one", "two", "three"] {
            l.admit(None, steer(t)).unwrap();
        }
        drop(l);
        let l = AdmissionLedger::open(&path, Open::Fold).unwrap();
        let texts: Vec<String> = l.peek_steers().into_iter().map(|(_, t)| t).collect();
        assert_eq!(texts, vec!["one", "two", "three"]);
    }

    #[test]
    fn replay_reports_levels_edges_and_stale_inputs() {
        let path = tmp("replay");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        l.admit(None, steer("undelivered")).unwrap();
        let delivered = fresh(l.admit(None, steer("delivered")).unwrap());
        l.settle(&delivered, AdmissionOutcome::Applied, "iter 1")
            .unwrap();
        l.admit(None, AdmittedInput::SetBudget { usd: 1.5 })
            .unwrap();
        l.admit(None, AdmittedInput::SetBudget { usd: 4.0 })
            .unwrap();
        l.admit(None, AdmittedInput::Pause).unwrap();
        let old_rescope = fresh(
            l.admit(
                Some(AdmissionKey::new("r1")),
                AdmittedInput::Rescope {
                    regime: "c=24".into(),
                },
            )
            .unwrap(),
        );
        l.settle(&old_rescope, AdmissionOutcome::Superseded, "replaced by r2")
            .unwrap();
        l.admit(
            Some(AdmissionKey::new("r2")),
            AdmittedInput::Rescope {
                regime: "c=48".into(),
            },
        )
        .unwrap();
        l.admit(Some(AdmissionKey::new("s1")), AdmittedInput::Stop)
            .unwrap();
        l.admit(Some(AdmissionKey::new("a1")), AdmittedInput::Approve)
            .unwrap();
        drop(l);

        let l = AdmissionLedger::open(&path, Open::Fold).unwrap();
        let replay = l.replay_for_resume();
        assert_eq!(replay.last_budget, Some(4.0), "the last level wins");
        assert!(replay.paused);
        assert_eq!(
            replay.unsettled_rescope,
            Some((AdmissionKey::new("r2"), "c=48".to_string())),
            "the superseded one is closed; only r2 is owed"
        );
        assert_eq!(replay.unsettled_deny, None);
        assert_eq!(replay.steers_pending, 1);
        assert_eq!(replay.stale_stops, vec![AdmissionKey::new("s1")]);
        assert_eq!(replay.stale_approves, vec![AdmissionKey::new("a1")]);
        assert_eq!(replay.skipped_lines, 0);
    }

    #[test]
    fn a_torn_tail_is_skipped_and_counted_on_the_fold() {
        let path = tmp("torn");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        l.admit(None, steer("whole")).unwrap();
        drop(l);
        // The process died mid-write of the second record.
        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str(r#"{"v":1,"kind":"admitted","key":"half"#);
        std::fs::write(&path, body).unwrap();

        let l = AdmissionLedger::open(&path, Open::Fold).unwrap();
        assert_eq!(l.peek_steers().len(), 1);
        assert_eq!(l.replay_for_resume().skipped_lines, 1);
        // The healed tail means the next admission is a whole line again.
        l.admit(None, steer("after")).unwrap();
        drop(l);
        let l = AdmissionLedger::open(&path, Open::Fold).unwrap();
        assert_eq!(l.peek_steers().len(), 2);
    }

    #[test]
    fn the_steer_file_is_admitted_then_blanked_and_the_batch_carries_every_pending_key() {
        let path = tmp("drain");
        let steer_path = path.with_file_name("STEER.md");
        let l = AdmissionLedger::open(&path, Open::Truncate).unwrap();
        // One steer from the bridge, one sitting in the file.
        l.admit(None, steer("from the bridge")).unwrap();
        std::fs::write(&steer_path, "from the file\n").unwrap();

        let batch = drain_steer(Some(&l), &steer_path);
        assert_eq!(batch.keys.len(), 2);
        assert_eq!(
            batch.text.as_deref(),
            Some("from the bridge\nfrom the file")
        );
        assert_eq!(
            std::fs::read_to_string(&steer_path).unwrap(),
            "",
            "the file channel is consumed"
        );

        // Un-settled, so a second drain re-delivers (a turn that never started must not
        // lose the operator's text).
        let again = drain_steer(Some(&l), &steer_path);
        assert_eq!(again.keys, batch.keys);
        l.settle_all(&batch.keys, AdmissionOutcome::Applied, "iter 1");
        assert!(drain_steer(Some(&l), &steer_path).text.is_none());
    }

    #[test]
    fn without_a_ledger_the_drain_is_the_historical_read_and_blank() {
        let path = tmp("no-ledger");
        let steer_path = path.with_file_name("STEER.md");
        std::fs::write(&steer_path, "plain text\n").unwrap();
        let batch = drain_steer(None, &steer_path);
        assert!(batch.keys.is_empty());
        assert_eq!(batch.text.as_deref(), Some("plain text\n"));
        assert!(drain_steer(None, &steer_path).text.is_none());
    }
}
