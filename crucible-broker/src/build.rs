//! Broker-side build/deploy (engine-side builds): the agent (sandboxed, no build creds) asks
//! `build_epp` / `deploy_candidate` over MCP; the loop pod does the work.
//!
//! The crux is the workspace sync. The agent's edits live only in the sandbox mid-turn (the driver
//! copies the workdir in/out per turn, it is not shared). So on a build the broker pulls the agent's
//! *current* tree out of the live sandbox with the same `openshell sandbox download` the driver uses
//! at end-of-turn, hash-verified for consistency (see [`sync_sandbox`]; the agent's own turn is
//! blocked on the tool call, but a background process it started is not), then hands it to the
//! generic [`forge`] build. No git push, no new egress, no driver IPC.
//!
//! The build is a durable step keyed by tree hash plus build config ([`forge::steps`]):
//! an unchanged tree replays the pushed ref, even across a broker restart.
//!
//! ```text
//!   agent (sandbox)  --build_epp-->  broker (loop pod)
//!       broker: openshell sandbox download ci <workdir>  <ctx>
//!       broker: forge::build_and_push(<ctx>)  ->  built{ref} | compile_error{log}
//!       broker: record latest-candidate
//!   agent  --deploy_candidate-->  broker: forge::deploy(latest)  ->  deployed{ref}
//! ```

use anyhow::{Context, Result};
use forge::steps::{StepKey, StepLedger, StepOutcome};
use forge::{BuildConfig, BuildOutcome, DeployConfig};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Domain hook the broker runs on the synced composite tree: it builds every changed backend,
/// rolls them live, and verifies the cross-component invariant. Env-configured so crucible-broker
/// stays domain-agnostic.
const DEFAULT_COMPOSITE_APPLY_CMD: &str = "fullstack-pd-apply";

/// What a build/deploy tool reports back to the agent (a tagged JSON, like the trace `Resolution`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildReply {
    /// Built + pushed; `image_ref` is live in the registry and recorded as the latest candidate.
    /// `cached` marks a replayed build: this exact tree+config was already pushed,
    /// possibly by a broker process that has since died.
    Built {
        image_ref: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cached: bool,
    },
    /// A composite deploy was kicked off on a background thread; poll `deploy_status(deploy_id)` to
    /// scan the live phase (and, opt-in, the raw log). Returns instantly so you never block on the roll.
    Building { deploy_id: String },
    /// The build failed; `log` is the tail of buildah's output. Fix it and call build_epp again.
    CompileError { log: String },
    /// Rolled the deployment onto `image_ref` and the rollout completed.
    Deployed { image_ref: String },
    /// The build tools are not enabled for this run (`BROKER_BUILD` unset).
    Disabled { reason: String },
    /// This turn's candidate budget is spent (or an operator asked to wrap up): commit the best
    /// candidate and end the turn. The cap keeps the search at the loop level rather than inside
    /// a single turn.
    WrapUp { reason: String },
    /// An engine-side failure (sync/push/deploy/config), distinct from a compile error.
    Error { error: String },
}

/// `build_epp`: pull the agent's current edits from the sandbox and build+push a candidate.
pub(crate) fn build_epp() -> String {
    if !enabled() {
        return json(&BuildReply::Disabled {
            reason: "build tools not enabled for this run (BROKER_BUILD unset)".into(),
        });
    }
    // Compile feedback is free, but once this turn's candidate budget is spent, stop building too.
    if let crate::turn::Budget::Spent { reason } = crate::turn::check(false) {
        return json(&BuildReply::WrapUp { reason });
    }
    json(&do_build().unwrap_or_else(|e| BuildReply::Error {
        error: format!("{e:#}"),
    }))
}

/// `deploy_candidate`: roll the deployment onto `image_ref` (or the latest built candidate when omitted).
pub(crate) fn deploy_candidate(image_ref: Option<String>) -> String {
    if !enabled() {
        return json(&BuildReply::Disabled {
            reason: "build tools not enabled for this run (BROKER_BUILD unset)".into(),
        });
    }
    // A deploy is a candidate; this consumes one against the per-turn budget (the cap / wrap-up).
    if let crate::turn::Budget::Spent { reason } = crate::turn::check(true) {
        return json(&BuildReply::WrapUp { reason });
    }
    json(&do_deploy(image_ref).unwrap_or_else(|e| BuildReply::Error {
        error: format!("{e:#}"),
    }))
}

fn do_build() -> Result<BuildReply> {
    let build_cfg = BuildConfig::from_env()
        .context("build config (set FORGE_REGISTRY / FORGE_AUTHFILE on the loop pod)")?;
    let ctx = ctx_dir(&build_cfg);
    let workdir = sandbox_workdir()?;
    let outcome = build_step(
        &crate::steps::ledger(),
        build_step_key(&workdir, &build_cfg),
        &mut || {
            sync_sandbox(&workdir, &ctx)?;
            forge::build_and_push(&build_cfg, &ctx, &unique_tag(&ctx))
        },
    )?;
    match outcome.value {
        BuildOutcome::CompileError { log } => Ok(BuildReply::CompileError { log }),
        BuildOutcome::Built { image_ref } => {
            // Runs on a replay too: the recorded ref is still the latest candidate, and the
            // pointer file may belong to a process that died before writing it.
            forge::record_latest(&latest_file(), &image_ref)?;
            Ok(BuildReply::Built {
                image_ref,
                cached: outcome.replayed,
            })
        }
    }
}

/// An unchanged tree against an unchanged config replays the ref it already pushed,
/// skipping the sandbox download AND the buildah run. No key = the build runs unrecorded.
fn build_step(
    ledger: &StepLedger,
    key: Option<StepKey>,
    build: &mut dyn FnMut() -> Result<BuildOutcome>,
) -> Result<StepOutcome<BuildOutcome>> {
    match key {
        Some(key) => ledger.run(&key, build),
        None => Ok(StepOutcome {
            value: build()?,
            replayed: false,
        }),
    }
}

/// The exact source tree plus the exact build config, so a replay is only possible when
/// nothing about the artifact could differ. `None` when the sandbox can't produce a tree
/// hash; the build then runs unrecorded rather than failing.
fn build_step_key(workdir: &str, cfg: &BuildConfig) -> Option<StepKey> {
    match sandbox_git_tree_hash(workdir) {
        Ok(tree) => Some(crate::steps::key(format!(
            "build-epp:{tree}:{}",
            cfg_fingerprint(cfg)
        ))),
        Err(e) => {
            eprintln!("==> build_epp: no sandbox git tree hash ({e:#}); building unrecorded");
            None
        }
    }
}

/// The whole finalized config is fingerprinted, not just the registry: the artifact depends on
/// the Dockerfile, the platform, and the push destination too.
fn cfg_fingerprint(cfg: &BuildConfig) -> String {
    let authfile = cfg.authfile.to_string_lossy();
    let storage = cfg
        .storage_root
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    forge::steps::fingerprint(&[
        &cfg.registry,
        &cfg.dockerfile,
        &authfile,
        &cfg.platform,
        &storage,
    ])
}

fn do_deploy(image_ref: Option<String>) -> Result<BuildReply> {
    let deploy_cfg =
        DeployConfig::from_env().context("deploy config (set FORGE_DEPLOY_* on the loop pod)")?;
    let image_ref = match image_ref.filter(|s| !s.is_empty()) {
        Some(r) => r,
        None => forge::read_latest(&latest_file())?
            .context("no candidate built yet — call build_epp first")?,
    };
    forge::deploy(&deploy_cfg, &image_ref)?;
    Ok(BuildReply::Deployed { image_ref })
}

/// Pull the sandbox tree to the loop pod, verified for consistency. The download copies a LIVE
/// tree: the agent's turn is usually quiescent during the tool call, but a background process it
/// started (`run_in_background` Bash, a stray `&`) can keep writing mid-copy and hand the build a
/// torn context no version of the tree ever matched. So: hash the sandbox tree, download, hash
/// again; equal hashes bracket the copy, proving nothing wrote during it. Retry a couple of times
/// on churn, then tell the agent to let its background work finish. When the sandbox can't produce
/// a hash (no sha256sum in the image), fall back to one unverified download rather than break builds.
pub(crate) fn sync_sandbox(sandbox_path: &str, dest: &Path) -> Result<()> {
    verified_sync(&mut || Ok(sandbox_tree_hash(sandbox_path)?), &mut || {
        download_sandbox(sandbox_path, dest)
    })
}

/// The verification state machine, IO injected so it's unit-testable (see [`sync_sandbox`]).
fn verified_sync(
    tree_hash: &mut dyn FnMut() -> Result<String>,
    download: &mut dyn FnMut() -> Result<()>,
) -> Result<()> {
    const ATTEMPTS: u32 = 3;
    let mut before = match tree_hash() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("==> sandbox tree hash unavailable ({e:#}); syncing unverified");
            return download();
        }
    };
    for attempt in 1..=ATTEMPTS {
        download()?;
        let after = tree_hash()?;
        if before == after {
            return Ok(());
        }
        eprintln!("==> sandbox tree changed during sync (attempt {attempt}/{ATTEMPTS}); retrying");
        before = after;
    }
    anyhow::bail!(
        "sandbox tree kept changing across {ATTEMPTS} sync attempts — a background process in the \
         sandbox is still writing. Wait for it to finish (or stop it), then call the tool again"
    )
}

/// One `openshell sandbox download`, into a cleared staging dir so a file the agent deleted in the
/// sandbox doesn't linger in the build context.
fn download_sandbox(sandbox_path: &str, dest: &Path) -> Result<()> {
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let name = sandbox_name();
    eprintln!(
        "==> openshell sandbox download {name} {sandbox_path} -> {}",
        dest.display()
    );
    let out = Command::new("openshell")
        .args([
            "sandbox",
            "download",
            &name,
            sandbox_path,
            &dest.to_string_lossy(),
        ])
        .output()
        .context("running `openshell sandbox download` (is openshell on PATH?)")?;
    if !out.status.success() {
        anyhow::bail!(
            "openshell sandbox download failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Every way an in-sandbox tree hash can fail, typed so callers can tell a gateway/transport
/// problem (fall back) from a command failure inside a live sandbox (surface it).
#[derive(Debug, thiserror::Error)]
pub(crate) enum TreeHashError {
    #[error(transparent)]
    Gateway(#[from] crate::gateway::GatewayError),
    #[error("{what} failed in the sandbox: {stderr}")]
    CommandFailed { what: &'static str, stderr: String },
    #[error("sandbox tree hash returned empty output")]
    EmptyHash,
    #[error("sandbox git write-tree returned no tree hash (got: {got:?})")]
    NotATreeHash { got: String },
}

/// A content hash of the sandbox tree, computed IN the sandbox (gateway exec RPC): every file's
/// sha256, sorted for a stable order, hashed again. Filenames + contents; mtimes don't matter
/// (the download doesn't reproduce them reliably) and neither do empty dirs.
pub(crate) fn sandbox_tree_hash(sandbox_path: &str) -> Result<String, TreeHashError> {
    let script = format!(
        "cd {} && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum",
        sh_quote(sandbox_path)
    );
    let out = sandbox_exec(&script)?;
    if !out.exit_ok {
        return Err(TreeHashError::CommandFailed {
            what: "tree hash",
            stderr: out.stderr.trim().to_string(),
        });
    }
    let hash = out.stdout.trim().to_string();
    if hash.is_empty() {
        return Err(TreeHashError::EmptyHash);
    }
    Ok(hash)
}

/// One in-sandbox `bash -c` over the gateway's exec RPC.
fn sandbox_exec(script: &str) -> Result<crate::gateway::ExecOutput, crate::gateway::GatewayError> {
    crate::gateway::exec_collect(
        &sandbox_name(),
        &["bash".to_string(), "-c".to_string(), script.to_string()],
    )
}

/// The same working-tree hash the loop-pod path computes (throwaway-index `git add -A` +
/// `write-tree`), run inside the live sandbox; same tree must yield the same build key on both
/// paths. [`sandbox_tree_hash`] stays for `verified_sync` churn detection (no git required).
pub(crate) fn sandbox_git_tree_hash(sandbox_path: &str) -> Result<String, TreeHashError> {
    // mktemp -u: name only; git refuses an EXISTING zero-length index file ("index file smaller
    // than expected"), so the index path must not exist yet (validated against a live sandbox).
    let script = format!(
        "cd {} && export GIT_INDEX_FILE=$(mktemp -u) && git add -A && git write-tree && rm -f \"$GIT_INDEX_FILE\"",
        sh_quote(sandbox_path)
    );
    let out = sandbox_exec(&script)?;
    if !out.exit_ok {
        return Err(TreeHashError::CommandFailed {
            what: "git tree hash",
            stderr: out.stderr.trim().to_string(),
        });
    }
    let hash = last_line(out.stdout.as_bytes());
    if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(TreeHashError::NotATreeHash { got: hash });
    }
    Ok(hash)
}

/// Single-quote a value for `sh` (wrap in `'…'`, escaping embedded single quotes).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A unique, readable tag per build: `<git-short-sha>-<unix-secs>`. The nonce keeps each build an
/// immutable tag even when the agent rebuilds without committing (same sha), so the recorded
/// candidate and the deployed image compare by string reliably.
fn unique_tag(dir: &Path) -> String {
    let sha = forge::context_tag(dir);
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{sha}-{secs}")
}

/// Stage the pulled sandbox tree under the build storage root (so it shares the build volume).
fn ctx_dir(cfg: &BuildConfig) -> PathBuf {
    cfg.storage_root
        .clone()
        .unwrap_or_else(|| PathBuf::from("/var/lib/forge"))
        .join("ctx")
}

/// Sandbox path holding the agent's edits; the build context once pulled to the loop pod. A
/// deployment fact with no sane universal default, so it is REQUIRED config: the rendered loop pod
/// projects it, hand-written pods must set it.
pub(crate) fn sandbox_workdir() -> Result<String> {
    std::env::var("BROKER_SANDBOX_WORKDIR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "BROKER_SANDBOX_WORKDIR is not set on the loop pod (the sandbox path holding the \
                 agent's edits, e.g. /sandbox/<workspace-basename>)"
            )
        })
}

/// The driver's per-run sandbox name (`ci-<pid>-<workspace-hash>`), handed over at spawn as
/// `BROKER_SANDBOX_NAME`; crucible derives it, we never re-derive (the hash isn't stable across
/// builds). The `ci` fallback pairs an externally-started broker with a fixed-name driver.
fn sandbox_name() -> String {
    std::env::var("BROKER_SANDBOX_NAME").unwrap_or_else(|_| "ci".into())
}

fn latest_file() -> PathBuf {
    std::env::var("FORGE_LATEST_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(forge::DEFAULT_LATEST_FILE))
}

fn enabled() -> bool {
    matches!(
        std::env::var("BROKER_BUILD").as_deref(),
        Ok("1") | Ok("true")
    )
}

pub(crate) fn composite_enabled() -> bool {
    matches!(
        std::env::var("BROKER_COMPOSITE").as_deref(),
        Ok("1") | Ok("true")
    )
}

pub(crate) fn composite_apply_cmd() -> String {
    std::env::var("BROKER_COMPOSITE_APPLY_CMD")
        .unwrap_or_else(|_| DEFAULT_COMPOSITE_APPLY_CMD.into())
}

/// Where the synced composite tree is staged for the apply hook (shares the build storage volume).
pub(crate) fn composite_ctx_dir() -> PathBuf {
    std::env::var("BROKER_COMPOSITE_CTX")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/var/lib/forge/composite-ctx"))
}

/// The last non-empty stdout line (the hook prints the deployed refs there), for the agent's reply.
pub(crate) fn last_line(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout)
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// The last ~2 KB of a failed apply's output (stdout then stderr), char-safe, for the agent's reply.
pub(crate) fn log_tail(stdout: &[u8], stderr: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(stdout).into_owned();
    s.push_str(&String::from_utf8_lossy(stderr));
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(2000);
    chars[start..].iter().collect()
}

pub(crate) fn json(reply: &BuildReply) -> String {
    serde_json::to_string(reply)
        .unwrap_or_else(|e| format!(r#"{{"status":"error","error":"{e}"}}"#))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_serialize_with_a_status_tag() {
        assert!(
            json(&BuildReply::Built {
                image_ref: "quay.io/x/y:t".into(),
                cached: false,
            })
            .contains(r#""status":"built""#)
        );
        assert!(
            json(&BuildReply::CompileError { log: "boom".into() })
                .contains(r#""status":"compile_error""#)
        );
        assert!(
            json(&BuildReply::Deployed {
                image_ref: "quay.io/x/y:t".into()
            })
            .contains(r#""status":"deployed""#)
        );
        assert!(
            json(&BuildReply::Disabled {
                reason: "off".into()
            })
            .contains(r#""status":"disabled""#)
        );
    }

    /// With the gate unset, both tools short-circuit to `disabled` and never touch the cluster.
    #[test]
    fn tools_are_disabled_without_the_gate() {
        // The test process doesn't set BROKER_BUILD, so the gate is off.
        assert!(!enabled());
        assert!(build_epp().contains(r#""status":"disabled""#));
        assert!(deploy_candidate(None).contains(r#""status":"disabled""#));
    }

    #[test]
    fn log_tail_is_char_safe_and_bounded() {
        // Multi-byte chars must not panic at the truncation boundary.
        let big = "λ".repeat(3000);
        let tail = log_tail(big.as_bytes(), b"");
        assert_eq!(tail.chars().count(), 2000);
    }

    /// A stable tree hash: one download, no retries.
    #[test]
    fn verified_sync_passes_on_a_stable_hash() {
        let mut downloads = 0;
        verified_sync(&mut || Ok("h1".into()), &mut || {
            downloads += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(downloads, 1);
    }

    /// One mid-copy write (hash flips once, then stabilizes): retried, then succeeds.
    #[test]
    fn verified_sync_retries_through_transient_churn() {
        let hashes = ["h1", "h2", "h2", "h2"];
        let mut i = 0;
        let mut downloads = 0;
        verified_sync(
            &mut || {
                let h = hashes[i];
                i += 1;
                Ok(h.into())
            },
            &mut || {
                downloads += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(downloads, 2, "first torn copy retried once");
    }

    /// A tree that never stops changing must FAIL (never hand a torn context to the build) with a
    /// message pointing the agent at its background process.
    #[test]
    fn verified_sync_fails_on_sustained_churn() {
        let mut i = 0;
        let err = verified_sync(
            &mut || {
                i += 1;
                Ok(format!("h{i}"))
            },
            &mut || Ok(()),
        )
        .unwrap_err();
        assert!(err.to_string().contains("background process"), "{err:#}");
    }

    /// No sha256sum in the sandbox image: degrade to one unverified download, don't break builds.
    #[test]
    fn verified_sync_degrades_when_hashing_is_unavailable() {
        let mut downloads = 0;
        verified_sync(&mut || anyhow::bail!("no sha256sum"), &mut || {
            downloads += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(downloads, 1);
    }

    /// The hash script quotes the sandbox path and pipes through a stable sort.
    #[test]
    fn sh_quote_wraps_and_escapes() {
        assert_eq!(sh_quote("/sandbox/epp"), "'/sandbox/epp'");
        assert_eq!(sh_quote("a'b"), "'a'\\''b'");
    }

    fn step_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("broker-steps-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The whole point: the second build of an unchanged tree neither syncs nor builds, and the
    /// agent is told the ref came back cached.
    #[test]
    fn an_unchanged_tree_replays_the_pushed_ref() {
        let ledger = StepLedger::new(&step_dir("replay"));
        let key = Some(crate::steps::key("build-epp:tree1:cfg1"));
        let mut builds = 0;
        let first = build_step(&ledger, key.clone(), &mut || {
            builds += 1;
            Ok(BuildOutcome::Built {
                image_ref: "quay.io/x/y:abc-1".into(),
            })
        })
        .unwrap();
        assert!(!first.replayed);

        let second = build_step(&ledger, key, &mut || {
            builds += 1;
            anyhow::bail!("a replayed build must not sync or run buildah")
        })
        .unwrap();
        assert!(second.replayed);
        assert_eq!(second.value, first.value, "the same immutable pushed ref");
        assert_eq!(builds, 1);
    }

    /// A compile error is a fact of the tree: replay it rather than rebuild a known-broken tree.
    #[test]
    fn a_compile_error_replays_without_a_rebuild() {
        let ledger = StepLedger::new(&step_dir("compile-error"));
        let key = Some(crate::steps::key("build-epp:tree1:cfg1"));
        build_step(&ledger, key.clone(), &mut || {
            Ok(BuildOutcome::CompileError {
                log: "error[E0432]".into(),
            })
        })
        .unwrap();
        let replay = build_step(&ledger, key, &mut || {
            anyhow::bail!("a replayed build must not run buildah")
        })
        .unwrap();
        assert!(replay.replayed);
        assert_eq!(
            replay.value,
            BuildOutcome::CompileError {
                log: "error[E0432]".into()
            }
        );
    }

    /// A sync/push failure is transport: nothing is recorded, so the next call really builds.
    #[test]
    fn a_sync_failure_is_not_recorded() {
        let ledger = StepLedger::new(&step_dir("sync-failure"));
        let key = Some(crate::steps::key("build-epp:tree1:cfg1"));
        let err = build_step(&ledger, key.clone(), &mut || {
            anyhow::bail!("openshell sandbox download failed")
        })
        .unwrap_err();
        assert!(err.to_string().contains("download failed"));
        let retry = build_step(&ledger, key, &mut || {
            Ok(BuildOutcome::Built {
                image_ref: "quay.io/x/y:abc-2".into(),
            })
        })
        .unwrap();
        assert!(!retry.replayed);
    }

    /// No tree hash, no identity: build every time rather than replay something unkeyed.
    #[test]
    fn an_unkeyed_build_always_runs() {
        let ledger = StepLedger::new(&step_dir("unkeyed"));
        let mut builds = 0;
        for _ in 0..2 {
            let out = build_step(&ledger, None, &mut || {
                builds += 1;
                Ok(BuildOutcome::Built {
                    image_ref: "quay.io/x/y:abc".into(),
                })
            })
            .unwrap();
            assert!(!out.replayed);
        }
        assert_eq!(builds, 2);
    }

    /// Identity covers the config, not just the tree: a changed registry must not replay.
    #[test]
    fn the_config_fingerprint_moves_with_the_config() {
        let base = BuildConfig {
            registry: "quay.io/a/b".into(),
            dockerfile: "Dockerfile".into(),
            authfile: PathBuf::from("/run/auth.json"),
            platform: "linux/amd64".into(),
            storage_root: Some(PathBuf::from("/var/lib/forge")),
        };
        let mut moved = base.clone();
        moved.registry = "quay.io/a/c".into();
        assert_eq!(cfg_fingerprint(&base), cfg_fingerprint(&base.clone()));
        assert_ne!(cfg_fingerprint(&base), cfg_fingerprint(&moved));
    }

    /// The replay flag rides the reply only when it is true, so an ordinary build's JSON is
    /// byte-identical to what agents saw before.
    #[test]
    fn cached_is_absent_from_a_fresh_build_reply() {
        let fresh = json(&BuildReply::Built {
            image_ref: "quay.io/x/y:t".into(),
            cached: false,
        });
        assert!(!fresh.contains("cached"), "{fresh}");
        let replayed = json(&BuildReply::Built {
            image_ref: "quay.io/x/y:t".into(),
            cached: true,
        });
        assert!(replayed.contains(r#""cached":true"#), "{replayed}");
    }

    #[test]
    fn unique_tag_appends_a_nonce_to_the_context_tag() {
        // A non-repo dir yields the `wip` sha; the tag still carries a numeric nonce suffix.
        let dir = std::env::temp_dir();
        let tag = unique_tag(&dir);
        let (sha, nonce) = tag.rsplit_once('-').expect("tag has a nonce suffix");
        assert!(!sha.is_empty());
        assert!(
            nonce.chars().all(|c| c.is_ascii_digit()),
            "nonce is unix secs: {nonce}"
        );
    }
}
