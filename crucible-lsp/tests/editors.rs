//! The editor manifests ship with every release, so a workspace version bump has to move them too.

use std::path::Path;

use serde_json::Value;

fn version(manifest: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(manifest);
    let text = std::fs::read_to_string(&path).expect("manifest");
    let json: Value = serde_json::from_str(&text).expect("manifest is JSON");
    json["version"].as_str().expect("version").to_owned()
}

#[test]
fn editor_manifests_carry_the_workspace_version() {
    for manifest in [
        "editors/vscode/package.json",
        "editors/claude-code/crucible-lsp/.claude-plugin/plugin.json",
    ] {
        assert_eq!(version(manifest), env!("CARGO_PKG_VERSION"), "{manifest}");
    }
}
