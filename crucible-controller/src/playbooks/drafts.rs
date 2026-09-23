//! Draft packs: a pack authored in the controller instead of imported from a git pin. A draft is
//! a versioned blob — every save tars the editor's `{path: content}` map, compiles it with the
//! pinned engine, and stores the tarball beside whatever the engine made of it. A save that does
//! not compile is still a version: the diagnostics are the payload the studio renders, not an
//! error that loses the bytes.
//!
//! Drafts never enter the registry. [`graduate`] exports one as a PR over the same branch-pair
//! push the scope-pack approval uses, and [`retire_matching`] stamps the draft retired once the
//! merged pack is registered from the repo/path it was graduated to.

#![allow(clippy::disallowed_macros)]

use crate::playbooks::packs::MaterializedPack;
use crate::playbooks::plan_graph::WorkflowGraphDto;
use crate::playbooks::registry::{MAX_PACK_TAR_BYTES, RegisterError, validate_id, validate_path};
use anyhow::{Context, Result};
use crucible_contract::content_digest;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use std::path::Path;

/// How many files one draft may hold. A playbook pack is a manifest, a workflow source and a
/// handful of skills; a save past this is not one.
const MAX_DRAFT_FILES: usize = 128;

/// How large one draft file may be, before gzip.
const MAX_DRAFT_FILE_BYTES: usize = 512 * 1024;

/// The manifest a draft starts from when no template seeds it.
const SKELETON_MANIFEST: &str =
    "[agent]\n\n[workflow]\ntype = \"playbook\"\nfile = \"workflow.star\"\n";

/// The workflow a draft starts from: the smallest graph that compiles, so the studio opens on a
/// green preview rather than on a diagnostic.
const SKELETON_WORKFLOW: &str = concat!(
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
    Internal(#[from] anyhow::Error),
}

impl From<crate::playbooks::packs::ReadTreeError> for DraftError {
    fn from(e: crate::playbooks::packs::ReadTreeError) -> Self {
        match e {
            crate::playbooks::packs::ReadTreeError::NotText(rel) => DraftError::Invalid(format!(
                "{rel} is not text, so it cannot be edited as a draft"
            )),
            crate::playbooks::packs::ReadTreeError::Io(e) => DraftError::Internal(e),
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

impl From<RegisterError> for DraftError {
    fn from(e: RegisterError) -> Self {
        match e {
            RegisterError::Invalid(m) | RegisterError::Compile(m) => DraftError::Invalid(m),
            RegisterError::Fetch(m) => DraftError::Push(m),
            RegisterError::RevMoved(rev) => DraftError::Conflict(format!("the ref moved to {rev}")),
            RegisterError::Conflict(m) => DraftError::Conflict(m),
            e @ RegisterError::ExposureChanged { .. } => DraftError::Conflict(e.to_string()),
            RegisterError::Internal(e) => DraftError::Internal(e),
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

/// A draft's origin, resolved against where that pack lives now. `rev` is what the draft was
/// seeded from and `current_rev` is what the origin serves today: a registry re-pin moves them
/// apart, which is what the studio offers a rebase against. An import's bytes are frozen, so its
/// two revs are the same one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftOrigin {
    pub kind: OriginKind,
    pub playbook: Option<String>,
    pub import_id: Option<String>,
    pub repo: Option<String>,
    pub path: Option<String>,
    pub rev: Option<String>,
    pub current_rev: Option<String>,
}

impl DraftOrigin {
    /// Whether the origin pack re-pinned under the draft.
    pub fn moved(&self) -> bool {
        match (self.rev.as_deref(), self.current_rev.as_deref()) {
            (Some(seeded), Some(current)) => seeded != current,
            _ => false,
        }
    }

    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Option<Self>> {
        let playbook: Option<String> = row.try_get("origin_playbook")?;
        let import_id: Option<String> = row.try_get("origin_import")?;
        let rev: Option<String> = row.try_get("origin_rev")?;
        if let Some(playbook) = playbook {
            return Ok(Some(DraftOrigin {
                kind: OriginKind::Playbook,
                playbook: Some(playbook),
                import_id: None,
                repo: row.try_get("origin_playbook_repo")?,
                path: row.try_get("origin_playbook_path")?,
                rev,
                current_rev: row.try_get("origin_current_rev")?,
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
        }))
    }
}

/// Every draft column the studio reads, joined to whatever its origin is now.
const DRAFT_COLUMNS: &str = "d.id, d.description, d.origin_playbook, d.origin_rev, \
                             d.origin_import, d.graduation_repo, d.graduation_path, \
                             d.graduation_pr_url, d.retired_at, d.owner, d.created_by, d.created_at, \
                             d.updated_at, p.repo AS origin_playbook_repo, \
                             p.path AS origin_playbook_path, p.rev AS origin_current_rev, \
                             i.repo AS origin_import_repo, i.path AS origin_import_path";

const DRAFT_JOINS: &str = "FROM playbook_drafts d \
                           LEFT JOIN playbooks p ON p.id = d.origin_playbook \
                           LEFT JOIN pack_imports i ON i.id = d.origin_import";

/// A draft's metadata row, without any version's bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftRow {
    pub id: String,
    pub description: String,
    pub origin: Option<DraftOrigin>,
    pub graduation_repo: Option<String>,
    pub graduation_path: Option<String>,
    pub graduation_pr_url: Option<String>,
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
            retired_at: row.try_get("retired_at")?,
            owner: crate::authz::model::Principal::parse(&row.try_get::<String, _>("owner")?)?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// One stored save, without its tarball.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftVersionRow {
    pub version: i64,
    pub tar_digest: String,
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
            tar_digest: row.try_get("tar_digest")?,
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
        validate_path(path).map_err(|_| {
            DraftError::Invalid(format!("{path:?} is not a relative path inside the pack"))
        })?;
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
    /// A pending import's frozen tarball, at the rev it was taken at.
    Import { id: &'a str, tar_gz: &'a [u8] },
}

/// The origin columns one create writes.
#[derive(Debug, Default)]
struct SeededOrigin {
    playbook: Option<String>,
    import_id: Option<String>,
    rev: Option<String>,
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
            if expected_rev.is_some_and(|rev| rev != registered.rev) {
                return Err(DraftError::Conflict(format!(
                    "playbook {template:?} moved from inspected revision {} to {}; reload before cloning",
                    expected_rev.unwrap_or_default(),
                    registered.rev
                )));
            }
            let pack = crate::playbooks::registry::materialize_at_rev(
                pool,
                template,
                &registered.rev,
                expected_digest,
            )
            .await
            .map_err(DraftError::Internal)?
            .ok_or_else(|| {
                DraftError::Conflict(format!(
                    "playbook {template:?} moved while cloning; reload before retrying"
                ))
            })?;
            (
                crate::playbooks::packs::read_tree(pack.path())?,
                SeededOrigin {
                    playbook: Some(registered.id),
                    import_id: None,
                    rev: Some(registered.rev),
                },
            )
        }
        DraftSeed::Import { id: import, tar_gz } => {
            let rev: Option<String> =
                sqlx::query_scalar("SELECT rev FROM pack_imports WHERE id = $1")
                    .bind(import)
                    .fetch_optional(pool)
                    .await
                    .context("reading the rev a draft is seeded at")
                    .map_err(DraftError::Internal)?;
            let pack = crate::playbooks::packs::unpack_to_scratch(tar_gz)
                .context("unpacking the pack tarball the draft is seeded from")
                .map_err(DraftError::Internal)?;
            (
                crate::playbooks::packs::read_tree(pack.path())?,
                SeededOrigin {
                    playbook: None,
                    import_id: Some(import.to_string()),
                    rev,
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
                                        created_by, created_at, updated_at, owner)
           VALUES ($1, $2, $3, $6, $7, $4, $5, $5, $8) ON CONFLICT (id) DO NOTHING"#,
    )
    .bind(id)
    .bind(description.trim())
    .bind(&origin.playbook)
    .bind(actor)
    .bind(&now)
    .bind(&origin.import_id)
    .bind(&origin.rev)
    .bind(owner.to_string())
    .execute(pool)
    .await
    .context("creating a playbook draft")
    .map_err(DraftError::Internal)?;
    if inserted.rows_affected() == 0 {
        return Err(DraftError::Conflict(format!("draft {id:?} already exists")));
    }

    save_version(pool, id, files, actor, None).await
}

/// Store a save: tar the file map, compile it, and write the next version row either way. A
/// `base_version` that is not the newest save is refused with [`DraftError::StaleBase`] carrying
/// the version that overtook it, so the writer re-reads and merges instead of clobbering the other
/// editor; `None` appends onto whatever is latest.
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
        let tar_gz = crate::playbooks::packs::tar_pack_tree(&root)
            .context("taring the draft tree")
            .map_err(DraftError::Internal)?;
        if tar_gz.len() > MAX_PACK_TAR_BYTES {
            return Err(DraftError::Invalid(format!(
                "the draft tarball is {} bytes, over the {MAX_PACK_TAR_BYTES}-byte limit",
                tar_gz.len()
            )));
        }
        Ok::<_, DraftError>((tar_gz, compile_tree(&root)))
    })
    .await
    .context("joining the draft compile worker")
    .map_err(DraftError::Internal)?;
    let (
        tar_gz,
        CompiledTree {
            params_schema,
            schema_digest,
            graph,
            diagnostics,
            agent,
        },
    ) = compiled?;

    let core_rev = crate::playbooks::registry::core_rev().map_err(DraftError::Internal)?;
    let tar_digest = content_digest(&tar_gz);
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
    let version: i64 = sqlx::query_scalar(
        r#"INSERT INTO playbook_draft_versions (draft_id, version, tar_gz, tar_digest, tar_bytes,
                                                params_schema, schema_digest, graph, diagnostics,
                                                agent_backend, agent_sandbox_image, core_rev,
                                                created_by, created_at, agent_requirements)
           VALUES ($1, $14, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $15)
           RETURNING version"#,
    )
    .bind(id)
    .bind(&tar_gz)
    .bind(&tar_digest)
    .bind(tar_gz.len() as i64)
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
    let rows = sqlx::query(&format!(
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
    let row = sqlx::query(&format!(
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
        r#"SELECT version, tar_digest, schema_digest, diagnostics, core_rev, created_by, created_at
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
    let Some((head, pack)) = materialize(pool, id, version)
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
        files: crate::playbooks::packs::read_tree(pack.path())?,
    }))
}

/// Recompute the exposure of one stored draft version with the linked engine, from the exact bytes
/// a launch of it would run. `None` when the draft or the version is unknown.
pub async fn exposure_of(
    pool: &PgPool,
    id: &str,
    version: i64,
) -> Result<Option<crate::playbooks::exposure::Extraction>, DraftError> {
    let Some((_, pack)) = materialize(pool, id, Some(version))
        .await
        .map_err(DraftError::Internal)?
    else {
        return Ok(None);
    };
    let extraction =
        tokio::task::spawn_blocking(move || crate::playbooks::exposure::extract(pack.path(), None))
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

/// Who saved a materialized version, and when.
struct MaterializedHead {
    version: i64,
    saved_by: Option<String>,
    saved_at: String,
    diagnostics: Vec<Diagnostic>,
}

/// Unpack a stored version into a scratch tree.
async fn materialize(
    pool: &PgPool,
    id: &str,
    version: Option<i64>,
) -> Result<Option<(MaterializedHead, MaterializedPack)>> {
    let row = match version {
        Some(v) => sqlx::query(
            "SELECT version, created_by, created_at, diagnostics, tar_gz \
             FROM playbook_draft_versions WHERE draft_id = $1 AND version = $2",
        )
        .bind(id)
        .bind(v),
        None => sqlx::query(
            "SELECT version, created_by, created_at, diagnostics, tar_gz \
             FROM playbook_draft_versions WHERE draft_id = $1 ORDER BY version DESC LIMIT 1",
        )
        .bind(id),
    }
    .fetch_optional(pool)
    .await
    .context("reading a draft tarball")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let version: i64 = row.try_get("version")?;
    let diagnostics: serde_json::Value = row.try_get("diagnostics")?;
    let head = MaterializedHead {
        version,
        saved_by: row.try_get("created_by")?,
        saved_at: row.try_get("created_at")?,
        diagnostics: serde_json::from_value(diagnostics)
            .context("decoding stored draft diagnostics")?,
    };
    let tar_gz: Vec<u8> = row.try_get("tar_gz")?;
    let pack = crate::playbooks::packs::unpack_to_scratch(&tar_gz)
        .with_context(|| format!("unpacking draft {id} version {version}"))?;
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
            let pack = crate::playbooks::registry::materialize(pool, &playbook)
                .await
                .map_err(DraftError::Internal)?
                .ok_or_else(|| {
                    DraftError::NotFound(format!(
                        "draft {id} came from playbook {playbook}, which is no longer registered"
                    ))
                })?;
            Ok(Some(OriginFiles {
                kind: OriginKind::Playbook,
                reference: playbook,
                rev: origin.current_rev,
                files: crate::playbooks::packs::read_tree(pack.path())?,
            }))
        }
        OriginKind::Import => {
            let import = origin.import_id.ok_or_else(|| {
                DraftError::Internal(anyhow::anyhow!("origin without an import id"))
            })?;
            let tar_gz: Option<Vec<u8>> =
                sqlx::query_scalar("SELECT tar_gz FROM pack_imports WHERE id = $1")
                    .bind(&import)
                    .fetch_optional(pool)
                    .await
                    .context("reading the import tarball a draft came from")
                    .map_err(DraftError::Internal)?;
            let tar_gz = tar_gz.ok_or_else(|| {
                DraftError::NotFound(format!(
                    "draft {id} came from import {import}, which no longer exists"
                ))
            })?;
            let pack = crate::playbooks::packs::unpack_to_scratch(&tar_gz)
                .context("unpacking the import a draft came from")
                .map_err(DraftError::Internal)?;
            Ok(Some(OriginFiles {
                kind: OriginKind::Import,
                reference: import,
                rev: origin.rev,
                files: crate::playbooks::packs::read_tree(pack.path())?,
            }))
        }
    }
}

/// One stored version's tarball, exactly the bytes the save stored. `None` when the draft or the
/// version is unknown.
pub async fn tarball(
    pool: &PgPool,
    id: &str,
    version: Option<i64>,
) -> Result<Option<(i64, Vec<u8>)>> {
    let row = match version {
        Some(v) => sqlx::query(
            "SELECT version, tar_gz FROM playbook_draft_versions WHERE draft_id = $1 AND version = $2",
        )
        .bind(id)
        .bind(v),
        None => sqlx::query(
            "SELECT version, tar_gz FROM playbook_draft_versions WHERE draft_id = $1 \
             ORDER BY version DESC LIMIT 1",
        )
        .bind(id),
    }
    .fetch_optional(pool)
    .await
    .context("reading a draft tarball for download")?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some((row.try_get("version")?, row.try_get("tar_gz")?)))
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
        r#"INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at)
           SELECT $3, tar_gz, tar_digest, tar_bytes, $4 FROM playbook_draft_versions
           WHERE draft_id = $1 AND version = $2
           ON CONFLICT (issue_slug) DO UPDATE SET
               tar_gz = excluded.tar_gz, digest = excluded.digest, bytes = excluded.bytes,
               created_at = excluded.created_at"#,
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

    let row = sqlx::query(
        r#"SELECT version, tar_gz FROM playbook_draft_versions
           WHERE draft_id = $1 AND schema_digest IS NOT NULL ORDER BY version DESC LIMIT 1"#,
    )
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
    let tar_gz: Vec<u8> = row
        .try_get("tar_gz")
        .map_err(|e| DraftError::Internal(e.into()))?;

    let title = format!("[pack] {id}");
    let body = format!(
        "Exported from the controller's authoring studio: draft `{id}` at version {version}.\n\n\
         The diff is the pack. Merge it, then import it at `{repo}`/`{path}` to register it; the \
         draft retires when that import lands.\n"
    );
    let (branch_repo, branch_path, branch_key) =
        (repo.to_string(), path.to_string(), format!("draft/{id}"));
    let pushed = tokio::task::spawn_blocking(move || {
        let pack = crate::playbooks::packs::unpack_to_scratch(&tar_gz)
            .context("unpacking the draft for export")
            .map_err(DraftError::Internal)?;
        let scratch = tempfile::tempdir()
            .context("draft export scratch dir")
            .map_err(DraftError::Internal)?;
        let out = if branch_path.is_empty() {
            scratch.path().to_path_buf()
        } else {
            scratch.path().join(&branch_path)
        };
        copy_tree(pack.path(), &out).map_err(DraftError::Internal)?;
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

/// Copy a tree of regular files, creating `to`.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry.context("reading a tree entry")?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skeleton() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("crucible.toml".to_string(), SKELETON_MANIFEST.to_string()),
            ("workflow.star".to_string(), SKELETON_WORKFLOW.to_string()),
        ])
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
    fn a_file_map_that_escapes_the_pack_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        for bad in ["../escape.star", "/etc/passwd", ""] {
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
            history[0].tar_digest, history[1].tar_digest,
            "an edited file is different bytes"
        );
        assert_eq!(
            history[1].tar_digest, history[1].tar_digest,
            "a digest is stable"
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
                tar_gz: &import_tar,
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

    /// A save's bytes come back as the bytes: the tarball a download serves is the one the version
    /// stored, and it unpacks to that save's files.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn every_version_downloads_as_the_tarball_it_stored(pool: PgPool) {
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
        save_version(&pool, "studio", second, None, Some(1))
            .await
            .expect("save");

        let (version, bytes) = tarball(&pool, "studio", None)
            .await
            .expect("tarball")
            .expect("a version");
        assert_eq!(version, 2);
        let pack = crate::playbooks::packs::unpack_to_scratch(&bytes).expect("unpack");
        let files = crate::playbooks::packs::read_tree(pack.path()).expect("read");
        assert_eq!(
            files.get("skills/read/SKILL.md").map(String::as_str),
            Some("read it\n")
        );

        let (version, first) = tarball(&pool, "studio", Some(1))
            .await
            .expect("tarball")
            .expect("a version");
        assert_eq!(version, 1);
        let pack = crate::playbooks::packs::unpack_to_scratch(&first).expect("unpack");
        assert!(
            !crate::playbooks::packs::read_tree(pack.path())
                .expect("read")
                .contains_key("skills/read/SKILL.md"),
            "version 1 is version 1"
        );
        assert!(
            tarball(&pool, "studio", Some(9))
                .await
                .expect("tarball")
                .is_none()
        );
        assert!(
            tarball(&pool, "nope", None)
                .await
                .expect("tarball")
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
}
