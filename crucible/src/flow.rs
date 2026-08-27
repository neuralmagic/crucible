//! `crucible flow`: post-hoc run explainability. Folds a finished run's `session.jsonl`
//! (required) plus a Datadog APM span export (optional, from a `--spans` file or fetched
//! live with `--dd-trace`, see `flow_dd`) into a small renderer-agnostic
//! flow model, then emits it as JSON, Graphviz dot, or mermaid — the output format is
//! picked by the `--out` extension.
//!
//! The JSON IR is the contract: an HTML (or any other) renderer needs nothing beyond
//! `flow.json`, so extraction and emission never mix. The session log alone covers
//! decisions, scores, edits, rungs, budget and publish; the span export adds wall-clock
//! (iteration windows, turn durations, the agent's per-tool-call timeline, rung
//! durations). Wide-round and infra rows are out of scope: this is the deep-loop
//! overview a human is walked through.

use anyhow::Result;
use crucible_contract::session::{EvidenceDisposition, RowWire, SessionEvent, decode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

#[derive(Debug, thiserror::Error)]
pub enum FlowError {
    #[error("--out needs a .json/.dot/.mmd/.html extension, got `{ext}`")]
    UnknownFormat { ext: String },
    #[error("parsing span export")]
    SpanJson(#[source] serde_json::Error),
    #[error("span export is neither a span array nor {{\"data\": [...]}}")]
    SpanShape,
    #[error("serializing flow model")]
    Model(#[source] serde_json::Error),
}

// ---------------------------------------------------------------------------
// The IR. Everything an emitter (including the later HTML one) needs, nothing more.
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FlowModel {
    pub run: RunHeader,
    pub preflight: Vec<PreflightStep>,
    pub iterations: Vec<Iteration>,
    pub budget: Vec<BudgetPoint>,
    pub publish: Publish,
    pub epilogue: Vec<EpilogueCheck>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RunHeader {
    /// The publish run-record id (`<stamp>-<goal-slug>`); absent when the run never published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub goal_line: String,
    pub gate: String,
    pub model: String,
    /// Score direction (`lower`/`higher`), from the run identity: an emitter needs it to
    /// say whether a delta is an improvement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_score: Option<f64>,
    pub improved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_secs: Option<u64>,
    /// The shutdown line's outcome (`finished`/`solved`/`budget`/...); absent when the
    /// log ends without one (the pod died mid-run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PreflightStep {
    pub cmd_short: String,
    /// `ok` or `failed`.
    pub outcome: String,
    pub note: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Iteration {
    pub n: u32,
    /// Normalized: `keep`, `discard`, or `rejected` (discarded by a rung before any score).
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delta_pct_vs_baseline: Option<f64>,
    pub note_short: String,
    pub edits: Edits,
    pub rungs: Vec<Rung>,
    pub turn: Turn,
    /// ISO8601 window bounds, only when a span export was joined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
}

/// File list + counts parsed from the row's unified diff — never the hunks themselves.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Edits {
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub files: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Rung {
    pub name: String,
    pub pass: bool,
    /// The check never ran (skipped disposition on the wire, or a gate that reports a
    /// skip as a pass with a "<task> skipped: why" note).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped: bool,
    pub note_short: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<f64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Turn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_s: Option<f64>,
    /// The agent's in-turn actions from the span export, sorted by `at_s` (seconds
    /// since the iteration started).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool: String,
    pub at_s: f64,
    pub duration_s: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BudgetPoint {
    pub at_s: u64,
    pub spent: f64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Publish {
    pub prs: Vec<Pr>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Pr {
    pub url: String,
    pub repo: String,
    pub branch: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EpilogueCheck {
    pub note: String,
    pub pass: bool,
}

// ---------------------------------------------------------------------------
// Entry
// ---------------------------------------------------------------------------

/// The inputs of one flow render: the run's session log and, when the caller has one, the
/// Datadog span export for the same run (spans API v2 objects, a JSON array or `{"data": [...]}`).
pub struct FlowInput {
    pub session_log: String,
    pub spans_json: Option<String>,
}

/// The output document format. The CLI picks it from the `--out` extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlowFormat {
    /// The IR (`flow.json`).
    Json,
    /// Graphviz dot.
    Dot,
    /// Mermaid.
    Mermaid,
    /// The self-contained explainer page.
    Html,
}

impl FlowFormat {
    /// The format an output path's extension selects.
    pub fn from_extension(ext: &str) -> Result<Self, FlowError> {
        match ext {
            "json" => Ok(FlowFormat::Json),
            "dot" => Ok(FlowFormat::Dot),
            "mmd" => Ok(FlowFormat::Mermaid),
            "html" => Ok(FlowFormat::Html),
            other => Err(FlowError::UnknownFormat { ext: other.into() }),
        }
    }
}

/// Render the flow document: the library form of `crucible flow`, which reads `--session` and
/// `--spans` (or fetches `--dd-trace`) into a [`FlowInput`] and writes the result to `--out`.
/// Pure: nothing beyond `input` is read.
#[tracing::instrument(skip_all, fields(format = ?format), err)]
pub fn render(input: &FlowInput, format: FlowFormat) -> Result<String> {
    let model = build_model(&input.session_log, input.spans_json.as_deref())?;
    Ok(match format {
        FlowFormat::Json => {
            let mut s = serde_json::to_string_pretty(&model).map_err(FlowError::Model)?;
            s.push('\n');
            s
        }
        FlowFormat::Dot => emit_dot(&model),
        FlowFormat::Mermaid => emit_mermaid(&model),
        FlowFormat::Html => crate::flow_html::emit_html(&model),
    })
}

/// Fold a session log into the rendered report pair (`flow.json`, `flow.html`) with no span
/// export — publish runs on the loop pod, which has no Datadog creds, so the page uses its
/// no-spans fallback.
pub fn render_report(session_log: &str) -> Result<(String, String)> {
    let model = build_model(session_log, None)?;
    let mut json = serde_json::to_string_pretty(&model).map_err(FlowError::Model)?;
    json.push('\n');
    Ok((json, crate::flow_html::emit_html(&model)))
}

/// Fold the session log (and, when given, the span export) into the flow model.
pub fn build_model(session_log: &str, spans_json: Option<&str>) -> Result<FlowModel> {
    let (mut model, aux) = extract(session_log);
    if let Some(raw) = spans_json {
        join_spans(&mut model, &aux, raw)?;
    }
    Ok(model)
}

/// Scores can be `INFINITY` (no measurement); `serde_json` refuses non-finite
/// floats, so drop those to `null` rather than blow up the whole record.
pub fn finite(x: f64) -> Option<f64> {
    x.is_finite().then_some(x)
}

/// First non-empty goal line, de-hashed and length-capped, for leaderboard display.
pub fn goal_line(goal: &str) -> String {
    let line = goal
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .trim_start_matches('#')
        .trim();
    line.chars().take(120).collect()
}

// ---------------------------------------------------------------------------
// Extraction: session.jsonl -> FlowModel
// ---------------------------------------------------------------------------

/// Extraction side-channel that must not live in the IR: per-rung measure-span kinds
/// (see [`rung_kind`]) keyed by `(iteration, rung name)`, for the span join.
#[derive(Default)]
struct Aux {
    rung_kind: HashMap<(u32, String), String>,
}

fn extract(log: &str) -> (FlowModel, Aux) {
    let mut m = FlowModel::default();
    let mut aux = Aux::default();
    let mut pending_preflight: Option<String> = None;
    for ev in log.lines().filter_map(decode) {
        match ev {
            SessionEvent::Start {
                goal, gate, model, ..
            } => {
                m.run.goal_line = goal_line(&goal);
                m.run.gate = gate;
                m.run.model = model;
            }
            SessionEvent::Identity { identity } => {
                if !identity.direction.is_empty() {
                    m.run.direction = Some(identity.direction);
                }
            }
            SessionEvent::Note { msg } => fold_note(&mut m, &mut pending_preflight, &msg),
            SessionEvent::Row { row, .. } => fold_row(&mut m, &mut aux, &row),
            SessionEvent::Budget {
                spent,
                elapsed_secs,
            } => m.budget.push(BudgetPoint {
                at_s: elapsed_secs,
                spent,
            }),
            SessionEvent::Summary { best_score, .. } => {
                m.run.best_score = best_score.and_then(finite);
            }
            SessionEvent::PrLinks { links } => {
                m.publish.prs = links
                    .into_iter()
                    .map(|l| Pr {
                        url: l.url,
                        repo: l.repo,
                        branch: l.branch,
                    })
                    .collect();
            }
            SessionEvent::Shutdown { outcome, .. } => m.run.outcome = Some(outcome),
            _ => {}
        }
    }
    if let Some(b) = m.run.baseline_score.filter(|b| *b != 0.0) {
        for it in &mut m.iterations {
            it.delta_pct_vs_baseline = it.score.map(|s| (s - b) / b * 100.0);
        }
    }
    m.run.improved = m.iterations.iter().any(|i| i.decision == "keep");
    if let Some(last) = m.budget.last() {
        m.run.cost_usd = Some(last.spent);
        m.run.elapsed_secs = Some(last.at_s);
    }
    (m, aux)
}

/// The preflight runner reports as note pairs (`preflight: <cmd>` then `preflight ok: …`);
/// the publish note carries the run-record URI whose last segment is the run id.
fn fold_note(m: &mut FlowModel, pending: &mut Option<String>, msg: &str) {
    if let Some(cmd) = msg.strip_prefix("preflight: ") {
        *pending = Some(short_cmd(cmd));
    } else if let Some(note) = msg.strip_prefix("preflight ok: ")
        && let Some(cmd_short) = pending.take()
    {
        m.preflight.push(PreflightStep {
            cmd_short,
            outcome: "ok".into(),
            note: fold_short(note, 120),
        });
    } else if let Some(uri) = msg.strip_prefix("published run record to ") {
        m.run.id = uri
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .map(String::from);
    }
}

fn fold_row(m: &mut FlowModel, aux: &mut Aux, row: &RowWire) {
    match (row.decision.as_str(), row.phase.as_deref()) {
        ("baseline", _) => m.run.baseline_score = row.score,
        // A preflight failure ends the run before any baseline; it reads best as a
        // failed preflight step.
        ("preflight-failed", _) => m.preflight.push(PreflightStep {
            cmd_short: String::new(),
            outcome: "failed".into(),
            note: fold_short(&row.note, 120),
        }),
        // Run-scoped epilogue rows keep phase "epilogue" whatever their verdict; the
        // decision string says how they ended ("epilogue" = passed, like publish.rs).
        (decision, Some("epilogue")) => {
            m.epilogue.push(EpilogueCheck {
                note: fold_short(&row.note, 120),
                pass: decision == "epilogue",
            });
        }
        (_, None) if row.iter > 0 => {
            let it = fold_iteration(aux, row);
            // A resume can re-log an iteration; the newest row wins.
            if let Some(last) = m.iterations.last_mut().filter(|l| l.n == it.n) {
                *last = it;
            } else {
                m.iterations.push(it);
            }
        }
        _ => {}
    }
}

fn fold_iteration(aux: &mut Aux, row: &RowWire) -> Iteration {
    Iteration {
        n: row.iter,
        decision: classify(&row.decision, row.score),
        score: row.score.and_then(finite),
        delta_pct_vs_baseline: None,
        note_short: fold_short(&row.note, 120),
        edits: parse_diff(&row.diff),
        rungs: fold_rungs(aux, row),
        turn: Turn::default(),
        started_at: None,
        ended_at: None,
    }
}

/// Normalize a row decision for rendering: a discard without a score means a rung
/// rejected the candidate before it was ever measured.
fn classify(decision: &str, score: Option<f64>) -> String {
    if decision.starts_with("keep") {
        "keep".into()
    } else if score.is_none() {
        "rejected".into()
    } else {
        "discard".into()
    }
}

/// Rung list for one graded row: order and dispositions from the declared evidence set,
/// per-rung notes from the grade detail's fold. A rejection leaves no evidence at all —
/// the row note (`<task> rejected the candidate: …`) is then the only trace of which
/// rung fired, so it is synthesized into a single failed rung.
fn fold_rungs(aux: &mut Aux, row: &RowWire) -> Vec<Rung> {
    if row.evidence.is_empty() {
        if let Some(pos) = row.note.find(" rejected the candidate") {
            let task = &row.note[..pos];
            if !task.is_empty() && !task.contains(char::is_whitespace) {
                return vec![Rung {
                    name: task.into(),
                    pass: false,
                    skipped: false,
                    note_short: fold_short(&row.note, 120),
                    duration_s: None,
                }];
            }
        }
        return Vec::new();
    }
    let detail: Value = serde_json::from_str(&row.detail).unwrap_or(Value::Null);
    let entries = detail.get("evidence").and_then(Value::as_object);
    row.evidence
        .iter()
        .map(|e| {
            let entry = entries.and_then(|o| o.get(&e.task));
            if let Some(kind) = entry.and_then(rung_kind) {
                aux.rung_kind.insert((row.iter, e.task.clone()), kind);
            }
            let note = entry
                .and_then(|v| v.get("note"))
                .and_then(Value::as_str)
                .unwrap_or(&e.note);
            // Some gates report a skip as a pass with a "<task> skipped: why" note
            // (nothing ran, so nothing failed); fold it like the wire's skipped
            // disposition so no emitter claims a check passed that never ran.
            let skipped = e.disposition == EvidenceDisposition::Skipped
                || note
                    .strip_prefix(e.task.as_str())
                    .is_some_and(|r| r.trim_start().starts_with("skipped"));
            Rung {
                name: e.task.clone(),
                pass: e.disposition == EvidenceDisposition::Passed,
                skipped,
                note_short: fold_short(note, 120),
                duration_s: None,
            }
        })
        .collect()
}

/// Which broker measure-span kind (`mcp.codegen_<kind>`) graded this rung. The rung name
/// is manifest-chosen, so the only link back to the span is the rung's log-file prefix
/// (`bench-…`/`lmeval-…`/`profile-…`); a build rung declares `stage: "build"` and writes
/// no log. Best-effort: a domain off the codegen broker just gets no rung durations.
fn rung_kind(entry: &Value) -> Option<String> {
    let d = entry.get("detail")?;
    if let Some(first) = d
        .get("logs")
        .and_then(Value::as_array)
        .and_then(|l| l.first())
        .and_then(Value::as_str)
    {
        return match first.split('-').next().unwrap_or_default() {
            "bench" => Some("benchmark".into()),
            "lmeval" => Some("lm_eval".into()),
            "profile" => Some("profile".into()),
            _ => None,
        };
    }
    (d.get("stage").and_then(Value::as_str) == Some("build")).then(|| "build".into())
}

/// File list + counts from a unified diff; hunks are deliberately never carried.
/// +/- lines only count inside a hunk, so file headers (`+++ b/…`) are never counted
/// and an added line whose content itself starts with `++` never gets dropped.
fn parse_diff(diff: &str) -> Edits {
    let mut e = Edits::default();
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            in_hunk = false;
            if let Some(path) = diff_b_path(rest) {
                e.files.push(path);
            }
        } else if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk && line.starts_with('+') {
            e.insertions += 1;
        } else if in_hunk && line.starts_with('-') {
            e.deletions += 1;
        }
    }
    e.files_changed = e.files.len();
    e
}

/// Post-image path from the `a/<old> b/<new>` tail of a `diff --git` line. Git quotes
/// a path containing specials (`"a/x y" "b/x y"`); the unquoted form is ambiguous when
/// a path itself contains ` b/`, so the symmetric old == new split is preferred and
/// `rfind(" b/")` is the rename fallback.
fn diff_b_path(rest: &str) -> Option<String> {
    if let Some(stripped) = rest.strip_suffix('"') {
        let pos = stripped.rfind("\"b/")?;
        return Some(stripped[pos + 3..].to_string());
    }
    let mid = rest.len() / 2;
    if rest.len() % 2 == 1
        && rest.as_bytes().get(mid) == Some(&b' ')
        && let (Some(pa), Some(pb)) = (
            rest[..mid].strip_prefix("a/"),
            rest[mid + 1..].strip_prefix("b/"),
        )
        && pa == pb
    {
        return Some(pb.to_string());
    }
    rest.rfind(" b/").map(|pos| rest[pos + 3..].to_string())
}

/// One-line fold + cap for notes that can carry whole tracebacks.
fn fold_short(s: &str, cap: usize) -> String {
    crate::turn_trace::truncate(&s.split_whitespace().collect::<Vec<_>>().join(" "), cap)
}

/// Presentation form of a preflight command: path tokens shrink to their last segment.
fn short_cmd(cmd: &str) -> String {
    let toks: Vec<&str> = cmd
        .split_whitespace()
        .map(|t| match t.rsplit_once('/') {
            Some((_, last)) if !last.is_empty() => last,
            _ => t,
        })
        .collect();
    crate::turn_trace::truncate(&toks.join(" "), 80)
}

// ---------------------------------------------------------------------------
// Span join: Datadog APM export -> timings on the model
// ---------------------------------------------------------------------------

/// One span from a Datadog spans-API v2 export; the envelope around the attributes
/// the join actually reads.
#[derive(Deserialize)]
struct DdSpan {
    #[serde(default)]
    attributes: DdAttrs,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct DdAttrs {
    service: String,
    resource_name: String,
    span_id: String,
    parent_id: String,
    start_timestamp: String,
    end_timestamp: String,
    custom: Value,
}

impl DdAttrs {
    fn window(&self) -> Option<(i128, i128)> {
        Some((ts_ns(&self.start_timestamp)?, ts_ns(&self.end_timestamp)?))
    }
}

/// ISO8601 -> epoch nanoseconds.
fn ts_ns(s: &str) -> Option<i128> {
    s.parse::<jiff::Timestamp>().ok().map(|t| t.as_nanosecond())
}

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn secs(from_ns: i128, to_ns: i128) -> f64 {
    r3((to_ns - from_ns) as f64 / 1e9)
}

/// The wall-clock window of one `iteration` span.
struct IterWin {
    start_ns: i128,
    end_ns: i128,
    span_id: String,
    /// When the iteration's `openshell_turn` ended; measure spans start after it.
    turn_end_ns: Option<i128>,
}

fn join_spans(m: &mut FlowModel, aux: &Aux, raw: &str) -> Result<()> {
    let spans = parse_spans(raw)?;

    // Iteration windows: crucible's `iteration` resource spans, keyed by their `iter` tag.
    let mut wins: HashMap<u32, IterWin> = HashMap::new();
    for s in &spans {
        if s.service == "crucible"
            && s.resource_name == "iteration"
            && let Some(n) = value_u32(s.custom.get("iter"))
            && let Some((start_ns, end_ns)) = s.window()
        {
            for it in m.iterations.iter_mut().filter(|it| it.n == n) {
                it.started_at = Some(s.start_timestamp.clone());
                it.ended_at = Some(s.end_timestamp.clone());
            }
            wins.insert(
                n,
                IterWin {
                    start_ns,
                    end_ns,
                    span_id: s.span_id.clone(),
                    turn_end_ns: None,
                },
            );
        }
    }

    // Turn durations: the `openshell_turn` span nested directly under each iteration.
    for s in &spans {
        if s.service == "crucible"
            && s.resource_name == "openshell_turn"
            && let Some((n, win)) = wins.iter_mut().find(|(_, w)| w.span_id == s.parent_id)
            && let Some((start_ns, end_ns)) = s.window()
        {
            win.turn_end_ns = Some(end_ns);
            if let Some(it) = m.iterations.iter_mut().find(|it| it.n == *n) {
                it.turn.duration_s = Some(secs(start_ns, end_ns));
            }
        }
    }

    // The agent's own actions: one `claude_code.tool` span per tool call, tagged with the
    // crucible turn it ran in (turn N == iteration N). The gateway's mirrored tool spans
    // re-emit the whole transcript every turn, so they are useless for bucketing; these
    // are emitted exactly once.
    for s in &spans {
        if s.service != "claude-code" || s.resource_name != "claude_code.tool" {
            continue;
        }
        let Some(n) = value_u32(s.custom.pointer("/crucible/turn")) else {
            continue;
        };
        let (Some(win), Some((start_ns, end_ns))) = (wins.get(&n), s.window()) else {
            continue;
        };
        let Some(tool) = s.custom.get("tool_name").and_then(Value::as_str) else {
            continue;
        };
        if let Some(it) = m.iterations.iter_mut().find(|it| it.n == n) {
            it.turn.tool_calls.push(ToolCall {
                tool: tool.into(),
                at_s: secs(win.start_ns, start_ns),
                duration_s: secs(start_ns, end_ns),
            });
        }
    }
    for it in &mut m.iterations {
        it.turn.tool_calls.sort_by(|a, b| a.at_s.total_cmp(&b.at_s));
    }

    // Rung durations: broker `mcp.codegen_<kind>` spans bucketed into the iteration whose
    // measure phase (after the turn ended) they started in, then claimed in rung order by
    // each rung whose extraction-time kind hint matches. One rung run is a long-poll
    // sequence: every poll that hits the broker's cap emits a `status: pending` span and
    // the final poll the terminal one, so pending spans extend the open sequence instead
    // of counting as durations — the duration runs first poll start → terminal end.
    // Two rungs of one kind (latency + ab-toggle both benchmark) claim successive
    // sequences.
    let mut buckets: HashMap<u32, HashMap<&str, VecDeque<f64>>> = HashMap::new();
    let mut open: HashMap<(u32, &str), i128> = HashMap::new();
    let mut measure: Vec<(&DdAttrs, i128, i128)> = spans
        .iter()
        .filter(|s| s.resource_name.starts_with("mcp.codegen_"))
        .filter_map(|s| s.window().map(|(a, b)| (s, a, b)))
        .collect();
    measure.sort_by_key(|(_, start_ns, _)| *start_ns);
    for (s, start_ns, end_ns) in measure {
        let Some((n, _)) = wins
            .iter()
            .find(|(_, w)| start_ns >= w.turn_end_ns.unwrap_or(w.start_ns) && start_ns < w.end_ns)
        else {
            continue;
        };
        let kind = &s.resource_name["mcp.codegen_".len()..];
        let seq_start = *open.entry((*n, kind)).or_insert(start_ns);
        if s.custom.get("status").and_then(Value::as_str) == Some("pending") {
            continue;
        }
        open.remove(&(*n, kind));
        buckets
            .entry(*n)
            .or_default()
            .entry(kind)
            .or_default()
            .push_back(secs(seq_start, end_ns));
    }
    // A sequence the export ends inside (pending spans with no terminal one) has no
    // knowable duration; it stays unclaimed rather than reporting the poll cap.
    for it in &mut m.iterations {
        let Some(kinds) = buckets.get_mut(&it.n) else {
            continue;
        };
        for rung in &mut it.rungs {
            if rung.skipped {
                continue;
            }
            if let Some(kind) = aux.rung_kind.get(&(it.n, rung.name.clone())) {
                rung.duration_s = kinds.get_mut(kind.as_str()).and_then(VecDeque::pop_front);
            }
        }
    }
    Ok(())
}

fn parse_spans(raw: &str) -> Result<Vec<DdAttrs>, FlowError> {
    let v: Value = serde_json::from_str(raw).map_err(FlowError::SpanJson)?;
    let arr = match v {
        Value::Array(a) => a,
        Value::Object(mut o) => match o.remove("data") {
            Some(Value::Array(a)) => a,
            _ => return Err(FlowError::SpanShape),
        },
        _ => return Err(FlowError::SpanShape),
    };
    // Tolerate junk entries; the join is best-effort over whatever parsed.
    Ok(arr
        .into_iter()
        .filter_map(|e| serde_json::from_value::<DdSpan>(e).ok())
        .map(|s| s.attributes)
        .collect())
}

/// A DD custom tag can arrive as a string or a number.
fn value_u32(v: Option<&Value>) -> Option<u32> {
    match v? {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Emitters: run-overview digraph (dot) and flowchart (mermaid)
// ---------------------------------------------------------------------------

const KEEP_FILL: &str = "#d5e8d4";
const DISCARD_FILL: &str = "#eeeeee";
const REJECTED_FILL: &str = "#f8cecc";
const PR_FILL: &str = "#fff2cc";
const BASELINE_FILL: &str = "#dae8fc";

pub(crate) fn score3(v: f64) -> String {
    format!("{v:.3}")
}

/// The two-line presentation label for an iteration node (shared by both emitters, with
/// the line break left to the caller).
fn iter_label(it: &Iteration) -> (String, String) {
    let head = format!("iter {} · {}", it.n, it.decision);
    let detail = match (it.score, it.delta_pct_vs_baseline) {
        (Some(s), Some(d)) => format!("{} ({d:+.2}%)", score3(s)),
        (Some(s), None) => score3(s),
        _ => it
            .rungs
            .iter()
            .find(|r| !r.pass)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| "no score".into()),
    };
    (head, detail)
}

fn fill(decision: &str) -> &'static str {
    match decision {
        "keep" => KEEP_FILL,
        "rejected" => REJECTED_FILL,
        _ => DISCARD_FILL,
    }
}

/// `owner/repo#N` from a GitHub PR URL, or the URL verbatim.
pub(crate) fn pr_short(url: &str) -> String {
    url.strip_prefix("https://github.com/")
        .map(|r| r.replacen("/pull/", "#", 1))
        .unwrap_or_else(|| url.to_string())
}

fn dot_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn emit_dot(m: &FlowModel) -> String {
    let mut s = String::from("digraph run_flow {\n    rankdir=LR;\n    labelloc=t;\n");
    let mut title = dot_escape(&m.run.goal_line);
    if let (Some(b), Some(best)) = (m.run.baseline_score, m.run.best_score) {
        title.push_str(&format!(
            "\\n{}: baseline {} → best {}",
            dot_escape(&m.run.gate),
            score3(b),
            score3(best)
        ));
    }
    s.push_str(&format!("    label=\"{title}\";\n"));
    s.push_str("    fontname=\"Helvetica\";\n");
    s.push_str(
        "    node [shape=box, style=\"rounded,filled\", fontname=\"Helvetica\", color=\"#666666\"];\n",
    );
    match m.run.baseline_score {
        Some(b) => s.push_str(&format!(
            "    baseline [label=\"baseline\\n{}\", fillcolor=\"{BASELINE_FILL}\"];\n",
            score3(b)
        )),
        None => s.push_str(&format!(
            "    baseline [label=\"baseline\", fillcolor=\"{BASELINE_FILL}\"];\n"
        )),
    }
    for it in &m.iterations {
        let (head, detail) = iter_label(it);
        s.push_str(&format!(
            "    it{} [label=\"{}\\n{}\", fillcolor=\"{}\"];\n",
            it.n,
            dot_escape(&head),
            dot_escape(&detail),
            fill(&it.decision)
        ));
    }
    for (i, pr) in m.publish.prs.iter().enumerate() {
        s.push_str(&format!(
            "    pr{i} [label=\"draft PR\\n{}\", fillcolor=\"{PR_FILL}\"];\n",
            dot_escape(&pr_short(&pr.url))
        ));
    }
    let mut prev = "baseline".to_string();
    for it in &m.iterations {
        s.push_str(&format!("    {prev} -> it{};\n", it.n));
        prev = format!("it{}", it.n);
    }
    // The PR carries the kept commits; its head is the last keep.
    if let Some(kept) = m.iterations.iter().rev().find(|i| i.decision == "keep") {
        for i in 0..m.publish.prs.len() {
            s.push_str(&format!(
                "    it{} -> pr{i} [label=\"kept\", style=dashed];\n",
                kept.n
            ));
        }
    }
    s.push_str("}\n");
    s
}

fn mmd_escape(s: &str) -> String {
    s.replace('"', "#quot;")
}

fn emit_mermaid(m: &FlowModel) -> String {
    let mut s = format!(
        "---\ntitle: \"{}\"\n---\nflowchart LR\n",
        mmd_escape(&m.run.goal_line)
    );
    match m.run.baseline_score {
        Some(b) => s.push_str(&format!("    B[\"baseline<br/>{}\"]\n", score3(b))),
        None => s.push_str("    B[\"baseline\"]\n"),
    }
    for it in &m.iterations {
        let (head, detail) = iter_label(it);
        s.push_str(&format!(
            "    I{}[\"{}<br/>{}\"]\n",
            it.n,
            mmd_escape(&head),
            mmd_escape(&detail)
        ));
    }
    for (i, pr) in m.publish.prs.iter().enumerate() {
        s.push_str(&format!(
            "    PR{i}[\"draft PR<br/>{}\"]\n",
            mmd_escape(&pr_short(&pr.url))
        ));
    }
    let mut chain = vec!["B".to_string()];
    chain.extend(m.iterations.iter().map(|it| format!("I{}", it.n)));
    if chain.len() > 1 {
        s.push_str(&format!("    {}\n", chain.join(" --> ")));
    }
    if let Some(kept) = m.iterations.iter().rev().find(|i| i.decision == "keep") {
        for i in 0..m.publish.prs.len() {
            s.push_str(&format!("    I{} -.-> PR{i}\n", kept.n));
        }
    }
    s.push_str(&format!(
        "    classDef keep fill:{KEEP_FILL},stroke:#82b366\n    classDef discard fill:{DISCARD_FILL},stroke:#999999\n    classDef rejected fill:{REJECTED_FILL},stroke:#b85450\n"
    ));
    for class in ["keep", "discard", "rejected"] {
        let nodes: Vec<String> = m
            .iterations
            .iter()
            .filter(|i| i.decision == class)
            .map(|i| format!("I{}", i.n))
            .collect();
        if !nodes.is_empty() {
            s.push_str(&format!("    class {} {class}\n", nodes.join(",")));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn fixture(name: &str) -> String {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata/flow")
            .join(name);
        std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("fixture {}: {e}", p.display()))
    }

    fn model() -> FlowModel {
        build_model(&fixture("run7-session.jsonl"), None).unwrap()
    }

    fn model_with_spans() -> FlowModel {
        build_model(
            &fixture("run7-session.jsonl"),
            Some(&fixture("run7-dd-spans.json")),
        )
        .unwrap()
    }

    fn approx(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() < eps, "{a} !~ {b}");
    }

    /// A task-lane run (no `[judge]`): gate "task", a skipped baseline, keep rows with no score.
    /// Captured from a real `examples/task` run.
    #[test]
    fn task_run_folds_scoreless_keeps() {
        let session = concat!(
            r#"{"v":1,"kind":"start","goal":"Append a progress line to log.txt each turn.","gate":"task","model":"claude-opus-4-6","namespace":"","iters_total":2,"max_cost":0.0,"max_secs":0}"#,
            "\n",
            r#"{"v":1,"kind":"row","row":{"iter":0,"decision":"baseline-skipped","note":"baseline skipped","detail":"","diff":"","diffstat":"","score":null,"total":null},"solved":false}"#,
            "\n",
            r#"{"v":1,"kind":"segment","fingerprint":"1d55012690acd92e","baseline_score":null,"regime":"default"}"#,
            "\n",
            r#"{"v":1,"kind":"row","row":{"iter":1,"decision":"keep","note":"turn complete (task mode: no gate)","detail":"task","diff":"","diffstat":"1 file changed, 1 insertion(+)","score":null,"total":null,"kept_snap":"69b2492eb3370c434c7dfc623eacafb301c8c685"},"solved":false}"#,
            "\n",
            r#"{"v":1,"kind":"row","row":{"iter":2,"decision":"keep","note":"turn complete (task mode: no gate)","detail":"task","diff":"","diffstat":"1 file changed, 1 insertion(+)","score":null,"total":null,"kept_snap":"5cf17726689b69b6406b7ec75ae7f39c09e74a1d"},"solved":false}"#,
            "\n",
            r#"{"v":1,"kind":"summary","rows":[],"gate":"task","best_score":null}"#,
            "\n",
            r#"{"v":1,"kind":"finished"}"#,
            "\n",
            r#"{"v":1,"kind":"shutdown","outcome":"finished","reason":"all iterations completed"}"#,
            "\n",
        );
        let m = build_model(session, None).unwrap();
        assert_eq!(m.run.gate, "task");
        assert_eq!(m.run.baseline_score, None);
        assert_eq!(m.run.best_score, None);
        assert!(m.run.improved, "keep rows count as improvement");
        assert_eq!(m.run.outcome.as_deref(), Some("finished"));
        let got: Vec<(u32, &str, Option<f64>)> = m
            .iterations
            .iter()
            .map(|i| (i.n, i.decision.as_str(), i.score))
            .collect();
        assert_eq!(got, vec![(1, "keep", None), (2, "keep", None)]);
    }

    const BASELINE: f64 = 8.750550938259494;
    const BEST: f64 = 8.65613441703772;

    #[test]
    fn run_header_folds_start_identity_summary_budget_shutdown() {
        let m = model();
        assert_eq!(
            m.run.id.as_deref(),
            Some("20260811T192719Z-goal-generalize-vllm-s-fused-a-gemm-for")
        );
        assert!(
            m.run.goal_line.starts_with("Goal: generalize vLLM's"),
            "{}",
            m.run.goal_line
        );
        assert_eq!(m.run.gate, "latency");
        assert_eq!(m.run.model, "claude-opus-4-6");
        assert_eq!(m.run.direction.as_deref(), Some("lower"));
        approx(m.run.baseline_score.unwrap(), BASELINE, 1e-12);
        approx(m.run.best_score.unwrap(), BEST, 1e-12);
        assert!(m.run.improved);
        approx(m.run.cost_usd.unwrap(), 33.31834675, 1e-9);
        assert_eq!(m.run.elapsed_secs, Some(43966));
        assert_eq!(m.run.outcome.as_deref(), Some("finished"));
    }

    #[test]
    fn preflight_steps_fold_as_cmd_note_pairs() {
        let m = model();
        assert_eq!(m.preflight.len(), 5);
        assert!(m.preflight.iter().all(|p| p.outcome == "ok"));
        assert_eq!(
            m.preflight[0].cmd_short,
            "python3 vllm_glm_gate.py --only-rung 1 --mode derive"
        );
        assert_eq!(m.preflight[0].note, "candidate built score=0");
        // The rung-3 (latency) probe seeds the baseline number.
        assert!(m.preflight[4].note.contains("tpot_ms=8.750550938259494"));
    }

    #[test]
    fn iterations_fold_decisions_and_scores() {
        let m = model();
        let got: Vec<(u32, &str, Option<f64>)> = m
            .iterations
            .iter()
            .map(|i| (i.n, i.decision.as_str(), i.score))
            .collect();
        assert_eq!(got.len(), 6);
        let want: [(u32, &str, Option<f64>); 6] = [
            (1, "discard", Some(8.857849192850153)),
            (2, "rejected", None),
            (3, "keep", Some(BEST)),
            (4, "discard", Some(8.754319360093632)),
            (5, "discard", Some(8.66481692042953)),
            (6, "discard", Some(8.753463029052)),
        ];
        for ((n, d, s), (wn, wd, ws)) in got.iter().zip(want.iter()) {
            assert_eq!((n, d), (wn, wd));
            match (s, ws) {
                (Some(a), Some(b)) => approx(*a, *b, 1e-12),
                (None, None) => {}
                other => panic!("iter {n}: score mismatch {other:?}"),
            }
        }
    }

    #[test]
    fn iter3_keep_carries_delta_and_diff_parsed_edits() {
        let m = model();
        let it3 = &m.iterations[2];
        assert_eq!(it3.decision, "keep");
        approx(
            it3.delta_pct_vs_baseline.unwrap(),
            (BEST - BASELINE) / BASELINE * 100.0,
            1e-12,
        );
        approx(it3.delta_pct_vs_baseline.unwrap(), -1.0789787, 1e-6);
        // Matches the row's own `git --shortstat`: 5 files, +180 -57.
        assert_eq!(it3.edits.files_changed, 5);
        assert_eq!(it3.edits.insertions, 180);
        assert_eq!(it3.edits.deletions, 57);
        assert!(
            it3.edits
                .files
                .iter()
                .any(|f| f == "csrc/libtorch_stable/dsv3_fused_a_gemm.cu")
        );
        assert!(it3.edits.files.iter().any(|f| f == "vllm/envs.py"));
        assert!(
            it3.edits
                .files
                .iter()
                .any(|f| f == "vllm/model_executor/layers/mla.py")
        );
    }

    #[test]
    fn iter2_shows_the_correctness_rejection() {
        let m = model();
        let it2 = &m.iterations[1];
        assert_eq!(it2.decision, "rejected");
        assert!(it2.score.is_none());
        assert!(
            it2.note_short
                .starts_with("correctness rejected the candidate:"),
            "{}",
            it2.note_short
        );
        assert_eq!(it2.rungs.len(), 1);
        assert_eq!(it2.rungs[0].name, "correctness");
        assert!(!it2.rungs[0].pass);
        assert!(it2.edits.files.is_empty());
    }

    #[test]
    fn rungs_fold_from_evidence_and_detail() {
        let m = model();
        let names: Vec<&str> = m.iterations[2]
            .rungs
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["build", "correctness", "latency", "ab-toggle", "mechanism"]
        );
        assert!(m.iterations[2].rungs.iter().all(|r| r.pass));
        let latency = &m.iterations[2].rungs[2];
        assert_eq!(latency.note_short, "latency: tpot_ms=8.65613441703772");
        assert_eq!(
            m.iterations[2].rungs[1].note_short,
            "correctness: score=0.93"
        );
        // ab-toggle's wire disposition is "passed" but its note says it never ran.
        let skipped: Vec<&str> = m.iterations[2]
            .rungs
            .iter()
            .filter(|r| r.skipped)
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(skipped, ["ab-toggle"]);
    }

    #[test]
    fn budget_publish_and_epilogue_fold() {
        let m = model();
        assert_eq!(m.budget.len(), 56);
        approx(m.budget[0].spent, 0.166685, 1e-9);
        assert_eq!(m.budget[0].at_s, 7402);
        approx(m.budget.last().unwrap().spent, 33.31834675, 1e-9);
        assert_eq!(m.publish.prs.len(), 1);
        assert_eq!(
            m.publish.prs[0].url,
            "https://github.com/wseaton/vllm/pull/8"
        );
        assert_eq!(m.publish.prs[0].repo, "wseaton/vllm");
        assert!(m.publish.prs[0].branch.starts_with("autoresearch/"));
        assert_eq!(m.epilogue.len(), 1);
        assert_eq!(m.epilogue[0].note, "full-eval: ok");
        assert!(m.epilogue[0].pass);
    }

    #[test]
    fn spans_join_adds_windows_turns_and_tool_calls() {
        let m = model_with_spans();
        // Every iteration gains its wall-clock window and turn duration.
        for it in &m.iterations {
            assert!(it.started_at.is_some(), "iter {} has no window", it.n);
            assert!(it.ended_at.is_some());
            assert!(it.turn.duration_s.is_some(), "iter {} has no turn", it.n);
        }
        assert_eq!(
            m.iterations[0].started_at.as_deref(),
            Some("2026-08-11T21:28:44.303Z")
        );
        // iter 1's turn: 21:28:44.304 -> 21:56:29.98.
        approx(m.iterations[0].turn.duration_s.unwrap(), 1665.676, 0.01);
        // One `claude_code.tool` span per agent action, bucketed by its turn tag.
        let counts: Vec<usize> = m
            .iterations
            .iter()
            .map(|i| i.turn.tool_calls.len())
            .collect();
        assert_eq!(counts, [198, 179, 79, 17, 17, 7]);
        for it in &m.iterations {
            for w in it.turn.tool_calls.windows(2) {
                assert!(w[0].at_s <= w[1].at_s, "iter {} tool calls unsorted", it.n);
            }
            for c in &it.turn.tool_calls {
                assert!(c.at_s >= 0.0 && c.duration_s >= 0.0);
            }
        }
        assert!(
            m.iterations[2]
                .turn
                .tool_calls
                .iter()
                .any(|c| c.tool == "Edit")
        );
    }

    #[test]
    fn spans_join_attaches_rung_durations() {
        let m = model_with_spans();
        let it3 = &m.iterations[2];
        let by_name = |n: &str| {
            it3.rungs
                .iter()
                .find(|r| r.name == n)
                .unwrap_or_else(|| panic!("no rung {n}"))
        };
        // Real sequence spans, not the broker's 20-min long-poll cap: a rung that
        // outlives one poll emits a pending span first, and the duration must cover
        // pending + terminal, never equal the constant 1200 s poll window.
        approx(by_name("build").duration_s.unwrap(), 1347.988, 0.01);
        approx(by_name("correctness").duration_s.unwrap(), 1816.946, 0.01);
        approx(by_name("latency").duration_s.unwrap(), 1353.859, 0.01);
        approx(by_name("mechanism").duration_s.unwrap(), 1467.727, 0.01);
        // The skipped rung never ran, so it must not claim anyone else's span.
        assert!(by_name("ab-toggle").duration_s.is_none());
        // Without spans no rung has a duration.
        let plain = model();
        assert!(
            plain
                .iterations
                .iter()
                .flat_map(|i| &i.rungs)
                .all(|r| r.duration_s.is_none())
        );
    }

    #[test]
    fn dot_emitter_renders_the_run_overview() {
        let dot = emit_dot(&model());
        assert!(dot.starts_with("digraph run_flow {"));
        assert!(dot.contains("rankdir=LR"));
        assert!(dot.contains("baseline\\n8.751"));
        assert!(dot.contains("iter 3 · keep\\n8.656 (-1.08%)"), "{dot}");
        assert!(dot.contains("iter 2 · rejected\\ncorrectness"));
        assert!(dot.contains(&format!("fillcolor=\"{KEEP_FILL}\"")));
        assert!(dot.contains("draft PR\\nwseaton/vllm#8"));
        assert!(dot.contains("baseline -> it1;"));
        assert!(dot.contains("it5 -> it6;"));
        assert!(dot.contains("it3 -> pr0 [label=\"kept\", style=dashed];"));
        assert!(dot.contains("latency: baseline 8.751 → best 8.656"));
    }

    #[test]
    fn mermaid_emitter_renders_the_run_overview() {
        let mmd = emit_mermaid(&model());
        assert!(mmd.contains("flowchart LR"));
        assert!(mmd.contains("B[\"baseline<br/>8.751\"]"));
        assert!(mmd.contains("I3[\"iter 3 · keep<br/>8.656 (-1.08%)\"]"));
        assert!(mmd.contains("I2[\"iter 2 · rejected<br/>correctness\"]"));
        assert!(mmd.contains("PR0[\"draft PR<br/>wseaton/vllm#8\"]"));
        assert!(mmd.contains("B --> I1 --> I2 --> I3 --> I4 --> I5 --> I6"));
        assert!(mmd.contains("I3 -.-> PR0"));
        assert!(mmd.contains("class I3 keep"));
        assert!(mmd.contains("class I1,I4,I5,I6 discard"));
        assert!(mmd.contains("class I2 rejected"));
    }

    #[test]
    fn parse_diff_counts_files_and_lines() {
        let diff = "diff --git a/src/a.rs b/src/a.rs\nindex 1..2 100644\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,2 @@\n-old\n+new\n+more\ndiff --git a/new.txt b/new.txt\nnew file mode 100644\n--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+hello\n";
        let e = parse_diff(diff);
        assert_eq!(e.files_changed, 2);
        assert_eq!(e.files, ["src/a.rs", "new.txt"]);
        assert_eq!(e.insertions, 3);
        assert_eq!(e.deletions, 1);
        let empty = parse_diff("");
        assert_eq!(empty.files_changed, 0);
        assert!(empty.files.is_empty());
    }

    #[test]
    fn parse_diff_keeps_content_lines_that_look_like_file_headers() {
        // Added "++i;" renders as "+++i;", removed "--j;" as "---j;": content, not headers.
        let diff = "diff --git a/x.c b/x.c\nindex 1..2 100644\n--- a/x.c\n+++ b/x.c\n@@ -1 +1 @@\n---j;\n+++i;\n";
        let e = parse_diff(diff);
        assert_eq!(e.files, ["x.c"]);
        assert_eq!(e.insertions, 1);
        assert_eq!(e.deletions, 1);
    }

    #[test]
    fn parse_diff_handles_quoted_and_spaced_paths() {
        let quoted = "diff --git \"a/dir/x y.rs\" \"b/dir/x y.rs\"\n--- \"a/dir/x y.rs\"\n+++ \"b/dir/x y.rs\"\n@@ -1 +1 @@\n-a\n+b\n";
        let e = parse_diff(quoted);
        assert_eq!(e.files, ["dir/x y.rs"]);
        assert_eq!((e.insertions, e.deletions), (1, 1));
        // Unquoted spaces: the symmetric a/P b/P split wins over rfind(" b/").
        let spaced = "diff --git a/has b/x.rs b/has b/x.rs\n@@ -1 +1 @@\n+z\n";
        assert_eq!(parse_diff(spaced).files, ["has b/x.rs"]);
    }

    #[test]
    fn classify_normalizes_decisions() {
        assert_eq!(classify("keep", Some(1.0)), "keep");
        assert_eq!(classify("discard", Some(1.0)), "discard");
        // A discard with no score means a rung rejected the candidate pre-measure.
        assert_eq!(classify("discarded", None), "rejected");
        assert_eq!(classify("discard", None), "rejected");
    }

    #[test]
    fn render_emits_all_four_formats_and_json_round_trips() {
        let input = FlowInput {
            session_log: fixture("run7-session.jsonl"),
            spans_json: None,
        };
        for format in [
            FlowFormat::Json,
            FlowFormat::Dot,
            FlowFormat::Mermaid,
            FlowFormat::Html,
        ] {
            assert!(!render(&input, format).unwrap().is_empty(), "{format:?}");
        }
        let ir = render(&input, FlowFormat::Json).unwrap();
        let m: FlowModel = serde_json::from_str(&ir).unwrap();
        assert_eq!(m.iterations.len(), 6);
        let err = FlowFormat::from_extension("txt").unwrap_err();
        assert!(err.to_string().contains(".json/.dot/.mmd/.html"), "{err}");
    }

    #[test]
    fn finite_drops_non_finite() {
        assert_eq!(finite(1.5), Some(1.5));
        assert_eq!(finite(f64::INFINITY), None);
        assert_eq!(finite(f64::NAN), None);
    }
}
