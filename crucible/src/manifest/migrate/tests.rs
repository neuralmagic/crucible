use crate::host_env::HostTable;
use crate::manifest::Manifest;
use crate::manifest::edition::Edition;
use crate::manifest::migrate::{
    Change, CommandName, Finding, MigrateError, PackFiles, ReadSite, Runner, STEPS, TableRef,
    Unresolved, migrate,
};
use crate::manifest::variant::VariantName;
use std::collections::BTreeMap;

fn pack(files: &[(&str, &str)]) -> PackFiles {
    let files: BTreeMap<String, String> = files
        .iter()
        .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
        .collect();
    PackFiles::new("crucible.toml", files).unwrap()
}

/// The declarations a migration made, as `(table, entry)`.
fn declared(changes: &[Change]) -> Vec<(String, String)> {
    changes
        .iter()
        .filter_map(|c| match c {
            Change::Declared { table, decl, .. } => Some((table.to_string(), decl.to_string())),
            Change::Edition { .. } => None,
        })
        .collect()
}

fn base(table: HostTable) -> TableRef {
    TableRef {
        variant: None,
        table,
    }
}

/// A playbook shaped like the drafts in use: an inject that carries the scripts, a declared
/// secret delivered as a variable, a Starlark workflow with a required parameter, command and
/// evaluate tasks over shell, Python, and nushell scripts, a skill with a helper script, and the
/// `command` backend's stand-in.
const PLAYBOOK_MANIFEST: &str = r#"# tickets-lite: one pass over the open tickets. No [judge]: nothing is scored.
# The scripts are deterministic; the one agent turn assesses.

# The scripts reach the workspace through the inject.
[workspace]
inject = ["scripts"] # every script, frozen

# The manifest requires a backend; the stand-in fails loudly if an agent turn runs.
[agent]
backend   = "command"
agent_cmd = "./scripts/no-agent.sh"
goal      = "Sort the open tickets."

[[secret]]
name = "jira-token"
kind = "opaque"
env = "JIRA_API_TOKEN"

[workflow]
type = "playbook"
file = "workflow.star"
"#;

const PLAYBOOK_WORKFLOW: &str = r#"params = {
    "project": {
        "type": "string",
        "required": True,
        "doc": "Jira project key",
        "pattern": "^[A-Z]+$",
    },
}

collect = command(
    name = "collect",
    run = "python3 scripts/collect.py",
    emits = ["tickets"],
)

assess = skill(
    name = "assess",
    skill = "skills/assess",
    args = {"project": param("project")},
    depends_on = [collect],
)

gate = evaluate(
    name = "gate",
    run = "sh scripts/gate.sh",
    depends_on = [assess],
)

notify = command(
    name = "notify",
    run = "nu scripts/notify.nu",
    depends_on = [gate],
    required = False,
)

probe = command(
    name = "probe",
    run = "sh -c 'curl -fsS \"$PROBE_URL\"'",
    required = False,
)

hook = command(
    name = "hook",
    run = "$HOOK_SCRIPT --verbose",
    required = False,
)

legacy = command(
    name = "legacy",
    run = "python3 scripts/retired.py",
    required = False,
)

tool = command(
    name = "tool",
    run = "./scripts/ticket-tool --check",
    required = False,
)

workflow(type = "playbook", tasks = [collect, assess, gate, notify, probe, hook, legacy, tool])
"#;

const COLLECT_PY: &str = r#""""Collect the open tickets for the assess step."""
import json
import os

import taskio
from jira import client


def main():
    inputs = taskio.read_inputs()
    url = os.environ["JIRA_URL"]
    token = os.environ.get("JIRA_API_TOKEN", "")
    for key in ("JIRA_EMAIL", "JIRA_USER"):
        user = os.environ.get(key)
    os.environ["JIRA_SESSION"] = "cookie"
    session = os.environ["JIRA_SESSION"]
    print(json.dumps({"tickets": client(url, token, user, session, inputs)}))


main()
"#;

const TASKIO_PY: &str = r#"import json
import os


def read_inputs():
    if os.environ.get("TASKIO_TRACE"):
        print("reading inputs")
    with open(os.environ["CRUCIBLE_INPUTS_FILE"]) as f:
        return json.load(f)
"#;

const JIRA_PY: &str = r#"import os


def client(url, token, user, session, inputs):
    timeout = int(os.getenv("JIRA_TIMEOUT", "30"))
    return [url, timeout]
"#;

const GATE_SH: &str = r#"#!/bin/sh
set -eu
# The threshold comes from the launcher's environment, defaulting to 3.
threshold="${GATE_THRESHOLD:-3}"
ROOT="${ROOT:-$HOME/tickets}"
OUT="$ROOT/out"
mkdir -p "$OUT"
export REPORT_DIR="$OUT/report"
python3 scripts/render.py "$CRUCIBLE_RUN_NAME" > "$OUT/render.txt"
echo "{\"pass\": true, \"score\": $threshold}"
"#;

const RENDER_PY: &str = r#"import os
import sys

target = os.environ["REPORT_DIR"]
theme = os.getenv("RENDER_THEME", "plain")
print(target, theme, sys.argv[1])
"#;

const NOTIFY_NU: &str = r#"# Post the gate's verdict.
let hook = $env.SLACK_WEBHOOK_URL
$env.NOTIFY_SENT = "1"
print $env.NOTIFY_SENT
http post $hook "sorted"
"#;

const NO_AGENT_SH: &str =
    "#!/bin/sh\necho \"no agent turn is reachable: $CRUCIBLE_PROMPT\" >&2\nexit 1\n";

const LOOKUP_SH: &str = "#!/bin/sh\ncat \"${ASSESS_CACHE:-/tmp}/tickets.json\"\n";

const BACKFILL_PY: &str = "import os\n\nsince = os.environ[\"BACKFILL_SINCE\"]\nprint(since)\n";

fn playbook() -> PackFiles {
    pack(&[
        ("crucible.toml", PLAYBOOK_MANIFEST),
        ("workflow.star", PLAYBOOK_WORKFLOW),
        (
            "README.md",
            "# tickets-lite\n\nRun it with `crux launch`.\n",
        ),
        ("scripts/collect.py", COLLECT_PY),
        ("scripts/taskio.py", TASKIO_PY),
        ("scripts/jira.py", JIRA_PY),
        ("scripts/gate.sh", GATE_SH),
        ("scripts/render.py", RENDER_PY),
        ("scripts/notify.nu", NOTIFY_NU),
        ("scripts/no-agent.sh", NO_AGENT_SH),
        ("scripts/ticket-tool", "\u{7f}ELF\u{2}\u{1}\u{1}"),
        (
            "skills/assess/SKILL.md",
            "Assess each ticket in the inputs.\n",
        ),
        ("skills/assess/lookup.sh", LOOKUP_SH),
        ("tools/backfill.py", BACKFILL_PY),
    ])
}

/// A scored pack shaped like `examples/counter`, with a measure script and world hooks.
const SCORED_MANIFEST: &str = r#"# counter-live: raise the deployed counter.

[repo]
path = "."

[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && cp value.txt measure.sh deploy.py workspace/ && git -C workspace init -q"

[agent]
backend   = "command"
agent_cmd = "./bump.nu"
goal      = "Raise the integer in value.txt."

[judge]
measure_cmd = "./measure.sh"
direction   = "higher"

[world]
apply_cmd    = "python3 deploy.py apply"
snapshot_cmd = "python3 deploy.py snapshot"
restore_cmd  = "python3 deploy.py restore"
"#;

const MEASURE_SH: &str = r#"#!/bin/sh
kubectl --kubeconfig "$KUBECONFIG" get cm counter >/dev/null
echo "{\"valid\": true, \"score\": $(cat value.txt), \"best\": \"${CRUCIBLE_BEST_SCORE:-none}\"}"
"#;

const DEPLOY_PY: &str = r#"import os
import sys

endpoint = os.environ.get("EPP_ENDPOINT", "127.0.0.1:8000")
if sys.argv[1] == "restore":
    token = os.environ["CRUCIBLE_TOKEN"]
best = os.environ.get("CRUCIBLE_BEST_SCORE")
print(endpoint)
"#;

fn scored() -> PackFiles {
    pack(&[
        ("crucible.toml", SCORED_MANIFEST),
        ("value.txt", "1\n"),
        ("measure.sh", MEASURE_SH),
        ("deploy.py", DEPLOY_PY),
        (
            "bump.nu",
            "open value.txt | into int | $in + 1 | save -f value.txt\n",
        ),
    ])
}

#[test]
fn a_playbook_declares_what_its_host_commands_read_as_optional_in_workflow_env() {
    let migration = migrate(&playbook(), Edition::Two).unwrap();
    assert_eq!(migration.from, Edition::One);
    assert_eq!(migration.to, Edition::Two);
    assert_eq!(
        migration.changes.first(),
        Some(&Change::Edition {
            from: Edition::One,
            to: Edition::Two
        })
    );
    let workflow = |name: &str| ("[workflow]".to_owned(), format!("{name}?"));
    assert_eq!(
        declared(&migration.changes),
        [
            workflow("GATE_THRESHOLD"),
            workflow("HOOK_SCRIPT"),
            workflow("JIRA_API_TOKEN"),
            workflow("JIRA_TIMEOUT"),
            workflow("JIRA_URL"),
            workflow("PROBE_URL"),
            workflow("RENDER_THEME"),
            workflow("ROOT"),
            workflow("SLACK_WEBHOOK_URL"),
            workflow("TASKIO_TRACE"),
        ]
    );
    let manifest = migration.files.manifest();
    assert!(manifest.contains("edition = 2\n"), "{manifest}");
    assert!(
        manifest.contains("env = [\n    \"GATE_THRESHOLD?\",\n"),
        "{manifest}"
    );
    let parsed: Manifest = toml::from_str(manifest).unwrap();
    assert_eq!(parsed.edition, Edition::Two);
    let env = parsed.workflow.unwrap().env;
    assert!(env.iter().all(|decl| !decl.required), "{env:?}");
}

#[test]
fn each_declaration_names_where_the_variable_is_read() {
    let migration = migrate(&playbook(), Edition::Two).unwrap();
    let sites = |name: &str| -> Vec<String> {
        migration
            .changes
            .iter()
            .find_map(|c| match c {
                Change::Declared { decl, read_at, .. } if decl.name.as_str() == name => {
                    Some(read_at.iter().map(ToString::to_string).collect())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("{name} was not declared"))
    };
    assert_eq!(sites("JIRA_URL"), ["scripts/collect.py:11"]);
    assert_eq!(sites("TASKIO_TRACE"), ["scripts/taskio.py:6"]);
    assert_eq!(sites("RENDER_THEME"), ["scripts/render.py:5"]);
    assert_eq!(sites("SLACK_WEBHOOK_URL"), ["scripts/notify.nu:2"]);
    assert_eq!(sites("HOOK_SCRIPT"), ["task \"hook\""]);
    assert_eq!(sites("PROBE_URL"), ["task \"probe\""]);
    let rendered = migration
        .changes
        .iter()
        .find(|c| matches!(c, Change::Declared { decl, .. } if decl.name.as_str() == "ROOT"))
        .map(ToString::to_string);
    assert_eq!(
        rendered.as_deref(),
        Some("[workflow].env declares \"ROOT?\", read at scripts/gate.sh:5")
    );
}

#[test]
fn a_variable_set_before_it_is_read_is_not_declared() {
    let migration = migrate(&playbook(), Edition::Two).unwrap();
    let names: Vec<String> = declared(&migration.changes)
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    for set_first in [
        "OUT?",
        "threshold?",
        "REPORT_DIR?",
        "JIRA_SESSION?",
        "NOTIFY_SENT?",
    ] {
        assert!(
            !names.iter().any(|n| n == set_first),
            "{set_first}: {names:?}"
        );
    }
}

#[test]
fn the_base_set_and_what_the_engine_sets_are_never_declared() {
    let migration = migrate(&playbook(), Edition::Two).unwrap();
    let names: Vec<String> = declared(&migration.changes)
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    for provided in ["HOME?", "CRUCIBLE_RUN_NAME?", "CRUCIBLE_INPUTS_FILE?"] {
        assert!(
            !names.iter().any(|n| n == provided),
            "{provided}: {names:?}"
        );
    }
}

#[test]
fn commands_whose_code_cannot_be_read_and_unattributed_reads_need_review() {
    let migration = migrate(&playbook(), Edition::Two).unwrap();
    let workflow = base(HostTable::Workflow);
    let unresolved = |task: &str, text: &str, reason: Unresolved| Finding::UnresolvedCommand {
        table: workflow.clone(),
        command: CommandName::Task(task.to_owned()),
        text: text.to_owned(),
        reason,
    };
    assert_eq!(
        migration.needs_review,
        [
            unresolved(
                "hook",
                "$HOOK_SCRIPT --verbose",
                Unresolved::Indirect {
                    word: "$HOOK_SCRIPT".to_owned()
                }
            ),
            unresolved(
                "legacy",
                "python3 scripts/retired.py",
                Unresolved::NotInPack {
                    path: "scripts/retired.py".to_owned()
                }
            ),
            unresolved(
                "probe",
                "sh -c 'curl -fsS \"$PROBE_URL\"'",
                Unresolved::InlineCode {
                    interpreter: "sh".to_owned()
                }
            ),
            unresolved(
                "tool",
                "./scripts/ticket-tool --check",
                Unresolved::Unreadable {
                    path: "scripts/ticket-tool".to_owned()
                }
            ),
            Finding::UnattributedReads {
                path: "tools/backfill.py".to_owned(),
                names: vec!["BACKFILL_SINCE".to_owned()],
            },
            Finding::NotAffected {
                path: "scripts/no-agent.sh".to_owned(),
                names: vec!["CRUCIBLE_PROMPT".to_owned()],
                runner: Runner::AgentCmd,
            },
            Finding::NotAffected {
                path: "skills/assess/lookup.sh".to_owned(),
                names: vec!["ASSESS_CACHE".to_owned()],
                runner: Runner::AgentTurn,
            },
            Finding::ComputedRead {
                site: ReadSite::File {
                    path: "scripts/collect.py".to_owned(),
                    line: 14,
                },
            },
        ]
    );
    let shown: Vec<String> = migration
        .needs_review
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        shown[1],
        "[workflow] task \"legacy\" `python3 scripts/retired.py` runs scripts/retired.py, which \
         the pack does not carry"
    );
    assert_eq!(
        shown[6],
        "skills/assess/lookup.sh reads ASSESS_CACHE, and only an agent turn in its sandbox runs \
         it, which the edition does not change"
    );
}

#[test]
fn a_scored_pack_declares_judge_and_world_reads_in_their_own_tables() {
    let migration = migrate(&scored(), Edition::Two).unwrap();
    assert_eq!(
        migration.files.manifest(),
        r#"# counter-live: raise the deployed counter.

edition = 2

[repo]
path = "."

[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && cp value.txt measure.sh deploy.py workspace/ && git -C workspace init -q"

[agent]
backend   = "command"
agent_cmd = "./bump.nu"
goal      = "Raise the integer in value.txt."

[judge]
measure_cmd = "./measure.sh"
direction   = "higher"
env = ["KUBECONFIG?"]

[world]
apply_cmd    = "python3 deploy.py apply"
snapshot_cmd = "python3 deploy.py snapshot"
restore_cmd  = "python3 deploy.py restore"
env = ["CRUCIBLE_BEST_SCORE?", "EPP_ENDPOINT?"]
"#
    );
    assert!(
        migration.needs_review.is_empty(),
        "{:?}",
        migration.needs_review
    );
}

#[test]
fn a_variant_with_its_own_table_declares_in_its_own_copy() {
    let text = format!(
        "{SCORED_MANIFEST}\n[variant.cluster.judge]\nmeasure_cmd = \"./measure-cluster.sh\"\ndirection = \"higher\"\n"
    );
    let mut files = scored().into_files();
    files.insert("crucible.toml".to_owned(), text);
    files.insert(
        "measure.sh".to_owned(),
        "#!/bin/sh\necho \"{\\\"valid\\\": true, \\\"score\\\": $LOCAL_SCORE}\"\n".to_owned(),
    );
    files.insert("measure-cluster.sh".to_owned(), MEASURE_SH.to_owned());
    let pack = PackFiles::new("crucible.toml", files).unwrap();
    let migration = migrate(&pack, Edition::Two).unwrap();
    assert_eq!(
        declared(&migration.changes),
        [
            ("[judge]".to_owned(), "LOCAL_SCORE?".to_owned()),
            ("[world]".to_owned(), "CRUCIBLE_BEST_SCORE?".to_owned()),
            ("[world]".to_owned(), "EPP_ENDPOINT?".to_owned()),
            (
                "[variant.cluster.judge]".to_owned(),
                "KUBECONFIG?".to_owned()
            ),
        ]
    );
    let manifest = migration.files.manifest();
    assert!(
        manifest.ends_with(
            "[variant.cluster.judge]\nmeasure_cmd = \"./measure-cluster.sh\"\ndirection = \"higher\"\nenv = [\"KUBECONFIG?\"]\n"
        ),
        "{manifest}"
    );
    let cluster = VariantName::parse("cluster").unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crucible.toml");
    std::fs::write(&path, manifest).unwrap();
    let judge = Manifest::load_variant(&path, Some(&cluster))
        .unwrap()
        .judge
        .unwrap();
    assert_eq!(
        judge
            .env
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["KUBECONFIG?"]
    );
}

#[test]
fn migrating_an_edition_two_pack_is_a_no_op() {
    let once = migrate(&playbook(), Edition::Two).unwrap();
    let twice = migrate(&once.files, Edition::Two).unwrap();
    assert!(twice.is_noop());
    assert_eq!(twice.from, Edition::Two);
    assert_eq!(twice.files, once.files);
    assert!(twice.needs_review.is_empty());
    assert!(twice.diffs(&once.files).is_empty());
}

#[test]
fn a_migration_is_deterministic() {
    assert_eq!(
        migrate(&playbook(), Edition::Two).unwrap(),
        migrate(&playbook(), Edition::Two).unwrap()
    );
}

#[test]
fn comments_and_formatting_survive_and_the_diff_only_adds_lines() {
    let before = playbook();
    let migration = migrate(&before, Edition::Two).unwrap();
    let after = migration.files.manifest();
    for line in PLAYBOOK_MANIFEST.lines() {
        assert!(after.lines().any(|l| l == line), "lost {line:?}:\n{after}");
    }
    assert!(after.starts_with(
        "# tickets-lite: one pass over the open tickets. No [judge]: nothing is scored.\n# The \
         scripts are deterministic; the one agent turn assesses.\n\nedition = 2\n\n# The scripts \
         reach the workspace through the inject.\n[workspace]\n"
    ));
    let diffs = migration.diffs(&before);
    assert_eq!(diffs.len(), 1);
    assert_eq!(diffs[0].path, "crucible.toml");
    let body: Vec<&str> = diffs[0].unified.lines().skip(2).collect();
    assert!(
        diffs[0]
            .unified
            .starts_with("--- a/crucible.toml\n+++ b/crucible.toml\n@@ ")
    );
    assert!(
        body.iter().all(|l| !l.starts_with('-')),
        "{}",
        diffs[0].unified
    );
    for path in before.files().keys() {
        if path != before.manifest_path() {
            assert_eq!(before.files().get(path), migration.files.files().get(path));
        }
    }
}

#[test]
fn an_unknown_target_edition_is_refused_naming_the_supported_ones() {
    let error = Edition::try_from(3)
        .map_err(MigrateError::from)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "edition = 3 is not an edition this engine supports; it supports 1 and 2"
    );
}

#[test]
fn lowering_an_edition_is_refused() {
    let raised = migrate(&scored(), Edition::Two).unwrap().files;
    assert_eq!(
        migrate(&raised, Edition::One).unwrap_err(),
        MigrateError::Downgrade {
            manifest: "crucible.toml".to_owned(),
            from: Edition::Two,
            to: Edition::One,
        }
    );
}

#[test]
fn every_edition_after_the_first_has_exactly_one_step() {
    let stepped: Vec<Edition> = STEPS.iter().map(|s| s.to).collect();
    assert_eq!(stepped, Edition::SUPPORTED[1..]);
}

#[test]
fn a_manifest_that_does_not_parse_is_refused_without_naming_the_scratch_copy() {
    let broken = pack(&[("crucible.toml", "[repo]\npath = \".\"\n")]);
    let error = migrate(&broken, Edition::Two).unwrap_err();
    let MigrateError::Parse { manifest, message } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(manifest, "crucible.toml");
    assert!(message.contains("missing field `agent`"), "{message}");
    let scratch = std::env::temp_dir();
    assert!(
        !message.contains(scratch.to_string_lossy().trim_end_matches('/')),
        "{message}"
    );
}

#[test]
fn a_workflow_that_does_not_compile_is_refused_naming_its_file() {
    let mut files = playbook().into_files();
    files.insert(
        "workflow.star".to_owned(),
        "x = command(name = \"x\", run = \"true\", emits = [\"complete\"])\nworkflow(type = \"playbook\", tasks = [x])\n".to_owned(),
    );
    let error = migrate(
        &PackFiles::new("crucible.toml", files).unwrap(),
        Edition::Two,
    )
    .unwrap_err();
    let MigrateError::Compile { file, message } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(file, "workflow.star");
    assert!(message.starts_with("workflow.star:2:"), "{message}");
}

#[test]
fn a_pack_needs_its_manifest_at_the_root_and_relative_paths() {
    let files = |entries: &[(&str, &str)]| -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
            .collect()
    };
    assert_eq!(
        PackFiles::new("crucible.toml", files(&[("README.md", "")])).unwrap_err(),
        MigrateError::NoManifest {
            path: "crucible.toml".to_owned()
        }
    );
    assert_eq!(
        PackFiles::new(
            "crucible.toml",
            files(&[("crucible.toml", ""), ("../escape.sh", "")])
        )
        .unwrap_err(),
        MigrateError::BadPath {
            path: "../escape.sh".to_owned()
        }
    );
}

#[test]
fn a_pack_read_from_disk_keeps_its_text_files_and_skips_binary_ones() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("scripts")).unwrap();
    std::fs::write(dir.path().join("crucible.toml"), SCORED_MANIFEST).unwrap();
    std::fs::write(dir.path().join("scripts/run.sh"), "echo $X\n").unwrap();
    std::fs::write(
        dir.path().join("logo.png"),
        [0x89, b'P', b'N', b'G', 0xff, 0xfe],
    )
    .unwrap();
    let pack = PackFiles::read(&dir.path().join("crucible.toml")).unwrap();
    assert_eq!(pack.manifest_path(), "crucible.toml");
    assert_eq!(
        pack.files().keys().collect::<Vec<_>>(),
        ["crucible.toml", "scripts/run.sh"]
    );
}
