//! `crucible migrate` end to end: the dry run prints the diff and leaves the pack alone, `--write`
//! rewrites the manifest into one `crucible check --parse-only` accepts, and a second run has
//! nothing to do.

use std::path::Path;
use std::process::{Command, Output};

const MANIFEST: &str = r#"# counter-live: raise the deployed counter.

[repo]
path = "."

[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && cp value.txt measure.sh workspace/ && git -C workspace init -q"

[agent]
backend   = "command"
agent_cmd = "./bump.nu"
goal      = "Raise the integer in value.txt."

[judge]
measure_cmd = "./measure.sh"
direction   = "higher"
"#;

const MEASURE: &str = "#!/bin/sh\nkubectl --kubeconfig \"$KUBECONFIG\" get cm counter >/dev/null\necho \"{\\\"valid\\\": true, \\\"score\\\": $(cat value.txt)}\"\n";

fn pack(dir: &Path) {
    std::fs::write(dir.join("crucible.toml"), MANIFEST).unwrap();
    std::fs::write(dir.join("measure.sh"), MEASURE).unwrap();
    std::fs::write(dir.join("value.txt"), "1\n").unwrap();
    std::fs::write(
        dir.join("bump.nu"),
        "open value.txt | into int | $in + 1 | save -f value.txt\n",
    )
    .unwrap();
}

fn crucible(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(args)
        .output()
        .expect("run crucible")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_dry_run_prints_the_diff_and_changes_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    pack(dir.path());
    let manifest = dir.path().join("crucible.toml");
    let out = crucible(&["migrate", "--manifest", manifest.to_str().unwrap()]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(
        stdout.starts_with("--- a/crucible.toml\n+++ b/crucible.toml\n"),
        "{stdout}"
    );
    assert!(stdout.contains("+edition = 2\n"), "{stdout}");
    assert!(stdout.contains("+env = [\"KUBECONFIG?\"]\n"), "{stdout}");
    assert!(
        stdout.contains("[crucible migrate] change: edition = 2 (was 1)\n"),
        "{stdout}"
    );
    assert!(
        stdout.contains(
            "[crucible migrate] change: [judge].env declares \"KUBECONFIG?\", read at \
             measure.sh:2\n"
        ),
        "{stdout}"
    );
    assert!(
        stdout.ends_with("[crucible migrate] dry run: pass --write to apply\n"),
        "{stdout}"
    );
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), MANIFEST);
}

#[test]
fn write_rewrites_the_manifest_into_one_that_parses_and_a_rerun_has_nothing_to_do() {
    let dir = tempfile::tempdir().unwrap();
    pack(dir.path());
    let manifest = dir.path().join("crucible.toml");
    let path = manifest.to_str().unwrap();
    let out = crucible(&["migrate", "--manifest", path, "--to", "2", "--write"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains(&format!("[crucible migrate] wrote {path}\n")),
        "{}",
        text(&out.stdout)
    );
    let migrated = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        migrated
            .starts_with("# counter-live: raise the deployed counter.\n\nedition = 2\n\n[repo]\n"),
        "{migrated}"
    );
    assert!(
        migrated.ends_with("direction   = \"higher\"\nenv = [\"KUBECONFIG?\"]\n"),
        "{migrated}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("measure.sh")).unwrap(),
        MEASURE
    );

    let check = crucible(&["check", "--parse-only", "--manifest", path]);
    assert!(check.status.success(), "{}", text(&check.stderr));
    assert!(
        !text(&check.stderr).contains("this pack is edition 1"),
        "{}",
        text(&check.stderr)
    );
    assert!(
        text(&check.stdout).contains("host-env    [judge] commands receive KUBECONFIG (optional)"),
        "{}",
        text(&check.stdout)
    );

    let again = crucible(&["migrate", "--manifest", path, "--write"]);
    assert!(again.status.success(), "{}", text(&again.stderr));
    assert_eq!(
        text(&again.stdout),
        format!("[crucible migrate] {path} is edition 2: nothing to do\n")
    );
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), migrated);
}

#[test]
fn an_unknown_or_lower_target_edition_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    pack(dir.path());
    let manifest = dir.path().join("crucible.toml");
    let path = manifest.to_str().unwrap();
    let unknown = crucible(&["migrate", "--manifest", path, "--to", "3"]);
    assert!(!unknown.status.success());
    assert!(
        text(&unknown.stderr)
            .contains("edition = 3 is not an edition this engine supports; it supports 1 and 2"),
        "{}",
        text(&unknown.stderr)
    );

    assert!(
        crucible(&["migrate", "--manifest", path, "--write"])
            .status
            .success()
    );
    let lower = crucible(&["migrate", "--manifest", path, "--to", "1"]);
    assert!(!lower.status.success());
    assert!(
        text(&lower.stderr).contains(
            "crucible.toml is edition 2; no step lowers an edition, so it cannot move to \
             edition 1"
        ),
        "{}",
        text(&lower.stderr)
    );
}
