//! Embed the openshell-core git rev pinned in Cargo.lock as `CRUCIBLE_OPENSHELL_REV`, so the
//! runtime gateway-version gate can compare the gateway it talks to against the exact fork rev
//! this binary's client was generated from. Pure lockfile string parsing: no network, no git.
//! If the dep ever stops being a git source the value falls back to "unknown" (the gate then
//! skips the rev comparison) rather than failing the build.

use std::path::Path;

fn main() {
    // The workspace lockfile lives one level up from this crate.
    let lock = Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let rev = std::fs::read_to_string(&lock)
        .ok()
        .and_then(|s| openshell_rev(&s))
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CRUCIBLE_OPENSHELL_REV={rev}");
}

/// Find the `openshell-core` package's git source in the lockfile and return the resolved
/// commit (the fragment after `#`). `None` when the package is missing or not a git source.
fn openshell_rev(lock: &str) -> Option<String> {
    let mut in_package = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "name = \"openshell-core\"" {
            in_package = true;
            continue;
        }
        if in_package {
            if line.starts_with("[[package]]") {
                return None; // ran into the next package without a source line
            }
            if let Some(source) = line
                .strip_prefix("source = \"git+")
                .and_then(|s| s.strip_suffix('"'))
            {
                let rev = source.rsplit_once('#')?.1;
                if !rev.is_empty() && rev.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Some(rev.to_string());
                }
                return None;
            }
        }
    }
    None
}
