//! `crucible plan compile-workflow` prints what the compiler noticed and still compiles.

use std::process::Command;

fn compile(source: &str) -> (bool, String, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let file = dir.path().join("workflow.star");
    std::fs::write(&file, source).expect("write source");
    let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(["plan", "compile-workflow", "--file"])
        .arg(&file)
        .output()
        .expect("run crucible");
    (
        out.status.success(),
        String::from_utf8(out.stdout).expect("stdout"),
        String::from_utf8(out.stderr).expect("stderr"),
    )
}

#[test]
fn a_deprecated_argument_is_a_warning_on_stderr_and_the_graph_still_prints() {
    let (ok, stdout, stderr) = compile(
        "scan = command(name = \"scan\", run = \"true\", emits = [\"items\"])\n\
         fan = command(name = \"fan\", run = \"true\", depends_on = [scan], over = scan.items, max_fanout = 4)\n\
         workflow(type = \"playbook\", tasks = [scan, fan])\n",
    );
    assert!(ok, "{stderr}");
    let warnings: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("warning: "))
        .collect();
    assert_eq!(warnings.len(), 2, "{stderr}");
    assert!(
        warnings[0].ends_with(
            "workflow.star:2:83: `max_fanout` is deprecated; write `over = each(producer.field, max = N)`"
        ),
        "{stderr}"
    );
    assert!(
        warnings[1].ends_with("workflow.star:2:43: `over` already makes `scan` a dependency"),
        "{stderr}"
    );
    let graph: serde_json::Value = serde_json::from_str(&stdout).expect("the compiled graph");
    assert_eq!(graph["task"][1]["max_fanout"], 4);
}

#[test]
fn a_pack_in_the_current_forms_prints_no_warning() {
    let (ok, _, stderr) = compile(
        "scan = command(name = \"scan\", run = \"true\", emits = [\"items\"])\n\
         fan = command(name = \"fan\", run = \"true\", over = each(scan.items, max = 4))\n\
         workflow(type = \"playbook\", tasks = [scan, fan])\n",
    );
    assert!(ok, "{stderr}");
    assert!(!stderr.contains("warning: "), "{stderr}");
}
