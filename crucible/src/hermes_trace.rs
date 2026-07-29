//! Reconstruct one hermes turn's telemetry from its SQLite `state.db`.
//!
//! Hermes has no machine-readable event stream, so its result verdict, cost, tool spans, and
//! conversation records are recovered post-hoc from the session db (`sessions` holds the per-turn
//! rollup, `messages` the conversation). This is the ONLY source of a hermes turn's result + cost,
//! so it is the load-bearing backfill (see `Harness::backfill_required`), but it is still pure
//! telemetry code: every failure degrades to `None`, never a panic or a wrong number. A `None`
//! surfaces upstream as a loud transcript error, never a silent $0 success.
//!
//! The schema is additive-and-self-healing across hermes versions, so columns are read by NAME
//! against `SELECT *` (a dropped/renamed column degrades to its default, not a hard prepare error).

use crate::event::{AgentEvent, estimate_cost};
use crate::turn_trace::{self, GenAiRecord, ToolCall, ToolInvocation};
use crucible_contract::event::Tokens;
use rusqlite::{Connection, OpenFlags, Row};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The longest tool-hint kept on a span, matched to `turn_trace`'s cap (identical span behavior).
const SUMMARY_CAP: usize = 200;

/// Everything one hermes turn contributes to the loop: the synthesized result event(s) + cost (the
/// keep/discard verdict), the tool spans, and the full conversation records (content-log export).
pub(crate) struct HermesTurn {
    pub events: Vec<AgentEvent>,
    pub cost_usd: Option<f64>,
    pub tool_calls: Vec<ToolInvocation>,
    pub records: Vec<GenAiRecord>,
}

/// Read the latest turn from an in-memory `state.db` (the sandbox-fetch bytes). rusqlite opens a
/// path, so the bytes land in a temp file first (a telemetry-path db is a few MB, fine). `None` on
/// any failure.
pub(crate) fn read_turn(db_bytes: &[u8]) -> Option<HermesTurn> {
    let mut tmp = match tempfile::Builder::new().suffix(".db").tempfile() {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(error = %e, "hermes state.db temp file create failed; no backfill");
            return None;
        }
    };
    if let Err(e) = tmp.write_all(db_bytes).and_then(|()| tmp.flush()) {
        tracing::warn!(error = %e, bytes = db_bytes.len(), "hermes state.db temp write failed; no backfill");
        return None;
    }
    read_turn_path(tmp.path())
}

/// Read the latest turn from a `state.db` on disk (the local-backend backfill path). The db opens
/// read-only; a WAL-mode db with no `-wal` sidecar reads its last checkpoint, which is what a
/// post-exit, single-file fetch carries.
pub(crate) fn read_turn_path(path: &Path) -> Option<HermesTurn> {
    let start = std::time::Instant::now();
    let conn = match Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(conn) => conn,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "hermes state.db open failed; no backfill");
            return None;
        }
    };
    let Some(session) = latest_session(&conn) else {
        tracing::debug!(path = %path.display(), "hermes state.db has no session; nothing to backfill");
        return None;
    };
    let messages = session_messages(&conn, &session.id);
    let redact = turn_trace::redact_enabled();

    let tool_calls = build_tool_calls(&messages, redact);
    let records = build_records(&messages, session.model.as_deref(), redact);
    let tokens = session.tokens();
    let cost = resolve_cost(&session, &tokens);
    let events = vec![result_event(&messages, &session, cost)];
    tracing::debug!(
        session = %session.id,
        messages = messages.len(),
        tool_calls = tool_calls.len(),
        records = records.len(),
        input_tokens = tokens.input,
        output_tokens = tokens.output,
        cost_usd = cost,
        elapsed_us = start.elapsed().as_micros(),
        "hermes turn backfilled from state.db"
    );
    Some(HermesTurn {
        events,
        cost_usd: Some(cost),
        tool_calls,
        records,
    })
}

/// The per-turn rollup row (the latest session; a fresh sandbox has exactly one).
struct SessionRow {
    id: String,
    model: Option<String>,
    end_reason: Option<String>,
    input: i64,
    output: i64,
    cache_read: i64,
    cache_write: i64,
    estimated_cost: Option<f64>,
    actual_cost: Option<f64>,
}

impl SessionRow {
    fn tokens(&self) -> Tokens {
        let nz = |v: i64| v.max(0) as u64;
        Tokens {
            input: nz(self.input),
            output: nz(self.output),
            cache_read: nz(self.cache_read),
            cache_write: nz(self.cache_write),
            total: nz(self.input + self.output),
            rate: None,
            cost_usd: None,
        }
    }
}

/// One conversation row, columns read by name so an additive schema change can't break the read.
struct MessageRow {
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    reasoning: Option<String>,
    timestamp: f64,
    active: bool,
}

/// The latest session by `started_at` (fresh sandbox ⇒ one row), or `None` when there are none.
fn latest_session(conn: &Connection) -> Option<SessionRow> {
    let mut stmt = conn
        .prepare("SELECT * FROM sessions ORDER BY started_at DESC LIMIT 1")
        .ok()?;
    let cols = column_index(&stmt);
    stmt.query_row([], |row| {
        Ok(SessionRow {
            id: opt_str(row, &cols, "id").unwrap_or_default(),
            model: opt_str(row, &cols, "model"),
            end_reason: opt_str(row, &cols, "end_reason"),
            input: opt_i64(row, &cols, "input_tokens"),
            output: opt_i64(row, &cols, "output_tokens"),
            cache_read: opt_i64(row, &cols, "cache_read_tokens"),
            cache_write: opt_i64(row, &cols, "cache_write_tokens"),
            estimated_cost: opt_f64(row, &cols, "estimated_cost_usd"),
            actual_cost: opt_f64(row, &cols, "actual_cost_usd"),
        })
    })
    .ok()
    .filter(|s| !s.id.is_empty())
}

/// All messages for a session, in insertion order (`id` monotonic). Empty on any query failure.
fn session_messages(conn: &Connection, session_id: &str) -> Vec<MessageRow> {
    let Ok(mut stmt) = conn.prepare("SELECT * FROM messages WHERE session_id = ?1 ORDER BY id ASC")
    else {
        return Vec::new();
    };
    let cols = column_index(&stmt);
    let rows = stmt.query_map([session_id], |row| {
        Ok(MessageRow {
            role: opt_str(row, &cols, "role").unwrap_or_default(),
            content: opt_str(row, &cols, "content"),
            tool_call_id: opt_str(row, &cols, "tool_call_id"),
            tool_calls: opt_str(row, &cols, "tool_calls"),
            reasoning: opt_str(row, &cols, "reasoning")
                .or_else(|| opt_str(row, &cols, "reasoning_content")),
            timestamp: opt_f64(row, &cols, "timestamp").unwrap_or(0.0),
            active: opt_i64(row, &cols, "active") != 0,
        })
    });
    match rows {
        Ok(iter) => iter.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

/// A tool-call turn's spans: each assistant `tool_calls` entry opens an invocation at that row's
/// timestamp; the matching `role='tool'` row (by `tool_call_id`) closes it and carries the error
/// verdict. Mirrors `turn_trace`'s pairing, over hermes's OpenAI-shaped tool-call JSON.
fn build_tool_calls(messages: &[MessageRow], redact: bool) -> Vec<ToolInvocation> {
    let mut order: Vec<ToolInvocation> = Vec::new();
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for m in messages {
        match m.role.as_str() {
            "assistant" => {
                for tc in parse_tool_calls(m.tool_calls.as_deref()) {
                    let input = serde_json::from_str::<Value>(&tc.arguments).ok();
                    let summary = turn_trace::summarize_input(&tc.name, input.as_ref(), redact);
                    by_id.insert(tc.id.clone(), order.len());
                    order.push(ToolInvocation {
                        name: tc.name,
                        start: epoch(m.timestamp),
                        end: None,
                        summary: turn_trace::truncate(&summary, SUMMARY_CAP),
                        error: false,
                    });
                }
            }
            "tool" => {
                if let Some(i) = m
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| by_id.get(id))
                    .copied()
                    && let Some(call) = order.get_mut(i)
                {
                    call.end = Some(epoch(m.timestamp));
                    call.error = tool_result_is_error(m.content.as_deref());
                }
            }
            _ => {}
        }
    }
    order
}

/// The full conversation as GenAI records, in transcript order. Direct role mapping; bodies ride
/// the same redact gate as the claude path.
fn build_records(messages: &[MessageRow], model: Option<&str>, redact: bool) -> Vec<GenAiRecord> {
    let mut out = Vec::new();
    for m in messages {
        match m.role.as_str() {
            "system" => {
                if let Some(c) = body(m.content.as_deref(), redact) {
                    out.push(GenAiRecord::System { content: c });
                }
            }
            "user" => {
                if let Some(c) = body(m.content.as_deref(), redact) {
                    out.push(GenAiRecord::User { content: c });
                }
            }
            "assistant" => {
                let text = body(m.content.as_deref(), redact);
                let reasoning = body(m.reasoning.as_deref(), redact);
                let tool_calls: Vec<ToolCall> = parse_tool_calls(m.tool_calls.as_deref())
                    .into_iter()
                    .map(|tc| ToolCall {
                        id: tc.id,
                        name: tc.name,
                        arguments: scrub_body(&tc.arguments, redact),
                    })
                    .collect();
                if text.is_none() && reasoning.is_none() && tool_calls.is_empty() {
                    continue;
                }
                out.push(GenAiRecord::Assistant {
                    text,
                    reasoning,
                    tool_calls,
                    model: model.map(str::to_string),
                });
            }
            "tool" => {
                out.push(GenAiRecord::Tool {
                    id: m.tool_call_id.clone().unwrap_or_default(),
                    content: body(m.content.as_deref(), redact).unwrap_or_default(),
                    is_error: tool_result_is_error(m.content.as_deref()),
                });
            }
            _ => {}
        }
    }
    out
}

/// The turn's result verdict, synthesized so keep/discard sees success/failure like claude's
/// `Result`. A turn with no active assistant completion (only a user row) is a failure; an
/// error-shaped `end_reason` also fails it. `cost` rides here AND in `HermesTurn.cost_usd` (the
/// caller folds the latter via max).
fn result_event(messages: &[MessageRow], session: &SessionRow, cost: f64) -> AgentEvent {
    let final_text = messages
        .iter()
        .rev()
        .find(|m| {
            m.role == "assistant"
                && m.active
                && m.content.as_deref().is_some_and(|c| !c.trim().is_empty())
        })
        .and_then(|m| m.content.clone());
    let turns = messages.iter().filter(|m| m.role == "assistant").count() as u32;
    let reason_err = session.end_reason.as_deref().is_some_and(is_error_reason);
    let is_error = final_text.is_none() || reason_err;
    AgentEvent::Result {
        subtype: session
            .end_reason
            .clone()
            .unwrap_or_else(|| "success".into()),
        is_error,
        turns,
        cost_usd: cost,
        error: is_error.then(|| {
            final_text
                .clone()
                .unwrap_or_else(|| "hermes turn produced no assistant response".into())
        }),
    }
}

/// Turn cost: the authoritative `actual_cost_usd`, else the `estimated_cost_usd`, else the
/// pricing-table estimate over the session tokens. A 0/absent stored cost falls through to the
/// estimate (hermes writes 0.0 with `cost_status='unknown'` when it hasn't priced the turn). The
/// model's provider prefix (`vertex_ai/`, `anthropic/`, `google/`) is stripped so the pricing
/// table's opus/sonnet/haiku substring match lands.
fn resolve_cost(session: &SessionRow, tokens: &Tokens) -> f64 {
    positive(session.actual_cost)
        .or_else(|| positive(session.estimated_cost))
        .unwrap_or_else(|| estimate_cost(strip_provider(session.model.as_deref()), tokens))
}

/// A cost value only counts when strictly positive (0.0 = not priced).
fn positive(v: Option<f64>) -> Option<f64> {
    v.filter(|c| *c > 0.0)
}

/// Strip a LiteLLM-style `provider/` prefix so the model name reaches the pricing table's substring
/// match (`vertex_ai/claude-opus-4-6` → `claude-opus-4-6`). No slash ⇒ unchanged; empty ⇒ "".
fn strip_provider(model: Option<&str>) -> &str {
    match model {
        Some(m) => m.rsplit('/').next().unwrap_or(m),
        None => "",
    }
}

/// Parse hermes's OpenAI-shaped `tool_calls` JSON array into `(id, name, arguments)` triples.
/// Each entry is `{id|call_id, function: {name, arguments}}` where `arguments` is a JSON string.
/// Robust to a torn/absent value: anything unparseable yields no calls.
fn parse_tool_calls(raw: Option<&str>) -> Vec<ToolCall> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|it| {
            let id = it
                .get("id")
                .or_else(|| it.get("call_id"))
                .and_then(Value::as_str)?
                .to_string();
            let func = it.get("function");
            let name = func
                .and_then(|f| f.get("name"))
                .or_else(|| it.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let arguments = func
                .and_then(|f| f.get("arguments"))
                .or_else(|| it.get("arguments"))
                .map(arg_string)
                .unwrap_or_default();
            Some(ToolCall {
                id,
                name,
                arguments,
            })
        })
        .collect()
}

/// A tool-call `arguments` value as a string: hermes stores it already-JSON-encoded (a string), so
/// pass a string through verbatim and serialize anything else.
fn arg_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// The error verdict for a `role='tool'` result: hermes's content is JSON like
/// `{"output": …, "exit_code": 0, "error": null}`, a non-zero exit or a non-null `error` fails it.
/// Non-JSON content (best-effort) is not an error.
fn tool_result_is_error(content: Option<&str>) -> bool {
    let Some(c) = content else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<Value>(c) else {
        return false;
    };
    let exit_bad = v
        .get("exit_code")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0);
    let err_present = v.get("error").is_some_and(|e| !e.is_null());
    exit_bad || err_present
}

/// Whether an `end_reason` marks a failed turn (vs a normal completion like `stop`/`end_turn`).
fn is_error_reason(reason: &str) -> bool {
    let r = reason.to_ascii_lowercase();
    ["error", "abort", "fail", "cancel", "timeout"]
        .iter()
        .any(|marker| r.contains(marker))
}

/// A message body for a GenAI record: trimmed to `None` when empty, otherwise scrubbed per the
/// redact gate (per-line, so multi-line bodies keep their structure, matching the claude path).
fn body(content: Option<&str>, redact: bool) -> Option<String> {
    let s = content?.trim();
    if s.is_empty() {
        None
    } else {
        Some(scrub_body(s, redact))
    }
}

/// Scrub each line through `turn_trace::redact` when the toggle is on; pass through otherwise.
fn scrub_body(s: &str, redact: bool) -> String {
    if redact {
        s.lines()
            .map(turn_trace::redact)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        s.to_string()
    }
}

/// A REAL epoch-seconds timestamp → `SystemTime`. A non-finite or non-positive value clamps to the
/// epoch (never panics, `Duration::from_secs_f64` panics on negatives/NaN).
fn epoch(ts: f64) -> SystemTime {
    if ts.is_finite() && ts > 0.0 {
        UNIX_EPOCH + Duration::from_secs_f64(ts)
    } else {
        UNIX_EPOCH
    }
}

/// Map a prepared statement's output columns to their indices, so rows are read by name against a
/// `SELECT *` (an additive schema change can't shift a positional read out from under us).
fn column_index(stmt: &rusqlite::Statement<'_>) -> HashMap<String, usize> {
    stmt.column_names()
        .iter()
        .enumerate()
        .map(|(i, n)| (n.to_string(), i))
        .collect()
}

fn opt_i64(row: &Row, cols: &HashMap<String, usize>, name: &str) -> i64 {
    cols.get(name)
        .and_then(|&i| row.get::<usize, Option<i64>>(i).ok().flatten())
        .unwrap_or(0)
}

fn opt_f64(row: &Row, cols: &HashMap<String, usize>, name: &str) -> Option<f64> {
    cols.get(name)
        .and_then(|&i| row.get::<usize, Option<f64>>(i).ok().flatten())
}

fn opt_str(row: &Row, cols: &HashMap<String, usize>, name: &str) -> Option<String> {
    cols.get(name)
        .and_then(|&i| row.get::<usize, Option<String>>(i).ok().flatten())
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The banked fixture db (7 sessions / 10 messages incl. a real tool-call turn and
    /// failure-shaped rows). Real SQLite, no mocks.
    const FIXTURE: &[u8] = include_bytes!("testdata/hermes_state_fixture.db");

    fn turn() -> HermesTurn {
        read_turn(FIXTURE).expect("fixture reads")
    }

    /// The latest session (by `started_at`) is the tool-call turn, not one of the earlier spike
    /// sessions, the ordering selection works.
    #[test]
    fn selects_the_latest_session() {
        let t = turn();
        // The tool-call turn has a terminal invocation; the spike sessions have none.
        assert_eq!(
            t.tool_calls.len(),
            1,
            "latest session is the tool-call turn"
        );
        assert!(t.records.iter().any(
            |r| matches!(r, GenAiRecord::User { content } if content.contains("echo HELLO_TOOL"))
        ));
    }

    /// The tool-call turn reconstructs one timed, non-errored `terminal` span with the command hint.
    #[test]
    fn tool_call_turn_reconstructs_the_span() {
        let t = turn();
        let call = &t.tool_calls[0];
        assert_eq!(call.name, "terminal");
        assert_eq!(call.summary, "echo HELLO_TOOL");
        assert!(!call.error, "exit_code 0 ⇒ not errored");
        assert!(call.end.is_some(), "closed by its tool row");
        assert!(call.end.unwrap() >= call.start, "closes at/after open");
    }

    /// The full conversation maps to GenAI records: user prompt, assistant tool call, tool result,
    /// final assistant completion, carrying the model + tool-call id correlation.
    #[test]
    fn tool_call_turn_reconstructs_the_records() {
        let t = turn();
        let has_assistant_toolcall = t.records.iter().any(|r| {
            matches!(r, GenAiRecord::Assistant { tool_calls, model, .. }
                if tool_calls.iter().any(|c| c.name == "terminal" && c.arguments.contains("echo HELLO_TOOL"))
                    && model.as_deref() == Some("vertex_ai/claude-opus-4-6"))
        });
        assert!(has_assistant_toolcall, "assistant tool-call record present");
        let tool_rec = t.records.iter().find_map(|r| match r {
            GenAiRecord::Tool {
                id,
                content,
                is_error,
            } => Some((id, content, is_error)),
            _ => None,
        });
        let (id, content, is_error) = tool_rec.expect("tool record present");
        assert!(
            id.starts_with("toolu_"),
            "tool result correlates by id: {id}"
        );
        assert!(content.contains("HELLO_TOOL"));
        assert!(!is_error);
        // The final assistant completion is the last record.
        assert!(matches!(
            t.records.last(),
            Some(GenAiRecord::Assistant { text: Some(txt), .. }) if txt.contains("HELLO_TOOL")
        ));
    }

    /// Tokens come back from the session rollup (>0), and cost resolves to a positive number, the
    /// stored 0.0/`unknown` cost falls through to the pricing-table estimate over the tokens.
    #[test]
    fn tokens_and_cost_backfill() {
        let t = turn();
        assert!(t.cost_usd.unwrap() > 0.0, "cost falls back to the estimate");
        // The session carried input=38043/output=80; the estimate is derived from those.
        let expected = estimate_cost(
            "claude-opus-4-6",
            &Tokens {
                input: 38043,
                output: 80,
                total: 38123,
                ..Default::default()
            },
        );
        assert!((t.cost_usd.unwrap() - expected).abs() < 1e-9);
    }

    /// The tool-call turn synthesizes a successful `Result` (has an assistant completion).
    #[test]
    fn tool_call_turn_result_is_success() {
        let t = turn();
        let result = t
            .events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Result {
                    is_error, cost_usd, ..
                } => Some((*is_error, *cost_usd)),
                _ => None,
            })
            .expect("a Result event");
        assert!(!result.0, "completed turn is not an error");
        assert!(result.1 > 0.0, "result carries the resolved cost");
    }

    /// A failure-shaped session (only a user message, no assistant row) is `is_error=true`. We
    /// isolate one by copying the fixture and deleting the later sessions so it becomes latest.
    #[test]
    fn failure_turn_reports_error() {
        // Session `20260716_234619_97f9aa` is user-only (its stdin spike attempt failed). Make it
        // the latest by dropping everything started after it, in a throwaway copy of the fixture.
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("state.db");
        std::fs::write(&db, FIXTURE).expect("write");
        {
            let conn = Connection::open(&db).expect("open rw");
            conn.execute(
                "DELETE FROM messages WHERE session_id IN \
                 (SELECT id FROM sessions WHERE id != '20260716_234619_97f9aa')",
                [],
            )
            .expect("prune messages");
            conn.execute(
                "DELETE FROM sessions WHERE id != '20260716_234619_97f9aa'",
                [],
            )
            .expect("prune sessions");
        }
        let t = read_turn_path(&db).expect("reads");
        assert!(t.tool_calls.is_empty(), "no tool calls in a failed turn");
        let is_error = t
            .events
            .iter()
            .any(|e| matches!(e, AgentEvent::Result { is_error, .. } if *is_error));
        assert!(is_error, "user-only turn ⇒ success=false");
    }

    /// Infallibility: garbage bytes, empty bytes, and a valid-but-empty db all degrade to `None`
    /// (never a panic).
    #[test]
    fn garbage_bytes_degrade_to_none() {
        assert!(read_turn(b"").is_none());
        assert!(read_turn(b"not a sqlite database at all").is_none());
        assert!(read_turn(&[0u8; 4096]).is_none());
        // A well-formed sqlite db with no sessions table also yields None.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("empty.db");
        Connection::open(&db)
            .unwrap()
            .execute("CREATE TABLE unrelated (x INTEGER)", [])
            .unwrap();
        assert!(read_turn_path(&db).is_none());
    }

    /// A tool result carrying a non-zero exit / non-null error flips the span + record verdict.
    #[test]
    fn tool_error_verdict_from_content_json() {
        assert!(tool_result_is_error(Some(
            r#"{"output":"","exit_code":1,"error":null}"#
        )));
        assert!(tool_result_is_error(Some(
            r#"{"output":"","exit_code":0,"error":"boom"}"#
        )));
        assert!(!tool_result_is_error(Some(
            r#"{"output":"ok","exit_code":0,"error":null}"#
        )));
        assert!(!tool_result_is_error(Some("not json")));
        assert!(!tool_result_is_error(None));
    }

    #[test]
    fn strips_provider_prefixes() {
        assert_eq!(
            strip_provider(Some("vertex_ai/claude-opus-4-6")),
            "claude-opus-4-6"
        );
        assert_eq!(
            strip_provider(Some("anthropic/claude-haiku-4-5")),
            "claude-haiku-4-5"
        );
        assert_eq!(strip_provider(Some("claude-opus-4-6")), "claude-opus-4-6");
        assert_eq!(strip_provider(None), "");
    }
}
