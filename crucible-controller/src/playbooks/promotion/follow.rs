//! The promotion follow loop: a source of the launch loop that looks at each open promotion's pull
//! or merge request at the calls' aging cadence, and registers the pack once it merged.

use crate::daemon::queue::{BoxFuture, DueSource, Pass, WorkQueue};
use crate::identity::forge::Forge;
use crate::playbooks::promotion::forge::{ForgeError, Looked, Target, look};
use crate::playbooks::promotion::{Promotion, PromotionState, store};
use crate::playbooks::registry::{PackGit, RegisterError, RegisterPlaybook};

const PASS_LIMIT: i64 = 32;

/// Why one look at a promotion did not finish.
#[derive(Debug, thiserror::Error)]
pub(crate) enum FollowError {
    #[error(transparent)]
    Forge(#[from] ForgeError),
    #[error("draft {0} is gone")]
    DraftGone(String),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// The launch-loop source that follows promotions.
pub(crate) struct PromotionFollow {
    db: crate::client::Db,
    cfg: crate::config::ControllerCfg,
}

impl PromotionFollow {
    pub(crate) fn new(db: crate::client::Db, cfg: crate::config::ControllerCfg) -> Self {
        PromotionFollow { db, cfg }
    }
}

impl DueSource for PromotionFollow {
    fn next_due(&self, _held: Vec<String>) -> BoxFuture<anyhow::Result<Option<jiff::Timestamp>>> {
        let db = self.db.clone();
        Box::pin(async move {
            let Some(earliest) = store::earliest_due(db.pool()).await? else {
                return Ok(None);
            };
            Ok(Some(earliest.parse()?))
        })
    }

    fn pass(&self, _queue: WorkQueue, _held: Vec<String>) -> BoxFuture<anyhow::Result<Pass>> {
        let db = self.db.clone();
        let cfg = self.cfg.clone();
        Box::pin(async move {
            let claimed = follow_due(&db, &cfg, jiff::Timestamp::now()).await?;
            Ok(Pass {
                claimed,
                held: Vec::new(),
            })
        })
    }
}

pub(crate) async fn follow_due(
    db: &crate::client::Db,
    cfg: &crate::config::ControllerCfg,
    now: jiff::Timestamp,
) -> anyhow::Result<usize> {
    let pool = db.pool();
    let due = store::due(pool, &crate::clock::stamp(now), PASS_LIMIT).await?;
    let count = due.len();
    let backends = crate::calls::Backends::following(cfg);
    for promotion in due {
        let age = promotion
            .created_at
            .parse::<jiff::Timestamp>()
            .ok()
            .and_then(|at| u64::try_from(now.duration_since(at).as_secs()).ok())
            .map(std::time::Duration::from_secs);
        let next = now.checked_add(jiff::SignedDuration::try_from(
            crate::calls::aging_cadence(age),
        )?)?;
        let next = crate::clock::stamp(next);
        if let Err(e) = follow(db, cfg, &backends, &promotion, &next).await {
            tracing::warn!(draft = %promotion.draft, error = %format!("{e:#}"), "following a promotion failed");
            store::looked(pool, &promotion.draft, None, &next).await?;
        }
    }
    Ok(count)
}

async fn read_token(
    cfg: &crate::config::ControllerCfg,
    backends: &crate::calls::Backends,
    promotion: &Promotion,
    target: &Target,
) -> anyhow::Result<Option<String>> {
    match target {
        Target::Github(repo) => match backends.github_read_token(repo).await {
            Some(token) => Ok(Some(token)),
            None => crate::runs::engine::resolve_pack_pr_token_for(cfg.github_app.as_ref()).await,
        },
        Target::Gitlab(..) => Ok(backends.gitlab_read_token(&promotion.host).await),
    }
}

async fn follow(
    db: &crate::client::Db,
    cfg: &crate::config::ControllerCfg,
    backends: &crate::calls::Backends,
    promotion: &Promotion,
    next: &str,
) -> Result<(), FollowError> {
    let pool = db.pool();
    let target = Target::new(promotion.forge, &promotion.host, &promotion.project)?;
    let token = read_token(cfg, backends, promotion, &target).await?;
    let state = match promotion.state {
        PromotionState::Merged => PromotionState::Merged,
        _ => match look(
            &target,
            promotion.number,
            token.as_deref(),
            promotion.etag.as_deref(),
        )
        .await
        {
            Ok(Looked::Unchanged) => {
                store::looked(pool, &promotion.draft, None, next).await?;
                return Ok(());
            }
            Ok(Looked::Now { state, etag }) => {
                store::looked(pool, &promotion.draft, etag.as_deref(), next).await?;
                state
            }
            Err(e @ ForgeError::Unreachable(_)) => return Err(e.into()),
            Err(ForgeError::Refused(e)) => {
                store::looked(pool, &promotion.draft, None, next).await?;
                if promotion.note != e {
                    store::moved(pool, &promotion.draft, promotion.state, &e).await?;
                }
                return Ok(());
            }
        },
    };
    match state {
        PromotionState::Open => {}
        PromotionState::Closed => {
            store::moved(
                pool,
                &promotion.draft,
                PromotionState::Closed,
                "closed without merging",
            )
            .await?;
        }
        PromotionState::Merged => {
            if promotion.state != PromotionState::Merged {
                store::moved(pool, &promotion.draft, PromotionState::Merged, "").await?;
            }
            let (moved, note) = import(pool, cfg, promotion, token).await?;
            store::moved(pool, &promotion.draft, moved, &note).await?;
        }
        PromotionState::Imported | PromotionState::ImportFailed => {}
    }
    Ok(())
}

pub(crate) async fn import(
    pool: &sqlx::PgPool,
    cfg: &crate::config::ControllerCfg,
    promotion: &Promotion,
    token: Option<String>,
) -> Result<(PromotionState, String), FollowError> {
    let draft = crate::playbooks::drafts::get(pool, &promotion.draft)
        .await?
        .ok_or_else(|| FollowError::DraftGone(promotion.draft.clone()))?;
    let mut git = PackGit::resolve(cfg.github_app.as_ref()).await?;
    if let (Forge::Gitlab, Some(token)) = (promotion.forge, token) {
        git = git.with_gitlab(promotion.host.as_str(), token);
    }
    let req = RegisterPlaybook {
        id: promotion.draft.clone(),
        owner: draft.owner.clone(),
        description: draft.description.clone(),
        repo: promotion.registry_repo(),
        git_ref: Some(promotion.base.clone()),
        path: promotion.path.clone(),
        expected_rev: None,
        accept_exposure_digest: None,
        variant: None,
    };
    match crate::playbooks::registry::register(pool, &git, req, Some(&promotion.promoted_by)).await
    {
        Ok(_) => Ok((PromotionState::Imported, String::new())),
        Err(e @ (RegisterError::Fetch(_) | RegisterError::Internal(_))) => {
            Ok((PromotionState::Merged, format!("{e:#}")))
        }
        Err(e) => Ok((PromotionState::ImportFailed, e.to_string())),
    }
}
