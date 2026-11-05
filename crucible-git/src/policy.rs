//! What a task may do to GitHub through the HTTPS proxy, decided per decrypted request.
//!
//! | host                      | allowed                                                         |
//! |---------------------------|-----------------------------------------------------------------|
//! | github.com                | git smart-HTTP: fetch a bound or public repo, push a bound one  |
//! | raw / codeload            | GET a bound or public repo's files                               |
//! | api.github.com            | GET a bound or public repo; a few account-free GETs             |
//! |                           | POST pulls (spends the draft-pr count), labels, reviewers,       |
//! |                           | PATCH pulls, on a bound repo                                     |
//! | api.github.com /graphql   | queries of a bound or public `repository`, `viewer`, `rateLimit`, |
//! |                           | and `node(s)` it created; `createPullRequest` on a bound repo     |
//! |                           | (spends the count); label, reviewer and update mutations on pull  |
//! |                           | requests it created                                              |
//!
//! Anything else is refused, including any GraphQL top-level field not listed.

use crate::allow::Allow;
use crate::github::GitHub;
use crate::graphql::{Field, Kind};
use axum::http::{Method, StatusCode};
use std::collections::{HashMap, HashSet};
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

/// What the policy learns while the run goes: which repositories are public, the node ids of the
/// bound repositories, and the pull requests this run created through GraphQL.
#[derive(Default)]
pub struct State {
    public: Mutex<HashMap<String, bool>>,
    node_ids: Mutex<HashMap<String, String>>,
    created: Mutex<HashSet<String>>,
}

const ACCOUNT_FREE: [&str; 4] = ["/", "/meta", "/rate_limit", "/zen"];

const PR_FOLLOW_UPS: [&str; 5] = [
    "requestReviews",
    "addLabelsToLabelable",
    "updatePullRequest",
    "addAssigneesToAssignable",
    "markPullRequestReadyForReview",
];

/// Whether the decision reads the request body: only a GraphQL document.
pub fn needs_body(host: &str, method: &Method, path: &str) -> bool {
    host == "api.github.com" && method == Method::POST && path.split('?').next() == Some("/graphql")
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
        return Verdict::Allow {
            scope: crate::SCOPE_READ,
        };
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
    match (method.clone(), tail.as_slice()) {
        (Method::POST, ["pulls"]) => match allow.open(&repo).await {
            Ok(()) => Verdict::Allow {
                scope: crate::SCOPE_WRITE,
            },
            Err(e) => Verdict::Deny(e.status(), e.to_string()),
        },
        (Method::POST | Method::DELETE, ["issues", _, "labels", ..])
        | (Method::POST | Method::DELETE, ["pulls", _, "requested_reviewers"])
        | (Method::PATCH, ["pulls", _]) => writable(allow, &repo).await,
        _ => deny(format!("crucible-git does not forward {method} {bare}")),
    }
}

async fn writable(allow: &Allow, repo: &str) -> Verdict {
    match allow.repo(repo).await {
        Ok(()) => Verdict::Allow {
            scope: crate::SCOPE_WRITE,
        },
        Err(e) => Verdict::Deny(e.status(), e.to_string()),
    }
}

/// A bound repository, or a public one.
async fn readable(state: &State, gh: &GitHub, allow: &Allow, repo: &str) -> Verdict {
    if allow.repo(repo).await.is_ok() {
        return Verdict::Allow {
            scope: crate::SCOPE_READ,
        };
    }
    let cached = state.public.lock().ok().and_then(|m| m.get(repo).copied());
    let public = match cached {
        Some(p) => p,
        None => match gh.is_public(repo).await {
            Ok(p) => {
                if let Ok(mut m) = state.public.lock() {
                    m.insert(repo.to_string(), p);
                }
                p
            }
            Err(e) => {
                return Verdict::Deny(
                    StatusCode::BAD_GATEWAY,
                    format!("could not tell whether {repo} is public: {e:#}"),
                );
            }
        },
    };
    match public {
        true => Verdict::Allow {
            scope: crate::SCOPE_READ,
        },
        false => deny(format!(
            "{repo} is private and outside this run's [outputs.draft-pr] target"
        )),
    }
}

async fn graphql(state: &State, gh: &GitHub, allow: &Allow, body: &[u8]) -> Verdict {
    let op = match crate::graphql::operation(body) {
        Ok(op) => op,
        Err(e) => return deny(e.to_string()),
    };
    match op.kind {
        Kind::Subscription => deny("crucible-git does not forward GraphQL subscriptions"),
        Kind::Query => {
            for field in &op.fields {
                if let Verdict::Deny(status, why) = query_field(state, gh, allow, field).await {
                    return Verdict::Deny(status, why);
                }
            }
            Verdict::Allow {
                scope: crate::SCOPE_READ,
            }
        }
        Kind::Mutation => mutation(state, gh, allow, &op.fields).await,
    }
}

/// A top-level query field: a bound or public repository, the caller itself, the rate limit, or
/// a pull request this run created.
async fn query_field(state: &State, gh: &GitHub, allow: &Allow, field: &Field) -> Verdict {
    let created = || state.created.lock().map(|c| c.clone()).unwrap_or_default();
    let arg = |k: &str| {
        field
            .args
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    match field.name.as_str() {
        "viewer" | "rateLimit" | "__typename" => Verdict::Allow {
            scope: crate::SCOPE_READ,
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
            Some(id) if created().contains(&id) => Verdict::Allow {
                scope: crate::SCOPE_READ,
            },
            _ => deny("a node query may only read pull requests this run created"),
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
            let created = created();
            match !ids.is_empty() && ids.iter().all(|id| created.contains(id)) {
                true => Verdict::Allow {
                    scope: crate::SCOPE_READ,
                },
                false => deny("a nodes query may only read pull requests this run created"),
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
                Ok(()) => Verdict::Allow {
                    scope: crate::SCOPE_WRITE,
                },
                Err(e) => Verdict::Deny(e.status(), e.to_string()),
            }
        }
        fields
            if !fields.is_empty()
                && fields
                    .iter()
                    .all(|f| PR_FOLLOW_UPS.contains(&f.name.as_str())) =>
        {
            let created = state.created.lock().map(|c| c.clone()).unwrap_or_default();
            for field in fields {
                let input = input(field);
                let ids: Vec<&str> = ["pullRequestId", "labelableId", "assignableId"]
                    .iter()
                    .filter_map(|k| input.get(*k).and_then(|v| v.as_str()))
                    .collect();
                if ids.is_empty() || !ids.iter().all(|id| created.contains(*id)) {
                    return deny(format!(
                        "{} may only touch pull requests this run created",
                        field.name
                    ));
                }
            }
            Verdict::Allow {
                scope: crate::SCOPE_WRITE,
            }
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

/// Record a pull request created through GraphQL, from the response body.
pub fn record_created(state: &State, response: &[u8]) {
    let id = serde_json::from_slice::<serde_json::Value>(response)
        .ok()
        .and_then(|v| {
            v.pointer("/data/createPullRequest/pullRequest/id")
                .and_then(|id| id.as_str().map(str::to_string))
        });
    if let (Some(id), Ok(mut created)) = (id, state.created.lock()) {
        created.insert(id);
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
