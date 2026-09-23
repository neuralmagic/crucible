#[cfg(feature = "autoresearch")]
use crate::issues::engine::{self, GroundedVerdict};
use crate::runs::workpod::*;
#[cfg(feature = "autoresearch")]
use anyhow::{Context, Result};
use k8s_openapi::api::core::v1::PodStatus;

/// Scrape the verdict off a turn pod's logs: the last `CRUCIBLE_VERDICT:` marker line, robust to the
/// podman/git chatter ahead of it. The marker's payload is the same verdict JSON the local
/// subprocess arm parses, so an `{"error":…}` object surfaces as `Err` (the caller keeps the text
/// tier) via the shared [`engine::verdict_from_json_line`] decoder.
#[cfg(feature = "autoresearch")]
pub(crate) fn parse_verdict_logs(logs: &str) -> Result<GroundedVerdict> {
    let line = logs
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(VERDICT_MARKER))
        .context("no CRUCIBLE_VERDICT marker line in the turn pod's logs")?;
    engine::verdict_from_json_line(line.trim())
}

/// Scrape the scope report off a scope turn pod's logs: the last `CRUCIBLE_SCOPE_REPORT:` marker
/// line, carrying the ScopeReport JSON. The turn's preserved agent transcript rides its own
/// `CRUCIBLE_SCOPE_TRANSCRIPT:` line (gzip+base64) just before it — attached when present, and
/// strictly best-effort: a missing or garbled transcript never fails the report.
#[cfg(feature = "autoresearch")]
pub(crate) fn parse_scope_report_logs(logs: &str) -> Result<engine::ScopeReport> {
    let line = logs
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(SCOPE_REPORT_MARKER))
        .context("no CRUCIBLE_SCOPE_REPORT marker line in the scope turn pod's logs")?;
    let mut report: engine::ScopeReport =
        serde_json::from_str(line.trim()).context("parsing scope report from pod logs")?;
    report.raw = line.trim().to_string();
    report.transcript_gz = parse_scope_transcript_logs(logs);
    (report.pack_tgz, report.pack_error) = parse_scope_pack_logs(logs);
    Ok(report)
}

/// The pack marker's decoded payload (gzip'd tar of the surviving pack dir), plus the reason it's
/// unusable when it isn't: `(None, None)` = no marker at all (a dead proposal, or a pre-feature
/// engine — the caller decides whether that's fatal), `(Some, None)` = the blob landed,
/// `(None, Some)` = the marker was there but carried an `{"error":…}` payload (an oversize pack)
/// or garbled base64. Unlike the transcript this is never best-effort: a survival whose pack
/// can't be recovered must fail the scope loudly.
#[cfg(feature = "autoresearch")]
pub(crate) fn parse_scope_pack_logs(logs: &str) -> (Option<Vec<u8>>, Option<String>) {
    use base64::Engine as _;
    let Some(payload) = logs
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(SCOPE_PACK_MARKER))
    else {
        return (None, None);
    };
    let payload = payload.trim();
    if payload.starts_with('{') {
        let msg = serde_json::from_str::<serde_json::Value>(payload)
            .ok()
            .and_then(|v| v.get("error")?.as_str().map(str::to_string))
            .unwrap_or_else(|| format!("unrecognized pack marker payload: {payload}"));
        return (None, Some(msg));
    }
    match base64::engine::general_purpose::STANDARD.decode(payload) {
        Ok(bytes) if !bytes.is_empty() => (Some(bytes), None),
        Ok(_) => (
            None,
            Some("the pack marker carried an empty payload".to_string()),
        ),
        Err(e) => (
            None,
            Some(format!("the pack marker failed to base64-decode: {e}")),
        ),
    }
}

/// The transcript marker's decoded payload (gzipped session NDJSON), or `None` when the line is
/// absent (a pre-feature engine, a hand-written pack path) or doesn't base64-decode.
#[cfg(feature = "autoresearch")]
pub(crate) fn parse_scope_transcript_logs(logs: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let payload = logs
        .lines()
        .rev()
        .find_map(|l| l.trim().strip_prefix(SCOPE_TRANSCRIPT_MARKER))?;
    match base64::engine::general_purpose::STANDARD.decode(payload.trim()) {
        Ok(bytes) if !bytes.is_empty() => Some(bytes),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "scope transcript marker line failed to base64-decode; dropping it");
            None
        }
    }
}

/// Defensive cap on a scraped session payload. The kubelet's own log retention (default 10Mi
/// rotation) bounds what `logs` can return well under this; the cap only guards a pathological
/// dispatcher. Over it, the payload's TAIL is kept — the summary/shutdown/pr_links events ingest
/// folds land last, and `parse_session` skips the torn first line.
const RUN_SESSION_SCRAPE_CAP: usize = 64 << 20;

/// The outcome of scraping a finished loop-run pod's stdout for its session log — a HONEST
/// three-way split so the no-session failure path can say WHY, not just "no session material".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSessionScrape {
    /// Session material recovered: the delimited `state/session.jsonl` dump, or (rotation ate it) the
    /// live-teed `{`-prefixed stream lines. Ingest proceeds on this payload.
    Found(String),
    /// The `SESSION (rc=…)` delimiter IS present but nothing publishable followed it and no streamed
    /// lines exist anywhere — the wrapper reached the `cat` but the session file was empty/absent, so
    /// the loop failed BEFORE it wrote a single event. Carries the wrapper's exit code and the last
    /// pre-delimiter log lines: that tail is the actual failure (a crash, a missing manifest), so the
    /// operator reads the park reason instead of doing pod forensics.
    DelimiterButNoSession { rc: Option<i32>, tail: String },
    /// No delimiter reached the retained logs AND no streamed lines — the wrapper died before the
    /// `cat`, or kubelet log rotation truncated everything. Carries the last log lines as evidence.
    NoDelimiter { tail: String },
}

/// How many pre-delimiter / trailing log lines to attach as evidence to a no-session outcome.
const SCRAPE_EVIDENCE_LINES: usize = 10;

/// Scrape a finished loop-run pod's session log off its stdout. The wrapper `cat`s the full
/// `state/session.jsonl` after a [`RUN_SESSION_DELIMITER`] line, so the slice after the LAST
/// delimiter is the authoritative copy (tolerant of podman/broker chatter before it). When the
/// delimiter never made it into the retained logs (kubelet rotation ate it, or the wrapper died
/// before the `cat`), fall back to the `{`-prefixed lines across the whole log: `--ui=stream` tees
/// every session event to stdout live, so a truncated run still ingests what arrived
/// (`parse_session` skips any foreign JSON that sneaks in). When neither yields session material, the
/// result distinguishes a present-but-empty delimiter (loop died before publishing — carry its rc +
/// the pre-delimiter tail) from a wholly absent one (rotation/truncation — carry the trailing tail).
pub(crate) fn extract_run_session_logs(logs: &str) -> RunSessionScrape {
    let delimiter = last_delimiter(logs);
    // The authoritative delimited dump, if it carries JSON lines.
    if let Some((_, start, _)) = delimiter {
        let p = &logs[start..];
        if p.lines().any(|l| l.trim_start().starts_with('{')) {
            return RunSessionScrape::Found(cap_session_tail(p.to_string()));
        }
    }
    // Fallback: the live-teed stream lines across the whole log (survives a lost tail dump).
    let streamed: Vec<&str> = logs
        .lines()
        .filter(|l| l.trim_start().starts_with('{'))
        .collect();
    if !streamed.is_empty() {
        return RunSessionScrape::Found(cap_session_tail(streamed.join("\n")));
    }
    // No session material at all — say which failure it is.
    match delimiter {
        Some((delim_offset, _, line)) => RunSessionScrape::DelimiterButNoSession {
            rc: parse_delimiter_rc(line),
            tail: last_lines(&logs[..delim_offset], SCRAPE_EVIDENCE_LINES),
        },
        None => RunSessionScrape::NoDelimiter {
            tail: last_lines(logs, SCRAPE_EVIDENCE_LINES),
        },
    }
}

/// The engine's own output in a finished pod's log: everything before the last
/// [`RUN_SESSION_DELIMITER`] line, or the whole log when the wrapper never printed one (the
/// drop-box path skips it).
pub(crate) fn engine_log_of(logs: &str) -> &str {
    &logs[..last_delimiter(logs).map_or(logs.len(), |(start, _, _)| start)]
}

/// The LAST session-delimiter line in `logs`: its start offset, the offset just past it (where the
/// dumped payload begins), and the trimmed line itself.
fn last_delimiter(logs: &str) -> Option<(usize, usize, &str)> {
    let mut found = None;
    let mut offset = 0;
    for line in logs.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with("===") && t.contains(RUN_SESSION_DELIMITER) {
            found = Some((offset, offset + line.len(), t));
        }
        offset += line.len();
    }
    found
}

/// The wrapper's exit code out of a `=== SESSION (rc=N) ===` delimiter line, `None` if it's malformed.
fn parse_delimiter_rc(line: &str) -> Option<i32> {
    let after = line.split_once("rc=")?.1;
    let digits: String = after
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    digits.parse().ok()
}

/// The last `n` non-empty lines of `text`, joined with newlines (evidence for a no-session park).
fn last_lines(text: &str, n: usize) -> String {
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n);
    lines.drain(..start);
    lines.join("\n")
}

/// Keep at most [`RUN_SESSION_SCRAPE_CAP`] bytes of a payload, from the tail, cut on a line
/// boundary (the next `\n` after the byte cut, so no torn UTF-8 and at most one lost line).
fn cap_session_tail(payload: String) -> String {
    if payload.len() <= RUN_SESSION_SCRAPE_CAP {
        return payload;
    }
    let mut cut = payload.len() - RUN_SESSION_SCRAPE_CAP;
    while !payload.is_char_boundary(cut) {
        cut += 1;
    }
    if let Some(i) = payload[cut..].find('\n') {
        cut += i + 1;
    }
    payload[cut..].to_string()
}

/// A turn pod's terminal phase, as the dispatcher observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnPhase {
    Succeeded,
    Failed,
    TimedOut,
}

/// The terminal outcome [`PodDispatcher::await_terminal`] observed: the pod's [`TurnPhase`] plus the
/// kubelet-captured termination message when present. Turn pods are single-container, so `message` is
/// that container's `state.terminated.message`; it is `None` on a timeout, on an OOM/eviction that
/// wrote nothing, or from an old engine image that only emits the marker (the controller then falls
/// back to the marker scrape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalState {
    pub(crate) phase: TurnPhase,
    pub(crate) message: Option<String>,
}

impl TerminalState {
    /// The kubelet termination envelope, available only for a terminal pod.
    #[cfg(feature = "autoresearch")]
    pub(crate) fn termination_message(&self) -> Option<&str> {
        match self.phase {
            TurnPhase::TimedOut => None,
            TurnPhase::Succeeded | TurnPhase::Failed => self.message.as_deref(),
        }
    }

    /// The current container-waiting reason, available only while the pod is non-terminal.
    pub(crate) fn waiting_detail(&self) -> Option<&str> {
        match self.phase {
            TurnPhase::TimedOut => self.message.as_deref(),
            TurnPhase::Succeeded | TurnPhase::Failed => None,
        }
    }
}

/// The kubelet-captured termination message from pod status. Turn pods are single-container, so this
/// reads the first container status carrying a terminated state; an empty message (an OOM/eviction
/// that never wrote the file) collapses to `None`.
pub(crate) fn terminated_message(status: Option<&PodStatus>) -> Option<String> {
    status?
        .container_statuses
        .as_ref()?
        .iter()
        .find_map(|cs| cs.state.as_ref()?.terminated.as_ref()?.message.clone())
        .filter(|m| !m.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_engine_log_is_what_precedes_the_last_delimiter_or_the_whole_log() {
        let logs = format!(
            "podman noise\ngateway_boot: ok\n=== {RUN_SESSION_DELIMITER} (rc=0) ===\n{{\"v\":1}}\n"
        );
        assert_eq!(engine_log_of(&logs), "podman noise\ngateway_boot: ok\n");
        assert_eq!(
            engine_log_of("plan v1: completed\n"),
            "plan v1: completed\n"
        );
        assert_eq!(engine_log_of(""), "");
    }
}
