//! Repo automation, cargo-xtask style (`cargo xtask <task>`).
//!
//! `openshell-rev` prints the openshell-core git rev pinned in the workspace Cargo.lock.
//! `modgraph` prints the crucible crate's module dependency graph; `--check` fails on cycles.
//! crucible's build.rs keeps its own minimal parse of the same lockfile entry (a build script
//! can't depend on a workspace crate), so change both together if the lockfile shape moves.

mod modgraph;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("openshell-rev") => match openshell_rev_from_workspace() {
            Ok(rev) => {
                println!("{rev}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("xtask openshell-rev: {e}");
                ExitCode::FAILURE
            }
        },
        Some("modgraph") => match modgraph::run(&args.collect::<Vec<_>>()) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(e) => {
                eprintln!("xtask modgraph: {e}");
                ExitCode::FAILURE
            }
        },
        Some(other) => {
            eprintln!("xtask: unknown task '{other}' (available: openshell-rev, modgraph)");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("usage: cargo xtask <task>  (available: openshell-rev, modgraph)");
            ExitCode::FAILURE
        }
    }
}

/// Read the workspace `Cargo.lock` (one level above this crate) and extract the rev.
fn openshell_rev_from_workspace() -> Result<String, String> {
    let lock_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .map_err(|e| format!("reading {}: {e}", lock_path.display()))?;
    openshell_rev(&lock)
}

/// The subset of a Cargo.lock entry this task reads.
#[derive(serde::Deserialize)]
struct Lockfile {
    #[serde(default)]
    package: Vec<Package>,
}

#[derive(serde::Deserialize)]
struct Package {
    name: String,
    source: Option<String>,
}

/// Extract openshell-core's resolved git rev from lockfile contents: the fragment after `#`
/// in its `git+…` source URL, required to be a full 40-char commit hash.
fn openshell_rev(lock: &str) -> Result<String, String> {
    let lockfile: Lockfile =
        toml::from_str(lock).map_err(|e| format!("Cargo.lock is not valid TOML: {e}"))?;
    let package = lockfile
        .package
        .iter()
        .find(|p| p.name == "openshell-core")
        .ok_or("no openshell-core package in Cargo.lock")?;
    let source = package
        .source
        .as_deref()
        .ok_or("openshell-core has no source (a workspace path dep?)")?;
    if !source.starts_with("git+") {
        return Err(format!("openshell-core is not a git source: {source}"));
    }
    let (_, rev) = source
        .split_once('#')
        .ok_or_else(|| format!("openshell-core git source has no resolved rev: {source}"))?;
    if rev.len() != 40 || !rev.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "openshell-core resolved rev is not a 40-char commit hash: {rev}"
        ));
    }
    Ok(rev.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "anyhow"
version = "1.0.98"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "e16d2d3311acee920a9eb8d33b8cbc1787ce4a264e85f964c2404b969bdcd487"

[[package]]
name = "openshell-core"
version = "0.0.0"
source = "git+https://github.com/wseaton/OpenShell.git?rev=bc8342bb53730f43bd9e27fc2f890745fbcb607f#bc8342bb53730f43bd9e27fc2f890745fbcb607f"
dependencies = [
 "base64",
]

[[package]]
name = "crucible"
version = "0.2.0"
"#;

    #[test]
    fn extracts_the_resolved_rev() {
        assert_eq!(
            openshell_rev(LOCK).unwrap(),
            "bc8342bb53730f43bd9e27fc2f890745fbcb607f"
        );
    }

    #[test]
    fn missing_package_is_a_clear_error() {
        let err = openshell_rev("version = 4\n").unwrap_err();
        assert!(err.contains("no openshell-core package"), "{err}");
    }

    #[test]
    fn path_dep_without_source_is_a_clear_error() {
        let lock = "[[package]]\nname = \"openshell-core\"\nversion = \"0.0.0\"\n";
        let err = openshell_rev(lock).unwrap_err();
        assert!(err.contains("no source"), "{err}");
    }

    #[test]
    fn registry_source_is_rejected() {
        let lock = "[[package]]\nname = \"openshell-core\"\nversion = \"0.0.1\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n";
        let err = openshell_rev(lock).unwrap_err();
        assert!(err.contains("not a git source"), "{err}");
    }

    #[test]
    fn short_or_missing_rev_fragment_is_rejected() {
        let no_fragment = "[[package]]\nname = \"openshell-core\"\nversion = \"0.0.0\"\nsource = \"git+https://github.com/wseaton/OpenShell.git?rev=abc\"\n";
        assert!(
            openshell_rev(no_fragment)
                .unwrap_err()
                .contains("no resolved rev")
        );
        let short = "[[package]]\nname = \"openshell-core\"\nversion = \"0.0.0\"\nsource = \"git+https://github.com/wseaton/OpenShell.git?rev=abc#abc123\"\n";
        assert!(
            openshell_rev(short)
                .unwrap_err()
                .contains("not a 40-char commit hash")
        );
    }

    #[test]
    fn garbage_toml_is_a_clear_error() {
        let err = openshell_rev("not toml at all [[[").unwrap_err();
        assert!(err.contains("not valid TOML"), "{err}");
    }
}
