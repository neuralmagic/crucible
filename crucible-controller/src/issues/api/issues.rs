use crate::api::dto::*;
use crate::api::state::*;
use crate::issues::model::{IssueQuery, SortKey, UpstreamState};
use crate::model::SortDir;
use crate::model::Status;
use crate::wire_enum::{parse_opt, parse_or_default};
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Per-facet option counts for the issues page. Each facet is counted with every OTHER active
/// filter applied but its own cleared, so a count answers "how many if I picked this instead".
#[derive(Debug, Serialize, ToSchema)]
pub struct IssueFacetsDto {
    /// Issues matching the filter set exactly as given.
    pub total: i64,
    pub kind: Vec<FacetCount>,
    pub status: Vec<FacetCount>,
    pub tier: Vec<FacetCount>,
    pub affinity: Vec<FacetCount>,
    pub repo: Vec<FacetCount>,
}

/// `status=`/`tier=`/`repo=`/`upstream=`/`upstream_since=`/`sort=`/`dir=` — the raw query-string
/// shape for `GET /api/issues`.
#[derive(Debug, Deserialize, Default, Clone, PartialEq)]
pub struct IssuesQuery {
    pub status: Option<String>,
    pub tier: Option<String>,
    pub affinity: Option<String>,
    pub repo: Option<String>,
    pub label: Option<String>,
    pub kind: Option<crate::issues::model::IssueKind>,
    pub exclude_kind: Option<crate::issues::model::IssueKind>,
    pub upstream: Option<String>,
    pub upstream_since: Option<String>,
    pub sort: Option<String>,
    pub dir: Option<String>,
}

impl IssuesQuery {
    /// Parse into the strong [`IssueQuery`] the DB layer takes, or the bad value's message (a
    /// caller error, worth a 400, not a 500).
    pub fn into_model(self) -> Result<IssueQuery, String> {
        let status = parse_opt::<Status>(self.status.as_deref())?;
        let upstream = parse_opt::<UpstreamState>(self.upstream.as_deref())?;
        // Validate the recency cutoff is a real RFC 3339 stamp — the DB comparison is lexical, so
        // a malformed value would silently match nothing instead of erroring.
        let upstream_since = self.upstream_since.filter(|s| !s.is_empty());
        if let Some(s) = upstream_since.as_deref() {
            s.parse::<jiff::Timestamp>()
                .map_err(|e| format!("bad upstream_since `{s}`: {e}"))?;
        }
        let sort = parse_or_default::<SortKey>(self.sort.as_deref())?;
        let dir = parse_or_default::<SortDir>(self.dir.as_deref())?;
        let affinity = parse_opt::<crate::issues::ranker::Affinity>(self.affinity.as_deref())?
            .map(|a| a.as_str().to_string());
        Ok(IssueQuery {
            status,
            tier: self.tier.filter(|t| !t.is_empty()),
            affinity,
            repo: self.repo.filter(|r| !r.is_empty()),
            label: self.label.filter(|l| !l.is_empty()),
            kind: self.kind,
            exclude_kind: self.exclude_kind,
            upstream,
            upstream_since,
            sort,
            dir,
        })
    }
}

#[utoipa::path(
    get,
    path = "/api/issues",
    params(
        ("status" = Option<String>, Query, description = "Filter by status"),
        ("tier" = Option<String>, Query, description = "Filter by tier"),
        ("affinity" = Option<String>, Query, description = "Filter by ranker affinity (perf|perf-adjacent|unrelated)"),
        ("repo" = Option<String>, Query, description = "Filter by repository (exact owner/name)"),
        ("label" = Option<String>, Query, description = "Filter by label"),
        ("kind" = Option<String>, Query, description = "Filter by input kind (github|scenario|jira|playbook)"),
        ("upstream" = Option<String>, Query, description = "Filter by upstream issue state (open|closed)"),
        ("upstream_since" = Option<String>, Query, description = "Keep issues with upstream activity at or after this RFC 3339 stamp"),
        ("sort" = Option<String>, Query, description = "Sort key (updated|upstream|tier|priority|title)"),
        ("dir" = Option<String>, Query, description = "Sort direction (asc|desc)"),
    ),
    responses(
        (status = 200, description = "List of issues matching filters (`parked_reason` is truncated; the full text is on `GET /api/issues/{key}`)", body = Vec<IssueDto>),
        (status = 400, description = "Invalid query parameters", body = ErrorBody)
    )
)]
pub(crate) async fn list_issues(
    State(state): State<ApiState>,
    Query(q): Query<IssuesQuery>,
) -> Result<Response, AppError> {
    let q = match q.into_model() {
        Ok(q) => q,
        Err(msg) => {
            return Ok(bad_request(msg));
        }
    };
    let rows = crate::issues::store::list_issues_filtered(state.db.pool(), &q).await?;
    let mut pr_urls = crate::issues::store::latest_kept_pr_urls(state.db.pool()).await?;
    let dtos: Vec<IssueDto> = rows
        .into_iter()
        .map(|i| {
            let pr_url = pr_urls.remove(&i.key);
            IssueDto::from_parts(i, pr_url).truncated_for_list()
        })
        .collect();
    Ok(Json(dtos).into_response())
}

/// One facet value and how many issues carry it.
#[derive(Debug, Serialize, ToSchema)]
pub struct FacetCount {
    pub value: String,
    pub count: i64,
}

fn tally(values: impl Iterator<Item = String>) -> Vec<FacetCount> {
    let mut counts: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
    for v in values {
        *counts.entry(v).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(value, count)| FacetCount { value, count })
        .collect()
}

#[utoipa::path(
    get,
    path = "/api/issues/facets",
    params(
        ("status" = Option<String>, Query, description = "Filter by status"),
        ("tier" = Option<String>, Query, description = "Filter by tier"),
        ("affinity" = Option<String>, Query, description = "Filter by ranker affinity"),
        ("repo" = Option<String>, Query, description = "Filter by repository (exact owner/name)"),
        ("label" = Option<String>, Query, description = "Filter by label"),
        ("kind" = Option<String>, Query, description = "Filter by input kind (github|scenario|jira|playbook)"),
        ("upstream" = Option<String>, Query, description = "Filter by upstream issue state (open|closed)"),
        ("upstream_since" = Option<String>, Query, description = "Keep issues with upstream activity at or after this RFC 3339 stamp"),
    ),
    responses(
        (status = 200, description = "Per-facet option counts for the current filter set", body = IssueFacetsDto),
        (status = 400, description = "Invalid query parameters", body = ErrorBody)
    )
)]
pub(crate) async fn issue_facets(
    State(state): State<ApiState>,
    Query(q): Query<IssuesQuery>,
) -> Result<Response, AppError> {
    let base = match q.into_model() {
        Ok(q) => q,
        Err(msg) => {
            return Ok(bad_request(msg));
        }
    };

    let pool = state.db.pool();
    let total = i64::try_from(
        crate::issues::store::list_issues_filtered(pool, &base)
            .await?
            .len(),
    )
    .unwrap_or(i64::MAX);

    let without_kind = IssueQuery {
        kind: None,
        ..base.clone()
    };
    let without_status = IssueQuery {
        status: None,
        ..base.clone()
    };
    let without_tier = IssueQuery {
        tier: None,
        ..base.clone()
    };
    let without_affinity = IssueQuery {
        affinity: None,
        ..base.clone()
    };
    let without_repo = IssueQuery {
        repo: None,
        ..base.clone()
    };

    let kind = tally(
        crate::issues::store::list_issues_filtered(pool, &without_kind)
            .await?
            .iter()
            .map(|i| i.kind.tag().to_string()),
    );
    let status = tally(
        crate::issues::store::list_issues_filtered(pool, &without_status)
            .await?
            .iter()
            .map(|i| i.status.as_str().to_string()),
    );
    let tier = tally(
        crate::issues::store::list_issues_filtered(pool, &without_tier)
            .await?
            .into_iter()
            .filter_map(|i| i.tier),
    );
    let affinity = tally(
        crate::issues::store::list_issues_filtered(pool, &without_affinity)
            .await?
            .into_iter()
            .filter_map(|i| i.affinity),
    );
    let repo = tally(
        crate::issues::store::list_issues_filtered(pool, &without_repo)
            .await?
            .into_iter()
            .map(|i| i.repo),
    );

    Ok(Json(IssueFacetsDto {
        total,
        kind,
        status,
        tier,
        affinity,
        repo,
    })
    .into_response())
}

#[utoipa::path(
    get,
    path = "/api/issues/{key}",
    params(
        ("key" = String, Path, description = "Issue key (must be percent-encoded, e.g. owner%2Frepo%231)")
    ),
    responses(
        (status = 200, description = "Full issue provenance: issue row, scopes, runs, candidates, events", body = IssueDetail),
        (status = 404, description = "Issue not found", body = ErrorBody)
    )
)]
pub(crate) async fn get_issue(
    State(state): State<ApiState>,
    Path(key): Path<String>,
) -> Result<Response, AppError> {
    match issue_detail(&state.db, &key).await? {
        Some(detail) => Ok(Json(detail).into_response()),
        None => Ok(not_found(format!("issue not found: {key}"))),
    }
}

/// `GET /api/issues/{key}/journey` — a server-assembled, typed timeline of one issue's full story:
/// discovery, ranking, grounding, scoping, the approval gate, each run, each kept PR, and the
/// terminal. Every step is sourced from existing rows + the event log ([`crate::issues::journey`]); a stage
/// that never happened is simply absent. An unknown issue 404s.
#[utoipa::path(
    get,
    path = "/api/issues/{key}/journey",
    params(
        ("key" = String, Path, description = "Issue key (must be percent-encoded, e.g. owner%2Frepo%231)")
    ),
    responses(
        (status = 200, description = "The issue's chronological timeline of steps", body = crate::issues::journey::JourneyDto),
        (status = 404, description = "Issue not found", body = ErrorBody)
    )
)]
pub(crate) async fn get_issue_journey(
    State(state): State<ApiState>,
    Path(key): Path<String>,
) -> Result<Response, AppError> {
    match crate::issues::journey::assemble_journey(&state.db, &key).await? {
        Some(journey) => Ok(Json(journey).into_response()),
        None => Ok(not_found(format!("issue not found: {key}"))),
    }
}
