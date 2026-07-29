use crate::event::AgentEvent;
use crate::session::SessionEvent;
use anyhow::{Context, Result};
use std::io::Write as _;
use std::path::Path;

/// The prefix of the single-line transcript marker `--marker` emits just before the report marker:
/// base64 of the gzipped session NDJSON the propose/refine/adversary turns streamed. Shares the
/// controller's scraper literal via `crucible-contract`.
pub use crucible_contract::SCOPE_TRANSCRIPT_MARKER;

/// Cap on the preserved transcript (uncompressed NDJSON bytes). A marker line must survive the
/// kubelet's per-container log rotation (10 MiB default): 8 MiB of NDJSON gzips well under that,
/// even before base64's 4/3 overhead. Over the cap the middle is dropped, whole lines only, with
/// an honest truncation note left in place (the head keeps the seed context, the tail keeps the
/// rounds that decided the outcome).
pub(super) const TRANSCRIPT_CAP_BYTES: usize = 8 * 1024 * 1024;

/// The engine's own crucible-contract + worked-example assets, embedded so the propose turn's
/// seed context travels with the binary regardless of the invocation cwd or the target repo.
const CONTRACT_MD: &str = include_str!("../../../docs/crucible-contract.md");
const COUNTER_MANIFEST: &str = include_str!("../../../examples/counter/crucible.toml");
const COUNTER_MEASURE: &str = include_str!("../../../examples/counter/measure.nu");

pub(super) fn write_seed_context(scratch: &Path, goal: &str) -> Result<()> {
    let ctx_dir = scratch.join("_scope_context");
    let examples_dir = ctx_dir.join("examples/counter");
    std::fs::create_dir_all(&examples_dir)
        .with_context(|| format!("creating {}", ctx_dir.display()))?;
    std::fs::write(ctx_dir.join("GOAL.md"), goal)?;
    std::fs::write(ctx_dir.join("crucible-contract.md"), CONTRACT_MD)?;
    std::fs::write(examples_dir.join("crucible.toml"), COUNTER_MANIFEST)?;
    std::fs::write(examples_dir.join("measure.nu"), COUNTER_MEASURE)?;
    Ok(())
}

pub(super) fn transcript_note(session: &mut String, msg: &str) {
    push_session_line(
        session,
        &SessionEvent::Note {
            msg: msg.to_string(),
        },
    );
}

/// Append one decoded agent event to the preserved transcript, nested exactly as the loop's
/// session log nests it (`{"kind":"agent","event":{…}}`).
pub(super) fn transcript_event(session: &mut String, ev: &AgentEvent) {
    push_session_line(session, &SessionEvent::Agent { event: ev.clone() });
}

pub(super) fn push_session_line(session: &mut String, ev: &SessionEvent) {
    if let Ok(line) = serde_json::to_string(ev) {
        session.push_str(&line);
        session.push('\n');
    }
}

/// Enforce [`TRANSCRIPT_CAP_BYTES`] on the preserved NDJSON: whole lines only, keeping the head
/// (the seed context and first rounds) and the tail (the rounds that decided the outcome) with an
/// honest truncation note between them. Returns the capped text and the bytes dropped (0 = intact).
pub(super) fn cap_transcript(ndjson: &str, cap: usize) -> (String, usize) {
    if ndjson.len() <= cap {
        return (ndjson.to_string(), 0);
    }
    let head_budget = cap / 4;
    let tail_budget = cap - head_budget;
    let mut head_end = 0;
    for line in ndjson.split_inclusive('\n') {
        if head_end + line.len() > head_budget {
            break;
        }
        head_end += line.len();
    }
    let mut tail_start = ndjson.len();
    let mut tail_len = 0;
    for line in ndjson.split_inclusive('\n').rev() {
        if tail_len + line.len() > tail_budget || ndjson.len() - (tail_len + line.len()) < head_end
        {
            break;
        }
        tail_len += line.len();
        tail_start -= line.len();
    }
    let dropped = tail_start - head_end;
    let mut capped = String::with_capacity(head_end + tail_len + 128);
    capped.push_str(&ndjson[..head_end]);
    transcript_note(
        &mut capped,
        &format!("transcript truncated: {dropped} bytes dropped mid-session (cap {cap} bytes)"),
    );
    capped.push_str(&ndjson[tail_start..]);
    (capped, dropped)
}

/// Gzip the capped transcript for delivery (a pod-log marker line or `--transcript-out` file).
pub(super) fn gzip_transcript(ndjson: &str) -> Result<Vec<u8>> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(ndjson.as_bytes())
        .context("gzipping the scope transcript")?;
    enc.finish().context("finishing the transcript gzip stream")
}
