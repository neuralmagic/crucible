//! What a caller may borrow the run's token for, decided per request.
//!
//! | host                      | allowed                                                          |
//! |---------------------------|------------------------------------------------------------------|
//! | github.com                | git smart-HTTP: fetch a bound or public repo, push a bound one   |
//! | raw / codeload            | GET a bound or public repo's files                                |
//! | api.github.com            | GET a bound or public repo; a few account-free GETs              |
//! |                           | POST pulls on a bound repo (spends the draft-pr count); labels,   |
//! |                           | reviewers and PATCH on pull requests the run opened               |
//! | api.github.com /graphql   | queries of a bound or public `repository`, `viewer`'s own login,  |
//! |                           | `rateLimit`, and `node(s)` the run opened; `createPullRequest` on |
//! |                           | a bound repo (spends the count); label, reviewer and update       |
//! |                           | mutations on pull requests the run opened                         |
//!
//! Anything else is refused: a path that is not canonical, any GraphQL top-level field not listed,
//! and any selection beneath an allowed field that crosses to another owner's repositories.

use crate::allow::Allow;
use crate::github::GitHub;
use crate::graphql::{Field, Kind};
use axum::http::{Method, StatusCode};
use std::collections::HashMap;
use std::sync::Mutex;

pub enum Verdict {
    /// Lend the run's token, to a caller holding `scope`.
    Allow {
        scope: &'static str,
    },
    Deny(StatusCode, String),
}

fn deny(why: impl Into<String>) -> Verdict {
    Verdict::Deny(StatusCode::FORBIDDEN, why.into())
}

const READ: Verdict = Verdict::Allow {
    scope: crate::SCOPE_READ,
};
const WRITE: Verdict = Verdict::Allow {
    scope: crate::SCOPE_WRITE,
};

/// What the policy learns while the run goes: which repositories are public, and the node ids of
/// the bound repositories.
#[derive(Default)]
pub struct State {
    public: Mutex<HashMap<String, bool>>,
    node_ids: Mutex<HashMap<String, String>>,
}

/// Visibility lookups kept before the cache starts over, so a caller naming endless repositories
/// cannot grow it without bound.
const PUBLIC_CACHE_LIMIT: usize = 512;

const ACCOUNT_FREE: [&str; 4] = ["/", "/meta", "/rate_limit", "/zen"];

const PR_FOLLOW_UPS: [&str; 5] = [
    "requestReviews",
    "addLabelsToLabelable",
    "updatePullRequest",
    "addAssigneesToAssignable",
    "markPullRequestReadyForReview",
];

/// What a caller may select under `viewer`: who it is, and nothing it can reach.
const VIEWER_FIELDS: [&str; 5] = ["login", "name", "id", "databaseId", "__typename"];

/// Fields that open another owner's or repository's contents, or a search. Refused anywhere
/// beneath an allowed field.
const CROSSING: [&str; 18] = [
    "repository",
    "repositories",
    "repositoryOwner",
    "organization",
    "organizations",
    "user",
    "users",
    "viewer",
    "search",
    "node",
    "enterprise",
    "forks",
    "starredRepositories",
    "watching",
    "pinnedItems",
    "topRepositories",
    "contributionsCollection",
    "sponsoring",
];

/// Fields that name another repository, which a caller may select for its identity alone (`gh`
/// reads a repository's fork parent), from [`SHALLOW`] and nothing deeper.
const IDENTITY_ONLY: [&str; 2] = ["parent", "templateRepository"];

/// What may be selected beneath an [`IDENTITY_ONLY`] field.
const SHALLOW: [&str; 18] = [
    "description",
    "hasIssuesEnabled",
    "hasWikiEnabled",
    "id",
    "databaseId",
    "name",
    "nameWithOwner",
    "owner",
    "login",
    "url",
    "__typename",
    "isPrivate",
    "isFork",
    "isArchived",
    "visibility",
    "viewerPermission",
    "defaultBranchRef",
    "isInOrganization",
];

/// Why a selection beneath an allowed field may not run, if it may not.
fn crosses(path: &[String]) -> Option<String> {
    for (i, seg) in path.iter().enumerate() {
        if CROSSING.contains(&seg.as_str()) {
            return Some(format!("{seg} reaches other repositories"));
        }
        if IDENTITY_ONLY.contains(&seg.as_str()) {
            return path[i + 1..]
                .iter()
                .find(|s| !SHALLOW.contains(&s.as_str()))
                .map(|s| format!("{seg} may only select another repository's identity, not {s}"));
        }
    }
    None
}

/// Whether the decision reads the response: a GraphQL request, or a REST pull request create,
/// whose result names the pull request the run now owns.
pub fn reads_response(host: &str, method: &Method, path: &str) -> bool {
    needs_body(host, method, path) || create_pull_target(host, method, path).is_some()
}

/// Whether the decision reads the request body: only a GraphQL document.
pub fn needs_body(host: &str, method: &Method, path: &str) -> bool {
    host == "api.github.com" && method == Method::POST && path.split('?').next() == Some("/graphql")
}

fn create_pull_target(host: &str, method: &Method, path: &str) -> Option<String> {
    if host != "api.github.com" || method != Method::POST {
        return None;
    }
    let rest = path.split('?').next()?.strip_prefix("/repos/")?;
    let (repo, tail) = repo_of(rest)?;
    (tail == ["pulls"]).then_some(repo)
}

/// Whether `path` names exactly what it spells: no empty, `.` or `..` segment, no encoded
/// separator, and nothing a URL parser would rewrite. The decision reads the path as written and
/// the upstream request is built from it, so the two must not be able to disagree.
fn canonical(path: &str) -> bool {
    let bare = path.split('?').next().unwrap_or("");
    if !bare.starts_with('/') || bare.contains('\\') {
        return false;
    }
    let lowered = bare.to_ascii_lowercase();
    if lowered.contains("%2e") || lowered.contains("%2f") || lowered.contains("%5c") {
        return false;
    }
    let segments: Vec<&str> = bare[1..].split('/').collect();
    let last = segments.len().saturating_sub(1);
    for (i, seg) in segments.iter().enumerate() {
        if *seg == "." || *seg == ".." || (seg.is_empty() && i != last) {
            return false;
        }
    }
    url::Url::parse(&format!("https://h{path}")).is_ok_and(|u| u.path() == bare)
}

/// `owner/name` from the first two segments of `path`, the name without `.git`.
fn repo_of(path: &str) -> Option<(String, Vec<String>)> {
    let path = path.split('?').next().unwrap_or("");
    let mut segs = path.trim_start_matches('/').split('/');
    let owner = segs.next()?;
    let name = segs.next()?;
    let name = name.strip_suffix(".git").unwrap_or(name);
    if !crate::proxy::is_name(owner) || !crate::proxy::is_name(name) {
        return None;
    }
    Some((
        format!("{owner}/{name}"),
        segs.map(str::to_string).collect(),
    ))
}

pub async fn decide(
    state: &State,
    gh: &GitHub,
    allow: &Allow,
    host: &str,
    method: &Method,
    path: &str,
    body: Option<&[u8]>,
) -> Verdict {
    if !canonical(path) {
        return deny("the request path is not canonical");
    }
    let read = matches!(*method, Method::GET | Method::HEAD);
    match host {
        "github.com" => {
            let Some((repo, rest)) = repo_of(path) else {
                return deny("github.com serves git's smart-HTTP endpoints only");
            };
            let query = path.split_once('?').map(|(_, q)| q);
            match crate::proxy::endpoint(&rest.join("/"), query) {
                Some(crate::SCOPE_WRITE) => writable(allow, &repo).await,
                Some(_) => readable(state, gh, allow, &repo).await,
                None => deny("github.com serves git's smart-HTTP endpoints only"),
            }
        }
        "raw.githubusercontent.com" | "codeload.github.com" => match (read, repo_of(path)) {
            (true, Some((repo, _))) => readable(state, gh, allow, &repo).await,
            _ => deny(format!("{host} is read-only and repository-scoped")),
        },
        "api.github.com" => api(state, gh, allow, method, path, body).await,
        _ => deny(format!("{host} is not intercepted")),
    }
}

async fn api(
    state: &State,
    gh: &GitHub,
    allow: &Allow,
    method: &Method,
    path: &str,
    body: Option<&[u8]>,
) -> Verdict {
    let bare = path.split('?').next().unwrap_or("");
    if bare == "/graphql" && method == Method::POST {
        return graphql(state, gh, allow, body.unwrap_or_default()).await;
    }
    let read = matches!(*method, Method::GET | Method::HEAD);
    if read && ACCOUNT_FREE.contains(&bare) {
        return READ;
    }
    let Some(rest) = bare.strip_prefix("/repos/") else {
        return deny(format!("crucible-git does not forward {method} {bare}"));
    };
    let Some((repo, tail)) = repo_of(rest) else {
        return deny(format!("{bare} names no repository"));
    };
    if read {
        return readable(state, gh, allow, &repo).await;
    }
    let tail: Vec<&str> = tail.iter().map(String::as_str).collect();
    let follow_up = |n: &str| match n.parse::<u64>() {
        Ok(n) if allow.opened_number(&repo, n) => None,
        _ => Some(deny(format!(
            "{method} {bare} may only touch pull requests this run opened"
        ))),
    };
    match (method.clone(), tail.as_slice()) {
        (Method::POST, ["pulls"]) => match allow.open(&repo).await {
            Ok(()) => WRITE,
            Err(e) => Verdict::Deny(e.status(), e.to_string()),
        },
        (Method::POST | Method::DELETE, ["issues", n, "labels"])
        | (Method::DELETE, ["issues", n, "labels", _])
        | (Method::POST | Method::DELETE, ["pulls", n, "requested_reviewers"])
        | (Method::PATCH, ["pulls", n]) => match follow_up(n) {
            Some(refused) => refused,
            None => writable(allow, &repo).await,
        },
        _ => deny(format!("crucible-git does not forward {method} {bare}")),
    }
}

async fn writable(allow: &Allow, repo: &str) -> Verdict {
    match allow.repo(repo).await {
        Ok(()) => WRITE,
        Err(e) => Verdict::Deny(e.status(), e.to_string()),
    }
}

/// A bound repository, or a public one. A repository the App cannot see and a private one are
/// refused alike, so the answer tells a caller nothing about which.
async fn readable(state: &State, gh: &GitHub, allow: &Allow, repo: &str) -> Verdict {
    if allow.repo(repo).await.is_ok() {
        return READ;
    }
    let key = repo.to_ascii_lowercase();
    let cached = state.public.lock().ok().and_then(|m| m.get(&key).copied());
    let public = match cached {
        Some(p) => p,
        None => {
            let p = gh.is_public(repo).await.unwrap_or(false);
            if let Ok(mut m) = state.public.lock() {
                if m.len() >= PUBLIC_CACHE_LIMIT {
                    m.clear();
                }
                m.insert(key, p);
            }
            p
        }
    };
    match public {
        true => READ,
        false => deny(format!("{repo} is not readable by this run")),
    }
}

async fn graphql(state: &State, gh: &GitHub, allow: &Allow, body: &[u8]) -> Verdict {
    let op = match crate::graphql::operation(body) {
        Ok(op) => op,
        Err(e) => return deny(e.to_string()),
    };
    for field in &op.fields {
        if let Some(why) = field.nested.iter().find_map(|p| crosses(p)) {
            return deny(format!("{}: {why}", field.name));
        }
    }
    match op.kind {
        Kind::Subscription => deny("crucible-git does not forward GraphQL subscriptions"),
        Kind::Query => {
            for field in &op.fields {
                if let Verdict::Deny(status, why) = query_field(state, gh, allow, field).await {
                    return Verdict::Deny(status, why);
                }
            }
            READ
        }
        Kind::Mutation => mutation(state, gh, allow, &op.fields).await,
    }
}

/// A top-level query field: a bound or public repository, the caller's own identity, the rate
/// limit, or a pull request this run opened.
async fn query_field(state: &State, gh: &GitHub, allow: &Allow, field: &Field) -> Verdict {
    let arg = |k: &str| {
        field
            .args
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    match field.name.as_str() {
        "rateLimit" | "__typename" => READ,
        "viewer" => match field
            .nested
            .iter()
            .flatten()
            .all(|n| VIEWER_FIELDS.contains(&n.as_str()))
        {
            true => READ,
            false => deny("viewer may only select its own login, name and ids"),
        },
        "repository" => match (arg("owner"), arg("name")) {
            (Some(owner), Some(name))
                if crate::proxy::is_name(&owner) && crate::proxy::is_name(&name) =>
            {
                readable(state, gh, allow, &format!("{owner}/{name}")).await
            }
            _ => deny("a repository query needs a literal or variable owner and name"),
        },
        "node" => match arg("id") {
            Some(id) if allow.opened_node(&id) => READ,
            _ => deny("a node query may only read pull requests this run opened"),
        },
        "nodes" => {
            let ids: Vec<String> = field
                .args
                .get("ids")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            match !ids.is_empty() && ids.iter().all(|id| allow.opened_node(id)) {
                true => READ,
                false => deny("a nodes query may only read pull requests this run opened"),
            }
        }
        other => deny(format!(
            "crucible-git does not forward the GraphQL query field {other}"
        )),
    }
}

async fn mutation(state: &State, gh: &GitHub, allow: &Allow, fields: &[Field]) -> Verdict {
    let input = |f: &Field| f.args.get("input").cloned().unwrap_or_default();
    match fields {
        [one] if one.name == "createPullRequest" => {
            let input = input(one);
            let Some(repo_id) = input.get("repositoryId").and_then(|v| v.as_str()) else {
                return deny("createPullRequest needs input.repositoryId");
            };
            let target = match allow.target().await {
                Ok(Some(t)) => t,
                Ok(None) => return deny("this run's draft-pr target names no single repository"),
                Err(e) => return Verdict::Deny(e.status(), e.to_string()),
            };
            let node = match node_id(state, gh, &target).await {
                Ok(n) => n,
                Err(e) => {
                    return Verdict::Deny(
                        StatusCode::BAD_GATEWAY,
                        format!("resolving {target}'s node id: {e:#}"),
                    );
                }
            };
            let head_repo = input.get("headRepositoryId").and_then(|v| v.as_str());
            if repo_id != node || head_repo.is_some_and(|h| h != node) {
                return deny(format!(
                    "createPullRequest targets a repository other than {target}"
                ));
            }
            match allow.open(&target).await {
                Ok(()) => WRITE,
                Err(e) => Verdict::Deny(e.status(), e.to_string()),
            }
        }
        fields
            if !fields.is_empty()
                && fields
                    .iter()
                    .all(|f| PR_FOLLOW_UPS.contains(&f.name.as_str())) =>
        {
            for field in fields {
                let input = input(field);
                let ids: Vec<&str> = ["pullRequestId", "labelableId", "assignableId"]
                    .iter()
                    .filter_map(|k| input.get(*k).and_then(|v| v.as_str()))
                    .collect();
                if ids.is_empty() || !ids.iter().all(|id| allow.opened_node(id)) {
                    return deny(format!(
                        "{} may only touch pull requests this run opened",
                        field.name
                    ));
                }
            }
            WRITE
        }
        fields => deny(format!(
            "crucible-git does not forward the mutation {}",
            fields
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Remember the pull request a lent request opened, from GitHub's response: a REST create on
/// `path`, or a GraphQL `createPullRequest` on the bound repository.
pub async fn record_created(
    allow: &Allow,
    host: &str,
    method: &Method,
    path: &str,
    response: &[u8],
) {
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(response) else {
        return;
    };
    if let Some(repo) = create_pull_target(host, method, path) {
        allow.record(
            &repo,
            doc.get("number").and_then(|n| n.as_u64()),
            doc.get("node_id").and_then(|n| n.as_str()),
        );
        return;
    }
    let Some(pr) = doc.pointer("/data/createPullRequest/pullRequest") else {
        return;
    };
    if let Ok(Some(target)) = allow.target().await {
        allow.record(
            &target,
            pr.get("number").and_then(|n| n.as_u64()),
            pr.get("id").and_then(|n| n.as_str()),
        );
    }
}

async fn node_id(state: &State, gh: &GitHub, repo: &str) -> anyhow::Result<String> {
    if let Some(id) = state
        .node_ids
        .lock()
        .ok()
        .and_then(|m| m.get(repo).cloned())
    {
        return Ok(id);
    }
    let id = gh.node_id(repo).await?;
    if let Ok(mut m) = state.node_ids.lock() {
        m.insert(repo.to_string(), id.clone());
    }
    Ok(id)
}
