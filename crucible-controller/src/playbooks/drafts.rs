//! Draft packs: a pack authored in the controller instead of imported from a git pin. A draft is
//! a versioned blob — every save stores the editor's `{path: content}` map as a pack tree, compiles
//! it with the pinned engine, and stores the tree and its tarball beside whatever the engine made
//! of it. A save that does not compile is still a version: the diagnostics are the payload the
//! studio renders, not an error that loses the bytes.
//!
//! Drafts never enter the registry. [`graduate`] exports one as a PR over the same branch-pair
//! push the scope-pack approval uses, and [`retire_matching`] stamps the draft retired once the
//! merged pack is registered from the repo/path it was graduated to.

#![allow(clippy::disallowed_macros)]

use crate::playbooks::pack_trees::PACK_COLS;
use crate::playbooks::plan_graph::WorkflowGraphDto;
use crate::playbooks::registry::{RegisterError, validate_id, validate_path};
use anyhow::{Context, Result};
use crucible_contract::pack_tree::{PackTree, TreeDigest};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use std::path::Path;

/// How many files one draft may hold. A playbook pack is a manifest, a workflow source and a
/// handful of skills; a save past this is not one.
pub(crate) const MAX_DRAFT_FILES: usize = 128;

/// How large one draft file may be, before gzip.
pub(crate) const MAX_DRAFT_FILE_BYTES: usize = 512 * 1024;

/// The largest draft save request body. Above axum's 2 MiB default so a compressible pack under
/// the delivery budget can be saved.
pub(crate) const MAX_DRAFT_SAVE_BYTES: usize = 16 * 1024 * 1024;

/// The manifest a draft starts from when no template seeds it.
pub(crate) const SKELETON_MANIFEST: &str =
    "[agent]\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n";

/// The workflow a draft starts from: the smallest graph that compiles, so the studio opens on a
/// green preview rather than on a diagnostic.
pub(crate) const SKELETON_WORKFLOW: &str = concat!(
    "params = {}\n",
    "\n",
    "hello = command(name = \"hello\", run = \"echo hello\")\n",
    "\n",
    "workflow(type = \"playbook\", tasks = [hello], result = hello)\n",
);

/// Why a draft operation was refused. The API maps [`NotFound`](DraftError::NotFound) to 404,
/// [`Conflict`](DraftError::Conflict) and [`StaleBase`](DraftError::StaleBase) to 409,
/// [`Invalid`](DraftError::Invalid) to 422,
/// [`Push`](DraftError::Push) to 502, and anything else to 500.
#[derive(Debug, thiserror::Error)]
pub enum DraftError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    StaleBase(StaleBase),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Push(String),
    #[error(transparent)]
    Internal(anyhow::Error),
}

/// An [`Unconvertible`](crate::playbooks::pack_trees::Unconvertible) pack is refused as invalid,
/// naming its reason; anything else is internal.
impl From<anyhow::Error> for DraftError {
    fn from(e: anyhow::Error) -> Self {
        if e.downcast_ref::<crate::playbooks::pack_trees::Unconvertible>()
            .is_some()
        {
            DraftError::Invalid(format!("{e:#}"))
        } else {
            DraftError::Internal(e)
        }
    }
}

impl From<crate::playbooks::packs::ReadTreeError> for DraftError {
    fn from(e: crate::playbooks::packs::ReadTreeError) -> Self {
        match e {
            crate::playbooks::packs::ReadTreeError::NotText(rel) => DraftError::Invalid(format!(
                "{rel} is not text, so it cannot be edited as a draft"
            )),
            crate::playbooks::packs::ReadTreeError::NotAPack(e) => {
                DraftError::Invalid(e.to_string())
            }
        }
    }
}

/// A save whose base is no longer the newest version: what the writer edited from, what overtook
/// it, and who put it there. The refusal body the studio turns into its merge prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StaleBase {
    pub base_version: i64,
    pub current_version: i64,
    pub saved_by: Option<String>,
    pub saved_at: String,
}

impl std::fmt::Display for StaleBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "this save edited version {}, but {} saved version {} at {}; re-read that version and merge",
            self.base_version,
            self.saved_by.as_deref().unwrap_or("someone else"),
            self.current_version,
            self.saved_at
        )
    }
}

impl From<crate::playbooks::packs::PackRefusal> for DraftError {
    fn from(e: crate::playbooks::packs::PackRefusal) -> Self {
        match e {
            crate::playbooks::packs::PackRefusal::Encode(e) => Self::Internal(e.into()),
            refusal => Self::Invalid(refusal.to_string()),
        }
    }
}

impl From<RegisterError> for DraftError {
    fn from(e: RegisterError) -> Self {
        match e {
            RegisterError::Invalid(m) | RegisterError::Compile(m) => DraftError::Invalid(m),
            RegisterError::Fetch(m) => DraftError::Push(m),
            RegisterError::RevMoved(rev) => DraftError::Conflict(format!("the ref moved to {rev}")),
            RegisterError::Conflict(m) => DraftError::Conflict(m),
            e @ RegisterError::ExposureChanged { .. } => DraftError::Conflict(e.to_string()),
            RegisterError::Internal(e) => DraftError::from(e),
        }
    }
}

/// What a diagnostic is about. A save can compile cleanly and still be undispatchable, so the two
/// are separate verdicts rather than one undifferentiated list: `compile` means the engine made
/// nothing launchable of the tree, `dispatch` means it compiled and this deployment cannot run it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticKind {
    #[default]
    Compile,
    Dispatch,
}

/// One complaint about a draft, anchored. `file`/`line`/`col` are parsed out of the engine's own
/// `file:line:col: …` prefix and rewritten relative to the pack root, so the editor can put the
/// message on the line that produced it; `message` stays the engine's text verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Diagnostic {
    pub file: Option<String>,
    pub line: Option<u32>,
    pub col: Option<u32>,
    pub message: String,
    /// Absent in versions stored before dispatch diagnostics existed, which were all compile.
    #[serde(default)]
    pub kind: DiagnosticKind,
}

/// Split an engine diagnostic into its anchor and its text. The engine prints the source path it
/// was handed, which is the scratch tree's absolute path, so `pack_root` is stripped back off, and
/// it spans a column range (`2:1-6`) where a stub prints one column — both anchor to the line.
fn parse_diagnostic(raw: &str, pack_root: &Path) -> Diagnostic {
    let message = raw.trim().to_string();
    let mut anchor = Diagnostic {
        file: None,
        line: None,
        col: None,
        message: message.clone(),
        kind: DiagnosticKind::Compile,
    };
    let head = message.lines().next().unwrap_or_default();
    let Some((path, line, col)) = head.split_whitespace().find_map(anchor_of) else {
        return anchor;
    };
    let relative = Path::new(path)
        .strip_prefix(pack_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string());
    anchor.file = Some(relative);
    anchor.line = Some(line);
    anchor.col = Some(col);
    anchor
}

/// `<path>:<line>:<col>` out of one whitespace-separated token, with a trailing `:` and a column
/// range both tolerated.
fn anchor_of(token: &str) -> Option<(&str, u32, u32)> {
    let token = token.trim_end_matches(':');
    let (front, col) = token.rsplit_once(':')?;
    let (path, line) = front.rsplit_once(':')?;
    let col = col.split('-').next()?;
    if path.is_empty() {
        return None;
    }
    Some((path, line.parse().ok()?, col.parse().ok()?))
}

/// What one save's `[agent]` earns when it is judged against where this deployment would dispatch
/// it. Empty for a workflow that spawns no agent turn, and for a save the engine made no graph of
/// — nothing there is launchable yet, so the compile diagnostics are the whole story.
///
/// Derived per request rather than stored with the version: the verdict is half deployment
/// configuration, and a version row that baked it in would answer for the controller that wrote
/// it rather than the one being asked.
pub fn dispatch_diagnostics(
    graph: Option<&WorkflowGraphDto>,
    agent: Option<&crate::playbooks::dispatch::PackAgent>,
    cap: &crate::playbooks::dispatch::DispatchCapability,
) -> Vec<Diagnostic> {
    let Some(graph) = graph else {
        return Vec::new();
    };
    if !graph
        .nodes
        .iter()
        .any(|n| n.kind == crate::playbooks::plan_graph::TaskKind::Agent)
    {
        return Vec::new();
    }
    let Some(agent) = agent else {
        return Vec::new();
    };
    cap.spawn_defect(agent)
        .into_iter()
        .map(|defect| Diagnostic {
            file: Some("crucible.toml".to_string()),
            line: None,
            col: None,
            message: defect.to_string(),
            kind: DiagnosticKind::Dispatch,
        })
        .collect()
}

/// What a draft is based on: a registered pack, or the import row whose frozen tarball seeded it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginKind {
    Playbook,
    Import,
}

impl OriginKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OriginKind::Playbook => "playbook",
            OriginKind::Import => "import",
        }
    }
}

/// A draft's origin, resolved against where that pack lives now. `rev` and `digest` are what the
/// draft was seeded from and `current_rev` and `current_digest` are what the origin serves today:
/// a registry re-pin moves them apart, which is what the studio offers a rebase against. An
/// import's bytes are frozen, so its two pins are the same one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftOrigin {
    pub kind: OriginKind,
    pub playbook: Option<String>,
    pub import_id: Option<String>,
    pub repo: Option<String>,
    pub path: Option<String>,
    pub rev: Option<String>,
    pub current_rev: Option<String>,
    pub digest: Option<String>,
    pub current_digest: Option<String>,
}

impl DraftOrigin {
    /// Whether the origin pack re-pinned under the draft: a different tree, or a different rev
    /// when either side has no tree recorded.
    pub fn moved(&self) -> bool {
        let pins = match (self.digest.as_deref(), self.current_digest.as_deref()) {
            (Some(seeded), Some(current)) => Some((seeded, current)),
            _ => self.rev.as_deref().zip(self.current_rev.as_deref()),
        };
        pins.is_some_and(|(seeded, current)| seeded != current)
    }

    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Option<Self>> {
        let playbook: Option<String> = row.try_get("origin_playbook")?;
        let import_id: Option<String> = row.try_get("origin_import")?;
        let rev: Option<String> = row.try_get("origin_rev")?;
        let digest: Option<String> = row.try_get("origin_digest")?;
        if let Some(playbook) = playbook {
            return Ok(Some(DraftOrigin {
                kind: OriginKind::Playbook,
                playbook: Some(playbook),
                import_id: None,
                repo: row.try_get("origin_playbook_repo")?,
                path: row.try_get("origin_playbook_path")?,
                rev,
                current_rev: row.try_get("origin_current_rev")?,
                digest,
                current_digest: row.try_get("origin_current_digest")?,
            }));
        }
        let Some(import_id) = import_id else {
            return Ok(None);
        };
        Ok(Some(DraftOrigin {
            kind: OriginKind::Import,
            playbook: None,
            import_id: Some(import_id),
            repo: row.try_get("origin_import_repo")?,
            path: row.try_get("origin_import_path")?,
            current_rev: rev.clone(),
            rev,
            current_digest: digest.clone(),
            digest,
        }))
    }
}

/// Every draft column the studio reads, joined to whatever its origin is now.
const DRAFT_COLUMNS: &str = "d.id, d.description, d.origin_playbook, d.origin_rev, \
                             d.origin_digest, d.origin_import, d.graduation_repo, \
                             d.graduation_path, d.graduation_pr_url, d.retired_at, d.owner, \
                             d.created_by, d.created_at, d.updated_at, \
                             p.repo AS origin_playbook_repo, p.path AS origin_playbook_path, \
                             p.rev AS origin_current_rev, p.tree_digest AS origin_current_digest, \
                             i.repo AS origin_import_repo, i.path AS origin_import_path, \
                             sp.id AS published_playbook";

const DRAFT_JOINS: &str = "FROM playbook_drafts d \
                           LEFT JOIN playbooks p ON p.id = d.origin_playbook \
                           LEFT JOIN pack_imports i ON i.id = d.origin_import \
                           LEFT JOIN playbooks sp ON sp.source_draft = d.id";

/// A draft's metadata row, without any version's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRow {
    pub id: String,
    pub description: String,
    pub origin: Option<DraftOrigin>,
    pub graduation_repo: Option<String>,
    pub graduation_path: Option<String>,
    pub graduation_pr_url: Option<String>,
    /// The playbook this draft was last published into.
    pub published_playbook: Option<String>,
    pub retired_at: Option<String>,
    pub owner: crate::authz::model::Principal,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl DraftRow {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self> {
        Ok(DraftRow {
            id: row.try_get("id")?,
            description: row.try_get("description")?,
            origin: DraftOrigin::from_row(row)?,
            graduation_repo: row.try_get("graduation_repo")?,
            graduation_path: row.try_get("graduation_path")?,
            graduation_pr_url: row.try_get("graduation_pr_url")?,
            published_playbook: row.try_get("published_playbook")?,
            retired_at: row.try_get("retired_at")?,
            owner: crate::authz::model::Principal::parse(&row.try_get::<String, _>("owner")?)?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// One stored save, without its files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftVersionRow {
    pub version: i64,
    /// The stored tree; `None` until startup conversion reaches a row an older controller wrote.
    pub tree_digest: Option<TreeDigest>,
    pub schema_digest: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub core_rev: String,
    pub created_by: Option<String>,
    pub created_at: String,
}

impl DraftVersionRow {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self> {
        let diagnostics: serde_json::Value = row.try_get("diagnostics")?;
        Ok(DraftVersionRow {
            version: row.try_get("version")?,
            tree_digest: row
                .try_get::<Option<String>, _>("tree_digest")?
                .map(TreeDigest::try_from)
                .transpose()
                .map_err(anyhow::Error::msg)?,
            schema_digest: row.try_get("schema_digest")?,
            diagnostics: serde_json::from_value(diagnostics)
                .context("decoding stored draft diagnostics")?,
            core_rev: row.try_get("core_rev")?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

/// A draft as the drafts listing shows it: its metadata plus the state of its newest save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSummary {
    pub draft: DraftRow,
    pub latest_version: i64,
    /// True when the newest save compiled, i.e. it has a schema and a graph to launch from.
    pub compiles: bool,
    pub diagnostics: usize,
}

/// What one save landed: the version number, who put it there, and everything the engine made of
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedVersion {
    pub version: i64,
    pub saved_by: Option<String>,
    pub saved_at: String,
    pub params_schema: Option<serde_json::Value>,
    pub schema_digest: Option<String>,
    pub graph: Option<WorkflowGraphDto>,
    pub diagnostics: Vec<Diagnostic>,
    /// The substrate this save's `[agent]` asks for; `None` when its manifest did not parse.
    pub agent: Option<crate::playbooks::dispatch::PackAgent>,
}

/// The newest save's compiled state, as the launch path reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatestVersion {
    pub version: i64,
    pub params_schema: Option<serde_json::Value>,
    pub schema_digest: Option<String>,
    pub agent: Option<crate::playbooks::dispatch::PackAgent>,
}

/// Validate a draft's file map and write it to a scratch tree. Paths are the same relative-inside
/// shape a pack path is, so nothing a hostile save names can land outside the tree.
fn write_tree(files: &BTreeMap<String, String>, root: &Path) -> Result<(), DraftError> {
    if files.is_empty() {
        return Err(DraftError::Invalid(
            "a draft holds at least one file".to_string(),
        ));
    }
    if files.len() > MAX_DRAFT_FILES {
        return Err(DraftError::Invalid(format!(
            "a draft holds at most {MAX_DRAFT_FILES} files, not {}",
            files.len()
        )));
    }
    for (path, content) in files {
        if path.trim().is_empty() {
            return Err(DraftError::Invalid("a draft file needs a path".to_string()));
        }
        let pack_path: crucible_contract::pack_tree::PackFilePath = path.parse().map_err(|_| {
            DraftError::Invalid(format!("{path:?} is not a relative path inside the pack"))
        })?;
        if pack_path.is_excluded() {
            return Err(DraftError::Invalid(format!(
                "{path} is under {}, which a pack never carries",
                crucible_contract::pack_tree::EXCLUDED_SEGMENTS.join(", ")
            )));
        }
        if content.len() > MAX_DRAFT_FILE_BYTES {
            return Err(DraftError::Invalid(format!(
                "{path} is {} bytes, over the {MAX_DRAFT_FILE_BYTES}-byte per-file limit",
                content.len()
            )));
        }
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {} in the draft tree", parent.display()))
                .map_err(DraftError::Internal)?;
        }
        std::fs::write(&target, content)
            .with_context(|| format!("writing {path} into the draft tree"))
            .map_err(DraftError::Internal)?;
    }
    Ok(())
}

/// What the pinned engine made of one draft tree.
struct CompiledTree {
    params_schema: Option<serde_json::Value>,
    schema_digest: Option<String>,
    graph: Option<WorkflowGraphDto>,
    diagnostics: Vec<Diagnostic>,
    agent: Option<crate::playbooks::dispatch::PackAgent>,
}

/// Compile a draft tree with the pinned engine. Blocking.
fn compile_tree(root: &Path) -> CompiledTree {
    match crate::playbooks::preview::preview_pack(
        root,
        &BTreeMap::new(),
        crate::playbooks::preview::Unvalued::Placeholder,
    ) {
        Ok(preview) => CompiledTree {
            params_schema: preview.params_schema,
            schema_digest: preview.schema_digest,
            graph: preview.graph,
            diagnostics: preview
                .diagnostics
                .iter()
                .map(|d| parse_diagnostic(d, root))
                .collect(),
            agent: preview.agent,
        },
        // A tree that is not a pack at all is a diagnostic here, not a refusal: mid-edit, a draft's
        // manifest is allowed to be wrong.
        Err(e) => CompiledTree {
            params_schema: None,
            schema_digest: None,
            graph: None,
            diagnostics: vec![parse_diagnostic(&format!("{e}"), root)],
            agent: crate::playbooks::dispatch::pack_agent(root).ok(),
        },
    }
}

/// Where a draft's first version comes from, and with it what the draft's origin is.
#[derive(Debug, Clone, Copy)]
pub enum DraftSeed<'a> {
    /// The smallest pack that compiles. No origin.
    Skeleton,
    /// A registered playbook's stored pack, by registry id.
    Template {
        id: &'a str,
        expected_rev: Option<&'a str>,
        expected_digest: Option<&'a str>,
    },
    /// A pending import's frozen tree, at the rev it was taken at.
    Import { id: &'a str, tree: &'a PackTree },
}

/// The origin columns one create writes.
#[derive(Debug, Default)]
struct SeededOrigin {
    playbook: Option<String>,
    import_id: Option<String>,
    rev: Option<String>,
    digest: Option<String>,
}

/// The refusal for a template pin `supplied` that the registry no longer serves: superseded,
/// naming its replacement, when it is a pre-tree digest of a tree `template` has held, and
/// `otherwise` for anything else, so an unknown pin and a superseded one the template never held
/// read alike. The caller may read `template`.
async fn superseded_or(
    pool: &PgPool,
    template: &str,
    supplied: &str,
    otherwise: impl FnOnce() -> String,
) -> DraftError {
    match crate::playbooks::pack_trees::superseded_in(pool, template, supplied).await {
        Ok(Some(replacement)) => DraftError::Conflict(format!(
            "playbook {template:?} revision {supplied} is superseded by {replacement}; reload \
             before cloning"
        )),
        Ok(None) => DraftError::Conflict(otherwise()),
        Err(e) => DraftError::Internal(e),
    }
}

/// Whether a draft can be created under `id`: a valid slug, a description, and an id the registry
/// has not already taken. The from-git create runs this before it clones anything, so a name
/// collision costs no fetch; [`create`] runs it again and its insert settles the race.
pub async fn ensure_available(
    pool: &PgPool,
    id: &str,
    description: &str,
) -> Result<(), DraftError> {
    validate_id(id).map_err(DraftError::Invalid)?;
    if description.trim().is_empty() {
        return Err(DraftError::Invalid(
            "description must be non-empty".to_string(),
        ));
    }
    if crate::playbooks::registry::get(pool, id)
        .await
        .map_err(DraftError::Internal)?
        .is_some()
    {
        return Err(DraftError::Conflict(format!(
            "{id} is a registered playbook; a draft shares the launch-key namespace, so pick another id"
        )));
    }
    if get(pool, id).await.map_err(DraftError::Internal)?.is_some() {
        return Err(DraftError::Conflict(format!("draft {id:?} already exists")));
    }
    Ok(())
}

/// Create a draft, seeding version 1 from `seed`.
pub async fn create(
    pool: &PgPool,
    id: &str,
    description: &str,
    seed: DraftSeed<'_>,
    actor: Option<&str>,
    owner: &crate::authz::model::Principal,
) -> Result<SavedVersion, DraftError> {
    ensure_available(pool, id, description).await?;

    let (files, origin) = match seed {
        DraftSeed::Template {
            id: template,
            expected_rev,
            expected_digest,
        } => {
            let registered = crate::playbooks::registry::get(pool, template)
                .await
                .map_err(DraftError::Internal)?
                .ok_or_else(|| DraftError::NotFound(format!("no playbook {template:?}")))?;
            if let Some(rev) = expected_rev
                && rev != registered.rev
            {
                return Err(superseded_or(pool, template, rev, || {
                    format!(
                        "playbook {template:?} moved from inspected revision {rev} to {}; \
                         reload before cloning",
                        registered.rev
                    )
                })
                .await);
            }
            let pinned = expected_digest
                .map(str::to_string)
                .or_else(|| registered.tree_digest.map(|t| t.to_string()));
            let tree = crate::playbooks::registry::pack_at_rev(
                pool,
                template,
                &registered.rev,
                pinned.as_deref(),
            )
            .await?;
            let Some(tree) = tree else {
                let moved =
                    || format!("playbook {template:?} moved while cloning; reload before retrying");
                return Err(match expected_digest {
                    Some(digest) => superseded_or(pool, template, digest, moved).await,
                    None => DraftError::Conflict(moved()),
                });
            };
            (
                crate::playbooks::packs::text_files(tree)?,
                SeededOrigin {
                    playbook: Some(registered.id),
                    import_id: None,
                    rev: Some(registered.rev),
                    digest: pinned,
                },
            )
        }
        DraftSeed::Import { id: import, tree } => {
            let pins: Option<(String, Option<String>)> =
                sqlx::query_as("SELECT rev, tree_digest FROM pack_imports WHERE id = $1")
                    .bind(import)
                    .fetch_optional(pool)
                    .await
                    .context("reading the rev a draft is seeded at")
                    .map_err(DraftError::Internal)?;
            let (rev, digest) = pins.unzip();
            (
                crate::playbooks::packs::text_files(tree.clone())?,
                SeededOrigin {
                    playbook: None,
                    import_id: Some(import.to_string()),
                    rev,
                    digest: digest.flatten(),
                },
            )
        }
        DraftSeed::Skeleton => (
            BTreeMap::from([
                ("crucible.toml".to_string(), SKELETON_MANIFEST.to_string()),
                ("workflow.star".to_string(), SKELETON_WORKFLOW.to_string()),
            ]),
            SeededOrigin::default(),
        ),
    };

    let now = crate::clock::now_rfc3339();
    let inserted = sqlx::query(
        r#"INSERT INTO playbook_drafts (id, description, origin_playbook, origin_import, origin_rev,
                                        created_by, created_at, updated_at, owner, origin_digest)
           VALUES ($1, $2, $3, $6, $7, $4, $5, $5, $8, $9) ON CONFLICT (id) DO NOTHING"#,
    )
    .bind(id)
    .bind(description.trim())
    .bind(&origin.playbook)
    .bind(actor)
    .bind(&now)
    .bind(&origin.import_id)
    .bind(&origin.rev)
    .bind(owner.to_string())
    .bind(&origin.digest)
    .execute(pool)
    .await
    .context("creating a playbook draft")
    .map_err(DraftError::Internal)?;
    if inserted.rows_affected() == 0 {
        return Err(DraftError::Conflict(format!("draft {id:?} already exists")));
    }

    save_version(pool, id, files, actor, None).await
}

/// Store a save: store the file map's tree, compile it, and write the next version row either
/// way. A `base_version` that is not the newest save is refused with [`DraftError::StaleBase`]
/// carrying the version that overtook it, so the writer re-reads and merges instead of clobbering
/// the other editor; `None` appends onto whatever is latest.
pub async fn save_version(
    pool: &PgPool,
    id: &str,
    files: BTreeMap<String, String>,
    actor: Option<&str>,
    base_version: Option<i64>,
) -> Result<SavedVersion, DraftError> {
    let draft = get(pool, id)
        .await
        .map_err(DraftError::Internal)?
        .ok_or_else(|| DraftError::NotFound(format!("no draft {id:?}")))?;
    if draft.retired_at.is_some() {
        return Err(DraftError::Conflict(format!(
            "draft {id} retired when its graduated pack was imported"
        )));
    }

    let compiled = tokio::task::spawn_blocking(move || {
        let scratch = tempfile::tempdir()
            .context("draft compile scratch dir")
            .map_err(DraftError::Internal)?;
        let root = scratch.path().join("pack");
        std::fs::create_dir_all(&root)
            .context("creating the draft tree")
            .map_err(DraftError::Internal)?;
        write_tree(&files, &root)?;
        let pack = crate::playbooks::packs::deliverable(&root)?;
        Ok::<_, DraftError>((pack, compile_tree(&root)))
    })
    .await
    .context("joining the draft compile worker")
    .map_err(DraftError::Internal)?;
    let (
        pack,
        CompiledTree {
            params_schema,
            schema_digest,
            graph,
            diagnostics,
            agent,
        },
    ) = compiled?;

    let core_rev = crate::playbooks::registry::core_rev().map_err(DraftError::Internal)?;
    let now = crate::clock::now_rfc3339();
    let graph_json = graph
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .context("serializing the draft graph")
        .map_err(DraftError::Internal)?;
    let diagnostics_json = serde_json::to_value(&diagnostics)
        .context("serializing the draft diagnostics")
        .map_err(DraftError::Internal)?;

    let mut tx = pool
        .begin()
        .await
        .context("opening the draft save transaction")
        .map_err(DraftError::Internal)?;
    let locked =
        sqlx::query("SELECT retired_at, updated_at FROM playbook_drafts WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .context("locking the draft for a save")
            .map_err(DraftError::Internal)?
            .ok_or_else(|| DraftError::NotFound(format!("no draft {id:?}")))?;
    if locked
        .try_get::<Option<String>, _>("retired_at")
        .map_err(|e| DraftError::Internal(e.into()))?
        .is_some()
    {
        return Err(DraftError::Conflict(format!(
            "draft {id} retired when its graduated pack was imported"
        )));
    }
    let head = sqlx::query(
        r#"SELECT version, created_by, created_at FROM playbook_draft_versions
           WHERE draft_id = $1 ORDER BY version DESC LIMIT 1"#,
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .context("reading the draft head for a save")
    .map_err(DraftError::Internal)?;
    let current: i64 = match head.as_ref() {
        Some(row) => row
            .try_get("version")
            .map_err(|e| DraftError::Internal(e.into()))?,
        None => 0,
    };
    if let Some(base) = base_version
        && base != current
    {
        let saved_at = match head.as_ref() {
            Some(row) => row
                .try_get("created_at")
                .map_err(|e| DraftError::Internal(e.into()))?,
            None => locked
                .try_get("updated_at")
                .map_err(|e| DraftError::Internal(e.into()))?,
        };
        let saved_by = match head.as_ref() {
            Some(row) => row
                .try_get("created_by")
                .map_err(|e| DraftError::Internal(e.into()))?,
            None => None,
        };
        return Err(DraftError::StaleBase(StaleBase {
            base_version: base,
            current_version: current,
            saved_by,
            saved_at,
        }));
    }
    let tree_digest = crate::playbooks::pack_trees::put_tree(&mut tx, pack.pack())
        .await
        .map_err(DraftError::Internal)?;
    let version: i64 = sqlx::query_scalar(
        r#"INSERT INTO playbook_draft_versions (draft_id, version, tar_gz, tar_digest, tar_bytes,
                                                params_schema, schema_digest, graph, diagnostics,
                                                agent_backend, agent_sandbox_image, core_rev,
                                                created_by, created_at, agent_requirements,
                                                tree_digest)
           VALUES ($1, $14, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $15, $16)
           RETURNING version"#,
    )
    .bind(id)
    .bind(pack.tarball())
    .bind(pack.tarball_digest())
    .bind(
        i64::try_from(pack.tarball().len())
            .context("pack size")
            .map_err(DraftError::Internal)?,
    )
    .bind(&params_schema)
    .bind(&schema_digest)
    .bind(&graph_json)
    .bind(&diagnostics_json)
    .bind(agent.as_ref().map(|a| a.backend.clone()))
    .bind(agent.as_ref().and_then(|a| a.sandbox_image.clone()))
    .bind(&core_rev)
    .bind(actor)
    .bind(&now)
    .bind(current + 1)
    .bind(agent.as_ref().map(|a| a.requirements_json()))
    .bind(tree_digest.as_str())
    .fetch_one(&mut *tx)
    .await
    .context("storing a draft version")
    .map_err(DraftError::Internal)?;
    sqlx::query("UPDATE playbook_drafts SET updated_at = $2 WHERE id = $1")
        .bind(id)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .context("stamping the draft")
        .map_err(DraftError::Internal)?;
    if schema_digest.is_some() {
        sqlx::query(
            "UPDATE playbook_standing_launches SET eligible_draft_version = $2, updated_at = $3 \
             WHERE target_kind = 'draft_head' AND playbook = $1",
        )
        .bind(id)
        .bind(version)
        .bind(&now)
        .execute(&mut *tx)
        .await
        .context("advancing draft-head schedule eligibility")
        .map_err(DraftError::Internal)?;
    }
    tx.commit()
        .await
        .context("committing the draft save")
        .map_err(DraftError::Internal)?;

    Ok(SavedVersion {
        version,
        saved_by: actor.map(str::to_string),
        saved_at: now,
        params_schema,
        schema_digest,
        graph,
        diagnostics,
        agent,
    })
}

/// Every draft, newest first, each with the state of its newest save.
pub async fn list(pool: &PgPool) -> Result<Vec<DraftSummary>> {
    let rows = sqlx::query(const_format::formatcp!(
        "SELECT {DRAFT_COLUMNS}, v.version, v.schema_digest, v.diagnostics \
         {DRAFT_JOINS} \
         LEFT JOIN LATERAL ( \
             SELECT version, schema_digest, diagnostics FROM playbook_draft_versions \
             WHERE draft_id = d.id ORDER BY version DESC LIMIT 1 \
         ) v ON TRUE \
         ORDER BY d.updated_at DESC, d.id"
    ))
    .fetch_all(pool)
    .await
    .context("listing playbook drafts")?;
    rows.iter()
        .map(|row| {
            let diagnostics: Option<serde_json::Value> = row.try_get("diagnostics")?;
            let diagnostics: Vec<Diagnostic> = diagnostics
                .map(serde_json::from_value)
                .transpose()
                .context("decoding stored draft diagnostics")?
                .unwrap_or_default();
            let schema_digest: Option<String> = row.try_get("schema_digest")?;
            Ok(DraftSummary {
                draft: DraftRow::from_row(row)?,
                latest_version: row.try_get::<Option<i64>, _>("version")?.unwrap_or(0),
                compiles: schema_digest.is_some(),
                diagnostics: diagnostics.len(),
            })
        })
        .collect()
}

/// One draft's metadata row.
pub async fn get(pool: &PgPool, id: &str) -> Result<Option<DraftRow>> {
    let row = sqlx::query(const_format::formatcp!(
        "SELECT {DRAFT_COLUMNS} {DRAFT_JOINS} WHERE d.id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("reading a playbook draft")?;
    row.as_ref().map(DraftRow::from_row).transpose()
}

/// A draft's save history, newest first.
pub async fn versions(pool: &PgPool, id: &str) -> Result<Vec<DraftVersionRow>> {
    let rows = sqlx::query(
        r#"SELECT version, tree_digest, schema_digest, diagnostics, core_rev, created_by, created_at
           FROM playbook_draft_versions WHERE draft_id = $1 ORDER BY version DESC"#,
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .context("listing draft versions")?;
    rows.iter().map(DraftVersionRow::from_row).collect()
}

/// The newest save's compiled state — what a launch is authorized against.
pub async fn latest(pool: &PgPool, id: &str) -> Result<Option<LatestVersion>> {
    let row = sqlx::query(
        r#"SELECT version, params_schema, schema_digest, agent_backend, agent_sandbox_image,
                  agent_requirements
           FROM playbook_draft_versions WHERE draft_id = $1 ORDER BY version DESC LIMIT 1"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("reading the newest draft version")?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(LatestVersion {
        version: row.try_get("version")?,
        params_schema: row.try_get("params_schema")?,
        schema_digest: row.try_get("schema_digest")?,
        agent: crate::playbooks::dispatch::agent_from_row(&row)?,
    }))
}

/// One stored version's compiled preview, for the studio's first paint.
pub async fn preview(
    pool: &PgPool,
    id: &str,
    version: Option<i64>,
) -> Result<Option<SavedVersion>> {
    let row = match version {
        Some(v) => sqlx::query(
            r#"SELECT version, created_by, created_at, params_schema, schema_digest, graph,
                      diagnostics, agent_backend, agent_sandbox_image, agent_requirements
               FROM playbook_draft_versions WHERE draft_id = $1 AND version = $2"#,
        )
        .bind(id)
        .bind(v),
        None => sqlx::query(
            r#"SELECT version, created_by, created_at, params_schema, schema_digest, graph,
                      diagnostics, agent_backend, agent_sandbox_image, agent_requirements
               FROM playbook_draft_versions WHERE draft_id = $1 ORDER BY version DESC LIMIT 1"#,
        )
        .bind(id),
    }
    .fetch_optional(pool)
    .await
    .context("reading a draft preview")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let graph: Option<serde_json::Value> = row.try_get("graph")?;
    let diagnostics: serde_json::Value = row.try_get("diagnostics")?;
    Ok(Some(SavedVersion {
        version: row.try_get("version")?,
        saved_by: row.try_get("created_by")?,
        saved_at: row.try_get("created_at")?,
        params_schema: row.try_get("params_schema")?,
        schema_digest: row.try_get("schema_digest")?,
        graph: graph
            .map(serde_json::from_value)
            .transpose()
            .context("decoding a stored draft graph")?,
        diagnostics: serde_json::from_value(diagnostics)
            .context("decoding stored draft diagnostics")?,
        agent: crate::playbooks::dispatch::agent_from_row(&row)?,
    }))
}

/// A stored version's files, with the version it is, who saved it, and what the engine said of it.
/// Everything the other editor needs to pick a save up mid-loop. `None` when the draft or the
/// version is unknown.
pub async fn files(
    pool: &PgPool,
    id: &str,
    version: Option<i64>,
) -> Result<Option<DraftFiles>, DraftError> {
    let Some((head, tree)) = version_pack(pool, id, version)
        .await
        .map_err(DraftError::Internal)?
    else {
        return Ok(None);
    };
    Ok(Some(DraftFiles {
        version: head.version,
        saved_by: head.saved_by,
        saved_at: head.saved_at,
        diagnostics: head.diagnostics,
        files: crate::playbooks::packs::text_files(tree)?,
    }))
}

/// Recompute the exposure of one stored draft version with the linked engine, from the exact bytes
/// a launch of it would run. `None` when the draft or the version is unknown.
pub async fn exposure_of(
    pool: &PgPool,
    id: &str,
    version: i64,
) -> Result<Option<crate::playbooks::exposure::Extraction>, DraftError> {
    let Some((_, tree)) = version_pack(pool, id, Some(version)).await? else {
        return Ok(None);
    };
    let extraction = tokio::task::spawn_blocking(move || {
        let pack = crate::playbooks::packs::materialize_tree(&tree)
            .map_err(crate::playbooks::exposure::ExtractError::Internal)?;
        crate::playbooks::exposure::extract(pack.path(), None)
    })
    .await
    .context("joining the draft exposure worker")
    .map_err(DraftError::Internal)?;
    match extraction {
        Ok(exposure) => Ok(Some(crate::playbooks::exposure::Extraction::Declared(
            exposure,
        ))),
        Err(crate::playbooks::exposure::ExtractError::Refused(message)) => {
            Err(DraftError::Invalid(format!(
                "the draft's exposure could not be extracted, so there is nothing to record what a \
                 launch of it may write: {message}"
            )))
        }
        Err(crate::playbooks::exposure::ExtractError::Internal(e)) => Err(DraftError::Internal(e)),
    }
}

/// One stored version's file map with the identity and diagnostics that came with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftFiles {
    pub version: i64,
    pub saved_by: Option<String>,
    pub saved_at: String,
    pub diagnostics: Vec<Diagnostic>,
    pub files: BTreeMap<String, String>,
}

/// Who saved a stored version, and when.
struct VersionHead {
    version: i64,
    saved_by: Option<String>,
    saved_at: String,
    diagnostics: Vec<Diagnostic>,
}

/// A stored version's pack, the newest when `version` is `None`.
async fn version_pack(
    pool: &PgPool,
    id: &str,
    version: Option<i64>,
) -> Result<Option<(VersionHead, PackTree)>> {
    let row = sqlx::query(const_format::formatcp!(
        "SELECT version, created_by, created_at, diagnostics, {PACK_COLS}
         FROM playbook_draft_versions WHERE draft_id = $1 AND ($2::BIGINT IS NULL OR version = $2)
         ORDER BY version DESC LIMIT 1"
    ))
    .bind(id)
    .bind(version)
    .fetch_optional(pool)
    .await
    .context("reading a draft version")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let version: i64 = row.try_get("version")?;
    let diagnostics: serde_json::Value = row.try_get("diagnostics")?;
    let head = VersionHead {
        version,
        saved_by: row.try_get("created_by")?,
        saved_at: row.try_get("created_at")?,
        diagnostics: serde_json::from_value(diagnostics)
            .context("decoding stored draft diagnostics")?,
    };
    let pack = crate::playbooks::pack_trees::load_row(pool, &row)
        .await
        .with_context(|| format!("reading draft {id} version {version}"))?;
    Ok(Some((head, pack)))
}

/// One origin pack's files as they stand now, for the rebase view: the studio diffs them against
/// the draft's own buffers when the origin re-pinned under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginFiles {
    pub kind: OriginKind,
    /// The pack the files were read from: a registry id, or an import id.
    pub reference: String,
    /// The rev those files are at.
    pub rev: Option<String>,
    pub files: BTreeMap<String, String>,
}

/// Read a draft's origin pack as it stands now. `None` when the draft is unknown or was started
/// from a skeleton; a registered origin that has since been deleted is a
/// [`NotFound`](DraftError::NotFound).
pub async fn origin_files(pool: &PgPool, id: &str) -> Result<Option<OriginFiles>, DraftError> {
    let Some(draft) = get(pool, id).await.map_err(DraftError::Internal)? else {
        return Ok(None);
    };
    let Some(origin) = draft.origin else {
        return Ok(None);
    };
    match origin.kind {
        OriginKind::Playbook => {
            let playbook = origin.playbook.ok_or_else(|| {
                DraftError::Internal(anyhow::anyhow!("origin without a playbook"))
            })?;
            let tree = crate::playbooks::registry::pack(pool, &playbook)
                .await?
                .ok_or_else(|| {
                    DraftError::NotFound(format!(
                        "draft {id} came from playbook {playbook}, which is no longer registered"
                    ))
                })?;
            Ok(Some(OriginFiles {
                kind: OriginKind::Playbook,
                reference: playbook,
                rev: origin.current_rev,
                files: crate::playbooks::packs::text_files(tree)?,
            }))
        }
        OriginKind::Import => {
            let import = origin.import_id.ok_or_else(|| {
                DraftError::Internal(anyhow::anyhow!("origin without an import id"))
            })?;
            let row = sqlx::query(const_format::formatcp!(
                "SELECT {} FROM pack_imports WHERE id = $1",
                crate::playbooks::pack_trees::PACK_COLS
            ))
            .bind(&import)
            .fetch_optional(pool)
            .await
            .context("reading the import a draft came from")?
            .ok_or_else(|| {
                DraftError::NotFound(format!(
                    "draft {id} came from import {import}, which no longer exists"
                ))
            })?;
            let tree = crate::playbooks::pack_trees::load_row(pool, &row)
                .await
                .with_context(|| format!("reading the frozen pack of import {import}"))?;
            Ok(Some(OriginFiles {
                kind: OriginKind::Import,
                reference: import,
                rev: origin.rev,
                files: crate::playbooks::packs::text_files(tree)?,
            }))
        }
    }
}

/// One stored version's pack, the newest when `version` is `None`. `None` when the draft or the
/// version is unknown.
pub async fn version_tree(
    pool: &PgPool,
    id: &str,
    version: Option<i64>,
) -> Result<Option<(i64, PackTree)>> {
    Ok(version_pack(pool, id, version)
        .await?
        .map(|(head, tree)| (head.version, tree)))
}

/// Drop a draft and every version it holds.
pub async fn delete(pool: &PgPool, id: &str) -> Result<bool> {
    let res = sqlx::query("DELETE FROM playbook_drafts WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .context("deleting a playbook draft")?;
    Ok(res.rows_affected() > 0)
}

/// Copy a draft version's tarball into `slug`'s `pack_tarballs` row, so a draft launch dispatches
/// through the same materialization a registered launch does. `false` when the version is unknown.
pub(crate) async fn copy_draft_pack_to<'e>(
    ex: impl sqlx::PgExecutor<'e>,
    id: &str,
    version: i64,
    slug: &str,
) -> Result<bool> {
    let res = sqlx::query(
        r#"INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at, tree_digest)
           SELECT $3, tar_gz, tar_digest, tar_bytes, $4, tree_digest FROM playbook_draft_versions
           WHERE draft_id = $1 AND version = $2
           ON CONFLICT (issue_slug) DO UPDATE SET
               tar_gz = excluded.tar_gz, digest = excluded.digest, bytes = excluded.bytes,
               created_at = excluded.created_at, tree_digest = excluded.tree_digest"#,
    )
    .bind(id)
    .bind(version)
    .bind(slug)
    .bind(crate::clock::now_rfc3339())
    .execute(ex)
    .await
    .context("copying a draft pack to a launch")?;
    Ok(res.rows_affected() > 0)
}

/// The newest version of draft `id` that compiled, with its pack.
pub async fn newest_compiling(pool: &PgPool, id: &str) -> Result<(i64, PackTree), DraftError> {
    let row = sqlx::query(const_format::formatcp!(
        "SELECT version, {PACK_COLS} FROM playbook_draft_versions
         WHERE draft_id = $1 AND schema_digest IS NOT NULL ORDER BY version DESC LIMIT 1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("reading the newest compiling draft version")
    .map_err(DraftError::Internal)?
    .ok_or_else(|| {
        DraftError::Invalid(format!(
            "draft {id} has never compiled, so there is nothing to export"
        ))
    })?;
    let version: i64 = row
        .try_get("version")
        .map_err(|e| DraftError::Internal(e.into()))?;
    let tree = crate::playbooks::pack_trees::load_row(pool, &row)
        .await
        .with_context(|| format!("reading draft {id} version {version}"))?;
    Ok((version, tree))
}

/// The playbook a publish of `draft` writes: the one asked for, else the one it last published
/// into, else the registered pack it was seeded from. The draft stays live after a publish, so the
/// playbook needs an id of its own.
pub fn publish_target(draft: &DraftRow, asked: Option<&str>) -> Result<String, DraftError> {
    if draft.retired_at.is_some() {
        return Err(DraftError::Conflict(format!(
            "draft {} retired when its graduated pack was imported",
            draft.id
        )));
    }
    let origin = draft.origin.as_ref().and_then(|o| o.playbook.as_deref());
    let target = asked
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .or(draft.published_playbook.as_deref())
        .or(origin)
        .ok_or_else(|| {
            DraftError::Invalid(format!(
                "draft {} has never been published; name the playbook to publish it as",
                draft.id
            ))
        })?;
    if target == draft.id {
        return Err(DraftError::Invalid(format!(
            "{target} is this draft's own id; the draft stays live after publishing, so the \
             playbook needs another"
        )));
    }
    validate_id(target).map_err(DraftError::Invalid)?;
    Ok(target.to_string())
}

/// Export a draft as a pull request: push its newest compiling version to `repo` under `path` as a
/// branch pair, and open the draft PR. Idempotent — a draft already graduated and not yet retired
/// keeps its stored url rather than opening a second PR.
pub async fn graduate(
    pool: &PgPool,
    id: &str,
    repo: &str,
    path: &str,
    token: Option<String>,
) -> Result<String, DraftError> {
    let draft = get(pool, id)
        .await
        .map_err(DraftError::Internal)?
        .ok_or_else(|| DraftError::NotFound(format!("no draft {id:?}")))?;
    if let Some(url) = draft.graduation_pr_url.as_deref()
        && draft.retired_at.is_none()
    {
        return Err(DraftError::Conflict(url.to_string()));
    }
    validate_path(path).map_err(DraftError::Invalid)?;
    if repo.trim().is_empty() || repo.starts_with('-') {
        return Err(DraftError::Invalid(format!("repo {repo:?} is not a repo")));
    }

    let (version, tree) = newest_compiling(pool, id).await?;

    let title = format!("[pack] {id}");
    let body = format!(
        "Exported from the controller's authoring studio: draft `{id}` at version {version}.\n\n\
         The diff is the pack. Merge it, then import it at `{repo}`/`{path}` to register it; the \
         draft retires when that import lands.\n"
    );
    let (branch_repo, branch_path, branch_key) =
        (repo.to_string(), path.to_string(), format!("draft/{id}"));
    let pushed = tokio::task::spawn_blocking(move || {
        let scratch = tempfile::tempdir()
            .context("draft export scratch dir")
            .map_err(DraftError::Internal)?;
        let out = if branch_path.is_empty() {
            scratch.path().to_path_buf()
        } else {
            scratch.path().join(&branch_path)
        };
        crate::playbooks::packs::write_tree(&tree, &out).map_err(DraftError::Internal)?;
        crate::runs::engine::open_draft_pr(
            &branch_repo,
            &branch_key,
            scratch.path(),
            &title,
            &body,
            token.as_deref(),
        )
        .map_err(|e| DraftError::Push(format!("{e:#}")))
    })
    .await
    .context("joining the draft export worker")
    .map_err(DraftError::Internal)?;
    let url = pushed?;

    sqlx::query(
        r#"UPDATE playbook_drafts
           SET graduation_repo = $2, graduation_path = $3, graduation_pr_url = $4, updated_at = $5
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(repo)
    .bind(path)
    .bind(&url)
    .bind(crate::clock::now_rfc3339())
    .execute(pool)
    .await
    .context("storing the draft graduation")
    .map_err(DraftError::Internal)?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use crate::playbooks::drafts::*;
    use crucible_contract::content_digest;

    fn skeleton() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("crucible.toml".to_string(), SKELETON_MANIFEST.to_string()),
            ("workflow.star".to_string(), SKELETON_WORKFLOW.to_string()),
        ])
    }

    /// A save whose files each fit the per-file cap, but whose delivered tarball is over the
    /// delivery budget, is refused and stores no version.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_save_over_the_delivery_budget_stores_nothing(pool: PgPool) {
        create(
            &pool,
            "studio",
            "a drafted pack",
            DraftSeed::Skeleton,
            Some("wren"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("the skeleton compiles");
        let before = versions(&pool, "studio").await.expect("versions").len();

        let mut files = skeleton();
        for seed in 1..=3 {
            files.insert(
                format!("blob{seed}.txt"),
                crate::testing::fixtures::incompressible_text(500 * 1024, seed),
            );
        }
        match save_version(&pool, "studio", files, None, None).await {
            Err(DraftError::Invalid(msg)) => assert!(msg.contains("delivery budget"), "{msg}"),
            other => panic!("expected a delivery-budget refusal, got {other:?}"),
        }
        assert_eq!(
            versions(&pool, "studio").await.expect("versions").len(),
            before
        );
    }

    /// The docs-drift case, at the layer that reported it clean: a draft whose manifest declares
    /// no `[agent]` compiles, and its agent task still cannot be spawned where a pod launch would
    /// put it. The save has to say so, anchored on the manifest, marked as the dispatch verdict
    /// rather than a compile failure.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_save_that_compiles_still_reports_what_cannot_be_spawned(pool: PgPool) {
        let pod = crate::playbooks::dispatch::DispatchCapability::new(
            crate::config::PlaybookExecutor::Pod,
            true,
        );
        let local = crate::playbooks::dispatch::DispatchCapability::new(
            crate::config::PlaybookExecutor::Local,
            false,
        );

        let created = create(
            &pool,
            "studio",
            "a drafted pack",
            DraftSeed::Skeleton,
            Some("wren"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("the skeleton compiles");
        assert!(
            !SKELETON_MANIFEST.contains("[repo]"),
            "a draft starts repo-less: its workspace is what [workspace].inject lists"
        );
        assert!(
            dispatch_diagnostics(created.graph.as_ref(), created.agent.as_ref(), &pod).is_empty(),
            "a workflow with no agent task spawns nothing to complain about"
        );

        let mut files = skeleton();
        files.insert(
            "workflow.star".to_string(),
            concat!(
                "params = {}\n",
                "write = agent(name = \"write\", prompt = \"go\")\n",
                "workflow(type = \"playbook\", tasks = [write], result = write)\n",
            )
            .to_string(),
        );
        let saved = save_version(&pool, "studio", files.clone(), None, None)
            .await
            .expect("save");
        assert!(saved.diagnostics.is_empty(), "{:?}", saved.diagnostics);
        assert!(saved.schema_digest.is_some(), "the tree compiles");

        let dispatch = dispatch_diagnostics(saved.graph.as_ref(), saved.agent.as_ref(), &pod);
        let [d] = dispatch.as_slice() else {
            panic!("expected one dispatch diagnostic, got {dispatch:?}");
        };
        assert_eq!(d.kind, DiagnosticKind::Dispatch);
        assert_eq!(d.file.as_deref(), Some("crucible.toml"));
        assert!(d.message.contains("sandbox_image"), "{}", d.message);
        assert!(
            dispatch_diagnostics(saved.graph.as_ref(), saved.agent.as_ref(), &local).is_empty(),
            "local mode spawns the agent on this machine, where the default backend is the point"
        );

        // Declaring the substrate the message asked for clears it, with nothing else changed.
        files.insert(
            "crucible.toml".to_string(),
            SKELETON_MANIFEST.replace(
                "[agent]\n",
                "[agent]\nbackend = \"openshell\"\nsandbox_image = \"ghcr.io/org/sandbox:latest\"\n",
            ),
        );
        let saved = save_version(&pool, "studio", files, None, None)
            .await
            .expect("save");
        assert!(saved.diagnostics.is_empty(), "{:?}", saved.diagnostics);
        assert!(
            dispatch_diagnostics(saved.graph.as_ref(), saved.agent.as_ref(), &pod).is_empty(),
            "the declared substrate is dispatchable"
        );
    }

    /// A tree the engine made nothing of is a compile failure, not a dispatch one: there is no
    /// graph to know whether it would ever spawn a turn.
    #[test]
    fn a_tree_that_did_not_compile_earns_no_dispatch_diagnostic() {
        let pod = crate::playbooks::dispatch::DispatchCapability::new(
            crate::config::PlaybookExecutor::Pod,
            true,
        );
        let agent = crate::playbooks::dispatch::PackAgent::new("local", None);
        assert!(dispatch_diagnostics(None, Some(&agent), &pod).is_empty());
    }

    #[test]
    fn a_diagnostic_is_anchored_relative_to_the_pack_root() {
        let root = Path::new("/tmp/scratch/pack");
        let d = parse_diagnostic(
            "/tmp/scratch/pack/workflow.star:3:7: unknown identifier",
            root,
        );
        assert_eq!(d.file.as_deref(), Some("workflow.star"));
        assert_eq!(d.line, Some(3));
        assert_eq!(d.col, Some(7));
        assert!(d.message.contains("unknown identifier"));

        // A diagnostic with no anchor keeps its whole text and points at no line.
        let bare = parse_diagnostic("the engine exploded", root);
        assert_eq!(bare.file, None);
        assert_eq!(bare.line, None);
        assert_eq!(bare.message, "the engine exploded");

        // The engine's own shape: an `Error:` prefix and a column range, the message on the lines
        // below.
        let ranged = parse_diagnostic(
            "Error: /tmp/scratch/pack/workflow.star:2:1-6\n\nCaused by:\n    unknown variable",
            root,
        );
        assert_eq!(ranged.file.as_deref(), Some("workflow.star"));
        assert_eq!(ranged.line, Some(2));
        assert_eq!(ranged.col, Some(1));
        assert!(ranged.message.contains("unknown variable"));

        // A path the engine printed relative rides through untouched.
        let rel = parse_diagnostic("skills/read/SKILL.md:1:1: bad frontmatter\n  detail", root);
        assert_eq!(rel.file.as_deref(), Some("skills/read/SKILL.md"));
        assert_eq!(rel.line, Some(1));
        assert!(rel.message.contains("detail"), "the whole text is kept");
    }

    #[test]
    fn a_file_map_that_escapes_the_pack_or_names_an_excluded_dir_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        for bad in [
            "../escape.star",
            "/etc/passwd",
            "",
            "a\\b",
            "skills/state/notes.md",
            "workspace/flow.star",
            ".git/config",
        ] {
            let files = BTreeMap::from([(bad.to_string(), "x".to_string())]);
            assert!(
                matches!(write_tree(&files, dir.path()), Err(DraftError::Invalid(_))),
                "{bad:?} is not a pack path"
            );
        }
        assert!(matches!(
            write_tree(&BTreeMap::new(), dir.path()),
            Err(DraftError::Invalid(_))
        ));
    }

    /// Saving the same file map twice appends two versions on one stored tree, and each version
    /// stores that tree's own tarball beside the tree it names.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn an_identical_file_map_saved_twice_shares_one_tree(pool: PgPool) {
        create(
            &pool,
            "studio",
            "a drafted pack",
            DraftSeed::Skeleton,
            Some("wren"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("create");
        let again = save_version(&pool, "studio", skeleton(), None, None)
            .await
            .expect("save");
        assert_eq!(again.version, 2);

        let expected = PackTree::from_pairs(&[
            ("crucible.toml", SKELETON_MANIFEST.as_bytes()),
            ("workflow.star", SKELETON_WORKFLOW.as_bytes()),
        ])
        .expect("tree");
        let tarball = expected.tarball().expect("tarball");
        let rows: Vec<(Option<String>, Vec<u8>, String)> = sqlx::query_as(
            "SELECT tree_digest, tar_gz, tar_digest FROM playbook_draft_versions \
             WHERE draft_id = 'studio' ORDER BY version",
        )
        .fetch_all(&pool)
        .await
        .expect("versions");
        let row = (
            Some(expected.digest().to_string()),
            tarball.clone(),
            content_digest(&tarball),
        );
        assert_eq!(rows, vec![row.clone(), row]);
        let trees: i64 = sqlx::query_scalar("SELECT count(*) FROM pack_trees")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(trees, 1);
    }

    /// Each save that changes one file of a pack stores exactly one new blob and one new tree,
    /// and every version reads back as the file map it saved.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_save_that_changes_one_file_stores_one_new_blob(pool: PgPool) {
        create(
            &pool,
            "studio",
            "a drafted pack",
            DraftSeed::Skeleton,
            Some("wren"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("create");
        let counts = || async {
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT (SELECT count(*) FROM pack_blobs), (SELECT count(*) FROM pack_trees)",
            )
            .fetch_one(&pool)
            .await
            .expect("counts")
        };
        let mut files = skeleton();
        for n in 0..8 {
            files.insert(format!("docs/{n}.md"), format!("doc {n}\n"));
        }
        save_version(&pool, "studio", files.clone(), None, None)
            .await
            .expect("save");
        let mut saved = vec![files.clone()];
        let (mut blobs, mut trees) = counts().await;

        for round in 0..5 {
            files.insert(format!("docs/{}.md", round % 8), format!("edit {round}\n"));
            save_version(&pool, "studio", files.clone(), None, None)
                .await
                .expect("save");
            saved.push(files.clone());
            assert_eq!(counts().await, (blobs + 1, trees + 1), "round {round}");
            (blobs, trees) = counts().await;
        }

        for (i, expected) in saved.iter().enumerate() {
            let version = i64::try_from(i).expect("version") + 2;
            let (_, tree) = version_tree(&pool, "studio", Some(version))
                .await
                .expect("read")
                .expect("stored");
            let read: BTreeMap<String, String> = tree
                .into_files()
                .into_iter()
                .map(|(path, bytes)| {
                    (
                        path.as_str().to_string(),
                        String::from_utf8(bytes).expect("utf8"),
                    )
                })
                .collect();
            assert_eq!(&read, expected, "version {version}");
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn saves_version_and_round_trip_the_file_map(pool: PgPool) {
        let (first, second, third) = {
            let first = create(
                &pool,
                "studio",
                "a drafted pack",
                DraftSeed::Skeleton,
                Some("wren"),
                &crate::authz::model::Principal::platform(),
            )
            .await
            .expect("create");
            let mut files = skeleton();
            files.insert("skills/read/SKILL.md".to_string(), "read it\n".to_string());
            let second = save_version(&pool, "studio", files.clone(), Some("wren"), Some(1))
                .await
                .expect("save");
            files.insert(
                "skills/read/SKILL.md".to_string(),
                "read it twice\n".to_string(),
            );
            let third = save_version(&pool, "studio", files, Some("agent:author"), Some(2))
                .await
                .expect("save");
            (first, second, third)
        };

        assert_eq!((first.version, second.version, third.version), (1, 2, 3));
        assert!(first.params_schema.is_some(), "the skeleton compiles");
        assert!(first.graph.is_some());
        assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);

        let history = versions(&pool, "studio").await.expect("versions");
        assert_eq!(
            history.iter().map(|v| v.version).collect::<Vec<_>>(),
            vec![3, 2, 1]
        );
        assert_ne!(
            history[0].tree_digest, history[1].tree_digest,
            "an edited file is a different tree"
        );

        assert_eq!(
            history
                .iter()
                .map(|v| v.created_by.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("agent:author"), Some("wren"), Some("wren")],
            "the history interleaves whoever saved each version"
        );

        let head = files(&pool, "studio", None)
            .await
            .expect("files")
            .expect("draft");
        assert_eq!(head.version, 3);
        assert_eq!(head.saved_by.as_deref(), Some("agent:author"));
        assert!(head.diagnostics.is_empty());
        let tree = head.files;
        assert_eq!(
            tree.keys().cloned().collect::<Vec<_>>(),
            vec![
                "crucible.toml".to_string(),
                "skills/read/SKILL.md".to_string(),
                "workflow.star".to_string()
            ]
        );
        assert_eq!(tree["skills/read/SKILL.md"], "read it twice\n");
        let older = files(&pool, "studio", Some(2))
            .await
            .expect("files")
            .expect("version 2");
        assert_eq!(older.saved_by.as_deref(), Some("wren"));
        assert_eq!(older.files["skills/read/SKILL.md"], "read it\n");

        let summaries = list(&pool).await.expect("list");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].latest_version, 3);
        assert!(summaries[0].compiles);
        assert_eq!(summaries[0].draft.created_by.as_deref(), Some("wren"));
    }

    /// Two editors saving from the same base at once: one appends, the other is refused with the
    /// version that overtook it. The refusal is what closes the max+1 race, so the history holds
    /// exactly the two versions and no insert ever collided on the primary key.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn racing_saves_from_one_base_append_once_and_refuse_once(pool: PgPool) {
        let (human, agent) = {
            create(
                &pool,
                "studio",
                "a drafted pack",
                DraftSeed::Skeleton,
                Some("wren"),
                &crate::authz::model::Principal::platform(),
            )
            .await
            .expect("create");

            let mut human_files = skeleton();
            human_files.insert("notes.md".to_string(), "the human's edit\n".to_string());
            let mut agent_files = skeleton();
            agent_files.insert("notes.md".to_string(), "the agent's edit\n".to_string());
            tokio::join!(
                save_version(&pool, "studio", human_files, Some("wren"), Some(1)),
                save_version(&pool, "studio", agent_files, Some("agent:author"), Some(1)),
            )
        };

        let history = versions(&pool, "studio").await.expect("versions");
        assert_eq!(
            history.iter().map(|v| v.version).collect::<Vec<_>>(),
            vec![2, 1],
            "one save appended, so no second insert collided on version 2"
        );

        let (winner, loser) = match (human, agent) {
            (Ok(saved), Err(e)) => (saved, e),
            (Err(e), Ok(saved)) => (saved, e),
            (a, b) => panic!("exactly one save lands: {a:?} / {b:?}"),
        };
        assert_eq!(winner.version, 2);
        let DraftError::StaleBase(stale) = loser else {
            panic!("the loser is refused with the version that overtook it: {loser:?}");
        };
        assert_eq!(stale.base_version, 1);
        assert_eq!(stale.current_version, 2);
        assert_eq!(stale.saved_by, winner.saved_by);
        assert_eq!(stale.saved_at, winner.saved_at);
        assert!(
            stale.to_string().contains("merge"),
            "the refusal tells the writer to re-read: {stale}"
        );

        // The loser's bytes never landed: the winner's file is what the next reader picks up.
        let head = files(&pool, "studio", None)
            .await
            .expect("files")
            .expect("draft");
        assert_eq!(head.version, 2);
        assert_eq!(head.saved_by, winner.saved_by);
        let expected = match winner.saved_by.as_deref() {
            Some("wren") => "the human's edit\n",
            _ => "the agent's edit\n",
        };
        assert_eq!(head.files["notes.md"], expected);

        // A save that names the version it actually read is accepted.
        let merged = {
            let mut files = skeleton();
            files.insert("notes.md".to_string(), "both edits\n".to_string());
            save_version(&pool, "studio", files, Some("wren"), Some(2)).await
        }
        .expect("a save from the current base lands");
        assert_eq!(merged.version, 3);
    }

    /// A save that does not compile is still a save: the bytes are kept, the schema is not, and the
    /// engine's anchor rides so the editor can put the message on the line.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refused_compile_stores_the_bytes_and_the_anchor(pool: PgPool) {
        create(
            &pool,
            "studio",
            "a drafted pack",
            DraftSeed::Skeleton,
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("create");
        let saved = {
            let mut files = skeleton();
            files.insert("workflow.star".to_string(), "dpeth\n".to_string());
            save_version(&pool, "studio", files, None, None)
                .await
                .expect("save")
        };

        assert_eq!(saved.version, 2);
        assert_eq!(saved.graph, None, "no graph without a compiled plan");
        assert_eq!(saved.diagnostics.len(), 1);
        assert_eq!(saved.diagnostics[0].file.as_deref(), Some("workflow.star"));
        assert_eq!(saved.diagnostics[0].line, Some(1));
        assert_eq!(saved.diagnostics[0].col, Some(1));

        let head = files(&pool, "studio", None)
            .await
            .expect("files")
            .expect("draft");
        assert_eq!(head.files["workflow.star"], "dpeth\n", "the bytes survive");
        assert_eq!(
            head.diagnostics, saved.diagnostics,
            "the reader picks the save's complaint up with its bytes"
        );
        let latest = latest(&pool, "studio").await.expect("latest").expect("row");
        assert_eq!(latest.version, 2);
    }

    /// Graduation and the import that follows it are one loop: a registration for the graduated
    /// repo/path retires the draft, and any other registration leaves it alone.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn retire_matching_only_retires_the_graduated_target(pool: PgPool) {
        create(
            &pool,
            "studio",
            "a drafted pack",
            DraftSeed::Skeleton,
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("create");
        sqlx::query(
            "UPDATE playbook_drafts SET graduation_repo = 'owner/packs', \
             graduation_path = 'packs/studio', graduation_pr_url = 'https://x/1' WHERE id = $1",
        )
        .bind("studio")
        .execute(&pool)
        .await
        .expect("graduate");

        assert_eq!(
            crate::playbooks::registry::retire_matching(&pool, "owner/packs", "packs/other")
                .await
                .expect("retire"),
            0,
            "another pack's import is not this draft's graduation"
        );
        assert_eq!(
            crate::playbooks::registry::retire_matching(&pool, "owner/packs", "packs/studio")
                .await
                .expect("retire"),
            1
        );
        let row = get(&pool, "studio").await.expect("get").expect("row");
        assert!(row.retired_at.is_some());
        assert_eq!(
            crate::playbooks::registry::retire_matching(&pool, "owner/packs", "packs/studio")
                .await
                .expect("retire"),
            0,
            "a retired draft retires once"
        );

        // A retired draft is read-only: its bytes are the merged pack's history now.
        let refused = {
            save_version(&pool, "studio", skeleton(), None, None)
                .await
                .expect_err("retired")
        };
        assert!(matches!(refused, DraftError::Conflict(_)), "{refused:#}");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_draft_may_not_take_a_registered_id_and_deletes_with_its_versions(pool: PgPool) {
        sqlx::query(
            "INSERT INTO playbooks (id, description, repo, rev, path, tar_gz, tar_digest, \
             tar_bytes, params_schema, schema_digest, core_rev, created_at, updated_at) \
             VALUES ('survey', 'd', 'o/r', 'abc', '', '\\x00', 'sha256:1', 1, '{}'::jsonb, \
             'sha256:2', 'pin', 'now', 'now')",
        )
        .execute(&pool)
        .await
        .expect("seed registry");

        let (clash, dup) = {
            let clash = create(
                &pool,
                "survey",
                "d",
                DraftSeed::Skeleton,
                None,
                &crate::authz::model::Principal::platform(),
            )
            .await
            .expect_err("registered");
            create(
                &pool,
                "studio",
                "d",
                DraftSeed::Skeleton,
                None,
                &crate::authz::model::Principal::platform(),
            )
            .await
            .expect("create");
            let dup = create(
                &pool,
                "studio",
                "d",
                DraftSeed::Skeleton,
                None,
                &crate::authz::model::Principal::platform(),
            )
            .await
            .expect_err("exists");
            (clash, dup)
        };
        assert!(matches!(clash, DraftError::Conflict(_)), "{clash:#}");
        assert!(matches!(dup, DraftError::Conflict(_)), "{dup:#}");

        assert!(delete(&pool, "studio").await.expect("delete"));
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_draft_versions")
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(left, 0, "versions cascade with their draft");
        assert!(!delete(&pool, "studio").await.expect("delete"));
    }

    /// A pack tarball holding `files`, the way the registry and an import both store one.
    fn packed(dir: &Path, files: &BTreeMap<String, String>) -> Vec<u8> {
        let root = dir.join("packed");
        std::fs::create_dir_all(&root).expect("mkdir");
        write_tree(files, &root).expect("write tree");
        crate::playbooks::packs::tar_pack_tree(&root).expect("tar")
    }

    /// A draft carries what it came from. A template names the registered pack at the rev it was
    /// copied at, so a later re-pin shows up as an origin that moved and a rebase source to read;
    /// an import names its row, whose bytes are frozen and so never move; a skeleton names nothing.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_draft_carries_the_origin_it_was_seeded_from(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let registered = BTreeMap::from([
            ("crucible.toml".to_string(), SKELETON_MANIFEST.to_string()),
            (
                "workflow.star".to_string(),
                "params = {}\n# registered\n".to_string(),
            ),
        ]);
        let tar_gz = packed(dir.path(), &registered);
        sqlx::query(
            "INSERT INTO playbooks (id, description, repo, rev, path, tar_gz, tar_digest, \
             tar_bytes, params_schema, schema_digest, core_rev, created_at, updated_at) \
             VALUES ('survey', 'd', 'o/r', 'aaa', 'packs/survey', $1, 'sha256:1', 1, '{}'::jsonb, \
             'sha256:2', 'pin', 'now', 'now')",
        )
        .bind(&tar_gz)
        .execute(&pool)
        .await
        .expect("seed registry");

        let imported = BTreeMap::from([
            ("crucible.toml".to_string(), SKELETON_MANIFEST.to_string()),
            (
                "workflow.star".to_string(),
                "params = {}\n# imported\n".to_string(),
            ),
        ]);
        let import_tar = packed(&dir.path().join("import"), &imported);
        sqlx::query(
            "INSERT INTO pack_imports (id, repo, git_ref, path, rev, tar_gz, tar_digest, \
             tar_bytes, diagnostics, core_rev, status, created_at) \
             VALUES ('imp-1', 'o/other', 'main', 'packs/audit', 'ccc', $1, 'sha256:3', 1, \
             '[]'::jsonb, 'pin', 'pending', 'now')",
        )
        .bind(&import_tar)
        .execute(&pool)
        .await
        .expect("seed import");

        create(
            &pool,
            "from-template",
            "d",
            DraftSeed::Template {
                id: "survey",
                expected_rev: None,
                expected_digest: None,
            },
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("template");
        create(
            &pool,
            "from-import",
            "d",
            DraftSeed::Import {
                id: "imp-1",
                tree: &crucible_contract::pack_tree::read_tar_gz(&import_tar)
                    .expect("read")
                    .tree,
            },
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("import");
        create(
            &pool,
            "from-nothing",
            "d",
            DraftSeed::Skeleton,
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("skeleton");

        let templated = get(&pool, "from-template")
            .await
            .expect("read")
            .expect("draft")
            .origin
            .expect("origin");
        assert_eq!(templated.kind, OriginKind::Playbook);
        assert_eq!(templated.playbook.as_deref(), Some("survey"));
        assert_eq!(templated.repo.as_deref(), Some("o/r"));
        assert_eq!(templated.path.as_deref(), Some("packs/survey"));
        assert_eq!(templated.rev.as_deref(), Some("aaa"));
        assert_eq!(templated.current_rev.as_deref(), Some("aaa"));
        assert!(!templated.moved(), "nothing re-pinned yet");

        let from_import = get(&pool, "from-import")
            .await
            .expect("read")
            .expect("draft")
            .origin
            .expect("origin");
        assert_eq!(from_import.kind, OriginKind::Import);
        assert_eq!(from_import.import_id.as_deref(), Some("imp-1"));
        assert_eq!(from_import.repo.as_deref(), Some("o/other"));
        assert_eq!(from_import.path.as_deref(), Some("packs/audit"));
        assert_eq!(from_import.rev.as_deref(), Some("ccc"));
        assert!(!from_import.moved(), "frozen bytes never move");

        assert!(
            get(&pool, "from-nothing")
                .await
                .expect("read")
                .expect("draft")
                .origin
                .is_none(),
            "a skeleton is based on nothing"
        );
        assert!(
            origin_files(&pool, "from-nothing")
                .await
                .expect("origin files")
                .is_none()
        );

        // The registry re-pins under the draft: the origin moved, and the rebase source is the
        // pack at the new rev, not the one the draft was copied from.
        let moved_files = BTreeMap::from([
            ("crucible.toml".to_string(), SKELETON_MANIFEST.to_string()),
            (
                "workflow.star".to_string(),
                "params = {}\n# re-pinned\n".to_string(),
            ),
        ]);
        let moved_tar = packed(&dir.path().join("moved"), &moved_files);
        sqlx::query("UPDATE playbooks SET rev = 'bbb', tar_gz = $1 WHERE id = 'survey'")
            .bind(&moved_tar)
            .execute(&pool)
            .await
            .expect("re-pin");

        let repinned = get(&pool, "from-template")
            .await
            .expect("read")
            .expect("draft")
            .origin
            .expect("origin");
        assert_eq!(repinned.current_rev.as_deref(), Some("bbb"));
        assert!(repinned.moved(), "the draft is behind its own origin");
        let listed = list(&pool).await.expect("list");
        let row = listed
            .iter()
            .find(|d| d.draft.id == "from-template")
            .expect("listed");
        assert!(
            row.draft.origin.as_ref().is_some_and(DraftOrigin::moved),
            "the rail reads the same origin the studio does"
        );

        let rebase = origin_files(&pool, "from-template")
            .await
            .expect("origin files")
            .expect("an origin");
        assert_eq!(rebase.reference, "survey");
        assert_eq!(rebase.rev.as_deref(), Some("bbb"));
        assert_eq!(
            rebase.files.get("workflow.star").map(String::as_str),
            Some("params = {}\n# re-pinned\n")
        );

        let from_frozen = origin_files(&pool, "from-import")
            .await
            .expect("origin files")
            .expect("an origin");
        assert_eq!(
            from_frozen.files.get("workflow.star").map(String::as_str),
            Some("params = {}\n# imported\n")
        );
    }

    /// Each version reads back as the tree its save stored, under the digest its row records.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn every_version_reads_back_as_the_tree_it_stored(pool: PgPool) {
        create(
            &pool,
            "studio",
            "d",
            DraftSeed::Skeleton,
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("create");
        let mut second = skeleton();
        second.insert("skills/read/SKILL.md".to_string(), "read it\n".to_string());
        save_version(&pool, "studio", second.clone(), None, Some(1))
            .await
            .expect("save");
        let recorded = |version: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, Option<String>>(
                    "SELECT tree_digest FROM playbook_draft_versions
                     WHERE draft_id = 'studio' AND version = $1",
                )
                .bind(version)
                .fetch_one(&pool)
                .await
                .expect("tree column")
            }
        };

        let (version, tree) = version_tree(&pool, "studio", None)
            .await
            .expect("read")
            .expect("a version");
        assert_eq!(version, 2);
        assert_eq!(
            crate::playbooks::packs::text_files(tree.clone()).expect("text"),
            second
        );
        assert_eq!(recorded(2).await, Some(tree.digest().to_string()));

        let (version, first) = version_tree(&pool, "studio", Some(1))
            .await
            .expect("read")
            .expect("a version");
        assert_eq!(version, 1);
        assert_eq!(
            crate::playbooks::packs::text_files(first.clone()).expect("text"),
            skeleton(),
            "version 1 is version 1"
        );
        assert_eq!(recorded(1).await, Some(first.digest().to_string()));
        assert!(
            version_tree(&pool, "studio", Some(9))
                .await
                .expect("read")
                .is_none()
        );
        assert!(
            version_tree(&pool, "nope", None)
                .await
                .expect("read")
                .is_none()
        );
    }

    /// The whole compile-on-save path: the skeleton compiles, a parameterized save compiles on
    /// its defaults, and a refused source keeps its bytes with an anchored diagnostic.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn the_engine_compiles_a_draft_on_save(pool: PgPool) {
        let created = {
            create(
                &pool,
                "studio",
                "a drafted pack",
                DraftSeed::Skeleton,
                Some("wren"),
                &crate::authz::model::Principal::platform(),
            )
            .await
            .expect("the real engine compiles the skeleton")
        };
        assert!(created.diagnostics.is_empty(), "{:?}", created.diagnostics);
        let graph = created.graph.expect("graph");
        assert_eq!(graph.workflow_type, "playbook");
        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["hello"]
        );

        // A parameterized source compiles on its declared defaults, so the studio draws a graph
        // without anyone typing a value.
        let source = concat!(
            "params = {\n",
            "    \"depth\": {\"type\": \"string\", \"default\": \"deep\"},\n",
            "}\n",
            "a = command(name = \"a\", run = \"true\")\n",
            "b = agent(name = \"b\", prompt = \"go\", model = \"opus\", depends_on = [a])\n",
            "workflow(type = \"playbook\", tasks = [a, b], result = b)\n",
        );
        let mut files = skeleton();
        files.insert("workflow.star".to_string(), source.to_string());
        let saved = {
            save_version(&pool, "studio", files, None, None)
                .await
                .expect("save")
        };
        assert!(saved.diagnostics.is_empty(), "{:?}", saved.diagnostics);
        let graph = saved.graph.expect("graph");
        assert_eq!(
            graph
                .nodes
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(
            saved.params_schema.expect("schema")["properties"]["depth"]["default"],
            "deep"
        );

        // A required param with no value is the launch form's business, not a save diagnostic: the
        // save compiles on a stand-in and stores a schema digest and a graph.
        let source = concat!(
            "params = {\n",
            "    \"repo_url\": {\"type\": \"string\", \"required\": True,\n",
            "                  \"pattern\": \"^https://github\\\\.com/[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$\"},\n",
            "}\n",
            "a = command(name = \"a\", run = \"true\")\n",
            "b = agent(name = \"b\", prompt = param(\"repo_url\"), model = \"opus\", depends_on = [a])\n",
            "workflow(type = \"playbook\", tasks = [a, b], result = b)\n",
        );
        let mut files = skeleton();
        files.insert("workflow.star".to_string(), source.to_string());
        let saved = {
            save_version(&pool, "studio", files, None, None)
                .await
                .expect("save")
        };
        assert!(saved.diagnostics.is_empty(), "{:?}", saved.diagnostics);
        assert!(saved.schema_digest.is_some_and(|d| !d.is_empty()));
        assert_eq!(
            saved
                .graph
                .expect("graph")
                .nodes
                .iter()
                .map(|n| n.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(
            saved.params_schema.expect("schema")["required"],
            serde_json::json!(["repo_url"])
        );

        // A source the engine refuses is a stored version carrying its anchored diagnostic.
        let mut broken = skeleton();
        broken.insert(
            "workflow.star".to_string(),
            "params = {}\ndpeth\n".to_string(),
        );
        let refused = {
            save_version(&pool, "studio", broken, None, None)
                .await
                .expect("save")
        };
        assert_eq!(refused.graph, None, "a refused source compiles no plan");
        assert!(!refused.diagnostics.is_empty());
        assert_eq!(
            refused.diagnostics[0].file.as_deref(),
            Some("workflow.star")
        );
        assert!(refused.diagnostics[0].line.is_some());
    }

    /// The origin pin is the tree. A registry repoint that keeps the rev but changes the tree
    /// moves the origin, a template pin naming the old tree is refused, and a pre-tree digest is
    /// refused as superseded only when the template has held its replacement. A superseded digest
    /// whose tree the template never held reads exactly as an unknown one.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_same_rev_repoint_to_another_tree_moves_the_origin(pool: PgPool) {
        let tree = |marker: &str| {
            PackTree::from_pairs(&[
                ("crucible.toml", SKELETON_MANIFEST.as_bytes()),
                (
                    "workflow.star",
                    format!("params = {{}}\n# {marker}\n").as_bytes(),
                ),
            ])
            .expect("tree")
        };
        let (first, second, elsewhere) = (tree("first"), tree("second"), tree("elsewhere"));
        let mut conn = pool.acquire().await.expect("conn");
        let mut stored = Vec::new();
        for t in [&first, &second, &elsewhere] {
            let pack = crate::playbooks::pack_trees::EncodedPack::new(t.clone()).expect("encode");
            crate::playbooks::pack_trees::put_tree(&mut conn, &pack)
                .await
                .expect("put");
            stored.push(pack);
        }
        let pin = |pack: &crate::playbooks::pack_trees::EncodedPack| {
            let pool = pool.clone();
            let (tarball, digest) = (pack.tarball().to_vec(), pack.tree().digest().to_string());
            async move {
                sqlx::query(
                    "INSERT INTO playbooks (id, description, repo, rev, path, tar_gz, \
                     tar_digest, tar_bytes, params_schema, schema_digest, core_rev, created_at, \
                     updated_at, tree_digest) \
                     VALUES ('survey', 'd', 'o/r', 'aaa', '', $1, $2, 1, '{}'::jsonb, 's', \
                     'pin', 'now', 'now', $3) \
                     ON CONFLICT (id) DO UPDATE SET tar_gz = excluded.tar_gz, \
                     tar_digest = excluded.tar_digest, tree_digest = excluded.tree_digest",
                )
                .bind(&tarball)
                .bind(content_digest(&tarball))
                .bind(&digest)
                .execute(&pool)
                .await
                .expect("pin");
                sqlx::query(
                    "INSERT INTO playbook_revisions (playbook_id, tree_digest, first_seen_at) \
                     VALUES ('survey', $1, 'now') ON CONFLICT DO NOTHING",
                )
                .bind(&digest)
                .execute(&pool)
                .await
                .expect("revision");
            }
        };
        fn template(expected_digest: Option<&str>) -> DraftSeed<'_> {
            DraftSeed::Template {
                id: "survey",
                expected_rev: Some("aaa"),
                expected_digest,
            }
        }
        async fn refuse(pool: &PgPool, seed: DraftSeed<'_>) -> String {
            match create(
                pool,
                "again",
                "d",
                seed,
                None,
                &crate::authz::model::Principal::platform(),
            )
            .await
            {
                Err(DraftError::Conflict(msg)) => msg,
                other => panic!("expected a conflict, got {other:?}"),
            }
        }
        pin(&stored[0]).await;
        let first_digest = first.digest().to_string();
        create(
            &pool,
            "fork",
            "d",
            template(Some(&first_digest)),
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("fork");
        let origin = || async {
            get(&pool, "fork")
                .await
                .expect("read")
                .expect("draft")
                .origin
                .expect("origin")
        };
        let seeded = origin().await;
        assert_eq!(seeded.digest.as_deref(), Some(first_digest.as_str()));
        assert!(!seeded.moved());

        pin(&stored[1]).await;
        let repointed = origin().await;
        assert_eq!(repointed.rev, repointed.current_rev, "the rev did not move");
        assert_eq!(repointed.current_digest, Some(second.digest().to_string()));
        assert!(repointed.moved(), "the tree did");

        let unknown = refuse(&pool, template(Some("sha256:never"))).await;
        assert!(unknown.contains("moved while cloning"), "{unknown}");
        assert_eq!(refuse(&pool, template(Some(&first_digest))).await, unknown);

        for (old, tree) in [("sha256:old", &second), ("sha256:foreign", &elsewhere)] {
            sqlx::query(
                "INSERT INTO pack_digest_aliases (old_digest, tree_digest, recorded_at)
                 VALUES ($1, $2, 'then')",
            )
            .bind(old)
            .bind(tree.digest().as_str())
            .execute(&pool)
            .await
            .expect("alias");
        }
        let superseded = refuse(&pool, template(Some("sha256:old"))).await;
        assert!(
            superseded.contains(&format!("superseded by {}", second.digest())),
            "{superseded}"
        );
        assert_eq!(
            refuse(&pool, template(Some("sha256:foreign"))).await,
            unknown,
            "a tree the template never held is not named"
        );
        let stale_rev = refuse(
            &pool,
            DraftSeed::Template {
                id: "survey",
                expected_rev: Some("sha256:old"),
                expected_digest: None,
            },
        )
        .await;
        assert!(
            stale_rev.contains(&format!("superseded by {}", second.digest())),
            "{stale_rev}"
        );
    }

    /// Text shaped like a pack's docs: headings over lines drawn from a pool of 512 sentences, so
    /// gzip finds the repeated phrasing it finds in written prose (about 7x).
    fn pack_prose(len: usize, seed: u64) -> String {
        const WORDS: [&str; 48] = [
            "the", "pack", "run", "agent", "tree", "file", "step", "build", "image", "draft",
            "save", "check", "result", "error", "test", "cluster", "pod", "turn", "score",
            "launch", "workflow", "manifest", "with", "from", "into", "when", "each", "every",
            "before", "after", "returns", "stores", "reads", "writes", "compiles", "refuses", "a",
            "an", "of", "to", "is", "and", "or", "not", "this", "that", "its", "once",
        ];
        let mut x = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut next = move |bound: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            usize::try_from(x % bound).expect("bounded")
        };
        let sentences: Vec<String> = (0..512)
            .map(|_| {
                let words: Vec<&str> = (0..5 + next(6)).map(|_| WORDS[next(48)]).collect();
                format!("{}.", words.join(" "))
            })
            .collect();
        let mut out = String::with_capacity(len + 256);
        let mut line = 0;
        while out.len() < len {
            if line % 24 == 0 {
                out.push_str(&format!("\n## Section {}\n\n", next(1000)));
            }
            out.push_str(&sentences[next(512)]);
            out.push(' ');
            out.push_str(&sentences[next(512)]);
            out.push('\n');
            line += 1;
        }
        out.truncate(len);
        out
    }

    fn percentile(samples: &[f64], p: f64) -> f64 {
        let mut sorted = samples.to_vec();
        sorted.sort_by(f64::total_cmp);
        let rank = (p * sorted.len() as f64).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    }

    async fn relation_bytes(pool: &PgPool) -> Vec<(&'static str, i64)> {
        let mut sizes = Vec::new();
        for table in [
            "pack_trees",
            "pack_tree_files",
            "pack_blobs",
            "playbook_draft_versions",
        ] {
            let bytes: Option<i64> =
                sqlx::query_scalar("SELECT pg_total_relation_size(to_regclass($1))")
                    .bind(table)
                    .fetch_one(pool)
                    .await
                    .expect("relation size");
            if let Some(bytes) = bytes {
                sizes.push((table, bytes));
            }
        }
        sizes
    }

    /// The draft save path on a large pack: 40 files of 125 KB of prose saved 20 times, one file
    /// changed per save, with each phase timed on the same input and the tables' growth.
    ///
    /// `cargo nextest run --release -p crucible-controller --run-ignored only --no-capture
    /// large_pack_save_profile`
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    #[ignore = "a measurement, not a check"]
    async fn large_pack_save_profile(pool: PgPool) {
        const FILES: u64 = 40;
        const FILE_BYTES: usize = 125 * 1024;
        const SAVES: u64 = 20;

        create(
            &pool,
            "studio",
            "a large drafted pack",
            DraftSeed::Skeleton,
            None,
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("the skeleton compiles");
        let mut files = skeleton();
        for n in 0..FILES {
            files.insert(format!("docs/{n:02}.md"), pack_prose(FILE_BYTES, n));
        }
        let raw: usize = files.values().map(String::len).sum();
        let before = relation_bytes(&pool).await;

        let phases = [
            "json decode",
            "write_tree",
            "walk_dir",
            "tarball encode",
            "hashes",
            "compile",
            "save_version",
            "db + rest",
        ];
        let mut samples: Vec<Vec<f64>> = vec![Vec::new(); phases.len()];
        let ms = |since: std::time::Instant| since.elapsed().as_secs_f64() * 1000.0;
        let mut delivered = 0;
        for save in 0..SAVES {
            let changed = format!("docs/{:02}.md", save % FILES);
            files.insert(changed, pack_prose(FILE_BYTES, 1000 + save));

            let body = serde_json::to_vec(&serde_json::json!({ "files": &files })).expect("json");
            let start = std::time::Instant::now();
            let decoded: BTreeMap<String, BTreeMap<String, String>> =
                serde_json::from_slice(&body).expect("decode");
            let decode = ms(start);
            assert_eq!(decoded.get("files"), Some(&files));

            let scratch = tempfile::tempdir().expect("tempdir");
            let root = scratch.path().join("pack");
            std::fs::create_dir_all(&root).expect("mkdir");
            let start = std::time::Instant::now();
            write_tree(&files, &root).expect("write_tree");
            let write = ms(start);
            let start = std::time::Instant::now();
            let walked = crucible_contract::pack_tree::walk_dir(&root).expect("walk");
            let walk = ms(start);
            let start = std::time::Instant::now();
            let encoded =
                crate::playbooks::pack_trees::EncodedPack::new(walked.tree).expect("encode");
            let encode = ms(start);
            delivered = encoded.tarball().len();
            let start = std::time::Instant::now();
            let _ = encoded.tree().digest_with_file_hashes();
            let hashes = ms(start);
            let start = std::time::Instant::now();
            let compiled = compile_tree(&root);
            let compile = ms(start);
            assert!(
                compiled.diagnostics.is_empty(),
                "{:?}",
                compiled.diagnostics
            );

            let start = std::time::Instant::now();
            save_version(&pool, "studio", files.clone(), None, None)
                .await
                .expect("save");
            let total = ms(start);
            let rest = total - write - walk - encode - hashes - compile;
            for (i, value) in [decode, write, walk, encode, hashes, compile, total, rest]
                .into_iter()
                .enumerate()
            {
                samples[i].push(value);
            }
            println!("save {:>2}: {total:>7.1} ms", save + 1);
        }

        let after = relation_bytes(&pool).await;
        println!(
            "\npack: {FILES} files, {raw} raw bytes, {delivered} gzipped ({:.2}x); {SAVES} saves",
            raw as f64 / delivered as f64
        );
        println!(
            "{:<16} {:>9} {:>9} {:>9}",
            "phase (ms)", "p50", "p95", "first"
        );
        for (phase, values) in phases.iter().zip(&samples) {
            println!(
                "{phase:<16} {:>9.1} {:>9.1} {:>9.1}",
                percentile(values, 0.5),
                percentile(values, 0.95),
                values[0]
            );
        }
        println!(
            "\n{:<24} {:>12} {:>12} {:>12} {:>12}",
            "relation", "before", "after", "growth", "per save"
        );
        for ((table, from), (_, to)) in before.iter().zip(&after) {
            println!(
                "{table:<24} {from:>12} {to:>12} {:>12} {:>12}",
                to - from,
                (to - from) / i64::try_from(SAVES).expect("saves")
            );
        }
    }
}
