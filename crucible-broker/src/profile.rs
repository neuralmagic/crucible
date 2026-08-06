//! Broker-side profiling: the "observe the candidate's internals" counterpart to [`crate::measure`].
//! Generic by construction: the broker shells domain-configured commands and knows nothing about the
//! profiler. An end-to-end gate can't isolate one component's hot path; a profile attributes cost to
//! functions and lines. Profile to *find* what to change; the gate to *score* the candidate.
//!
//! Profiling is keyed by a named **target** (one per profileable component). Each target is three
//! domain-set commands:
//!   - `BROKER_PROFILE_<T>_CAPTURE_CMD`: env `$KIND`, `$SECONDS`, `$OUT`; must write the profile to `$OUT`.
//!   - `BROKER_PROFILE_<T>_QUERY_CMD`: env `$PROFILE`, `$QUERY`; prints a text analysis of `$PROFILE`.
//!   - `BROKER_PROFILE_<T>_EXT`: the capture artifact's file extension (the broker stores bytes,
//!     never sniffs them; a format-keyed analyzer wants the right name, e.g. `json.gz` for a torch trace).
//!
//! The set of targets is `BROKER_PROFILE_TARGETS`. A single-component deployment skips the list and just
//! sets the un-prefixed `BROKER_PROFILE_CAPTURE_CMD` / `_QUERY_CMD` / `_EXT`; one implicit target.
//! `profile{target?, ...}` / `profile_query{target?, ...}` take an optional target: omit it when
//! there's exactly one, name it when there's more than one.

use serde::Serialize;
use serde_json::json;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// Hard ceiling on a capture window. A profile is sampled, so longer = more signal, but a tool call
/// that blocks for minutes is a foot-gun (and the house rule is "kill anything past 5 min"). Backends
/// that genuinely need a longer window can capture in their own command and ignore `$SECONDS`.
const MAX_SECONDS: u32 = 300;

/// Neutral default capture extension; the generic core carries no tool-specific name.
const DEFAULT_EXT: &str = "prof";

/// A resolved profile target: a named component plus the three domain commands/knobs that profile it.
struct TargetCfg {
    name: String,
    capture_cmd: Option<String>,
    query_cmd: Option<String>,
    ext: String,
}

/// What a profiling tool reports back to the agent (a tagged JSON, like the build `BuildReply`).
/// Why a capture or query refused. The MCP reply is JSON text, so these are rendered with
/// anyhow's `{:#}` (`anyhow::Error::new(err)`) to flatten the `source()` chain onto one line.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ProfileError {
    #[error("creating profile dir {}", .dir.display())]
    CreateProfileDir {
        dir: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("running capture command")]
    SpawnCapture(#[source] io::Error),
    #[error("capture failed: {stderr}")]
    CaptureFailed { stderr: String },
    #[error("capture produced no profile at $OUT (is the endpoint reachable / authorized?)")]
    CaptureEmpty,
    #[error("running query command")]
    SpawnQuery(#[source] io::Error),
    #[error("query failed: {stderr}")]
    QueryFailed { stderr: String },
    #[error("handle must be a bare capture name, not a path: {handle:?}")]
    HandleIsAPath { handle: String },
    #[error(
        "no captured profile named {handle:?} for target {target:?} \
         (capture one with `profile` first)"
    )]
    NoSuchProfile { handle: String, target: String },
    #[error("no profile captured yet for target {target:?} — call `profile` first")]
    NoLatestCapture { target: String },
    #[error("the latest capture for target {target:?} is gone; call `profile` again")]
    LatestCaptureGone { target: String },
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProfileReply {
    /// Captured a profile; `handle` names it for `profile_query`, `target` says which component it's of
    /// (echo it back to `profile_query` on a multi-target deployment), `bytes` proves it's non-empty.
    Captured {
        handle: String,
        target: String,
        kind: String,
        seconds: u32,
        bytes: u64,
    },
    /// A query's text output (e.g. pprof's `top` / `list` listing).
    Query { output: String },
    /// Profiling isn't configured for this run (no `BROKER_PROFILE_*` commands).
    Disabled { reason: String },
    /// An engine-side or input failure (bad target/kind/query, capture produced nothing, command failed).
    Error { error: String },
}

/// `profile{target?, kind, seconds}`: capture a profile of `kind` over `seconds` via the target's
/// capture command, store it under the target's profile dir, and return a handle + size. The broker
/// picks `$OUT` and records the capture as the target's latest so a `profile_query` with no handle
/// targets it.
pub(crate) fn profile(target: Option<&str>, kind: &str, seconds: u32) -> String {
    let cfg = match resolve_target(target) {
        Ok(c) => c,
        Err(reply) => return json(&reply),
    };
    let cmd = match cfg.capture_cmd.as_deref() {
        Some(c) => c,
        None => {
            return json(&ProfileReply::Error {
                error: format!("target {:?} has no capture command configured", cfg.name),
            });
        }
    };
    if let Err(e) = validate_token(kind) {
        return json(&ProfileReply::Error {
            error: format!("invalid kind {kind:?}: {e}"),
        });
    }
    let seconds = seconds.clamp(1, MAX_SECONDS);
    json(
        &do_capture(&cfg, cmd, kind, seconds).unwrap_or_else(|e| ProfileReply::Error {
            error: format!("{:#}", anyhow::Error::new(e)),
        }),
    )
}

/// `profile_query{target?, query, handle?}`: run the target's read-only analyzer over a stored profile
/// (the one named by `handle`, or the target's most recent capture when omitted) and return its text.
pub(crate) fn profile_query(target: Option<&str>, query: &str, handle: Option<String>) -> String {
    let cfg = match resolve_target(target) {
        Ok(c) => c,
        Err(reply) => return json(&reply),
    };
    let cmd = match cfg.query_cmd.as_deref() {
        Some(c) => c,
        None => {
            return json(&ProfileReply::Error {
                error: format!("target {:?} has no query command configured", cfg.name),
            });
        }
    };
    if let Err(e) = validate_query(query) {
        return json(&ProfileReply::Error {
            error: format!("rejected query {query:?}: {e}"),
        });
    }
    json(
        &do_query(&cfg, cmd, query, handle).unwrap_or_else(|e| ProfileReply::Error {
            error: format!("{:#}", anyhow::Error::new(e)),
        }),
    )
}

fn do_capture(
    cfg: &TargetCfg,
    cmd: &str,
    kind: &str,
    seconds: u32,
) -> Result<ProfileReply, ProfileError> {
    let dir = target_dir(&cfg.name);
    std::fs::create_dir_all(&dir).map_err(|source| ProfileError::CreateProfileDir {
        dir: dir.clone(),
        source,
    })?;
    let handle = format!("{kind}-{}.{}", nonce(), cfg.ext);
    let out = dir.join(&handle);

    let status = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("KIND", kind)
        .env("SECONDS", seconds.to_string())
        .env("OUT", &out)
        .output()
        .map_err(ProfileError::SpawnCapture)?;

    if !status.status.success() {
        return Err(ProfileError::CaptureFailed {
            stderr: String::from_utf8_lossy(&status.stderr).trim().to_owned(),
        });
    }
    let bytes = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if bytes == 0 {
        return Err(ProfileError::CaptureEmpty);
    }
    // Record the latest so `profile_query` with no handle targets this capture (per target).
    let _ = std::fs::write(latest_marker(&cfg.name), out.to_string_lossy().as_bytes());
    Ok(ProfileReply::Captured {
        handle,
        target: cfg.name.clone(),
        kind: kind.to_string(),
        seconds,
        bytes,
    })
}

fn do_query(
    cfg: &TargetCfg,
    cmd: &str,
    query: &str,
    handle: Option<String>,
) -> Result<ProfileReply, ProfileError> {
    let profile_path = resolve_profile(&target_dir(&cfg.name), &cfg.name, handle)?;
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env("PROFILE", &profile_path)
        .env("QUERY", query)
        .output()
        .map_err(ProfileError::SpawnQuery)?;

    if !out.status.success() {
        // pprof writes useful diagnostics to stderr; surface them so the agent can correct the query.
        return Err(ProfileError::QueryFailed {
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    let text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    Ok(ProfileReply::Query {
        output: if text.is_empty() {
            "(query produced no output)".into()
        } else {
            text
        },
    })
}

/// Resolve the profile to query within a target's dir: an explicit handle (sanitized to a bare filename
/// under that dir, no path traversal) or the target's recorded latest capture.
fn resolve_profile(
    dir: &Path,
    target: &str,
    handle: Option<String>,
) -> Result<PathBuf, ProfileError> {
    match handle.filter(|h| !h.is_empty()) {
        Some(h) => {
            if h.contains('/') || h.contains("..") {
                return Err(ProfileError::HandleIsAPath { handle: h });
            }
            let p = dir.join(&h);
            if !p.is_file() {
                return Err(ProfileError::NoSuchProfile {
                    handle: h,
                    target: target.to_owned(),
                });
            }
            Ok(p)
        }
        None => {
            let marker = latest_marker(target);
            let path = std::fs::read_to_string(&marker)
                .map_err(|_| ProfileError::NoLatestCapture {
                    target: target.to_owned(),
                })?
                .trim()
                .to_string();
            let p = PathBuf::from(path);
            if !p.is_file() {
                return Err(ProfileError::LatestCaptureGone {
                    target: target.to_owned(),
                });
            }
            Ok(p)
        }
    }
}

/// Resolve a `target` argument against the configured targets. `None`/empty picks the sole target when
/// there's exactly one (the single-component path); with several it's an error that lists them, so the
/// agent learns the names. No targets at all → `Disabled`, mirroring `measure`.
fn resolve_target(target: Option<&str>) -> Result<TargetCfg, ProfileReply> {
    let mut all = targets();
    if all.is_empty() {
        return Err(ProfileReply::Disabled {
            reason: "profiling not configured for this run (no BROKER_PROFILE_* commands)".into(),
        });
    }
    let names = || {
        all.iter()
            .map(|c| c.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match target.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => {
            let names = names();
            all.into_iter()
                .find(|c| c.name == t)
                .ok_or_else(|| ProfileReply::Error {
                    error: format!("unknown profile target {t:?}; available: {names}"),
                })
        }
        None => {
            if all.len() == 1 {
                Ok(all.remove(0))
            } else {
                Err(ProfileReply::Error {
                    error: format!(
                        "multiple profile targets — pass `target` (one of: {})",
                        names()
                    ),
                })
            }
        }
    }
}

/// Read the configured profile targets in declared order. A multi-component deployment sets
/// `BROKER_PROFILE_TARGETS` plus per-name prefixed commands; a single-component deployment sets the
/// un-prefixed commands (one implicit target). An env name's `-` maps to `_` in the var prefix.
fn targets() -> Vec<TargetCfg> {
    if let Some(list) = env_nonempty("BROKER_PROFILE_TARGETS") {
        return list
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            // A name that isn't a safe token can't be addressed (it lands in a dir + env prefix), so
            // drop it rather than build an unreachable/unsafe target.
            .filter(|name| validate_token(name).is_ok())
            .map(|name| {
                let up = name.to_ascii_uppercase().replace('-', "_");
                TargetCfg {
                    name: name.to_string(),
                    capture_cmd: env_nonempty(&format!("BROKER_PROFILE_{up}_CAPTURE_CMD")),
                    query_cmd: env_nonempty(&format!("BROKER_PROFILE_{up}_QUERY_CMD")),
                    ext: ext_or_default(env_nonempty(&format!("BROKER_PROFILE_{up}_EXT"))),
                }
            })
            .collect();
    }
    let capture = env_nonempty("BROKER_PROFILE_CAPTURE_CMD");
    let query = env_nonempty("BROKER_PROFILE_QUERY_CMD");
    if capture.is_none() && query.is_none() {
        return vec![];
    }
    vec![TargetCfg {
        name: env_nonempty("BROKER_PROFILE_NAME").unwrap_or_else(|| "default".into()),
        capture_cmd: capture,
        query_cmd: query,
        ext: ext_or_default(env_nonempty("BROKER_PROFILE_EXT")),
    }]
}

/// An env var's value if set and non-empty (trimmed), else `None`. The "unset == off" convention.
fn env_nonempty(k: &str) -> Option<String> {
    std::env::var(k)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Profiles live under their own dir (default: a `profiles/` subdir of the build storage root, so
/// they ride the same volume as build contexts). `BROKER_PROFILE_DIR` overrides. Per-target captures
/// nest under `<dir>/<target>/`.
fn profile_dir() -> PathBuf {
    if let Ok(d) = std::env::var("BROKER_PROFILE_DIR")
        && !d.is_empty()
    {
        return PathBuf::from(d);
    }
    let root = std::env::var("FORGE_STORAGE_ROOT").unwrap_or_else(|_| "/var/lib/forge".into());
    PathBuf::from(root).join("profiles")
}

/// A target's capture dir (the target name is a validated token, so it can't escape `profile_dir`).
fn target_dir(target: &str) -> PathBuf {
    profile_dir().join(target)
}

fn latest_marker(target: &str) -> PathBuf {
    target_dir(target).join("latest")
}

fn nonce() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A capture artifact's extension is a DOMAIN concern; the broker stores the bytes
/// the capture command writes and never interprets them. A format-sniffing analyzer wants the right
/// name (pprof doesn't care; a PyTorch GPU trace must end in `json.gz`). Sanitize a configured value to
/// a bare extension or fall back to the neutral default.
fn ext_or_default(v: Option<String>) -> String {
    v.map(|s| s.trim().trim_start_matches('.').to_string())
        .filter(|s| is_safe_ext(s))
        .unwrap_or_else(|| DEFAULT_EXT.to_string())
}

/// A handle extension is interpolated straight into a filename, so keep it to dotted alphanumerics
/// (`json.gz`, `pprof`, `prof`) and reject anything that could escape the profile dir.
fn is_safe_ext(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 16
        && !s.contains("..")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.')
        && !s.starts_with('.')
        && !s.ends_with('.')
}

/// A capture `kind` is interpolated into the target's capture command (e.g. a URL path), and a
/// target name lands in a dir + env-var prefix, so constrain both to an identifier-ish token.
fn validate_token(s: &str) -> Result<(), String> {
    if s.is_empty() {
        return Err("must not be empty".into());
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("only letters, digits, '_' and '-' are allowed".into());
    }
    Ok(())
}

/// pprof query flags that start a server or write binary/file output, denied so a query can only
/// ever produce text. Matched against each whitespace token's flag name (the
/// part after leading dashes, before any `=`), so a legit `list=ServeHTTP` is NOT caught by `http`.
const DENY_FLAGS: &[&str] = &[
    "http", "web", "weblist", "output", "o", "svg", "pdf", "png", "gif", "ps", "eps", "dot", "gv",
    "proto", "raw", "dump", "disasm", "save",
];

/// Read-only guard on `profile_query` using a deny-list. The query value goes to the backend command
/// as `$QUERY`, referenced as a flag; env expansion means shell metacharacters aren't re-evaluated, but
/// we still reject them as defense in depth and a clear "this isn't a query" signal, and we block the
/// flags that bind a server or write files.
fn validate_query(query: &str) -> Result<(), String> {
    if query.trim().is_empty() {
        return Err("must not be empty".into());
    }
    // Belt-and-suspenders: a profile query has no business containing shell control characters.
    const SHELL_META: &[char] = &[';', '|', '&', '`', '>', '<', '\n', '\r'];
    if let Some(c) = query.chars().find(|c| SHELL_META.contains(c)) {
        return Err(format!("contains shell metacharacter {c:?}"));
    }
    if query.contains("$(") {
        return Err("contains a command substitution".into());
    }
    for tok in query.split_whitespace() {
        let name = tok
            .trim_start_matches('-')
            .split('=')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if DENY_FLAGS.contains(&name.as_str()) {
            return Err(format!(
                "{name:?} would start a server or write a file — text queries only \
                 (top, list, peek, traces, tree, ...)"
            ));
        }
    }
    Ok(())
}

fn json(reply: &ProfileReply) -> String {
    serde_json::to_string(reply).unwrap_or_else(|e| {
        json!({"status": "error", "error": format!("serializing reply: {e}")}).to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_serialize_with_a_status_tag() {
        assert!(
            json(&ProfileReply::Captured {
                handle: "cpu-1.pprof".into(),
                target: "epp".into(),
                kind: "cpu".into(),
                seconds: 10,
                bytes: 4096,
            })
            .contains(r#""status":"captured""#)
        );
        assert!(
            json(&ProfileReply::Query {
                output: "flat  cum".into()
            })
            .contains(r#""status":"query""#)
        );
    }

    /// With nothing configured, both tools report a clean `disabled` (the test process sets no
    /// BROKER_PROFILE_*; guard against a stray env in the runner just in case).
    #[test]
    fn unconfigured_profiling_is_disabled() {
        if std::env::vars().any(|(k, _)| k.starts_with("BROKER_PROFILE")) {
            return;
        }
        assert!(profile(None, "cpu", 10).contains(r#""status":"disabled""#));
        assert!(profile_query(None, "top", None).contains(r#""status":"disabled""#));
    }

    #[test]
    fn kind_and_target_tokens_reject_injection_and_paths() {
        assert!(validate_token("cpu").is_ok());
        assert!(validate_token("vllm").is_ok());
        assert!(validate_token("epp-metrics").is_ok());
        assert!(validate_token("goroutine").is_ok());
        assert!(validate_token("").is_err());
        assert!(validate_token("cpu;rm").is_err());
        assert!(validate_token("../etc").is_err());
        assert!(validate_token("a b").is_err());
        assert!(validate_token("a/b").is_err());
    }

    #[test]
    fn handle_extension_is_a_sanitized_bare_extension() {
        // Real domain extensions pass.
        assert!(is_safe_ext("pprof"));
        assert!(is_safe_ext("json.gz"));
        assert!(is_safe_ext("prof"));
        // Anything that could escape the profile dir or mangle the filename is rejected.
        assert!(!is_safe_ext(""));
        assert!(!is_safe_ext("../etc"));
        assert!(!is_safe_ext("a/b"));
        assert!(!is_safe_ext("..gz"));
        assert!(!is_safe_ext(".hidden"));
        assert!(!is_safe_ext("trailing."));
        assert!(!is_safe_ext("way.too.long.extension.value"));
        // A bad/unset value falls back to the neutral default, never panics.
        assert_eq!(ext_or_default(None), "prof");
        assert_eq!(ext_or_default(Some("../bad".into())), "prof");
        assert_eq!(ext_or_default(Some(".json.gz".into())), "json.gz");
    }

    #[test]
    fn query_denylist_blocks_servers_and_file_writes() {
        // Read-only text queries pass.
        assert!(validate_query("top").is_ok());
        assert!(validate_query("top -nodecount=30").is_ok());
        assert!(validate_query("list=HandleRequestBody").is_ok());
        assert!(validate_query("peek=json.Unmarshal").is_ok());
        assert!(validate_query("traces").is_ok());
        // A function-name regex that merely CONTAINS a denied word is fine (flag name is `list`).
        assert!(validate_query("list=ServeHTTP").is_ok());

        // Server / file-writing flags are blocked, even smuggled as a second token.
        assert!(validate_query("http=:8080").is_err());
        assert!(validate_query("web").is_err());
        assert!(validate_query("top -http=:80").is_err());
        assert!(validate_query("top -output=/etc/passwd").is_err());
        assert!(validate_query("svg").is_err());
        assert!(validate_query("-o=/tmp/x").is_err());

        // Shell metacharacters are rejected outright.
        assert!(validate_query("top; rm -rf /").is_err());
        assert!(validate_query("top | sh").is_err());
        assert!(validate_query("top > /etc/x").is_err());
        assert!(validate_query("$(reboot)").is_err());
        assert!(validate_query("").is_err());
    }

    #[test]
    fn handle_resolution_rejects_path_traversal() {
        let dir = Path::new("/var/lib/forge/profiles/epp");
        assert!(resolve_profile(dir, "epp", Some("../../etc/passwd".into())).is_err());
        assert!(resolve_profile(dir, "epp", Some("sub/dir.pprof".into())).is_err());
        // A bare name that doesn't exist is a clean error rather than a panic.
        assert!(resolve_profile(dir, "epp", Some("nope-123.pprof".into())).is_err());
    }
}
