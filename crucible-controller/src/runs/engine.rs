//! The controller-to-engine boundary. The controller drives the `crucible` engine as a
//! **subprocess**: it is a sibling binary staged on `PATH` in both images, so [`resolve_bin`]
//! defaults to the bare `crucible` name; `CRUCIBLE_BIN` overrides it (tests point it at a scripted
//! stand-in). Also the git/`gh` plumbing that pushes a tree as a branch pair and opens the draft PR
//! whose diff is exactly that tree.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// The item a run is parameterized by, read by `crucible_contract::outputs::DefaultTargets` as the
/// engine default target for `tracker-comment`. Unset, the engine refuses every write of that kind.
pub(crate) const ITEM_ENV: &str = "CRUCIBLE_ITEM";
#[cfg(feature = "autoresearch")]
pub(crate) mod pack_pr;

/// The `crucible` engine binary: `CRUCIBLE_BIN` if set, else the bare name on `PATH` (both images
/// stage the engine alongside this binary).
pub fn resolve_bin() -> PathBuf {
    match std::env::var_os("CRUCIBLE_BIN") {
        Some(bin) => PathBuf::from(bin),
        None => PathBuf::from("crucible"),
    }
}

/// Download one object by shelling `crucible fetch --uri … --out …` — S3 access stays behind the
/// engine subprocess boundary; local paths never come here. A nonzero exit carries the engine's
/// stderr verbatim: the invoking shell may lack the pod's S3 credentials, and that stderr is the
/// only diagnostic.
pub async fn fetch_object(
    contracts: Option<&crate::runs::contract::ContractRegistry>,
    uri: &str,
    out: &Path,
) -> Result<()> {
    let bin = resolve_bin();
    if let Some(contracts) = contracts {
        contracts
            .admit_all(
                crate::runs::contract::RequestKind::Fetch,
                &[crate::runs::contract::DispatchTarget::Binary(bin.clone())],
            )
            .await?;
    }
    let output = tokio::process::Command::new(&bin)
        .arg("fetch")
        .arg("--uri")
        .arg(uri)
        .arg("--out")
        .arg(out)
        .output()
        .await
        .with_context(|| format!("spawning `{} fetch`", bin.display()))?;
    if !output.status.success() {
        bail!(
            "`crucible fetch {uri}` exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The clone URL for a watched `owner/repo` (scope's `--repo`, the code under test). A value that
/// already looks like a URL or local path is passed through untouched.
pub(crate) fn repo_clone_url(repo: &str) -> String {
    if repo.contains("://") || repo.contains('@') || repo.starts_with('/') || repo.ends_with(".git")
    {
        repo.to_string()
    } else {
        format!("https://github.com/{repo}.git")
    }
}

/// The forge PAT for pushing the pack + opening the PR — `AUTORESEARCH_PR_TOKEN` (fallback
/// `GITHUB_TOKEN`/`GH_TOKEN`), the same credential the publisher and the broker share. The
/// fallback arm of [`resolve_pack_pr_token`]; deploys with a GitHub App configured never get here.
fn pack_pr_token() -> Option<String> {
    std::env::var("AUTORESEARCH_PR_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|s| !s.is_empty())
}

/// The pack-PR credential: an App installation token when the deploy configured one
/// (`cfg.github_app`, from the `CONTROLLER_GITHUB_APP_*` env vars — the durable path, since the
/// org's token policy forbids long-lived PATs), else the [`pack_pr_token`] env chain. A configured
/// App that fails to mint is an error, never a silent PAT fallback — the retry re-drives it.
/// Installation tokens are documented to work both as the `x-access-token` git password and as
/// `GH_TOKEN` for `gh`, so one credential serves both halves of [`open_pack_pr`].
#[cfg(feature = "autoresearch")]
pub(crate) async fn resolve_pack_pr_token(
    cfg: &crate::config::ControllerCfg,
) -> Result<Option<String>> {
    resolve_pack_pr_token_for(cfg.github_app.as_ref()).await
}

/// [`resolve_pack_pr_token`] against the App source alone, for callers that hold the source rather
/// than the whole parsed config.
pub(crate) async fn resolve_pack_pr_token_for(
    app: Option<&crate::secrets::github_app::GithubAppTokenSource>,
) -> Result<Option<String>> {
    if let Some(app) = app {
        let token = app
            .token()
            .await
            .context("minting the GitHub App installation token for the pack PR")?;
        return Ok(Some(token));
    }
    Ok(pack_pr_token())
}

/// Encode an issue key into a Kubernetes label *value* (alphanumeric plus `-_.`, ≤63 chars, must
/// start/end alphanumeric) for [`crate::daemon::ISSUE_KEY_LABEL`]: every other char (`/`, `#`, …)
/// becomes `-`. `owner/repo#42` → `owner-repo-42`. Deliberately lossy — a human-readable hint
/// only. The key the watch reads back rides [`crate::daemon::ISSUE_KEY_ANNOTATION`] verbatim
/// (annotation values are unrestricted, so it round-trips exactly).
pub(crate) fn issue_key_label_value(key: &str) -> String {
    let mut v: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    v.truncate(63);
    let trimmed = v.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use crate::runs::engine::*;

    #[test]
    fn repo_clone_url_builds_github_https_for_a_bare_slug() {
        assert_eq!(
            repo_clone_url("neuralmagic/crucible"),
            "https://github.com/neuralmagic/crucible.git"
        );
    }

    #[test]
    fn repo_clone_url_passes_through_urls_and_paths() {
        assert_eq!(
            repo_clone_url("https://example.com/x.git"),
            "https://example.com/x.git"
        );
        assert_eq!(repo_clone_url("/abs/local/repo"), "/abs/local/repo");
        assert_eq!(
            repo_clone_url("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn issue_key_label_value_is_k8s_label_safe() {
        assert_eq!(issue_key_label_value("owner/repo#42"), "owner-repo-42");
        assert_eq!(issue_key_label_value("a_b.c-1"), "a_b.c-1");
        let v = issue_key_label_value("owner/repo#42");
        assert!(v.len() <= 63);
        assert!(v.chars().next().unwrap().is_ascii_alphanumeric());
        assert!(v.chars().last().unwrap().is_ascii_alphanumeric());
    }

    #[test]
    fn resolve_bin_honors_the_env_override() {
        // Serialize on the crate-wide env lock: `set_var`/`remove_var` race any other test that
        // reads the environ or spawns a subprocess (the whole crate shares one guard). A sync test
        // runs outside any tokio runtime, so `blocking_lock` is the sanctioned acquire.
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::set_var("CRUCIBLE_BIN", "/tmp/fake-crucible");
        }
        assert_eq!(resolve_bin(), PathBuf::from("/tmp/fake-crucible"));
        unsafe {
            std::env::remove_var("CRUCIBLE_BIN");
        }
    }

    /// `fetch_object` shells `CRUCIBLE_BIN fetch` (here a script) and surfaces a nonzero exit's
    /// stderr — the only diagnostic when the operator's shell lacks S3 credentials.
    #[tokio::test]
    async fn fetch_object_shells_the_engine_and_surfaces_stderr() {
        let _g = crate::ENV_LOCK.lock().await;
        let root = std::env::temp_dir().join(format!("crucible-fetchobj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let bin = root.join("crucible-fake");
        crate::testing::write_exec(
            &bin,
            "#!/bin/sh\n[ \"$1\" = fetch ] || exit 1\nshift\nwhile [ $# -gt 0 ]; do\n  case \"$1\" in\n    --uri) uri=\"$2\"; shift 2;;\n    --out) out=\"$2\"; shift 2;;\n    *) shift;;\n  esac\ndone\ncase \"$uri\" in\n  s3://b/ok*) printf 'fetched' > \"$out\";;\n  *) echo 'AccessDenied: no credentials' >&2; exit 1;;\nesac\n",
        );
        unsafe {
            std::env::set_var("CRUCIBLE_BIN", &bin);
        }

        let out = root.join("session.jsonl");
        fetch_object(None, "s3://b/ok/session.jsonl", &out)
            .await
            .expect("fetch succeeds");
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "fetched");

        let err = fetch_object(None, "s3://b/missing", &out)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("AccessDenied: no credentials"),
            "stderr rides the error: {err:#}"
        );

        unsafe {
            std::env::remove_var("CRUCIBLE_BIN");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The pack-PR credential preference order: a configured GitHub App wins over a set PAT
    /// (minting through a wiremock exchange, never the real GitHub); no App ⇒ the env chain.
    #[cfg(feature = "autoresearch")]
    #[tokio::test]
    async fn resolve_pack_pr_token_prefers_the_app_over_the_pat_chain() -> Result<()> {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let _g = crate::ENV_LOCK.lock().await;
        let vars = ["AUTORESEARCH_PR_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"];
        let prior: Vec<_> = vars.iter().map(std::env::var_os).collect();
        unsafe {
            std::env::set_var("AUTORESEARCH_PR_TOKEN", "pat-chain-token");
        }
        unsafe {
            std::env::remove_var("GITHUB_TOKEN");
        }
        unsafe {
            std::env::remove_var("GH_TOKEN");
        }

        // No App threaded → the PAT chain answers.
        let mut cfg = crate::testing::cfg_from_args(["ctl"]);
        assert_eq!(
            resolve_pack_pr_token(&cfg).await?.as_deref(),
            Some("pat-chain-token")
        );

        // App threaded → the installation token wins even with the PAT still set.
        let server = MockServer::start().await;
        let expires = jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() + 3600)?;
        Mock::given(method("POST"))
            .and(path("/app/installations/7/access_tokens"))
            .respond_with(ResponseTemplate::new(201).set_body_json(serde_json::json!({
                "token": "ghs_app_token",
                "expires_at": expires.to_string(),
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir()?;
        let key = crate::secrets::github_app::testkey::write_test_key(dir.path());
        cfg.github_app = Some(crate::secrets::github_app::GithubAppTokenSource::new(
            "4210340",
            "7",
            key,
            server.uri(),
        ));
        assert_eq!(
            resolve_pack_pr_token(&cfg).await?.as_deref(),
            Some("ghs_app_token")
        );

        for (v, p) in vars.iter().zip(prior) {
            match p {
                Some(val) => unsafe { std::env::set_var(v, val) },
                None => unsafe { std::env::remove_var(v) },
            }
        }
        Ok(())
    }
}
