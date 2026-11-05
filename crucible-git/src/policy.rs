//! What a task may do to GitHub through the HTTPS proxy, decided per decrypted request.
//!
//! | host                      | allowed                                                         |
//! |---------------------------|-----------------------------------------------------------------|
//! | github.com                | git smart-HTTP: fetch a bound or public repo, push a bound one  |
//! | raw / codeload            | GET a bound or public repo's files                               |
//! | api.github.com            | GET a bound or public repo; a few account-free GETs             |
//! |                           | POST pulls (spends the draft-pr count), labels, reviewers,       |
//! |                           | PATCH pulls, on a bound repo                                     |
//! | api.github.com /graphql   | queries; `createPullRequest` on a bound repo (spends the count); |
//! |                           | label, reviewer and update mutations on pull requests it created |
//!
//! Anything else is refused. GraphQL queries are not scoped to a repository: a query can read any
//! repository the App installation reaches.

use crate::allow::Allow;
use crate::github::GitHub;
use axum::http::{Method, StatusCode};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

pub enum Verdict {
    Allow,
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
        return Verdict::Allow;
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
            Ok(()) => Verdict::Allow,
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
        Ok(()) => Verdict::Allow,
        Err(e) => Verdict::Deny(e.status(), e.to_string()),
    }
}

/// A bound repository, or a public one.
async fn readable(state: &State, gh: &GitHub, allow: &Allow, repo: &str) -> Verdict {
    if allow.repo(repo).await.is_ok() {
        return Verdict::Allow;
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
        true => Verdict::Allow,
        false => deny(format!(
            "{repo} is private and outside this run's [outputs.draft-pr] target"
        )),
    }
}

async fn graphql(state: &State, gh: &GitHub, allow: &Allow, body: &[u8]) -> Verdict {
    let Ok(doc) = serde_json::from_slice::<serde_json::Value>(body) else {
        return deny("the GraphQL request is not JSON");
    };
    let query = doc
        .get("query")
        .and_then(|q| q.as_str())
        .unwrap_or_default();
    let fields = match mutation_fields(query) {
        None => return Verdict::Allow,
        Some(f) => f,
    };
    let vars = doc.get("variables").cloned().unwrap_or_default();
    let input = vars.get("input").cloned().unwrap_or_default();
    match fields.as_slice() {
        [one] if one == "createPullRequest" => {
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
            if repo_id != node {
                return deny(format!(
                    "createPullRequest targets a repository other than {target}"
                ));
            }
            match allow.open(&target).await {
                Ok(()) => Verdict::Allow,
                Err(e) => Verdict::Deny(e.status(), e.to_string()),
            }
        }
        fields if fields.iter().all(|f| PR_FOLLOW_UPS.contains(&f.as_str())) => {
            let ids = ["pullRequestId", "labelableId", "assignableId"]
                .iter()
                .filter_map(|k| input.get(*k).and_then(|v| v.as_str()))
                .collect::<Vec<_>>();
            let created = state.created.lock().map(|c| c.clone()).unwrap_or_default();
            match !ids.is_empty() && ids.iter().all(|id| created.contains(*id)) {
                true => Verdict::Allow,
                false => deny("this mutation may only touch pull requests this run created"),
            }
        }
        fields => deny(format!(
            "crucible-git does not forward the mutation {}",
            fields.join(", ")
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

/// The top-level field names of the document's mutation, or `None` when it is not one.
fn mutation_fields(query: &str) -> Option<Vec<String>> {
    let src = strip(query);
    let doc = src.trim_start();
    let rest = doc.strip_prefix("mutation")?;
    if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let mut depth = 0usize;
    let mut paren = 0usize;
    let mut fields = Vec::new();
    let mut token = String::new();
    for c in rest.chars() {
        let ident = c.is_alphanumeric() || c == '_';
        let top = depth == 1 && paren == 0;
        if top && !ident && !token.is_empty() {
            if c == ':' {
                token.clear();
                continue;
            }
            fields.push(std::mem::take(&mut token));
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
            }
            '(' => paren += 1,
            ')' => paren = paren.saturating_sub(1),
            c if ident && top => token.push(c),
            _ => {}
        }
    }
    Some(fields)
}

/// The document without comments and string literals.
fn strip(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '#' => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
                out.push(' ');
            }
            '"' => {
                let mut escaped = false;
                for c in chars.by_ref() {
                    match (escaped, c) {
                        (false, '\\') => escaped = true,
                        (false, '"') => break,
                        _ => escaped = false,
                    }
                }
                out.push_str("\"\"");
            }
            c => out.push(c),
        }
    }
    out
}
