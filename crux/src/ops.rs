//! The operations, once.
//!
//! Every one of these is both a CLI subcommand and an MCP tool, over this code, printing the same
//! bytes. That is the point: an agent can loop over `crux issues` in a shell script when
//! that is cheaper than a tool call per issue, and get exactly what the tool would have returned.

#![allow(clippy::disallowed_macros)]

use crate::client::{AdoptBody, Client, DraftSave, encode};
use crate::dto;
use crate::render;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

/// `--json` renders the controller's payload verbatim instead of the compact text. Pretty-printed,
/// because the caller asked for JSON in order to read or pipe it, not to save four bytes.
fn json(v: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(v)?)
}

/// Rows `issues` prints when the caller names no limit. Applied client-side: `GET /api/issues`
/// takes no `limit` of its own.
pub const ISSUES_LIMIT_DEFAULT: i64 = 50;

pub async fn issues(
    c: &Client,
    kind: Option<&str>,
    status: Option<&str>,
    limit: Option<i64>,
    as_json: bool,
) -> Result<String> {
    let status = status
        .map(dto::IssueStatus::from_str)
        .transpose()?
        .map(dto::IssueStatus::wire);
    let cap = usize::try_from(limit.unwrap_or(ISSUES_LIMIT_DEFAULT).max(0)).unwrap_or(usize::MAX);
    if as_json {
        let mut raw: Value = c.issues(kind, status).await?;
        if let Some(rows) = raw.as_array_mut() {
            rows.truncate(cap);
        }
        return json(&raw);
    }
    let mut list: Vec<dto::Issue> = c.issues(kind, status).await?;
    let total = list.len();
    list.truncate(cap);
    Ok(render::issues(&list, total))
}

pub async fn issue(c: &Client, key: &str, body_max: usize, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.issue::<Value>(key).await?);
    }
    let detail: dto::IssueDetail = c.issue(key).await?;
    Ok(render::issue(&detail, body_max))
}

/// The controller has no issue filter on `/api/turns`, so it is applied here. Doing it client-side
/// is honest about the cost (the full page comes back either way) and keeps the filter available
/// today rather than after a controller change.
pub async fn turns(
    c: &Client,
    issue_key: Option<&str>,
    kind: Option<&str>,
    state: Option<&str>,
    as_json: bool,
) -> Result<String> {
    let mut raw: Value = c.turns(kind, state).await?;
    if let (Some(key), Some(rows)) = (issue_key, raw.as_array_mut()) {
        rows.retain(|t| t.get("issue_key").and_then(Value::as_str) == Some(key));
    }
    if as_json {
        return json(&raw);
    }
    let list: Vec<dto::Turn> = serde_json::from_value(raw)?;
    Ok(render::turns(&list))
}

pub async fn turn(c: &Client, pod: &str, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.turn::<Value>(pod).await?);
    }
    let t: dto::Turn = c.turn(pod).await?;
    Ok(render::turn(&t))
}

pub async fn graph(c: &Client, run_id: &str, mermaid: bool, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.graph::<Value>(run_id).await?);
    }
    let g: dto::RunGraph = c.graph(run_id).await?;
    Ok(if mermaid {
        render::graph_mermaid(&g)
    } else {
        render::graph(run_id, &g)
    })
}

/// What build the controller is running, and whether it is the commit the caller expected.
///
/// A mismatch is an error, not a line of prose: the whole point is to be a gate a script can stand
/// on, and a zero exit beside the word STALE would be read by exactly nobody.
pub async fn deployed(c: &Client, expect: Option<&str>, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.version::<Value>().await?);
    }
    let v: dto::Version = c.version().await?;
    let report = render::deployed(c.base(), &v, expect);
    match expect {
        Some(want) if !render::same_commit(&v.git_sha, want) => {
            anyhow::bail!(
                "{report}\nthe controller is not running {}; it is running {}",
                want.trim(),
                if v.git_sha.is_empty() {
                    "unknown"
                } else {
                    &v.git_sha
                }
            )
        }
        _ => Ok(report),
    }
}

pub async fn run_log(
    c: &Client,
    run_id: &str,
    cursor: usize,
    limit: Option<usize>,
    as_json: bool,
) -> Result<String> {
    if as_json {
        return json(&c.run_log::<Value>(run_id).await?);
    }
    let log: dto::RunLog = c.run_log(run_id).await?;
    let limit = limit
        .unwrap_or(render::LOG_WINDOW_LINES)
        .clamp(1, render::LOG_WINDOW_LINES);
    Ok(render::run_log(run_id, &log, cursor, limit))
}

/// The index after the last printed line inside a window whose head the controller dropped, or
/// `None` when that line is gone too.
fn resume_after(all: &[&str], last_line: Option<&str>) -> Option<usize> {
    let last = last_line?;
    all.iter().rposition(|x| *x == last).map(|pos| pos + 1)
}

/// Follow a run's log until the run leaves `running`, printing each new line as it appears.
///
/// The controller serves the whole log on every read, tail-capped, so this polls and prints
/// only what is past `cursor`. When the cap drops the head, line numbers stop meaning anything;
/// the last printed line is then used to find where to resume, and if it is gone too, the
/// whole window is printed with a marker rather than silently skipped.
pub async fn run_log_follow(
    c: &Client,
    run_id: &str,
    cursor: usize,
    interval: Duration,
    out: &mut (impl std::io::Write + ?Sized),
) -> Result<String> {
    let mut follow = Follow {
        seen: cursor,
        last_line: None,
        headed: false,
    };
    loop {
        let log: dto::RunLog = c.run_log(run_id).await?;
        follow.emit(run_id, &log, out)?;
        let detail: dto::RunDetail = c.run(run_id).await?;
        if !matches!(detail.run.status.as_str(), "running" | "pending") {
            return Ok(format!(
                "-- run {} is {}, {} lines seen\n",
                run_id, detail.run.status, follow.seen
            ));
        }
        tokio::time::sleep(interval).await;
    }
}

/// What one `run-log -f` has printed so far: the line to resume from, the last line printed (to
/// find that point again once the controller drops the head), and whether the header is out.
struct Follow {
    seen: usize,
    last_line: Option<String>,
    headed: bool,
}

impl Follow {
    /// Print what is new in this window.
    fn emit(
        &mut self,
        run_id: &str,
        log: &dto::RunLog,
        out: &mut (impl std::io::Write + ?Sized),
    ) -> Result<()> {
        let Some(text) = log.text.as_deref().filter(|t| !t.is_empty()) else {
            return Ok(());
        };
        if !self.headed {
            writeln!(
                out,
                "run {run_id} [{}] following from line {}",
                log.dispatch, self.seen
            )?;
            self.headed = true;
        }
        let all: Vec<&str> = text.lines().collect();
        let start = if log.truncated {
            match resume_after(&all, self.last_line.as_deref()) {
                Some(pos) => pos,
                None => {
                    if self.last_line.is_some() {
                        writeln!(
                            out,
                            "-- head dropped by the controller's cap; resuming from what it kept"
                        )?;
                    }
                    0
                }
            }
        } else {
            self.seen.min(all.len())
        };
        for line in &all[start..] {
            writeln!(out, "{line}")?;
            self.last_line = Some((*line).to_string());
        }
        self.seen = if log.truncated {
            self.seen + all.len().saturating_sub(start)
        } else {
            all.len()
        };
        out.flush()?;
        Ok(())
    }
}

pub async fn run_files(c: &Client, run_id: &str, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.run_files::<Value>(run_id).await?);
    }
    let listing: dto::RunFiles = c.run_files(run_id).await?;
    Ok(render::run_files(run_id, &listing))
}

/// Fetch one captured file. With `out`, write it there and say so; without, print it, which is only
/// honest for text — a file that is not UTF-8 says so and names `--out` rather than spraying bytes
/// at a terminal.
pub async fn run_file(c: &Client, run_id: &str, key: &str, out: Option<&Path>) -> Result<String> {
    let bytes = c.run_file(run_id, key).await?;
    match out {
        Some(path) => {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(path, &bytes).with_context(|| format!("writing {}", path.display()))?;
            Ok(format!(
                "wrote {} bytes to {}\n",
                bytes.len(),
                path.display()
            ))
        }
        None => match String::from_utf8(bytes) {
            Ok(text) => Ok(text),
            Err(e) => Ok(format!(
                "{key} is {} bytes of non-text content; fetch it with --out <path>\n",
                e.as_bytes().len()
            )),
        },
    }
}

pub async fn contracts(c: &Client, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.contracts::<Value>().await?);
    }
    let list: dto::BrokerContracts = c.contracts().await?;
    Ok(render::contracts(&list))
}

pub async fn approvals(c: &Client, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.approvals::<Value>().await?);
    }
    let d: dto::Approvals = c.approvals().await?;
    Ok(render::approvals(&d))
}

/// The approval PR and pack digest for one issue, read out of its scopes.
///
/// `GET /api/approvals` only lists what is currently awaiting a human, so an issue whose approval has
/// already been walked through would simply be absent from it. This reads the issue instead, which
/// answers "what is the approval, and did anyone use it" for any issue at any point in its life.
pub async fn approval_detail(c: &Client, key: &str, as_json: bool) -> Result<String> {
    let raw: Value = c.issue(key).await?;
    if as_json {
        return json(&raw);
    }
    let detail: dto::IssueDetail = serde_json::from_value(raw)?;
    Ok(render::approval_detail(key, &detail))
}

// --- mutations -------------------------------------------------------------
//
// Each returns the controller's ack with the `actor` it recorded. The acting identity is not a
// courtesy: a request that reached the controller without identity headers books the change to
// `anonymous`, and the ack is the only place that becomes visible before someone goes looking in
// the event log a week later.

pub async fn park(c: &Client, key: &str, reason: &str, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.park::<Value>(key, reason).await?);
    }
    let ack: dto::OverrideAck = c.park(key, reason).await?;
    Ok(render::ack(
        "park",
        &ack.key,
        ack.actor.as_deref(),
        &format!("reason: {reason}"),
    ))
}

pub async fn unpark(c: &Client, key: &str, reason: Option<&str>, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.unpark::<Value>(key, reason).await?);
    }
    let ack: dto::OverrideAck = c.unpark(key, reason).await?;
    Ok(render::ack(
        "unpark",
        &ack.key,
        ack.actor.as_deref(),
        &reason.map(|r| format!("reason: {r}")).unwrap_or_default(),
    ))
}

pub async fn bump(
    c: &Client,
    key: &str,
    priority: i64,
    reason: Option<&str>,
    as_json: bool,
) -> Result<String> {
    if as_json {
        return json(&c.bump::<Value>(key, priority, reason).await?);
    }
    let ack: dto::OverrideAck = c.bump(key, priority, reason).await?;
    Ok(render::ack(
        "bump",
        &ack.key,
        ack.actor.as_deref(),
        &format!("priority: {}", ack.priority.unwrap_or(priority)),
    ))
}

pub async fn redispatch(
    c: &Client,
    key: &str,
    justification: &str,
    as_json: bool,
) -> Result<String> {
    if as_json {
        return json(&c.redispatch::<Value>(key, justification).await?);
    }
    let ack: dto::OverrideAck = c.redispatch(key, justification).await?;
    Ok(render::ack(
        "redispatch",
        &ack.key,
        ack.actor.as_deref(),
        &format!("justification: {justification}"),
    ))
}

/// `POST /api/reconcile` takes no key: it wakes the daemon's next full pass. Repeated calls
/// coalesce, and it returns before the pass runs.
pub async fn reconcile(c: &Client, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.reconcile::<Value>().await?);
    }
    let ack: dto::ReconcileAck = c.reconcile().await?;
    Ok(render::ack(
        "reconcile",
        "(all issues)",
        ack.actor.as_deref(),
        "the daemon will pick this up on its next pass; it does not block",
    ))
}

pub async fn approve(c: &Client, key: &str, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.approve::<Value>(key).await?);
    }
    let ack: dto::ApproveAck = c.approve(key).await?;
    Ok(render::ack(
        "approve",
        &ack.key,
        Some(ack.approved_by.as_str()),
        &format!("scope {} approved at {}", ack.scope_id, ack.approved_at),
    ))
}

pub async fn adopt(c: &Client, body: &AdoptBody, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.adopt::<Value>(body).await?);
    }
    let ack: dto::ScenarioAck = c.adopt(body).await?;
    let mut detail = format!(
        "tier: {}\nrepos: {}",
        ack.tier,
        ack.affected_repos.join(", ")
    );
    // Echo back what the controller stored, not what was sent: it trims and validates both, and a
    // silently-normalized ref is worth seeing before a turn clones the wrong branch.
    if let Some(r) = &ack.git_ref {
        detail.push_str(&format!("\ngit_ref: {r}"));
    }
    if let Some(cc) = &ack.codegen_contract {
        detail.push_str(&format!("\ncodegen_contract: {cc}"));
    }
    Ok(render::ack(
        "adopt",
        &ack.key,
        ack.actor.as_deref(),
        &detail,
    ))
}

/// Who the controller thinks you are, and how you got there. The first command to run when a
/// mutation 403s.
pub async fn whoami(c: &Client) -> Result<String> {
    let w = c.whoami().await?;
    Ok(render::whoami(&c.endpoint(), &c.credential(), &w))
}

// --- pack authoring (imports and drafts: the surface an agent shares with a human) -------------

/// Propose an import and hand back the link a human opens on it.
///
/// `id`/`description` are the registry naming the proposer suggests; they ride the preview URL as
/// query values so the review page opens on them instead of asking the admin to retype what the
/// proposal already decided. Registering is still the admin's click.
pub async fn playbook_import(
    c: &Client,
    repo: &str,
    git_ref: Option<&str>,
    path: Option<&str>,
    id: Option<&str>,
    description: Option<&str>,
    as_json: bool,
) -> Result<String> {
    let mut raw: Value = c.import_pack(repo, git_ref, path).await?;
    let import: dto::PackImport = serde_json::from_value(raw.clone())?;
    let url = c.web_url(&import_preview_path(&import.id, id, description));
    if as_json {
        // The URL is this client's fact, not the controller's, and it is the whole point of the
        // call — so it is added rather than left out of the raw payload.
        if let Some(obj) = raw.as_object_mut() {
            obj.insert("preview_url".to_string(), Value::String(url));
        }
        return json(&raw);
    }
    Ok(render::pack_import(&import, &url))
}

pub async fn draft_files(
    c: &Client,
    draft_id: &str,
    version: Option<i64>,
    as_json: bool,
) -> Result<String> {
    if as_json {
        return json(&c.draft_files::<Value>(draft_id, version).await?);
    }
    let files: dto::DraftFiles = c.draft_files(draft_id, version).await?;
    Ok(render::draft_files(draft_id, &files))
}

/// Save a draft version against the base it was edited from. A base another save overtook comes
/// back as the refusal, not as an error: nothing was written, and the merge is the caller's next
/// move.
pub async fn draft_create(
    c: &Client,
    draft_id: &str,
    description: &str,
    template: Option<&str>,
    as_json: bool,
) -> Result<String> {
    let mut raw: Value = c.create_draft(draft_id, description, template).await?;
    let saved: dto::DraftCompile = serde_json::from_value(raw.clone())?;
    let url = studio_url(c, draft_id);
    if as_json {
        if let Some(obj) = raw.as_object_mut() {
            obj.insert("studio_url".to_string(), Value::String(url));
        }
        return json(&raw);
    }
    Ok(render::draft_created(draft_id, &saved, &url))
}

pub async fn draft_save(
    c: &Client,
    draft_id: &str,
    base_version: i64,
    files: &std::collections::BTreeMap<String, String>,
    as_json: bool,
) -> Result<String> {
    match c.save_draft(draft_id, files, base_version).await? {
        DraftSave::Saved(mut raw) => {
            let saved: dto::DraftCompile = serde_json::from_value(raw.clone())?;
            let url = studio_url(c, draft_id);
            if as_json {
                if let Some(obj) = raw.as_object_mut() {
                    obj.insert("studio_url".to_string(), Value::String(url));
                }
                return json(&raw);
            }
            Ok(render::draft_saved(draft_id, &saved, &url))
        }
        DraftSave::Stale(stale) => {
            if as_json {
                return json(&serde_json::to_value(serde_json::json!({
                    "error": stale.error,
                    "base_version": stale.base_version,
                    "current_version": stale.current_version,
                    "saved_by": stale.saved_by,
                    "saved_at": stale.saved_at,
                }))?);
            }
            Ok(render::stale_base(draft_id, &stale))
        }
    }
}

pub async fn playbook_caps(c: &Client, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.playbook_caps::<Value>().await?);
    }
    let caps: dto::PlaybookCaps = c.playbook_caps().await?;
    Ok(render::playbook_caps(&caps))
}

pub async fn secrets(c: &Client, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.secrets::<Value>().await?);
    }
    let list: Vec<dto::Secret> = c.secrets().await?;
    Ok(render::secrets(&list))
}

/// What a binding says: which secret, at which scope, reaching the run as what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretBind<'a> {
    pub secret_id: &'a str,
    /// `repo`, `playbook`, or `domain`.
    pub scope_kind: &'a str,
    pub scope_id: &'a str,
    /// `env` or `file`.
    pub projection_kind: &'a str,
    /// The variable name, or the absolute path.
    pub projection: &'a str,
    /// The manifest's `[[secret]] name` this satisfies. None means the secret's own name.
    pub declared_name: Option<&'a str>,
}

pub async fn secret_bind(c: &Client, bind: SecretBind<'_>, as_json: bool) -> Result<String> {
    let mut body = serde_json::json!({
        "scope_kind": bind.scope_kind,
        "scope_id": bind.scope_id,
        "projection_kind": bind.projection_kind,
        "projection": bind.projection,
    });
    put_trimmed(&mut body, "declared_name", bind.declared_name);
    if as_json {
        return json(&c.bind_secret::<Value>(bind.secret_id, &body).await?);
    }
    let bound: dto::SecretBinding = c.bind_secret(bind.secret_id, &body).await?;
    Ok(render::secret_bound(&bound))
}

pub async fn draft_preview(
    c: &Client,
    draft_id: &str,
    version: Option<i64>,
    as_json: bool,
) -> Result<String> {
    if as_json {
        return json(&c.draft_preview::<Value>(draft_id, version).await?);
    }
    let compiled: dto::DraftCompile = c.draft_preview(draft_id, version).await?;
    Ok(render::draft_preview(
        draft_id,
        &compiled,
        &studio_url(c, draft_id),
    ))
}

/// What a draft test-fire carries. `params` are strings because the endpoint takes a string map,
/// not the arbitrary JSON a registered launch accepts. An optional field left blank is omitted
/// from the body, so the controller's own default applies.
#[derive(Debug, Clone, Default)]
pub struct DraftLaunch {
    pub params: BTreeMap<String, String>,
    pub max_cost: f64,
    pub max_time: String,
    /// The digest the values were filled against: pass what `crucible_draft_files` or the preview
    /// returned and a save that landed since is refused instead of launching against a schema
    /// that moved.
    pub schema_digest: Option<String>,
    pub dispatch_target: Option<String>,
    /// The registered inference provider the run's agent talks to, replacing the pack manifest's
    /// `[agent]` harness; `model` is the model to ask it for and needs a provider.
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Test-fire a draft. The values are the newest save's stored schema's, and a draft launch is held
/// to the same admin caps a registered one is.
pub async fn draft_launch(
    c: &Client,
    draft_id: &str,
    launch: DraftLaunch,
    as_json: bool,
) -> Result<String> {
    let mut body = serde_json::json!({
        "params": launch.params,
        "max_cost": launch.max_cost,
        "max_time": launch.max_time,
    });
    put_trimmed(&mut body, "schema_digest", launch.schema_digest.as_deref());
    put_trimmed(
        &mut body,
        "dispatch_target",
        launch.dispatch_target.as_deref(),
    );
    put_trimmed(&mut body, "provider", launch.provider.as_deref());
    put_trimmed(&mut body, "model", launch.model.as_deref());
    if as_json {
        return json(&c.launch_draft::<Value>(draft_id, &body).await?);
    }
    let ack: dto::LaunchAck = c.launch_draft(draft_id, &body).await?;
    let url = c.web_url(&format!("/playbook-runs/{}", encode(&ack.key)));
    Ok(render::launched(&ack, &url))
}

/// Delete a draft outright. Admin only, and gone is gone: every version, and the id is free
/// for a new `draft-create`.
pub async fn draft_delete(c: &Client, draft_id: &str) -> Result<String> {
    c.delete_draft(draft_id).await?;
    Ok(format!("deleted draft {draft_id}\n"))
}

/// Register a draft's newest compiling version as a playbook, with no review.
pub async fn draft_publish(
    c: &Client,
    draft_id: &str,
    playbook: Option<&str>,
    as_json: bool,
) -> Result<String> {
    if as_json {
        return json(&c.publish_draft::<Value>(draft_id, playbook).await?);
    }
    let ack: dto::PublishAck = c.publish_draft(draft_id, playbook).await?;
    Ok(render::draft_published(draft_id, &ack))
}

/// Export a draft as a PR against `repo`. Already graduated comes back as the controller's own
/// refusal, which carries the open PR.
pub async fn draft_graduate(
    c: &Client,
    draft_id: &str,
    repo: &str,
    path: Option<&str>,
    as_json: bool,
) -> Result<String> {
    if as_json {
        return json(&c.graduate_draft::<Value>(draft_id, repo, path).await?);
    }
    let ack: dto::GraduateAck = c.graduate_draft(draft_id, repo, path).await?;
    Ok(render::draft_graduated(draft_id, &ack))
}

/// A pack path, checked before it becomes a filesystem path. A pack is a relative tree; anything
/// absolute, rooted or climbing out of it is refused, in both directions — the controller's map is
/// no more trusted than the directory a person hands back.
fn pack_relative(path: &str) -> Result<PathBuf> {
    if path.trim().is_empty() {
        bail!("a pack file needs a path");
    }
    let relative = Path::new(path);
    let mut out = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::ParentDir
            | Component::CurDir
            | Component::RootDir
            | Component::Prefix(_) => {
                bail!("{path:?} is not a relative path inside the pack")
            }
        }
    }
    if out.as_os_str().is_empty() {
        bail!("{path:?} is not a relative path inside the pack");
    }
    Ok(out)
}

/// Write a draft version into a directory, so a person edits the pack in their own editor. The
/// version it wrote is the base their next push must carry.
pub async fn draft_pull(
    c: &Client,
    draft_id: &str,
    dir: &Path,
    version: Option<i64>,
    as_json: bool,
) -> Result<String> {
    let pulled: dto::DraftFiles = c.draft_files(draft_id, version).await?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut written = Vec::new();
    for (path, content) in &pulled.files {
        let target = dir.join(pack_relative(path)?);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&target, content)
            .with_context(|| format!("writing {}", target.display()))?;
        written.push(path.clone());
    }
    if as_json {
        return json(&serde_json::json!({
            "draft_id": draft_id,
            "version": pulled.version,
            "dir": dir.to_string_lossy(),
            "files": written,
        }));
    }
    Ok(render::draft_pulled(
        draft_id,
        pulled.version,
        dir,
        &written,
    ))
}

/// Save a directory back as the next draft version. The whole tree is the save, so a file deleted
/// on disk is a file deleted from the pack, and `base_version` is what protects the other editor.
pub async fn draft_push(
    c: &Client,
    draft_id: &str,
    dir: &Path,
    base_version: i64,
    as_json: bool,
) -> Result<String> {
    let files = read_pack_dir(dir)?;
    if files.is_empty() {
        bail!(
            "{} holds no files; a save with none would empty the pack",
            dir.display()
        );
    }
    draft_save(c, draft_id, base_version, &files, as_json).await
}

/// A pack directory as the `{path: content}` map a save is. Symlinks and non-text files are
/// refused rather than followed or dropped: a save is the whole tree, and a file this cannot carry
/// is a file the save would delete.
fn read_pack_dir(dir: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(relative) = stack.pop() {
        let here = dir.join(&relative);
        let entries =
            std::fs::read_dir(&here).with_context(|| format!("reading {}", here.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading an entry of {}", here.display()))?;
            let name = entry.file_name();
            let child = relative.join(&name);
            let kind = entry
                .file_type()
                .with_context(|| format!("stat {}", entry.path().display()))?;
            if kind.is_symlink() {
                bail!(
                    "{} is a symlink; a pack holds regular files only",
                    child.display()
                );
            }
            if kind.is_dir() {
                stack.push(child);
                continue;
            }
            let path = child.to_string_lossy().to_string();
            pack_relative(&path)?;
            let bytes = std::fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            let text = String::from_utf8(bytes)
                .with_context(|| format!("{path} is not text, so it cannot be saved as a draft"))?;
            files.insert(path, text);
        }
    }
    Ok(files)
}

fn import_preview_path(import_id: &str, id: Option<&str>, description: Option<&str>) -> String {
    let query: Vec<String> = [("id", id), ("description", description)]
        .into_iter()
        .filter_map(|(name, value)| {
            let v = value.map(str::trim).filter(|v| !v.is_empty())?;
            Some(format!("{name}={}", encode(v)))
        })
        .collect();
    let suffix = if query.is_empty() {
        String::new()
    } else {
        format!("?{}", query.join("&"))
    };
    format!("/playbooks/import/{}{suffix}", encode(import_id))
}

/// The studio page a draft is edited on.
fn studio_url(c: &Client, draft_id: &str) -> String {
    c.web_url(&format!("/playbooks/drafts/{}", encode(draft_id)))
}

pub async fn playbooks(c: &Client, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.playbooks::<Value>().await?);
    }
    let list: Vec<dto::Playbook> = c.playbooks().await?;
    Ok(render::playbooks(&list))
}

/// The parameter schema, always as JSON: it IS a JSON Schema, and an agent about to fill one in
/// needs the types and the enums, not a prose summary of them.
pub async fn playbook_schema(c: &Client, id: &str) -> Result<String> {
    json(&c.playbook_schema::<Value>(id).await?)
}

/// What a registered launch carries. `params` is the object the playbook's schema accepts. An
/// optional field left blank is omitted from the body, so the controller's own default applies.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    pub params: Value,
    pub max_cost: f64,
    pub max_time: String,
    pub dispatch_target: Option<String>,
    /// The registered inference provider the run's agent talks to, replacing the pack manifest's
    /// `[agent]` harness; `model` is the model to ask it for and needs a provider.
    pub provider: Option<String>,
    pub model: Option<String>,
}

fn put_trimmed(body: &mut Value, field: &str, value: Option<&str>) {
    if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
        body[field] = Value::String(v.to_string());
    }
}

/// Launch a playbook. The caps are the ceilings the launch is authorized against, and the
/// controller refuses anything above the admin's own.
pub async fn launch(c: &Client, id: &str, launch: Launch, as_json: bool) -> Result<String> {
    let mut body = serde_json::json!({
        "params": launch.params,
        "max_cost": launch.max_cost,
        "max_time": launch.max_time,
    });
    put_trimmed(
        &mut body,
        "dispatch_target",
        launch.dispatch_target.as_deref(),
    );
    put_trimmed(&mut body, "provider", launch.provider.as_deref());
    put_trimmed(&mut body, "model", launch.model.as_deref());
    if as_json {
        return json(&c.launch::<Value>(id, &body).await?);
    }
    let ack: dto::LaunchAck = c.launch(id, &body).await?;
    let url = c.web_url(&format!("/playbook-runs/{}", encode(&ack.key)));
    Ok(render::launched(&ack, &url))
}

pub async fn playbook_runs(
    c: &Client,
    status: Option<&str>,
    playbook: Option<&str>,
    as_json: bool,
) -> Result<String> {
    if as_json {
        return json(&c.playbook_runs::<Value>(status, playbook).await?);
    }
    let list: Vec<dto::PlaybookRun> = c.playbook_runs(status, playbook).await?;
    Ok(render::playbook_runs(&list))
}

/// One launch, whole. Always JSON: its `params` are arbitrary and the refusals are long, and both
/// are exactly what a caller reads this for.
pub async fn playbook_run(c: &Client, key: &str) -> Result<String> {
    json(&c.playbook_run::<Value>(key).await?)
}

pub async fn runs(
    c: &Client,
    status: Option<&str>,
    repo: Option<&str>,
    dispatch_target: Option<&str>,
    limit: Option<i64>,
    as_json: bool,
) -> Result<String> {
    if as_json {
        return json(
            &c.runs::<Value>(status, repo, dispatch_target, limit)
                .await?,
        );
    }
    let list: Vec<dto::RunRow> = c.runs(status, repo, dispatch_target, limit).await?;
    Ok(render::runs(&list))
}

pub async fn run(c: &Client, run_id: &str) -> Result<String> {
    json(&c.run::<Value>(run_id).await?)
}

pub async fn schedules(c: &Client, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.schedules::<Value>().await?);
    }
    let list: Vec<dto::Schedule> = c.schedules().await?;
    Ok(render::schedules(&list))
}

pub async fn watches(c: &Client, as_json: bool) -> Result<String> {
    if as_json {
        return json(&c.watches::<Value>().await?);
    }
    let list: Vec<dto::Watch> = c.watches().await?;
    Ok(render::watches(&list))
}

pub async fn watch(c: &Client, id: &str) -> Result<String> {
    json(&c.watch::<Value>(id).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The proposer's naming rides the link so the admin opens the review page on what the agent
    /// decided, rather than retyping it. An id it never supplied must not appear as an empty one.
    #[test]
    fn a_preview_link_carries_the_proposed_naming_when_there_is_any() {
        assert_eq!(
            import_preview_path("0192f4a1", None, None),
            "/playbooks/import/0192f4a1"
        );
        assert_eq!(
            import_preview_path("0192f4a1", Some("  "), Some("")),
            "/playbooks/import/0192f4a1"
        );
        assert_eq!(
            import_preview_path(
                "0192f4a1",
                Some("calibrate"),
                Some("EPP calibration & scoring")
            ),
            "/playbooks/import/0192f4a1?id=calibrate&description=EPP%20calibration%20%26%20scoring"
        );
    }

    /// Both directions of the mapping refuse a path that leaves the pack. The controller is not
    /// the only writer of these maps, and a directory handed back is not one at all.
    #[test]
    fn a_path_that_leaves_the_pack_is_refused_in_both_directions() {
        assert_eq!(
            pack_relative("skills/read/SKILL.md").expect("relative"),
            PathBuf::from("skills/read/SKILL.md")
        );
        for hostile in [
            "/etc/passwd",
            "../escape",
            "skills/../../escape",
            "./workflow.star",
            "",
            "   ",
        ] {
            assert!(
                pack_relative(hostile).is_err(),
                "{hostile:?} must not become a filesystem path"
            );
        }
    }

    /// A pack directory is the whole tree: nested files come with it, a file deleted on disk is
    /// absent from the save, and a symlink is refused rather than followed.
    #[test]
    fn a_pack_directory_reads_back_as_the_whole_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("skills/read")).expect("mkdir");
        std::fs::write(root.join("crucible.toml"), "[workflow]\n").expect("write");
        std::fs::write(root.join("skills/read/SKILL.md"), "read it\n").expect("write");
        let files = read_pack_dir(root).expect("read");
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec![
                "crucible.toml".to_string(),
                "skills/read/SKILL.md".to_string()
            ]
        );
        assert_eq!(files["skills/read/SKILL.md"], "read it\n");

        std::os::unix::fs::symlink("/etc/passwd", root.join("sneaky")).expect("symlink");
        let refused = read_pack_dir(root).expect_err("a symlink is not a pack file");
        assert!(format!("{refused:#}").contains("symlink"), "{refused:#}");
    }

    /// The round trip the subcommands are: pull a version into a directory, edit it there, and
    /// push it back on the base the pull printed. A base another save overtook refuses the push
    /// and writes nothing, which is the whole point of carrying it.
    #[tokio::test]
    async fn pull_and_push_round_trip_a_directory_with_the_base_it_pulled() {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use std::sync::Arc;
        use std::sync::Mutex;

        type Saved = Arc<Mutex<Vec<BTreeMap<String, String>>>>;

        async fn files() -> (StatusCode, String) {
            (
                StatusCode::OK,
                serde_json::json!({
                    "version": 4,
                    "saved_by": "wren",
                    "saved_at": "2026-08-23T10:00:00Z",
                    "diagnostics": [],
                    "files": {
                        "crucible.toml": "[workflow]\n",
                        "skills/read/SKILL.md": "read it\n",
                    },
                })
                .to_string(),
            )
        }

        async fn versions(State(saved): State<Saved>, body: String) -> (StatusCode, String) {
            let sent: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
            if sent["base_version"] != serde_json::json!(4) {
                return (
                    StatusCode::CONFLICT,
                    serde_json::json!({
                        "error": "this save edited version 3, but agent:author saved version 4",
                        "base_version": sent["base_version"],
                        "current_version": 4,
                        "saved_by": "agent:author",
                        "saved_at": "2026-08-23T10:05:00Z",
                    })
                    .to_string(),
                );
            }
            let files: BTreeMap<String, String> =
                serde_json::from_value(sent["files"].clone()).expect("a file map");
            saved.lock().expect("lock").push(files);
            (
                StatusCode::OK,
                serde_json::json!({
                    "version": 5,
                    "saved_by": "wren",
                    "saved_at": "2026-08-23T10:10:00Z",
                    "schema_digest": "sha256:aa",
                    "diagnostics": [],
                })
                .to_string(),
            )
        }

        let saved: Saved = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/playbook-drafts/{id}/files", get(files))
            .route("/api/playbook-drafts/{id}/versions", post(versions))
            .with_state(saved.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("pack");
        std::fs::create_dir_all(&root).expect("mkdir");
        let pulled = draft_pull(&client, "studio", &root, None, false)
            .await
            .expect("pull");
        assert!(pulled.contains("version 4"), "{pulled}");
        assert!(pulled.contains("base_version=4"), "{pulled}");
        assert_eq!(
            std::fs::read_to_string(root.join("skills/read/SKILL.md")).expect("pulled file"),
            "read it\n"
        );

        std::fs::write(root.join("workflow.star"), "params = {}\n").expect("write");
        std::fs::remove_file(root.join("skills/read/SKILL.md")).expect("delete");
        let pushed = draft_push(&client, "studio", &root, 4, false)
            .await
            .expect("push");
        assert!(pushed.contains("version 5"), "{pushed}");
        let landed = {
            let saved = saved.lock().expect("lock");
            assert_eq!(saved.len(), 1);
            saved[0].keys().cloned().collect::<Vec<_>>()
        };
        assert_eq!(
            landed,
            vec!["crucible.toml".to_string(), "workflow.star".to_string()],
            "the whole tree is the save, so a deleted file is deleted"
        );

        let refused = draft_push(&client, "studio", &root, 3, false)
            .await
            .expect("a stale base is a refusal, not an error");
        assert!(refused.starts_with("REFUSED:"), "{refused}");
        assert!(refused.contains("base_version=4"), "{refused}");
        assert_eq!(
            saved.lock().expect("lock").len(),
            1,
            "the stale push wrote nothing"
        );
    }

    /// A controller that answers with a path outside the pack writes nothing anywhere.
    #[tokio::test]
    async fn a_pull_refuses_a_file_map_that_escapes_the_directory() {
        use axum::http::StatusCode;
        use axum::routing::get;

        async fn files() -> (StatusCode, String) {
            (
                StatusCode::OK,
                serde_json::json!({
                    "version": 1,
                    "files": { "../escape.txt": "owned\n" },
                })
                .to_string(),
            )
        }

        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/playbook-drafts/{id}/files", get(files));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("pack");
        std::fs::create_dir_all(&root).expect("mkdir");
        let refused = draft_pull(&client, "studio", &root, None, false)
            .await
            .expect_err("a path outside the pack");
        assert!(
            format!("{refused:#}").contains("relative path inside the pack"),
            "{refused:#}"
        );
        assert!(
            !dir.path().join("escape.txt").exists(),
            "nothing landed beside the directory"
        );
    }

    #[tokio::test]
    async fn an_issue_list_is_capped_client_side_and_says_what_it_cut() {
        use axum::extract::{Query as AxumQuery, State};
        use axum::http::StatusCode;
        use axum::routing::get;
        use std::sync::Arc;
        use std::sync::Mutex;

        type Seen = Arc<Mutex<Vec<BTreeMap<String, String>>>>;

        async fn issues_route(
            State(seen): State<Seen>,
            AxumQuery(q): AxumQuery<BTreeMap<String, String>>,
        ) -> (StatusCode, String) {
            seen.lock().expect("lock").push(q);
            let rows: Vec<Value> = (0..120)
                .map(|n| {
                    serde_json::json!({
                        "key": format!("o/r#{n}"),
                        "kind": {"type": "github", "owner": "o", "repo": "r", "number": n},
                        "status": "parked",
                        "title": "t",
                    })
                })
                .collect();
            (StatusCode::OK, serde_json::Value::Array(rows).to_string())
        }

        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/issues", get(issues_route))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let capped = issues(&client, None, None, None, false)
            .await
            .expect("list");
        assert_eq!(
            capped.lines().next().expect("a header"),
            "50 of 120 issues (raise limit for more)"
        );
        assert_eq!(capped.lines().count(), 52);

        let asked = issues(&client, None, Some("pr-open"), Some(3), false)
            .await
            .expect("list");
        assert_eq!(
            asked.lines().next().expect("a header"),
            "3 of 120 issues (raise limit for more)"
        );
        assert_eq!(
            seen.lock().expect("lock").last().expect("a request")["status"],
            "pr-open"
        );

        let raw = issues(&client, None, None, Some(2), true)
            .await
            .expect("json");
        let parsed: Value = serde_json::from_str(&raw).expect("json");
        assert_eq!(parsed.as_array().expect("an array").len(), 2);

        let requests_before = seen.lock().expect("lock").len();
        let refused = issues(&client, None, Some("scoping"), None, false)
            .await
            .expect_err("`scoping` is not a controller status");
        let msg = format!("{refused:#}");
        assert!(msg.contains("scoped"), "{msg}");
        assert!(msg.contains("awaiting-approval"), "{msg}");
        assert_eq!(
            seen.lock().expect("lock").len(),
            requests_before,
            "an unknown status never reaches the controller"
        );
    }

    /// The authoring loop, end to end and without leaving these calls: read what the newest save
    /// compiled to, test-fire it against that digest, then graduate it.
    #[tokio::test]
    async fn a_draft_is_previewed_test_fired_and_graduated_through_the_same_client() {
        use axum::extract::{Query as AxumQuery, State};
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use std::sync::Arc;
        use std::sync::Mutex;

        type Launched = Arc<Mutex<Vec<Value>>>;

        async fn preview(
            AxumQuery(q): AxumQuery<BTreeMap<String, String>>,
        ) -> (StatusCode, String) {
            let version = q.get("version").map_or(9, |v| v.parse().unwrap_or(9));
            (
                StatusCode::OK,
                serde_json::json!({
                    "version": version,
                    "saved_by": "wren",
                    "saved_at": "2026-08-28T10:00:00Z",
                    "schema_digest": "sha256:bb",
                    "diagnostics": [],
                })
                .to_string(),
            )
        }

        async fn launch(State(seen): State<Launched>, body: String) -> (StatusCode, String) {
            let sent: Value = serde_json::from_str(&body).expect("a JSON body");
            if sent["schema_digest"] != serde_json::json!("sha256:bb") {
                return (
                    StatusCode::CONFLICT,
                    serde_json::json!({ "error": "a save landed under the launcher" }).to_string(),
                );
            }
            seen.lock().expect("lock").push(sent);
            (
                StatusCode::CREATED,
                serde_json::json!({
                    "key": "scenario:draft-1",
                    "playbook": "calibrate",
                    "max_cost": 4.0,
                    "max_time": "30m",
                    "actor": "wren",
                    "dispatch_target": "wharf",
                })
                .to_string(),
            )
        }

        async fn graduate(body: String) -> (StatusCode, String) {
            let sent: Value = serde_json::from_str(&body).expect("a JSON body");
            assert_eq!(sent["repo"], serde_json::json!("wren/packs"));
            (
                StatusCode::OK,
                serde_json::json!({ "pr_url": "https://github.com/wren/packs/pull/7" }).to_string(),
            )
        }

        let seen: Launched = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/playbook-drafts/{id}/preview", get(preview))
            .route("/api/playbook-drafts/{id}/launch", post(launch))
            .route("/api/playbook-drafts/{id}/graduate", post(graduate))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let compiled = draft_preview(&client, "calibrate", None, false)
            .await
            .expect("preview");
        assert!(compiled.contains("version 9 compiled"), "{compiled}");
        assert!(compiled.contains("sha256:bb"), "{compiled}");

        let params = BTreeMap::from([("repo".to_string(), "wren/packs".to_string())]);
        let launch = |digest: &str| DraftLaunch {
            params: params.clone(),
            max_cost: 4.0,
            max_time: "30m".to_string(),
            schema_digest: Some(digest.to_string()),
            provider: Some("plat-openai".to_string()),
            model: Some(" gpt-5.6-luna ".to_string()),
            ..DraftLaunch::default()
        };
        let fired = draft_launch(&client, "calibrate", launch("sha256:bb"), false)
            .await
            .expect("launch");
        assert!(fired.contains("scenario:draft-1"), "{fired}");
        assert!(
            fired.contains(&format!("{url}/playbook-runs/scenario%3Adraft-1")),
            "{fired}"
        );
        let sent = seen.lock().expect("lock")[0].clone();
        assert_eq!(sent["params"]["repo"], serde_json::json!("wren/packs"));
        assert_eq!(sent["provider"], serde_json::json!("plat-openai"));
        assert_eq!(sent["model"], serde_json::json!("gpt-5.6-luna"));
        assert!(sent.get("dispatch_target").is_none(), "{sent}");

        let stale = draft_launch(&client, "calibrate", launch("sha256:aa"), false)
            .await
            .expect_err("a digest the save moved out from under");
        assert!(
            format!("{stale:#}").contains("a save landed under the launcher"),
            "{stale:#}"
        );
        assert_eq!(
            seen.lock().expect("lock").len(),
            1,
            "the refused launch fired nothing"
        );

        let graduated = draft_graduate(&client, "calibrate", "wren/packs", None, false)
            .await
            .expect("graduate");
        assert!(
            graduated.contains("https://github.com/wren/packs/pull/7"),
            "{graduated}"
        );
    }

    /// A registered launch sends the pin the way a test-fire does: trimmed, and omitted rather
    /// than sent blank so the controller's own default applies instead of a 422.
    #[tokio::test]
    async fn a_launch_carries_its_pin_and_omits_what_was_left_blank() {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use std::sync::Arc;
        use std::sync::Mutex;

        type Launched = Arc<Mutex<Vec<Value>>>;

        async fn accept(State(seen): State<Launched>, body: String) -> (StatusCode, String) {
            let sent: Value = serde_json::from_str(&body).expect("a JSON body");
            let ack = serde_json::json!({
                "key": "playbook:survey:1",
                "playbook": "survey",
                "max_cost": sent["max_cost"],
                "max_time": sent["max_time"],
                "actor": "wren",
                "dispatch_target": sent.get("dispatch_target").cloned(),
                "provider": sent.get("provider").cloned(),
                "model": sent.get("model").cloned(),
            });
            seen.lock().expect("lock").push(sent);
            (StatusCode::CREATED, ack.to_string())
        }

        let seen: Launched = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/playbooks/{id}/launch", post(accept))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let pinned = launch(
            &client,
            "survey",
            Launch {
                params: serde_json::json!({"topic": "attention sinks"}),
                max_cost: 4.0,
                max_time: "30m".to_string(),
                dispatch_target: Some("  ".to_string()),
                provider: Some("plat-openai".to_string()),
                model: Some(" gpt-5.6-luna ".to_string()),
            },
            false,
        )
        .await
        .expect("launch");
        assert!(
            pinned.contains("launched survey as playbook:survey:1"),
            "{pinned}"
        );
        assert!(
            pinned.contains("agent plat-openai · gpt-5.6-luna"),
            "{pinned}"
        );
        assert!(
            pinned.contains(&format!("{url}/playbook-runs/playbook%3Asurvey%3A1")),
            "{pinned}"
        );
        let sent = seen.lock().expect("lock")[0].clone();
        assert_eq!(
            sent["params"]["topic"],
            serde_json::json!("attention sinks")
        );
        assert_eq!(sent["max_cost"], serde_json::json!(4.0));
        assert_eq!(sent["max_time"], serde_json::json!("30m"));
        assert_eq!(sent["provider"], serde_json::json!("plat-openai"));
        assert_eq!(sent["model"], serde_json::json!("gpt-5.6-luna"));
        assert!(
            sent.get("dispatch_target").is_none(),
            "a blank target is omitted, not sent: {sent}"
        );

        let bare = launch(
            &client,
            "survey",
            Launch {
                params: serde_json::json!({}),
                max_cost: 1.0,
                max_time: "5m".to_string(),
                provider: Some("plat-openai".to_string()),
                ..Launch::default()
            },
            false,
        )
        .await
        .expect("launch");
        assert!(
            bare.contains("agent plat-openai (its default model)"),
            "{bare}"
        );
        let sent = seen.lock().expect("lock")[1].clone();
        assert_eq!(sent["provider"], serde_json::json!("plat-openai"));
        assert!(sent.get("model").is_none(), "{sent}");

        let unpinned = launch(
            &client,
            "survey",
            Launch {
                params: serde_json::json!({}),
                max_cost: 1.0,
                max_time: "5m".to_string(),
                ..Launch::default()
            },
            true,
        )
        .await
        .expect("launch");
        let ack: Value = serde_json::from_str(&unpinned).expect("json output");
        assert!(ack["provider"].is_null(), "{ack}");
        let sent = seen.lock().expect("lock")[2].clone();
        assert!(
            sent.get("provider").is_none() && sent.get("model").is_none(),
            "{sent}"
        );
    }

    /// Listing is metadata only, and a bind sends exactly the scope and projection asked for,
    /// with the declared name only when one was given.
    #[tokio::test]
    async fn caps_secrets_and_bind_speak_their_routes() {
        use axum::extract::Path as AxumPath;
        use axum::http::StatusCode;
        use axum::routing::{get, post};

        async fn list() -> (StatusCode, String) {
            (
                StatusCode::OK,
                serde_json::json!([{
                    "id": "01a0",
                    "name": "gh-token",
                    "owner": "user:wynn",
                    "kind": "opaque",
                    "visibility": "agent_visible",
                    "consumer": "run",
                    "mode": "managed",
                    "vault_path": "secret/x",
                    "created_by": "wynn",
                    "created_at": "2026-08-31T20:15:07Z",
                    "updated_at": "2026-08-31T20:15:07Z"
                }])
                .to_string(),
            )
        }

        async fn bind(AxumPath(id): AxumPath<String>, body: String) -> (StatusCode, String) {
            assert_eq!(id, "01a0");
            let sent: Value = serde_json::from_str(&body).expect("a JSON body");
            assert_eq!(
                sent,
                serde_json::json!({
                    "scope_kind": "playbook",
                    "scope_id": "docs-drift",
                    "projection_kind": "env",
                    "projection": "GH_TOKEN",
                    "declared_name": "docs-drift-pr-token",
                }),
                "nothing beyond what was asked for, and no absent declared_name"
            );
            (
                StatusCode::CREATED,
                serde_json::json!({
                    "id": "b1",
                    "secret_id": "01a0",
                    "scope_kind": "playbook",
                    "scope_id": "docs-drift",
                    "projection_kind": "env",
                    "projection": "GH_TOKEN",
                    "declared_name": "docs-drift-pr-token",
                    "created_by": "wynn",
                    "created_at": "2026-08-31T20:16:00Z"
                })
                .to_string(),
            )
        }

        async fn caps() -> (StatusCode, String) {
            (
                StatusCode::OK,
                serde_json::json!({ "max_cost": 5.0, "max_time": "30m" }).to_string(),
            )
        }

        async fn delete_draft(AxumPath(id): AxumPath<String>) -> StatusCode {
            if id == "gone-already" {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::NO_CONTENT
            }
        }

        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/config/playbook-caps", get(caps))
            .route("/api/secrets", get(list))
            .route("/api/secrets/{id}/bindings", post(bind))
            .route(
                "/api/playbook-drafts/{id}",
                axum::routing::delete(delete_draft),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let deleted = draft_delete(&client, "ci-optimize").await.expect("204");
        assert_eq!(deleted, "deleted draft ci-optimize\n");
        let missing = draft_delete(&client, "gone-already")
            .await
            .expect_err("404");
        assert!(missing.to_string().contains("404"), "{missing}");

        let caps = playbook_caps(&client, false).await.expect("caps");
        assert!(caps.contains("max_cost: $5.00\nmax_time: 30m\n"), "{caps}");

        let listed = secrets(&client, false).await.expect("list");
        assert!(listed.contains("1 secrets"), "{listed}");
        assert!(listed.contains("01a0"), "{listed}");
        assert!(listed.contains("agent_visible"), "{listed}");
        assert!(
            !listed.contains("secret/x"),
            "no vault path in the listing: {listed}"
        );

        let bound = secret_bind(
            &client,
            SecretBind {
                secret_id: "01a0",
                scope_kind: "playbook",
                scope_id: "docs-drift",
                projection_kind: "env",
                projection: "GH_TOKEN",
                declared_name: Some("docs-drift-pr-token"),
            },
            false,
        )
        .await
        .expect("bind");
        assert!(
            bound.contains("bound 01a0 to playbook/docs-drift: accepted"),
            "{bound}"
        );
        assert!(
            bound.contains("as docs-drift-pr-token -> env GH_TOKEN"),
            "{bound}"
        );
        assert!(bound.contains("actor: wynn"), "{bound}");
    }

    /// Following prints only what is new on each poll and stops the moment the run is no
    /// longer running, with the final status in the trailer.
    #[tokio::test]
    async fn follow_prints_new_lines_and_stops_when_the_run_settles() {
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::get;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Each poll sees one more line; the run settles on the third.
        async fn log(State(polls): State<Arc<AtomicUsize>>) -> (StatusCode, String) {
            let n = polls.fetch_add(1, Ordering::SeqCst) + 1;
            let text = (1..=n)
                .map(|i| format!("line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            (
                StatusCode::OK,
                serde_json::json!({ "run_id": "r1", "dispatch": "pod", "text": text, "truncated": false })
                    .to_string(),
            )
        }
        async fn run(State(polls): State<Arc<AtomicUsize>>) -> (StatusCode, String) {
            let status = if polls.load(Ordering::SeqCst) >= 3 {
                "finished"
            } else {
                "running"
            };
            (
                StatusCode::OK,
                serde_json::json!({ "run": { "run_id": "r1", "status": status }, "candidates": [] })
                    .to_string(),
            )
        }

        let polls = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/runs/{id}/log", get(log))
            .route("/api/runs/{id}", get(run))
            .with_state(polls);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        let client = test_client(&url);

        let mut out: Vec<u8> = Vec::new();
        let trailer = run_log_follow(&client, "r1", 0, Duration::from_millis(10), &mut out)
            .await
            .expect("follow");
        let printed = String::from_utf8(out).expect("utf8");
        assert_eq!(
            printed, "run r1 [pod] following from line 0\nline 1\nline 2\nline 3\n",
            "each line exactly once"
        );
        assert_eq!(trailer, "-- run r1 is finished, 3 lines seen\n");
    }

    fn test_client(url: &str) -> Client {
        let cfg = crate::config::Config {
            url: url.to_string(),
            auth: crate::config::Auth::None,
        };
        Client::connect(&cfg).expect("connect")
    }
}
