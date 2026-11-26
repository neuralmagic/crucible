//! The `github_workflow` call target over forge's GitHub Actions client: the
//! interface read from the workflow file at a commit, dispatch, progress from the run's jobs,
//! the outcome read from the declared artifact, job logs, and cancel.

pub mod artifact;
mod backend;
pub mod progress;
pub mod workflow;

use std::collections::BTreeMap;
use std::time::Duration;

use crucible_contract::calls::{
    CallBudget, CallStatus, CallTarget, FileRef, GitRefName, GithubOutputs, GithubRepo, LockedCall,
    MAX_GITHUB_INPUTS, NodeLink, Pin, ProgressGraph, TargetHandle, TargetInterface, WorkflowPath,
};
use crucible_contract::link::ExternalUrl;
use forge::github::workflow::Workflow;
use forge::github::{Client, CommitSha, GithubError, Repo, RunStatus, Token, WorkflowRun};
use serde_json::{Value, json};

use crate::authz::model::Principal;
use crate::calls::github::artifact::{ResultFile, read_result};
use crate::calls::github::progress::{Settle, job_of, run_graph, settle};
use crate::calls::github::workflow::{dispatch_inputs, interface};
use crate::identity::github_logins::GithubLogins;
use crate::secrets::github_app::{Access, GithubAppTokenSource, GithubOwner, RepoDenied};

const _: () = assert!(forge::github::workflow::MAX_INPUTS == MAX_GITHUB_INPUTS);

const GH_CLI_TIMEOUT: Duration = Duration::from_secs(15);
const ERR_BODY_CHARS: usize = 300;

const CALL_PERMISSIONS: &[(&str, Access)] =
    &[("actions", Access::Write), ("contents", Access::Read)];

/// Why a GitHub target operation failed.
#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error(transparent)]
    Github(#[from] GithubError),
    #[error("`gh auth token` failed: {0}")]
    GhCli(String),
    #[error("{0}")]
    Denied(RepoDenied),
    #[error("minting a GitHub App token: {0:#}")]
    AppToken(anyhow::Error),
    #[error("{0} is not a github_workflow target")]
    WrongKind(String),
    #[error("{0}")]
    Refused(String),
}

impl TargetError {
    pub fn is_transient(&self) -> bool {
        match self {
            TargetError::Github(e) => e.is_transient(),
            TargetError::GhCli(_) | TargetError::AppToken(_) => true,
            TargetError::Denied(_) | TargetError::WrongKind(_) | TargetError::Refused(_) => false,
        }
    }
}

/// Where the backend takes the token for a repository.
#[derive(Clone, Debug)]
pub(crate) enum TokenSource {
    Person(crate::identity::forge::held::SignedIn),
    App {
        app: Box<GithubAppTokenSource>,
        logins: GithubLogins,
    },
}

/// What a dispatch created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubDispatched {
    pub handle: TargetHandle,
    pub link: Option<NodeLink>,
    pub commit: String,
}

/// A completed run's settlement.
#[derive(Clone, Debug, PartialEq)]
pub struct GithubOutcome {
    pub status: CallStatus,
    pub note: String,
    pub result: Value,
    pub files: Vec<ResultFile>,
}

/// One poll of a run: its progress, and its outcome once it completed.
#[derive(Clone, Debug, PartialEq)]
pub struct RunPoll {
    pub progress: ProgressGraph,
    pub head_sha: String,
    pub run_number: Option<u64>,
    pub outcome: Option<GithubOutcome>,
}

/// The `github_workflow` target backend.
pub struct GithubWorkflowBackend {
    client: Client,
    tokens: TokenSource,
    max_wall_clock: Duration,
}

impl GithubWorkflowBackend {
    pub(crate) fn new(tokens: TokenSource, max_wall_clock: Duration) -> Result<Self, TargetError> {
        Ok(GithubWorkflowBackend {
            client: Client::new()?,
            tokens,
            max_wall_clock,
        })
    }

    pub fn wall_clock(&self, budget: Option<CallBudget>) -> Duration {
        budget
            .and_then(|b| b.wall_clock_secs)
            .filter(|secs| *secs > 0)
            .map(Duration::from_secs)
            .map_or(self.max_wall_clock, |own| own.min(self.max_wall_clock))
    }

    pub async fn interface(&self, target: &CallTarget) -> Result<LockedCall, TargetError> {
        let (repo, workflow, git_ref, outputs) = github_target(target)?;
        let api_repo = api_repo(repo)?;
        let token = self.token(&api_repo).await?;
        let commit = self
            .client
            .resolve(&token, &api_repo, git_ref.as_str())
            .await?;
        let interface = self
            .interface_at(&token, &api_repo, workflow, &commit, outputs)
            .await?;
        let pin = Pin::GithubRef {
            repo: repo.clone(),
            workflow: workflow.clone(),
            git_ref: git_ref.clone(),
            commit: commit.to_string(),
        };
        LockedCall::new(pin, interface)
            .map_err(|e| TargetError::Refused(format!("hashing the interface: {e}")))
    }

    pub async fn dispatch(
        &self,
        target: &CallTarget,
        locked: &LockedCall,
        values: &BTreeMap<String, Value>,
        files: &BTreeMap<String, FileRef>,
    ) -> Result<GithubDispatched, TargetError> {
        let (repo, workflow, git_ref, outputs) = github_target(target)?;
        if !locked.pin.pins(target) {
            return Err(TargetError::Refused(format!(
                "calls.lock pins a different target than {repo} {} at {}",
                workflow.as_str(),
                git_ref.as_str()
            )));
        }
        let api_repo = api_repo(repo)?;
        let token = self.token(&api_repo).await?;
        let commit = self
            .client
            .resolve(&token, &api_repo, git_ref.as_str())
            .await?;
        let current = self
            .interface_at(&token, &api_repo, workflow, &commit, outputs)
            .await?;
        if let Some(moved) = crate::calls::interface_moved(&locked.interface, &current) {
            return Err(TargetError::Refused(format!(
                "{repo} {} at {} (commit {}) {moved} than calls.lock records",
                workflow.as_str(),
                git_ref.as_str(),
                commit.short()
            )));
        }
        let inputs = dispatch_inputs(&locked.interface.params, values, files)?;
        let created = self
            .client
            .dispatch(
                &token,
                &api_repo,
                workflow.file_name(),
                git_ref.as_str(),
                &inputs,
            )
            .await?;
        Ok(GithubDispatched {
            handle: TargetHandle::GithubWorkflow {
                repo: repo.clone(),
                run_id: created.run_id,
            },
            link: ExternalUrl::new(created.html_url)
                .ok()
                .map(|url| NodeLink::External { url }),
            commit: commit.to_string(),
        })
    }

    pub async fn moved_head(
        &self,
        target: &CallTarget,
        locked: &TargetInterface,
        commit: &str,
        head: &str,
    ) -> Result<Option<String>, TargetError> {
        if head == commit {
            return Ok(None);
        }
        let (repo, workflow, git_ref, outputs) = github_target(target)?;
        let Some(head_sha) = CommitSha::parse(head) else {
            return Ok(Some(format!("ran on an unreadable head commit {head:?}")));
        };
        let api_repo = api_repo(repo)?;
        let token = self.token(&api_repo).await?;
        let at_head = self
            .interface_at(&token, &api_repo, workflow, &head_sha, outputs)
            .await?;
        Ok(
            crate::calls::interface_moved(locked, &at_head).map(|moved| {
                format!(
                    "{} moved to commit {} during dispatch, whose workflow {moved} than calls.lock \
                 records",
                    git_ref.as_str(),
                    head_sha.short()
                )
            }),
        )
    }

    pub async fn authorize(
        &self,
        who: &Principal,
        repo: &GithubRepo,
        linked: Option<&str>,
    ) -> Result<(), TargetError> {
        let TokenSource::App { app, logins } = &self.tokens else {
            return Ok(());
        };
        let api_repo = api_repo(repo)?;
        let owner = GithubOwner::parse(api_repo.owner())
            .map_err(|e| TargetError::Refused(e.to_string()))?;
        let Principal::User(user) = who else {
            return Err(TargetError::Refused(format!(
                "{who} is not a person with a GitHub login, so it may not dispatch the workflows \
                 of {repo}"
            )));
        };
        let mut paired = logins.github_logins(user);
        if let Some(linked) = linked
            && !paired.iter().any(|p| p.eq_ignore_ascii_case(linked))
        {
            paired.push(linked);
        }
        if paired.is_empty() {
            return Err(TargetError::Refused(format!(
                "{who} has no linked GitHub login (sign in to GitHub in the desktop app, or \
                 CONTROLLER_GITHUB_LOGINS pairs one), so it may not dispatch the workflows of \
                 {repo}"
            )));
        }
        let mut held = Vec::with_capacity(paired.len());
        for login in paired {
            let login =
                GithubOwner::parse(login).map_err(|e| TargetError::Refused(e.to_string()))?;
            let role = app
                .repo_role(&owner, api_repo.name(), &login, CALL_PERMISSIONS)
                .await
                .map_err(app_failure)?;
            if role.may_write() {
                return Ok(());
            }
            held.push(format!("{login} has {}", role.as_str()));
        }
        Err(TargetError::Refused(format!(
            "{who} may not dispatch the workflows of {repo}: GitHub login {}, and dispatching \
             needs write, maintain or admin",
            held.join(", ")
        )))
    }

    pub async fn poll(
        &self,
        repo: &GithubRepo,
        run_id: u64,
        graph: &ProgressGraph,
        outputs: &GithubOutputs,
        caller_cancelled: bool,
    ) -> Result<RunPoll, TargetError> {
        let api_repo = api_repo(repo)?;
        let token = self.token(&api_repo).await?;
        let run = self.client.run(&token, &api_repo, run_id).await?;
        let jobs = self.client.jobs(&token, &api_repo, run_id).await?;
        let progress = run_graph(graph, &jobs);
        let outcome = match run.status {
            Some(RunStatus::Completed) => Some(
                self.outcome(&token, &api_repo, &run, outputs, caller_cancelled)
                    .await?,
            ),
            _ => None,
        };
        Ok(RunPoll {
            progress,
            run_number: run.run_number,
            head_sha: run.head_sha,
            outcome,
        })
    }

    pub async fn node_log(
        &self,
        repo: &GithubRepo,
        run_id: u64,
        graph: &ProgressGraph,
        node: &str,
    ) -> Result<Option<String>, TargetError> {
        let api_repo = api_repo(repo)?;
        let token = self.token(&api_repo).await?;
        let jobs = self.client.jobs(&token, &api_repo, run_id).await?;
        let Some(job) = job_of(graph, &jobs, node) else {
            return Ok(None);
        };
        Ok(Some(self.client.job_log(&token, &api_repo, job).await?))
    }

    pub async fn cancel(&self, repo: &GithubRepo, run_id: u64) -> Result<(), TargetError> {
        let api_repo = api_repo(repo)?;
        let token = self.token(&api_repo).await?;
        Ok(self.client.cancel(&token, &api_repo, run_id).await?)
    }

    async fn outcome(
        &self,
        token: &Token,
        repo: &Repo,
        run: &WorkflowRun,
        outputs: &GithubOutputs,
        caller_cancelled: bool,
    ) -> Result<GithubOutcome, TargetError> {
        let failed = |note: String| GithubOutcome {
            status: CallStatus::Failed,
            note,
            result: Value::Null,
            files: Vec::new(),
        };
        match settle(run.conclusion, caller_cancelled) {
            Settle::Fail(note) => Ok(failed(note)),
            Settle::Cancelled => Ok(GithubOutcome {
                status: CallStatus::Cancelled,
                note: "the call was cancelled".to_owned(),
                result: Value::Null,
                files: Vec::new(),
            }),
            Settle::Pass { read_result: false } => Ok(GithubOutcome {
                status: CallStatus::Passed,
                note: String::new(),
                result: json!({}),
                files: Vec::new(),
            }),
            Settle::Pass { read_result: true } => {
                let Some(zip) = self
                    .client
                    .artifact(token, repo, run.id, &outputs.artifact)
                    .await?
                else {
                    return Ok(failed(format!(
                        "the run uploaded no artifact {:?}",
                        outputs.artifact
                    )));
                };
                Ok(match read_result(&zip, outputs) {
                    Ok((result, files)) => GithubOutcome {
                        status: CallStatus::Passed,
                        note: String::new(),
                        result,
                        files,
                    },
                    Err(note) => failed(note),
                })
            }
        }
    }

    async fn token(&self, repo: &Repo) -> Result<Token, TargetError> {
        match &self.tokens {
            TokenSource::Person(signed_in) => match signed_in.token(
                crate::identity::forge::Forge::Github,
                &crate::identity::forge::ForgeHost::github(),
            ) {
                Some(token) => Ok(Token::new(token)),
                None => gh_cli_token().await,
            },
            TokenSource::App { app, .. } => {
                let owner = GithubOwner::parse(repo.owner())
                    .map_err(|e| TargetError::Refused(e.to_string()))?;
                app.repo_token(&owner, repo.name(), CALL_PERMISSIONS)
                    .await
                    .map(Token::new)
                    .map_err(app_failure)
            }
        }
    }

    async fn interface_at(
        &self,
        token: &Token,
        repo: &Repo,
        workflow: &WorkflowPath,
        commit: &CommitSha,
        outputs: &GithubOutputs,
    ) -> Result<TargetInterface, TargetError> {
        let text = self
            .client
            .workflow_file(token, repo, workflow.as_str(), commit.as_str())
            .await?;
        let parsed = Workflow::parse(&text).map_err(|error| GithubError::Workflow {
            repo: repo.to_string(),
            workflow: workflow.as_str().to_owned(),
            git_ref: commit.short().to_owned(),
            error: Box::new(error),
        })?;
        Ok(interface(&parsed, outputs))
    }
}

fn app_failure(e: anyhow::Error) -> TargetError {
    match e.downcast_ref::<RepoDenied>() {
        Some(denied) => TargetError::Denied(denied.clone()),
        None => TargetError::AppToken(e),
    }
}

fn api_repo(repo: &GithubRepo) -> Result<Repo, TargetError> {
    Ok(Repo::parse(repo.as_str())?)
}

fn github_target(
    target: &CallTarget,
) -> Result<(&GithubRepo, &WorkflowPath, &GitRefName, &GithubOutputs), TargetError> {
    match target {
        CallTarget::GithubWorkflow {
            repo,
            workflow,
            git_ref,
            outputs,
        } => Ok((repo, workflow, git_ref, outputs)),
        other => Err(TargetError::WrongKind(other.kind().to_string())),
    }
}

async fn gh_cli_token() -> Result<Token, TargetError> {
    let run = tokio::process::Command::new("gh")
        .args(["auth", "token", "--hostname", "github.com"])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(GH_CLI_TIMEOUT, run)
        .await
        .map_err(|_| TargetError::GhCli(format!("no answer in {GH_CLI_TIMEOUT:?}")))?
        .map_err(|e| TargetError::GhCli(e.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(TargetError::GhCli(
            stderr.trim().chars().take(ERR_BODY_CHARS).collect(),
        ));
    }
    let token = String::from_utf8(out.stdout)
        .map_err(|_| TargetError::GhCli("the token is not UTF-8".to_owned()))?;
    let token = token.trim();
    match token.is_empty() || token.contains(char::is_whitespace) {
        true => Err(TargetError::GhCli(
            "gh printed no token; run `gh auth login`".to_owned(),
        )),
        false => Ok(Token::new(token)),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use crucible_contract::calls::{
        CallBudget, CallStatus, CallTarget, GithubRepo, ProgressGraph, TargetHandle,
    };
    use serde_json::json;

    use crate::authz::model::Principal;
    use crate::calls::github::{GithubWorkflowBackend, TargetError, TokenSource};
    use crate::secrets::github_app::RepoDenied;

    fn backend() -> GithubWorkflowBackend {
        GithubWorkflowBackend::new(
            TokenSource::Person(Default::default()),
            Duration::from_secs(4 * 3600),
        )
        .unwrap()
    }

    #[test]
    fn a_call_without_its_own_wall_clock_gets_the_controllers() {
        let backend = backend();
        let budget = |secs| {
            Some(CallBudget {
                cost_usd: None,
                wall_clock_secs: secs,
            })
        };
        assert_eq!(backend.wall_clock(None), Duration::from_secs(4 * 3600));
        assert_eq!(
            backend.wall_clock(budget(None)),
            Duration::from_secs(4 * 3600)
        );
        assert_eq!(
            backend.wall_clock(budget(Some(0))),
            Duration::from_secs(4 * 3600)
        );
        assert_eq!(
            backend.wall_clock(budget(Some(60))),
            Duration::from_secs(60)
        );
        assert_eq!(
            backend.wall_clock(budget(Some(10 * 3600))),
            Duration::from_secs(4 * 3600),
            "a call's own wall clock stays within the controller's"
        );
    }

    #[test]
    fn denials_are_final_and_token_faults_are_retried() {
        assert!(!TargetError::Denied(RepoDenied::NotInstalled("o/r".into())).is_transient());
        assert!(
            TargetError::AppToken(anyhow::Error::new(std::io::Error::other("connect refused")))
                .is_transient()
        );
        assert!(TargetError::GhCli("timeout".into()).is_transient());
        assert!(!TargetError::Refused("no".into()).is_transient());
    }

    fn app_backend(server: &wiremock::MockServer, dir: &std::path::Path) -> GithubWorkflowBackend {
        let app = crate::secrets::github_app::GithubAppTokenSource::new(
            "1",
            "2",
            crate::secrets::github_app::testkey::write_test_key(dir),
            server.uri(),
        );
        let logins = crate::identity::github_logins::GithubLogins::parse([
            "wren-gh=wren",
            "ash-gh=ash",
            "ash-alt=ash",
        ]);
        GithubWorkflowBackend::new(
            TokenSource::App {
                app: Box::new(app),
                logins,
            },
            Duration::from_secs(3600),
        )
        .unwrap()
    }

    async fn mount_repo(server: &wiremock::MockServer, roles: &[(&str, &str)]) {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/installation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 7,
                "permissions": {"actions": "write", "contents": "read"},
            })))
            .mount(server)
            .await;
        let in_an_hour = jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() + 3600)
            .unwrap()
            .to_string();
        Mock::given(method("POST"))
            .and(path("/app/installations/7/access_tokens"))
            .respond_with(
                ResponseTemplate::new(201)
                    .set_body_json(json!({"token": "ghs_repo", "expires_at": in_an_hour})),
            )
            .mount(server)
            .await;
        for (login, role) in roles {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/repos/acme/app/collaborators/{login}/permission"
                )))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"permission": role, "role_name": role})),
                )
                .mount(server)
                .await;
        }
    }

    fn user(login: &str) -> Principal {
        Principal::User(login.into())
    }

    #[tokio::test]
    async fn an_app_dispatch_needs_an_owner_with_write_on_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        mount_repo(
            &server,
            &[
                ("wren-gh", "write"),
                ("ash-gh", "read"),
                ("ash-alt", "triage"),
                ("mal-gh", "maintain"),
            ],
        )
        .await;
        let backend = app_backend(&server, dir.path());
        let repo = GithubRepo::parse("acme/app").unwrap();

        backend.authorize(&user("wren"), &repo, None).await.unwrap();
        backend.authorize(&user("WREN"), &repo, None).await.unwrap();

        let refused = |e: TargetError| match e {
            TargetError::Refused(note) => note,
            other => panic!("not a refusal: {other}"),
        };
        let ash = refused(
            backend
                .authorize(&user("ash"), &repo, None)
                .await
                .unwrap_err(),
        );
        assert!(ash.contains("ash-alt has triage"), "{ash}");
        assert!(ash.contains("ash-gh has read"), "{ash}");
        assert!(ash.contains("needs write, maintain or admin"), "{ash}");

        let mallory = refused(
            backend
                .authorize(&user("mallory"), &repo, None)
                .await
                .unwrap_err(),
        );
        assert!(mallory.contains("no linked GitHub login"), "{mallory}");
        assert!(mallory.contains("CONTROLLER_GITHUB_LOGINS"), "{mallory}");
        backend
            .authorize(&user("mallory"), &repo, Some("mal-gh"))
            .await
            .unwrap();
        let ash_linked = refused(
            backend
                .authorize(&user("ash"), &repo, Some("ASH-GH"))
                .await
                .unwrap_err(),
        );
        assert_eq!(
            ash_linked.matches("ash-gh has read").count(),
            1,
            "{ash_linked}"
        );

        let team = Principal::Team(crate::authz::model::TeamSlug::parse("ops").unwrap());
        let team = refused(backend.authorize(&team, &repo, None).await.unwrap_err());
        assert!(team.contains("not a person"), "{team}");
    }

    #[tokio::test]
    async fn an_app_dispatch_to_a_repository_without_the_app_is_denied() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, ResponseTemplate};
        let dir = tempfile::tempdir().unwrap();
        let server = wiremock::MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/app/installation"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let backend = app_backend(&server, dir.path());
        let denied = backend
            .authorize(&user("wren"), &GithubRepo::parse("acme/app").unwrap(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(&denied, TargetError::Denied(RepoDenied::NotInstalled(r)) if r == "acme/app"),
            "{denied}"
        );
        assert!(!denied.is_transient());
    }

    #[tokio::test]
    async fn the_gh_cli_token_is_its_users_own_access() {
        let team = Principal::Team(crate::authz::model::TeamSlug::parse("ops").unwrap());
        backend()
            .authorize(&team, &GithubRepo::parse("acme/app").unwrap(), None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_run_on_the_checked_commit_has_not_moved() {
        let target: CallTarget = serde_json::from_value(json!({
            "kind": "github_workflow",
            "repo": "acme/app",
            "workflow": ".github/workflows/x.yml",
            "ref": "main",
            "outputs": {"artifact": "result"},
        }))
        .unwrap();
        let locked = crucible_contract::calls::TargetInterface {
            params: json!({}),
            result: vec![],
            result_files: vec![],
            graph: ProgressGraph::default(),
        };
        let sha = "a".repeat(40);
        assert_eq!(
            backend()
                .moved_head(&target, &locked, &sha, &sha)
                .await
                .unwrap(),
            None
        );
        let unreadable = backend()
            .moved_head(&target, &locked, &sha, "not-a-sha")
            .await
            .unwrap();
        assert!(
            unreadable.is_some_and(|n| n.contains("unreadable head")),
            "a head that is no commit has moved"
        );
    }

    #[tokio::test]
    async fn a_target_of_another_kind_is_refused() {
        let playbook: CallTarget =
            serde_json::from_value(json!({"kind": "playbook", "playbook": "triage"})).unwrap();
        assert!(matches!(
            backend().interface(&playbook).await,
            Err(TargetError::WrongKind(kind)) if kind == "playbook"
        ));
    }

    #[tokio::test]
    #[ignore = "dispatches a real workflow; set CRUCIBLE_GH_LIVE=1"]
    async fn live_dispatch_of_the_demo_workflow() {
        if std::env::var("CRUCIBLE_GH_LIVE").as_deref() != Ok("1") {
            return;
        }
        let target: CallTarget = serde_json::from_value(json!({
            "kind": "github_workflow",
            "repo": "wseaton/snapshotd",
            "workflow": ".github/workflows/crucible-call-demo.yml",
            "ref": "crucible-call-demo",
            "outputs": {
                "artifact": "crucible-result",
                "fields": {"summary": "string", "flavor": "string", "loud": "boolean", "legs": "integer", "payload_keys": "list"},
                "files": ["notes.md"],
            },
        }))
        .unwrap();
        let CallTarget::GithubWorkflow { outputs, .. } = &target else {
            unreachable!()
        };
        let backend = backend();
        let locked = backend.interface(&target).await.unwrap();
        assert_eq!(
            locked.interface.params["required"],
            json!(["message"]),
            "{}",
            locked.interface.params
        );
        let ids: Vec<&str> = locked
            .interface
            .graph
            .nodes
            .iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(ids, ["prepare", "build", "report"]);

        let values = BTreeMap::from([
            ("message".to_owned(), json!("hello from crucible")),
            ("flavor".to_owned(), json!("thorough")),
            ("loud".to_owned(), json!(true)),
            ("payload".to_owned(), json!({"alpha": 1, "beta": [2]})),
        ]);
        let dispatched = backend
            .dispatch(&target, &locked, &values, &BTreeMap::new())
            .await
            .unwrap();
        let TargetHandle::GithubWorkflow { repo, run_id } = &dispatched.handle else {
            panic!("not a github handle");
        };
        assert_eq!(repo, &GithubRepo::parse("wseaton/snapshotd").unwrap());
        eprintln!("dispatched run {run_id} at {}", dispatched.commit);

        let mut seen_legs = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(900);
        let outcome = loop {
            let poll = backend
                .poll(repo, *run_id, &locked.interface.graph, outputs, false)
                .await
                .unwrap();
            seen_legs |= poll
                .progress
                .nodes
                .iter()
                .filter(|n| n.group.as_deref() == Some("build"))
                .count()
                == 2;
            if let Some(outcome) = poll.outcome {
                break outcome;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the run did not finish"
            );
            tokio::time::sleep(Duration::from_secs(10)).await;
        };
        assert!(seen_legs, "the matrix legs were never drawn as a group");
        assert_eq!(outcome.status, CallStatus::Passed, "{}", outcome.note);
        assert_eq!(
            outcome.result,
            json!({
                "summary": "THOROUGH BUILD OF 'HELLO FROM CRUCIBLE' OVER 2 LEGS",
                "flavor": "thorough",
                "loud": true,
                "legs": 2,
                "payload_keys": ["alpha", "beta"],
            })
        );
        assert_eq!(outcome.files.len(), 1);
        assert!(String::from_utf8_lossy(&outcome.files[0].bytes).contains("hello from crucible"));

        let log = backend
            .node_log(repo, *run_id, &locked.interface.graph, "prepare")
            .await
            .unwrap()
            .expect("the prepare job's log");
        assert!(!log.is_empty());
        assert!(
            backend
                .node_log(repo, *run_id, &locked.interface.graph, "ghost")
                .await
                .unwrap()
                .is_none()
        );

        let unused = ProgressGraph::default();
        let again = backend
            .poll(repo, *run_id, &unused, outputs, false)
            .await
            .unwrap();
        assert!(again.outcome.is_some());
        backend.cancel(repo, *run_id).await.unwrap();
    }
}
