//! The optimize-loop pack's verifier is stdlib Python that runs in the loop pod; its own
//! unittest suite is the test, run here so CI runs it with the rest of the workspace.

use std::path::Path;
use std::process::Command;

#[test]
fn the_optimize_loop_verifier_suite_passes() {
    let verify = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/optimize-loop/verify");
    let out = Command::new("python3")
        .args(["-m", "unittest", "discover", "-s"])
        .arg(&verify)
        .output()
        .expect("python3 runs");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
