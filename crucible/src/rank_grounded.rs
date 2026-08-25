//! `crucible rank-grounded`: one code-grounded triage-ranking turn. The controller's cheap text-only
//! ranker runs first; when it is unsure (or is configured to always ground its verdicts), it shells
//! THIS subcommand, which hands the agent the actual checkout so it can grep before tiering,
//! "ranking decides IF, scope decides HOW".
//!
//! The turn is read-only by intent: the prompt forbids edits, and it is contained by running in a
//! throwaway `git worktree` detached at the checkout's HEAD, so a stray write lands in the disposable
//! worktree and never touches the caller's checkout (which the controller owns and re-uses). The
//! agent ends its turn with a one-line verdict JSON (`{tier,rationale,confidence}`), the
//! scope-propose stdout-contract pattern, which this command parses, augments with the turn's cost,
//! and prints back out for the caller to fold into the ledger.
//!
//! Backends mirror `scope`'s propose turn: `--agent-cmd` selects the deterministic `command` backend
//! (tests / hermetic runs), otherwise `--agent-backend` picks the real one, `local` claude on a
//! laptop, `openshell` in-pod (the sandbox image carries the claude CLI the loop image does not).

use crate::Paths;
use crate::activity::ActivityFeed;
use crate::agent::{self, AgentBackend, TurnFailure, TurnOutcome};
use crate::event::{AgentEvent, RawStream};
use crate::harness::HarnessRuntime;
use anyhow::{Context, Result};
use clap::Parser;
use crucible_contract::Disposition;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const RANK_GROUNDED_PROMPT: &str = include_str!("prompts/rank-grounded.md");

/// The prefix of the single-line verdict marker `--marker` emits as the command's last output. The
/// controller's WorkPod log scraper matches the same shared literal from `crucible-contract`.
pub use crucible_contract::VERDICT_MARKER;

/// CLI-level options for `crucible rank-grounded`. Also the clap `Args` group behind
/// [`crate::Cmd::RankGrounded`].
#[derive(clap::Args)]
pub struct RankGroundedArgs {
    /// Issue to rank (`owner/repo#N`).
    #[arg(long)]
    pub issue: String,
    /// Existing checkout of the code under test (caller-owned; never cloned or mutated).
    #[arg(long)]
    pub workspace: PathBuf,
    /// Cap on the turn's cost in USD.
    #[arg(long, default_value_t = 5.0)]
    pub max_cost: f64,
    /// Emit the verdict as one JSON object instead of human lines.
    #[arg(long)]
    pub json: bool,
    /// Also print a trailing `CRUCIBLE_VERDICT: {…}` marker line (WorkPod log scraping).
    #[arg(long)]
    pub marker: bool,
    /// Real agent backend for the turn; ignored when `--agent-cmd` is set.
    #[arg(long, value_enum, default_value_t = crate::agent::AgentBackend::Local)]
    pub agent_backend: AgentBackend,
    /// Sandbox image for `--agent-backend openshell`.
    #[arg(long)]
    pub sandbox_image: Option<String>,
    /// Override the turn's agent with a `command`-backend script (test hook).
    #[arg(long, hide = true)]
    pub agent_cmd: Option<String>,
    /// Override the turn's model (env fallback: `CRUCIBLE_RANK_MODEL`).
    #[arg(long)]
    pub model: Option<String>,
    /// Compute driver for the turn's gateway. Mirrors the loop's `--compute-driver`.
    #[arg(long, value_enum, default_value_t = crate::openshell::gateway::ComputeDriver::Podman)]
    pub compute_driver: crate::openshell::gateway::ComputeDriver,
}

/// A parsed, validated grounded verdict, the disposition canonicalized to its
/// `T0|T1|T2|T3|N|stale` spelling and confidence to `high|low`, so the JSON this command prints
/// back is always well-formed. `stale` supersedes every tier definition: if the requested change
/// already exists in the checkout, the verdict MUST be `stale`, never a tier, see the prompt's
/// hard rule and [`crucible_contract::Disposition`]'s doc comment for why it's not folded into
/// the tier vocabulary.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub disposition: Disposition,
    pub rationale: String,
    pub confidence: String,
}

/// Why a grounded turn yielded no verdict. The two cases are not interchangeable: a turn that
/// never started tells the caller to look at the pod, a completed turn without a verdict tells it
/// to look at the model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GroundedError {
    /// The turn itself failed. The agent may still have printed something on its way down;
    /// that tail rides on [`GroundedReport::output_tail`].
    #[error("the turn never completed: {0}")]
    Turn(#[from] TurnFailure),
    /// The turn ran and its last line was not a verdict.
    #[error("the agent produced no parseable verdict on its last line")]
    NoVerdict,
}

impl GroundedError {
    /// The machine-readable discriminant on the verdict marker, so the controller records which
    /// failure it was instead of re-deriving it from prose.
    fn kind(&self) -> &'static str {
        match self {
            GroundedError::Turn(_) => "turn_failed",
            GroundedError::NoVerdict => "no_verdict",
        }
    }
}

/// The turn's outcome: the parsed verdict (absent when the agent produced no parseable one on its
/// last line), the turn's cost, and whether that cost blew the budget.
#[derive(Debug, Clone)]
pub struct GroundedReport {
    pub verdict: Option<Verdict>,
    pub cost_usd: f64,
    pub over_budget: bool,
    /// Set when no verdict was parsed, the reason, for the human/stderr path.
    pub error: Option<GroundedError>,
    /// The tail of whatever the agent printed, carried only on the failure path. A turn that dies
    /// during startup prints its diagnostic and nothing else; without this the report says the
    /// turn failed and drops the one line that says why.
    pub output_tail: Option<String>,
}

impl GroundedReport {
    /// The one-line JSON the caller (the controller's grounded arm) parses off stdout: the verdict
    /// fields plus the turn's cost, or an `error` object when the agent emitted no parseable verdict.
    fn to_json(&self) -> serde_json::Value {
        match &self.verdict {
            Some(v) => serde_json::json!({
                "tier": v.disposition.as_str(),
                "rationale": v.rationale,
                "confidence": v.confidence,
                "cost_usd": self.cost_usd,
                "over_budget": self.over_budget,
            }),
            None => serde_json::json!({
                "error": self
                    .error
                    .as_ref()
                    .map(GroundedError::to_string)
                    .unwrap_or_else(|| "no verdict".to_string()),
                "error_kind": self.error.as_ref().map(GroundedError::kind).unwrap_or("no_verdict"),
                "output_tail": self.output_tail,
                "cost_usd": self.cost_usd,
                "over_budget": self.over_budget,
            }),
        }
    }
}

/// The wire shape of the agent's verdict line (the scope-propose stdout contract): a strict object,
/// tolerant only of an absent `confidence` (defaults to `high`).
#[derive(Debug, Deserialize)]
struct RawVerdict {
    tier: String,
    rationale: String,
    #[serde(default)]
    confidence: Option<String>,
}

fn render_prompt(title: &str, body: &str, labels: &[String]) -> String {
    RANK_GROUNDED_PROMPT
        .replace("{{TITLE}}", title)
        .replace("{{LABELS}}", &labels.join(", "))
        .replace("{{BODY}}", body)
}

/// A throwaway path under the temp dir (NOT created, `git worktree add` wants a fresh path).
fn worktree_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "crucible-rank-grounded-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ))
}

fn git(workspace: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .status()
        .context("running git")?;
    if !status.success() {
        return Err(GitFailed {
            subcommand: args.first().unwrap_or(&"").to_string(),
            status,
        }
        .into());
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("git {subcommand:?} failed ({status})")]
struct GitFailed {
    subcommand: String,
    status: std::process::ExitStatus,
}

/// Accumulate the agent's textual output so the verdict can be scraped from its last line: streamed
/// answer text (`Text` deltas, the real claude path) and the `command` backend's raw stdout lines
/// (`Raw`) both carry the verdict; a native cost/tool/token event does not, so it is skipped here
/// (its cost is accounted separately by `run_turn`).
fn accumulate(ev: Option<&AgentEvent>, buf: &mut String) {
    match ev {
        Some(AgentEvent::Text { delta }) => buf.push_str(delta),
        Some(AgentEvent::Raw {
            text,
            stream: RawStream::Stdout,
        }) => {
            buf.push_str(text);
            buf.push('\n');
        }
        // A backend that dies at spawn/auth must be visible in pod logs, not silently a $0 turn.
        Some(AgentEvent::Error {
            error_type,
            message,
        }) => eprintln!("[crucible rank-grounded] agent error ({error_type}): {message}"),
        _ => {}
    }
}

/// Strip a lone markdown code fence off a candidate verdict line.
fn strip_fence(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

/// Scan the agent's accumulated output from the end for the last line that parses as a valid verdict
/// (the stdout contract: the verdict is the final JSON object the agent emits). Models sometimes
/// pretty-print the object across lines despite the single-line instruction, so when no single
/// line parses, fall back to the last brace-balanced `{...}` block in the output. `None` when
/// nothing parses, a ranking failure the caller treats as "no grounded verdict".
fn extract_verdict(buf: &str) -> Option<Verdict> {
    for line in buf.lines().rev() {
        let cleaned = strip_fence(line);
        if cleaned.is_empty() {
            continue;
        }
        if let Ok(v) = parse_verdict(cleaned) {
            return Some(v);
        }
    }
    last_balanced_block(buf)
}

/// The pretty-printed fallback: walk `}`s from the end of the output, match each to its opening
/// `{` by depth (braces are ASCII, so byte indexing is safe), and return the last block that
/// parses as a verdict.
fn last_balanced_block(buf: &str) -> Option<Verdict> {
    let bytes = buf.as_bytes();
    let mut end = bytes.len();
    while let Some(close) = buf[..end].rfind('}') {
        let mut depth = 0i32;
        for i in (0..=close).rev() {
            match bytes[i] {
                b'}' => depth += 1,
                b'{' => {
                    depth -= 1;
                    if depth == 0 {
                        if let Ok(v) = parse_verdict(&buf[i..=close]) {
                            return Some(v);
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
        end = close;
    }
    None
}

/// Why a candidate verdict line was rejected. Every arm means "skip this line", so the caller
/// keeps scanning rather than trusting a half-formed verdict.
#[derive(Debug, thiserror::Error)]
enum VerdictError {
    #[error("verdict line is not valid JSON")]
    NotJson(#[from] serde_json::Error),
    #[error("empty rationale")]
    EmptyRationale,
    #[error("unknown tier")]
    UnknownTier(#[from] crucible_contract::TierParseError),
    #[error("unknown confidence `{got}`")]
    UnknownConfidence { got: String },
}

/// Parse + validate one verdict line: an unknown tier/confidence or an empty rationale is a parse
/// error (never a silent default), so a half-formed line is skipped rather than trusted.
fn parse_verdict(raw: &str) -> Result<Verdict, VerdictError> {
    let v: RawVerdict = serde_json::from_str(raw)?;
    if v.rationale.trim().is_empty() {
        return Err(VerdictError::EmptyRationale);
    }
    let disposition = Disposition::parse(&v.tier)?;
    let confidence = match v.confidence.as_deref() {
        None | Some("high") => "high",
        Some("low") => "low",
        Some(other) => {
            return Err(VerdictError::UnknownConfidence {
                got: other.to_owned(),
            });
        }
    };
    Ok(Verdict {
        disposition,
        rationale: v.rationale,
        confidence: confidence.to_string(),
    })
}

fn grounded_paths(scratch: &Path) -> Paths {
    Paths {
        workspace: scratch.to_path_buf(),
        skills: None,
        steer: scratch.join("STEER.md"),
        state: scratch.join("state"),
        session_log: scratch.join("state/session.jsonl"),
        control: scratch.join("state/control.json"),
        admissions: scratch.join("state/admissions.jsonl"),
        escalation: scratch.join("ESCALATION.json"),
        provisioning: scratch.join("PROVISIONING_PENDING.json"),
    }
}

/// Run the grounded turn in a throwaway worktree of `workspace` and return its outcome plus the
/// agent's accumulated output. The worktree (detached at HEAD) is the read-only-intent mechanism:
/// any write the agent makes lands here and is discarded on `worktree remove --force`, so the
/// caller's checkout is never touched.
fn run_grounded_turn(
    workspace: &Path,
    prompt: &str,
    a: &RankGroundedArgs,
) -> Result<(TurnOutcome, String)> {
    let scratch = worktree_path();
    git(
        workspace,
        &[
            "worktree",
            "add",
            "--detach",
            &scratch.to_string_lossy(),
            "HEAD",
        ],
    )
    .context("creating the read-only worktree for the grounded turn")?;

    let mut args = crate::Cli::parse_from(["crucible"]).run;
    match &a.agent_cmd {
        Some(cmd) => {
            args.agent_backend = AgentBackend::Command;
            args.agent_cmd = Some(cmd.clone());
        }
        None => args.agent_backend = a.agent_backend,
    }
    args.sandbox_image = a.sandbox_image.clone();
    args.compute_driver = a.compute_driver;
    // Manifest-less turn: the Vertex agent env normally supplied by `[agent].env` comes from the
    // turn pod's own env instead. Only for a Vertex-authenticated harness.
    if args.harness().auth_provider() == crate::harness::AuthProvider::Vertex {
        crate::openshell::relay_vertex_env(&mut args.env);
    }
    // Flag wins; CRUCIBLE_RANK_MODEL lets a parent (rank-compare, the controller's escalation
    // arm) pin the model without threading a parameter through every layer.
    let model = a.model.clone().or_else(|| {
        std::env::var("CRUCIBLE_RANK_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
    });
    if let Some(model) = model {
        args.model = model;
    }
    let paths = grounded_paths(&scratch);
    let _ = std::fs::create_dir_all(&paths.state);

    let mut buf = String::new();
    // The turn's own narration on stdout: stage banners, tool calls and the agent's stderr, so a
    // turn that dies mid-orchestration leaves more than a 7-line pod log. Bounded by the feed's
    // caps and byte budget, and always before the verdict marker `run` prints last.
    let model = args.model.clone();
    let mut activity = ActivityFeed::new(crucible_contract::RANK_ACTIVITY_MARKER, a.marker);
    let outcome = agent::run_turn(&args, &paths, prompt, false, |_line, _stream, ev| {
        if let Some(ev) = ev {
            activity.observe(&model, ev);
        }
        accumulate(ev, &mut buf)
    });

    // Discard the worktree (and its detached commits/untracked writes), the caller's checkout stays
    // pristine. Best-effort: a leaked worktree is pruned by the next `worktree add`/`prune`.
    let _ = git(
        workspace,
        &["worktree", "remove", "--force", &scratch.to_string_lossy()],
    );
    let _ = std::fs::remove_dir_all(&scratch);
    Ok((outcome, buf))
}

/// Drive one grounded ranking turn and return its report. Pure of process exit / printing, [`run`]
/// owns rendering + the exit code, so tests drive this directly.
pub fn execute(a: &RankGroundedArgs) -> Result<GroundedReport> {
    let (repo, number) = crate::issue::parse_ref(&a.issue)?;
    let issue = crate::issue::fetch(&repo, number, "crucible-rank-grounded")
        .with_context(|| format!("fetching {} for grounded ranking", a.issue))?;
    let prompt = render_prompt(&issue.title, &issue.body, &issue.labels);
    let (outcome, buf) = run_grounded_turn(&a.workspace, &prompt, a)?;
    Ok(report_from_turn(outcome, &buf, a.max_cost))
}

/// Classify one finished turn into a report. A transport failure outranks the verdict scrape: an
/// agent that never ran cannot have written a bad last line.
fn report_from_turn(outcome: TurnOutcome, buf: &str, max_cost: f64) -> GroundedReport {
    let over_budget = max_cost > 0.0 && outcome.cost_usd > max_cost;
    let (verdict, error) = match outcome.failure {
        Some(failure) => (None, Some(GroundedError::Turn(failure))),
        None => match extract_verdict(buf) {
            Some(v) => (Some(v), None),
            None => (None, Some(GroundedError::NoVerdict)),
        },
    };
    let output_tail = error.is_some().then(|| tail_of(buf)).flatten();
    GroundedReport {
        verdict,
        cost_usd: outcome.cost_usd,
        over_budget,
        error,
        output_tail,
    }
}

/// Bound on the replayed agent output: enough for a startup diagnostic and its context, small
/// enough that a chatty turn cannot flood the marker line the controller scrapes.
const OUTPUT_TAIL_LINES: usize = 20;
const OUTPUT_TAIL_BYTES: usize = 4_096;

/// The last non-blank lines of the agent's accumulated output, or `None` if it printed nothing.
fn tail_of(buf: &str) -> Option<String> {
    let lines: Vec<&str> = buf.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(OUTPUT_TAIL_LINES);
    let tail = lines[start..].join("\n");
    let tail = if tail.len() > OUTPUT_TAIL_BYTES {
        let mut cut = tail.len() - OUTPUT_TAIL_BYTES;
        while cut < tail.len() && !tail.is_char_boundary(cut) {
            cut += 1;
        }
        tail[cut..].to_string()
    } else {
        tail
    };
    (!tail.is_empty()).then_some(tail)
}

/// CLI entry point: run the turn, print the verdict (JSON or human lines), and exit nonzero if the
/// agent produced no verdict OR the turn ran over `--max-cost` (the `scope` visible-guard shape).
pub fn run(a: RankGroundedArgs) -> Result<()> {
    let json = a.json;
    // A controller-dispatched rank pod: adopt the dispatch as this turn's trace parent so the
    // `openshell_turn` nests under the controller's tree instead of floating orphaned. `None` (a
    // local run, or telemetry off) changes nothing. Closed right after the turn, not at fn end,
    // the no-verdict/over-budget tail exits via `process::exit`, which would skip the drop and
    // lose the span.
    let turn_span =
        crate::engine::turn_span(crate::engine::TurnSpanKind::RankGrounded, Some(&a.issue));
    let report = {
        let _turn_guard = turn_span.as_ref().map(tracing::Span::enter);
        execute(&a)?
    };
    drop(turn_span);

    if json {
        println!("{}", report.to_json());
    } else {
        match &report.verdict {
            Some(v) => {
                println!(
                    "[crucible rank-grounded] tier={} confidence={}",
                    v.disposition.as_str(),
                    v.confidence
                );
                println!("  {}", v.rationale);
                println!("  turn cost ${:.4}", report.cost_usd);
            }
            None => eprintln!(
                "[crucible rank-grounded] no verdict (turn cost ${:.4}): {}",
                report.cost_usd,
                report
                    .error
                    .as_ref()
                    .map(GroundedError::to_string)
                    .unwrap_or_else(|| "unknown".to_string())
            ),
        }
        if let Some(tail) = &report.output_tail {
            eprintln!("[crucible rank-grounded] agent output tail:");
            for line in tail.lines() {
                eprintln!("  {line}");
            }
        }
        if report.over_budget {
            eprintln!(
                "[crucible rank-grounded] OVER BUDGET: turn cost ${:.4} exceeds --max-cost ${:.2}",
                report.cost_usd, a.max_cost
            );
        }
    }

    // The WorkPod scrape line, printed LAST so it survives preceding pod-log noise. Emitted even for
    // a no-verdict/over-budget turn (the marker's payload carries the error object), so the controller
    // records the real reason on the work_pods row instead of guessing from a nonzero exit alone.
    // The same JSON also rides the structured turn-result endpoint: the controller prefers it,
    // the marker is the phase-1 fallback. Best-effort, a failed write leaves the marker in charge.
    if a.marker {
        println!("{VERDICT_MARKER} {}", report.to_json());
        crate::result_mode::emit(crucible_contract::EnvelopeKind::Verdict, report.to_json());
    }

    if report.verdict.is_none() || report.over_budget {
        // `process::exit` skips `EngineCtx::Drop`; flush buffered spans first.
        crate::engine::flush();
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crucible_contract::Tier;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-rankg-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    fn write_exec(path: &Path, content: &str) {
        fs::write(path, content).expect("write script");
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    /// A git repo standing in for the checkout under test, the grounded turn adds a detached
    /// worktree off its HEAD.
    fn git_repo_fixture(dir: &Path) {
        fs::write(dir.join("README.md"), "hello\n").expect("seed file");
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .status()
                    .expect("spawn git")
                    .success()
            );
        };
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "seed",
        ]);
    }

    /// A local listener standing in for api.github.com (pointed at via GITHUB_API_URL): serve one
    /// canned issue. Returns the address to set as GITHUB_API_URL.
    fn fake_github(title: &str, body: &str) -> (std::thread::JoinHandle<()>, String) {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let body_json = format!(
            r#"{{"title": {}, "body": {}, "labels": [{{"name": "bug"}}]}}"#,
            serde_json::to_string(title).unwrap(),
            serde_json::to_string(body).unwrap(),
        );
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0u8; 4096];
            let mut read = 0;
            while !buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buf[read..]).expect("read");
                if n == 0 {
                    break;
                }
                read += n;
            }
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_json}",
                body_json.len()
            );
            stream.write_all(resp.as_bytes()).expect("write");
        });
        (handle, format!("http://{addr}"))
    }

    /// Serializes the tests that point the process-global GITHUB_API_URL at a listener, the
    /// crate-wide guard shared with `scope`/`run` so cross-module env races can't happen.
    fn github_env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock()
    }

    /// The turn's two no-verdict cases must not collapse into one: a failed turn names the
    /// transport, a completed one names the model. The controller reads `error_kind` to tell them
    /// apart without parsing prose.
    #[test]
    fn a_failed_turn_and_a_verdictless_turn_are_different_reports() {
        let failed = report_from_turn(
            TurnOutcome::failed(0.0, TurnFailure::Spawn("no such binary".into())),
            "",
            0.0,
        );
        assert!(failed.verdict.is_none());
        assert_eq!(
            failed.error,
            Some(GroundedError::Turn(TurnFailure::Spawn(
                "no such binary".into()
            )))
        );
        assert_eq!(failed.to_json()["error_kind"], "turn_failed");
        assert!(
            failed.to_json()["error"]
                .as_str()
                .unwrap_or_default()
                .contains("no such binary"),
            "the real reason survives into the marker payload"
        );

        let verdictless = report_from_turn(TurnOutcome::completed(0.12), "thinking out loud", 0.0);
        assert!(verdictless.verdict.is_none());
        assert_eq!(verdictless.error, Some(GroundedError::NoVerdict));
        assert_eq!(verdictless.to_json()["error_kind"], "no_verdict");
        assert_eq!(verdictless.cost_usd, 0.12);
    }

    /// A turn that broke after billing tokens reports what it spent, and its output is never
    /// scraped for a verdict.
    #[test]
    fn a_partial_turn_keeps_its_cost_and_ignores_stale_output() {
        let buf = "{\"tier\":\"T1\",\"rationale\":\"leftovers\"}";
        let report = report_from_turn(
            TurnOutcome::failed(
                0.42,
                TurnFailure::Orchestration("sandbox exec failed".into()),
            ),
            buf,
            0.25,
        );
        assert!(report.verdict.is_none(), "a broken turn has no verdict");
        assert_eq!(report.cost_usd, 0.42);
        assert!(report.over_budget, "0.42 > 0.25 max-cost");
        assert_eq!(report.to_json()["error_kind"], "turn_failed");
    }

    #[test]
    fn a_completed_turn_with_a_verdict_reports_no_error() {
        let report = report_from_turn(
            TurnOutcome::completed(0.3),
            "{\"tier\":\"T1\",\"rationale\":\"one file\"}",
            0.0,
        );
        assert_eq!(report.error, None);
        assert_eq!(
            report.verdict.as_ref().map(|v| v.disposition),
            Some(Disposition::Tier(Tier::T1))
        );
        assert!(report.to_json().get("error_kind").is_none());
    }

    /// The outage shape: the turn fails and the agent's only output is the diagnostic it
    /// printed on its way down. Reporting "the turn never completed" while dropping that line
    /// is what made this class of failure unreadable.
    #[test]
    fn a_failed_turn_replays_what_the_agent_printed() {
        let report = report_from_turn(
            TurnOutcome::failed(0.0, TurnFailure::Spawn("the agent exited 1".to_string())),
            "Error: could not fetch an access token\n",
            0.0,
        );
        assert_eq!(
            report.output_tail.as_deref(),
            Some("Error: could not fetch an access token")
        );
        assert_eq!(
            report.to_json().get("output_tail").and_then(|v| v.as_str()),
            Some("Error: could not fetch an access token")
        );
    }

    #[test]
    fn a_failed_turn_that_printed_nothing_carries_no_tail() {
        let report = report_from_turn(
            TurnOutcome::failed(0.0, TurnFailure::Spawn("boom".to_string())),
            "   \n\n",
            0.0,
        );
        assert_eq!(report.output_tail, None);
    }

    /// A turn that produced a verdict is the success path: no tail, so the marker line the
    /// controller scrapes stays lean.
    #[test]
    fn a_successful_turn_carries_no_tail() {
        let report = report_from_turn(
            TurnOutcome::completed(0.3),
            "{\"tier\":\"T1\",\"rationale\":\"one file\"}",
            0.0,
        );
        assert_eq!(report.output_tail, None);
    }

    #[test]
    fn the_replayed_tail_is_bounded_to_the_last_lines() {
        let buf: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let report = report_from_turn(
            TurnOutcome::failed(0.0, TurnFailure::Spawn("boom".to_string())),
            &buf,
            0.0,
        );
        let tail = report.output_tail.expect("tail");
        assert_eq!(tail.lines().count(), OUTPUT_TAIL_LINES);
        assert!(tail.starts_with("line 80"), "{tail}");
        assert!(tail.ends_with("line 99"), "{tail}");
    }

    /// The seam between the parser and the report: a line claude prints that is not stream-json
    /// must survive decoding as `Raw{Stdout}` AND be accumulated, or the failure path has nothing
    /// to replay. Driven through the real parser, not a hand-built event.
    #[test]
    fn undecodable_agent_stdout_reaches_the_verdict_buffer() {
        let mut parser = crucible_harness::StreamJsonParser::default();
        let mut buf = String::new();
        for ev in parser.push("Error: could not fetch an access token") {
            accumulate(Some(&ev), &mut buf);
        }
        assert_eq!(buf, "Error: could not fetch an access token\n");
        assert_eq!(
            tail_of(&buf).as_deref(),
            Some("Error: could not fetch an access token")
        );
    }

    fn args(workspace: &Path, script: &Path, max_cost: f64) -> RankGroundedArgs {
        RankGroundedArgs {
            issue: "owner/repo#42".to_string(),
            workspace: workspace.to_path_buf(),
            max_cost,
            json: true,
            marker: false,
            agent_backend: AgentBackend::Local,
            sandbox_image: None,
            agent_cmd: Some(script.display().to_string()),
            model: None,
            compute_driver: crate::openshell::gateway::ComputeDriver::Podman,
        }
    }

    #[test]
    fn parse_verdict_accepts_a_clean_object_and_defaults_confidence() {
        let v = parse_verdict(r#"{"tier":"T1","rationale":"needs a new bench"}"#).expect("parses");
        assert_eq!(v.disposition, Disposition::Tier(Tier::T1));
        assert_eq!(v.confidence, "high");
    }

    #[test]
    fn extract_verdict_takes_the_last_json_line_after_prose() {
        let buf = "Let me look at the code.\nI grepped the tests.\n{\"tier\":\"T0\",\"rationale\":\"a failing test exists in tests/foo.rs\",\"confidence\":\"high\"}\n";
        let v = extract_verdict(buf).expect("verdict");
        assert_eq!(v.disposition, Disposition::Tier(Tier::T0));
        assert!(v.rationale.contains("failing test"));
    }

    #[test]
    fn extract_verdict_recovers_a_pretty_printed_fenced_block() {
        // Models pretty-print despite the single-line instruction (observed on the first real
        // grounded run: $0.55 spent, verdict lost). No single line parses here; the balanced-
        // block fallback must recover it, and take the LAST block, not an earlier example.
        let buf = "Here is an example shape:\n{\n  \"tier\": \"bogus\"\n}\nMy verdict:\n```json\n{\n  \"tier\": \"T2\",\n  \"rationale\": \"gate needs one deployed component\",\n  \"confidence\": \"low\"\n}\n```\nHope this helps.\n";
        let v = extract_verdict(buf).expect("verdict");
        assert_eq!(v.disposition, Disposition::Tier(Tier::T2));
        assert_eq!(v.confidence, "low");
    }

    #[test]
    fn extract_verdict_is_none_without_a_parseable_line() {
        assert!(extract_verdict("just prose, no json here\n").is_none());
    }

    #[test]
    fn scripted_verdict_flows_through_execute() {
        let _guard = github_env_lock();
        let repo = tempdir("repo-ok");
        git_repo_fixture(&repo);
        let scripts = tempdir("scripts-ok");
        let script = scripts.join("proposer.sh");
        // The agent's last stdout line is the verdict (the stdout contract).
        write_exec(
            &script,
            "#!/bin/sh\necho 'inspecting the checkout...'\necho '{\"tier\":\"T1\",\"rationale\":\"found a metric hook in src/metrics.rs\",\"confidence\":\"high\"}'\n",
        );

        let (handle, api) = fake_github("perf: p99 too high", "reduce it");
        unsafe {
            std::env::set_var("GITHUB_API_URL", &api);
        }
        let report = execute(&args(&repo, &script, 5.0));
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }
        handle.join().expect("server thread");

        let report = report.expect("execute ok");
        let v = report.verdict.expect("verdict");
        assert_eq!(v.disposition, Disposition::Tier(Tier::T1));
        assert!(!report.over_budget);
        assert!(report.error.is_none());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&scripts);
    }

    #[test]
    fn malformed_output_yields_no_verdict() {
        let _guard = github_env_lock();
        let repo = tempdir("repo-bad");
        git_repo_fixture(&repo);
        let scripts = tempdir("scripts-bad");
        let script = scripts.join("proposer.sh");
        write_exec(&script, "#!/bin/sh\necho 'I cannot decide, sorry'\n");

        let (handle, api) = fake_github("ambiguous", "unclear");
        unsafe {
            std::env::set_var("GITHUB_API_URL", &api);
        }
        let report = execute(&args(&repo, &script, 5.0)).expect("execute ok");
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }
        handle.join().expect("server thread");

        assert!(report.verdict.is_none());
        assert!(report.error.is_some());
        assert!(report.to_json().get("error").is_some());

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&scripts);
    }

    /// `stale` parses to [`Disposition::Stale`] end-to-end through `execute`, not a [`Tier`],
    /// the hard rule that supersedes every tier definition when the checkout already has the
    /// requested change.
    #[test]
    fn stale_disposition_parses_end_to_end() {
        let _guard = github_env_lock();
        let repo = tempdir("repo-stale");
        git_repo_fixture(&repo);
        let scripts = tempdir("scripts-stale");
        let script = scripts.join("proposer.sh");
        write_exec(
            &script,
            "#!/bin/sh\necho 'grepped the tests, this is already fixed'\necho '{\"tier\":\"stale\",\"rationale\":\"already implemented in src/foo.rs:42\",\"confidence\":\"high\"}'\n",
        );

        let (handle, api) = fake_github("bug: X is broken", "already fixed?");
        unsafe {
            std::env::set_var("GITHUB_API_URL", &api);
        }
        let report = execute(&args(&repo, &script, 5.0)).expect("execute ok");
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }
        handle.join().expect("server thread");

        assert_eq!(report.to_json()["tier"], "stale");
        let v = report.verdict.expect("verdict");
        assert_eq!(v.disposition, Disposition::Stale);
        assert!(v.rationale.contains("src/foo.rs:42"));

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&scripts);
    }

    #[test]
    fn over_budget_is_flagged_from_the_reported_cost() {
        let _guard = github_env_lock();
        let repo = tempdir("repo-budget");
        git_repo_fixture(&repo);
        let scripts = tempdir("scripts-budget");
        let script = scripts.join("proposer.sh");
        // A native `result` event carries the cost (decoded, not accumulated); the verdict is the
        // trailing non-native JSON line.
        write_exec(
            &script,
            "#!/bin/sh\necho '{\"v\":1,\"kind\":\"result\",\"subtype\":\"success\",\"stop_reason\":\"end_turn\",\"duration_ms\":1,\"api_duration_ms\":1,\"ttft_ms\":1,\"turns\":1,\"cost_usd\":0.84}'\necho '{\"tier\":\"T2\",\"rationale\":\"needs one live service\",\"confidence\":\"low\"}'\n",
        );

        let (handle, api) = fake_github("live thing", "deploy and measure");
        unsafe {
            std::env::set_var("GITHUB_API_URL", &api);
        }
        let report = execute(&args(&repo, &script, 0.5)).expect("execute ok");
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }
        handle.join().expect("server thread");

        assert!(
            (report.cost_usd - 0.84).abs() < 1e-9,
            "cost from the result event"
        );
        assert!(report.over_budget, "0.84 > 0.5 max-cost");
        assert_eq!(
            report.verdict.expect("verdict").disposition,
            Disposition::Tier(Tier::T2)
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&scripts);
    }

    /// The real in-pod path: the `openshell` backend runs the grounded turn in a sandbox carrying
    /// the claude CLI. Compile-tested + `#[ignore]`d (the established carve-out), it needs a live
    /// sandbox image, GCP creds, and network, none of which the unit suite has.
    #[test]
    #[ignore = "requires a real openshell sandbox image + claude CLI + network"]
    fn openshell_backend_runs_the_real_grounded_turn() {
        let repo = tempdir("repo-openshell");
        git_repo_fixture(&repo);
        let a = RankGroundedArgs {
            issue: "neuralmagic/crucible#1".to_string(),
            workspace: repo.clone(),
            max_cost: 5.0,
            json: true,
            marker: false,
            agent_backend: AgentBackend::Openshell,
            sandbox_image: Some("ghcr.io/neuralmagic/crucible-sandbox:latest".to_string()),
            agent_cmd: None,
            model: None,
            compute_driver: crate::openshell::gateway::ComputeDriver::Podman,
        };
        let _ = execute(&a);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn read_only_intent_is_contained_by_the_throwaway_worktree() {
        let _guard = github_env_lock();
        let repo = tempdir("repo-ro");
        git_repo_fixture(&repo);
        let scripts = tempdir("scripts-ro");
        let script = scripts.join("proposer.sh");
        // A misbehaving agent that writes a file into its cwd (the worktree) before the verdict.
        write_exec(
            &script,
            "#!/bin/sh\necho 'i should not do this' > VIOLATION.txt\necho '{\"tier\":\"N\",\"rationale\":\"pure discussion\",\"confidence\":\"high\"}'\n",
        );

        let (handle, api) = fake_github("discuss", "roadmap");
        unsafe {
            std::env::set_var("GITHUB_API_URL", &api);
        }
        let report = execute(&args(&repo, &script, 5.0)).expect("execute ok");
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }
        handle.join().expect("server thread");

        assert_eq!(
            report.verdict.expect("verdict").disposition,
            Disposition::Tier(Tier::N)
        );
        // The write landed in the (removed) worktree, never in the caller's checkout.
        assert!(
            !repo.join("VIOLATION.txt").exists(),
            "the caller's checkout must be untouched"
        );
        // And the checkout has no stray uncommitted change.
        let status = std::process::Command::new("git")
            .args(["-C", &repo.to_string_lossy(), "status", "--porcelain"])
            .output()
            .expect("git status");
        assert!(
            String::from_utf8_lossy(&status.stdout).trim().is_empty(),
            "the caller's checkout must have no diff after a grounded turn"
        );

        let _ = fs::remove_dir_all(&repo);
        let _ = fs::remove_dir_all(&scripts);
    }
}
