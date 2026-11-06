//! Draft packs in the authoring studio: the surface an agent shares with a human.

use crate::client::{Client, DraftSave, MigrateReply, encode};
use crate::dto;
use crate::ops::playbooks::put_rerun;
use crate::ops::{Operation, put_trimmed};
use crate::render;
use anyhow::{Context, Result, bail};
use crucible_contract::pack_tree::{TreeDigest, walk_dir};
use rmcp::schemars;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// The studio page a draft is edited on.
pub(crate) fn studio_url(c: &Client, draft_id: &str) -> String {
    c.web_url(&format!("/playbooks/drafts/{}", encode(draft_id)))
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftFiles {
    pub draft_id: String,
    /// Which save to read. Omit for the newest.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Which save to read. Omit for the newest — which is what a save must be based on."
    )]
    pub version: Option<i64>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for DraftFiles {
    type Output = dto::DraftFiles;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.draft_files(&self.draft_id, self.version).await
    }

    fn render(&self, _c: &Client, files: Self::Output) -> Result<String> {
        Ok(render::draft_files(&self.draft_id, &files))
    }

    fn json(&self) -> bool {
        self.json
    }
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftCreate {
    #[schemars(
        description = "Lowercase slug; the studio page and every launch of the draft are keyed by it."
    )]
    pub draft_id: String,
    #[arg(long)]
    pub description: String,
    /// A registered playbook id to seed version 1 from.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "A registered playbook id to seed version 1 from; omitted opens on a minimal skeleton."
    )]
    pub template: Option<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for DraftCreate {
    type Output = dto::DraftCompile;

    async fn call(&self, c: &Client) -> Result<Value> {
        let mut raw: Value = c
            .create_draft(&self.draft_id, &self.description, self.template.as_deref())
            .await?;
        if let Some(obj) = raw.as_object_mut() {
            obj.insert(
                "studio_url".to_string(),
                Value::String(studio_url(c, &self.draft_id)),
            );
            obj.insert(
                "skill_url".to_string(),
                Value::String(c.web_url("/api/playbooks/drafts/skill")),
            );
        }
        Ok(raw)
    }

    fn render(&self, c: &Client, saved: Self::Output) -> Result<String> {
        Ok(render::draft_created(
            &self.draft_id,
            &saved,
            &studio_url(c, &self.draft_id),
            &c.web_url("/api/playbooks/drafts/skill"),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// What a save landed: the stored version, with the studio link added, or the refusal naming the
/// save that overtook it. A stale base is data, not an error: nothing was written, and the merge
/// is the caller's next move.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Saved {
    Stale(dto::StaleBase),
    Version(dto::DraftCompile),
}

/// Save `files` as the next version and report it, with the directory it came from when it came
/// from one.
async fn save(
    c: &Client,
    draft_id: &str,
    base_version: i64,
    files: &BTreeMap<String, String>,
    local: Option<&LocalPack>,
) -> Result<Value> {
    let mut raw = match c.save_draft(draft_id, files, base_version).await? {
        DraftSave::Saved(mut raw) => {
            if let Some(obj) = raw.as_object_mut() {
                obj.insert(
                    "studio_url".to_string(),
                    Value::String(studio_url(c, draft_id)),
                );
            }
            raw
        }
        DraftSave::Stale(stale) => serde_json::json!({
            "error": stale.error,
            "base_version": stale.base_version,
            "current_version": stale.current_version,
            "saved_by": stale.saved_by,
            "saved_at": stale.saved_at,
        }),
    };
    if let (Some(local), Some(obj)) = (local, raw.as_object_mut()) {
        local.insert_into(obj);
    }
    Ok(raw)
}

fn render_saved(c: &Client, draft_id: &str, saved: Saved, local: Option<&LocalPack>) -> String {
    let text = match saved {
        Saved::Version(saved) => render::draft_saved(draft_id, &saved, &studio_url(c, draft_id)),
        Saved::Stale(stale) => render::stale_base(draft_id, &stale),
    };
    match local {
        Some(local) => text + &render::local_pack(&local.digest, &local.ignored),
        None => text,
    }
}

/// Save a draft version from an inline file map. The CLI fills one from a file or stdin.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
pub struct DraftSaveFiles {
    pub draft_id: String,
    /// The version these edits were made against, from crucible_draft_files. A save whose base is
    /// no longer the newest is refused, not merged.
    pub base_version: i64,
    /// The whole pack, `{path: content}`. A path missing from the map is deleted from the pack.
    pub files: BTreeMap<String, String>,
    #[serde(default)]
    pub json: bool,
}

impl Operation for DraftSaveFiles {
    type Output = Saved;

    async fn call(&self, c: &Client) -> Result<Value> {
        save(c, &self.draft_id, self.base_version, &self.files, None).await
    }

    fn render(&self, c: &Client, saved: Self::Output) -> Result<String> {
        Ok(render_saved(c, &self.draft_id, saved, None))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// A pack path, checked before it becomes a filesystem path. A pack is a relative tree; anything
/// absolute, rooted or climbing out of it is refused, in both directions: the controller's map is
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
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftPull {
    pub draft_id: String,
    /// The directory to write the pack into. Created if it does not exist.
    #[schemars(
        description = "The local directory to write the pack into. Created if it does not exist."
    )]
    pub dir: PathBuf,
    /// Which save to pull. Omit for the newest.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Which save to pull. Omit for the newest — which is what a save must be based on."
    )]
    pub version: Option<i64>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

/// What a pull wrote, as its JSON reply says it.
#[derive(Debug, Deserialize)]
pub struct Pulled {
    pub version: i64,
    pub files: Vec<String>,
    pub local_tree_digest: TreeDigest,
    pub ignored: Vec<String>,
}

impl Operation for DraftPull {
    type Output = Pulled;

    async fn call(&self, c: &Client) -> Result<Value> {
        let pulled: dto::DraftFiles = c.draft_files(&self.draft_id, self.version).await?;
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let mut written = Vec::new();
        for (path, content) in &pulled.files {
            let target = self.dir.join(pack_relative(path)?);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::fs::write(&target, content)
                .with_context(|| format!("writing {}", target.display()))?;
            written.push(path.clone());
        }
        let local = read_pack_dir(&self.dir).with_context(|| {
            format!(
                "wrote version {} into {}, but a push cannot send that directory back",
                pulled.version,
                self.dir.display()
            )
        })?;
        let mut out = serde_json::json!({
            "draft_id": self.draft_id,
            "version": pulled.version,
            "dir": self.dir.to_string_lossy(),
            "files": written,
        });
        if let Some(obj) = out.as_object_mut() {
            local.insert_into(obj);
        }
        Ok(out)
    }

    fn render(&self, _c: &Client, pulled: Self::Output) -> Result<String> {
        Ok(
            render::draft_pulled(&self.draft_id, pulled.version, &self.dir, &pulled.files)
                + &render::local_pack(&pulled.local_tree_digest, &pulled.ignored),
        )
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Save a directory back as the next draft version. The whole tree is the save, so a file deleted
/// on disk is a file deleted from the pack, and `base_version` is what protects the other editor.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftPush {
    pub draft_id: String,
    /// The directory holding the pack.
    #[schemars(description = "The local directory holding the pack. Its whole tree is the save.")]
    pub dir: PathBuf,
    /// The version these edits were made against (what `draft-pull` printed).
    #[arg(long = "base-version")]
    #[schemars(
        description = "The version this directory was pulled at, from crucible_draft_pull. A save whose base is\nno longer the newest is refused, not merged."
    )]
    pub base_version: i64,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl DraftPush {
    fn local(&self) -> Result<LocalPack> {
        let local = read_pack_dir(&self.dir)?;
        if local.files.is_empty() {
            bail!(
                "{} holds no files; a save with none would empty the pack",
                self.dir.display()
            );
        }
        Ok(local)
    }
}

impl Operation for DraftPush {
    type Output = Saved;

    async fn call(&self, c: &Client) -> Result<Value> {
        let local = self.local()?;
        save(
            c,
            &self.draft_id,
            self.base_version,
            &local.files,
            Some(&local),
        )
        .await
    }

    fn render(&self, c: &Client, saved: Self::Output) -> Result<String> {
        let local = self.local()?;
        Ok(render_saved(c, &self.draft_id, saved, Some(&local)))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// A pack directory read the way the controller and the engine read one.
#[derive(Debug)]
struct LocalPack {
    /// The `{path: content}` map a save is, `/`-separated whatever the platform.
    files: BTreeMap<String, String>,
    digest: TreeDigest,
    /// The `state`, `workspace` and `.git` paths skipped, which a save never carries.
    ignored: Vec<String>,
}

impl LocalPack {
    fn insert_into(&self, obj: &mut serde_json::Map<String, Value>) {
        obj.insert(
            "local_tree_digest".to_string(),
            Value::String(self.digest.to_string()),
        );
        obj.insert(
            "ignored".to_string(),
            Value::Array(self.ignored.iter().cloned().map(Value::String).collect()),
        );
    }
}

/// A pack directory as the save it is. Symlinks and non-text files are refused rather than
/// followed or dropped: a save is the whole tree, and a file this cannot carry is a file the save
/// would delete.
fn read_pack_dir(dir: &Path) -> Result<LocalPack> {
    let read = walk_dir(dir).with_context(|| format!("reading the pack in {}", dir.display()))?;
    let digest = read.tree.digest();
    let files = read
        .tree
        .into_files()
        .into_iter()
        .map(|(path, bytes)| {
            let path = path.to_string();
            match String::from_utf8(bytes) {
                Ok(text) => Ok((path, text)),
                Err(_) => bail!("{path} is not text, so it cannot be saved as a draft"),
            }
        })
        .collect::<Result<_>>()?;
    Ok(LocalPack {
        files,
        digest,
        ignored: read.ignored,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftPreview {
    pub draft_id: String,
    /// Which save to compile. Omit for the newest.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Which save to compile. Omit for the newest — which is what a launch runs."
    )]
    pub version: Option<i64>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for DraftPreview {
    type Output = dto::DraftCompile;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.draft_preview(&self.draft_id, self.version).await
    }

    fn render(&self, c: &Client, compiled: Self::Output) -> Result<String> {
        Ok(render::draft_preview(
            &self.draft_id,
            &compiled,
            &studio_url(c, &self.draft_id),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

fn parse_string_map(s: &str) -> Result<BTreeMap<String, String>, String> {
    serde_json::from_str(s).map_err(|e| {
        format!(
            "not a JSON object of string to string; a draft launch takes form values as strings: {e}"
        )
    })
}

/// Test-fire a draft. The values are the newest save's stored schema's, strings because the
/// endpoint takes a string map, and a draft launch is held to the same admin caps a registered
/// one is. An optional field left blank is omitted from the body, so the controller's own default
/// applies.
#[derive(Debug, Clone, PartialEq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftLaunch {
    pub draft_id: String,
    /// The form values its schema accepts, as a JSON object of string to string.
    #[arg(long, default_value = "{}", value_parser = parse_string_map)]
    #[serde(default)]
    #[schemars(
        description = "The form values the newest save's schema accepts, as strings. Read the schema digest and\nthe diagnostics from crucible_draft_preview first."
    )]
    pub params: BTreeMap<String, String>,
    /// Dollar ceiling for this launch.
    #[arg(long)]
    #[schemars(
        description = "Dollar ceiling for this launch. The controller refuses anything above the admin cap."
    )]
    pub max_cost: f64,
    /// Wall-clock ceiling, e.g. 30m, 2h.
    #[arg(long)]
    #[schemars(description = "Wall-clock ceiling, e.g. `30m`, `2h`.")]
    pub max_time: String,
    /// The schema digest these values were filled against (see `draft-preview`). A save that
    /// landed since is refused instead of launched.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "The schema digest these values were filled against, from crucible_draft_preview. A save\nthat landed since is refused rather than launched against a schema that moved."
    )]
    pub schema_digest: Option<String>,
    /// Cluster to dispatch onto. Omit for the controller's default.
    #[arg(long)]
    #[serde(default)]
    pub dispatch_target: Option<String>,
    /// Registered inference provider the run's agent talks to, replacing the pack manifest's
    /// `[agent]` harness. Omit to resolve through the configured defaults.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Registered inference provider id the run's agent talks to (see GET /api/config/providers),\nreplacing the pack manifest's `[agent]` harness. Omit to resolve through the configured\ndefaults."
    )]
    pub provider: Option<String>,
    /// The model to ask that provider for. Needs --provider; omit for its default.
    #[arg(long, requires = "provider")]
    #[serde(default)]
    #[schemars(
        description = "The model to ask that provider for, replacing the manifest's `[agent].model`. Needs\n`provider`; omit for the provider's default."
    )]
    pub model: Option<String>,
    /// Rerun this finished run of the same pack instead of starting afresh: every task it
    /// passed is reused, and the tasks --from names run again with everything after them.
    #[arg(long, value_name = "RUN")]
    #[serde(skip)]
    pub rerun_of: Option<String>,
    /// A task the rerun dispatches again, with everything downstream of it. Repeatable.
    #[arg(long = "from", value_name = "TASK", requires = "rerun_of")]
    #[serde(skip)]
    pub from: Vec<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for DraftLaunch {
    type Output = dto::LaunchAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        let mut body = serde_json::json!({
            "params": self.params,
            "max_cost": self.max_cost,
            "max_time": self.max_time,
        });
        put_trimmed(&mut body, "schema_digest", self.schema_digest.as_deref());
        put_trimmed(
            &mut body,
            "dispatch_target",
            self.dispatch_target.as_deref(),
        );
        put_trimmed(&mut body, "provider", self.provider.as_deref());
        put_trimmed(&mut body, "model", self.model.as_deref());
        put_rerun(&mut body, self.rerun_of.as_deref(), &self.from);
        c.launch_draft(&self.draft_id, &body).await
    }

    fn render(&self, c: &Client, ack: Self::Output) -> Result<String> {
        let url = c.web_url(&format!("/playbook-runs/{}", encode(&ack.key)));
        Ok(render::launched(&ack, &url))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Delete a draft outright. Admin only, and gone is gone: every version, and the id is free for a
/// new `draft-create`.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftDelete {
    pub draft_id: String,
}

impl Operation for DraftDelete {
    type Output = ();

    async fn call(&self, c: &Client) -> Result<Value> {
        c.delete_draft(&self.draft_id).await?;
        Ok(Value::Null)
    }

    fn render(&self, _c: &Client, (): Self::Output) -> Result<String> {
        Ok(format!("deleted draft {}\n", self.draft_id))
    }

    fn json(&self) -> bool {
        false
    }
}

/// Move a draft to a new id. Its saves, secret bindings, shares, standing launches and launches
/// follow it.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct DraftRename {
    pub draft_id: String,
    /// The new id.
    pub to: String,
    #[arg(long)]
    pub json: bool,
}

impl Operation for DraftRename {
    type Output = dto::DraftRenamed;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.rename_draft(&self.draft_id, &self.to).await
    }

    fn render(&self, c: &Client, renamed: Self::Output) -> Result<String> {
        Ok(render::draft_renamed(&renamed, &studio_url(c, &renamed.id)))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Copy a draft's newest save into a new draft. A binding comes along only when the caller could
/// bind its secret themselves; a reference-mode secret needs a Vault token this client never
/// sends, so its bindings are reported as left behind.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct DraftClone {
    pub draft_id: String,
    /// The copy's id.
    pub to: String,
    /// Defaults to the source draft's.
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub json: bool,
}

impl Operation for DraftClone {
    type Output = dto::DraftCloned;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.clone_draft(&self.draft_id, &self.to, self.description.as_deref())
            .await
    }

    fn render(&self, c: &Client, cloned: Self::Output) -> Result<String> {
        Ok(render::draft_cloned(
            &self.draft_id,
            &cloned,
            &studio_url(c, &cloned.id),
        ))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Register a draft's newest compiling version as a playbook, with no review.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftPublish {
    pub draft_id: String,
    /// The playbook to publish into. Omit to re-pin the one this draft last published into,
    /// else the one it was seeded from.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "The playbook id to publish into. Omit to re-pin the one this draft last published into,\nelse the registered pack it was seeded from."
    )]
    pub playbook: Option<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for DraftPublish {
    type Output = dto::PublishAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.publish_draft(&self.draft_id, self.playbook.as_deref())
            .await
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        Ok(render::draft_published(&self.draft_id, &ack))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Export a draft as a PR against `repo`. Already graduated comes back as the controller's own
/// refusal, which carries the open PR.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftGraduate {
    pub draft_id: String,
    /// `owner/repo` the export PR opens against.
    #[arg(long)]
    pub repo: String,
    /// The pack directory inside that repo. Omit for the repo root.
    #[arg(long)]
    #[serde(default)]
    pub path: Option<String>,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl Operation for DraftGraduate {
    type Output = dto::GraduateAck;

    async fn call(&self, c: &Client) -> Result<Value> {
        c.graduate_draft(&self.draft_id, &self.repo, self.path.as_deref())
            .await
    }

    fn render(&self, _c: &Client, ack: Self::Output) -> Result<String> {
        Ok(render::draft_graduated(&self.draft_id, &ack))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// Move drafts to a newer pack edition with the controller's engine. A dry run unless `--apply`:
/// each draft's diff, the changes, and what needs review, with nothing saved.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args, Deserialize, schemars::JsonSchema)]
pub struct DraftMigrate {
    /// The draft to migrate. Omit with --all.
    #[arg(required_unless_present = "all", conflicts_with = "all")]
    #[serde(default)]
    #[schemars(description = "The draft to migrate. Omit when `all` is set.")]
    pub draft_id: Option<String>,
    /// Every draft you may read that is below the target edition, retired ones aside.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Every draft you may read that is below the target edition, retired ones aside."
    )]
    pub all: bool,
    /// With --all, only drafts this principal owns: `user:<login>` or `team:<slug>`.
    #[arg(long, requires = "all")]
    #[serde(default)]
    #[schemars(
        description = "With `all`, only drafts this principal owns: `user:<login>` or `team:<slug>`."
    )]
    pub owner: Option<String>,
    /// The edition to move to. Omit for the newest the controller's engine runs.
    #[arg(long)]
    #[serde(default)]
    pub to: Option<u32>,
    /// Print each diff and its findings and save nothing. The default.
    #[arg(long, conflicts_with = "apply")]
    #[serde(default)]
    pub dry_run: bool,
    /// Save each migration as its draft's next version.
    #[arg(long)]
    #[serde(default)]
    #[schemars(
        description = "Save each migration as its draft's next version. Without it nothing is saved."
    )]
    pub apply: bool,
    #[arg(long)]
    #[serde(default)]
    pub json: bool,
}

impl DraftMigrate {
    /// The drafts to migrate, each with the version its migration is based on, and the ones a
    /// sweep passes over.
    async fn targets(&self, c: &Client) -> Result<(Vec<(String, i64)>, Vec<Value>)> {
        if let Some(id) = &self.draft_id {
            let detail: dto::DraftDetail = c
                .get(&format!("/api/playbook-drafts/{}", encode(id)))
                .await?;
            return Ok((vec![(id.clone(), detail.draft.latest_version)], Vec::new()));
        }
        if !self.all {
            bail!("name a draft, or pass --all");
        }
        let mut listed: Vec<dto::DraftListEntry> = c.get("/api/playbook-drafts").await?;
        listed.retain(|d| self.owner.as_deref().is_none_or(|o| d.owner == o));
        listed.sort_by(|a, b| a.id.cmp(&b.id));
        let mut targets = Vec::new();
        let mut skipped = Vec::new();
        for draft in listed {
            if draft.retired_at.is_some() {
                skipped.push(passed(&draft.id, "retired"));
            } else {
                targets.push((draft.id, draft.latest_version));
            }
        }
        Ok((targets, skipped))
    }
}

fn passed(draft_id: &str, reason: &str) -> Value {
    serde_json::json!({ "draft_id": draft_id, "reason": reason })
}

impl Operation for DraftMigrate {
    type Output = dto::DraftMigrations;

    async fn call(&self, c: &Client) -> Result<Value> {
        if self.draft_id.is_some() && self.all {
            bail!("name a draft or pass --all, not both");
        }
        if self.owner.is_some() && !self.all {
            bail!("--owner picks drafts for --all");
        }
        if self.dry_run && self.apply {
            bail!("--dry-run and --apply are opposites; pass one");
        }
        let dry_run = !self.apply;
        let (targets, mut skipped) = self.targets(c).await?;
        let mut migrated = Vec::new();
        let mut refused = Vec::new();
        for (id, base_version) in targets {
            match c.migrate_draft(&id, self.to, base_version, dry_run).await? {
                MigrateReply::Migrated(mut raw) => {
                    let unchanged = raw["changes"].as_array().is_none_or(Vec::is_empty);
                    if unchanged {
                        let edition = raw["from"].as_i64().unwrap_or_default();
                        skipped.push(passed(&id, &format!("already at edition {edition}")));
                        continue;
                    }
                    if let Some(obj) = raw.as_object_mut() {
                        obj.insert("draft_id".to_string(), Value::String(id.clone()));
                        obj.insert("studio_url".to_string(), Value::String(studio_url(c, &id)));
                    }
                    migrated.push(raw);
                }
                MigrateReply::Stale(stale) => refused.push(passed(&id, &stale.error)),
                MigrateReply::Refused(error) => refused.push(passed(&id, &error)),
            }
        }
        if let (Some(id), Some(refusal)) = (&self.draft_id, refused.first()) {
            bail!(
                "draft {id}: {}",
                refusal["reason"].as_str().unwrap_or_default()
            );
        }
        Ok(serde_json::json!({
            "dry_run": dry_run,
            "migrated": migrated,
            "skipped": skipped,
            "refused": refused,
        }))
    }

    fn render(&self, _c: &Client, out: Self::Output) -> Result<String> {
        Ok(render::draft_migrations(&out))
    }

    fn json(&self) -> bool {
        self.json
    }
}

/// [`DraftCreate`] as a call, for callers outside this crate.
pub async fn draft_create(
    c: &Client,
    draft_id: &str,
    description: &str,
    template: Option<&str>,
    as_json: bool,
) -> Result<String> {
    DraftCreate {
        draft_id: draft_id.to_string(),
        description: description.to_string(),
        template: template.map(str::to_string),
        json: as_json,
    }
    .run(c)
    .await
}

/// [`DraftRename`] as a call, for callers outside this crate.
pub async fn draft_rename(c: &Client, draft_id: &str, to: &str, as_json: bool) -> Result<String> {
    DraftRename {
        draft_id: draft_id.to_string(),
        to: to.to_string(),
        json: as_json,
    }
    .run(c)
    .await
}

/// [`DraftClone`] as a call, for callers outside this crate.
pub async fn draft_clone(
    c: &Client,
    draft_id: &str,
    to: &str,
    description: Option<&str>,
    as_json: bool,
) -> Result<String> {
    DraftClone {
        draft_id: draft_id.to_string(),
        to: to.to_string(),
        description: description.map(str::to_string),
        json: as_json,
    }
    .run(c)
    .await
}

/// [`DraftDelete`] as a call, for callers outside this crate.
pub async fn draft_delete(c: &Client, draft_id: &str) -> Result<String> {
    DraftDelete {
        draft_id: draft_id.to_string(),
    }
    .run(c)
    .await
}

#[cfg(test)]
mod tests {
    use crate::ops::drafts::*;
    use crate::ops::test_client;

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
        let local = read_pack_dir(root).expect("read");
        assert_eq!(
            local.files.keys().cloned().collect::<Vec<_>>(),
            vec![
                "crucible.toml".to_string(),
                "skills/read/SKILL.md".to_string()
            ]
        );
        assert_eq!(local.files["skills/read/SKILL.md"], "read it\n");
        assert!(local.ignored.is_empty(), "{:?}", local.ignored);

        std::os::unix::fs::symlink("/etc/passwd", root.join("sneaky")).expect("symlink");
        let refused = read_pack_dir(root).expect_err("a symlink is not a pack file");
        assert!(
            format!("{refused:#}").contains("sneaky is a symbolic link"),
            "{refused:#}"
        );
    }

    /// `state`, `workspace` and `.git` subtrees are skipped at any depth and listed, a symlink
    /// inside one is never looked at, and the digest is the one the controller computes for the
    /// same files.
    #[test]
    fn a_pack_directory_skips_and_lists_runtime_subtrees() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("skills/read/state")).expect("mkdir");
        std::fs::create_dir_all(root.join("workspace")).expect("mkdir");
        std::fs::create_dir_all(root.join(".git")).expect("mkdir");
        std::fs::write(root.join("crucible.toml"), "[workflow]\n").expect("write");
        std::fs::write(root.join("skills/read/SKILL.md"), "read it\n").expect("write");
        std::fs::write(root.join("skills/read/state/cursor"), [0xff, 0xfe]).expect("write");
        std::fs::write(root.join("workspace/main.go"), "package main\n").expect("write");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write");
        std::os::unix::fs::symlink("/etc/passwd", root.join("workspace/link")).expect("symlink");

        let local = read_pack_dir(root).expect("read");
        assert_eq!(
            local.files.keys().cloned().collect::<Vec<_>>(),
            vec![
                "crucible.toml".to_string(),
                "skills/read/SKILL.md".to_string()
            ]
        );
        assert_eq!(local.ignored, [".git", "skills/read/state", "workspace"]);
        let expected = crucible_contract::pack_tree::PackTree::from_pairs(&[
            ("crucible.toml", b"[workflow]\n"),
            ("skills/read/SKILL.md", b"read it\n"),
        ])
        .expect("tree");
        assert_eq!(local.digest, expected.digest());

        let rendered = render::local_pack(&local.digest, &local.ignored);
        assert_eq!(
            rendered,
            format!(
                "local tree: {}\nignored: .git, skills/read/state, workspace\n",
                expected.digest()
            )
        );
        assert_eq!(
            render::local_pack(&local.digest, &[]),
            format!("local tree: {}\n", expected.digest())
        );
    }

    /// A file that is not text cannot ride a save's `{path: content}` map, so it is refused by
    /// name rather than dropped.
    #[test]
    fn a_pack_directory_refuses_a_binary_file_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("crucible.toml"), "[workflow]\n").expect("write");
        std::fs::write(root.join("blob.bin"), [0xff, 0xfe, 0x00]).expect("write");
        let refused = read_pack_dir(root).expect_err("a binary file is not a draft file");
        assert!(
            format!("{refused:#}").contains("blob.bin is not text"),
            "{refused:#}"
        );
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

        type SavedMaps = Arc<Mutex<Vec<BTreeMap<String, String>>>>;

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

        async fn versions(State(saved): State<SavedMaps>, body: String) -> (StatusCode, String) {
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

        let saved: SavedMaps = Arc::new(Mutex::new(Vec::new()));
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
        let pulled = DraftPull {
            draft_id: "studio".into(),
            dir: root.clone(),
            version: None,
            json: false,
        }
        .run(&client)
        .await
        .expect("pull");
        assert!(pulled.contains("version 4"), "{pulled}");
        assert!(pulled.contains("base_version=4"), "{pulled}");
        let pulled_tree = crucible_contract::pack_tree::PackTree::from_pairs(&[
            ("crucible.toml", b"[workflow]\n"),
            ("skills/read/SKILL.md", b"read it\n"),
        ])
        .expect("tree");
        assert!(
            pulled.contains(&format!("local tree: {}\n", pulled_tree.digest())),
            "{pulled}"
        );
        assert!(!pulled.contains("ignored:"), "{pulled}");
        assert_eq!(
            std::fs::read_to_string(root.join("skills/read/SKILL.md")).expect("pulled file"),
            "read it\n"
        );

        std::fs::write(root.join("workflow.star"), "params = {}\n").expect("write");
        std::fs::remove_file(root.join("skills/read/SKILL.md")).expect("delete");
        std::fs::create_dir_all(root.join("state")).expect("mkdir");
        std::fs::write(root.join("state/run.json"), "{}").expect("write");
        let pushed_tree = crucible_contract::pack_tree::PackTree::from_pairs(&[
            ("crucible.toml", b"[workflow]\n"),
            ("workflow.star", b"params = {}\n"),
        ])
        .expect("tree");
        let push = |base_version: i64, json: bool| DraftPush {
            draft_id: "studio".into(),
            dir: root.clone(),
            base_version,
            json,
        };
        let pushed = push(4, false).run(&client).await.expect("push");
        assert!(pushed.contains("version 5"), "{pushed}");
        assert!(
            pushed.ends_with(&format!(
                "local tree: {}\nignored: state\n",
                pushed_tree.digest()
            )),
            "{pushed}"
        );
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

        let pushed: Value =
            serde_json::from_str(&push(4, true).run(&client).await.expect("push")).expect("json");
        assert_eq!(pushed["version"], 5);
        assert_eq!(
            pushed["local_tree_digest"],
            pushed_tree.digest().to_string()
        );
        assert_eq!(pushed["ignored"], serde_json::json!(["state"]));
        assert_eq!(saved.lock().expect("lock").len(), 2);

        let refused = push(3, false)
            .run(&client)
            .await
            .expect("a stale base is a refusal, not an error");
        assert!(refused.starts_with("REFUSED:"), "{refused}");
        assert!(refused.contains("base_version=4"), "{refused}");
        assert!(
            refused.contains(&format!("local tree: {}", pushed_tree.digest())),
            "{refused}"
        );
        let refused: Value = serde_json::from_str(
            &push(3, true)
                .run(&client)
                .await
                .expect("a stale base is a refusal, not an error"),
        )
        .expect("json");
        assert_eq!(refused["current_version"], 4);
        assert_eq!(
            refused["local_tree_digest"],
            pushed_tree.digest().to_string()
        );
        assert_eq!(refused["ignored"], serde_json::json!(["state"]));
        assert_eq!(
            saved.lock().expect("lock").len(),
            2,
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
        let refused = DraftPull {
            draft_id: "studio".into(),
            dir: root,
            version: None,
            json: false,
        }
        .run(&client)
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

        let compiled = DraftPreview {
            draft_id: "calibrate".into(),
            version: None,
            json: false,
        }
        .run(&client)
        .await
        .expect("preview");
        assert!(compiled.contains("version 9 compiled"), "{compiled}");
        assert!(compiled.contains("sha256:bb"), "{compiled}");

        let launch = |digest: &str| DraftLaunch {
            draft_id: "calibrate".into(),
            params: BTreeMap::from([("repo".to_string(), "wren/packs".to_string())]),
            max_cost: 4.0,
            max_time: "30m".into(),
            schema_digest: Some(digest.to_string()),
            dispatch_target: None,
            provider: Some("plat-openai".into()),
            model: Some(" gpt-5.6-luna ".into()),
            rerun_of: None,
            from: Vec::new(),
            json: false,
        };
        let fired = launch("sha256:bb").run(&client).await.expect("launch");
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

        let stale = launch("sha256:aa")
            .run(&client)
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

        let graduated = DraftGraduate {
            draft_id: "calibrate".into(),
            repo: "wren/packs".into(),
            path: None,
            json: false,
        }
        .run(&client)
        .await
        .expect("graduate");
        assert!(
            graduated.contains("https://github.com/wren/packs/pull/7"),
            "{graduated}"
        );
    }

    #[tokio::test]
    async fn a_delete_answers_with_the_id_and_carries_a_404_through() {
        use axum::extract::Path as AxumPath;
        use axum::http::StatusCode;
        use axum::routing::get;

        async fn delete_draft(AxumPath(id): AxumPath<String>) -> StatusCode {
            if id == "gone-already" {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::NO_CONTENT
            }
        }

        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
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
    }

    /// A stand-in controller holding four drafts: two of alice's at edition 1, one of hers already
    /// at edition 2, and a retired one of bob's. It records each migrate request it is sent.
    async fn migrate_controller() -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::extract::{Path as AxumPath, State};
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use std::sync::{Arc, Mutex};

        type Seen = Arc<Mutex<Vec<Value>>>;

        async fn list() -> axum::Json<Value> {
            axum::Json(serde_json::json!([
                {"id": "notify", "owner": "user:alice", "latest_version": 4, "retired_at": null},
                {"id": "done", "owner": "user:alice", "latest_version": 9, "retired_at": null},
                {"id": "broken", "owner": "user:alice", "latest_version": 2, "retired_at": null},
                {"id": "old", "owner": "user:bob", "latest_version": 1,
                 "retired_at": "2026-10-01T00:00:00Z"},
            ]))
        }

        async fn migrate(
            State(seen): State<Seen>,
            AxumPath(id): AxumPath<String>,
            axum::Json(body): axum::Json<Value>,
        ) -> (StatusCode, axum::Json<Value>) {
            let mut request = body.clone();
            request["draft_id"] = Value::String(id.clone());
            seen.lock().expect("seen").push(request);
            let dry_run = body["dry_run"].as_bool().unwrap_or(true);
            match id.as_str() {
                "done" => (
                    StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "from": 2, "to": 2, "base_version": 9, "dry_run": dry_run,
                        "diffs": [], "changes": [], "findings": [], "saved": null,
                    })),
                ),
                "broken" => (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    axum::Json(serde_json::json!({
                        "error": "crucible.toml does not parse, so it cannot be migrated: missing field `agent`",
                    })),
                ),
                _ => {
                    let saved = (!dry_run).then(|| {
                        serde_json::json!({
                            "version": 5, "saved_by": "alice", "saved_at": "t",
                            "schema_digest": "sha256:aa", "diagnostics": [],
                        })
                    });
                    (
                        StatusCode::OK,
                        axum::Json(serde_json::json!({
                            "from": 1, "to": 2, "base_version": 4, "dry_run": dry_run,
                            "diffs": [{"path": "crucible.toml", "unified": "--- a/crucible.toml\n+++ b/crucible.toml\n@@ -1 +1,3 @@\n+edition = 2\n+\n [agent]\n"}],
                            "changes": [
                                {"kind": "edition", "table": null, "entry": null, "read_at": [],
                                 "message": "edition = 2 (was 1)"},
                                {"kind": "declared", "table": "[workflow]", "entry": "SLACK_WEBHOOK_URL?",
                                 "read_at": ["scripts/notify.py:3"],
                                 "message": "[workflow].env declares \"SLACK_WEBHOOK_URL?\", read at scripts/notify.py:3"},
                            ],
                            "findings": [
                                {"kind": "unattributed_reads", "path": "tools/backfill.py",
                                 "message": "tools/backfill.py reads BACKFILL_SINCE, and no host command was found to run it; if one does, declare them in that command's table"},
                            ],
                            "saved": saved,
                        })),
                    )
                }
            }
        }

        async fn detail(AxumPath(id): AxumPath<String>) -> axum::Json<Value> {
            axum::Json(serde_json::json!({
                "draft": {"id": id, "owner": "user:alice", "latest_version": 2, "retired_at": null},
                "versions": [],
            }))
        }

        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/healthz", get(|| async { "ok" }))
            .route("/api/playbook-drafts", get(list))
            .route("/api/playbook-drafts/{id}", get(detail))
            .route("/api/playbook-drafts/{id}/migrate", post(migrate))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        (url, seen)
    }

    fn sweep(owner: Option<&str>, apply: bool) -> DraftMigrate {
        DraftMigrate {
            draft_id: None,
            all: true,
            owner: owner.map(str::to_string),
            to: None,
            dry_run: false,
            apply,
            json: false,
        }
    }

    /// A sweep is a dry run unless asked: every unretired draft of the owner is migrated against
    /// its newest version, one already at the edition is skipped, and a refusal is reported
    /// without stopping the rest.
    #[tokio::test]
    async fn a_sweep_previews_each_draft_and_skips_the_ones_already_migrated() {
        let (url, seen) = migrate_controller().await;
        let client = test_client(&url);

        let printed = sweep(Some("user:alice"), false)
            .run(&client)
            .await
            .expect("sweep");
        let notify_url = studio_url(&client, "notify");
        assert_eq!(
            printed,
            format!(
                "dry run: nothing saved; --apply saves each migration as its draft's next version\n\
                 == draft notify (version 4): edition 1 -> 2\n\
                 --- a/crucible.toml\n+++ b/crucible.toml\n@@ -1 +1,3 @@\n+edition = 2\n+\n [agent]\n\
                 change: edition = 2 (was 1)\n\
                 change: [workflow].env declares \"SLACK_WEBHOOK_URL?\", read at scripts/notify.py:3\n\
                 review: tools/backfill.py reads BACKFILL_SINCE, and no host command was found to \
                 run it; if one does, declare them in that command's table\n\
                 studio: {notify_url}\n\
                 skipped: done (already at edition 2)\n\
                 refused: broken (HTTP 422 Unprocessable Entity: crucible.toml does not parse, so it \
                 cannot be migrated: missing field `agent`)\n\
                 1 migrated, 1 skipped, 1 refused\n"
            )
        );
        let requests = seen.lock().expect("seen").clone();
        assert_eq!(
            requests,
            [
                serde_json::json!({"draft_id": "broken", "base_version": 2, "dry_run": true}),
                serde_json::json!({"draft_id": "done", "base_version": 9, "dry_run": true}),
                serde_json::json!({"draft_id": "notify", "base_version": 4, "dry_run": true}),
            ]
        );
    }

    /// `--apply` saves, and without `--owner` a sweep covers every draft listed, retired ones
    /// reported as passed over rather than sent.
    #[tokio::test]
    async fn applying_a_sweep_saves_and_reports_retired_drafts_as_skipped() {
        let (url, seen) = migrate_controller().await;
        let client = test_client(&url);

        let op = DraftMigrate {
            json: true,
            ..sweep(None, true)
        };
        let raw: Value =
            serde_json::from_str(&op.run(&client).await.expect("sweep")).expect("json");
        assert_eq!(raw["dry_run"], false);
        assert_eq!(raw["migrated"][0]["draft_id"], "notify");
        assert_eq!(raw["migrated"][0]["saved"]["version"], 5);
        assert_eq!(
            raw["skipped"],
            serde_json::json!([
                {"draft_id": "old", "reason": "retired"},
                {"draft_id": "done", "reason": "already at edition 2"},
            ])
        );
        assert!(
            seen.lock()
                .expect("seen")
                .iter()
                .all(|r| r["dry_run"] == false && r["draft_id"] != "old"),
            "{seen:?}"
        );
    }

    /// One named draft's refusal is the command's error, not a line in a report.
    #[tokio::test]
    async fn one_named_draft_that_is_refused_fails_the_command() {
        let (url, _seen) = migrate_controller().await;
        let client = test_client(&url);
        let op = DraftMigrate {
            draft_id: Some("broken".to_string()),
            all: false,
            owner: None,
            to: Some(2),
            dry_run: true,
            apply: false,
            json: false,
        };
        let error = op.call(&client).await.expect_err("refused");
        assert!(
            error.to_string().starts_with(
                "draft broken: HTTP 422 Unprocessable Entity: crucible.toml does not parse"
            ),
            "{error}"
        );
    }
}
