//! What a promotion asks of a forge: whether the person may push to the target, one commit and a
//! pull or merge request written with their token, and a conditional read of its state with the
//! controller's own.

use std::collections::BTreeMap;
use std::time::Duration;

use crucible_ci::gitlab::{Gitlab, GitlabError, ProjectPath};
use crucible_ci::token::BearerToken;
use forge::github::{Client, GithubError, Repo, Since, Token};

use crate::identity::forge::{Forge, ForgeHost};
use crate::playbooks::promotion::PromotionState;

const HTTP_TIMEOUT: Duration = Duration::from_secs(60);
const DEVELOPER: u32 = 30;

/// Why a forge step failed: the forge refused it, or it could not be asked.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ForgeError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Unreachable(String),
}

impl From<GithubError> for ForgeError {
    fn from(e: GithubError) -> Self {
        match e.is_transient() {
            true => ForgeError::Unreachable(e.to_string()),
            false => ForgeError::Refused(e.to_string()),
        }
    }
}

impl From<GitlabError> for ForgeError {
    fn from(e: GitlabError) -> Self {
        match e.is_transient() {
            true => ForgeError::Unreachable(e.to_string()),
            false => ForgeError::Refused(e.to_string()),
        }
    }
}

/// A checked target: the forge's own name for the project, and the client that reaches it.
pub(crate) enum Target {
    Github(Repo),
    Gitlab(Gitlab, ProjectPath),
}

impl Target {
    pub(crate) fn new(forge: Forge, host: &ForgeHost, project: &str) -> Result<Self, ForgeError> {
        match forge {
            Forge::Github => Repo::parse(project)
                .map(Target::Github)
                .map_err(|e| ForgeError::Refused(e.to_string())),
            Forge::Gitlab => {
                let project =
                    ProjectPath::parse(project).map_err(|e| ForgeError::Refused(e.to_string()))?;
                let http = reqwest::Client::builder()
                    .timeout(HTTP_TIMEOUT)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|e| ForgeError::Unreachable(e.to_string()))?;
                let api = Gitlab::new(http, &format!("https://{}", host.as_str()))?;
                Ok(Target::Gitlab(api, project))
            }
        }
    }
}

/// What the person asked to write, and where.
pub(crate) struct Write<'a> {
    pub base: Option<&'a str>,
    pub branch: &'a str,
    pub path: &'a str,
    pub files: &'a BTreeMap<String, Vec<u8>>,
    pub message: &'a str,
    pub title: &'a str,
    pub body: &'a str,
}

/// The pull or merge request a write opened or kept.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Opened {
    pub base: String,
    pub number: u64,
    pub url: String,
}

pub(crate) async fn propose(
    target: &Target,
    account_id: u64,
    token: &str,
    write: &Write<'_>,
) -> Result<Opened, ForgeError> {
    match target {
        Target::Github(repo) => {
            let client = Client::new()?;
            let token = Token::new(token);
            let repository = client.repository(&token, repo).await?;
            if !repository.permissions.is_some_and(|p| p.push) {
                return Err(ForgeError::Refused(format!(
                    "your GitHub account may not push to {repo}"
                )));
            }
            let base = write.base.map_or(repository.default_branch, str::to_owned);
            let pull = client
                .propose(
                    &token,
                    repo,
                    &forge::github::pulls::Proposal {
                        base: &base,
                        branch: write.branch,
                        path: write.path,
                        files: write.files,
                        message: write.message,
                        title: write.title,
                        body: write.body,
                    },
                )
                .await?;
            Ok(Opened {
                base,
                number: pull.number,
                url: pull.html_url,
            })
        }
        Target::Gitlab(api, project) => {
            let token = BearerToken::new(token);
            let held = api.member_access(&token, project, account_id).await?;
            if !held.is_some_and(|m| m.is_active() && m.access_level >= DEVELOPER) {
                return Err(ForgeError::Refused(format!(
                    "your GitLab account may not push to {}: it needs an active Developer role or \
                     above",
                    project.as_str()
                )));
            }
            let base = match write.base {
                Some(base) => base.to_owned(),
                None => api
                    .project(&token, project)
                    .await?
                    .default_branch
                    .ok_or_else(|| {
                        ForgeError::Refused(format!("{} has no default branch", project.as_str()))
                    })?,
            };
            let mr = api
                .propose(
                    &token,
                    project,
                    &crucible_ci::gitlab::merge::Proposal {
                        base: &base,
                        branch: write.branch,
                        path: write.path,
                        files: write.files,
                        message: write.message,
                        title: write.title,
                        description: write.body,
                    },
                )
                .await?;
            Ok(Opened {
                base,
                number: mr.iid,
                url: mr.web_url,
            })
        }
    }
}

/// One look at a promotion's pull or merge request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Looked {
    Unchanged,
    Now {
        state: PromotionState,
        etag: Option<String>,
    },
}

pub(crate) async fn look(
    target: &Target,
    number: u64,
    token: Option<&str>,
    etag: Option<&str>,
) -> Result<Looked, ForgeError> {
    match target {
        Target::Github(repo) => {
            let client = Client::new()?;
            let token = Token::new(token.ok_or_else(|| {
                ForgeError::Refused(
                    "this controller has no GitHub credential to read the pull request with \
                     (CONTROLLER_CALLS_GITHUB_TOKEN or the GitHub App)"
                        .to_owned(),
                )
            })?);
            match client.pull_since(&token, repo, number, etag).await? {
                Since::Unchanged => Ok(Looked::Unchanged),
                Since::Changed { value, etag } => Ok(Looked::Now {
                    state: github_state(&value),
                    etag,
                }),
            }
        }
        Target::Gitlab(api, project) => {
            let token = BearerToken::new(token.ok_or_else(|| {
                ForgeError::Refused(
                    "this controller has no GitLab credential for this host to read the merge \
                     request with (CONTROLLER_CALLS_GITLAB_TOKEN)"
                        .to_owned(),
                )
            })?);
            match api
                .merge_request_since(&token, project, number, etag)
                .await?
            {
                crucible_ci::gitlab::Since::Unchanged => Ok(Looked::Unchanged),
                crucible_ci::gitlab::Since::Changed { value, etag } => Ok(Looked::Now {
                    state: gitlab_state(&value.state),
                    etag,
                }),
            }
        }
    }
}

fn github_state(pull: &forge::github::pulls::PullRequest) -> PromotionState {
    match (pull.merged, pull.state.as_str()) {
        (true, _) => PromotionState::Merged,
        (false, "closed") => PromotionState::Closed,
        _ => PromotionState::Open,
    }
}

fn gitlab_state(state: &str) -> PromotionState {
    match state {
        "merged" => PromotionState::Merged,
        "closed" | "locked" => PromotionState::Closed,
        _ => PromotionState::Open,
    }
}

#[cfg(test)]
mod tests {
    use crate::playbooks::promotion::forge::*;

    fn pull(state: &str, merged: bool) -> forge::github::pulls::PullRequest {
        forge::github::pulls::PullRequest {
            number: 1,
            html_url: "u".into(),
            state: state.into(),
            merged,
            merge_commit_sha: None,
        }
    }

    #[test]
    fn a_review_reads_as_open_merged_or_closed_on_either_forge() {
        assert_eq!(github_state(&pull("open", false)), PromotionState::Open);
        assert_eq!(github_state(&pull("closed", true)), PromotionState::Merged);
        assert_eq!(github_state(&pull("closed", false)), PromotionState::Closed);
        assert_eq!(gitlab_state("opened"), PromotionState::Open);
        assert_eq!(gitlab_state("merged"), PromotionState::Merged);
        assert_eq!(gitlab_state("closed"), PromotionState::Closed);
        assert_eq!(gitlab_state("locked"), PromotionState::Closed);
    }

    #[test]
    fn a_target_takes_the_forges_own_project_names() {
        let gh = ForgeHost::github();
        assert!(matches!(
            Target::new(Forge::Github, &gh, "acme/packs"),
            Ok(Target::Github(_))
        ));
        assert!(matches!(
            Target::new(Forge::Github, &gh, "acme"),
            Err(ForgeError::Refused(_))
        ));
        let gl = ForgeHost::parse("gitlab.example.com").unwrap();
        assert!(matches!(
            Target::new(Forge::Gitlab, &gl, "group/sub/packs"),
            Ok(Target::Gitlab(..))
        ));
        assert!(matches!(
            Target::new(Forge::Gitlab, &gl, "packs"),
            Err(ForgeError::Refused(_))
        ));
    }
}
