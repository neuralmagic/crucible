//! discovery: fetch changed GitHub issues and upsert them into the ledger. Two moves:
//!
//! - **fetch** ([`list_changed_issues`]) — the GitHub issues-list REST endpoint, paginated via the
//!   `Link` header, reusing `crucible/src/scope.rs`'s `goal_from_issue` auth/UA pattern.
//! - **upsert** ([`poll_repo`] / [`triage_repo`]) — one `Db::upsert_issue` per changed issue
//!   (status and tier untouched), then the repo's watermark advances.
//!
//! Triage is *pure discovery* — it writes no tier. A prior version of this module carried a
//! deterministic label/keyword heuristic as a cheap pre-filter; it was deleted: it
//! misranked, every issue gets exactly one cached LLM verdict at reconcile time regardless
//! ([`crate::issues::ranker`]), and a wrong cheap tier is worse than an honest NULL. See [`crucible_contract::Tier`]
//! for the (now LLM-only) tier vocabulary.
use crate::client::Db;
use crate::issues::github::{self, GhUser, github_api_base, github_token};
use crate::issues::model::{Issue, NewIssue};
use crate::model::{ParkReason, ParkedBy, Status};
use anyhow::{Context, Result};
use serde::Deserialize;

/// One GitHub issue as triage needs it (a subset of the issues-list REST response). Pull requests
/// come back from the same endpoint; [`list_changed_issues`] filters them out.
#[derive(Debug, Clone, PartialEq)]
pub struct GhIssue {
    pub(crate) number: u64,
    pub(crate) title: String,
    pub(crate) body: Option<String>,
    pub(crate) labels: Vec<String>,
    pub(crate) html_url: String,
    pub(crate) updated_at: String,
    pub(crate) state: String,
    /// The issue author's login, or `None` for the (rare/deleted-account) case GitHub omits
    /// `user`.
    pub(crate) author: Option<String>,
    /// GitHub's comment count from the list payload — lets a sweep skip the per-issue comments
    /// GET entirely for the common zero-comment case (an empty local mirror needs no fetch).
    comments: u64,
}

/// The raw REST shape, decoded then narrowed into [`GhIssue`] (and used to detect+drop PRs).
#[derive(Debug, Deserialize)]
struct GhIssueRaw {
    number: u64,
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Vec<GhLabelRaw>,
    html_url: String,
    updated_at: String,
    state: String,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
    #[serde(default)]
    user: Option<GhUser>,
    #[serde(default)]
    comments: u64,
}

#[derive(Debug, Deserialize)]
struct GhLabelRaw {
    name: String,
}

impl From<GhIssueRaw> for GhIssue {
    fn from(r: GhIssueRaw) -> Self {
        GhIssue {
            number: r.number,
            title: r.title,
            body: r.body,
            labels: r.labels.into_iter().map(|l| l.name).collect(),
            html_url: r.html_url,
            updated_at: r.updated_at,
            state: r.state,
            author: r.user.map(|u| u.login),
            comments: r.comments,
        }
    }
}

/// One GitHub issue comment as discovery mirrors it (a subset of the issue-comments REST
/// response).
#[derive(Debug, Clone, PartialEq)]
pub struct GhComment {
    id: i64,
    author: Option<String>,
    created_at: String,
    updated_at: String,
    body: String,
}

/// The raw REST shape, narrowed into [`GhComment`].
#[derive(Debug, Deserialize)]
struct GhCommentRaw {
    id: i64,
    #[serde(default)]
    user: Option<GhUser>,
    created_at: String,
    updated_at: String,
    #[serde(default)]
    body: Option<String>,
}

impl From<GhCommentRaw> for GhComment {
    fn from(r: GhCommentRaw) -> Self {
        GhComment {
            id: r.id,
            author: r.user.map(|u| u.login),
            created_at: r.created_at,
            updated_at: r.updated_at,
            body: r.body.unwrap_or_default(),
        }
    }
}

/// List every issue in `repo` updated at or after `since` (GitHub's `since` is inclusive),
/// paginating via the `Link` header until exhausted. PRs are filtered out (the issues endpoint
/// returns both). Honors `GITHUB_API_URL` and `GITHUB_TOKEN`/`GH_TOKEN` (the `scope.rs` pattern);
/// [`list_changed_issues_from`] is the env-free, directly-testable inner form.
pub async fn list_changed_issues(repo: &str, since: Option<&str>) -> Result<Vec<GhIssue>> {
    list_changed_issues_from(&github_api_base(), github_token().as_deref(), repo, since).await
}

/// The testable core of [`list_changed_issues`]: an explicit API base + token instead of env vars,
/// so tests point at a local listener without mutating global process state (env vars are shared
/// across concurrently-run `#[tokio::test]`s in one process — a real race, not just style).
async fn list_changed_issues_from(
    api_base: &str,
    token: Option<&str>,
    repo: &str,
    since: Option<&str>,
) -> Result<Vec<GhIssue>> {
    let url = match since {
        Some(s) => format!(
            "{}/repos/{repo}/issues?state=all&sort=updated&per_page=100&since={s}",
            api_base.trim_end_matches('/'),
        ),
        None => format!(
            "{}/repos/{repo}/issues?state=all&sort=updated&per_page=100",
            api_base.trim_end_matches('/'),
        ),
    };
    Ok(
        github::paginate::<GhIssueRaw>(token, url, &format!("issues for {repo}"))
            .await?
            .into_iter()
            .filter(|i| i.pull_request.is_none())
            .map(GhIssue::from)
            .collect(),
    )
}

/// List every comment on one issue, paginating via the `Link` header until exhausted — the
/// per-issue GET the comment mirror needs (comments never ride the issues-list payload). Same
/// auth/UA/retry discipline as [`list_changed_issues`]; called only for issues the discovery
/// sweep reports as changed (and on first ingest), never fleet-wide.
async fn list_issue_comments_from(
    api_base: &str,
    token: Option<&str>,
    repo: &str,
    number: u64,
) -> Result<Vec<GhComment>> {
    let url = format!(
        "{}/repos/{repo}/issues/{number}/comments?per_page=100",
        api_base.trim_end_matches('/'),
    );
    Ok(
        github::paginate::<GhCommentRaw>(token, url, &format!("comments for {repo}#{number}"))
            .await?
            .into_iter()
            .map(GhComment::from)
            .collect(),
    )
}

/// Fetch one issue's full content (title/body/labels) by number — the single-issue GET the
/// tier-ranking cache needs (as opposed to [`list_changed_issues`]'s paginated list). Same
/// auth/UA/retry discipline as the list fetch, and the sole GET a `new`-issue reconcile makes.
pub(crate) async fn fetch_issue(repo: &str, number: u64) -> Result<GhIssue> {
    fetch_issue_from(&github_api_base(), github_token().as_deref(), repo, number).await
}

/// The testable core of [`fetch_issue`]: an explicit API base + token, like
/// [`list_changed_issues_from`] is to [`list_changed_issues`].
async fn fetch_issue_from(
    api_base: &str,
    token: Option<&str>,
    repo: &str,
    number: u64,
) -> Result<GhIssue> {
    let client = github::read_client()?;
    let url = format!(
        "{}/repos/{repo}/issues/{number}",
        api_base.trim_end_matches('/'),
    );
    let resp = github::get_retryable(&client, &url, token)
        .await
        .with_context(|| format!("GET {url}"))?;
    let raw: GhIssueRaw = resp
        .json()
        .await
        .with_context(|| format!("decoding GitHub issue from {url}"))?;
    Ok(GhIssue::from(raw))
}

/// Whether `repo` (`owner/repo`) exists on GitHub — the `POST /api/repos` existence check
/// (Lane O3). Reuses this module's auth/UA/retry client rather than a second HTTP path, so the
/// call counts against the same `crucible_github_api_requests_total` metric as every other
/// GitHub fetch here.
pub async fn repo_exists(repo: &str) -> Result<bool> {
    repo_exists_from(&github_api_base(), github_token().as_deref(), repo).await
}

/// The testable core of [`repo_exists`]: an explicit API base + token, like
/// [`list_changed_issues_from`] is to [`list_changed_issues`].
async fn repo_exists_from(api_base: &str, token: Option<&str>, repo: &str) -> Result<bool> {
    let client = github::read_client()?;
    let url = format!("{}/repos/{repo}", api_base.trim_end_matches('/'));
    match github::get_retryable(&client, &url, token).await {
        Ok(_) => Ok(true),
        Err(crate::issues::http::HttpError::NotFound) => Ok(false),
        Err(e) => Err(e).with_context(|| format!("checking whether {repo} exists")),
    }
}

/// One fetched-and-upserted issue's outcome, for the CLI summary ([`triage_repo`]) and the
/// daemon's changed-key feed ([`poll_repo`]).
struct IssueOutcome {
    key: String,
    was_new: bool,
}

/// One repo's whole sweep: the upserted issues plus the closed-upstream accounting
/// ([`triage_one_repo`]'s return — the summary and the changed-key feed both read off it).
struct SweepOutcome {
    outcomes: Vec<IssueOutcome>,
    /// Listed issues that were closed upstream and untracked locally — never ingested at all.
    skipped_closed: usize,
    /// Tracked non-terminal rows whose upstream issue showed closed, parked this sweep.
    retired: usize,
}

/// Whether a row is resting in the retired-because-upstream-closed shape (and is therefore the
/// reopen edge's to-revive candidate).
fn retired_upstream_closed(issue: &Issue) -> bool {
    issue.status == Status::Parked
        && issue.parked_by == Some(ParkedBy::Machine)
        && issue.park_reason() == Some(ParkReason::UpstreamClosed)
}

/// Retire one tracked issue whose upstream listing shows closed. Returns whether a park took.
///
/// - `done` is terminal, already-human-parked is sticky, already-retired is idempotent — all
///   untouched.
/// - `running` is deliberately left to [`crate::issues::approvals::reconcile_upstream_close`] (driven by the
///   re-enqueued key): that approval stops the loop pod *before* parking, which triage must not skip.
/// - any other machine park is relabeled: a stale park's own closing activity would otherwise
///   auto-unpark it straight back into the sweep.
async fn retire_closed_issue(db: &Db, key: &str, existing: &Issue) -> Result<bool> {
    match existing.status {
        Status::Done | Status::Running => Ok(false),
        Status::Parked => {
            if existing.parked_by == Some(ParkedBy::Human) || retired_upstream_closed(existing) {
                return Ok(false);
            }
            crate::issues::transitions::park(
                db.pool(),
                db.events(),
                key,
                Status::Parked,
                &ParkReason::UpstreamClosed,
                ParkedBy::Machine,
            )
            .await
        }
        // `building` has no running loop pod (only detached build Jobs, reaped on their own), so it
        // parks directly like the other pre-launch states — no approval hand-off needed.
        Status::New
        | Status::Scoped
        | Status::AwaitingApproval
        | Status::Building
        | Status::PrOpen => {
            crate::issues::transitions::park(
                db.pool(),
                db.events(),
                key,
                existing.status,
                &ParkReason::UpstreamClosed,
                ParkedBy::Machine,
            )
            .await
        }
    }
}

/// Fetch, rank, and upsert every issue in `repo` changed since its stored watermark, then advance
/// the watermark to the newest `updated_at` seen. Idempotent: GitHub's `since` is inclusive, so the
/// boundary issue is re-fetched next time, but re-upserting it (same tier, same evidence) is a
/// harmless no-op — re-tiering is idempotent under duplicate events.
///
/// `full` ignores the stored watermark for this one sweep (`since = None`, so GitHub returns every
/// issue instead of only what changed) — the backfill/repair path for columns (like `title`/
/// `author`) added after rows were first triaged. The watermark still advances afterward as usual.
async fn triage_one_repo(
    db: &Db,
    api_base: &str,
    token: Option<&str>,
    repo: &str,
    full: bool,
) -> Result<SweepOutcome> {
    let since = crate::issues::store::get_watermark(db.pool(), repo).await?;
    let fetch_since = if full { None } else { since.as_deref() };
    let issues = list_changed_issues_from(api_base, token, repo, fetch_since).await?;

    let mut outcomes = Vec::with_capacity(issues.len());
    let mut skipped_closed = 0usize;
    let mut retired = 0usize;
    let mut newest = since;
    for issue in &issues {
        let key = format!("{repo}#{}", issue.number);
        let closed = !issue.state.eq_ignore_ascii_case("open");
        let existing = crate::issues::store::get_issue(db.pool(), &key).await?;

        // The watermark advances past every listed issue, closed included — closures must stay
        // observable, and re-listing a skipped issue forever would defeat the `since` sweep.
        if newest
            .as_deref()
            .is_none_or(|n| issue.updated_at.as_str() > n)
        {
            newest = Some(issue.updated_at.clone());
        }

        // Closed upstream and never tracked: not our problem, and never was — no row, no
        // comment sync. This is what keeps a fresh onboarding from ingesting the repo's whole
        // closed history as `new`.
        if closed && existing.is_none() {
            skipped_closed += 1;
            continue;
        }

        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: key.clone(),
                repo: repo.to_string(),
                priority: 0,
                evidence_url: Some(issue.html_url.clone()),
                title: Some(issue.title.clone()),
                author: issue.author.clone(),
                body: issue.body.clone(),
                labels: issue.labels.clone(),
                upstream_updated_at: Some(issue.updated_at.clone()),
            },
        )
        .await?;
        if let Some(existing) = &existing {
            if closed {
                if retire_closed_issue(db, &key, existing).await? {
                    retired += 1;
                }
            } else if retired_upstream_closed(existing) {
                // Reopened upstream: the retire park is the one machine park a listing revives.
                crate::issues::transitions::unpark(
                    db.pool(),
                    db.events(),
                    &key,
                    Some("upstream reopened"),
                    None,
                )
                .await?;
            }
        }
        sync_issue_comments(db, api_base, token, repo, issue, &key).await?;
        outcomes.push(IssueOutcome {
            key,
            was_new: existing.is_none(),
        });
    }
    if let Some(w) = newest {
        crate::issues::store::set_watermark(db.pool(), repo, &w).await?;
    }
    if skipped_closed > 0 || retired > 0 {
        tracing::info!(
            %repo,
            skipped_closed,
            retired,
            "triage: closed upstream issues skipped/retired"
        );
    }
    Ok(SweepOutcome {
        outcomes,
        skipped_closed,
        retired,
    })
}

/// Mirror one changed issue's comments into `issue_comments`: fetch the full set and replace the
/// local rows (upsert by id, delete vanished). The list payload's comment count spares the GET for
/// the zero-comment case — replacing with the empty set is a pure local write that still clears
/// rows whose comments were all deleted upstream. Failing here fails the sweep *before* the
/// watermark advances, so a flaky comments fetch is retried by the next poll, never dropped.
async fn sync_issue_comments(
    db: &Db,
    api_base: &str,
    token: Option<&str>,
    repo: &str,
    issue: &GhIssue,
    key: &str,
) -> Result<()> {
    // Only ever called from the GitHub sweep loop with a key built from repo+number, so this
    // never fires today — the explicit guard documents the invariant this function relies on.
    if !crate::issues::model::InputKind::from_parts("github", key).has_upstream() {
        return Ok(());
    }
    let comments = if issue.comments == 0 {
        Vec::new()
    } else {
        let fetched = list_issue_comments_from(api_base, token, repo, issue.number).await;
        if let Some(m) = db.metrics() {
            m.record_github(fetched.is_ok());
        }
        fetched.with_context(|| format!("fetching comments for {key}"))?
    };
    let rows: Vec<crate::issues::model::IssueComment> = comments
        .into_iter()
        .map(|c| crate::issues::model::IssueComment {
            id: c.id,
            issue_key: key.to_string(),
            author: c.author,
            created_at: c.created_at,
            updated_at: c.updated_at,
            body: c.body,
        })
        .collect();
    crate::issues::store::replace_issue_comments(db.pool(), key, &rows).await
}

/// Triage one repo and return the changed issue keys (`"owner/repo#N"`) — the public surface the
/// daemon polls through (its upstream-watermark source calls this per watched repo).
pub async fn poll_repo(db: &Db, repo: &str) -> Result<Vec<String>> {
    Ok(triage_one_repo(
        db,
        &github_api_base(),
        github_token().as_deref(),
        repo,
        false,
    )
    .await?
    .outcomes
    .into_iter()
    .map(|o| o.key)
    .collect())
}

/// One repo's triage-run summary for the `crucible-controller triage` CLI table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriageSummary {
    pub repo: String,
    /// Issues seen for the first time this run (entered the ledger at `new`).
    pub new: usize,
    /// Already-tracked issues whose discovery metadata (title/author/labels/evidence) was
    /// refreshed. Tier is never among it — tiering is the ranker's job alone.
    pub changed: usize,
    /// Issues currently parked for this repo (informational — apart from the closed-upstream
    /// retire edge, parking is reconcile's job).
    pub parked: usize,
    /// Listed issues closed upstream and untracked locally — skipped, never ingested.
    pub skipped_closed: usize,
    /// Tracked rows retired ("upstream closed" machine park) this sweep.
    pub retired: usize,
}

/// Triage one repo and return the CLI-shaped summary (new/changed/parked counts). `full` is the
/// `crucible-controller triage --full` backfill/repair sweep: ignore the stored watermark and re-fetch +
/// re-upsert every issue in `repo`, not just what changed.
pub async fn triage_repo(db: &Db, repo: &str, full: bool) -> Result<TriageSummary> {
    let sweep = triage_one_repo(
        db,
        &github_api_base(),
        github_token().as_deref(),
        repo,
        full,
    )
    .await?;
    let new = sweep.outcomes.iter().filter(|o| o.was_new).count();
    let changed = sweep.outcomes.len() - new;
    let parked = usize::try_from(
        crate::issues::store::count_issues_by_status(db.pool(), repo, Status::Parked).await?,
    )
    .unwrap_or(0);
    Ok(TriageSummary {
        repo: repo.to_string(),
        new,
        changed,
        parked,
        skipped_closed: sweep.skipped_closed,
        retired: sweep.retired,
    })
}

/// One repo's `upstream_updated_at` backfill outcome (the per-pass summary line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillSummary {
    repo: String,
    /// NULL rows that took a stamp from the listing this pass.
    stamped: u64,
    /// Rows still NULL afterward: absent from the listing (deleted/transferred upstream). Left
    /// NULL on purpose — inventing a timestamp would forge upstream activity.
    still_null: i64,
}

/// Backfill `issues.upstream_updated_at` for every watched repo that still has NULL rows — issues
/// ingested before migration 0007 added the column, which a watermark sweep never re-fetches
/// (GitHub's `since` only reports what changed). Lists the repo's issues without the `since`
/// filter (same pagination/retry guards) and stamps `updated_at` onto NULL rows only; the
/// watermark path owns every non-NULL stamp. Self-quiescing: a repo with no NULL rows costs zero
/// GitHub calls, so the pass is free once the backlog is stamped.
pub async fn backfill_upstream_updated_at(db: &Db) -> Result<Vec<BackfillSummary>> {
    backfill_upstream_updated_at_from(db, &github_api_base(), github_token().as_deref()).await
}

/// The testable core of [`backfill_upstream_updated_at`]: an explicit API base + token, like
/// [`list_changed_issues_from`] is to [`list_changed_issues`].
async fn backfill_upstream_updated_at_from(
    db: &Db,
    api_base: &str,
    token: Option<&str>,
) -> Result<Vec<BackfillSummary>> {
    let mut out = Vec::new();
    for repo in crate::issues::repo_watch::watched_repos(db.pool()).await? {
        if crate::issues::store::count_null_upstream_updated_at(db.pool(), &repo).await? == 0 {
            continue;
        }
        let fetched = list_changed_issues_from(api_base, token, &repo, None).await;
        if let Some(m) = db.metrics() {
            m.record_github(fetched.is_ok());
        }
        let issues = fetched.with_context(|| format!("backfill: listing issues for {repo}"))?;
        let stamps: Vec<(String, String)> = issues
            .iter()
            .map(|i| (format!("{repo}#{}", i.number), i.updated_at.clone()))
            .collect();
        let stamped =
            crate::issues::store::stamp_null_upstream_updated_at(db.pool(), &stamps).await?;
        let still_null =
            crate::issues::store::count_null_upstream_updated_at(db.pool(), &repo).await?;
        tracing::info!(
            %repo,
            stamped,
            still_null,
            "triage: upstream_updated_at backfill pass"
        );
        out.push(BackfillSummary {
            repo,
            stamped,
            still_null,
        });
    }
    Ok(out)
}

/// One repo's closed-upstream repair outcome (the per-pass summary line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedRepairSummary {
    repo: String,
    /// Tracked non-terminal rows retired ("upstream closed" machine park) this pass.
    retired: u64,
}

/// Retire every tracked row whose upstream issue is closed, per watched repo — the one-time
/// repair for ledgers contaminated before [`triage_one_repo`] honored `state` (a normal sweep is
/// watermark-gated, so a long-closed issue never reappears in it). Lists the repo's full issue
/// set once (the `backfill_upstream_updated_at` pattern), retires per [`retire_closed_issue`],
/// then stamps `repos.closed_repaired_at` — a stamped repo costs zero GitHub calls, so the pass
/// self-quiesces.
pub(crate) async fn repair_closed_issues(db: &Db) -> Result<Vec<ClosedRepairSummary>> {
    repair_closed_issues_from(db, &github_api_base(), github_token().as_deref()).await
}

/// The testable core of [`repair_closed_issues`]: an explicit API base + token, like
/// [`list_changed_issues_from`] is to [`list_changed_issues`].
async fn repair_closed_issues_from(
    db: &Db,
    api_base: &str,
    token: Option<&str>,
) -> Result<Vec<ClosedRepairSummary>> {
    let mut out = Vec::new();
    for repo in crate::issues::repo_watch::repos_needing_closed_repair(db.pool()).await? {
        let fetched = list_changed_issues_from(api_base, token, &repo, None).await;
        if let Some(m) = db.metrics() {
            m.record_github(fetched.is_ok());
        }
        let issues =
            fetched.with_context(|| format!("closed repair: listing issues for {repo}"))?;
        let mut retired = 0u64;
        for issue in issues
            .iter()
            .filter(|i| !i.state.eq_ignore_ascii_case("open"))
        {
            let key = format!("{repo}#{}", issue.number);
            if let Some(existing) = crate::issues::store::get_issue(db.pool(), &key).await?
                && retire_closed_issue(db, &key, &existing).await?
            {
                retired += 1;
            }
        }
        crate::issues::repo_watch::mark_closed_repaired(db.pool(), &repo).await?;
        tracing::info!(%repo, retired, "triage: closed-upstream repair pass");
        out.push(ClosedRepairSummary { repo, retired });
    }
    Ok(out)
}

#[cfg(test)]
// The env-mutating tests below hold the crate-wide `ENV_LOCK` (an async mutex) across their bodies
// so `set_var` on `GITHUB_API_URL` can't race any other test that reads the environ or spawns a
// subprocess.
mod tests {
    use super::*;
    use sqlx::PgPool;
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn db_with(pool: PgPool) -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Db::new(pool);
        (db, dir)
    }

    fn raw_issue(number: u64, title: &str, labels: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "number": number,
            "title": title,
            "body": format!("body of issue #{number}"),
            "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
            "html_url": format!("https://github.com/owner/repo/issues/{number}"),
            "updated_at": "2026-07-01T00:00:00Z",
            "state": "open",
            "user": {"login": "octocat"},
        })
    }

    fn raw_comment(id: i64, body: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "user": {"login": "commenter"},
            "created_at": format!("2026-07-01T00:00:{:02}Z", id),
            "updated_at": format!("2026-07-01T00:00:{:02}Z", id),
            "body": body,
        })
    }

    // --- fetch: pagination -----------------------------------------------------------------

    #[tokio::test]
    async fn list_changed_issues_paginates_via_link_header() {
        let server = MockServer::start().await;
        let page1_url = format!(
            "{}/repos/owner/repo/issues?state=all&sort=updated&per_page=100",
            server.uri()
        );

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(move |req: &wiremock::Request| {
                if req.url.query().unwrap_or("").contains("page=2") {
                    ResponseTemplate::new(200).set_body_json(vec![raw_issue(2, "second page issue", &[])])
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(vec![raw_issue(1, "first page issue", &[])])
                        .insert_header(
                            "Link",
                            format!(r#"<{page1_url}&page=2>; rel="next", <{page1_url}&page=9>; rel="last""#),
                        )
                }
            })
            .expect(2)
            .mount(&server)
            .await;

        let issues = list_changed_issues_from(&server.uri(), None, "owner/repo", None)
            .await
            .expect("fetch ok");

        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 1);
        assert_eq!(issues[1].number, 2);
    }

    #[tokio::test]
    async fn list_changed_issues_filters_out_pull_requests() {
        let server = MockServer::start().await;
        let mut pr = raw_issue(3, "a pull request", &[]);
        pr["pull_request"] = serde_json::json!({"url": "https://api.github.com/..."});

        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(vec![raw_issue(1, "real issue", &[]), pr]),
            )
            .mount(&server)
            .await;

        let issues = list_changed_issues_from(&server.uri(), None, "owner/repo", None)
            .await
            .expect("fetch ok");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 1);
    }

    #[tokio::test]
    async fn list_changed_issues_honors_since_and_auth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .and(header("Authorization", "Bearer test-token"))
            .respond_with(move |req: &wiremock::Request| {
                assert!(
                    req.url
                        .query()
                        .unwrap_or("")
                        .contains("since=2026-06-01T00:00:00Z")
                );
                ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new())
            })
            .mount(&server)
            .await;

        let issues = list_changed_issues_from(
            &server.uri(),
            Some("test-token"),
            "owner/repo",
            Some("2026-06-01T00:00:00Z"),
        )
        .await
        .expect("fetch ok");
        assert!(issues.is_empty());
    }

    // --- fetch: retry on 429 ----------------------------------------------------------------

    #[tokio::test]
    async fn list_changed_issues_retries_after_429() {
        let server = MockServer::start().await;

        // First response: 429 with a near-zero Retry-After so the test doesn't sleep meaningfully.
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                7,
                "after retry",
                &[],
            )]))
            .mount(&server)
            .await;

        let issues = list_changed_issues_from(&server.uri(), None, "owner/repo", None)
            .await
            .expect("fetch ok after retry");

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].number, 7);
    }

    // --- upsert / watermark --------------------------------------------------------------------

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn watermark_round_trips(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        assert_eq!(
            crate::issues::store::get_watermark(db.pool(), "owner/repo").await?,
            None
        );
        crate::issues::store::set_watermark(db.pool(), "owner/repo", "2026-06-01T00:00:00Z")
            .await?;
        assert_eq!(
            crate::issues::store::get_watermark(db.pool(), "owner/repo")
                .await?
                .as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
        // Advancing overwrites, not appends.
        crate::issues::store::set_watermark(db.pool(), "owner/repo", "2026-06-02T00:00:00Z")
            .await?;
        assert_eq!(
            crate::issues::store::get_watermark(db.pool(), "owner/repo")
                .await?
                .as_deref(),
            Some("2026-06-02T00:00:00Z")
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn triage_repo_upsert_does_not_clobber_status(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                5,
                "reduce p99 latency of the router",
                &["performance"],
            )]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }

        let summary = triage_repo(&db, "owner/repo", false).await?;
        assert_eq!(summary.new, 1);
        assert_eq!(summary.changed, 0);
        assert!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#5")
                .await?
                .expect("tracked")
                .tier
                .is_none(),
            "triage is pure discovery: a fresh row's tier is NULL, never guessed"
        );

        // Seed a non-`new` status and a ranker-confirmed tier the same way reconcile would, then
        // re-triage (a re-poll of unchanged/updated issue content).
        assert!(
            crate::issues::store::claim_issue(
                db.pool(),
                "owner/repo#5",
                Status::New,
                Status::Scoped
            )
            .await?
        );
        crate::issues::store::set_ranked_tier(db.pool(), "owner/repo#5", "T1", "perf", "somehash")
            .await?;

        let summary2 = triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(
            summary2.new, 0,
            "already tracked, so this is a `changed` row"
        );
        assert_eq!(summary2.changed, 1);
        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#5")
            .await?
            .expect("issue tracked");
        assert_eq!(got.status, Status::Scoped, "upsert must not touch status");
        assert_eq!(
            got.tier.as_deref(),
            Some("T1"),
            "upsert must not touch a tier the ranker already set"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn poll_repo_returns_changed_keys_and_advances_watermark(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                raw_issue(1, "a bug with steps to reproduce", &["bug"]),
                raw_issue(2, "docs: fix a typo", &["documentation"]),
            ]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }

        let keys = poll_repo(&db, "owner/repo").await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(
            keys,
            vec!["owner/repo#1".to_string(), "owner/repo#2".to_string()]
        );
        assert_eq!(
            crate::issues::store::get_watermark(db.pool(), "owner/repo")
                .await?
                .as_deref(),
            Some("2026-07-01T00:00:00Z")
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn triage_repo_persists_title_and_author(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                6,
                "a fresh bug report",
                &["bug"],
            )]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#6")
            .await?
            .expect("issue tracked");
        assert_eq!(got.title.as_deref(), Some("a fresh bug report"));
        assert_eq!(got.author.as_deref(), Some("octocat"));
        assert_eq!(got.body.as_deref(), Some("body of issue #6"));
        assert_eq!(got.labels, vec!["bug".to_string()]);
        Ok(())
    }

    // --- comments: mirrored for changed issues only ---------------------------------------------

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn changed_issue_comments_are_mirrored_and_pruned(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);

        let server = MockServer::start().await;
        let mut issue = raw_issue(8, "an issue with a comment thread", &[]);
        issue["comments"] = serde_json::json!(2);
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue]))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/8/comments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                raw_comment(11, "first comment"),
                raw_comment(12, "second comment"),
            ]))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        triage_repo(&db, "owner/repo", false).await?;

        let got = crate::issues::store::list_issue_comments(db.pool(), "owner/repo#8").await?;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, 11);
        assert_eq!(got[0].author.as_deref(), Some("commenter"));
        assert_eq!(got[0].body, "first comment");
        assert_eq!(got[1].id, 12);

        // The issue changes again: one comment was deleted upstream, the other edited. The sweep
        // upserts by id and prunes the vanished row.
        let mut issue = raw_issue(8, "an issue with a comment thread", &[]);
        issue["comments"] = serde_json::json!(1);
        issue["updated_at"] = serde_json::json!("2026-07-02T00:00:00Z");
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/8/comments"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(vec![raw_comment(12, "second comment (edited)")]),
            )
            .mount(&server)
            .await;
        triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        let got = crate::issues::store::list_issue_comments(db.pool(), "owner/repo#8").await?;
        assert_eq!(got.len(), 1, "the deleted comment's row is pruned");
        assert_eq!(got[0].id, 12);
        assert_eq!(got[0].body, "second comment (edited)");
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn zero_comment_issues_skip_the_comments_fetch_but_still_prune(
        pool: PgPool,
    ) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);

        // A stale local row from an earlier sweep; upstream has since deleted every comment.
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: "owner/repo#9".to_string(),
                repo: "owner/repo".to_string(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        crate::issues::store::replace_issue_comments(
            db.pool(),
            "owner/repo#9",
            &[crate::issues::model::IssueComment {
                id: 99,
                issue_key: "owner/repo#9".to_string(),
                author: Some("commenter".to_string()),
                created_at: "2026-06-01T00:00:00Z".to_string(),
                updated_at: "2026-06-01T00:00:00Z".to_string(),
                body: "since deleted upstream".to_string(),
            }],
        )
        .await?;

        let server = MockServer::start().await;
        // `raw_issue` carries no `comments` field (decodes as 0) — the sweep must not GET the
        // comments endpoint at all for it.
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                9,
                "no comments left",
                &[],
            )]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/9/comments"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert!(
            crate::issues::store::list_issue_comments(db.pool(), "owner/repo#9")
                .await?
                .is_empty(),
            "a zero-count sweep clears the stale mirror without an HTTP fetch"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn comment_fetch_paginates_via_link_header(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);

        let server = MockServer::start().await;
        let mut issue = raw_issue(10, "a long thread", &[]);
        issue["comments"] = serde_json::json!(2);
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![issue]))
            .mount(&server)
            .await;
        let page1_url = format!(
            "{}/repos/owner/repo/issues/10/comments?per_page=100",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/10/comments"))
            .respond_with(move |req: &wiremock::Request| {
                if req.url.query().unwrap_or("").contains("page=2") {
                    ResponseTemplate::new(200).set_body_json(vec![raw_comment(22, "from page two")])
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(vec![raw_comment(21, "from page one")])
                        .insert_header("Link", format!(r#"<{page1_url}&page=2>; rel="next""#))
                }
            })
            .expect(2)
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        let got = crate::issues::store::list_issue_comments(db.pool(), "owner/repo#10").await?;
        assert_eq!(
            got.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![21, 22],
            "both pages landed"
        );
        Ok(())
    }

    /// `crucible-controller triage --full`'s reason to exist: a normal sweep only asks GitHub for issues
    /// changed `since` the stored watermark, so a pre-existing row whose upstream `updated_at`
    /// hasn't moved is never re-fetched — title/author (added by migration 0004 after the row was
    /// first triaged) stay `NULL` forever under a normal sweep. `--full` drops `since` for one
    /// sweep, fetching (and re-upserting) every issue regardless.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn full_sweep_backfills_title_and_author_a_normal_sweep_would_skip(
        pool: PgPool,
    ) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);

        // Seed a row the way it would look pre-migration-0004: tracked, but title/author never
        // populated. Its watermark sits strictly after the issue's upstream `updated_at`, so a
        // normal sweep's `since` query excludes it — the upstream content hasn't changed since it
        // was last triaged.
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: "owner/repo#7".to_string(),
                repo: "owner/repo".to_string(),
                priority: 0,
                evidence_url: Some("https://github.com/owner/repo/issues/7".to_string()),
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        crate::issues::store::set_watermark(db.pool(), "owner/repo", "2026-07-02T00:00:00Z")
            .await?;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .and(query_param("since", "2026-07-02T00:00:00Z"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Vec::<serde_json::Value>::new()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .and(query_param_is_missing("since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                7,
                "a bug pre-dating title/author",
                &["bug"],
            )]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }

        // A normal sweep: `since` is sent, the mock reports nothing changed, so the row's
        // title/author stay `NULL` — a normal sweep never re-fetches it.
        triage_repo(&db, "owner/repo", false).await?;
        let unchanged = crate::issues::store::get_issue(db.pool(), "owner/repo#7")
            .await?
            .expect("still tracked");
        assert!(
            unchanged.title.is_none() && unchanged.author.is_none() && unchanged.labels.is_empty(),
            "a normal sweep must not have re-fetched the unchanged issue"
        );

        // `--full`: `since` is dropped, the mock returns the issue, and it re-upserts with
        // title/author populated even though `updated_at` never moved.
        triage_repo(&db, "owner/repo", true).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#7")
            .await?
            .expect("still tracked");
        assert_eq!(got.title.as_deref(), Some("a bug pre-dating title/author"));
        assert_eq!(got.author.as_deref(), Some("octocat"));
        assert_eq!(got.labels, vec!["bug".to_string()], "labels backfilled too");
        Ok(())
    }

    // --- closed upstream issues: skip, retire, reopen -------------------------------------------

    fn raw_closed_issue(number: u64, title: &str) -> serde_json::Value {
        let mut issue = raw_issue(number, title, &[]);
        issue["state"] = serde_json::json!("closed");
        issue
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn closed_untracked_issues_are_skipped_entirely(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);

        let server = MockServer::start().await;
        // The closed issue is the newest in the listing AND claims comments, so this test also
        // pins: no row, no comment GET, and the watermark still advances past it.
        let mut closed = raw_closed_issue(2, "long since fixed");
        closed["comments"] = serde_json::json!(3);
        closed["updated_at"] = serde_json::json!("2026-07-03T00:00:00Z");
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(vec![raw_issue(1, "a live bug", &[]), closed]),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues/2/comments"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        let summary = triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(summary.new, 1);
        assert_eq!(summary.skipped_closed, 1);
        assert_eq!(summary.retired, 0);
        assert!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#1")
                .await?
                .is_some()
        );
        assert!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#2")
                .await?
                .is_none(),
            "a closed, never-tracked issue must not enter the ledger"
        );
        assert_eq!(
            crate::issues::store::get_watermark(db.pool(), "owner/repo")
                .await?
                .as_deref(),
            Some("2026-07-03T00:00:00Z"),
            "the watermark advances past skipped closed issues"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn closed_tracked_issue_retires_with_upstream_closed_park(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                4,
                "tracked bug",
                &[],
            )]))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        triage_repo(&db, "owner/repo", false).await?;
        assert!(
            crate::issues::store::claim_issue(
                db.pool(),
                "owner/repo#4",
                Status::New,
                Status::Scoped
            )
            .await?,
            "seed a non-new status so the retire covers more than the entry state"
        );

        let mut closed = raw_closed_issue(4, "tracked bug");
        closed["updated_at"] = serde_json::json!("2026-07-02T00:00:00Z");
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![closed]))
            .mount(&server)
            .await;
        let summary = triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(summary.retired, 1);
        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#4")
            .await?
            .expect("still tracked");
        assert_eq!(got.status, Status::Parked);
        assert_eq!(got.parked_by, Some(ParkedBy::Machine));
        assert_eq!(got.park_reason(), Some(ParkReason::UpstreamClosed));
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn reopened_issue_returns_to_new(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);
        seed_issue(&db, "owner/repo#5", Some("2026-06-01T00:00:00Z")).await?;
        crate::issues::transitions::park_and_purge(
            db.pool(),
            "owner/repo#5",
            &ParkReason::UpstreamClosed.to_string(),
            ParkedBy::Machine,
        )
        .await?;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                5,
                "back from the dead",
                &[],
            )]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#5")
            .await?
            .expect("tracked");
        assert_eq!(got.status, Status::New, "a reopened issue re-enters at new");
        assert!(got.parked_reason.is_none() && got.parked_by.is_none());
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn terminal_and_human_parked_rows_stay_untouched_on_close(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);
        seed_issue(&db, "owner/repo#6", None).await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), "owner/repo#6", Status::New, Status::Done)
                .await?
        );
        seed_issue(&db, "owner/repo#7", None).await?;
        crate::issues::transitions::park_and_purge(
            db.pool(),
            "owner/repo#7",
            "operator said hands off",
            ParkedBy::Human,
        )
        .await?;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                raw_closed_issue(6, "finished long ago"),
                raw_closed_issue(7, "human-parked"),
            ]))
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        let summary = triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(summary.retired, 0);
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#6")
                .await?
                .expect("tracked")
                .status,
            Status::Done
        );
        let human = crate::issues::store::get_issue(db.pool(), "owner/repo#7")
            .await?
            .expect("tracked");
        assert_eq!(human.status, Status::Parked);
        assert_eq!(human.parked_by, Some(ParkedBy::Human));
        assert_eq!(
            human.parked_reason.as_deref(),
            Some("operator said hands off"),
            "a human park's reason is never relabeled"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn running_row_is_left_to_the_upstream_close_approval(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);
        seed_issue(&db, "owner/repo#8", None).await?;
        assert!(
            crate::issues::store::claim_issue(
                db.pool(),
                "owner/repo#8",
                Status::New,
                Status::Running
            )
            .await?
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(vec![raw_closed_issue(8, "in flight")]),
            )
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        let summary = triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(summary.retired, 0);
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#8")
                .await?
                .expect("tracked")
                .status,
            Status::Running,
            "the pod-stopping upstream-close approval check owns the running edge, not triage"
        );
        assert_eq!(
            summary.changed, 1,
            "the key still rides the sweep so reconcile drives that approval"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn stale_machine_park_is_relabeled_on_close(pool: PgPool) -> Result<()> {
        let _guard = crate::ENV_LOCK.lock().await;
        let (db, _d) = db_with(pool);
        seed_issue(&db, "owner/repo#9", None).await?;
        crate::issues::transitions::park_and_purge(
            db.pool(),
            "owner/repo#9",
            "stale: no upstream activity since 2026-01-01",
            ParkedBy::Machine,
        )
        .await?;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(vec![raw_closed_issue(9, "stale-parked")]),
            )
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("GITHUB_API_URL", server.uri());
        }
        let summary = triage_repo(&db, "owner/repo", false).await?;
        unsafe {
            std::env::remove_var("GITHUB_API_URL");
        }

        assert_eq!(summary.retired, 1);
        let got = crate::issues::store::get_issue(db.pool(), "owner/repo#9")
            .await?
            .expect("tracked");
        assert_eq!(got.status, Status::Parked);
        assert_eq!(
            got.park_reason(),
            Some(ParkReason::UpstreamClosed),
            "the stale reason is relabeled so its closing activity can't auto-unpark it"
        );
        Ok(())
    }

    // --- closed-upstream repair pass ------------------------------------------------------------

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn repair_retires_contaminated_rows_then_quiesces(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::repo_watch::insert_watched_repo(db.pool(), "owner/repo", Some("env"))
            .await?;
        // #1: contaminated (ingested as `new` while closed upstream) — retires.
        seed_issue(&db, "owner/repo#1", Some("2026-05-01T00:00:00Z")).await?;
        // #2: genuinely open — untouched.
        seed_issue(&db, "owner/repo#2", Some("2026-05-01T00:00:00Z")).await?;
        // #3: done — terminal, untouched even though closed upstream.
        seed_issue(&db, "owner/repo#3", None).await?;
        assert!(
            crate::issues::store::claim_issue(db.pool(), "owner/repo#3", Status::New, Status::Done)
                .await?
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .and(query_param_is_missing("since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                raw_closed_issue(1, "closed contamination"),
                raw_issue(2, "still open", &[]),
                raw_closed_issue(3, "closed but done"),
            ]))
            .expect(1)
            .mount(&server)
            .await;

        let summaries = repair_closed_issues_from(&db, &server.uri(), None).await?;
        assert_eq!(
            summaries,
            vec![ClosedRepairSummary {
                repo: "owner/repo".to_string(),
                retired: 1,
            }]
        );
        let retired = crate::issues::store::get_issue(db.pool(), "owner/repo#1")
            .await?
            .expect("tracked");
        assert_eq!(retired.status, Status::Parked);
        assert_eq!(retired.parked_by, Some(ParkedBy::Machine));
        assert_eq!(retired.park_reason(), Some(ParkReason::UpstreamClosed));
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#2")
                .await?
                .expect("tracked")
                .status,
            Status::New,
            "an open row is untouched"
        );
        assert_eq!(
            crate::issues::store::get_issue(db.pool(), "owner/repo#3")
                .await?
                .expect("tracked")
                .status,
            Status::Done,
            "a terminal row is untouched"
        );

        // Second pass: the repo is stamped, so zero GitHub calls (the `.expect(1)` above is the
        // enforcement) and nothing to report.
        let again = repair_closed_issues_from(&db, &server.uri(), None).await?;
        assert!(again.is_empty(), "a repaired repo quiesces the pass");
        Ok(())
    }

    // --- backfill: upstream_updated_at from the issues listing ---------------------------------

    /// Seed a tracked row the way it looks pre-migration-0007: `upstream_updated_at` as given,
    /// everything else minimal.
    async fn seed_issue(db: &Db, key: &str, upstream_updated_at: Option<&str>) -> Result<()> {
        crate::issues::store::upsert_issue(
            db.pool(),
            &NewIssue {
                key: key.to_string(),
                repo: "owner/repo".to_string(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: upstream_updated_at.map(str::to_string),
            },
        )
        .await
    }

    async fn upstream_stamp(db: &Db, key: &str) -> Result<Option<String>> {
        Ok(crate::issues::store::get_issue(db.pool(), key)
            .await?
            .expect("tracked")
            .upstream_updated_at)
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn backfill_stamps_null_rows_only_and_leaves_absent_rows_null(
        pool: PgPool,
    ) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::repo_watch::insert_watched_repo(db.pool(), "owner/repo", Some("env"))
            .await?;
        // #1: NULL, present in the listing — gets stamped.
        seed_issue(&db, "owner/repo#1", None).await?;
        // #2: already stamped by the watermark path — untouched even though the listing disagrees.
        seed_issue(&db, "owner/repo#2", Some("2026-05-05T00:00:00Z")).await?;
        // #3: NULL, absent from the listing (deleted/transferred upstream) — stays NULL.
        seed_issue(&db, "owner/repo#3", None).await?;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .and(query_param_is_missing("since"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![
                raw_issue(1, "a pre-0007 issue", &[]),
                raw_issue(2, "already stamped", &[]),
            ]))
            .mount(&server)
            .await;

        let summaries = backfill_upstream_updated_at_from(&db, &server.uri(), None).await?;
        assert_eq!(
            summaries,
            vec![BackfillSummary {
                repo: "owner/repo".to_string(),
                stamped: 1,
                still_null: 1,
            }]
        );
        assert_eq!(
            upstream_stamp(&db, "owner/repo#1").await?.as_deref(),
            Some("2026-07-01T00:00:00Z"),
            "the NULL row took the listing's updated_at"
        );
        assert_eq!(
            upstream_stamp(&db, "owner/repo#2").await?.as_deref(),
            Some("2026-05-05T00:00:00Z"),
            "a watermark-owned stamp is never overwritten"
        );
        assert_eq!(
            upstream_stamp(&db, "owner/repo#3").await?,
            None,
            "a row missing upstream keeps NULL — no invented timestamp"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn backfill_is_a_no_op_without_null_rows(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::repo_watch::insert_watched_repo(db.pool(), "owner/repo", Some("env"))
            .await?;
        seed_issue(&db, "owner/repo#1", Some("2026-06-01T00:00:00Z")).await?;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let summaries = backfill_upstream_updated_at_from(&db, &server.uri(), None).await?;
        assert!(
            summaries.is_empty(),
            "a fully-stamped repo is skipped without a single HTTP call"
        );
        Ok(())
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn backfill_paginates_via_link_header(pool: PgPool) -> Result<()> {
        let (db, _d) = db_with(pool);
        crate::issues::repo_watch::insert_watched_repo(db.pool(), "owner/repo", Some("env"))
            .await?;
        seed_issue(&db, "owner/repo#1", None).await?;
        seed_issue(&db, "owner/repo#2", None).await?;

        let server = MockServer::start().await;
        let page1_url = format!(
            "{}/repos/owner/repo/issues?state=all&sort=updated&per_page=100",
            server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/issues"))
            .respond_with(move |req: &wiremock::Request| {
                if req.url.query().unwrap_or("").contains("page=2") {
                    ResponseTemplate::new(200).set_body_json(vec![raw_issue(
                        2,
                        "from page two",
                        &[],
                    )])
                } else {
                    ResponseTemplate::new(200)
                        .set_body_json(vec![raw_issue(1, "from page one", &[])])
                        .insert_header("Link", format!(r#"<{page1_url}&page=2>; rel="next""#))
                }
            })
            .expect(2)
            .mount(&server)
            .await;

        let summaries = backfill_upstream_updated_at_from(&db, &server.uri(), None).await?;
        assert_eq!(
            summaries,
            vec![BackfillSummary {
                repo: "owner/repo".to_string(),
                stamped: 2,
                still_null: 0,
            }]
        );
        assert_eq!(
            upstream_stamp(&db, "owner/repo#2").await?.as_deref(),
            Some("2026-07-01T00:00:00Z"),
            "the second page's row landed"
        );
        Ok(())
    }

    // --- repo_exists (Lane O3: POST /api/repos' GitHub-existence check) ------------------------

    #[tokio::test]
    async fn repo_exists_true_on_200() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 1})))
            .mount(&server)
            .await;
        assert!(
            repo_exists_from(&server.uri(), None, "owner/repo")
                .await
                .expect("ok")
        );
    }

    #[tokio::test]
    async fn repo_exists_false_on_404() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/ghost"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(
            !repo_exists_from(&server.uri(), None, "owner/ghost")
                .await
                .expect("ok")
        );
    }

    #[tokio::test]
    async fn repo_exists_retries_after_429_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo"))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "0"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"id": 1})))
            .mount(&server)
            .await;
        assert!(
            repo_exists_from(&server.uri(), None, "owner/repo")
                .await
                .expect("ok after retry")
        );
    }

    #[tokio::test]
    async fn repo_exists_propagates_a_non_retryable_client_error() {
        let server = MockServer::start().await;
        // 403 (rate-limited-without-a-429, or a token lacking scope) is neither a 404 nor
        // retryable — it must surface as an error, not a silent `false`.
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let err = repo_exists_from(&server.uri(), None, "owner/repo")
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("403"));
    }
}
