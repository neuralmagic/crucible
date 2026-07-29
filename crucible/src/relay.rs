//! Credential relay into the agent's (remote-sandbox) workspace.
//!
//! A remote backend (OpenShell) runs the agent in a nested container that can't see the
//! outer pod's filesystem or service-account creds. The in-Rust OpenShell driver renders
//! each declared relay file host-side and targeted-uploads it to the sandbox path before
//! the turn. This is a generic engine concern, any domain whose tools need cluster/remote
//! access hits it, so the manifest just *declares* the files:
//!
//! ```toml
//! [[agent.relay]]
//! dest = ".kube/config"            # -> ~/.kube/config in the sandbox; kubectl finds it
//! template = """
//! ...server: ${env:RIG_API_URL}...
//! token: ${file:/var/run/kube/token}
//! certificate-authority-data: ${file64:/var/run/kube/ca.crt}
//! """
//! ```
//!
//! A file's content comes from `template` (interpolated), `from_file` (verbatim copy), or
//! `from_cmd` (a host command's stdout, e.g. mint a fresh `kubectl create token`). Template
//! interpolation: `${env:NAME}`, `${file:/path}` (trimmed), `${file64:/path}` (base64 of the
//! raw bytes, for a kubeconfig's `*-data` fields), and `${cmd:...}` (a host command's trimmed
//! stdout, brace-balanced so `kubectl -o jsonpath={.x}` parses). Relayed files never enter
//! the workspace/repo, so tokens cannot leak into git memory or published diffs.

use crate::manifest::RelayFile;
use anyhow::{Context, Result, bail};
use base64::Engine;
use std::path::Path;

/// Render a relay file's content from its single source: `template` (interpolated),
/// `from_file` (verbatim), or `from_cmd` (a host command's stdout). The OpenShell backend
/// targeted-uploads the rendered bytes to the sandbox path.
pub fn render(ws: &Path, rf: &RelayFile) -> Result<String> {
    match (&rf.template, &rf.from_file, &rf.from_cmd) {
        (Some(tpl), None, None) => interpolate(ws, tpl)
            .with_context(|| format!("rendering relay template for {}", rf.dest)),
        (None, Some(src), None) => std::fs::read_to_string(src)
            .with_context(|| format!("reading relay source {src} for {}", rf.dest)),
        (None, None, Some(cmd)) => {
            run(ws, cmd).with_context(|| format!("running relay command for {}", rf.dest))
        }
        _ => bail!(
            "relay `{}` needs exactly one of `template` / `from_file` / `from_cmd`",
            rf.dest
        ),
    }
}

/// Replace `${env:NAME}` / `${file:/p}` / `${file64:/p}` / `${cmd:...}` in a relay template.
/// The closing `}` is matched with brace balancing, so a `${cmd:...}` whose command contains
/// braces (e.g. kubectl `-o jsonpath={.x}`) is parsed whole.
fn interpolate(ws: &Path, tpl: &str) -> Result<String> {
    let mut out = String::with_capacity(tpl.len());
    let mut rest = tpl;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let end = matching_brace(after).context("unterminated `${...}` in relay template")?;
        let expr = &after[..end];
        let val = if let Some(name) = expr.strip_prefix("env:") {
            std::env::var(name).with_context(|| format!("relay template needs env `{name}`"))?
        } else if let Some(path) = expr.strip_prefix("file64:") {
            let bytes =
                std::fs::read(path).with_context(|| format!("relay template reads `{path}`"))?;
            base64::engine::general_purpose::STANDARD.encode(bytes)
        } else if let Some(path) = expr.strip_prefix("file:") {
            std::fs::read_to_string(path)
                .with_context(|| format!("relay template reads `{path}`"))?
                .trim()
                .to_string()
        } else if let Some(cmd) = expr.strip_prefix("cmd:") {
            run(ws, cmd)?
        } else {
            bail!("relay template: unknown `${{{expr}}}` (use env:, file:, file64:, or cmd:)");
        };
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Byte index of the `}` that closes the `${` we just consumed, balancing nested `{}` so a
/// command's own braces don't terminate the directive early.
fn matching_brace(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Run a host command via `sh -c` in the workspace and return its trimmed stdout. Used by
/// `from_cmd` and `${cmd:}`, e.g. mint a fresh token with `kubectl create token …`.
fn run(ws: &Path, cmd: &str) -> Result<String> {
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(ws)
        .output()
        .with_context(|| format!("exec relay command `{cmd}`"))?;
    if !out.status.success() {
        bail!(
            "relay command `{cmd}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_env_file_and_file64() {
        let tmp = std::env::temp_dir().join(format!("relay-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let tok = tmp.join("tok");
        std::fs::write(&tok, "  abc123\n").unwrap();
        unsafe {
            std::env::set_var("RELAY_TEST_VAR", "the-server");
        }

        let tpl = format!(
            "server: ${{env:RELAY_TEST_VAR}}\ntoken: ${{file:{p}}}\nca: ${{file64:{p}}}",
            p = tok.display()
        );
        let out = interpolate(&tmp, &tpl).unwrap();
        assert!(out.contains("server: the-server"));
        assert!(out.contains("token: abc123"), "file is trimmed: {out}");
        // base64 of the raw "  abc123\n" bytes (untrimmed).
        let want = base64::engine::general_purpose::STANDARD.encode("  abc123\n");
        assert!(
            out.contains(&format!("ca: {want}")),
            "file64 untrimmed b64: {out}"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn interpolates_cmd_with_balanced_braces() {
        let tmp = std::env::temp_dir().join(format!("relay-cmd-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // A command whose own braces must not terminate the directive early.
        let out = interpolate(&tmp, "tok: ${cmd:printf '%s' tok-{abc}-end}").unwrap();
        assert_eq!(out, "tok: tok-{abc}-end");
        // A failing command surfaces as an error.
        assert!(interpolate(&tmp, "${cmd:exit 4}").is_err());
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn unknown_directive_and_unterminated_error() {
        let tmp = std::env::temp_dir();
        assert!(interpolate(&tmp, "x ${bogus:y}").is_err());
        assert!(interpolate(&tmp, "x ${env:OK").is_err());
    }
}
