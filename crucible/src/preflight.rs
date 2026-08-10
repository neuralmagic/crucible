//! Preflight: run the domain's rung ladder against the unmodified baseline tree before
//! iteration 1. Every rung here costs zero agent dollars, so an environment fault surfaces as a
//! refuse-to-start verdict instead of a burned iteration, and the optional baseline rung's score
//! seeds `segment.baseline_score` so the first `decide` compares against a real number.
//!
//! Execution contract mirrors [`crate::plan::runner::ShellRunner`]: `sh -c <cmd>` in the
//! workspace, the last non-empty stdout line is a JSON object, success is exit 0 plus
//! `pass` absent-or-true.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::Ordering;

use anyhow::Result;
use serde_json::Value;

use crate::STOP;
use crate::manifest::{MODE_PLACEHOLDER, PreflightCfg};
use crate::reporter::Reporter;

/// The `{digest}` placeholder, filled from the most recent `digest` a rung emitted.
const DIGEST_PLACEHOLDER: &str = "{digest}";

/// How much of a failing rung's stderr rides the verdict.
const STDERR_TAIL_LINES: usize = 40;
const STDERR_TAIL_CHARS: usize = 4000;

/// The measurement the optional baseline rung produced, seeding segment 0's baseline.
#[derive(Debug, Clone)]
pub(crate) struct PreflightBaseline {
    pub score: f64,
    pub tiebreak: Option<f64>,
    pub note: String,
}

/// A rung that did not pass. Any of these is an environment verdict: the run refuses to start.
#[derive(Debug, thiserror::Error)]
#[error("preflight rung failed: `{command}`: {note}\n--- stderr tail ---\n{stderr_tail}")]
pub(crate) struct PreflightFailed {
    pub command: String,
    pub note: String,
    pub stderr_tail: String,
}

/// A stop signal arrived between rungs. Distinct from [`PreflightFailed`]: the environment is not
/// condemned, the operator just quit.
#[derive(Debug, thiserror::Error)]
#[error("preflight stopped by stop signal")]
pub(crate) struct PreflightStopped;

/// What one rung reported on its JSON line.
#[derive(Default)]
struct Capture {
    digest: Option<String>,
    score: Option<f64>,
    tiebreak: Option<f64>,
    note: String,
    logs: Vec<String>,
}

/// Run every rung in order, then the optional baseline rung. `modes` is the declared build-mode
/// list a `{mode}` rung fans out over (validated at manifest load, so an unexpandable command
/// cannot reach here).
///
/// The broker's per-turn GPU budget degrades to uncapped when the turn-token file is empty, and
/// teardown clears it after every turn, so these rungs run un-capped by design: preflight is
/// engine work, not part of any agent's turn budget.
pub(crate) fn run(
    cfg: &PreflightCfg,
    modes: &[String],
    workspace: &Path,
    r: &mut impl Reporter,
) -> Result<Option<PreflightBaseline>> {
    let mut digest: Option<String> = None;
    for command in &cfg.commands {
        for expanded in expand_modes(command, modes) {
            let cap = run_rung(&expanded, &mut digest, workspace, r)?;
            if cap.digest.is_some() {
                digest = cap.digest;
            }
        }
    }
    let Some(baseline) = &cfg.baseline else {
        return Ok(None);
    };
    // A `{mode}`-fanned baseline reports one score per mode; the last one stands, matching the
    // ladder's own "the final rung is the verdict" ordering.
    let mut seeded = None;
    for expanded in expand_modes(baseline, modes) {
        let cap = run_rung(&expanded, &mut digest, workspace, r)?;
        if let Some(score) = cap.score {
            seeded = Some(PreflightBaseline {
                score,
                tiebreak: cap.tiebreak,
                note: cap.note,
            });
        }
        if cap.digest.is_some() {
            digest = cap.digest;
        }
    }
    Ok(seeded)
}

/// Announce, substitute, execute, report. `digest` is read (not written) here: the caller owns
/// the chaining so the baseline rung and the ladder share one rule.
fn run_rung(
    command: &str,
    digest: &mut Option<String>,
    workspace: &Path,
    r: &mut impl Reporter,
) -> Result<Capture> {
    if STOP.load(Ordering::SeqCst) {
        return Err(PreflightStopped.into());
    }
    let command = substitute_digest(command, digest.as_deref())?;
    r.note(&format!("preflight: {command}"));
    let cap = run_one(&command, workspace)?;
    for line in &cap.logs {
        r.note(&format!("preflight log: {line}"));
    }
    let score = cap.score.map(|s| format!(" score={s}")).unwrap_or_default();
    r.note(&format!("preflight ok: {}{score}", cap.note));
    Ok(cap)
}

/// `{mode}` fans a command out over the declared modes, in declared order. A command without the
/// placeholder runs exactly once, whatever the mode list holds.
fn expand_modes(command: &str, modes: &[String]) -> Vec<String> {
    if !command.contains(MODE_PLACEHOLDER) {
        return vec![command.to_string()];
    }
    modes
        .iter()
        .map(|m| command.replace(MODE_PLACEHOLDER, m))
        .collect()
}

fn substitute_digest(command: &str, digest: Option<&str>) -> Result<String> {
    if !command.contains(DIGEST_PLACEHOLDER) {
        return Ok(command.to_string());
    }
    match digest {
        Some(d) => Ok(command.replace(DIGEST_PLACEHOLDER, d)),
        None => Err(PreflightFailed {
            command: command.to_string(),
            note: format!(
                "{DIGEST_PLACEHOLDER} used before any rung produced a digest (order the \
                 building rung first)"
            ),
            stderr_tail: String::new(),
        }
        .into()),
    }
}

fn run_one(command: &str, workspace: &Path) -> Result<Capture> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(workspace)
        .output()
        .map_err(|e| PreflightFailed {
            command: command.to_string(),
            note: format!("spawn failed: {e}"),
            stderr_tail: String::new(),
        })?;
    let stderr_tail = stderr_tail(&String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let json = stdout
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .filter(Value::is_object);

    if !out.status.success() {
        // The exit code is the fact; a JSON note (when the rung managed to emit one) is the
        // explanation, so prefer it.
        let note = json
            .as_ref()
            .and_then(|v| v.get("note"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("exit {}", out.status.code().unwrap_or(-1)));
        return Err(PreflightFailed {
            command: command.to_string(),
            note,
            stderr_tail,
        }
        .into());
    }
    let Some(json) = json else {
        return Err(PreflightFailed {
            command: command.to_string(),
            note: "no JSON object on the last stdout line".to_string(),
            stderr_tail,
        }
        .into());
    };
    let note = json
        .get("note")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let passed = json.get("pass").and_then(Value::as_bool).unwrap_or(true);
    if !passed {
        return Err(PreflightFailed {
            command: command.to_string(),
            note: if note.is_empty() {
                "rung reported pass=false".to_string()
            } else {
                note
            },
            stderr_tail,
        }
        .into());
    }
    Ok(Capture {
        digest: json
            .get("digest")
            .and_then(Value::as_str)
            .map(str::to_string),
        score: json.get("score").and_then(Value::as_f64),
        tiebreak: json.get("tiebreak").and_then(Value::as_f64),
        note,
        logs: json
            .get("logs")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The last [`STDERR_TAIL_LINES`] non-empty lines, capped at [`STDERR_TAIL_CHARS`] from the end:
/// a build failure's verdict is at the bottom of the log, never the top.
fn stderr_tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(STDERR_TAIL_LINES);
    let tail = lines[start..].join("\n");
    if tail.chars().count() <= STDERR_TAIL_CHARS {
        return tail;
    }
    tail.chars()
        .skip(tail.chars().count() - STDERR_TAIL_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporter::{AgentTurn, Phase, Row, Stop, TurnBudget};
    use crate::{Args, Paths};

    /// Collects notes; every other `Reporter` call is inert. Preflight only ever notes.
    #[derive(Default)]
    struct Notes(Vec<String>);
    impl Reporter for Notes {
        fn start(&mut self, _: &str, _: &str) {}
        fn phase(&mut self, _: Phase) {}
        fn note(&mut self, msg: &str) {
            self.0.push(msg.to_string());
        }
        fn row(&mut self, _: &Row, _: bool) {}
        fn run_agent(
            &mut self,
            _: &Args,
            _: &Paths,
            _: u32,
            _: &str,
            _: Option<&str>,
            _: Option<&str>,
            _: TurnBudget,
        ) -> AgentTurn {
            AgentTurn::default()
        }
        fn check_interrupt(&mut self, _: &Paths, _: &[Row]) -> Stop {
            Stop::Continue
        }
        fn summary(&mut self, _: &[Row], _: &str, _: f64) {}
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "crucible-preflight-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cfg(commands: &[&str], baseline: Option<&str>) -> PreflightCfg {
        PreflightCfg {
            commands: commands.iter().map(|c| c.to_string()).collect(),
            baseline: baseline.map(str::to_string),
        }
    }

    #[test]
    fn last_json_line_is_the_result_and_baseline_seeds_the_score() {
        let dir = tempdir("ok");
        let c = cfg(
            &[r#"echo chatter; echo '{"pass":true,"note":"built"}'"#],
            Some(r#"echo '{"score":12.5,"tiebreak":0.25,"note":"acc=0.81"}'"#),
        );
        let mut r = Notes::default();
        let out = run(&c, &[], &dir, &mut r)
            .expect("both rungs pass")
            .expect("the baseline rung seeds a score");
        assert_eq!(out.score, 12.5);
        assert_eq!(out.tiebreak, Some(0.25));
        assert_eq!(out.note, "acc=0.81");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_chains_from_one_rung_to_the_next() {
        let dir = tempdir("digest");
        let c = cfg(
            &[
                r#"echo '{"digest":"sha256:abc"}'"#,
                r#"echo '{"note":"used {digest}"}'"#,
            ],
            None,
        );
        // The second rung echoes the substituted text back, so the assertion proves substitution,
        // not just that the command ran.
        let mut r = Notes::default();
        assert!(run(&c, &[], &dir, &mut r).unwrap().is_none());
        assert!(
            r.0.iter().any(|n| n.contains("used sha256:abc")),
            "digest substituted: {:?}",
            r.0
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_before_any_digest_is_an_error() {
        let dir = tempdir("nodigest");
        let c = cfg(&[r#"echo '{"note":"{digest}"}'"#], None);
        let mut r = Notes::default();
        let err = run(&c, &[], &dir, &mut r).expect_err("no digest was produced yet");
        let failed = err
            .downcast_ref::<PreflightFailed>()
            .expect("typed preflight failure");
        assert!(
            failed.note.contains("before any rung produced a digest"),
            "{}",
            failed.note
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pass_false_fails_with_the_json_note() {
        let dir = tempdir("passfalse");
        let c = cfg(&[r#"echo '{"pass":false,"note":"nvrtc.h missing"}'"#], None);
        let mut r = Notes::default();
        let err = run(&c, &[], &dir, &mut r).expect_err("pass:false is a failure");
        let failed = err.downcast_ref::<PreflightFailed>().unwrap();
        assert_eq!(failed.note, "nvrtc.h missing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nonzero_exit_carries_the_stderr_tail() {
        let dir = tempdir("exit");
        let c = cfg(
            &["echo boom-line-one 1>&2; echo boom-tail 1>&2; exit 3"],
            None,
        );
        let mut r = Notes::default();
        let err = run(&c, &[], &dir, &mut r).expect_err("nonzero exit fails");
        let failed = err.downcast_ref::<PreflightFailed>().unwrap();
        assert_eq!(failed.note, "exit 3");
        assert!(failed.stderr_tail.contains("boom-tail"), "{failed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_json_line_fails() {
        let dir = tempdir("nojson");
        let c = cfg(&["echo not-json"], None);
        let mut r = Notes::default();
        let err = run(&c, &[], &dir, &mut r).expect_err("no JSON result");
        let failed = err.downcast_ref::<PreflightFailed>().unwrap();
        assert!(failed.note.contains("no JSON object"), "{}", failed.note);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mode_fans_out_in_declared_order() {
        let dir = tempdir("modes");
        let c = cfg(&[r#"printf '%s\n' '{"note":"mode={mode}"}'"#], None);
        let modes = vec!["derive".to_string(), "full".to_string()];
        let mut r = Notes::default();
        run(&c, &modes, &dir, &mut r).unwrap();
        let seen: Vec<&String> = r.0.iter().filter(|n| n.contains("preflight ok")).collect();
        assert_eq!(seen.len(), 2, "one run per mode: {:?}", r.0);
        assert!(seen[0].contains("mode=derive"), "{}", seen[0]);
        assert!(seen[1].contains("mode=full"), "{}", seen[1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commands_run_in_the_workspace() {
        let dir = tempdir("cwd");
        std::fs::write(dir.join("marker"), "x").unwrap();
        let c = cfg(&[r#"test -f marker && echo '{"note":"found"}'"#], None);
        let mut r = Notes::default();
        run(&c, &[], &dir, &mut r).expect("cwd is the workspace");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
