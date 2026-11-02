// rust-embed's #[folder = "ui/dist"] requires the directory to exist at compile time, but the
// SPA build output is gitignored — make sure a Node-less debug checkout still compiles (spa.rs
// then answers 503 "UI not built").
fn main() -> Result<(), String> {
    let dist = std::path::Path::new("ui/dist");
    if !dist.exists() {
        let created = std::fs::create_dir_all(dist).and_then(|()| {
            std::fs::File::open(dist)?.set_modified(std::time::SystemTime::UNIX_EPOCH)
        });
        if let Err(e) = created {
            println!("cargo:warning=could not create ui/dist: {e}");
        }
    }
    println!("cargo:rerun-if-changed=ui/dist");
    require_ui_bundle(dist)?;

    let rev = source_rev();
    println!("cargo:rustc-env=CRUCIBLE_SOURCE_REV={rev}");
    println!(
        "cargo:rustc-env=CRUCIBLE_GIT_SHA={}",
        rev.chars().take(12).collect::<String>()
    );
    println!("cargo:rerun-if-env-changed=CRUCIBLE_GIT_SHA");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    Ok(())
}

/// Refuses a build that would embed an empty SPA. rust-embed only embeds when debug assertions
/// are off; a debug build reads `ui/dist` from disk per request, so it may be built later.
fn require_ui_bundle(dist: &std::path::Path) -> Result<(), String> {
    println!("cargo:rerun-if-env-changed=CRUCIBLE_CONTROLLER_NO_UI");
    let embeds = std::env::var_os("CARGO_CFG_DEBUG_ASSERTIONS").is_none();
    let opted_out = std::env::var_os("CRUCIBLE_CONTROLLER_NO_UI").is_some_and(|v| v == "1");
    if !embeds || opted_out || dist.join("index.html").is_file() {
        return Ok(());
    }
    Err(format!(
        "{} has no index.html, so this build would embed no UI and answer every page with a 503. \
         Run `cd crucible-controller/ui && bun install && bun run build` first, \
         or set CRUCIBLE_CONTROLLER_NO_UI=1 for a deliberately UI-less build",
        dist.display()
    ))
}

/// The commit this binary is built from. CI passes it in the environment because a container build
/// has no `.git` to read; a local build reads git directly. A build with neither still compiles,
/// and says so rather than claiming a commit it cannot prove.
fn source_rev() -> String {
    if let Ok(sha) = std::env::var("CRUCIBLE_GIT_SHA") {
        let sha = sha.trim();
        if !sha.is_empty() {
            return sha.to_string();
        }
    }
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|sha| sha.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}
