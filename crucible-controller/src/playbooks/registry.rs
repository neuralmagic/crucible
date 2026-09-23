//! The playbook registry: a git-pinned pack plus the launch form the pinned engine extracted from
//! it. Registration clones the repo at a ref, tars the pack subtree, extracts the params schema
//! through the linked engine (`declared_params`, the library form of `crucible plan params`)
//! against the pack's declared workflow source, and stores tarball + schema + digests in one
//! transaction — a pack whose source does not compile registers nothing and carries the engine's
//! `file:line:col` error back to the caller.
//!
//! The stored `core_rev` is the engine pin the schema was extracted with. The engine is linked
//! into this binary, so its pin only changes when this binary changes; [`rederive_stale`]
//! runs once at daemon startup and re-extracts every row whose pin no longer matches, keeping the
//! old schema when the new engine refuses the pack.
//!
//! Everything the launch path needs is a function here — [`schema`], [`materialize`] — so no other
//! module reaches into the table.

use crate::playbooks::packs::MaterializedPack;
use anyhow::{Context, Result};
use crucible::plan::starlark::declared_params;
use crucible_contract::content_digest;
use serde::Deserialize;
use sqlx::{PgPool, Row};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

/// Upper bound on a registry id. It rides a launch key and a pod label hint, so it stays short.
const ID_MAX_LEN: usize = 64;

/// Upper bound on a registered pack's gzipped tarball. A playbook pack is authored files, not a
/// data set; a repo subtree that misses by this much is a mis-pointed `path`.
pub(crate) const MAX_PACK_TAR_BYTES: usize = 8 * 1024 * 1024;

/// The manifest table name a registered pack must declare its graph in.
const PLAYBOOK_WORKFLOW_TYPE: &str = "playbook";

/// Why a registration was refused. The API maps [`Invalid`](RegisterError::Invalid) and
/// [`Compile`](RegisterError::Compile) to 422 (the caller's pack or arguments), [`Fetch`] to 502
/// (the git remote), [`Conflict`](RegisterError::Conflict) to 409, and anything else to 500.
#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Compile(String),
    #[error("{0}")]
    Fetch(String),
    #[error("the ref now resolves to {0}")]
    RevMoved(String),
    #[error(
        "this pack's declared exposure changed from {} to {}; review the new declaration and \
         re-register accepting that digest",
        prior.as_deref().unwrap_or("absent-legacy"),
        next.as_deref().unwrap_or("absent-legacy")
    )]
    ExposureChanged {
        prior: Option<String>,
        next: Option<String>,
    },
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// What to register, owned so the fetch + extraction can run on a blocking thread.
#[derive(Debug, Clone)]
pub struct RegisterPlaybook {
    pub id: String,
    /// The principal the registration is owned to; unchanged when the id is re-registered.
    pub owner: crate::authz::model::Principal,
    pub description: String,
    /// `owner/repo` slug or a clone URL ([`crate::runs::engine::repo_clone_url`] resolves it).
    pub repo: String,
    /// Branch or tag; `None` = the repo's default branch.
    pub git_ref: Option<String>,
    /// Pack directory inside the repo; empty = the repo root.
    pub path: String,
    /// The commit a preview was taken against. When set and the fresh clone resolves elsewhere,
    /// registration is refused with [`RegisterError::RevMoved`].
    pub expected_rev: Option<String>,
    /// The exposure digest the caller reviewed. Re-registering an id whose declared exposure
    /// changed is refused with [`RegisterError::ExposureChanged`] unless this names the new digest.
    pub accept_exposure_digest: Option<String>,
}

/// What a registration landed: the resolved pin, the stored digests, and whether the form changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registered {
    pub id: String,
    pub rev: String,
    pub tar_digest: String,
    pub schema_digest: String,
    /// True when an id that was already registered now serves a different form.
    pub schema_changed: bool,
    /// The digest of the exposure this revision discloses. `None` is absent-legacy: nothing was
    /// stored and nothing is enforced against it.
    pub exposure_digest: Option<String>,
    /// True when an id that was already registered now discloses a different exposure.
    pub exposure_changed: bool,
}

/// One registry row, without its tarball.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookRow {
    pub id: String,
    pub description: String,
    pub repo: String,
    pub git_ref: Option<String>,
    pub rev: String,
    pub path: String,
    pub tar_digest: String,
    pub schema_digest: String,
    /// The substrate the pack's `[agent]` asks for, read off its manifest at registration. `None`
    /// when it was never recorded: a manifest that did not parse, or a row the startup backfill
    /// has not reached.
    pub agent: Option<crate::playbooks::dispatch::PackAgent>,
    pub core_rev: String,
    /// `None` is absent-legacy.
    pub exposure_digest: Option<String>,
    pub owner: crate::authz::model::Principal,
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

const PLAYBOOK_COLS: &str = "id, description, repo, git_ref, rev, path, tar_digest, schema_digest, \
     agent_backend, agent_sandbox_image, agent_requirements, core_rev, exposure_digest, owner, \
     created_by, created_at, updated_at";

impl PlaybookRow {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self> {
        Ok(PlaybookRow {
            id: row.try_get("id")?,
            description: row.try_get("description")?,
            repo: row.try_get("repo")?,
            git_ref: row.try_get("git_ref")?,
            rev: row.try_get("rev")?,
            path: row.try_get("path")?,
            tar_digest: row.try_get("tar_digest")?,
            schema_digest: row.try_get("schema_digest")?,
            agent: crate::playbooks::dispatch::agent_from_row(row)?,
            core_rev: row.try_get("core_rev")?,
            exposure_digest: row.try_get("exposure_digest")?,
            owner: crate::authz::model::Principal::parse(&row.try_get::<String, _>("owner")?)?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

/// The engine revision this controller was built from, the source commit build.rs stamped.
/// The engine is linked into this binary, so the rev a pack is registered against is the
/// build's own. A build that could not name its commit refuses rather than recording a
/// placeholder.
pub fn core_rev() -> Result<String> {
    let rev = env!("CRUCIBLE_SOURCE_REV");
    if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(rev.to_string())
    } else {
        Err(anyhow::Error::msg(format!(
            "this build carries no source revision ({rev}); set CRUCIBLE_GIT_SHA at build time"
        )))
    }
}

/// A registry id is a lowercase slug: it is embedded in the launch key `playbook:{id}:{uuid}` and
/// in pod name/label hints, so it may carry neither the key's delimiters nor anything a DNS label
/// would refuse.
pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > ID_MAX_LEN {
        return Err(format!("id must be 1..={ID_MAX_LEN} characters"));
    }
    let ok_first = id
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !ok_first {
        return Err("id must start with a lowercase letter or digit".to_string());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err("id may only contain lowercase letters, digits, and '-'".to_string());
    }
    Ok(())
}

/// A pack path is a relative subtree of the repo. Empty is the repo root.
pub(crate) fn validate_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Ok(());
    }
    let p = Path::new(path);
    if p.is_absolute() || p.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err("path must be a relative directory inside the repo".to_string());
    }
    Ok(())
}

/// The pack subtree of a checkout, refusing a `path` that leaves it.
pub(crate) fn pack_root(checkout: &Path, path: &str) -> Result<PathBuf, RegisterError> {
    validate_path(path).map_err(RegisterError::Invalid)?;
    let root = if path.is_empty() {
        checkout.to_path_buf()
    } else {
        checkout.join(path)
    };
    if !root.is_dir() {
        return Err(RegisterError::Invalid(format!(
            "the pack directory {path:?} does not exist at this ref"
        )));
    }
    Ok(root)
}

/// `git` with the given args, mapping a nonzero exit (or a missing `git`) to [`RegisterError::Fetch`]
/// with the command's own stderr.
fn git(args: &[&str]) -> Result<String, RegisterError> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| RegisterError::Fetch(format!("running `git` (is it on PATH?): {e}")))?;
    if !out.status.success() {
        return Err(RegisterError::Fetch(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A scratch clone of a pack repo at a ref: the commit it resolved to, and the tree it landed in.
/// The checkout lives as long as this value.
#[derive(Debug)]
pub(crate) struct Checkout {
    pub rev: String,
    scratch: tempfile::TempDir,
}

impl Checkout {
    /// The working tree the clone landed in.
    pub fn path(&self) -> PathBuf {
        self.scratch.path().join("repo")
    }
}

/// The credential a pack clone presents to github.com: the App installation token when the
/// deploy configured one, else the PAT chain, else nothing. Resolved once per request, before the
/// blocking clone. Other hosts and local paths never see it.
#[derive(Clone, Default)]
pub struct PackGit {
    token: Option<String>,
}

impl PackGit {
    pub(crate) async fn resolve(
        app: Option<&crate::secrets::github_app::GithubAppTokenSource>,
    ) -> Result<Self> {
        let token = crate::runs::engine::resolve_pack_pr_token_for(app).await?;
        Ok(PackGit { token })
    }

    fn clone_url(&self, repo: &str) -> String {
        let url = crate::runs::engine::repo_clone_url(repo);
        match &self.token {
            Some(t) if url.starts_with("https://github.com/") => {
                url.replacen("https://", &format!("https://x-access-token:{t}@"), 1)
            }
            _ => url,
        }
    }

    /// The token never leaves this value through an error message.
    fn scrub(&self, err: RegisterError) -> RegisterError {
        match (err, &self.token) {
            (RegisterError::Fetch(msg), Some(t)) => RegisterError::Fetch(msg.replace(t, "***")),
            (err, _) => err,
        }
    }

    /// Clone `repo` at `git_ref` into a scratch dir and resolve the commit. Blocking.
    pub(crate) fn fetch_checkout(
        &self,
        repo: &str,
        git_ref: Option<&str>,
    ) -> Result<Checkout, RegisterError> {
        if repo.starts_with('-') {
            return Err(RegisterError::Invalid(format!(
                "repo {repo:?} starts with '-'"
            )));
        }
        let scratch = tempfile::tempdir()
            .context("playbook fetch scratch dir")
            .map_err(RegisterError::Internal)?;
        let checkout = scratch.path().join("repo");
        let url = self.clone_url(repo);
        let checkout_arg = checkout.to_string_lossy().to_string();
        let mut args = vec!["clone", "--quiet", "--depth", "1"];
        if let Some(r) = git_ref {
            args.extend_from_slice(&["--branch", r]);
        }
        args.extend_from_slice(&["--", url.as_str(), checkout_arg.as_str()]);
        git(&args).map_err(|e| self.scrub(e))?;
        let rev = git(&["-C", &checkout_arg, "rev-parse", "HEAD"])?;
        Ok(Checkout { rev, scratch })
    }
}

/// One directory of a checkout that could be registered: a `crucible.toml` declaring a playbook
/// workflow whose source file is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportCandidate {
    /// The pack directory relative to the checkout root; empty = the root itself.
    pub path: String,
    /// The `[workflow].file` it declares, relative to the pack directory.
    pub workflow_file: String,
}

/// How deep under a checkout root a pack directory is looked for.
const CANDIDATE_MAX_DEPTH: usize = 6;

/// How many pack directories one import lists. A repo past this is not a pack repo.
const CANDIDATE_MAX: usize = 64;

/// Every directory of `checkout` holding a registrable playbook pack, shallowest first. A bounded
/// walk: an import fetches whatever URL the caller names, so neither depth nor breadth is trusted.
pub(crate) fn enumerate_candidates(checkout: &Path) -> Vec<ImportCandidate> {
    let mut found = Vec::new();
    let mut queue = std::collections::VecDeque::from([(checkout.to_path_buf(), 0usize)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if found.len() >= CANDIDATE_MAX {
            break;
        }
        if let Ok(source) = workflow_source(&dir)
            && let Ok(rel) = source.strip_prefix(&dir)
        {
            let path = dir
                .strip_prefix(checkout)
                .unwrap_or(&dir)
                .to_string_lossy()
                .to_string();
            found.push(ImportCandidate {
                path,
                workflow_file: rel.to_string_lossy().to_string(),
            });
        }
        if depth >= CANDIDATE_MAX_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.file_name().is_some_and(|n| n != ".git"))
            .collect();
        children.sort();
        for child in children {
            queue.push_back((child, depth + 1));
        }
    }
    found
}

/// A pack fetched at a ref: the commit it resolved to, and the pack subtree tarred.
pub(crate) struct FetchedPack {
    pub rev: String,
    pub tar_gz: Vec<u8>,
    /// The scratch checkout's pack tree, alive while this value is.
    pub pack: MaterializedPack,
}

/// Which pack to fetch, and the commit the caller expects to find it at.
#[derive(Debug, Clone, Copy)]
pub struct PackSource<'a> {
    pub repo: &'a str,
    pub git_ref: Option<&'a str>,
    pub path: &'a str,
    pub expected_rev: Option<&'a str>,
}

/// Clone `req`'s repo at its ref, resolve the commit, and tar the pack subtree. Blocking.
pub(crate) fn fetch_pack(git: &PackGit, req: PackSource<'_>) -> Result<FetchedPack, RegisterError> {
    let checkout = git.fetch_checkout(req.repo, req.git_ref)?;
    if let Some(expected) = req.expected_rev
        && expected != checkout.rev
    {
        return Err(RegisterError::RevMoved(checkout.rev));
    }

    let root = pack_root(&checkout.path(), req.path)?;
    let tar_gz = crate::playbooks::packs::tar_pack_tree(&root)
        .context("taring the playbook pack tree")
        .map_err(RegisterError::Internal)?;
    if tar_gz.len() > MAX_PACK_TAR_BYTES {
        return Err(RegisterError::Invalid(format!(
            "the pack tarball is {} bytes, over the {MAX_PACK_TAR_BYTES}-byte registry limit",
            tar_gz.len()
        )));
    }
    // The stored bytes go through the same traversal rejection every materialization runs, before
    // they become the durable pack.
    let pack = crate::playbooks::packs::unpack_to_scratch(&tar_gz)
        .context("validating the playbook pack tarball")
        .map_err(RegisterError::Internal)?;
    Ok(FetchedPack {
        rev: checkout.rev,
        tar_gz,
        pack,
    })
}

/// The `[workflow]` table of a pack manifest, partially parsed: the registry needs the declared
/// type and source file, and ignores everything else the manifest carries.
#[derive(Debug, Deserialize)]
struct ManifestWorkflow {
    workflow: Option<WorkflowTable>,
}

#[derive(Debug, Deserialize)]
struct WorkflowTable {
    #[serde(rename = "type")]
    workflow_type: Option<String>,
    file: Option<String>,
}

/// The workflow family accepted at a pack boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PackWorkflowKind {
    Playbook,
    #[cfg(feature = "autoresearch")]
    Autoresearch,
}

impl PackWorkflowKind {
    fn declared_type(self) -> &'static str {
        match self {
            Self::Playbook => PLAYBOOK_WORKFLOW_TYPE,
            #[cfg(feature = "autoresearch")]
            Self::Autoresearch => "autoresearch",
        }
    }
}

/// The workflow source a registered pack declares, resolved against the pack root.
pub(crate) fn workflow_source(pack: &Path) -> Result<PathBuf, RegisterError> {
    workflow_source_for(pack, PackWorkflowKind::Playbook)
}

/// Resolve the workflow source while keeping the accepted family explicit at non-registry doors.
pub(crate) fn workflow_source_for(
    pack: &Path,
    kind: PackWorkflowKind,
) -> Result<PathBuf, RegisterError> {
    let manifest = pack.join("crucible.toml");
    let text = std::fs::read_to_string(&manifest).map_err(|e| {
        RegisterError::Invalid(format!("reading the pack's crucible.toml failed: {e}"))
    })?;
    let parsed: ManifestWorkflow = toml::from_str(&text)
        .map_err(|e| RegisterError::Invalid(format!("parsing the pack's crucible.toml: {e}")))?;
    let workflow = parsed.workflow.ok_or_else(|| {
        RegisterError::Invalid("the pack declares no [workflow] table".to_string())
    })?;
    let declared_type = workflow.workflow_type.as_deref().unwrap_or_default();
    let expected = kind.declared_type();
    if declared_type != expected {
        return Err(RegisterError::Invalid(format!(
            "this operation takes [workflow] type = \"{expected}\" packs; \
             this one declares {declared_type:?}"
        )));
    }
    let file = workflow.file.filter(|f| !f.is_empty()).ok_or_else(|| {
        RegisterError::Invalid(
            "the pack's [workflow] declares no `file`, so it has no source to extract params from"
                .to_string(),
        )
    })?;
    validate_path(&file).map_err(|_| {
        RegisterError::Invalid("[workflow].file must be inside the pack".to_string())
    })?;
    let source = pack.join(&file);
    if !source.is_file() {
        return Err(RegisterError::Invalid(format!(
            "[workflow].file {file:?} is not in the pack"
        )));
    }
    Ok(source)
}

/// Extract the pack's params schema through the linked engine (`declared_params`, the library form
/// of `plan params`) and return it with its digest. A refused source carries the engine's
/// diagnostic verbatim — that `file:line:col` compile error is the whole reason extraction happens
/// at registration. Blocking.
fn extract_params_schema(pack: &Path) -> Result<(serde_json::Value, String), RegisterError> {
    let source = workflow_source(pack)?;
    let text = std::fs::read_to_string(&source)
        .with_context(|| format!("reading {}", source.display()))
        .map_err(RegisterError::Internal)?;
    let schema = declared_params(&text, &source)
        .map_err(|e| RegisterError::Compile(crucible::errors::report(&e)))?;
    let digest = schema_digest(&schema).map_err(RegisterError::Internal)?;
    Ok((schema, digest))
}

/// Compute the pack's declared exposure with the linked engine; a manifest it refuses refuses
/// the registration. Blocking.
///
/// A launch of a registered playbook renders no `--pr-repo`, so neither does this: the `draft-pr`
/// default a run resolves is the manifest's own `[publish].pr_repo` or nothing.
fn extract_exposure(pack: &Path) -> Result<crate::playbooks::exposure::Exposure, RegisterError> {
    crate::playbooks::exposure::extract(pack, None).map_err(|e| match e {
        crate::playbooks::exposure::ExtractError::Refused(msg) => {
            RegisterError::Compile(format!("extracting the pack's exposure: {msg}"))
        }
        crate::playbooks::exposure::ExtractError::Internal(e) => RegisterError::Internal(e),
    })
}

/// The schema's content digest. `serde_json` writes map keys in order, so the same schema always
/// digests the same.
pub(crate) fn schema_digest(schema: &serde_json::Value) -> Result<String> {
    let bytes = serde_json::to_vec(schema).context("serializing the params schema")?;
    Ok(content_digest(&bytes))
}

/// Register (or re-pin) a playbook: fetch the pack at its ref, extract its params schema with the
/// pinned engine, and store tarball + schema + digests in one transaction. Re-POSTing an id is the
/// pin-bump path; the ack says whether the form changed.
pub async fn register(
    pool: &PgPool,
    git: &PackGit,
    req: RegisterPlaybook,
    actor: Option<&str>,
) -> Result<Registered, RegisterError> {
    validate_id(&req.id).map_err(RegisterError::Invalid)?;
    if req.description.trim().is_empty() {
        return Err(RegisterError::Invalid(
            "description must be non-empty".to_string(),
        ));
    }
    validate_path(&req.path).map_err(RegisterError::Invalid)?;
    if let Some(msg) = blocking_registration(pool, &req.id, &req.repo, &req.path)
        .await
        .map_err(RegisterError::Internal)?
    {
        return Err(RegisterError::Conflict(msg));
    }
    let core_rev = core_rev().map_err(RegisterError::Internal)?;

    let git = git.clone();
    let fetched = tokio::task::spawn_blocking(move || {
        let fetched = fetch_pack(
            &git,
            PackSource {
                repo: &req.repo,
                git_ref: req.git_ref.as_deref(),
                path: &req.path,
                expected_rev: req.expected_rev.as_deref(),
            },
        )?;
        let (schema, digest) = extract_params_schema(fetched.pack.path())?;
        let exposure = crate::playbooks::exposure::Extraction::Declared(extract_exposure(
            fetched.pack.path(),
        )?);
        let agent = crate::playbooks::dispatch::pack_agent(fetched.pack.path())
            .map_err(|e| RegisterError::Invalid(format!("{e:#}")))?;
        Ok::<_, RegisterError>((
            req,
            fetched.rev,
            fetched.tar_gz,
            schema,
            digest,
            exposure,
            agent,
        ))
    })
    .await
    .context("joining the playbook registration worker")
    .map_err(RegisterError::Internal)?;
    let (req, rev, tar_gz, schema, schema_digest, exposure, agent) = fetched?;
    let (exposure_json, exposure_digest) = exposure.stored().map_err(RegisterError::Internal)?;

    let tar_digest = content_digest(&tar_gz);
    let now = crate::clock::now_rfc3339();
    let mut tx = pool
        .begin()
        .await
        .context("opening the playbook registration transaction")
        .map_err(RegisterError::Internal)?;
    let prior_row = sqlx::query(
        "SELECT schema_digest, exposure_digest FROM playbooks WHERE id = $1 FOR UPDATE",
    )
    .bind(&req.id)
    .fetch_optional(&mut *tx)
    .await
    .context("reading the prior playbook row")
    .map_err(RegisterError::Internal)?;
    let prior: Option<String> = prior_row.as_ref().map(|r| r.get("schema_digest"));
    let prior_exposure: Option<Option<String>> =
        prior_row.as_ref().map(|r| r.get("exposure_digest"));
    let exposure_changed = prior_exposure
        .as_ref()
        .is_some_and(|p| *p != exposure_digest);
    let accepted = exposure_digest.is_some() && req.accept_exposure_digest == exposure_digest;
    if exposure_changed && !accepted {
        return Err(RegisterError::ExposureChanged {
            prior: prior_exposure.flatten(),
            next: exposure_digest,
        });
    }
    if let Some(msg) = blocking_registration(&mut *tx, &req.id, &req.repo, &req.path)
        .await
        .map_err(RegisterError::Internal)?
    {
        return Err(RegisterError::Conflict(msg));
    }
    sqlx::query(
        r#"INSERT INTO playbooks (id, description, repo, git_ref, rev, path, tar_gz, tar_digest,
                                  tar_bytes, params_schema, schema_digest, agent_backend,
                                  agent_sandbox_image, core_rev, created_by, created_at,
                                  updated_at, exposure, exposure_digest, agent_requirements, owner)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $16,
                   $17, $18, $19, $20)
           ON CONFLICT (id) DO UPDATE SET
               description = excluded.description, repo = excluded.repo,
               git_ref = excluded.git_ref, rev = excluded.rev, path = excluded.path,
               tar_gz = excluded.tar_gz, tar_digest = excluded.tar_digest,
               tar_bytes = excluded.tar_bytes, params_schema = excluded.params_schema,
               schema_digest = excluded.schema_digest,
               agent_backend = excluded.agent_backend,
               agent_sandbox_image = excluded.agent_sandbox_image,
               agent_requirements = excluded.agent_requirements,
               core_rev = excluded.core_rev, updated_at = excluded.updated_at,
               exposure = excluded.exposure, exposure_digest = excluded.exposure_digest"#,
    )
    .bind(&req.id)
    .bind(req.description.trim())
    .bind(&req.repo)
    .bind(req.git_ref.as_deref())
    .bind(&rev)
    .bind(&req.path)
    .bind(&tar_gz)
    .bind(&tar_digest)
    .bind(tar_gz.len() as i64)
    .bind(&schema)
    .bind(&schema_digest)
    .bind(&agent.backend)
    .bind(agent.sandbox_image.as_deref())
    .bind(&core_rev)
    .bind(actor)
    .bind(&now)
    .bind(&exposure_json)
    .bind(&exposure_digest)
    .bind(agent.requirements_json())
    .bind(req.owner.to_string())
    .execute(&mut *tx)
    .await
    .context("storing the playbook row")
    .map_err(RegisterError::Internal)?;
    tx.commit()
        .await
        .context("committing the playbook registration")
        .map_err(RegisterError::Internal)?;

    // The other half of graduation: the merged pack is registered, so the draft it was exported
    // from stops being the place to edit it.
    retire_matching(pool, &req.repo, &req.path)
        .await
        .map_err(RegisterError::Internal)?;

    Ok(Registered {
        id: req.id,
        rev,
        tar_digest,
        schema_changed: prior.is_some_and(|p| p != schema_digest),
        schema_digest,
        exposure_changed,
        exposure_digest,
    })
}

/// Every registered playbook, newest registration first.
pub async fn list(pool: &PgPool) -> Result<Vec<PlaybookRow>> {
    let rows = sqlx::query(&format!(
        "SELECT {PLAYBOOK_COLS} FROM playbooks ORDER BY created_at DESC, id"
    ))
    .fetch_all(pool)
    .await
    .context("listing playbooks")?;
    rows.iter().map(PlaybookRow::from_row).collect()
}

/// One registered playbook's row, or `None` when the id is unknown.
pub async fn get(pool: &PgPool, id: &str) -> Result<Option<PlaybookRow>> {
    let row = sqlx::query(&format!(
        "SELECT {PLAYBOOK_COLS} FROM playbooks WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("reading a playbook")?;
    row.as_ref().map(PlaybookRow::from_row).transpose()
}

/// The stored params schema, verbatim as the engine printed it. `None` when the id is unknown.
pub async fn schema(pool: &PgPool, id: &str) -> Result<Option<serde_json::Value>> {
    let row = sqlx::query("SELECT params_schema FROM playbooks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("reading a playbook schema")?;
    row.map(|r| r.try_get("params_schema"))
        .transpose()
        .context("decoding a playbook schema")
}

/// Unpack a registered playbook's pack into a scratch tree. `None` when the id is unknown.
pub async fn materialize(pool: &PgPool, id: &str) -> Result<Option<MaterializedPack>> {
    let row = sqlx::query("SELECT tar_gz FROM playbooks WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("reading a playbook tarball")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let tar_gz: Vec<u8> = row.try_get("tar_gz")?;
    crate::playbooks::packs::unpack_to_scratch(&tar_gz)
        .with_context(|| format!("unpacking the stored pack for playbook {id}"))
        .map(Some)
}

/// Unpack only when the registry still serves the revision the caller inspected.
pub(crate) async fn materialize_at_rev(
    pool: &PgPool,
    id: &str,
    rev: &str,
    tar_digest: Option<&str>,
) -> Result<Option<MaterializedPack>> {
    let row = sqlx::query(
        "SELECT tar_gz FROM playbooks WHERE id = $1 AND rev = $2 \
         AND ($3::text IS NULL OR tar_digest = $3)",
    )
    .bind(id)
    .bind(rev)
    .bind(tar_digest)
    .fetch_optional(pool)
    .await
    .context("reading a playbook tarball at an expected revision")?;
    let Some(row) = row else { return Ok(None) };
    let tar_gz: Vec<u8> = row.try_get("tar_gz")?;
    crate::playbooks::packs::unpack_to_scratch(&tar_gz)
        .with_context(|| format!("unpacking the stored pack for playbook {id} at {rev}"))
        .map(Some)
}

/// Read every UTF-8 file in a registered pack at its pinned revision. The same bounded pack bytes
/// that launches and template drafts consume back this inspector view.
pub async fn files(
    pool: &PgPool,
    id: &str,
) -> Result<Option<std::collections::BTreeMap<String, String>>> {
    let Some(pack) = materialize(pool, id).await? else {
        return Ok(None);
    };
    crate::playbooks::packs::read_tree(pack.path())
        .map(Some)
        .map_err(anyhow::Error::new)
}

/// Copy a registered pack's stored tarball into `slug`'s `pack_tarballs` row, so a launch runs the
/// bytes it was authorized against even after the registry is re-pinned. `INSERT .. SELECT`: the
/// blob never round-trips through the controller. `false` when the id is unknown.
pub(crate) async fn copy_pack_to<'e>(
    ex: impl sqlx::PgExecutor<'e>,
    id: &str,
    slug: &str,
) -> Result<bool> {
    let res = sqlx::query(
        r#"INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at)
           SELECT $2, tar_gz, tar_digest, tar_bytes, $3 FROM playbooks WHERE id = $1
           ON CONFLICT (issue_slug) DO UPDATE SET
               tar_gz = excluded.tar_gz, digest = excluded.digest, bytes = excluded.bytes,
               created_at = excluded.created_at"#,
    )
    .bind(id)
    .bind(slug)
    .bind(crate::clock::now_rfc3339())
    .execute(ex)
    .await
    .context("copying a registered pack to a launch")?;
    Ok(res.rows_affected() > 0)
}

/// Re-extract the params schema of every row whose stored `core_rev` is not this binary's pin, and
/// stamp the new pin. A pack the new engine refuses keeps its old schema and its old pin, logged:
/// a pin bump must not leave a registered playbook with no form to render.
pub async fn rederive_stale(pool: &PgPool) -> Result<usize> {
    let core_rev = core_rev()?;
    let rows = sqlx::query("SELECT id, tar_gz FROM playbooks WHERE core_rev <> $1 ORDER BY id")
        .bind(&core_rev)
        .fetch_all(pool)
        .await
        .context("listing playbooks registered against an older engine revision")?;
    let mut updated = 0usize;
    for row in rows {
        let id: String = row.try_get("id")?;
        let tar_gz: Vec<u8> = row.try_get("tar_gz")?;
        let extracted = tokio::task::spawn_blocking(move || {
            let pack = crate::playbooks::packs::unpack_to_scratch(&tar_gz)
                .context("unpacking a stored playbook pack")
                .map_err(RegisterError::Internal)?;
            let (schema, digest) = extract_params_schema(pack.path())?;
            let exposure =
                crate::playbooks::exposure::Extraction::Declared(extract_exposure(pack.path())?);
            Ok::<_, RegisterError>((schema, digest, exposure))
        })
        .await
        .context("joining the playbook re-derivation worker")?;
        let (schema, digest, exposure) = match extracted {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    playbook = %id,
                    error = %format!("{e:#}"),
                    "playbook schema re-derivation failed; keeping the stored schema"
                );
                continue;
            }
        };
        let (exposure_json, exposure_digest) = exposure.stored()?;
        sqlx::query(
            r#"UPDATE playbooks
               SET params_schema = $2, schema_digest = $3, core_rev = $4, updated_at = $5,
                   exposure = $6, exposure_digest = $7
               WHERE id = $1"#,
        )
        .bind(&id)
        .bind(&schema)
        .bind(&digest)
        .bind(&core_rev)
        .bind(crate::clock::now_rfc3339())
        .bind(&exposure_json)
        .bind(&exposure_digest)
        .execute(pool)
        .await
        .with_context(|| format!("re-deriving the schema for playbook {id}"))?;
        updated += 1;
    }
    Ok(updated)
}

// --- launch-time validation ---------------------------------------------------

/// One rejected param value, addressed by the form field the launcher typed it into. The launch
/// endpoint answers 422 with a list of these; the SPA form pins them to their inputs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
pub struct FieldError {
    /// The param name, or empty when the schema rejected the object as a whole.
    pub field: String,
    pub message: String,
}

/// Validate a launcher's values against a pack's stored params schema. The schema is the pack's
/// declaration, extracted at registration; this is the enforcement the SPA form is only a
/// convenience over. Compiled per call — a params schema is a handful of properties, and a cache
/// would be one more thing to invalidate on a pin bump.
pub fn validate_params(
    schema: &serde_json::Value,
    params: &std::collections::BTreeMap<String, String>,
) -> Result<serde_json::Value, Vec<FieldError>> {
    let instance = serde_json::Value::Object(
        params
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect(),
    );
    let validator = jsonschema::validator_for(schema).map_err(|e| {
        vec![FieldError {
            field: String::new(),
            message: format!("the stored params schema is not a usable JSON Schema: {e}"),
        }]
    })?;
    let errors: Vec<FieldError> = validator
        .iter_errors(&instance)
        .map(|e| FieldError {
            field: field_of(&e),
            message: e.to_string(),
        })
        .collect();
    if errors.is_empty() {
        Ok(instance)
    } else {
        Err(errors)
    }
}

/// Which param a validation error belongs to. A value error points at `/name`; a missing required
/// property points at the object itself and names the property in its kind, so both address a
/// form field rather than "the request body".
fn field_of(e: &jsonschema::ValidationError<'_>) -> String {
    if let jsonschema::error::ValidationErrorKind::Required { property } = e.kind()
        && let Some(name) = property.as_str()
    {
        return name.to_string();
    }
    e.instance_path()
        .as_str()
        .trim_start_matches('/')
        .replace("~1", "/")
        .replace("~0", "~")
}

/// The refusal a registration of `id` earns from a live draft holding it, or `None`. Drafts and
/// registered playbooks share the launch-key namespace in both directions. The other half of
/// graduation is exempt: registering a graduated draft's own target retires that draft, so it may
/// reuse the id.
pub async fn blocking_registration<'e, E>(
    exec: E,
    id: &str,
    repo: &str,
    path: &str,
) -> Result<Option<String>>
where
    E: sqlx::PgExecutor<'e>,
{
    let row = sqlx::query(
        "SELECT graduation_repo, graduation_path, graduation_pr_url FROM playbook_drafts \
         WHERE id = $1 AND retired_at IS NULL",
    )
    .bind(id)
    .fetch_optional(exec)
    .await
    .context("reading a live draft that may hold a registry id")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let graduated = row
        .try_get::<Option<String>, _>("graduation_pr_url")
        .context("reading a draft's graduation url")?
        .is_some();
    let grad_repo: Option<String> = row
        .try_get("graduation_repo")
        .context("reading a draft's graduation repo")?;
    let grad_path: Option<String> = row
        .try_get("graduation_path")
        .context("reading a draft's graduation path")?;
    if graduated && grad_repo.as_deref() == Some(repo) && grad_path.as_deref() == Some(path) {
        return Ok(None);
    }
    Ok(Some(format!(
        "{id} is a live draft; a playbook shares the launch-key namespace, so graduate or delete the draft first"
    )))
}

/// Retire every graduated draft whose export target is the pack just registered. The merged pack
/// is the source of truth from that point on, so its draft stops being editable.
pub async fn retire_matching(pool: &PgPool, repo: &str, path: &str) -> Result<u64> {
    let res = sqlx::query(
        r#"UPDATE playbook_drafts SET retired_at = $3, updated_at = $3
           WHERE graduation_repo = $1 AND graduation_path = $2
             AND graduation_pr_url IS NOT NULL AND retired_at IS NULL"#,
    )
    .bind(repo)
    .bind(path)
    .bind(crate::clock::now_rfc3339())
    .execute(pool)
    .await
    .context("retiring graduated drafts")?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::fixtures::{
        PLAYBOOK_REPO_MANIFEST, WORKFLOW_TOPIC, WORKFLOW_TOPIC_DEPTH, WORKFLOW_UNPARSABLE,
        git_pack_repo, run_git, schema_of,
    };

    /// The schema is the pack's declaration; a launcher's values are checked against it field by
    /// field, so the form can pin each message to the input that produced it.
    #[test]
    fn validate_params_names_every_offending_field() {
        let schema: serde_json::Value = serde_json::from_str(
            r#"{"type":"object",
                "properties":{"topic":{"type":"string","pattern":"^[a-z]+$"},
                              "depth":{"type":"string"}},
                "required":["topic","depth"],
                "additionalProperties":false}"#,
        )
        .expect("schema");

        let ok = std::collections::BTreeMap::from([
            ("topic".to_string(), "attention".to_string()),
            ("depth".to_string(), "deep".to_string()),
        ]);
        assert_eq!(
            validate_params(&schema, &ok).expect("valid"),
            serde_json::json!({"topic": "attention", "depth": "deep"})
        );

        let bad =
            std::collections::BTreeMap::from([("topic".to_string(), "ATTENTION".to_string())]);
        let errors = validate_params(&schema, &bad).expect_err("refused");
        let fields: Vec<&str> = errors.iter().map(|e| e.field.as_str()).collect();
        assert!(fields.contains(&"topic"), "{errors:?}");
        assert!(
            fields.contains(&"depth"),
            "a missing required value addresses its own field: {errors:?}"
        );

        let extra = std::collections::BTreeMap::from([
            ("topic".to_string(), "attention".to_string()),
            ("depth".to_string(), "deep".to_string()),
            ("smuggled".to_string(), "yes".to_string()),
        ]);
        assert!(
            validate_params(&schema, &extra).is_err(),
            "a param the pack never declared is a mistake, not a passthrough"
        );
    }

    /// The tarball of a playbook pack whose source the engine refuses.
    fn broken_pack_tar_gz(dir: &Path) -> Vec<u8> {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(dir.join("crucible.toml"), PLAYBOOK_REPO_MANIFEST).expect("manifest");
        std::fs::write(dir.join("workflow.star"), WORKFLOW_UNPARSABLE).expect("source");
        crate::playbooks::packs::tar_pack_tree(dir).expect("tar")
    }

    /// A real git repo holding a one-file playbook pack whose workflow is [`WORKFLOW_TOPIC`].
    fn fixture_repo(dir: &Path, manifest: &str) -> String {
        git_pack_repo(dir, manifest, WORKFLOW_TOPIC)
    }

    /// The playbook manifest bounding `count` draft PRs against `owner/repo` and disclosing
    /// `GH_TOKEN` as an agent-context credential.
    fn manifest_bounding_draft_prs(count: u32) -> String {
        format!(
            "{PLAYBOOK_REPO_MANIFEST}\n[outputs.draft-pr]\ncount = {count}\ntarget = {{ fixed = \
             \"owner/repo\" }}\n\n[[capabilities.secret]]\nname = \"GH_TOKEN\"\ncontext = \
             \"agent\"\nsystem = \"github\"\nscope = \"repo\"\n"
        )
    }

    /// Registration stores what the engine computes from the manifest, and a bump that widens it
    /// is reported before anyone accepts it.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_bump_that_widens_the_exposure_is_reported_and_presented(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), &manifest_bounding_draft_prs(1));

        let first = register(
            &pool,
            &PackGit::default(),
            request("survey", &repo),
            Some("wren"),
        )
        .await
        .expect("register");
        assert!(!first.exposure_changed, "nothing to change from");
        let first_digest = first.exposure_digest.clone().expect("a digest was stored");

        let same = register(
            &pool,
            &PackGit::default(),
            request("survey", &repo),
            Some("wren"),
        )
        .await
        .expect("re-register");
        assert!(
            !same.exposure_changed && same.exposure_digest.as_deref() == Some(&first_digest),
            "the same pack under the same engine discloses the same thing: {same:?}"
        );

        std::fs::write(
            Path::new(&repo).join("crucible.toml"),
            manifest_bounding_draft_prs(9),
        )
        .expect("widen the manifest");
        run_git(&["-C", &repo, "commit", "--quiet", "-am", "widen"]);
        let held = register(
            &pool,
            &PackGit::default(),
            request("survey", &repo),
            Some("wren"),
        )
        .await
        .expect_err("a widened bound is held until someone accepts it");
        let next = match &held {
            RegisterError::ExposureChanged { prior, next } => {
                assert_eq!(prior.as_deref(), Some(first_digest.as_str()));
                next.clone().expect("the new digest is named")
            }
            other => panic!("expected ExposureChanged, got {other:?}"),
        };
        assert_ne!(next, first_digest);
        assert_eq!(
            get(&pool, "survey")
                .await
                .expect("get")
                .expect("row")
                .exposure_digest
                .as_deref(),
            Some(first_digest.as_str()),
            "a held registration leaves the row untouched"
        );

        let stale_acceptance = register(
            &pool,
            &PackGit::default(),
            RegisterPlaybook {
                owner: crate::authz::model::Principal::platform(),
                accept_exposure_digest: Some(first_digest.clone()),
                ..request("survey", &repo)
            },
            Some("wren"),
        )
        .await
        .expect_err("accepting the old digest accepts nothing");
        assert!(matches!(
            stale_acceptance,
            RegisterError::ExposureChanged { .. }
        ));

        let bumped = register(
            &pool,
            &PackGit::default(),
            RegisterPlaybook {
                owner: crate::authz::model::Principal::platform(),
                accept_exposure_digest: Some(next.clone()),
                ..request("survey", &repo)
            },
            Some("wren"),
        )
        .await
        .expect("bump");
        assert!(
            bumped.exposure_changed,
            "the accepted change is still reported: {bumped:?}"
        );
        assert_eq!(bumped.exposure_digest.as_deref(), Some(next.as_str()));
        assert_eq!(
            get(&pool, "survey")
                .await
                .expect("get")
                .expect("row")
                .exposure_digest,
            bumped.exposure_digest,
            "the registry row carries the digest the ack reported"
        );

        let stored = crate::playbooks::exposure::registered(&pool, "survey")
            .await
            .expect("exposure")
            .expect("row")
            .expect("a document");
        let lines = crate::playbooks::exposure::present(Some(&stored));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("draft-pr x9 -> owner/repo")),
            "the approval surface presents the widened bound: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("credential GH_TOKEN (agent)")),
            "{lines:?}"
        );
    }

    /// The whole registration path: the engine's schema is what lands, and a hostile source is
    /// the engine's compile error, not a controller failure.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn registration_stores_the_engines_schema_for_a_parameterized_pack(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = concat!(
            "params = {\n",
            "    \"repo\": {\"type\": \"string\", \"required\": True, \"pattern\": \"^[a-z]+/[a-z]+$\"},\n",
            "    \"limit\": {\"type\": \"string\", \"default\": \"6\"},\n",
            "}\n",
            "s = command(name = \"s\", run = \"true\")\n",
            "workflow(type = \"playbook\", tasks = [s])\n",
        );
        let repo = git_pack_repo(dir.path(), PLAYBOOK_REPO_MANIFEST, source);

        let out = register(
            &pool,
            &PackGit::default(),
            request("real", &repo),
            Some("wren"),
        )
        .await
        .expect("the engine registers the pack");

        let stored = schema(&pool, "real").await.expect("schema").expect("row");
        assert_eq!(stored["required"], serde_json::json!(["repo"]));
        assert_eq!(stored["properties"]["limit"]["default"], "6");
        assert_eq!(stored["properties"]["repo"]["pattern"], "^[a-z]+/[a-z]+$");
        assert!(!out.schema_digest.is_empty());

        // A hostile source is the engine's compile error, not a controller failure.
        let deep = format!("x = {}1\n", "lambda: ".repeat(4800));
        let hostile_dir = tempfile::tempdir().expect("tempdir");
        let hostile = git_pack_repo(hostile_dir.path(), PLAYBOOK_REPO_MANIFEST, &deep);
        let err = register(
            &pool,
            &PackGit::default(),
            request("hostile", &hostile),
            Some("wren"),
        )
        .await
        .expect_err("a hostile pack is refused");
        match err {
            RegisterError::Compile(detail) => {
                assert!(
                    detail.contains("levels deep"),
                    "engine error rides: {detail}"
                );
            }
            other => panic!("expected a compile refusal, got {other:?}"),
        }
    }

    fn request(id: &str, repo: &str) -> RegisterPlaybook {
        RegisterPlaybook {
            owner: crate::authz::model::Principal::platform(),
            id: id.to_string(),
            description: "a registered playbook".to_string(),
            repo: repo.to_string(),
            git_ref: Some("main".to_string()),
            path: String::new(),
            expected_rev: None,
            accept_exposure_digest: None,
        }
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn register_pins_the_rev_and_stores_the_schema_with_a_stable_digest(pool: PgPool) {
        let _g = crate::ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), PLAYBOOK_REPO_MANIFEST);

        let (first, second) = {
            let first = register(
                &pool,
                &PackGit::default(),
                request("survey", &repo),
                Some("wren"),
            )
            .await
            .expect("register");
            let second = register(
                &pool,
                &PackGit::default(),
                request("survey", &repo),
                Some("wren"),
            )
            .await
            .expect("re-register");
            (first, second)
        };

        let head = Command::new("git")
            .args(["-C", &repo, "rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
        assert_eq!(first.rev, head, "the resolved commit is the pin");

        let stored = schema(&pool, "survey").await.expect("schema").expect("row");
        assert_eq!(
            stored,
            schema_of(WORKFLOW_TOPIC),
            "the engine's document round-trips through JSONB"
        );
        assert_eq!(
            first.schema_digest, second.schema_digest,
            "identical content digests identically"
        );
        assert!(!second.schema_changed, "the form did not change");

        let row = get(&pool, "survey").await.expect("get").expect("row");
        assert_eq!(row.core_rev, core_rev().expect("engine revision"));
        assert_eq!(row.created_by.as_deref(), Some("wren"));
        assert_eq!(row.tar_digest, second.tar_digest, "the last pin is stored");

        // The stored tarball is the pack the schema came from.
        let pack = materialize(&pool, "survey")
            .await
            .expect("materialize")
            .expect("row");
        assert!(pack.path().join("workflow.star").is_file());
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_compile_error_fails_registration_and_stores_nothing(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = git_pack_repo(dir.path(), PLAYBOOK_REPO_MANIFEST, WORKFLOW_UNPARSABLE);

        let err = {
            register(&pool, &PackGit::default(), request("survey", &repo), None)
                .await
                .expect_err("extraction failed")
        };

        assert!(
            matches!(err, RegisterError::Compile(ref e) if e.contains("Parse error") && e.contains("workflow.star:")),
            "the engine's diagnostic rides the error verbatim: {err:#}"
        );
        assert!(
            list(&pool).await.expect("list").is_empty(),
            "nothing half-registers"
        );
    }

    /// Drafts and the registry share the launch-key namespace in both directions: a live draft
    /// refuses its id to a registration, and the one exemption is the other half of graduation,
    /// which retires the draft.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_live_draft_blocks_its_id_until_graduation_registers_its_target(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), PLAYBOOK_REPO_MANIFEST);

        let err = {
            crate::playbooks::drafts::create(
                &pool,
                "survey",
                "a drafted pack",
                crate::playbooks::drafts::DraftSeed::Skeleton,
                Some("wren"),
                &crate::authz::model::Principal::platform(),
            )
            .await
            .expect("draft");
            register(
                &pool,
                &PackGit::default(),
                request("survey", &repo),
                Some("wren"),
            )
            .await
            .expect_err("a live draft holds the id")
        };
        assert!(
            matches!(err, RegisterError::Conflict(ref m) if m.contains("live draft")),
            "the refusal names the draft: {err:#}"
        );
        assert!(
            list(&pool).await.expect("list").is_empty(),
            "nothing registered past the draft"
        );

        sqlx::query(
            "UPDATE playbook_drafts SET graduation_repo = $1, graduation_path = '', \
             graduation_pr_url = 'https://example.invalid/pr/1' WHERE id = 'survey'",
        )
        .bind(&repo)
        .execute(&pool)
        .await
        .expect("stamp graduation");
        register(
            &pool,
            &PackGit::default(),
            request("survey", &repo),
            Some("wren"),
        )
        .await
        .expect("registering the graduated target lands");
        let draft = crate::playbooks::drafts::get(&pool, "survey")
            .await
            .expect("get")
            .expect("row");
        assert!(
            draft.retired_at.is_some(),
            "the registration retired the draft"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn re_registering_re_derives_the_schema_and_reports_the_change(pool: PgPool) {
        let _g = crate::ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), PLAYBOOK_REPO_MANIFEST);
        let first = {
            register(&pool, &PackGit::default(), request("survey", &repo), None)
                .await
                .expect("register")
        };

        std::fs::write(Path::new(&repo).join("workflow.star"), WORKFLOW_TOPIC_DEPTH)
            .expect("edit source");
        run_git(&["-C", &repo, "commit", "--quiet", "-am", "add a param"]);

        let second = register(&pool, &PackGit::default(), request("survey", &repo), None)
            .await
            .expect("re-register");

        assert_ne!(second.rev, first.rev, "the pin moved");
        assert_ne!(second.tar_digest, first.tar_digest, "the pack changed");
        assert_ne!(second.schema_digest, first.schema_digest);
        assert!(second.schema_changed, "the form changed with the pack");
        assert_eq!(
            schema(&pool, "survey").await.expect("schema").expect("row"),
            schema_of(WORKFLOW_TOPIC_DEPTH)
        );
        assert_eq!(list(&pool).await.expect("list").len(), 1, "one row per id");
    }

    /// A pin bump re-derives every stale row it can: a pack the linked engine still compiles moves
    /// to the new pin with a fresh schema, and one it now refuses keeps its old schema and its old
    /// pin, so a registered playbook never loses its form.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn rederive_stale_updates_a_pin_bump_and_keeps_a_refused_pack_intact(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let good = git_pack_repo(
            &dir.path().join("good"),
            PLAYBOOK_REPO_MANIFEST,
            WORKFLOW_TOPIC_DEPTH,
        );
        let bad = git_pack_repo(
            &dir.path().join("bad"),
            PLAYBOOK_REPO_MANIFEST,
            WORKFLOW_TOPIC,
        );
        register(&pool, &PackGit::default(), request("audit", &good), None)
            .await
            .expect("register");
        register(&pool, &PackGit::default(), request("survey", &bad), None)
            .await
            .expect("register");
        // Both rows predate the running binary's pin; the older engine derived a form the current
        // one would not, and froze a pack the current one refuses.
        sqlx::query("UPDATE playbooks SET core_rev = 'older-pin'")
            .execute(&pool)
            .await
            .expect("age the rows");
        let stale = serde_json::json!({"type": "object"});
        sqlx::query("UPDATE playbooks SET params_schema = $1 WHERE id = 'audit'")
            .bind(&stale)
            .execute(&pool)
            .await
            .expect("stale the form");
        sqlx::query("UPDATE playbooks SET tar_gz = $1 WHERE id = 'survey'")
            .bind(broken_pack_tar_gz(&dir.path().join("broken")))
            .execute(&pool)
            .await
            .expect("freeze a refused pack");

        let updated = rederive_stale(&pool).await.expect("re-derive");
        assert_eq!(updated, 1, "only the pack the engine accepts moved");

        let audit = get(&pool, "audit").await.expect("get").expect("row");
        assert_eq!(audit.core_rev, core_rev().expect("engine revision"));
        assert_eq!(
            schema(&pool, "audit").await.expect("schema").expect("row"),
            schema_of(WORKFLOW_TOPIC_DEPTH)
        );

        let survey = get(&pool, "survey").await.expect("get").expect("row");
        assert_eq!(
            survey.core_rev, "older-pin",
            "the stale pin is kept, not faked"
        );
        assert_eq!(
            schema(&pool, "survey").await.expect("schema").expect("row"),
            schema_of(WORKFLOW_TOPIC),
            "the refused pack's old form still renders"
        );
    }

    /// A stored pack the engine refuses keeps its schema and its pin: re-derivation is a bump,
    /// never a demotion.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_refused_pack_keeps_the_stored_schema_and_pin(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), PLAYBOOK_REPO_MANIFEST);
        register(&pool, &PackGit::default(), request("survey", &repo), None)
            .await
            .expect("register");
        sqlx::query("UPDATE playbooks SET core_rev = 'older-pin', tar_gz = $1")
            .bind(broken_pack_tar_gz(&dir.path().join("broken")))
            .execute(&pool)
            .await
            .expect("age the row behind a refused pack");

        let updated = rederive_stale(&pool).await.expect("re-derive survives");

        assert_eq!(updated, 0, "nothing was re-derived");
        let row = get(&pool, "survey").await.expect("get").expect("row");
        assert_eq!(
            row.core_rev, "older-pin",
            "the stale pin is kept, not faked"
        );
        assert_eq!(
            schema(&pool, "survey").await.expect("schema").expect("row"),
            schema_of(WORKFLOW_TOPIC),
            "the old form still renders"
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_pack_without_a_compilable_playbook_workflow_is_refused(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, manifest, want) in [
            ("no-table", "[repo]\npath = \".\"\n", "no [workflow] table"),
            (
                "no-file",
                "[workflow]\ntype = \"playbook\"\n",
                "declares no `file`",
            ),
            (
                "wrong-type",
                "[workflow]\ntype = \"autoresearch\"\nfile = \"workflow.star\"\n",
                "takes [workflow] type = \"playbook\"",
            ),
        ] {
            let case = dir.path().join(name);
            std::fs::create_dir_all(&case).expect("case dir");
            let repo = fixture_repo(&case, manifest);
            let err = {
                register(&pool, &PackGit::default(), request(name, &repo), None)
                    .await
                    .expect_err("refused")
            };
            assert!(
                matches!(err, RegisterError::Invalid(ref e) if e.contains(want)),
                "{name}: {err:#}"
            );
        }
        assert!(list(&pool).await.expect("list").is_empty());
    }

    /// A repo whose packs sit in nested directories, next to trees that are not packs. The walk
    /// lists exactly the directories a registration would accept.
    #[test]
    fn enumerate_candidates_lists_playbook_packs_and_nothing_else() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checkout = dir.path().join("checkout");
        let write_pack = |rel: &str, manifest: &str, source: Option<&str>| {
            let root = checkout.join(rel);
            std::fs::create_dir_all(&root).expect("mkdir");
            std::fs::write(root.join("crucible.toml"), manifest).expect("manifest");
            if let Some(source) = source {
                std::fs::write(root.join("workflow.star"), source).expect("source");
            }
        };
        write_pack(
            "packs/survey",
            PLAYBOOK_REPO_MANIFEST,
            Some("params = {}\n"),
        );
        write_pack(
            "packs/nested/audit",
            PLAYBOOK_REPO_MANIFEST,
            Some("params = {}\n"),
        );
        write_pack(
            "packs/loop",
            "[workflow]\ntype = \"autoresearch\"\nfile = \"workflow.star\"\n",
            Some("params = {}\n"),
        );
        write_pack("packs/headless", PLAYBOOK_REPO_MANIFEST, None);
        std::fs::create_dir_all(checkout.join("docs")).expect("mkdir");
        std::fs::create_dir_all(checkout.join(".git/objects")).expect("mkdir");
        std::fs::write(checkout.join(".git/crucible.toml"), PLAYBOOK_REPO_MANIFEST).expect("decoy");
        std::fs::write(checkout.join(".git/workflow.star"), "params = {}\n").expect("decoy");

        let found = enumerate_candidates(&checkout);
        assert_eq!(
            found,
            vec![
                ImportCandidate {
                    path: "packs/survey".to_string(),
                    workflow_file: "workflow.star".to_string(),
                },
                ImportCandidate {
                    path: "packs/nested/audit".to_string(),
                    workflow_file: "workflow.star".to_string(),
                },
            ],
            "shallowest first, and only registrable playbook packs"
        );
    }

    /// A pack at the checkout root lists as the empty path — the same `path` a registration takes.
    #[test]
    fn enumerate_candidates_names_a_root_pack_by_the_empty_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), PLAYBOOK_REPO_MANIFEST);
        let checkout = Path::new(&repo);
        assert_eq!(
            enumerate_candidates(checkout),
            vec![ImportCandidate {
                path: String::new(),
                workflow_file: "workflow.star".to_string(),
            }]
        );
    }

    #[test]
    fn fetch_checkout_resolves_the_ref_to_a_commit_and_lands_the_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), PLAYBOOK_REPO_MANIFEST);
        let checkout = PackGit::default()
            .fetch_checkout(&repo, Some("main"))
            .expect("clone");
        let head = Command::new("git")
            .args(["-C", &repo, "rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        assert_eq!(
            checkout.rev,
            String::from_utf8_lossy(&head.stdout).trim(),
            "the resolved commit is the pin a preview quotes"
        );
        assert!(checkout.path().join("workflow.star").is_file());

        let err = PackGit::default()
            .fetch_checkout("--upload-pack=touch pwned", None)
            .expect_err("refused");
        assert!(matches!(err, RegisterError::Invalid(_)), "{err:#}");
    }

    /// A registration quoting a rev the ref no longer resolves to is refused: the preview gate
    /// must never pin bytes nobody looked at.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_moved_ref_refuses_a_previewed_registration(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), PLAYBOOK_REPO_MANIFEST);
        let previewed = PackGit::default()
            .fetch_checkout(&repo, Some("main"))
            .expect("clone")
            .rev;

        std::fs::write(
            Path::new(&repo).join("workflow.star"),
            "params = {}  # more\n",
        )
        .expect("edit source");
        run_git(&["-C", &repo, "commit", "--quiet", "-am", "move the ref"]);

        let (moved, pinned) = {
            let mut req = request("survey", &repo);
            req.expected_rev = Some(previewed.clone());
            let moved = register(&pool, &PackGit::default(), req, None)
                .await
                .expect_err("the ref moved");

            let mut req = request("survey", &repo);
            req.expected_rev = Some(
                Command::new("git")
                    .args(["-C", &repo, "rev-parse", "HEAD"])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .expect("rev-parse"),
            );
            let pinned = register(&pool, &PackGit::default(), req, None)
                .await
                .expect("register");
            (moved, pinned)
        };

        match moved {
            RegisterError::RevMoved(current) => assert_ne!(current, previewed),
            other => panic!("expected a moved-ref refusal, got {other:?}"),
        }
        assert_eq!(
            list(&pool).await.expect("list").len(),
            1,
            "only the pinned one"
        );
        assert_eq!(
            get(&pool, "survey").await.expect("get").expect("row").rev,
            pinned.rev
        );
    }

    #[test]
    fn ids_are_launch_key_safe_slugs() {
        for ok in ["survey", "a", "paper-survey-2", "0x"] {
            assert!(validate_id(ok).is_ok(), "{ok}");
        }
        for bad in ["", "Survey", "a:b", "a/b", "-lead", "a b", &"x".repeat(65)] {
            assert!(validate_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_build_revision_is_a_commit() {
        let rev = core_rev().expect("engine revision");
        assert_eq!(rev.len(), 40, "{rev}");
        assert!(rev.chars().all(|c| c.is_ascii_hexdigit()), "{rev}");
    }

    #[test]
    fn a_github_clone_carries_the_token_and_nothing_else_does() {
        let git = PackGit {
            token: Some("ghs_secret".to_string()),
        };
        assert_eq!(
            git.clone_url("neuralmagic/crucible"),
            "https://x-access-token:ghs_secret@github.com/neuralmagic/crucible.git"
        );
        assert_eq!(
            git.clone_url("https://github.com/neuralmagic/crucible.git"),
            "https://x-access-token:ghs_secret@github.com/neuralmagic/crucible.git"
        );
        assert_eq!(
            git.clone_url("https://gitlab.com/o/r.git"),
            "https://gitlab.com/o/r.git"
        );
        assert_eq!(git.clone_url("/abs/local/repo"), "/abs/local/repo");
        assert_eq!(
            PackGit::default().clone_url("neuralmagic/crucible"),
            "https://github.com/neuralmagic/crucible.git"
        );
    }

    #[test]
    fn a_failed_clone_never_echoes_the_token() {
        let git = PackGit {
            token: Some("ghs_secret".to_string()),
        };
        let err = git.scrub(RegisterError::Fetch(
            "git clone failed: fatal: https://x-access-token:ghs_secret@github.com/o/r.git".into(),
        ));
        let text = err.to_string();
        assert!(!text.contains("ghs_secret"), "{text}");
        assert!(text.contains("x-access-token:***@github.com"), "{text}");
        let other = git.scrub(RegisterError::Invalid("ghs_secret".into()));
        assert!(matches!(other, RegisterError::Invalid(_)));
    }
}
