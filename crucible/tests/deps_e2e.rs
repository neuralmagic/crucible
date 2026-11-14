//! Remote Starlark dependencies, end to end through the binary: a library in a real git
//! repository, locked, fetched into a cache, and loaded by a pack that holds none of its files.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LIBRARY_GATES: &str = r#"
load("lib/names.star", "suffix")

def review(topic):
    return agent(
        name = "review-" + topic + suffix,
        prompt = prompt_file("prompts/review.md"),
        isolated = True,
    )
"#;

const WORKFLOW: &str = r#"
load("@gates//gates.star", "review")

own = agent(name = "own", prompt = prompt_file("prompts/review.md"), isolated = True)
direct = agent(name = "direct", prompt = prompt_file("@gates//prompts/review.md"), isolated = True)
workflow([review("api"), own, direct])
"#;

struct World {
    root: tempfile::TempDir,
}

impl World {
    fn new() -> Self {
        let world = World {
            root: tempfile::tempdir().unwrap(),
        };
        let library = world.library();
        std::fs::create_dir_all(library.join("lib")).unwrap();
        std::fs::create_dir_all(library.join("prompts")).unwrap();
        git(&library, &["init", "--quiet", "--initial-branch", "main"]);
        std::fs::write(library.join("gates.star"), LIBRARY_GATES).unwrap();
        std::fs::write(library.join("lib/names.star"), "suffix = \"-v1\"\n").unwrap();
        std::fs::write(library.join("prompts/review.md"), "Library review one.\n").unwrap();
        world.commit("one");

        let pack = world.pack();
        std::fs::create_dir_all(pack.join("prompts")).unwrap();
        std::fs::write(pack.join("prompts/review.md"), "Pack review.\n").unwrap();
        std::fs::write(pack.join("workflow.star"), WORKFLOW).unwrap();
        std::fs::write(
            pack.join("crucible.toml"),
            format!(
                "[deps.gates]\ngit = \"file://{}\"\nrev = \"main\"\n",
                library.display()
            ),
        )
        .unwrap();
        world
    }

    fn library(&self) -> PathBuf {
        self.root.path().join("library")
    }

    fn pack(&self) -> PathBuf {
        self.root.path().join("pack")
    }

    fn commit(&self, message: &str) {
        git(&self.library(), &["add", "."]);
        git(&self.library(), &["commit", "--quiet", "-m", message]);
    }

    fn crucible(&self, cache: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_crucible"))
            .args(args)
            .current_dir(self.pack())
            .env("CRUCIBLE_DEPS_DIR", self.root.path().join(cache))
            .output()
            .unwrap()
    }

    fn ok(&self, cache: &str, args: &[&str]) -> String {
        let output = self.crucible(cache, args);
        assert!(
            output.status.success(),
            "crucible {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fails(&self, cache: &str, args: &[&str]) -> String {
        let output = self.crucible(cache, args);
        assert!(!output.status.success(), "crucible {args:?} succeeded");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Compile the pack's workflow and return each task's name and prompt.
    fn compile(&self, cache: &str) -> Vec<(String, String)> {
        prompts(&self.ok(cache, &COMPILE))
    }

    fn workflow(&self, source: &str) {
        std::fs::write(self.pack().join("workflow.star"), source).unwrap();
    }
}

const COMPILE: [&str; 4] = ["plan", "compile-workflow", "--file", "workflow.star"];

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn prompts(json: &str) -> Vec<(String, String)> {
    let compiled: serde_json::Value = serde_json::from_str(json).unwrap();
    compiled["task"]
        .as_array()
        .unwrap_or_else(|| panic!("no tasks in {json}"))
        .iter()
        .map(|task| {
            (
                task["name"].as_str().unwrap().to_owned(),
                task["prompt"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pair(name: &str, prompt: &str) -> (String, String) {
    (name.to_owned(), prompt.to_owned())
}

#[test]
fn a_pack_loads_a_locked_library_and_each_tree_reads_its_own_files() {
    let world = World::new();

    let unlocked = world.fails("cache", &COMPILE);
    assert!(
        unlocked.contains("names dependency \"gates\", which crucible.lock does not list"),
        "{unlocked}"
    );
    assert!(unlocked.contains("workflow.star:2:"), "{unlocked}");

    let locked = world.ok("cache", &["deps", "lock"]);
    assert!(locked.starts_with("gates "), "{locked}");
    assert!(locked.contains(" tree1:"), "{locked}");

    let output = world.crucible("cache", &COMPILE);
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("embedded prompt: @gates//prompts/review.md"),
        "{stderr}"
    );
    assert!(
        stderr.contains("embedded prompt: prompts/review.md"),
        "{stderr}"
    );
    assert_eq!(
        world.compile("cache"),
        [
            pair("review-api-v1", "Library review one.\n"),
            pair("own", "Pack review.\n"),
            pair("direct", "Library review one.\n"),
        ]
    );
    let pack_files: Vec<_> = std::fs::read_dir(world.pack())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    assert!(!pack_files.iter().any(|name| name == "gates.star"));
}

#[test]
fn the_lock_pins_the_library_until_it_is_relocked() {
    let world = World::new();
    world.ok("cache", &["deps", "lock"]);
    let pinned = world.compile("cache");

    std::fs::write(
        world.library().join("prompts/review.md"),
        "Library review two.\n",
    )
    .unwrap();
    world.commit("two");
    assert_eq!(world.compile("cache"), pinned);

    let cold = world.fails("cold", &COMPILE);
    assert!(
        cold.contains("is not in the cache; run `crucible deps fetch`"),
        "{cold}"
    );
    assert_eq!(world.ok("cold", &["deps", "fetch"]), "fetched gates\n");
    assert_eq!(world.ok("cold", &["deps", "fetch"]), "");
    assert_eq!(world.compile("cold"), pinned);

    world.ok("pack.deps", &["deps", "fetch"]);
    assert_eq!(world.compile("nowhere"), pinned);

    world.ok("cache", &["deps", "lock"]);
    assert_eq!(
        world.compile("cache")[0],
        pair("review-api-v1", "Library review two.\n")
    );
}

#[test]
fn a_reference_cannot_leave_its_tree_or_name_an_unlocked_library() {
    let world = World::new();
    std::fs::write(world.pack().join("prompts/pack-only.md"), "Pack only.\n").unwrap();
    std::fs::write(
        world.library().join("reach.star"),
        "stolen = prompt_file(\"prompts/pack-only.md\")\n",
    )
    .unwrap();
    std::fs::write(
        world.library().join("climb.star"),
        "load(\"../pack/workflow.star\", \"own\")\n",
    )
    .unwrap();
    world.commit("probes");
    world.ok("cache", &["deps", "lock"]);

    let cases = [
        (
            "load(\"@gates//reach.star\", \"stolen\")\nworkflow([])\n",
            "reading prompt metadata",
            "reach.star:1:",
        ),
        (
            "load(\"@gates//climb.star\", \"own\")\nworkflow([])\n",
            "may not contain `..` or escape the pack",
            "climb.star:1:",
        ),
        (
            "load(\"@gates//../pack/workflow.star\", \"own\")\nworkflow([])\n",
            "may not contain `..` or escape the pack",
            "workflow.star:1:",
        ),
        (
            "load(\"@other//gates.star\", \"review\")\nworkflow([])\n",
            "names dependency \"other\", which crucible.lock does not list",
            "workflow.star:1:",
        ),
        (
            "load(\"@Gates//gates.star\", \"review\")\nworkflow([])\n",
            "dependency name \"Gates\" must start with a lowercase letter",
            "workflow.star:1:",
        ),
        (
            "load(\"@gates\", \"review\")\nworkflow([])\n",
            "must be a non-empty pack-relative path",
            "workflow.star:1:",
        ),
    ];
    for (source, expected, at) in cases {
        world.workflow(source);
        let error = world.fails("cache", &COMPILE);
        assert!(error.contains(expected), "{source}: {error}");
        assert!(error.contains(at), "{source}: {error}");
    }
}

const RUN_MANIFEST: &str = r#"
[repo]
path = "."
[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
[agent]
backend = "command"
agent_cmd = "true"
goal = "run a library task"
[workflow]
type = "playbook"
file = "workflow.star"
"#;

/// The pod's situation: the machine that runs has the pack and the graph a controller compiled,
/// and none of the libraries.
#[test]
fn a_run_given_the_compiled_graph_needs_no_library() {
    let world = World::new();
    std::fs::write(
        world.library().join("steps.star"),
        "def mark(name):\n    return command(name = name, run = \"echo \" + name + \" >> ../ran.log && printf '{}\\n'\")\n",
    )
    .unwrap();
    world.commit("steps");
    let manifest = std::fs::read_to_string(world.pack().join("crucible.toml")).unwrap();
    std::fs::write(
        world.pack().join("crucible.toml"),
        format!("{RUN_MANIFEST}{manifest}"),
    )
    .unwrap();
    world.workflow(
        "load(\"@gates//steps.star\", \"mark\")\nstep = mark(\"from-library\")\nworkflow(type = \"playbook\", tasks = [step], result = step)\n",
    );
    world.ok("cache", &["deps", "lock"]);
    let compiled = world.ok("cache", &COMPILE);
    std::fs::write(world.pack().join("workflow.resolved.json"), compiled).unwrap();

    let run = |cache: &str, resolved: bool| {
        let mut args = vec![
            "plan",
            "run",
            "--manifest",
            "crucible.toml",
            "--max-cost",
            "1",
            "--max-time",
            "5m",
        ];
        if resolved {
            args.extend(["--resolved-workflow", "workflow.resolved.json"]);
        }
        Command::new(env!("CARGO_BIN_EXE_crucible"))
            .args(args)
            .current_dir(world.pack())
            .env("CRUCIBLE_DEPS_DIR", world.root.path().join(cache))
            .env("FORGE_STORAGE_ROOT", world.pack().join("storage"))
            .env("CRUCIBLE_RUN_NAME", "crucible-run-deps")
            .output()
            .unwrap()
    };

    let without = run("no-libraries", false);
    assert!(!without.status.success());
    let stderr = String::from_utf8_lossy(&without.stderr);
    assert!(
        stderr.contains("is not in the cache; run `crucible deps fetch`"),
        "{stderr}"
    );
    assert!(!world.pack().join("ran.log").exists());

    let with = run("no-libraries", true);
    assert!(
        with.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&with.stdout),
        String::from_utf8_lossy(&with.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(world.pack().join("ran.log")).unwrap(),
        "from-library\n"
    );

    std::fs::write(
        world.pack().join("workflow.resolved.json"),
        "{\"type\": \"playbook\", \"file\": \"workflow.star\"}",
    )
    .unwrap();
    let empty = run("no-libraries", true);
    assert!(!empty.status.success());
    let stderr = String::from_utf8_lossy(&empty.stderr);
    assert!(
        stderr.contains("the supplied compiled workflow holds no tasks"),
        "{stderr}"
    );
}
