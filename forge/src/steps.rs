//! Durable tool steps: content-keyed records of expensive work that already finished.
//! At-least-once-executed, exactly-once-recorded: bodies must be safe to repeat; only
//! settled values are recorded, never claims about mutable state. The file is a cache.

use crate::ndjson::{Durability, Ledger, fold};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Bumped only by a breaking change to [`StepRecord`]; newer records are skipped
/// (re-executing is safe, mis-reading is not).
pub const STEP_WIRE_VERSION: u8 = 1;

const LEDGER_FILE: &str = "steps.jsonl";

/// `step` carries the content key of the work, so changed inputs never replay stale
/// results; `scope` bounds lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StepKey {
    pub scope: String,
    pub step: String,
}

impl StepKey {
    pub fn new(scope: impl Into<String>, step: impl Into<String>) -> Self {
        Self {
            scope: scope.into(),
            step: step.into(),
        }
    }
}

/// Values are small: a large artifact is recorded as a pointer (log handle, digest),
/// never inline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    #[serde(default = "default_version")]
    pub v: u8,
    #[serde(flatten)]
    pub key: StepKey,
    pub value: Value,
    /// Epoch seconds, informational; ordering comes from the file.
    pub recorded_at: u64,
}

fn default_version() -> u8 {
    STEP_WIRE_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome<T> {
    pub value: T,
    /// Came from the ledger: skip charging a budget for work that did not happen.
    pub replayed: bool,
}

/// Holds no open handle and no in-memory state: every lookup re-reads the file, so a
/// record written by a different process is visible to this one.
pub struct StepLedger {
    path: PathBuf,
}

impl StepLedger {
    /// Touches the filesystem only on the first record; an uncreatable directory degrades
    /// to "every step executes".
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join(LEDGER_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `None` when absent or no longer decoding into `T` (the work must be redone).
    pub fn lookup<T: DeserializeOwned>(&self, key: &StepKey) -> Option<T> {
        let value = self.folded().remove(key)?;
        serde_json::from_value(value).ok()
    }

    /// Every value under `scope`, last-wins, ordered by step name.
    pub fn scan<T: DeserializeOwned>(&self, scope: &str) -> Vec<T> {
        self.folded()
            .into_iter()
            .filter(|(k, _)| k.scope == scope)
            .filter_map(|(_, v)| serde_json::from_value(v).ok())
            .collect()
    }

    /// Append one record. Errors are the caller's to log, never to fail work over.
    pub fn record<T: Serialize>(&self, key: &StepKey, value: &T) -> Result<()> {
        let record = StepRecord {
            v: STEP_WIRE_VERSION,
            key: key.clone(),
            value: serde_json::to_value(value).context("encoding a step value")?,
            recorded_at: now_secs(),
        };
        let line = serde_json::to_string(&record).context("encoding a step record")?;
        let mut ledger = Ledger::append_only(&self.path, Durability::Fsync)
            .with_context(|| format!("opening step ledger {}", self.path.display()))?;
        ledger
            .append(&line)
            .with_context(|| format!("appending to step ledger {}", self.path.display()))
    }

    /// Replay the recorded value for `key`, or run `f` and record its `Ok`. An `Err`
    /// records nothing; a failed append still returns the value (bookkeeping never fails
    /// work).
    pub fn run<T, E>(
        &self,
        key: &StepKey,
        f: impl FnOnce() -> Result<T, E>,
    ) -> Result<StepOutcome<T>, E>
    where
        T: Serialize + DeserializeOwned,
    {
        if let Some(value) = self.lookup::<T>(key) {
            return Ok(StepOutcome {
                value,
                replayed: true,
            });
        }
        let value = f()?;
        if let Err(e) = self.record(key, &value) {
            eprintln!("warning: recording step {:?} failed: {e:#}", key.step);
        }
        Ok(StepOutcome {
            value,
            replayed: false,
        })
    }

    /// Last-wins fold; a future-version record is skipped (re-execute, don't guess).
    fn folded(&self) -> BTreeMap<StepKey, Value> {
        let mut map = BTreeMap::new();
        for record in fold(&self.path, decode).records {
            map.insert(record.key, record.value);
        }
        map
    }
}

fn decode(line: &str) -> Option<StepRecord> {
    let record: StepRecord = serde_json::from_str(line.trim()).ok()?;
    (record.v <= STEP_WIRE_VERSION).then_some(record)
}

/// sha256 over `parts` with a NUL separator, truncated to 16 hex chars. Stable across
/// processes and toolchains, unlike `DefaultHasher`.
pub fn fingerprint(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for part in parts {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())[..16].to_string()
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

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case")]
    enum Outcome {
        Built { digest: String },
        CompileError { log: String },
    }

    fn tmp(name: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("forge-steps-{}-{}-{name}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn key(step: &str) -> StepKey {
        StepKey::new("broker", step)
    }

    #[test]
    fn a_recorded_value_is_looked_up_by_identity() {
        let l = StepLedger::new(&tmp("roundtrip"));
        let built = Outcome::Built {
            digest: "sha256:aa".into(),
        };
        l.record(&key("build|t1"), &built).unwrap();
        assert_eq!(l.lookup::<Outcome>(&key("build|t1")), Some(built));
        assert_eq!(l.lookup::<Outcome>(&key("build|t2")), None);
    }

    /// A different process wrote the record; this one replays it without a handle in common.
    #[test]
    fn a_second_ledger_over_the_same_dir_sees_the_record() {
        let dir = tmp("cross-process");
        StepLedger::new(&dir)
            .record(
                &key("bench|sha256:aa"),
                &BTreeMap::from([("tps".to_string(), 12.5)]),
            )
            .unwrap();
        let metrics: BTreeMap<String, f64> = StepLedger::new(&dir)
            .lookup(&key("bench|sha256:aa"))
            .expect("recorded by the dead process");
        assert_eq!(metrics["tps"], 12.5);
    }

    #[test]
    fn the_last_record_for_an_identity_wins() {
        let l = StepLedger::new(&tmp("last-wins"));
        l.record(&key("k"), &"first").unwrap();
        l.record(&key("k"), &"second").unwrap();
        assert_eq!(l.lookup::<String>(&key("k")).as_deref(), Some("second"));
    }

    #[test]
    fn run_executes_once_and_replays_after() {
        let l = StepLedger::new(&tmp("replay"));
        let mut runs = 0;
        let first = l
            .run(&key("build|t1"), || {
                runs += 1;
                Ok::<_, anyhow::Error>(Outcome::Built {
                    digest: "sha256:aa".into(),
                })
            })
            .unwrap();
        assert!(!first.replayed);
        let second = l
            .run(&key("build|t1"), || -> Result<Outcome> {
                runs += 1;
                panic!("a replayed step must not run its body")
            })
            .unwrap();
        assert!(second.replayed);
        assert_eq!(first.value, second.value);
        assert_eq!(runs, 1);
    }

    /// A changed content key is a different step: no replay, and both records survive.
    #[test]
    fn a_changed_content_key_executes_fresh() {
        let l = StepLedger::new(&tmp("changed-key"));
        let mut runs = 0;
        for step in ["build|t1", "build|t2"] {
            l.run(&key(step), || {
                runs += 1;
                Ok::<_, anyhow::Error>(Outcome::Built {
                    digest: format!("sha256:{step}"),
                })
            })
            .unwrap();
        }
        assert_eq!(runs, 2);
        assert_eq!(
            l.lookup::<Outcome>(&key("build|t1")),
            Some(Outcome::Built {
                digest: "sha256:build|t1".into()
            })
        );
    }

    /// A deterministic *measured* failure is a fact of the identity and replays; the caller
    /// decides what is settled by choosing what it returns as `Ok`.
    #[test]
    fn a_settled_failure_value_replays() {
        let l = StepLedger::new(&tmp("compile-error"));
        l.run(&key("build|broken"), || {
            Ok::<_, anyhow::Error>(Outcome::CompileError {
                log: "E0432".into(),
            })
        })
        .unwrap();
        let replay = l
            .run(&key("build|broken"), || -> Result<Outcome> {
                panic!("a replayed step must not run its body")
            })
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(
            replay.value,
            Outcome::CompileError {
                log: "E0432".into()
            }
        );
    }

    #[test]
    fn an_err_records_nothing_and_re_executes() {
        let l = StepLedger::new(&tmp("err"));
        let mut runs = 0;
        let err = l
            .run(&key("build|t1"), || -> Result<Outcome> {
                runs += 1;
                anyhow::bail!("sandbox download failed")
            })
            .unwrap_err();
        assert!(err.to_string().contains("sandbox download"));
        assert_eq!(l.lookup::<Outcome>(&key("build|t1")), None);
        let out = l
            .run(&key("build|t1"), || {
                runs += 1;
                Ok::<_, anyhow::Error>(Outcome::Built {
                    digest: "sha256:aa".into(),
                })
            })
            .unwrap();
        assert!(!out.replayed);
        assert_eq!(runs, 2);
    }

    #[test]
    fn a_torn_tail_is_skipped_and_the_next_record_lands_whole() {
        let dir = tmp("torn");
        let l = StepLedger::new(&dir);
        l.record(&key("a"), &"one").unwrap();
        // The process died mid-append.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(l.path())
            .unwrap();
        use std::io::Write;
        file.write_all(br#"{"v":1,"scope":"broker","step":"b","valu"#)
            .unwrap();
        drop(file);

        assert_eq!(l.lookup::<String>(&key("a")).as_deref(), Some("one"));
        assert_eq!(l.lookup::<String>(&key("b")), None);
        l.record(&key("c"), &"three").unwrap();
        assert_eq!(l.lookup::<String>(&key("c")).as_deref(), Some("three"));
        assert_eq!(l.lookup::<String>(&key("a")).as_deref(), Some("one"));
    }

    #[test]
    fn a_record_from_a_future_version_is_ignored() {
        let dir = tmp("future");
        let l = StepLedger::new(&dir);
        std::fs::write(
            l.path(),
            format!(
                "{{\"v\":{},\"scope\":\"broker\",\"step\":\"k\",\"value\":\"x\",\"recorded_at\":0}}\n",
                STEP_WIRE_VERSION + 1
            ),
        )
        .unwrap();
        assert_eq!(
            l.lookup::<String>(&key("k")),
            None,
            "re-execute, don't guess"
        );
    }

    #[test]
    fn scan_returns_only_its_own_scope() {
        let dir = tmp("scope");
        let l = StepLedger::new(&dir);
        l.record(&StepKey::new("broker", "a"), &"mine").unwrap();
        l.record(&StepKey::new("other", "b"), &"theirs").unwrap();
        assert_eq!(l.scan::<String>("broker"), vec!["mine".to_string()]);
        assert_eq!(l.scan::<String>("other"), vec!["theirs".to_string()]);
    }

    #[test]
    fn a_missing_ledger_is_a_miss_not_an_error() {
        let l = StepLedger::new(&std::env::temp_dir().join("forge-steps-nonexistent-dir"));
        assert_eq!(l.lookup::<String>(&key("k")), None);
        assert!(l.scan::<String>("broker").is_empty());
    }

    #[test]
    fn fingerprints_are_stable_and_input_sensitive() {
        assert_eq!(fingerprint(&["a", "b"]), fingerprint(&["a", "b"]));
        assert_ne!(fingerprint(&["a", "b"]), fingerprint(&["a", "c"]));
        // The separator is not forgeable by concatenation.
        assert_ne!(fingerprint(&["a", "b"]), fingerprint(&["ab"]));
        assert_eq!(fingerprint(&["a"]).len(), 16);
    }
}
