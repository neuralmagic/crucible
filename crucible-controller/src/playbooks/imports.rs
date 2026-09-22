//! Proposed pack imports. A row here is one fetch-and-compile, frozen at the commit the ref
//! resolved to: the tarball, the params schema, the compiled graph and the engine's diagnostics
//! are stored once, so the preview a link opens is the pack that was proposed even after the
//! branch moves.
//!
//! A row is `pending` until someone resolves it. [`register`] completes it through
//! [`crate::playbooks::registry::register`], the one registration path, and stamps the row `registered`;
//! [`discard`] stamps it `discarded`. Either way it takes no further transitions. [`open_as_draft`]
//! seeds a draft from the frozen tarball, which is how a human edits what an agent proposed.

use crate::playbooks::drafts::{DraftError, DraftSeed, SavedVersion};
use crate::playbooks::plan_graph::WorkflowGraphDto;
use crate::playbooks::preview::PackPreview;
use crate::playbooks::registry::{
    PackGit, PackSource, RegisterError, RegisterPlaybook, Registered, validate_path,
};
use crate::wire_enum::wire_enum;
use anyhow::{Context, Result};
use crucible_contract::content_digest;
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;

/// Where a proposed import stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum ImportStatus {
    Pending,
    Registered,
    Discarded,
}

wire_enum!(ImportStatus, "pack import status", both, {
    ImportStatus::Pending => "pending",
    ImportStatus::Registered => "registered",
    ImportStatus::Discarded => "discarded",
});

/// Why an import operation was refused. The API maps [`NotFound`](ImportError::NotFound) to 404,
/// [`Conflict`](ImportError::Conflict) to 409, and hands the wrapped refusals to the mappers the
/// registration and draft paths already have.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Register(RegisterError),
    #[error(transparent)]
    Draft(DraftError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<RegisterError> for ImportError {
    fn from(e: RegisterError) -> Self {
        match e {
            RegisterError::Internal(e) => ImportError::Internal(e),
            other => ImportError::Register(other),
        }
    }
}

impl From<DraftError> for ImportError {
    fn from(e: DraftError) -> Self {
        match e {
            DraftError::Internal(e) => ImportError::Internal(e),
            other => ImportError::Draft(other),
        }
    }
}

/// One proposed import, without its tarball.
#[derive(Debug, Clone, PartialEq)]
pub struct PackImport {
    pub id: String,
    pub repo: String,
    pub git_ref: Option<String>,
    pub path: String,
    /// The commit everything below was taken at.
    pub rev: String,
    pub tar_digest: String,
    pub params_schema: Option<serde_json::Value>,
    pub schema_digest: Option<String>,
    pub graph: Option<WorkflowGraphDto>,
    pub diagnostics: Vec<String>,
    /// The substrate the pack's `[agent]` asks for; `None` when its manifest did not parse.
    pub agent: Option<crate::playbooks::dispatch::PackAgent>,
    /// The credentials the pack's manifest declares, frozen at the proposed rev.
    pub declared_secrets: Vec<crate::secrets::manifest::DeclaredSecret>,
    /// What a run of this proposal would be allowed to write and reach. `None` when the manifest
    /// could not be loaded; the diagnostics say why.
    pub exposure: Option<crate::playbooks::exposure::Exposure>,
    pub exposure_digest: Option<String>,
    pub core_rev: String,
    pub status: ImportStatus,
    pub playbook: Option<String>,
    pub draft_id: Option<String>,
    pub owner: crate::authz::model::Principal,
    pub proposed_by: Option<String>,
    pub created_at: String,
    pub resolved_by: Option<String>,
    pub resolved_at: Option<String>,
}

const COLUMNS: &str = "id, repo, git_ref, path, rev, tar_digest, params_schema, schema_digest, \
                       graph, diagnostics, agent_backend, agent_sandbox_image, agent_requirements, declared_secrets, \
                       exposure, exposure_digest, core_rev, status, playbook, draft_id, owner, proposed_by, created_at, resolved_by, \
                       resolved_at";

impl PackImport {
    fn from_row(row: &sqlx::postgres::PgRow) -> Result<Self> {
        let graph: Option<serde_json::Value> = row.try_get("graph")?;
        let diagnostics: serde_json::Value = row.try_get("diagnostics")?;
        let declared_secrets: serde_json::Value = row.try_get("declared_secrets")?;
        Ok(PackImport {
            id: row.try_get("id")?,
            repo: row.try_get("repo")?,
            git_ref: row.try_get("git_ref")?,
            path: row.try_get("path")?,
            rev: row.try_get("rev")?,
            tar_digest: row.try_get("tar_digest")?,
            params_schema: row.try_get("params_schema")?,
            schema_digest: row.try_get("schema_digest")?,
            graph: graph
                .map(serde_json::from_value)
                .transpose()
                .context("decoding a stored import graph")?,
            diagnostics: serde_json::from_value(diagnostics)
                .context("decoding stored import diagnostics")?,
            agent: crate::playbooks::dispatch::agent_from_row(row)?,
            declared_secrets: serde_json::from_value(declared_secrets)
                .context("decoding a stored import's declared secrets")?,
            exposure: row
                .try_get::<Option<serde_json::Value>, _>("exposure")?
                .map(crate::playbooks::exposure::Exposure::from_value)
                .transpose()?,
            exposure_digest: row.try_get("exposure_digest")?,
            core_rev: row.try_get("core_rev")?,
            status: row.try_get("status")?,
            playbook: row.try_get("playbook")?,
            draft_id: row.try_get("draft_id")?,
            owner: crate::authz::model::Principal::parse(&row.try_get::<String, _>("owner")?)?,
            proposed_by: row.try_get("proposed_by")?,
            created_at: row.try_get("created_at")?,
            resolved_by: row.try_get("resolved_by")?,
            resolved_at: row.try_get("resolved_at")?,
        })
    }

    /// A resolved import is done: it registered or it was discarded, and neither can be undone.
    fn ensure_pending(&self) -> Result<(), ImportError> {
        match self.status {
            ImportStatus::Pending => Ok(()),
            other => Err(ImportError::Conflict(format!(
                "import {} is {}; a resolved import takes no further transitions",
                self.id,
                other.as_str()
            ))),
        }
    }
}

/// Fetch a pack at a ref, compile it with the pinned engine, and store the result as a pending
/// import. The compile is the same one the preview gate renders, so a source the engine refuses
/// still lands a row: its diagnostics are what the proposal is.
pub async fn propose(
    pool: &PgPool,
    git: &PackGit,
    source: PackSource<'_>,
    actor: Option<&str>,
    owner: &crate::authz::model::Principal,
) -> Result<PackImport, ImportError> {
    validate_path(source.path).map_err(RegisterError::Invalid)?;
    let core_rev = crate::playbooks::registry::core_rev()?;

    let owned = (
        source.repo.to_string(),
        source.git_ref.map(str::to_string),
        source.path.to_string(),
    );
    let git = git.clone();
    let fetched = tokio::task::spawn_blocking(move || {
        let (repo, git_ref, path) = owned;
        let fetched = crate::playbooks::registry::fetch_pack(
            &git,
            PackSource {
                repo: &repo,
                git_ref: git_ref.as_deref(),
                path: &path,
                expected_rev: None,
            },
        )?;
        let preview = crate::playbooks::preview::preview_pack(
            fetched.pack.path(),
            &BTreeMap::new(),
            crate::playbooks::preview::Unvalued::Refuse,
        )?;
        Ok::<_, RegisterError>((fetched.rev, fetched.tar_gz, preview))
    })
    .await
    .context("joining the pack import worker")?;
    let (rev, tar_gz, preview) = fetched?;
    let (repo, git_ref, path) = (source.repo, source.git_ref, source.path);

    let id = uuid::Uuid::now_v7().to_string();
    let tar_digest = content_digest(&tar_gz);
    let graph = preview
        .graph
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .context("serializing the import graph")?;
    let diagnostics =
        serde_json::to_value(&preview.diagnostics).context("serializing the import diagnostics")?;
    let declared_secrets = serde_json::to_value(&preview.declared_secrets)
        .context("serializing the import's declared secrets")?;
    let exposure = preview
        .exposure
        .as_ref()
        .map(|e| e.to_value())
        .transpose()?;
    let now = crate::clock::now_rfc3339();

    let row = sqlx::query(&format!(
        r#"INSERT INTO pack_imports (id, repo, git_ref, path, rev, tar_gz, tar_digest, tar_bytes,
                                     params_schema, schema_digest, graph, diagnostics,
                                     agent_backend, agent_sandbox_image, declared_secrets,
                                     exposure, exposure_digest,
                                     core_rev, status, proposed_by, created_at, agent_requirements,
                                     owner)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18,
                   'pending', $19, $20, $21, $22)
           RETURNING {COLUMNS}"#
    ))
    .bind(&id)
    .bind(repo)
    .bind(git_ref)
    .bind(path)
    .bind(&rev)
    .bind(&tar_gz)
    .bind(&tar_digest)
    .bind(tar_gz.len() as i64)
    .bind(&preview.params_schema)
    .bind(&preview.schema_digest)
    .bind(&graph)
    .bind(&diagnostics)
    .bind(preview.agent.as_ref().map(|a| a.backend.clone()))
    .bind(preview.agent.as_ref().and_then(|a| a.sandbox_image.clone()))
    .bind(&declared_secrets)
    .bind(&exposure)
    .bind(&preview.exposure_digest)
    .bind(&core_rev)
    .bind(actor)
    .bind(&now)
    .bind(preview.agent.as_ref().map(|a| a.requirements_json()))
    .bind(owner.to_string())
    .fetch_one(pool)
    .await
    .context("storing a pack import")?;
    Ok(PackImport::from_row(&row)?)
}

/// One import row.
pub async fn get(pool: &PgPool, id: &str) -> Result<Option<PackImport>> {
    let row = sqlx::query(&format!("SELECT {COLUMNS} FROM pack_imports WHERE id = $1"))
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("reading a pack import")?;
    row.as_ref().map(PackImport::from_row).transpose()
}

/// Every import that is still waiting on a human, newest first.
pub async fn pending(pool: &PgPool) -> Result<Vec<PackImport>> {
    let rows = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM pack_imports WHERE status = 'pending' ORDER BY created_at DESC, id"
    ))
    .fetch_all(pool)
    .await
    .context("listing pending pack imports")?;
    rows.iter().map(PackImport::from_row).collect()
}

/// Recompile the frozen tarball with `params`. A pack with required parameters compiles no plan
/// until they are supplied, so the preview gate asks for this rather than re-fetching the ref:
/// the bytes stay the ones the import was taken at, and nothing is stored.
pub async fn compile(
    pool: &PgPool,
    id: &str,
    params: BTreeMap<String, String>,
) -> Result<PackPreview, ImportError> {
    let tar_gz = tarball(pool, id)
        .await?
        .ok_or_else(|| ImportError::NotFound(format!("no pack import {id:?}")))?;
    let preview = tokio::task::spawn_blocking(move || {
        let pack = crate::playbooks::packs::unpack_to_scratch(&tar_gz)
            .context("unpacking the frozen import tarball")
            .map_err(RegisterError::Internal)?;
        crate::playbooks::preview::preview_pack(
            pack.path(),
            &params,
            crate::playbooks::preview::Unvalued::Refuse,
        )
    })
    .await
    .context("joining the pack import compile worker")?;
    Ok(preview?)
}

async fn tarball(pool: &PgPool, id: &str) -> Result<Option<Vec<u8>>> {
    let row = sqlx::query("SELECT tar_gz FROM pack_imports WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .context("reading a pack import tarball")?;
    row.map(|r| r.try_get("tar_gz"))
        .transpose()
        .map_err(Into::into)
}

/// Register a pending import: the pack is re-fetched at the frozen rev through the one
/// registration path, so a ref that moved since the proposal is refused rather than pinned.
pub async fn register(
    pool: &PgPool,
    git: &PackGit,
    id: &str,
    registry_id: &str,
    description: &str,
    actor: Option<&str>,
) -> Result<Registered, ImportError> {
    // The row lock serializes register against discard: a discard landing mid-register blocks
    // here and then matches zero pending rows, taking its own Conflict path.
    let mut tx = pool
        .begin()
        .await
        .context("locking a pack import to register it")?;
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM pack_imports WHERE id = $1 FOR UPDATE"
    ))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await
    .context("reading a pack import for registration")?;
    let import = row
        .as_ref()
        .map(PackImport::from_row)
        .transpose()?
        .ok_or_else(|| ImportError::NotFound(format!("no pack import {id:?}")))?;
    import.ensure_pending()?;

    let registered = crate::playbooks::registry::register(
        pool,
        git,
        RegisterPlaybook {
            id: registry_id.to_string(),
            owner: import.owner.clone(),
            description: description.to_string(),
            repo: import.repo.clone(),
            git_ref: import.git_ref.clone(),
            path: import.path.clone(),
            expected_rev: Some(import.rev.clone()),
            accept_exposure_digest: import.exposure_digest.clone(),
        },
        actor,
    )
    .await?;

    sqlx::query(
        r#"UPDATE pack_imports
           SET status = 'registered', playbook = $2, resolved_by = $3, resolved_at = $4
           WHERE id = $1 AND status = 'pending'"#,
    )
    .bind(id)
    .bind(&registered.id)
    .bind(actor)
    .bind(crate::clock::now_rfc3339())
    .execute(&mut *tx)
    .await
    .context("stamping a pack import registered")?;
    tx.commit()
        .await
        .context("committing a pack import registration")?;
    Ok(registered)
}

/// Drop a pending import. The row stays for the audit trail; nothing else can act on it.
pub async fn discard(
    pool: &PgPool,
    id: &str,
    actor: Option<&str>,
) -> Result<PackImport, ImportError> {
    let row = sqlx::query(&format!(
        r#"UPDATE pack_imports SET status = 'discarded', resolved_by = $2, resolved_at = $3
           WHERE id = $1 AND status = 'pending' RETURNING {COLUMNS}"#
    ))
    .bind(id)
    .bind(actor)
    .bind(crate::clock::now_rfc3339())
    .fetch_optional(pool)
    .await
    .context("discarding a pack import")?;
    match row {
        Some(row) => Ok(PackImport::from_row(&row)?),
        None => {
            let existing = get(pool, id)
                .await?
                .ok_or_else(|| ImportError::NotFound(format!("no pack import {id:?}")))?;
            existing.ensure_pending()?;
            Err(ImportError::Conflict(format!(
                "import {id} was resolved while this discard ran; re-read it"
            )))
        }
    }
}

/// Seed a draft from a pending import's frozen tarball: what an agent proposed becomes what a
/// human edits, on the authoring surface both of them save through.
pub async fn open_as_draft(
    pool: &PgPool,
    id: &str,
    draft_id: &str,
    description: &str,
    actor: Option<&str>,
) -> Result<SavedVersion, ImportError> {
    let import = get(pool, id)
        .await?
        .ok_or_else(|| ImportError::NotFound(format!("no pack import {id:?}")))?;
    import.ensure_pending()?;
    let tar_gz = tarball(pool, id)
        .await?
        .ok_or_else(|| ImportError::NotFound(format!("no pack import {id:?}")))?;
    let saved = crate::playbooks::drafts::create(
        pool,
        draft_id,
        description,
        DraftSeed::Import {
            id,
            tar_gz: &tar_gz,
        },
        actor,
        &import.owner,
    )
    .await?;
    sqlx::query("UPDATE pack_imports SET draft_id = $2 WHERE id = $1")
        .bind(id)
        .bind(draft_id)
        .execute(pool)
        .await
        .context("recording the draft an import was opened as")?;
    Ok(saved)
}

/// Propose an import and open it as a draft in one motion: the pack a person named by repo, ref
/// and path lands as a draft they can edit, and the import row it minted rides along as the
/// provenance of where those bytes came from. Both halves are the existing paths, in order.
pub async fn propose_as_draft(
    pool: &PgPool,
    git: &PackGit,
    source: PackSource<'_>,
    draft_id: &str,
    description: &str,
    actor: Option<&str>,
    owner: &crate::authz::model::Principal,
) -> Result<(PackImport, SavedVersion), ImportError> {
    crate::playbooks::drafts::ensure_available(pool, draft_id, description).await?;
    let import = propose(pool, git, source, actor, owner).await?;
    let saved = open_as_draft(pool, &import.id, draft_id, description, actor).await?;
    Ok((import, saved))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(repo: &str) -> PackSource<'_> {
        PackSource {
            repo,
            git_ref: Some("main"),
            path: "",
            expected_rev: None,
        }
    }
    use crate::testing::fixtures::{
        PLAYBOOK_REPO_MANIFEST, WORKFLOW_NO_PARAMS, git_pack_repo, run_git,
    };
    use std::path::Path;

    /// A real git repo holding a one-file playbook pack. Returns its path.
    fn fixture_repo(dir: &Path, source: &str) -> String {
        git_pack_repo(dir, PLAYBOOK_REPO_MANIFEST, source)
    }

    /// Commit `source` over the pack's workflow file, moving the branch.
    fn move_branch(repo: &str, source: &str) {
        std::fs::write(Path::new(repo).join("workflow.star"), source).expect("source");
        run_git(&["-C", repo, "add", "-A"]);
        run_git(&["-C", repo, "commit", "--quiet", "-m", "edit"]);
    }

    /// The gate names a pack's credentials before an operator has recompiled anything, so the
    /// declarations have to be frozen onto the row with the rest of the preview.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_import_freezes_the_credentials_the_pack_declares(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest = format!(
            "{PLAYBOOK_REPO_MANIFEST}\n[[secret]]\nname = \"pr_token\"\nenv = \"AUTORESEARCH_PR_TOKEN\"\n\n             [[secret]]\nname = \"registry\"\nkind = \"registry_authfile\"\npath = \"/etc/quay.json\"\n"
        );
        let repo = git_pack_repo(dir.path(), &manifest, WORKFLOW_NO_PARAMS);

        let import = propose(
            &pool,
            &PackGit::default(),
            source(&repo),
            Some("agent"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("proposes");

        let declared: Vec<&str> = import
            .declared_secrets
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(declared, vec!["pr_token", "registry"]);
        assert_eq!(
            import.declared_secrets[1].kind,
            crate::secrets::SecretKind::RegistryAuthfile
        );
        assert_eq!(
            import.declared_secrets[0].projection,
            Some((
                crate::secrets::ProjectionKind::Env,
                "AUTORESEARCH_PR_TOKEN".to_string()
            ))
        );

        let reread = get(&pool, &import.id)
            .await
            .expect("reads")
            .expect("the row is there");
        assert_eq!(
            reread.declared_secrets, import.declared_secrets,
            "the declarations survive the round trip through the row"
        );
    }

    /// The whole point of the row: the fetch and the compile happen once, and what the link opens
    /// afterwards is the pack that was proposed, not whatever the branch holds now.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_import_freezes_the_pack_at_the_rev_it_resolved(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), WORKFLOW_NO_PARAMS);

        let import = propose(
            &pool,
            &PackGit::default(),
            source(&repo),
            Some("agent"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("proposes");
        move_branch(&repo, &format!("{WORKFLOW_NO_PARAMS}# moved on\n"));

        assert_eq!(import.status, ImportStatus::Pending);
        assert_eq!(import.proposed_by.as_deref(), Some("agent"));
        assert_eq!(import.rev.len(), 40);
        assert!(import.params_schema.is_some(), "the form was extracted");
        assert!(import.graph.is_some(), "the plan compiled");

        let reread = get(&pool, &import.id)
            .await
            .expect("reads")
            .expect("the row is there");
        assert_eq!(reread, import, "a re-read is the frozen proposal, verbatim");

        let refused = register(
            &pool,
            &PackGit::default(),
            &import.id,
            "survey",
            "a survey",
            Some("wren"),
        )
        .await
        .expect_err("the ref moved under the proposal");
        assert!(
            matches!(refused, ImportError::Register(RegisterError::RevMoved(_))),
            "{refused:?}"
        );
        assert_eq!(
            get(&pool, &import.id)
                .await
                .expect("reads")
                .expect("row")
                .status,
            ImportStatus::Pending,
            "a refused registration leaves the proposal open"
        );
    }

    /// Registering completes through the one registration path and closes the row; nothing may
    /// act on it afterwards.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_registered_import_takes_no_further_transitions(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), WORKFLOW_NO_PARAMS);

        let import = propose(
            &pool,
            &PackGit::default(),
            source(&repo),
            Some("agent"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("proposes");
        let registered = register(
            &pool,
            &PackGit::default(),
            &import.id,
            "survey",
            "a survey",
            Some("wren"),
        )
        .await
        .expect("registers");
        assert_eq!(registered.rev, import.rev, "pinned at what was proposed");
        assert!(
            crate::playbooks::registry::get(&pool, "survey")
                .await
                .expect("reads")
                .is_some(),
            "the registry holds the pack"
        );

        let row = get(&pool, &import.id).await.expect("reads").expect("row");
        assert_eq!(row.status, ImportStatus::Registered);
        assert_eq!(row.playbook.as_deref(), Some("survey"));
        assert_eq!(row.resolved_by.as_deref(), Some("wren"));

        for refusal in [
            register(
                &pool,
                &PackGit::default(),
                &import.id,
                "survey-two",
                "again",
                Some("wren"),
            )
            .await
            .err(),
            discard(&pool, &import.id, Some("wren")).await.err(),
            open_as_draft(&pool, &import.id, "survey-draft", "edit it", Some("wren"))
                .await
                .err(),
        ] {
            assert!(
                matches!(refusal, Some(ImportError::Conflict(_))),
                "{refusal:?}"
            );
        }
        assert!(
            crate::playbooks::registry::get(&pool, "survey-two")
                .await
                .expect("reads")
                .is_none(),
            "the refused re-registration registered nothing"
        );
    }

    /// A discarded proposal is equally closed, and nothing registered on the way out.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_discarded_import_takes_no_further_transitions(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), WORKFLOW_NO_PARAMS);

        let import = propose(
            &pool,
            &PackGit::default(),
            source(&repo),
            Some("agent"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("proposes");
        let discarded = discard(&pool, &import.id, Some("wren"))
            .await
            .expect("discards");
        assert_eq!(discarded.status, ImportStatus::Discarded);
        assert_eq!(discarded.resolved_by.as_deref(), Some("wren"));

        let again = discard(&pool, &import.id, Some("wren"))
            .await
            .expect_err("already resolved");
        assert!(matches!(again, ImportError::Conflict(_)), "{again:?}");
        let refused = register(
            &pool,
            &PackGit::default(),
            &import.id,
            "survey",
            "a survey",
            Some("wren"),
        )
        .await
        .expect_err("already resolved");
        assert!(matches!(refused, ImportError::Conflict(_)), "{refused:?}");
        assert!(
            crate::playbooks::registry::get(&pool, "survey")
                .await
                .expect("reads")
                .is_none()
        );
        assert!(pending(&pool).await.expect("lists").is_empty());
    }

    /// A human editing an agent's proposal starts from the bytes that were proposed, not from
    /// whatever the branch moved on to.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn opening_a_draft_seeds_it_from_the_frozen_tarball(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), WORKFLOW_NO_PARAMS);

        let import = propose(
            &pool,
            &PackGit::default(),
            source(&repo),
            Some("agent"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("proposes");
        move_branch(&repo, &format!("{WORKFLOW_NO_PARAMS}# moved on\n"));
        open_as_draft(&pool, &import.id, "survey-draft", "edit it", Some("wren"))
            .await
            .expect("opens as a draft");

        let head = crate::playbooks::drafts::files(&pool, "survey-draft", None)
            .await
            .expect("reads")
            .expect("the draft is there");
        assert_eq!(head.version, 1);
        assert_eq!(head.saved_by.as_deref(), Some("wren"));
        assert_eq!(
            head.files.get("workflow.star").map(String::as_str),
            Some(WORKFLOW_NO_PARAMS),
            "the draft holds the proposed source, not the moved branch"
        );

        let row = get(&pool, &import.id).await.expect("reads").expect("row");
        assert_eq!(row.draft_id.as_deref(), Some("survey-draft"));
        assert_eq!(
            row.status,
            ImportStatus::Pending,
            "forking the bytes is not resolving the proposal"
        );
    }

    /// The rail reads pending rows only, newest first, with whoever proposed them.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn pending_lists_what_is_still_waiting_on_a_human(pool: PgPool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = fixture_repo(dir.path(), WORKFLOW_NO_PARAMS);

        let first = propose(
            &pool,
            &PackGit::default(),
            source(&repo),
            Some("agent"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("proposes");
        let second = propose(
            &pool,
            &PackGit::default(),
            source(&repo),
            Some("wren"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("proposes");
        discard(&pool, &first.id, Some("wren"))
            .await
            .expect("discards");

        let waiting = pending(&pool).await.expect("lists");
        assert_eq!(
            waiting.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec![second.id.as_str()]
        );
        assert_eq!(waiting[0].proposed_by.as_deref(), Some("wren"));
    }
}
