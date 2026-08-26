//! Every render the controller links (RFC-0004 C-LINKED-RENDER) produces, for the same inputs,
//! exactly the bytes the `crucible` command line prints. Each test drives both paths.

use crucible::deploy::{
    DeployProfile, PackDelivery, PackPath, PlaybookLaunch, ProposeTier, RenderOpts, TurnKind,
    TurnOpts, render_turn, render_yaml,
};
use crucible::flow::{FlowFormat, FlowInput, render};
use crucible::manifest::Harness;
use crucible::plan::starlark::{compile_file_with, declared_params, parent_or_cwd};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixture(rel: &str) -> PathBuf {
    crate_dir().join("tests/fixtures").join(rel)
}

fn delta_profile() -> PathBuf {
    fixture("deploy/gamma/delta/profile.toml")
}

/// Run the binary and return its stdout; a non-zero exit fails the test with stderr.
fn cli(args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_crucible"))
        .args(args)
        .output()
        .expect("spawn crucible");
    assert!(
        out.status.success(),
        "crucible {args:?} failed ({}): {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("stdout is UTF-8")
}

fn scratch(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("crucible-render-parity-{name}-"))
        .tempdir()
        .expect("tempdir")
}

#[test]
fn deploy_render_matches_render_yaml() {
    let manifest = fixture("domains/gamma/crucible.delta.toml");
    let profile = delta_profile();
    let from_cli = cli(&[
        "deploy",
        "render",
        "--manifest",
        manifest.to_str().unwrap(),
        "--profile",
        profile.to_str().unwrap(),
        "--no-pin",
        "--iterations",
        "3",
        "--max-cost",
        "12.5",
        "--pr-repo",
        "example/fork",
        "--harness",
        "hermes",
        "--model",
        "hermes-4-70b",
    ]);
    let from_lib = render_yaml(
        &manifest,
        &profile,
        &RenderOpts {
            iterations: 3,
            max_cost: 12.5,
            digests: None,
            pr_repo: Some("example/fork".to_string()),
            pack: None,
            clusters_file: None,
            harness: Some(Harness::Hermes),
            model: Some("hermes-4-70b".to_string()),
            playbook: None,
        },
    )
    .expect("library render");
    assert!(from_lib.contains("kind: Pod"));
    assert_eq!(from_cli, from_lib);
}

fn write_playbook_pack(dir: &Path) {
    std::fs::write(
        dir.join("crucible.toml"),
        r#"
[repo]
path = "."
[agent]
backend = "openshell"
goal = "draft the roundup"
sandbox_image = "registry.example.com/alpha-sandbox:latest"
[workflow]
type = "playbook"
file = "workflow.star"
"#,
    )
    .expect("crucible.toml");
    std::fs::write(
        dir.join("workflow.star"),
        r#"
params = {
    "topic": {"type": "string", "required": True, "doc": "what to write up"},
    "depth": {"type": "string", "default": "--shallow", "doc": "how far to dig"},
}

draft = command(
    name = "draft",
    run = "echo " + param("topic") + " " + param("depth"),
)
"#,
    )
    .expect("workflow.star");
}

#[test]
fn deploy_render_pack_playbook_matches_render_yaml() {
    let tmp = scratch("pack");
    let pack = tmp.path().join("roundup");
    std::fs::create_dir_all(&pack).expect("mkdir pack");
    write_playbook_pack(&pack);
    let manifest = pack.join("crucible.toml");
    let profile = delta_profile();
    let from_cli = cli(&[
        "deploy",
        "render",
        "--manifest",
        manifest.to_str().unwrap(),
        "--profile",
        profile.to_str().unwrap(),
        "--no-pin",
        "--pack",
        "--pack-configmap-name",
        "crucible-run-42-pack",
        "--playbook",
        "--max-cost",
        "4.5",
        "--max-time",
        "30m",
        "--param",
        "topic=attention sinks",
        "--param",
        "depth=--deep",
    ]);
    let from_lib = render_yaml(
        &manifest,
        &profile,
        &RenderOpts {
            iterations: 1,
            max_cost: 4.5,
            digests: None,
            pr_repo: None,
            pack: Some(PackDelivery {
                configmap_name: "crucible-run-42-pack".to_string(),
            }),
            clusters_file: None,
            harness: None,
            model: None,
            playbook: Some(PlaybookLaunch {
                max_time: "30m".parse().expect("30m is a duration"),
                max_cost: 4.5,
                params: BTreeMap::from([
                    ("topic".to_string(), "attention sinks".to_string()),
                    ("depth".to_string(), "--deep".to_string()),
                ]),
            }),
        },
    )
    .expect("library render");
    assert!(from_lib.contains("kind: ConfigMap"));
    assert!(from_lib.contains("plan run"));
    assert_eq!(from_cli, from_lib);
}

#[test]
fn deploy_render_turn_matches_render_turn() {
    let tmp = scratch("turn");
    let goal = tmp.path().join("goal.md");
    std::fs::write(&goal, "# Make the router faster\n\nOne p99 at a time.\n").expect("goal.md");
    let profile = delta_profile();
    let from_cli = cli(&[
        "deploy",
        "render-turn",
        "--profile",
        profile.to_str().unwrap(),
        "--name",
        "crucible-scope-7",
        "--issue",
        "example/router#7",
        "--goal-file",
        goal.to_str().unwrap(),
        "--repo-url",
        "https://github.com/example/router.git",
        "--sandbox-image",
        "registry.example.com/router-sandbox:latest",
        "--max-cost",
        "2.5",
        "--no-pin",
        "--turn-kind",
        "scope",
        "--tier",
        "t1",
        "--gaming-refine-rounds",
        "2",
        "--authoritative",
        "--harness",
        "codex",
        "--model",
        "gpt-5.6-sol",
    ]);
    let from_lib = render_turn(
        &DeployProfile::load(&profile).expect("profile parses"),
        &TurnOpts {
            kind: TurnKind::Scope,
            name: "crucible-scope-7".to_string(),
            issue: "example/router#7".to_string(),
            goal_text: Some(std::fs::read_to_string(&goal).expect("goal")),
            repo_url: "https://github.com/example/router.git".to_string(),
            repo_ref: None,
            sandbox_image: "registry.example.com/router-sandbox:latest".to_string(),
            max_cost: 2.5,
            digests: None,
            tier: Some(ProposeTier::T1),
            gaming_refine_rounds: 2,
            skip_gaming_review: false,
            authoritative: true,
            harness: Some(Harness::Codex),
            model: Some("gpt-5.6-sol".to_string()),
            pack_path: None,
        },
    )
    .expect("library render");
    assert!(from_lib.contains("--tier t1"));
    assert_eq!(from_cli, from_lib);
}

/// `--pack-path` renders the same pack turn through the CLI as through the library, so a caller
/// that still shells the binary and one that links it dispatch the same pod.
#[test]
fn pack_path_renders_the_same_through_cli_and_library() {
    let profile = delta_profile();
    let from_cli = cli(&[
        "deploy",
        "render-turn",
        "--profile",
        profile.to_str().unwrap(),
        "--name",
        "crucible-scope-8",
        "--issue",
        "example/router#8",
        "--repo-url",
        "https://github.com/example/router.git",
        "--sandbox-image",
        "registry.example.com/router-sandbox:latest",
        "--no-pin",
        "--turn-kind",
        "scope",
        "--pack-path",
        "examples/selfhost",
    ]);
    let from_lib = render_turn(
        &DeployProfile::load(&profile).expect("profile parses"),
        &TurnOpts {
            kind: TurnKind::Scope,
            name: "crucible-scope-8".to_string(),
            issue: "example/router#8".to_string(),
            goal_text: None,
            repo_url: "https://github.com/example/router.git".to_string(),
            repo_ref: None,
            sandbox_image: "registry.example.com/router-sandbox:latest".to_string(),
            max_cost: 5.0,
            digests: None,
            tier: None,
            gaming_refine_rounds: 0,
            skip_gaming_review: false,
            authoritative: false,
            harness: None,
            model: None,
            pack_path: Some(PackPath::parse("examples/selfhost").expect("valid")),
        },
    )
    .expect("library render");
    assert!(from_lib.contains(r#"scope --pack "$CHECKOUT/examples/selfhost""#));
    assert!(!from_lib.contains("--propose"));
    assert_eq!(from_cli, from_lib);
}

fn paper_workflow() -> PathBuf {
    crate_dir().join("../examples/paper/workflow.star")
}

#[test]
fn plan_params_matches_declared_params() {
    let file = paper_workflow();
    let from_cli = cli(&["plan", "params", "--file", file.to_str().unwrap()]);
    let source = std::fs::read_to_string(&file).expect("workflow source");
    let schema = declared_params(&source, &file).expect("declared params");
    let from_lib = format!(
        "{}\n",
        serde_json::to_string_pretty(&schema).expect("schema serializes")
    );
    assert!(from_lib.contains("paper_url"));
    assert_eq!(from_cli, from_lib);
}

#[test]
fn plan_compile_workflow_matches_compile_file_with() {
    let file = paper_workflow();
    let from_cli = cli(&[
        "plan",
        "compile-workflow",
        "--file",
        file.to_str().unwrap(),
        "--param",
        "paper_url=https://arxiv.org/abs/2309.17453",
    ]);
    let params = BTreeMap::from([(
        "paper_url".to_string(),
        "https://arxiv.org/abs/2309.17453".to_string(),
    )]);
    let compiled = compile_file_with(&file, parent_or_cwd(&file), &params).expect("compiles");
    assert!(compiled.declares_params);
    assert_eq!(from_cli, compiled.canonical_json);
}

#[test]
fn flow_matches_render() {
    let session = crate_dir().join("testdata/flow/run7-session.jsonl");
    let input = FlowInput {
        session_log: std::fs::read_to_string(&session).expect("session log"),
        spans_json: None,
    };
    let tmp = scratch("flow");
    for (ext, format) in [
        ("json", FlowFormat::Json),
        ("dot", FlowFormat::Dot),
        ("mmd", FlowFormat::Mermaid),
        ("html", FlowFormat::Html),
    ] {
        let out = tmp.path().join(format!("flow.{ext}"));
        cli(&[
            "flow",
            "--session",
            session.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ]);
        let from_cli = std::fs::read_to_string(&out).expect("the CLI wrote --out");
        let from_lib = render(&input, format).expect("library render");
        assert_eq!(from_cli, from_lib, "{ext}");
    }
}
