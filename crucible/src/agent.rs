//! Running one agent turn, shared by every front-end.
//!
//! The loop consumes a stream of [`AgentEvent`]s and does not care where they came
//! from. [`run_turn`] picks an [`AgentSource`] (resolved once from [`Args`]), hands
//! each line + decoded event to a `sink`, and returns a [`TurnOutcome`]: the turn's cost (the max
//! authoritative cost the agent reported, so the loop can budget on it) plus the transport failure
//! that stopped it, if any, so a turn that never ran is not mistaken for one that answered badly.
//!
//! Sources today, more by design:
//!
//! - [`AgentSource::LocalClaude`]: spawn `claude` here on this machine and parse its
//!   `--output-format stream-json` directly. Default for the `local`
//!   backend; see [`crate::stream_json`].
//! - [`AgentSource::OpenshellDriver`]: drive the OpenShell gateway over its gRPC API
//!   (the CLI remains only for file transfer) and parse Claude's `stream-json` from
//!   inside the sandbox.
//! - [`AgentSource::Command`]: run a deterministic shell command in the workspace for
//!   examples/tests; native `AgentEvent` JSON lines are decoded, everything else is raw.

use crate::event::{AgentEvent, RawStream, Tokens, cost_of, estimate_cost};
use crate::harness::StreamDecoder;
use crate::{Args, Paths};
use crucible_harness::OtelCollector;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::thread;

/// Which backend a locally-spawned agent turn runs against.
///
/// `Local` runs the agent on this machine (the original behavior). `Openshell`
/// runs it in an OpenShell sandbox (Landlock + egress policy), what an in-pod
/// loop uses so its turns are isolated; it needs a `--sandbox-image` carrying
/// the domain's toolbox binaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum AgentBackend {
    Local,
    Openshell,
    /// Run a fixed shell command as the "agent turn" (no LLM). A deterministic, free proposer
    /// for testing the engine end to end, see `examples/counter`.
    Command,
}

/// The agent's reasoning-effort tier, passed to Claude Code as `--effort <level>`. A closed set
/// (Claude Code 2.1: low|medium|high|xhigh|max); unset means we don't pass the flag and Claude
/// Code picks its own default. Per-domain because the right tier is task-dependent (mechanical
/// remove-deprecated fixes need ~none; algorithmic issues want real reasoning), and it's a harness
/// hyperparameter worth ablating: does more thinking find better fixes, or just better reward hacks?
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    /// The exact token Claude Code's `--effort` flag expects.
    pub(crate) fn as_flag(self) -> &'static str {
        match self {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Xhigh => "xhigh",
            ReasoningEffort::Max => "max",
        }
    }
}

/// Where a turn's events come from. Resolved once from [`Args`] (see
/// [`Args::agent_source`]); each variant knows how to launch its transport and expose
/// a stdout line stream plus an optional stderr stream.
///
/// Everything downstream (cost, sink, reporters) only ever sees [`AgentEvent`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentSource {
    /// Spawn `claude` directly and parse its `stream-json` (the `local` backend).
    LocalClaude,
    /// Drive the OpenShell gateway over its gRPC API for a sandboxed turn (the `openshell`
    /// backend; the CLI is used only for file transfer). Has its own multi-step flow, so
    /// [`run_turn`] delegates to [`crate::openshell::run::turn`] rather than the generic
    /// spawn/stream path.
    OpenshellDriver,
    /// The `command` backend: run a fixed shell command in the workspace as the proposal
    /// (deterministic, no LLM). Its stdout flows through the same sink; cost is 0.
    Command(String),
}

/// How a turn ended when it did not complete: the agent never produced output because the
/// transport itself failed. Distinct from a turn that ran and answered badly.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TurnFailure {
    /// The local transport could not be launched (bad argv, missing binary, harness error).
    #[error("agent spawn failed: {0}")]
    Spawn(String),
    /// The openshell driver's multi-step flow failed (gateway, provider, sandbox, exec, transfer).
    #[error("openshell orchestration failed: {0}")]
    Orchestration(String),
}

/// One turn's result: what it cost, and whether it ran at all. `failure` is `Some` when the turn
/// never completed; `cost_usd` still carries whatever the turn spent before it broke, so a partial
/// turn is not silently free.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    pub cost_usd: f64,
    pub failure: Option<TurnFailure>,
}

impl TurnOutcome {
    /// A turn that ran to completion at `cost_usd`. Says nothing about the quality of its output.
    pub fn completed(cost_usd: f64) -> Self {
        Self {
            cost_usd,
            failure: None,
        }
    }

    /// A turn that broke at `failure` after spending `cost_usd`.
    pub fn failed(cost_usd: f64, failure: TurnFailure) -> Self {
        Self {
            cost_usd,
            failure: Some(failure),
        }
    }

    pub fn failure(&self) -> Option<&TurnFailure> {
        self.failure.as_ref()
    }
}

pub(crate) fn supports_persistent_sessions(args: &Args) -> bool {
    backend_supports_persistent_sessions(args.agent_backend, args.harness())
}

/// Capability predicate used by runtime admission and scope preview.
pub(crate) fn backend_supports_persistent_sessions(
    backend: AgentBackend,
    harness: crate::harness::Harness,
) -> bool {
    backend == AgentBackend::Command || harness == crate::harness::Harness::Claude
}

/// A spawned transport: a child process plus the streams a turn reads from it.
struct Spawned {
    child: Child,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
}

impl AgentSource {
    /// Launch the transport for one turn. On failure returns the error so the caller
    /// can surface it through the sink exactly as before. `extra_env` carries harness-injected
    /// vars (the OTLP collector's `OTEL_*` matrix) that override the manifest `[agent].env`.
    fn spawn(
        &self,
        args: &Args,
        p: &Paths,
        prompt: &str,
        extra_env: &[(String, String)],
        session: Option<&crate::agent_session::SessionTurn>,
    ) -> std::io::Result<Spawned> {
        match self {
            AgentSource::LocalClaude => spawn_local(args, p, prompt, extra_env, session),
            AgentSource::Command(cmd) => spawn_command(cmd, args, p, prompt, extra_env, session),
            // The openshell driver runs a multi-step flow, not a single child; `run_turn`
            // intercepts it before `spawn`, so this is never reached.
            AgentSource::OpenshellDriver => Err(std::io::Error::other(
                "OpenshellDriver is driven by openshell::run::turn, not spawn",
            )),
        }
    }
}

/// Spawn the `command` backend's proposal: `sh -c <cmd>` in the workspace. Deterministic;
/// its output is echoed through the sink like any other turn.
fn spawn_command(
    cmd: &str,
    args: &Args,
    p: &Paths,
    prompt: &str,
    extra_env: &[(String, String)],
    session: Option<&crate::agent_session::SessionTurn>,
) -> std::io::Result<Spawned> {
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(cmd)
        .current_dir(&p.workspace)
        // The turn's prompt, so a deterministic stand-in can play more than one role
        // (a plan's coder vs its reviewer) by branching on it — the same env contract
        // as the shell runner's `--agent-cmd` stand-in.
        .env("CRUCIBLE_PROMPT", prompt)
        // The manifest's `[agent].env` and the harness's own vars, in the same precedence the
        // local backend uses. Without these a `command` pack's declared env silently vanished.
        .envs(args.env.iter().map(|(k, v)| (k, v)))
        .envs(extra_env.iter().map(|(k, v)| (k, v)))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(session) = session {
        c.env("CRUCIBLE_AGENT_SESSION", &session.logical_name)
            .env("CRUCIBLE_AGENT_SESSION_ID", &session.provider_id)
            .env(
                "CRUCIBLE_AGENT_SESSION_ACTION",
                if session.is_resume() {
                    "resume"
                } else {
                    "start"
                },
            );
    }
    // Same rule as `spawn_claude`: this child inherits the engine's process env, which under a
    // controller-dispatched run/scope/rank carries the engine's OWN trace parent. Whatever the
    // command shells out to (often `claude`) must not graft its spans onto our trace.
    c.env_remove("TRACEPARENT").env_remove("TRACESTATE");
    let mut child = c.spawn()?;
    let stdout = child
        .stdout
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    let stderr = child
        .stderr
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    Ok(Spawned {
        child,
        stdout,
        stderr,
    })
}

/// Spawn the harness CLI directly in the workspace for a local turn. The manifest's
/// `[agent].env` already carries the Vertex/API-key cred config; we add a few harness
/// defaults (manifest env still wins).
fn spawn_local(
    args: &Args,
    p: &Paths,
    prompt: &str,
    extra_env: &[(String, String)],
    session: Option<&crate::agent_session::SessionTurn>,
) -> std::io::Result<Spawned> {
    let argv = match session {
        Some(session) => args.harness().local_session_argv(args, prompt, session)?,
        None => args.harness().local_argv(args, prompt),
    };
    let Some((program, rest)) = argv.split_first() else {
        return Err(std::io::Error::other("harness produced an empty argv"));
    };
    let mut cmd = Command::new(program);
    cmd.args(rest)
        .current_dir(&p.workspace)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // This local child inherits the engine's process env wholesale (Command's default), which under
    // a controller-dispatched run carries the engine's own TRACEPARENT/TRACESTATE. That is the
    // engine's run-trace parent, not the agent's, never hand it to the agent, or its OTLP would
    // silently graft onto our run trace. Strip it (the sandbox path already can't leak it: its
    // env_script is built from the manifest env, not the process env).
    cmd.env_remove("TRACEPARENT").env_remove("TRACESTATE");
    // Harness defaults first, so a manifest `[agent].env` override takes precedence.
    for (k, v) in args.harness().local_env_defaults() {
        cmd.env(k, v);
    }
    for (k, v) in &args.env {
        cmd.env(k, v);
    }
    // Harness-injected env (the OTLP collector's OTEL_* matrix) wins over the manifest env.
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;
    let stdout = child
        .stdout
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    let stderr = child
        .stderr
        .take()
        .map(|s| Box::new(s) as Box<dyn Read + Send>);
    Ok(Spawned {
        child,
        stdout,
        stderr,
    })
}

/// Run one agent turn against the source resolved from `args`. `sink(raw_line, stream,
/// event)` is called per output line; returns the turn's [`TurnOutcome`]: the highest cost the
/// agent reported (0 if none) plus the transport failure that stopped it, if any.
pub fn run_turn(
    args: &Args,
    p: &Paths,
    prompt: &str,
    json: bool,
    sink: impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> TurnOutcome {
    run_turn_with_session(args, p, prompt, json, None, sink)
}

/// Run a turn using a prepared session; the caller commits successful transport afterward.
pub(crate) fn run_turn_with_session(
    args: &Args,
    p: &Paths,
    prompt: &str,
    json: bool,
    session: Option<&crate::agent_session::SessionTurn>,
    sink: impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> TurnOutcome {
    let source = args.agent_source();
    // The openshell driver owns a multi-step turn (gateway/provider/sandbox/exec/download),
    // not a single streamed child, delegate to its module rather than the generic path. It
    // reaches the engine runtime handle itself (see `crate::engine::handle`).
    if source == AgentSource::OpenshellDriver {
        return crate::openshell::run::turn(args, p, prompt, json, session, sink);
    }
    run_turn_with(&source, args, p, prompt, json, session, sink)
}

/// Inner driver, generic over the source. Split out so the source is explicit and
/// testable; [`run_turn`] is the convenience that resolves it from `args`.
fn run_turn_with(
    source: &AgentSource,
    args: &Args,
    p: &Paths,
    prompt: &str,
    json: bool,
    session: Option<&crate::agent_session::SessionTurn>,
    mut sink: impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> TurnOutcome {
    // Start the in-process OTLP collector for a local claude turn when telemetry is opted in
    // . A bind failure degrades to telemetry-off: no collector, no otel_summary, the
    // pricing-table estimate stays the cost fallback. The `otel.jsonl` lands next to the session
    // log, the `otel-log` Tier 2 artifact.
    let collector = if matches!(source, AgentSource::LocalClaude)
        && args.harness().otel_capable()
        && otel_enabled(args)
    {
        match OtelCollector::start(
            p.state.join("otel.jsonl"),
            "127.0.0.1",
            0,
            otel_forward(args),
        ) {
            Ok(c) => Some(c),
            Err(e) => {
                let ev = AgentEvent::Raw {
                    text: format!("otel collector unavailable (cost falls back to estimate): {e}"),
                    stream: RawStream::Stderr,
                };
                sink("", RawStream::Stderr, Some(&ev));
                None
            }
        }
    } else {
        None
    };
    let extra_env = collector
        .as_ref()
        .map(|c| crucible_harness::otel_env(&c.local_endpoint()))
        .unwrap_or_default();
    let meters = collector.as_ref().map(OtelCollector::meters);

    let spawned = match source.spawn(args, p, prompt, &extra_env, session) {
        Ok(s) => s,
        Err(e) => {
            let message = format!("failed to launch agent source: {e}");
            let ev = AgentEvent::Error {
                error_type: "spawn".into(),
                message: message.clone(),
            };
            sink("", RawStream::Stderr, Some(&ev));
            return TurnOutcome::failed(0.0, TurnFailure::Spawn(message));
        }
    };
    let Spawned {
        mut child,
        stdout,
        stderr,
    } = spawned;

    let child_pid = child.id() as i32;
    crate::pid_registry::register(child_pid);

    // Drain stderr into a buffer on a thread (keeps the sink single-threaded);
    // replay it through the sink after the process exits.
    let stderr_handle = thread::spawn(move || {
        let mut lines = Vec::new();
        if let Some(err) = stderr {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                lines.push(line);
            }
        }
        lines
    });

    let mut cost = 0.0_f64;
    // Highest cumulative token sample seen, for the estimate fallback below.
    let mut best_tokens: Option<Tokens> = None;
    if let Some(out) = stdout {
        if matches!(source, AgentSource::LocalClaude) {
            // Local agent: decode via the harness's stream decoder (shared with the openshell
            // exec path).
            let decoder = args
                .harness()
                .decoder(args, meters.as_ref(), tool_io_full(args));
            let (c, bt) = pump_stream(out, json, decoder, &mut sink);
            cost = c;
            best_tokens = bt;
        } else {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let ev = line_event(&line, RawStream::Stdout);
                if let Some(e) = ev.as_ref() {
                    account(e, &mut cost, &mut best_tokens);
                }
                sink(&line, RawStream::Stdout, ev.as_ref());
            }
        }
    }

    let _ = child.wait();
    crate::pid_registry::deregister(child_pid);

    if let Ok(errlines) = stderr_handle.join() {
        for line in errlines {
            let ev = line_event(&line, RawStream::Stderr);
            if let Some(e) = ev.as_ref() {
                account(e, &mut cost, &mut best_tokens);
            }
            sink(&line, RawStream::Stderr, ev.as_ref());
        }
    }

    // A backfill harness has no machine-readable event stream: its result events + cost arrive
    // post-hoc from the local session store. Dead for claude (`backfill_required` is false).
    if matches!(source, AgentSource::LocalClaude)
        && args.harness().backfill_required()
        && let Some(artifacts) = args.harness().local_backfill(p)
    {
        for ev in &artifacts.events {
            account(ev, &mut cost, &mut best_tokens);
            sink("", RawStream::Stdout, Some(ev));
        }
        if let Some(c) = artifacts.cost_usd {
            cost = cost.max(c);
        }
    }

    // The agent has fully exited, so its last OTLP export has landed: roll the capture up into the
    // `otel_summary` event (authoritative cost, per-model usage, API latency, active time). The
    // event finally has a producer again. Routed through `account` so `cost_of`'s prefer-OTEL rule
    // puts the authoritative number on the turn's cost.
    if let Some(collector) = collector {
        if let Some(summary) = collector.summary() {
            let ev = summary.to_event();
            account(&ev, &mut cost, &mut best_tokens);
            sink("", RawStream::Stdout, Some(&ev));
        }
        collector.stop();
    }

    // No authoritative cost (telemetry off, or the OpenShell sandbox runs without the local
    // collector) but tokens flowed, estimate from model pricing so `--max-cost` budgeting still
    // has a signal. The agent's real number wins whenever `result`/OTEL reported one above.
    if cost == 0.0
        && let Some(t) = &best_tokens
    {
        cost = estimate_cost(&args.model, t);
    }
    TurnOutcome::completed(cost)
}

/// Whether the in-process OTLP collector should run for this turn. Opt-in ("result bundling": "result
/// mode is opt-in exactly like `--marker`"), keyed on `CRUCIBLE_OTEL` being truthy in the
/// manifest's `[agent].env` or the process environment. Off by default keeps a local run's behavior
/// byte-identical to today (pricing-table estimate). Promoting this to a first-class manifest field
/// is a follow-up.
pub(crate) fn otel_enabled(args: &Args) -> bool {
    let truthy = |v: &str| matches!(v.trim(), "1" | "true" | "yes" | "on");
    if let Some((_, v)) = args.env.iter().find(|(k, _)| k == "CRUCIBLE_OTEL") {
        return truthy(v);
    }
    std::env::var("CRUCIBLE_OTEL")
        .map(|v| truthy(&v))
        .unwrap_or(false)
}

/// The upstream OTLP/HTTP receiver the collector mirrors this turn's agent telemetry to, named by
/// `CRUCIBLE_OTEL_FORWARD` in the manifest's `[agent].env` or the process environment. Unset keeps
/// the collector a terminal sink (capture stays in `otel.jsonl`).
///
/// Spans are re-parented onto the turn span before they leave, so the agent's `llm_request` spans
/// nest under the run instead of forming an orphan trace: the agent's exporter cannot adopt a
/// parent itself, since the OTel JS SDK does not read `TRACEPARENT` from the environment.
pub(crate) fn otel_forward(args: &Args) -> Option<crucible_harness::OtelForward> {
    let endpoint = match args.env.iter().find(|(k, _)| k == "CRUCIBLE_OTEL_FORWARD") {
        Some((_, v)) => v.clone(),
        None => std::env::var("CRUCIBLE_OTEL_FORWARD").ok()?,
    };
    if endpoint.trim().is_empty() {
        return None;
    }
    let tp = crate::engine::current_trace_env().map(|(tp, _)| tp);
    Some(crucible_harness::OtelForward::new(endpoint, tp.as_deref()))
}

/// Whether session-log tool events carry full inputs and result excerpts
/// (`CRUCIBLE_SESSION_TOOL_IO=full` in the manifest `[agent].env` or the process
/// env). Off by default: the compact name+summary form keeps the log small, but
/// made a run's edits unreconstructable without diffing the PR — this flag exists
/// for runs someone will want to review.
pub(crate) fn tool_io_full(args: &Args) -> bool {
    let full = |v: &str| v.trim().eq_ignore_ascii_case("full");
    if let Some((_, v)) = args
        .env
        .iter()
        .find(|(k, _)| k == "CRUCIBLE_SESSION_TOOL_IO")
    {
        return full(v);
    }
    std::env::var("CRUCIBLE_SESSION_TOOL_IO")
        .map(|v| full(&v))
        .unwrap_or(false)
}

/// Decode a command-backend line. Native `AgentEvent` JSON is preserved; all other
/// non-empty output is raw so the UI/session log do not lose it.
fn line_event(line: &str, stream: RawStream) -> Option<AgentEvent> {
    if stream == RawStream::Stdout
        && let Some(ev) = AgentEvent::from_json_line(line)
    {
        return Some(ev);
    }
    let text = line.trim();
    (!text.is_empty()).then(|| AgentEvent::Raw {
        text: text.to_string(),
        stream,
    })
}

/// The decoder-driving core of an agent stdout pump: one [`StreamDecoder`] plus the
/// turn's running (max authoritative cost, largest token sample). Pure and sync, no I/O. Each
/// complete stdout line is [`push`](StreamPump::push)ed in; the local-child path feeds it off a
/// `BufReader` ([`pump_stream`]), the openshell exec path feeds it lines straight off the
/// gRPC stream. Splitting the loop from the byte source is what lets the async exec path reuse the
/// exact same accounting + sink dispatch from any line source (BufReader or gRPC stream).
pub(crate) struct StreamPump {
    decoder: StreamDecoder,
    cost: f64,
    best_tokens: Option<Tokens>,
}

impl StreamPump {
    /// A fresh pump over the harness's `decoder` (see [`crate::harness::Harness::decoder`]).
    pub(crate) fn new(decoder: StreamDecoder) -> Self {
        Self {
            decoder,
            cost: 0.0,
            best_tokens: None,
        }
    }

    /// Feed one complete stdout line: decode it into [`AgentEvent`]s, fold each into the
    /// running totals, and drive `sink`. `json` matches the front-end mode, `true` consumers read
    /// the event, `false` (console) prints a human line.
    pub(crate) fn push(
        &mut self,
        line: &str,
        json: bool,
        sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
    ) {
        for ev in self.decoder.push(line) {
            account(&ev, &mut self.cost, &mut self.best_tokens);
            if json {
                sink(line, RawStream::Stdout, Some(&ev));
            } else if let Some(human) = human_line(&ev) {
                sink(&human, RawStream::Stdout, Some(&ev));
            }
        }
    }

    /// The turn's (max authoritative cost, largest token sample) once the stream ends.
    pub(crate) fn finish(self) -> (f64, Option<Tokens>) {
        (self.cost, self.best_tokens)
    }
}

/// Stream agent stdout from `reader`, decoding each line into [`AgentEvent`]s via `decoder` and
/// driving `sink`, returning the turn's (max authoritative cost, largest token sample). The
/// local-child wrapper around [`StreamPump`]: it loops `BufReader` lines into the pump. Used by
/// the local path ([`run_turn_with`]); the openshell `sandbox exec` path drives a
/// [`StreamPump`] directly off its gRPC stream.
pub(crate) fn pump_stream(
    reader: impl Read,
    json: bool,
    decoder: StreamDecoder,
    sink: &mut impl FnMut(&str, RawStream, Option<&AgentEvent>),
) -> (f64, Option<Tokens>) {
    let mut pump = StreamPump::new(decoder);
    for line in BufReader::new(reader).lines().map_while(Result::ok) {
        pump.push(&line, json, sink);
    }
    pump.finish()
}

/// Fold one event into the turn's running totals: the highest authoritative cost seen,
/// and the largest token sample (the estimate fallback when no cost is reported).
fn account(ev: &AgentEvent, cost: &mut f64, best_tokens: &mut Option<Tokens>) {
    if let Some(c) = cost_of(ev) {
        *cost = cost.max(c);
    }
    if let AgentEvent::Tokens(t) = ev
        && best_tokens.as_ref().is_none_or(|b| t.total >= b.total)
    {
        *best_tokens = Some(t.clone());
    }
}

/// Render an event as a human-readable line for the headless console. `None` for events that stay quiet
/// in a log (init/result/lifecycle); token/tool/text/thinking/retry/error show.
fn human_line(ev: &AgentEvent) -> Option<String> {
    match ev {
        AgentEvent::Text { delta } => Some(delta.clone()),
        AgentEvent::Thinking { delta } => Some(format!("\u{1f9e0} {delta}")),
        AgentEvent::Tool {
            name,
            summary,
            subagent,
            ..
        } => {
            let icon = if *subagent { "\u{1f916}" } else { "\u{1f527}" };
            Some(format!("{icon} {name} {summary}").trim_end().to_string())
        }
        AgentEvent::Tokens(t) => Some(format!(
            "\u{1f4ca} TOKENS in={} out={} cache_r={} cache_w={} total={}",
            t.input, t.output, t.cache_read, t.cache_write, t.total
        )),
        AgentEvent::Retry {
            attempt,
            max,
            error,
        } => Some(format!("\u{1f504} Retry {attempt}/{max} {error}")),
        AgentEvent::Error {
            error_type,
            message,
        } => Some(format!("\u{274c} Error: {error_type}: {message}")),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Parse a bare arg list into [`Args`] via the top-level CLI (the loop is the
    /// default subcommand), so we exercise the real clap wiring.
    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["crucible"];
        argv.extend_from_slice(extra);
        crate::Cli::parse_from(argv).run
    }

    #[test]
    fn default_backend_spawns_claude_directly() {
        assert_eq!(args(&[]).agent_source(), AgentSource::LocalClaude);
    }

    #[test]
    fn openshell_backend_routes_to_the_in_rust_driver() {
        let src = args(&["--agent-backend", "openshell"]).agent_source();
        assert_eq!(src, AgentSource::OpenshellDriver);
    }

    #[test]
    fn tool_io_defaults_compact_and_manifest_env_opts_in() {
        let mut a = args(&[]);
        assert!(!tool_io_full(&a), "compact by default");
        a.env
            .push(("CRUCIBLE_SESSION_TOOL_IO".to_string(), "full".to_string()));
        assert!(tool_io_full(&a));
        // Any value other than "full" stays compact.
        a.env.pop();
        a.env
            .push(("CRUCIBLE_SESSION_TOOL_IO".to_string(), "yes".to_string()));
        assert!(!tool_io_full(&a));
    }

    /// A temp workspace for a real turn (the `command` backend shells out for real).
    fn workspace(name: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!(
            "crucible-agent-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(dir.join("state")).expect("mkdir workspace");
        Paths::for_worktree(dir, None)
    }

    #[test]
    fn a_turn_that_ran_reports_no_failure() {
        let a = args(&[]);
        let p = workspace("ran");
        let src = AgentSource::Command("echo ran".to_string());
        let mut lines = Vec::new();
        let outcome = run_turn_with(&src, &a, &p, "prompt", false, None, |line, _s, _ev| {
            lines.push(line.to_string())
        });
        let _ = std::fs::remove_dir_all(&p.workspace);
        assert_eq!(outcome.failure(), None, "the command ran to completion");
        assert_eq!(outcome.cost_usd, 0.0, "the command backend is free");
        assert!(lines.iter().any(|l| l == "ran"), "stdout reached the sink");
    }

    /// A turn whose transport never launches is a failure, not a $0 turn that answered nothing.
    /// `OpenshellDriver` is the one source whose `spawn` always errors (its flow is driven
    /// elsewhere), so it exercises the launch-failure path without a broken fixture binary.
    #[test]
    fn a_turn_that_never_launched_reports_a_spawn_failure() {
        let a = args(&[]);
        let p = workspace("nospawn");
        let mut errors = Vec::new();
        let outcome = run_turn_with(
            &AgentSource::OpenshellDriver,
            &a,
            &p,
            "prompt",
            false,
            None,
            |_line, _s, ev| {
                if let Some(AgentEvent::Error { message, .. }) = ev {
                    errors.push(message.clone());
                }
            },
        );
        let _ = std::fs::remove_dir_all(&p.workspace);
        assert_eq!(outcome.cost_usd, 0.0);
        match outcome.failure() {
            Some(TurnFailure::Spawn(msg)) => {
                assert!(msg.contains("failed to launch agent source"), "{msg}")
            }
            other => panic!("expected a spawn failure, got {other:?}"),
        }
        assert_eq!(errors.len(), 1, "the sink still sees the error event");
    }

    #[test]
    fn command_backend_uses_fixed_command() {
        let mut a = args(&["--agent-backend", "command"]);
        a.agent_cmd = Some("./bump.nu".into());
        assert_eq!(a.agent_source(), AgentSource::Command("./bump.nu".into()));
    }
}
