//! Pinned tools, end to end through the binary: archives served over HTTP, locked by digest,
//! fetched into a cache, and called by name from a command task.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

const MANIFEST: &str = r#"
[repo]
path = "."
[workspace]
dir = "workspace"
setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=c@l -c user.name=c -c commit.gpgsign=false commit -q --allow-empty -m baseline"
[agent]
backend = "command"
agent_cmd = "true"
goal = "call pinned tools"
[workflow]
type = "playbook"
file = "workflow.star"
"#;

const WORKFLOW: &str = r#"
step = command(name = "step", run = "greet > ../out.txt && rawtool >> ../out.txt && printf '{}\n'")
workflow(type = "playbook", tasks = [step], result = step)
"#;

fn host() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux_amd64",
        ("linux", "aarch64") => "linux_arm64",
        ("macos", "x86_64") => "darwin_amd64",
        ("macos", "aarch64") => "darwin_arm64",
        other => panic!("no tool platform for {other:?}"),
    }
}

struct Server {
    child: Child,
    port: u16,
}

impl Server {
    fn start(dir: &Path) -> Self {
        let mut child = Command::new("python3")
            .args(["-u", "-m", "http.server", "0", "--bind", "127.0.0.1"])
            .current_dir(dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line
            .split("port ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|port| port.parse().ok())
            .unwrap_or_else(|| panic!("no port in {line:?}"));
        Server { child, port }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct World {
    root: tempfile::TempDir,
    server: Server,
}

impl World {
    fn new() -> Self {
        Self::with(
            WORKFLOW,
            "[tools.greet]\nurl = \"{base}/greet_{platform}.tar.gz\"\nmember = \"bin/greet\"\n\n[tools.rawtool]\nurl = \"{base}/rawtool-{platform}\"\n",
            |assets| {
                for platform in ["linux_amd64", "linux_arm64", host()] {
                    archive(assets, platform, &format!("greet built for {platform}"));
                }
                script(
                    &assets.join(format!("rawtool-{}", host())),
                    "raw tool, no archive",
                );
            },
        )
    }

    /// A pack running `workflow` with the `tools` tables, `{base}` in them standing for the
    /// server that serves what `fill` puts under `assets`.
    fn with(workflow: &str, tools: &str, fill: impl FnOnce(&Path)) -> Self {
        let root = tempfile::tempdir().unwrap();
        let assets = root.path().join("assets");
        std::fs::create_dir_all(&assets).unwrap();
        fill(&assets);
        let server = Server::start(&assets);

        let pack = root.path().join("pack");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("workflow.star"), workflow).unwrap();
        let base = format!("http://127.0.0.1:{}", server.port);
        std::fs::write(
            pack.join("crucible.toml"),
            format!("{MANIFEST}\n{}", tools.replace("{base}", &base)),
        )
        .unwrap();
        World { root, server }
    }

    fn pack(&self) -> PathBuf {
        self.root.path().join("pack")
    }

    fn assets(&self) -> PathBuf {
        self.root.path().join("assets")
    }

    fn crucible(&self, cache: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_crucible"))
            .args(args)
            .current_dir(self.pack())
            .env("CRUCIBLE_DEPS_DIR", self.root.path().join(cache))
            .env("FORGE_STORAGE_ROOT", self.pack().join("storage"))
            .env("CRUCIBLE_RUN_NAME", "crucible-run-tools")
            .output()
            .unwrap()
    }

    fn ok(&self, cache: &str, args: &[&str]) -> String {
        let output = self.crucible(cache, args);
        assert!(
            output.status.success(),
            "crucible {args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fails(&self, cache: &str, args: &[&str]) -> String {
        let output = self.crucible(cache, args);
        assert!(!output.status.success(), "crucible {args:?} succeeded");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn out(&self) -> String {
        std::fs::read_to_string(self.pack().join("out.txt")).unwrap()
    }
}

const RUN: [&str; 8] = [
    "plan",
    "run",
    "--manifest",
    "crucible.toml",
    "--max-cost",
    "1",
    "--max-time",
    "5m",
];

fn script(path: &Path, says: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\necho '{says}'\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `greet_<platform>.tar.gz` under `assets`, holding `bin/greet` and a decoy beside it.
fn archive(assets: &Path, platform: &str, says: &str) {
    let stage = assets.join(format!(".stage-{platform}"));
    std::fs::create_dir_all(stage.join("bin")).unwrap();
    script(&stage.join("bin/greet"), says);
    std::fs::write(stage.join("README"), "not the tool\n").unwrap();
    let status = Command::new("tar")
        .arg("-czf")
        .arg(assets.join(format!("greet_{platform}.tar.gz")))
        .arg("-C")
        .arg(&stage)
        .args(["README", "bin/greet"])
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::remove_dir_all(stage).unwrap();
}

#[test]
fn a_command_task_calls_locked_tools_by_name() {
    let world = World::new();

    let locked = world.ok("cache", &["deps", "lock"]);
    let lines: Vec<&str> = locked.lines().collect();
    let greets = lines.iter().filter(|l| l.starts_with("greet ")).count();
    assert_eq!(
        greets,
        if host().starts_with("linux") { 2 } else { 3 },
        "{locked}"
    );
    assert!(lines.iter().all(|l| l.contains(" sha256:")), "{locked}");
    let raw: Vec<&&str> = lines.iter().filter(|l| l.starts_with("rawtool ")).collect();
    assert_eq!(raw.len(), 1, "{locked}");
    assert!(raw[0].contains(host()), "{locked}");

    world.ok("cache", &RUN);
    assert_eq!(
        world.out(),
        format!("greet built for {}\nraw tool, no archive\n", host())
    );
}

#[test]
fn a_cold_cache_is_filled_from_the_lock_and_a_changed_download_is_refused() {
    let world = World::new();
    world.ok("cache", &["deps", "lock"]);

    let cold = world.fails("cold", &RUN);
    assert!(
        cold.contains("is not in the cache; run `crucible deps fetch`"),
        "{cold}"
    );
    assert!(!world.pack().join("out.txt").exists());

    assert_eq!(
        world.ok("cold", &["deps", "fetch"]),
        "fetched greet\nfetched rawtool\n"
    );
    assert_eq!(world.ok("cold", &["deps", "fetch"]), "");
    world.ok("cold", &RUN);
    assert!(world.out().starts_with("greet built for "));

    archive(&world.assets(), host(), "something else entirely");
    let changed = world.fails("changed", &["deps", "fetch"]);
    assert!(changed.contains("tool \"greet\""), "{changed}");
    assert!(changed.contains("pins sha256:"), "{changed}");
    let unfetched = world.fails("changed", &RUN);
    assert!(unfetched.contains("is not in the cache"), "{unfetched}");
}

#[test]
fn a_missing_download_and_a_missing_member_are_refused() {
    let world = World::new();
    let manifest = std::fs::read_to_string(world.pack().join("crucible.toml")).unwrap();

    std::fs::write(
        world.pack().join("crucible.toml"),
        manifest.replace("member = \"bin/greet\"", "member = \"bin/absent\""),
    )
    .unwrap();
    let error = world.fails("cache", &["deps", "lock"]);
    assert!(error.contains("has no file \"bin/absent\""), "{error}");

    std::fs::write(
        world.pack().join("crucible.toml"),
        manifest.replace("rawtool-{platform}", "nothing-{platform}"),
    )
    .unwrap();
    let error = world.fails("cache", &["deps", "lock"]);
    assert!(
        error.contains("tool \"rawtool\": no platform it names has a download"),
        "{error}"
    );

    std::fs::write(
        world.pack().join("crucible.toml"),
        format!("{manifest}platforms = [\"linux_amd64\", \"plan9_mips\"]\n"),
    )
    .unwrap();
    let error = world.fails("cache", &["deps", "lock"]);
    assert!(error.contains("unknown platform \"plan9_mips\""), "{error}");
    let _ = world.server.port;
}

/// A run pod fetches its tools from the controller by digest, with its pod token, and from
/// nowhere else.
#[test]
fn a_pod_fetches_its_tools_from_the_controller_by_digest() {
    let world = World::new();
    world.ok("cache", &["deps", "lock"]);

    let served = world.assets().join("api/tools");
    std::fs::create_dir_all(&served).unwrap();
    for asset in [
        format!("greet_{}.tar.gz", host()),
        format!("rawtool-{}", host()),
    ] {
        let origin = world.assets().join(asset);
        let digest = crucible_contract::content_digest(&std::fs::read(&origin).unwrap());
        std::fs::rename(origin, served.join(digest.strip_prefix("sha256:").unwrap())).unwrap();
    }
    let fetch = |controller: Option<String>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crucible"));
        command
            .args(["deps", "fetch", "--from-controller"])
            .current_dir(world.pack())
            .env("CRUCIBLE_DEPS_DIR", world.root.path().join("pod"))
            .env_remove("CRUCIBLE_CONTROLLER_URL")
            .env("CRUCIBLE_POD_TOKEN", "crr_id_secret");
        if let Some(controller) = controller {
            command.env("CRUCIBLE_CONTROLLER_URL", controller);
        }
        command.output().unwrap()
    };

    let nowhere = fetch(None);
    assert!(!nowhere.status.success());
    let error = String::from_utf8_lossy(&nowhere.stderr);
    assert!(
        error.contains("needs CRUCIBLE_CONTROLLER_URL in the environment"),
        "{error}"
    );
    assert!(world.fails("pod", &["deps", "fetch"]).contains("not found"));

    let fetched = fetch(Some(format!("http://127.0.0.1:{}/", world.server.port)));
    assert!(
        fetched.status.success(),
        "{}",
        String::from_utf8_lossy(&fetched.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&fetched.stdout),
        "fetched greet\nfetched rawtool\n"
    );
    world.ok("pod", &RUN);
    assert_eq!(
        world.out(),
        format!("greet built for {}\nraw tool, no archive\n", host())
    );
}

const SDK_WORKFLOW: &str = r#"
params = {
    "profile": {"type": "string", "required": True, "doc": "whose tickets"},
    "manifest": {"type": "bool", "default": True, "doc": "whether stage writes its file"},
}

stage = command(
    name = "stage",
    run = "echo 'run 7' > manifest.txt && printf '{\"members\": [\"A-1\", \"A-2\"]}\n'",
    emits = {"members": "list"},
    emits_files = ["manifest.txt"] if param("manifest") else [],
)
report = command(
    name = "report",
    run = "report",
    depends_on = [stage],
    emits = {"task": "string", "tickets": "integer"},
    emits_files = ["REPORT.md"],
)
check = command(
    name = "check",
    run = "cp \"$CRUCIBLE_INPUTS_FILE\" ../seen.json && cp inputs/report/REPORT.md ../staged.md && printf '{}\n'",
    depends_on = [report],
)
workflow(type = "playbook", tasks = [stage, report, check], result = check)
"#;

/// The task SDK's demo binary, built into the directory this test's `crucible` was.
fn sdk_demo() -> PathBuf {
    let profile = Path::new(env!("CARGO_BIN_EXE_crucible")).parent().unwrap();
    let mut build = Command::new(env!("CARGO"));
    build.args([
        "build",
        "-p",
        "crucible-task",
        "--bin",
        "crucible-task-demo",
    ]);
    if profile.ends_with("release") {
        build.arg("--release");
    }
    assert!(build.status().unwrap().success());
    profile.join("crucible-task-demo")
}

fn sdk_world() -> World {
    World::with(
        SDK_WORKFLOW,
        "[tools.report]\nurl = \"{base}/report-{platform}\"\n",
        |assets| {
            std::fs::copy(sdk_demo(), assets.join(format!("report-{}", host()))).unwrap();
        },
    )
}

#[test]
fn a_task_written_with_the_sdk_runs_as_a_pinned_tool() {
    let world = sdk_world();
    world.ok("cache", &["deps", "lock"]);

    let mut run = RUN.to_vec();
    run.extend(["--param", "profile=llm-d"]);
    world.ok("cache", &run);

    let seen: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(world.pack().join("seen.json")).unwrap())
            .unwrap();
    assert_eq!(
        seen["report"],
        serde_json::json!({"task": "report", "tickets": 2})
    );
    assert_eq!(
        std::fs::read_to_string(world.pack().join("staged.md")).unwrap(),
        "llm-d: A-1, A-2 (run 7)\n"
    );
}

#[test]
fn an_sdk_task_that_fails_settles_failed_with_its_reason() {
    let world = sdk_world();
    world.ok("cache", &["deps", "lock"]);

    let mut run = RUN.to_vec();
    run.extend(["--param", "profile=llm-d", "--param", "manifest=false"]);
    let output = world.crucible("cache", &run);
    assert!(!output.status.success());
    let table = String::from_utf8_lossy(&output.stdout);
    let report = table
        .lines()
        .find(|line| line.trim_start().starts_with("report "))
        .unwrap();
    assert!(report.contains(" fail "), "{table}");
    assert!(
        report.contains(
            "(exit 1: task \"stage\" declared no file manifest.txt; add it to that task's emits_files"
        ),
        "{table}"
    );
    assert!(table.contains("check                blocked"), "{table}");
    assert!(!world.pack().join("seen.json").exists());
}
