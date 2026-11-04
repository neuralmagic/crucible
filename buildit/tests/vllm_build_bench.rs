//! examples/vllm-build-bench's Python suites: the replay driver, and the informer against this
//! package's `buildit cache`.

use std::path::Path;
use std::process::Command;

#[test]
fn the_vllm_build_bench_python_suites_pass() {
    let bench = Path::new(env!("CARGO_MANIFEST_DIR")).join("../examples/vllm-build-bench/bench");
    let out = Command::new("python3")
        .args(["-m", "unittest", "discover", "-s"])
        .arg(&bench)
        .env("BUILDIT_BIN", env!("CARGO_BIN_EXE_buildit"))
        .output()
        .expect("python3 runs");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
