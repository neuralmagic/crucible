//! `crucible check`: validate a manifest without spending a loop iteration (no agent turn), parse it
//! (the parser's `deny_unknown_fields` errors are the loud-typo path), resolve every file it references,
//! set up the workspace and run `measure_cmd` exactly once to prove the measure contract
//! (docs/crucible-contract.md §3), and warn when the gate is reachable by the agent's own edits (frozen-
//! judge wall, applied here as a lint rather than a block).

use crate::command_judge::Direction;
use crate::manifest::{self, AgentCfg, CompositeManifest, Manifest, WorldCfg};
use crate::selftest::{self, SelftestReport};
use anyhow::Result;
use std::path::{Path, PathBuf};

/// The result of a check: empty `findings` means the manifest is good to run. `warnings` never fail
/// the check (exit 0) but are worth a human's attention (e.g. an editable gate).
///
/// `selftest` and `measure_stderr_tail` are structured evidence for the refine loop; the
/// `crucible check` CLI only renders `findings`/`warnings`, so they're invisible there.
#[derive(Default, Debug)]
pub struct CheckOutcome {
    pub findings: Vec<String>,
    pub warnings: Vec<String>,
    /// The self-test's structured verdict, present whenever the self-test actually ran (pass or
    /// fail). `None` when there's no `[judge.selftest]` or an earlier finding short-circuited it.
    pub selftest: Option<SelftestReport>,
    /// The tail of `measure_cmd`'s stderr from the contract probe (bounded), so a `Contract`
    /// failure can quote why the gate blew up.
    pub measure_stderr_tail: Vec<String>,
}

/// How many trailing stderr lines the contract probe keeps for evidence.
const MEASURE_STDERR_TAIL_LINES: usize = 50;

impl CheckOutcome {
    pub fn ok(&self) -> bool {
        self.findings.is_empty()
    }

    fn fail(msg: impl Into<String>) -> Self {
        Self {
            findings: vec![msg.into()],
            ..Self::default()
        }
    }
}

/// Check `manifest_path`: composite or single, picked the same way [`crate::run::dispatch`] does.
pub fn run(manifest_path: &Path) -> Result<CheckOutcome> {
    if manifest::is_composite(manifest_path) {
        check_composite(manifest_path)
    } else {
        check_single(manifest_path)
    }
}

/// The parse-only slice of the check: parse the manifest (composite or single, the same
/// `deny_unknown_fields` + validate path a run takes) and stop. No referenced-file resolution,
/// shipped packs may inject files that deliberately never land in the repo (`_controls/` answer
/// keys), and no workspace setup, measure probe, or self-test. Needs nothing but the manifest
/// itself, so CI sweeps it over every shipped domain manifest.
pub fn run_parse_only(manifest_path: &Path) -> Result<CheckOutcome> {
    let result = if manifest::is_composite(manifest_path) {
        CompositeManifest::load(manifest_path)
            .map(|_| ())
            .map_err(|e| format!("composite manifest parse failed: {e:#}"))
    } else {
        Manifest::load(manifest_path)
            .map(|_| ())
            .map_err(|e| format!("manifest parse failed: {e:#}"))
    };
    Ok(match result {
        Ok(()) => CheckOutcome::default(),
        Err(msg) => CheckOutcome::fail(msg),
    })
}

/// Validate a deploy profile's spoke-cluster wiring: the named `[measure].cluster` resolves
/// against the merged fleet file, its secret name is non-empty, and no bastion block is selected
/// (schema-accepted, not implemented yet). With `live`, also assert the deployment's isolation
/// claim: the sandbox SA (sandbox pods run as the loop SA, see `kubernetes_sandbox_env`) must NOT
/// be able to read the spoke kubeconfig Secret in the loop namespace; an unreachable API server
/// degrades that probe to a warning, an "allowed" verdict is a finding.
pub fn check_profile(
    profile_path: &Path,
    clusters_override: Option<&Path>,
    live: bool,
) -> CheckOutcome {
    let mut out = CheckOutcome::default();
    let profile = match crate::deploy::profile::DeployProfile::load_with_fleet(
        profile_path,
        clusters_override,
    ) {
        Ok(p) => p,
        Err(e) => return CheckOutcome::fail(format!("deploy profile failed to load: {e:#}")),
    };
    let (name, entry) = match profile.measure_cluster() {
        Ok(Some(pair)) => pair,
        Ok(None) => return out,
        Err(e) => return CheckOutcome::fail(format!("cluster wiring: {e:#}")),
    };
    if entry.bastion.is_some() {
        out.findings.push(format!(
            "[clusters.{name}] has a bastion block, but the SSH tunnel is not implemented yet; \
             remove the bastion block or target a routable spoke"
        ));
    }
    if !live {
        return out;
    }
    let ns = &profile.cluster.loop_namespace;
    let sa = &profile.cluster.service_account;
    let secret = &entry.kubeconfig_secret;
    // The probe uses the ambient client; name what that points at, so a laptop run can't silently
    // validate the wrong cluster.
    eprintln!(
        "[crucible check] live sandbox-SA probe via {}",
        forge::kube::ambient_context_description()
    );
    match forge::kube::sa_can_read_secret(ns, sa, secret) {
        Ok(true) => out.findings.push(format!(
            "sandbox SA {ns}/{sa} CAN read Secret {secret} in the loop namespace — the spoke \
             kubeconfig mounts on the loop pod and must be unreachable from the sandbox; tighten \
             the RBAC (no unpinned secrets get/list/watch for this SA)"
        )),
        Ok(false) => {}
        Err(e) => out.warnings.push(format!(
            "could not verify the sandbox-SA-cannot-read-Secrets assertion for {ns}/{sa} \
             (cluster unreachable?): {e:#}"
        )),
    }
    out
}

fn manifest_dir_of(manifest_path: &Path) -> PathBuf {
    manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn check_single(manifest_path: &Path) -> Result<CheckOutcome> {
    let m = match Manifest::load_frozen(manifest_path) {
        Ok(m) => m,
        Err(e) => return Ok(CheckOutcome::fail(format!("manifest parse failed: {e:#}"))),
    };
    let manifest_dir = manifest_dir_of(manifest_path);
    let workspace = manifest_dir.join(&m.workspace.dir);

    let mut out = CheckOutcome::default();
    check_referenced_files(&m, &manifest_dir, &mut out);
    if !out.ok() {
        // Missing goal/prompt/inject files means the run would fail before ever measuring,
        // no point spending a workspace setup + measure_cmd exec on top.
        return Ok(out);
    }

    if !workspace.exists() {
        if let Err(e) = crate::run::manifest_setup(&m, &manifest_dir, &workspace) {
            out.findings.push(format!("workspace setup failed: {e:#}"));
            return Ok(out);
        }
        for (src, dst, _frozen) in m.resolved_injects(&manifest_dir, &workspace) {
            if let Err(e) = manifest::apply_inject(&src, &dst) {
                out.findings
                    .push(format!("applying [workspace].inject failed: {e:#}"));
                return Ok(out);
            }
        }
    }
    if let Err(e) = crucible_vcs::vcs::ensure_repo(&workspace) {
        out.findings
            .push(format!("workspace is not a usable git repo: {e:#}"));
        return Ok(out);
    }

    // The contract's frozen-inject rule: re-established before EVERY scored measurement, so the
    // probe must apply them too, a pre-existing workspace may predate an inject added since.
    for (src, dst) in frozen_injects(&m, &manifest_dir, &workspace) {
        if let Err(e) = manifest::apply_inject(&src, &dst) {
            out.findings
                .push(format!("applying frozen [workspace].inject failed: {e:#}"));
            return Ok(out);
        }
    }
    let Some(judge) = m.judge.as_ref() else {
        // Task lane: no gate to probe, lint, or self-test; require a goal instead.
        out.warnings.push(
            "task mode: no [judge] — every completed turn is kept and published unscored"
                .to_string(),
        );
        let has_goal = m
            .agent
            .goal
            .as_deref()
            .is_some_and(|g| !g.trim().is_empty())
            || m.agent.goal_file.is_some();
        if !has_goal {
            out.findings
                .push("task manifest has no [agent].goal or goal_file".to_string());
        }
        if let Some(f) = unfenced_rig_toolbox_finding(&m.world, &m.agent) {
            out.findings.push(f);
        }
        return Ok(out);
    };
    check_measure_once(&judge.measure_cmd, &workspace, &mut out);
    if let Some(w) = agent_editable_gate_warning(&judge.measure_cmd, &m, &manifest_dir, &workspace)
    {
        out.warnings.push(w);
    }
    if let Some(f) = unfenced_rig_toolbox_finding(&m.world, &m.agent) {
        out.findings.push(f);
    }
    if out.ok() {
        match &judge.selftest {
            Some(cfg) => {
                let result = (|| -> Result<SelftestReport> {
                    let frozen = frozen_injects(&m, &manifest_dir, &workspace);
                    let world = m.build_world(workspace.clone());
                    let judge = m.build_judge(workspace.clone(), frozen)?;
                    let direction = m.direction()?;
                    selftest::run(world.as_ref(), judge.as_ref(), &workspace, cfg, direction)
                })();
                fold_selftest_result(result, &mut out);
            }
            None => out.warnings.push(no_selftest_warning()),
        }
    }
    Ok(out)
}

/// `[[workspace.inject]]` entries with `frozen = true`, resolved to absolute `(src, dst)`, the
/// same set [`crate::command_judge::CommandJudge`] re-establishes before every scored measure.
fn frozen_injects(m: &Manifest, manifest_dir: &Path, workspace: &Path) -> Vec<(PathBuf, PathBuf)> {
    m.resolved_injects(manifest_dir, workspace)
        .into_iter()
        .filter(|(_, _, frozen)| *frozen)
        .map(|(src, dst, _)| (src, dst))
        .collect()
}

fn no_selftest_warning() -> String {
    "no [judge.selftest] declared — the gate hasn't been proven to discriminate a known-good \
     from a known-bad config"
        .to_string()
}

/// Fold a self-test run into `out`: a run error is a finding (like any other check failure); a
/// completed run folds pass/fail + the two scores in via [`fold_selftest`].
fn fold_selftest_result(result: Result<SelftestReport>, out: &mut CheckOutcome) {
    match result {
        Ok(report) => {
            fold_selftest(&report, out);
            out.selftest = Some(report);
        }
        Err(e) => out
            .findings
            .push(format!("gate self-test failed to run: {e:#}")),
    }
}

fn fold_selftest(report: &SelftestReport, out: &mut CheckOutcome) {
    let dir_word = match report.direction {
        Direction::Lower => "lower wins",
        Direction::Higher => "higher wins",
    };
    let summary = format!(
        "gate self-test ({dir_word}, {} run(s)): good={:.4} bad={:.4}",
        report.runs, report.good.mean_score, report.bad.mean_score
    );
    if report.passed {
        out.warnings.push(format!("{summary} — discriminates"));
    } else {
        out.findings.push(format!(
            "{summary} — does not discriminate (good must be strictly better than bad, and both readings valid)"
        ));
    }
}

fn check_composite(manifest_path: &Path) -> Result<CheckOutcome> {
    let m = match CompositeManifest::load_frozen(manifest_path) {
        Ok(m) => m,
        Err(e) => {
            return Ok(CheckOutcome::fail(format!(
                "composite manifest parse failed: {e:#}"
            )));
        }
    };
    let manifest_dir = manifest_dir_of(manifest_path);

    let mut out = CheckOutcome::default();
    if let Some(f) = &m.agent.goal_file {
        check_file_exists("[agent].goal_file", &manifest_dir, f, &mut out);
    }
    if let Some(f) = &m.agent.method_prompt {
        check_file_exists("[agent].method_prompt", &manifest_dir, f, &mut out);
    }
    let components = match m.resolve_components(&manifest_dir) {
        Ok(c) => c,
        Err(e) => {
            out.findings
                .push(format!("resolving [[component]] entries failed: {e:#}"));
            return Ok(out);
        }
    };
    if !out.ok() {
        return Ok(out);
    }

    for c in &components {
        if !c.workspace.exists() {
            let repo = &c.manifest.repo;
            let Some(src) = repo.url.clone().or_else(|| repo.path.clone()) else {
                out.findings
                    .push(format!("component `{}` [repo] needs url or path", c.name));
                continue;
            };
            if let Err(e) = crate::run::clone_repo(&src, repo.git_ref.as_deref(), &c.workspace) {
                out.findings
                    .push(format!("cloning component `{}` failed: {e:#}", c.name));
                continue;
            }
        }
        if let Err(e) = crucible_vcs::vcs::ensure_repo(&c.workspace) {
            out.findings.push(format!(
                "component `{}` workspace is not a usable git repo: {e:#}",
                c.name
            ));
        }
    }
    if !out.ok() {
        return Ok(out);
    }

    let base = m.base_dir(&manifest_dir);
    check_measure_once(&m.judge.measure_cmd, &base, &mut out);
    if let Some(f) = unfenced_rig_toolbox_finding(&m.world, &m.agent) {
        out.findings.push(f);
    }
    if out.ok() {
        match &m.judge.selftest {
            Some(cfg) => {
                let result = (|| -> Result<SelftestReport> {
                    let world = m.build_world(&manifest_dir)?;
                    let judge = m.build_judge(&manifest_dir)?;
                    let direction = m.direction()?;
                    selftest::run(world.as_ref(), judge.as_ref(), &base, cfg, direction)
                })();
                fold_selftest_result(result, &mut out);
            }
            None => out.warnings.push(no_selftest_warning()),
        }
    }
    Ok(out)
}

fn check_file_exists(label: &str, manifest_dir: &Path, rel: &str, out: &mut CheckOutcome) {
    let p = manifest_dir.join(rel);
    if !p.exists() {
        out.findings.push(format!(
            "{label} `{rel}` does not resolve: {} not found",
            p.display()
        ));
    }
}

/// Everything a manifest points at by relative path must resolve, or the run fails before ever
/// reaching the agent: `goal_file`, `method_prompt`, `toolbox_dir`, and every
/// `[[workspace.inject]].src`.
/// The `[[workspace.inject]].src` half of [`check_referenced_files`], as a hard error rather than a
/// finding. `deploy render` calls this: a dangling inject renders a perfectly valid-looking pack
/// whose missing file only surfaces inside the sandbox, mid-turn, as whatever the agent was supposed
/// to read not being there.
pub fn ensure_injects_resolve(
    m: &Manifest,
    manifest_dir: &Path,
) -> Result<(), manifest::ManifestError> {
    let missing: Vec<String> = m
        .workspace
        .inject
        .iter()
        .filter(|inject| !manifest_dir.join(&inject.src).exists())
        .map(|inject| format!("  {} -> {}", inject.src, inject.dst))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(manifest::ManifestError::InjectSrcMissing {
        manifest_dir: manifest_dir.to_path_buf(),
        missing,
    })
}

fn check_referenced_files(m: &Manifest, manifest_dir: &Path, out: &mut CheckOutcome) {
    if let Some(f) = &m.agent.goal_file {
        check_file_exists("[agent].goal_file", manifest_dir, f, out);
    }
    if let Some(f) = &m.agent.method_prompt {
        check_file_exists("[agent].method_prompt", manifest_dir, f, out);
    }
    if let Some(d) = &m.agent.toolbox_dir {
        let p = manifest_dir.join(d);
        if !p.is_dir() {
            out.findings.push(format!(
                "[agent].toolbox_dir `{d}` does not resolve to a directory: {}",
                p.display()
            ));
        }
    }
    for inject in &m.workspace.inject {
        let p = manifest_dir.join(&inject.src);
        if !p.exists() {
            out.findings.push(format!(
                "[[workspace.inject]].src `{}` does not resolve: {} not found",
                inject.src,
                p.display()
            ));
        }
    }
}

/// The measure contract, checked directly (not via [`crate::command_judge::CommandJudge`]) so a
/// nonzero exit and a missing/invalid contract line are reported as distinct findings instead of
/// collapsing into one opaque `valid: false`.
fn check_measure_once(measure_cmd: &str, workspace: &Path, out: &mut CheckOutcome) {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(measure_cmd)
        .current_dir(workspace)
        .output();
    let output = match output {
        Ok(o) => o,
        Err(e) => {
            out.findings.push(format!(
                "running measure_cmd `{measure_cmd}` failed to spawn: {e:#}"
            ));
            return;
        }
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let tail: Vec<String> = stderr
        .lines()
        .rev()
        .take(MEASURE_STDERR_TAIL_LINES)
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if !tail.is_empty() {
        out.measure_stderr_tail = tail;
    }
    if !output.status.success() {
        out.findings.push(format!(
            "measure_cmd `{measure_cmd}` exited nonzero: {}",
            output.status
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(line) = stdout
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with('{'))
    else {
        out.findings.push(format!(
            "measure_cmd `{measure_cmd}` printed no JSON line (last stdout line must start with `{{`)"
        ));
        return;
    };
    let parsed: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            out.findings.push(format!(
                "measure_cmd `{measure_cmd}` last line isn't valid JSON: {e} ({line})"
            ));
            return;
        }
    };
    match parsed.get("valid") {
        Some(serde_json::Value::Bool(valid)) => {
            if *valid && !matches!(parsed.get("score"), Some(v) if v.is_number()) {
                out.warnings.push(format!(
                    "measure_cmd `{measure_cmd}` reports valid:true but `score` isn't a number \
                     ({line}) — the engine treats a missing score as invalid"
                ));
            }
        }
        Some(_) => out.findings.push(format!(
            "measure_cmd `{measure_cmd}` contract line's `valid` must be a bool ({line})"
        )),
        None => out.findings.push(format!(
            "measure_cmd `{measure_cmd}` contract line is missing required field `valid` ({line})"
        )),
    }
}

/// The frozen-judge lint: if `measure_cmd` names a file that lives inside the workspace and no
/// `frozen = true` `[[workspace.inject]]` re-establishes it before every scored measure, the agent's
/// own edits can reach and game the gate.
fn agent_editable_gate_warning(
    measure_cmd: &str,
    m: &Manifest,
    manifest_dir: &Path,
    workspace: &Path,
) -> Option<String> {
    let frozen_dsts: Vec<PathBuf> = m
        .resolved_injects(manifest_dir, workspace)
        .into_iter()
        .filter(|(_, _, frozen)| *frozen)
        .map(|(_, dst, _)| dst)
        .collect();
    for tok in measure_cmd.split_whitespace() {
        let candidate = workspace.join(tok);
        if !candidate.is_file() {
            continue;
        }
        let covered = frozen_dsts.iter().any(|d| paths_match(d, &candidate));
        if !covered {
            return Some(format!(
                "[judge].measure_cmd references `{tok}`, a file inside the workspace with no \
                 matching frozen [[workspace.inject]] — the agent can edit its own gate (see \
                 docs/adr/0001-adaptive-harness.md)"
            ));
        }
    }
    None
}

/// A domain with `[world].snapshot_cmd`/`restore_cmd` has a live, externally-mutable deployment on top of
/// the git tree, the evaluation surface the gate is scored against. A loop toolbox is a flat
/// directory of skills copied in wholesale unless the manifest names which ones are setup-only (see
/// [`AgentCfg::toolbox_exclude`]); an empty list on a domain that clearly has a deployment worth protecting
/// means every skill, including any that can move that deployment instead of acting on the candidate under
/// test, reaches the loop agent. A hard failure, not a warning: this is the exact reward-hack shape
/// where the agent reaches an axis the gate assumed was nailed down.
fn unfenced_rig_toolbox_finding(world: &WorldCfg, agent: &AgentCfg) -> Option<String> {
    let has_live_rig = world.snapshot_cmd.is_some() || world.restore_cmd.is_some();
    if has_live_rig && agent.toolbox_dir.is_some() && agent.toolbox_exclude.is_empty() {
        return Some(
            "[world] snapshot_cmd/restore_cmd is set (a live rig on top of the git tree) but \
             [agent].toolbox_exclude is empty — every skill in the loop toolbox is reachable, \
             including any that mutate the rig/workload instead of the candidate under test; \
             name the setup-only skills in toolbox_exclude"
                .to_string(),
        );
    }
    None
}

fn paths_match(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A dangling inject must stop `deploy render`, not ride along: the missing file would only
    /// show up inside the sandbox, mid-turn, as content the agent was told to read but cannot.
    #[test]
    fn ensure_injects_resolve_rejects_a_dangling_src() {
        let dir = tempdir("inject-dangling");
        fs::write(dir.join("present.txt"), "x").expect("write");
        let manifest_path = dir.join("crucible.toml");
        fs::write(
            &manifest_path,
            r#"
[repo]
path = "."
[judge]
measure_cmd = "./measure.nu"
direction = "higher"
objective = "value"
[agent]
backend = "command"
agent_cmd = "./bump.nu"
goal = "raise it"
[[workspace.inject]]
src = "present.txt"
dst = "present.txt"
[[workspace.inject]]
src = "traces/median_block.txt"
dst = "traces/median_block.txt"
"#,
        )
        .expect("write manifest");
        let m = Manifest::load(&manifest_path).expect("manifest parses");

        let err = ensure_injects_resolve(&m, &dir).expect_err("a missing src must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("traces/median_block.txt"),
            "the error must name the missing file: {msg}"
        );
        assert!(
            !msg.contains("present.txt"),
            "a resolvable src must not be reported: {msg}"
        );

        fs::create_dir_all(dir.join("traces")).expect("mkdir");
        fs::write(dir.join("traces/median_block.txt"), "y").expect("write");
        ensure_injects_resolve(&m, &dir).expect("all srcs present now");

        let _ = fs::remove_dir_all(&dir);
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "crucible-check-test-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("mkdir tmp");
        dir
    }

    fn write_exec(path: &Path, content: &str) {
        fs::write(path, content).expect("write script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
    }

    /// A minimal counter-shaped domain: repo path=".", explicit setup_cmd copying just the
    /// measure script in, so the workspace never needs a real git remote.
    fn scaffold_domain(dir: &Path, measure_body: &str) {
        write_exec(&dir.join("measure.sh"), measure_body);
        let manifest = r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && cp measure.sh workspace/ && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=t@t -c user.name=t commit -qm baseline"
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "goal"
            [judge]
            measure_cmd = "./measure.sh"
            direction = "higher"
            objective = "score"
            "#;
        fs::write(dir.join(MANIFEST), manifest).expect("write manifest");
    }

    const MANIFEST: &str = "crucible.toml";

    #[test]
    fn check_applies_frozen_injects_to_a_preexisting_workspace() {
        // Regression (live-caught on llm-d-router#850): the measure probe ran raw, so a frozen
        // inject added AFTER the workspace was first created never landed and the gate 127'd.
        let dir = tempdir("frozen-inject-late");
        write_exec(
            &dir.join("measure.sh"),
            "#!/bin/sh\necho '{\"valid\": true, \"score\": 1.0}'\n",
        );
        // Workspace pre-exists WITHOUT the gate script; the manifest injects it frozen and no
        // setup_cmd will run (the dir already exists).
        let ws = dir.join("workspace");
        fs::create_dir_all(&ws).expect("mkdir ws");
        crucible_vcs::vcs::ensure_repo(&ws).expect("init ws repo");
        let manifest = r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            [[workspace.inject]]
            src = "measure.sh"
            dst = "measure.sh"
            frozen = true
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "goal"
            [judge]
            measure_cmd = "./measure.sh"
            direction = "higher"
            objective = "score"
            "#;
        fs::write(dir.join(MANIFEST), manifest).expect("write manifest");

        let out = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(
            out.findings.is_empty(),
            "probe must apply frozen injects first: {:?}",
            out.findings
        );
        assert!(ws.join("measure.sh").exists(), "inject landed");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_passes_on_a_good_manifest() {
        let dir = tempdir("good");
        scaffold_domain(
            &dir,
            "#!/bin/sh\necho '{\"valid\": true, \"score\": 1.0}'\n",
        );
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(outcome.ok(), "findings: {:?}", outcome.findings);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("no matching frozen")),
            "measure.sh is an unprotected in-workspace gate: {:?}",
            outcome.warnings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_only_skips_the_measure_probe() {
        // A measure_cmd that would fail the full check must not run: parse-only stops at parse +
        // referenced-file resolution.
        let dir = tempdir("parse-only-good");
        scaffold_domain(&dir, "#!/bin/sh\nexit 1\n");
        let outcome = run_parse_only(&dir.join(MANIFEST)).expect("check runs");
        assert!(outcome.ok(), "findings: {:?}", outcome.findings);
        assert!(
            !dir.join("workspace").exists(),
            "parse-only must not set up the workspace"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_only_still_fails_on_bad_parse_but_not_missing_files() {
        let dir = tempdir("parse-only-bad");
        fs::write(
            dir.join(MANIFEST),
            "[repo]\npath = \".\"\ntypo_field = 3\n[judge]\nmeasure_cmd = \"m\"\ndirection = \"higher\"\n[agent]\nbackend=\"command\"\nagent_cmd=\"x\"\ngoal=\"g\"\n",
        )
        .unwrap();
        let outcome = run_parse_only(&dir.join(MANIFEST)).expect("check runs");
        assert!(!outcome.ok());
        assert!(outcome.findings[0].contains("parse failed"));

        // Missing referenced files are NOT parse-only findings: shipped packs may inject files
        // that deliberately never land in the repo (_controls/ answer keys).
        fs::write(
            dir.join(MANIFEST),
            "[repo]\npath = \".\"\n[judge]\nmeasure_cmd = \"m\"\ndirection = \"higher\"\n[agent]\nbackend=\"command\"\nagent_cmd=\"x\"\ngoal_file=\"missing.md\"\n",
        )
        .unwrap();
        let outcome = run_parse_only(&dir.join(MANIFEST)).expect("check runs");
        assert!(outcome.ok(), "findings: {:?}", outcome.findings);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_fails_on_bad_manifest_parse() {
        let dir = tempdir("bad-parse");
        fs::write(
            dir.join(MANIFEST),
            "[repo]\npath = \".\"\ntypo_field = 3\n[judge]\nmeasure_cmd = \"m\"\ndirection = \"higher\"\n[agent]\nbackend=\"command\"\nagent_cmd=\"x\"\ngoal=\"g\"\n",
        )
        .unwrap();
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(!outcome.ok());
        assert!(outcome.findings[0].contains("parse failed"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn task_manifest_passes_without_a_measure_probe() {
        let dir = tempdir("task-ok");
        fs::write(
            dir.join(MANIFEST),
            "[repo]\npath = \".\"\n[workspace]\ndir = \"workspace\"\nsetup_cmd = \"mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=t@t -c user.name=t commit -q --allow-empty -m baseline\"\n[agent]\nbackend=\"command\"\nagent_cmd=\"true\"\ngoal=\"do the chore\"\n",
        )
        .unwrap();
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(outcome.ok(), "findings: {:?}", outcome.findings);
        assert!(
            outcome.warnings.iter().any(|w| w.contains("task mode")),
            "the task-mode notice is loud: {:?}",
            outcome.warnings
        );
        assert!(
            !outcome
                .warnings
                .iter()
                .any(|w| w.contains("[judge.selftest]")),
            "no selftest nag on a task manifest: {:?}",
            outcome.warnings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn task_manifest_without_a_goal_is_a_finding() {
        let dir = tempdir("task-no-goal");
        fs::write(
            dir.join(MANIFEST),
            "[repo]\npath = \".\"\n[workspace]\ndir = \"workspace\"\nsetup_cmd = \"mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=t@t -c user.name=t commit -q --allow-empty -m baseline\"\n[agent]\nbackend=\"command\"\nagent_cmd=\"true\"\n",
        )
        .unwrap();
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(!outcome.ok());
        assert!(
            outcome
                .findings
                .iter()
                .any(|f| f.contains("no [agent].goal")),
            "{:?}",
            outcome.findings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_fails_when_measure_cmd_misbehaves() {
        let dir = tempdir("bad-measure");
        scaffold_domain(&dir, "#!/bin/sh\necho 'not json'\nexit 1\n");
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(!outcome.ok());
        assert!(
            outcome
                .findings
                .iter()
                .any(|f| f.contains("exited nonzero"))
        );
        assert!(outcome.findings.iter().any(|f| f.contains("no JSON line")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_fails_on_missing_goal_file() {
        let dir = tempdir("missing-goal");
        write_exec(
            &dir.join("measure.sh"),
            "#!/bin/sh\necho '{\"valid\":true,\"score\":1}'\n",
        );
        fs::write(
            dir.join(MANIFEST),
            r#"
            [repo]
            path = "."
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal_file = "does-not-exist.md"
            [judge]
            measure_cmd = "./measure.sh"
            direction = "higher"
            "#,
        )
        .unwrap();
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(!outcome.ok());
        assert!(
            outcome
                .findings
                .iter()
                .any(|f| f.contains("goal_file") && f.contains("does not resolve"))
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn frozen_inject_silences_the_gate_warning() {
        let dir = tempdir("frozen");
        write_exec(
            &dir.join("measure.sh"),
            "#!/bin/sh\necho '{\"valid\": true, \"score\": 1.0}'\n",
        );
        let manifest = r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && git -C workspace init -q && git -C workspace -c user.email=t@t -c user.name=t commit -qm baseline --allow-empty"
            [[workspace.inject]]
            src = "measure.sh"
            dst = "measure.sh"
            frozen = true
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "g"
            [judge]
            measure_cmd = "./measure.sh"
            direction = "higher"
            "#;
        fs::write(dir.join(MANIFEST), manifest).unwrap();
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(outcome.ok(), "findings: {:?}", outcome.findings);
        assert!(
            !outcome
                .warnings
                .iter()
                .any(|w| w.contains("no matching frozen")),
            "a frozen inject covering the gate file must silence the warning: {:?}",
            outcome.warnings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A domain whose score is `value.txt`'s content in the workspace, so `good_cmd`/`bad_cmd`
    /// can stage a real discriminating (or non-discriminating) gate.
    fn scaffold_value_domain(dir: &Path, good_value: &str, bad_value: &str) {
        write_exec(
            &dir.join("measure.sh"),
            "#!/bin/sh\nv=$(cat value.txt)\necho \"{\\\"valid\\\": true, \\\"score\\\": $v}\"\n",
        );
        let manifest = format!(
            r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && cp measure.sh workspace/ && echo 0 > workspace/value.txt && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=t@t -c user.name=t commit -qm baseline"
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "g"
            [judge]
            measure_cmd = "./measure.sh"
            direction = "higher"
            objective = "score"
            [judge.selftest]
            good_cmd = "echo {good_value} > value.txt && git add value.txt && git -c user.email=t@t -c user.name=t commit -qm good"
            bad_cmd = "echo {bad_value} > value.txt && git add value.txt && git -c user.email=t@t -c user.name=t commit -qm bad"
            "#
        );
        fs::write(dir.join(MANIFEST), manifest).expect("write manifest");
    }

    #[test]
    fn check_absent_selftest_warns_not_fails() {
        let dir = tempdir("no-selftest");
        scaffold_domain(
            &dir,
            "#!/bin/sh\necho '{\"valid\": true, \"score\": 1.0}'\n",
        );
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(outcome.ok(), "no selftest is a warning, not a failure");
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("no [judge.selftest] declared")),
            "{:?}",
            outcome.warnings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_runs_declared_selftest_and_passes_when_discriminating() {
        let dir = tempdir("selftest-discriminates");
        scaffold_value_domain(&dir, "100", "10");
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(outcome.ok(), "findings: {:?}", outcome.findings);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|w| w.contains("gate self-test") && w.contains("discriminates")),
            "{:?}",
            outcome.warnings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_fails_when_declared_selftest_does_not_discriminate() {
        let dir = tempdir("selftest-fails");
        scaffold_value_domain(&dir, "10", "10");
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(!outcome.ok(), "equal good/bad must fail check");
        assert!(
            outcome
                .findings
                .iter()
                .any(|f| f.contains("gate self-test") && f.contains("does not discriminate")),
            "{:?}",
            outcome.findings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A live-deployment fixture: a live system under test (`[world]` snapshot/restore) plus a loop toolbox with a
    /// setup-only skill of exactly the kind that must never reach the loop agent.
    fn scaffold_rig_domain(dir: &Path, toolbox_exclude: &str) {
        write_exec(
            &dir.join("measure.sh"),
            "#!/bin/sh\necho '{\"valid\": true, \"score\": 1.0}'\n",
        );
        fs::create_dir_all(dir.join("skills/rig-config")).expect("mkdir skill");
        fs::create_dir_all(dir.join("skills/apply-live-config")).expect("mkdir skill");
        let manifest = format!(
            r#"
            [repo]
            path = "."
            [workspace]
            dir = "workspace"
            setup_cmd = "mkdir -p workspace && cp measure.sh workspace/ && git -C workspace init -q && git -C workspace add -A && git -C workspace -c user.email=t@t -c user.name=t commit -qm baseline"
            [agent]
            backend = "command"
            agent_cmd = "true"
            goal = "goal"
            toolbox_dir = "skills"
            {toolbox_exclude}
            [judge]
            measure_cmd = "./measure.sh"
            direction = "higher"
            objective = "score"
            [world]
            snapshot_cmd = "echo dG9r"
            restore_cmd = "true"
            "#
        );
        fs::write(dir.join(MANIFEST), manifest).expect("write manifest");
    }

    #[test]
    fn check_fails_when_a_live_rig_has_no_toolbox_exclusions() {
        let dir = tempdir("rig-unfenced");
        scaffold_rig_domain(&dir, "");
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(
            !outcome.ok(),
            "an unfenced live-rig toolbox must fail check"
        );
        assert!(
            outcome
                .findings
                .iter()
                .any(|f| f.contains("toolbox_exclude") && f.contains("empty")),
            "{:?}",
            outcome.findings
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn check_passes_when_the_live_rig_toolbox_is_fenced() {
        let dir = tempdir("rig-fenced");
        scaffold_rig_domain(&dir, r#"toolbox_exclude = ["rig-config"]"#);
        let outcome = run(&dir.join(MANIFEST)).expect("check runs");
        assert!(
            !outcome
                .findings
                .iter()
                .any(|f| f.contains("toolbox_exclude") && f.contains("empty")),
            "an exclusion list, even a partial one, silences the unfenced-rig finding: {:?}",
            outcome.findings
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
