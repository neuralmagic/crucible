// rust-embed's #[folder = "ui/dist"] requires the directory to exist at compile time, but the
// SPA build output is gitignored — make sure a Node-less checkout still compiles (spa.rs then
// answers 503 "UI not built").
fn main() {
    if let Err(e) = std::fs::create_dir_all("ui/dist") {
        println!("cargo:warning=could not create ui/dist: {e}");
    }
    println!("cargo:rerun-if-changed=ui/dist");

    let rev = source_rev();
    println!("cargo:rustc-env=CRUCIBLE_SOURCE_REV={rev}");
    println!(
        "cargo:rustc-env=CRUCIBLE_GIT_SHA={}",
        rev.chars().take(12).collect::<String>()
    );
    println!("cargo:rerun-if-env-changed=CRUCIBLE_GIT_SHA");
    println!("cargo:rerun-if-changed=../.git/HEAD");
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
