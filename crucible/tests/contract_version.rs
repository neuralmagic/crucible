//! `crucible --contract-version` prints the crate constant the runtime image is labelled with.

use std::process::Command;

#[test]
fn contract_version_flag_prints_constant() {
    let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .arg("--contract-version")
        .output()
        .expect("run crucible");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        format!("{}\n", crucible_contract::CONTRACT_VERSION)
    );
}

#[test]
fn contract_version_is_semver() {
    let parts: Vec<u64> = crucible_contract::CONTRACT_VERSION
        .split('.')
        .map(|p| p.parse().expect("numeric semver component"))
        .collect();
    assert_eq!(parts.len(), 3, "{}", crucible_contract::CONTRACT_VERSION);
}
