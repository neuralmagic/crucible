//! The GitHub issue reader behind `scope --issue` and `rank-grounded --issue`.
//!
//! Honors `GITHUB_TOKEN`/`GH_TOKEN` for private repos and rate limits (unauthenticated works for
//! public repos) and `GITHUB_API_URL` for GHES, the same variables Actions sets.

/// An issue as the two callers need it: scope renders the goal from title + body, rank-grounded
/// also prompts on the label names.
pub struct Issue {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum IssueError {
    #[error("--issue must be owner/repo#N, got {issue:?}")]
    MalformedRef { issue: String },
    #[error("--issue number isn't a valid integer: {issue:?}")]
    NumberNotAnInteger {
        issue: String,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("building the GitHub API client")]
    Client(#[source] reqwest::Error),
    #[error("GET {url}")]
    Request {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("GET {url} returned {status}")]
    Status {
        url: String,
        status: reqwest::StatusCode,
    },
    #[error("decoding the response from {url}")]
    Decode {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("issue {repo}#{number} has no title or body")]
    Empty { repo: String, number: u64 },
}

/// `owner/repo#N` -> `(owner/repo, N)`.
pub fn parse_ref(issue: &str) -> Result<(String, u64), IssueError> {
    let (repo, number) = issue
        .split_once('#')
        .ok_or_else(|| IssueError::MalformedRef {
            issue: issue.to_owned(),
        })?;
    let number = number
        .parse()
        .map_err(|source| IssueError::NumberNotAnInteger {
            issue: issue.to_owned(),
            source,
        })?;
    Ok((repo.to_string(), number))
}

/// Fetch one issue over the REST API. `user_agent` names the calling command; GitHub rejects
/// requests without one.
pub fn fetch(repo: &str, number: u64, user_agent: &str) -> Result<Issue, IssueError> {
    let api = std::env::var("GITHUB_API_URL").unwrap_or_else(|_| "https://api.github.com".into());
    let url = format!("{}/repos/{repo}/issues/{number}", api.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .user_agent(user_agent.to_owned())
        .build()
        .map_err(IssueError::Client)?;
    let mut req = client
        .get(&url)
        .header("Accept", "application/vnd.github+json");
    let token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|t| !t.is_empty());
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().map_err(|source| IssueError::Request {
        url: url.clone(),
        source,
    })?;
    let status = resp.status();
    if !status.is_success() {
        return Err(IssueError::Status { url, status });
    }
    let issue: serde_json::Value = resp
        .json()
        .map_err(|source| IssueError::Decode { url, source })?;
    let title = issue["title"].as_str().unwrap_or("").to_string();
    let body = issue["body"].as_str().unwrap_or("").to_string();
    if title.is_empty() && body.is_empty() {
        return Err(IssueError::Empty {
            repo: repo.to_owned(),
            number,
        });
    }
    let labels = issue["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(Issue {
        title,
        body,
        labels,
    })
}
