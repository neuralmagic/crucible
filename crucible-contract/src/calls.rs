//! Playbook calls: the manifest's `[calls]` entries, the `calls.lock` pack file, a
//! target's interface, the progress graph every target kind reports, and the call API the engine
//! reaches with its pod token. The engine, the controller and the desktop round-trip these types.

use std::collections::{BTreeMap, BTreeSet};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::emits::{DeclaredFile, EmitWire, FieldType};
use crate::link::ExternalUrl;
use crate::session::TaskStatus;

pub const LOCK_FILE: &str = "calls.lock";

pub const LOCK_VERSION: u32 = 1;

pub const MAX_CALL_FANOUT: u32 = 64;

pub const MAX_PROGRESS_NODES: usize = 512;

pub const MAX_CALL_DEPTH: u32 = 4;

pub const MAX_JSON_PARAM_BYTES: usize = 64 * 1024;

pub const MAX_FILE_PARAM_BYTES: u64 = 16 * 1024 * 1024;

pub const MAX_GITHUB_INPUTS: usize = 25;

pub const GITHUB_RESULT_FILE: &str = "result.json";

pub const GITLAB_RESULT_FILE: &str = "result.json";

pub const CALLS_PATH: &str = "/api/runs/{run}/calls";

pub const CALL_PATH: &str = "/api/runs/{run}/calls/{call}";

pub const CALL_CANCEL_PATH: &str = "/api/runs/{run}/calls/{call}/cancel";

pub const CALL_RESULT_FILE_PATH: &str = "/api/runs/{run}/calls/{call}/files/{*path}";

pub const CALL_INPUT_FILE_PATH: &str = "/api/runs/{run}/call-files/{digest}";

pub const CALL_PROGRESS_PATH: &str = "/api/runs/{run}/call-progress";

pub const CALL_NODE_LOG_PATH: &str = "/api/runs/{run}/calls/{call}/nodes/{node}/log";

pub const PARAM_FILE_PATH: &str = "/api/runs/{run}/param-files/{digest}";

/// The kinds of target a call may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum CallKind {
    Playbook,
    GithubWorkflow,
    GitlabPipeline,
    Tekton,
}

impl CallKind {
    pub const ALL: [CallKind; 4] = [
        CallKind::Playbook,
        CallKind::GithubWorkflow,
        CallKind::GitlabPipeline,
        CallKind::Tekton,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            CallKind::Playbook => "playbook",
            CallKind::GithubWorkflow => "github_workflow",
            CallKind::GitlabPipeline => "gitlab_pipeline",
            CallKind::Tekton => "tekton",
        }
    }

    pub fn presentation(self) -> Presentation {
        match self {
            CallKind::Playbook => Presentation {
                name: "Crucible playbook",
                badge: "playbook",
                open: "Open run",
                accent: None,
                icon: None,
                nodes: NodeLook::Tasks,
            },
            CallKind::GithubWorkflow => Presentation {
                name: "GitHub Actions",
                badge: "github",
                open: "Open on GitHub",
                accent: Some(Rgb(0x82, 0x50, 0xdf)),
                icon: Some(BackendIcon::Github),
                nodes: NodeLook::Jobs,
            },
            CallKind::GitlabPipeline => Presentation {
                name: "GitLab CI",
                badge: "gitlab",
                open: "Open on GitLab",
                accent: Some(Rgb(0xfc, 0x6d, 0x26)),
                icon: Some(BackendIcon::Gitlab),
                nodes: NodeLook::Jobs,
            },
            CallKind::Tekton => Presentation {
                name: "Tekton",
                badge: "tekton",
                open: "Open run page",
                accent: Some(Rgb(0x1f, 0x6f, 0xc5)),
                icon: Some(BackendIcon::Tekton),
                nodes: NodeLook::Jobs,
            },
        }
    }
}

/// How a client draws a target kind's runs: its name, its colour, its mark, and its nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Presentation {
    pub name: &'static str,
    pub badge: &'static str,
    pub open: &'static str,
    pub accent: Option<Rgb>,
    pub icon: Option<BackendIcon>,
    pub nodes: NodeLook,
}

/// An sRGB colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// A backend's mark, which a client maps to its own icon set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendIcon {
    Github,
    Gitlab,
    Tekton,
}

/// How a target graph's nodes are drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeLook {
    Tasks,
    Jobs,
}

impl std::fmt::Display for CallKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a string is not one of this module's names.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error(
        "{0:?} is not a call alias: start with a lowercase letter, then lowercase letters, digits, \
         '_' or '-', at most 63 characters"
    )]
    Alias(String),
    #[error("{0:?} is not a GitHub repository: write owner/name")]
    Repo(String),
    #[error("{0:?} is not a workflow file: a .yml or .yaml path under .github/workflows/")]
    Workflow(String),
    #[error("{0:?} is not a playbook: name a registered playbook, or draft:<id>")]
    Playbook(String),
    #[error("a git ref must not be empty")]
    EmptyRef,
}

/// A `[calls.<alias>]` key: how a `call()` names its target.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CallAlias(String);

impl CallAlias {
    pub fn parse(raw: &str) -> Result<Self, NameError> {
        let mut chars = raw.chars();
        let ok = raw.len() <= 63
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        match ok {
            true => Ok(Self(raw.to_owned())),
            false => Err(NameError::Alias(raw.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for CallAlias {
    type Error = NameError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<CallAlias> for String {
    fn from(alias: CallAlias) -> Self {
        alias.0
    }
}

impl std::fmt::Display for CallAlias {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A GitHub repository, `owner/name`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GithubRepo(String);

impl GithubRepo {
    pub fn parse(raw: &str) -> Result<Self, NameError> {
        let segment = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        match raw.split_once('/') {
            Some((owner, name)) if segment(owner) && segment(name) => Ok(Self(raw.to_owned())),
            _ => Err(NameError::Repo(raw.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn owner(&self) -> &str {
        self.0.split_once('/').map_or("", |(owner, _)| owner)
    }

    pub fn name(&self) -> &str {
        self.0.split_once('/').map_or("", |(_, name)| name)
    }
}

impl TryFrom<String> for GithubRepo {
    type Error = NameError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<GithubRepo> for String {
    fn from(repo: GithubRepo) -> Self {
        repo.0
    }
}

impl std::fmt::Display for GithubRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A workflow file path in its repository, under `.github/workflows/`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WorkflowPath(String);

impl WorkflowPath {
    pub fn parse(raw: &str) -> Result<Self, NameError> {
        let file = raw.strip_prefix(".github/workflows/");
        let ok = file.is_some_and(|file| {
            !file.is_empty()
                && !file.contains('/')
                && (file.ends_with(".yml") || file.ends_with(".yaml"))
        });
        match ok {
            true => Ok(Self(raw.to_owned())),
            false => Err(NameError::Workflow(raw.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }
}

impl TryFrom<String> for WorkflowPath {
    type Error = NameError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<WorkflowPath> for String {
    fn from(path: WorkflowPath) -> Self {
        path.0
    }
}

/// A branch or tag name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct GitRefName(String);

impl GitRefName {
    pub fn parse(raw: &str) -> Result<Self, NameError> {
        match raw.trim().is_empty() {
            true => Err(NameError::EmptyRef),
            false => Ok(Self(raw.to_owned())),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for GitRefName {
    type Error = NameError;
    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Self::parse(&raw)
    }
}

impl From<GitRefName> for String {
    fn from(name: GitRefName) -> Self {
        name.0
    }
}

/// A playbook a `[calls]` entry names: registered by name, or a draft by id (`draft:<id>`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlaybookRef {
    Registered(String),
    Draft(String),
}

const DRAFT_PREFIX: &str = "draft:";

impl PlaybookRef {
    pub fn parse(raw: &str) -> Result<Self, NameError> {
        let word = |s: &str| !s.is_empty() && !s.chars().any(char::is_whitespace);
        match raw.strip_prefix(DRAFT_PREFIX) {
            Some(id) if word(id) => Ok(PlaybookRef::Draft(id.to_owned())),
            None if word(raw) => Ok(PlaybookRef::Registered(raw.to_owned())),
            _ => Err(NameError::Playbook(raw.to_owned())),
        }
    }
}

impl std::fmt::Display for PlaybookRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlaybookRef::Registered(name) => f.write_str(name),
            PlaybookRef::Draft(id) => write!(f, "{DRAFT_PREFIX}{id}"),
        }
    }
}

impl Serialize for PlaybookRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PlaybookRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        PlaybookRef::parse(&raw).map_err(de::Error::custom)
    }
}

/// What a GitHub workflow returns: the artifact it uploads, the typed fields of the JSON object
/// at [`GITHUB_RESULT_FILE`] in it, and further file paths in it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubOutputs {
    pub artifact: String,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldType>,
    #[serde(default)]
    pub files: Vec<String>,
}

/// What a GitLab pipeline returns: the job whose artifacts hold the typed fields of the JSON
/// object at [`GITLAB_RESULT_FILE`], and further file paths among them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitlabOutputs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldType>,
    #[serde(default)]
    pub files: Vec<String>,
}

/// One `[calls.<alias>]` entry of a manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CallTarget {
    Playbook {
        playbook: PlaybookRef,
    },
    GithubWorkflow {
        repo: GithubRepo,
        workflow: WorkflowPath,
        #[serde(rename = "ref")]
        git_ref: GitRefName,
        outputs: GithubOutputs,
    },
    GitlabPipeline {
        host: String,
        project: String,
        #[serde(rename = "ref")]
        git_ref: GitRefName,
        #[serde(default)]
        outputs: GitlabOutputs,
    },
    Tekton {
        cluster: String,
        namespace: String,
        repository: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event: Option<PacEvent>,
        #[serde(default)]
        expected: Vec<TektonExpected>,
    },
}

/// Which Pipelines-as-Code events a Tekton watch follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PacEvent {
    Push,
    PullRequest,
}

/// One run a Tekton watch waits for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TektonExpected {
    pub name: String,
    #[serde(default)]
    pub results: Vec<String>,
}

impl TektonExpected {
    pub fn field(&self) -> String {
        self.name.replace('-', "_")
    }
}

pub const WATCH_COMMIT_PARAM: &str = "commit";
pub const WATCH_APPEAR_PARAM: &str = "appear";
pub const WATCH_DEFAULT_APPEAR: &str = "10m";

impl CallTarget {
    pub fn kind(&self) -> CallKind {
        match self {
            CallTarget::Playbook { .. } => CallKind::Playbook,
            CallTarget::GithubWorkflow { .. } => CallKind::GithubWorkflow,
            CallTarget::GitlabPipeline { .. } => CallKind::GitlabPipeline,
            CallTarget::Tekton { .. } => CallKind::Tekton,
        }
    }

    pub fn watches(&self) -> bool {
        matches!(self, CallTarget::Tekton { .. })
    }
}

/// What a lock entry pins its target to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Pin {
    Revision {
        playbook: String,
        digest: String,
    },
    DraftHead {
        draft: String,
    },
    GithubRef {
        repo: GithubRepo,
        workflow: WorkflowPath,
        #[serde(rename = "ref")]
        git_ref: GitRefName,
        commit: String,
    },
    GitlabRef {
        host: String,
        project: String,
        #[serde(rename = "ref")]
        git_ref: GitRefName,
        commit: String,
    },
    TektonWatch,
}

impl Pin {
    pub fn kind(&self) -> CallKind {
        match self {
            Pin::Revision { .. } | Pin::DraftHead { .. } => CallKind::Playbook,
            Pin::GithubRef { .. } => CallKind::GithubWorkflow,
            Pin::GitlabRef { .. } => CallKind::GitlabPipeline,
            Pin::TektonWatch => CallKind::Tekton,
        }
    }

    pub fn moves(&self) -> bool {
        matches!(
            self,
            Pin::DraftHead { .. } | Pin::GithubRef { .. } | Pin::GitlabRef { .. }
        )
    }

    pub fn pins(&self, target: &CallTarget) -> bool {
        match (self, target) {
            (
                Pin::Revision { playbook, .. },
                CallTarget::Playbook {
                    playbook: PlaybookRef::Registered(name),
                },
            ) => playbook == name,
            (
                Pin::DraftHead { draft },
                CallTarget::Playbook {
                    playbook: PlaybookRef::Draft(id),
                },
            ) => draft == id,
            (
                Pin::GithubRef {
                    repo,
                    workflow,
                    git_ref,
                    ..
                },
                CallTarget::GithubWorkflow {
                    repo: r,
                    workflow: w,
                    git_ref: g,
                    ..
                },
            ) => repo == r && workflow == w && git_ref == g,
            (
                Pin::GitlabRef {
                    host,
                    project,
                    git_ref,
                    ..
                },
                CallTarget::GitlabPipeline {
                    host: h,
                    project: p,
                    git_ref: g,
                    ..
                },
            ) => host == h && project == p && git_ref == g,
            (Pin::TektonWatch, CallTarget::Tekton { .. }) => true,
            _ => false,
        }
    }
}

/// What a target takes and returns, and the graph it runs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetInterface {
    pub params: Value,
    #[serde(default)]
    pub result: Vec<EmitWire>,
    #[serde(default)]
    pub result_files: Vec<DeclaredFile>,
    #[serde(default)]
    pub graph: ProgressGraph,
}

impl TargetInterface {
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        let value = serde_json::to_value(self)?;
        let bytes = serde_json::to_vec(&sorted(value))?;
        Ok(format!("sha256:{:x}", Sha256::digest(&bytes)))
    }
}

fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let ordered: BTreeMap<String, Value> =
                map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            Value::Object(ordered.into_iter().collect())
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// One alias of a [`CallsLock`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedCall {
    pub kind: CallKind,
    pub pin: Pin,
    pub interface: TargetInterface,
    pub interface_digest: String,
}

impl LockedCall {
    pub fn new(pin: Pin, interface: TargetInterface) -> Result<Self, serde_json::Error> {
        Ok(LockedCall {
            kind: pin.kind(),
            interface_digest: interface.digest()?,
            pin,
            interface,
        })
    }
}

/// `calls.lock`: every alias's pin and interface, as JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallsLock {
    pub version: u32,
    pub calls: BTreeMap<CallAlias, LockedCall>,
}

/// Why a lock is refused.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("reading {LOCK_FILE}: {0}")]
    Read(#[from] std::io::Error),
    #[error("{LOCK_FILE} is not a lock: {0}")]
    Malformed(#[from] serde_json::Error),
    #[error("{LOCK_FILE} is version {got}; this build reads version {LOCK_VERSION}")]
    Version { got: u32 },
    #[error("{LOCK_FILE} entry {alias} records kind {kind} but pins a {pinned} target")]
    KindMismatch {
        alias: CallAlias,
        kind: CallKind,
        pinned: CallKind,
    },
    #[error("{LOCK_FILE} entry {alias} records an interface digest that does not match it")]
    Digest { alias: CallAlias },
    #[error("the manifest declares [calls] but the pack has no {LOCK_FILE}")]
    Missing,
    #[error("{LOCK_FILE} has no entry for [calls.{0}]")]
    Unlocked(CallAlias),
    #[error("{LOCK_FILE} names {0}, which [calls] does not declare")]
    Undeclared(CallAlias),
    #[error("{LOCK_FILE} entry {0} pins a different target than [calls.{0}] names")]
    Stale(CallAlias),
}

impl CallsLock {
    pub fn parse(text: &str) -> Result<Self, LockError> {
        let lock: CallsLock = serde_json::from_str(text)?;
        if lock.version != LOCK_VERSION {
            return Err(LockError::Version { got: lock.version });
        }
        for (alias, call) in &lock.calls {
            if call.kind != call.pin.kind() {
                return Err(LockError::KindMismatch {
                    alias: alias.clone(),
                    kind: call.kind,
                    pinned: call.pin.kind(),
                });
            }
            if call.interface.digest()? != call.interface_digest {
                return Err(LockError::Digest {
                    alias: alias.clone(),
                });
            }
        }
        Ok(lock)
    }

    pub fn render(&self) -> Result<String, serde_json::Error> {
        let mut text = serde_json::to_string_pretty(self)?;
        text.push('\n');
        Ok(text)
    }

    pub fn covers(&self, calls: &BTreeMap<CallAlias, CallTarget>) -> Result<(), LockError> {
        for (alias, target) in calls {
            let Some(locked) = self.calls.get(alias) else {
                return Err(LockError::Unlocked(alias.clone()));
            };
            if !locked.pin.pins(target) {
                return Err(LockError::Stale(alias.clone()));
            }
        }
        Ok(())
    }

    pub fn check(&self, calls: &BTreeMap<CallAlias, CallTarget>) -> Result<(), LockError> {
        if let Some(alias) = self.calls.keys().find(|a| !calls.contains_key(*a)) {
            return Err(LockError::Undeclared(alias.clone()));
        }
        for (alias, target) in calls {
            let Some(locked) = self.calls.get(alias) else {
                return Err(LockError::Unlocked(alias.clone()));
            };
            if !locked.pin.pins(target) {
                return Err(LockError::Stale(alias.clone()));
            }
        }
        Ok(())
    }
}

/// Where a progress node stands, in the statuses a Crucible run's graph uses. Written as one
/// token: `pending`, `running`, or a [`TaskStatus`] token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeStatus {
    Pending,
    Running,
    Settled(TaskStatus),
}

impl NodeStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            NodeStatus::Pending => "pending",
            NodeStatus::Running => "running",
            NodeStatus::Settled(status) => status.as_str(),
        }
    }

    pub fn ended(self) -> bool {
        matches!(self, NodeStatus::Settled(_))
    }
}

impl Serialize for NodeStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NodeStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        match raw.as_str() {
            "pending" => Ok(NodeStatus::Pending),
            "running" => Ok(NodeStatus::Running),
            other => other
                .parse()
                .map(NodeStatus::Settled)
                .map_err(de::Error::custom),
        }
    }
}

/// Where a progress node opens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeLink {
    Run {
        run: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        task: Option<String>,
    },
    External {
        url: ExternalUrl,
    },
}

/// One step of a GitHub job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobStep {
    pub name: String,
    pub status: NodeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
}

/// What a node carries that only its kind knows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeDetail {
    PlaybookTask { task_kind: String },
    GithubJob { steps: Vec<JobStep> },
    TektonTask { task_runs: Vec<TektonTaskRun> },
}

/// One TaskRun of a Tekton task: its steps, and the pod a step's log is read from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TektonTaskRun {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pod: Option<String>,
    pub steps: Vec<TektonStep>,
}

/// One step of a Tekton task, with the container its log is in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TektonStep {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub status: NodeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
}

/// One node of a progress graph.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressNode {
    pub id: String,
    pub label: String,
    pub status: NodeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<NodeDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<NodeLink>,
}

/// One edge, from a node to a node that depends on it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressEdge {
    pub from: String,
    pub to: String,
}

/// A target's graph or a target run's progress: the one shape every target kind reports.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgressGraph {
    pub nodes: Vec<ProgressNode>,
    #[serde(default)]
    pub edges: Vec<ProgressEdge>,
    #[serde(default)]
    pub omitted: u32,
}

impl ProgressGraph {
    pub fn bounded(mut nodes: Vec<ProgressNode>, edges: Vec<ProgressEdge>) -> Self {
        let omitted = nodes.len().saturating_sub(MAX_PROGRESS_NODES);
        nodes.truncate(MAX_PROGRESS_NODES);
        let kept: BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
        let edges = edges
            .into_iter()
            .filter(|e| kept.contains(e.from.as_str()) && kept.contains(e.to.as_str()))
            .collect();
        ProgressGraph {
            nodes,
            edges,
            omitted: u32::try_from(omitted).unwrap_or(u32::MAX),
        }
    }

    pub fn pending(&self) -> Self {
        let nodes = self
            .nodes
            .iter()
            .map(|n| ProgressNode {
                status: NodeStatus::Pending,
                started: None,
                ended: None,
                ..n.clone()
            })
            .collect();
        ProgressGraph {
            nodes,
            edges: self.edges.clone(),
            omitted: self.omitted,
        }
    }

    pub fn unlinked(&self) -> Self {
        let nodes = self
            .nodes
            .iter()
            .map(|n| ProgressNode {
                link: None,
                detail: None,
                ..n.clone()
            })
            .collect();
        ProgressGraph {
            nodes,
            edges: self.edges.clone(),
            omitted: self.omitted,
        }
    }
}

/// The controller's id for one call: one target run per run, task, instance and attempt.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CallId(pub String);

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The target run on its own system.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TargetHandle {
    Playbook {
        run: String,
    },
    GithubWorkflow {
        repo: GithubRepo,
        run_id: u64,
    },
    GitlabPipeline {
        host: String,
        project: String,
        pipeline_id: u64,
    },
    TektonWatch {
        cluster: String,
        namespace: String,
        commit: String,
    },
}

impl TargetHandle {
    pub fn kind(&self) -> CallKind {
        match self {
            TargetHandle::Playbook { .. } => CallKind::Playbook,
            TargetHandle::GithubWorkflow { .. } => CallKind::GithubWorkflow,
            TargetHandle::GitlabPipeline { .. } => CallKind::GitlabPipeline,
            TargetHandle::TektonWatch { .. } => CallKind::Tekton,
        }
    }
}

/// A file a call binds or a target returns. `path` is where it was staged, when it was.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

/// A call's own ceilings for its target run, in place of the controller's launch defaults.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_secs: Option<u64>,
}

/// `POST`ed to [`CALLS_PATH`] when a call becomes ready, and again on resume: the controller
/// answers the call it already holds for the same task, instance and attempt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DispatchRequest {
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub attempt: u32,
    pub alias: CallAlias,
    pub interface_digest: String,
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
    #[serde(default)]
    pub files: BTreeMap<String, FileRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<CallBudget>,
}

/// What a dispatch created, or found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchResponse {
    pub call: CallId,
    pub handle: TargetHandle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<NodeLink>,
}

/// Why a dispatch created no target run. The call settles failing with `note`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRefused {
    pub note: String,
}

/// Where a call's target run stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Pending,
    Running,
    Passed,
    Failed,
    Cancelled,
}

impl CallStatus {
    pub fn ended(self) -> bool {
        matches!(
            self,
            CallStatus::Passed | CallStatus::Failed | CallStatus::Cancelled
        )
    }
}

/// A call as the controller holds it, answered by every poll.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallState {
    pub call: CallId,
    pub status: CallStatus,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub result: Value,
    #[serde(default)]
    pub result_files: Vec<FileRef>,
    #[serde(default)]
    pub progress: ProgressGraph,
    pub handle: TargetHandle,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<NodeLink>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<NamedLink>,
}

/// A page of a target run's system worth opening, such as one of its runs, its commit or its
/// pull request, under the name a reader sees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedLink {
    pub label: String,
    pub url: ExternalUrl,
}

pub const LINKS_DETAIL_KEY: &str = "links";

/// One call of a run, as a reader of the run sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CallView {
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    pub attempt: u32,
    pub alias: CallAlias,
    pub kind: CallKind,
    pub state: CallState,
}

pub fn url(controller: &str, path: &str, run: &str, call: Option<&CallId>) -> String {
    let mut out = path.replace("{run}", &crate::artifact::path_segment(run));
    if let Some(call) = call {
        out = out.replace("{call}", &crate::artifact::path_segment(&call.0));
    }
    format!("{}{out}", controller.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use crate::calls::*;
    use crate::emits::{DeclaredFile, EmitWire, FieldType};
    use crate::session::TaskStatus;
    use serde_json::json;

    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de>,
    {
        let text = serde_json::to_string(value).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn node(id: &str, status: NodeStatus) -> ProgressNode {
        ProgressNode {
            id: id.into(),
            label: id.into(),
            status,
            group: None,
            started: Some("2026-11-25T10:00:00Z".into()),
            ended: None,
            detail: None,
            link: None,
        }
    }

    fn interface() -> TargetInterface {
        TargetInterface {
            params: json!({"type": "object", "properties": {"pr": {"type": "integer"}}}),
            result: vec![EmitWire {
                field: "verdict".into(),
                ty: Some(FieldType::String),
            }],
            result_files: vec![DeclaredFile::from("report.md")],
            graph: ProgressGraph {
                nodes: vec![node("build", NodeStatus::Pending)],
                edges: vec![],
                omitted: 0,
            },
        }
    }

    fn github_pin() -> Pin {
        Pin::GithubRef {
            repo: GithubRepo::parse("acme/app").unwrap(),
            workflow: WorkflowPath::parse(".github/workflows/release.yml").unwrap(),
            git_ref: GitRefName::parse("main").unwrap(),
            commit: "0123abcd".into(),
        }
    }

    #[test]
    fn names_refuse_what_they_cannot_be() {
        assert!(CallAlias::parse("release-ci_2").is_ok());
        for bad in ["", "Release", "2x", "a.b", &"a".repeat(64)] {
            assert!(CallAlias::parse(bad).is_err(), "{bad}");
        }
        let repo = GithubRepo::parse("acme/app.rs").unwrap();
        assert_eq!((repo.owner(), repo.name()), ("acme", "app.rs"));
        for bad in [
            "acme",
            "acme/",
            "/app",
            "acme/app/x",
            "acme/..",
            "ac me/app",
        ] {
            assert!(GithubRepo::parse(bad).is_err(), "{bad}");
        }
        let path = WorkflowPath::parse(".github/workflows/ci.yaml").unwrap();
        assert_eq!(path.file_name(), "ci.yaml");
        for bad in [
            "ci.yml",
            ".github/workflows/",
            ".github/workflows/a/ci.yml",
            ".github/workflows/ci.json",
        ] {
            assert!(WorkflowPath::parse(bad).is_err(), "{bad}");
        }
        assert!(GitRefName::parse(" ").is_err());
        assert_eq!(
            PlaybookRef::parse("draft:0199").unwrap(),
            PlaybookRef::Draft("0199".into())
        );
        assert_eq!(
            PlaybookRef::parse("triage").unwrap(),
            PlaybookRef::Registered("triage".into())
        );
        for bad in ["", "draft:", "two words"] {
            assert!(PlaybookRef::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn manifest_entries_read_by_kind() {
        let playbook: CallTarget =
            serde_json::from_value(json!({"kind": "playbook", "playbook": "draft:abc"})).unwrap();
        assert_eq!(playbook.kind(), CallKind::Playbook);
        assert_eq!(round_trip(&playbook), playbook);
        let github: CallTarget = serde_json::from_value(json!({
            "kind": "github_workflow",
            "repo": "acme/app",
            "workflow": ".github/workflows/release.yml",
            "ref": "main",
            "outputs": {"artifact": "result", "fields": {"version": "string"}, "files": ["notes.md"]},
        }))
        .unwrap();
        assert_eq!(github.kind(), CallKind::GithubWorkflow);
        assert_eq!(round_trip(&github), github);
        assert!(
            serde_json::from_value::<CallTarget>(json!({"kind": "gitlab_pipeline"})).is_err(),
            "an unimplemented kind is refused"
        );
        assert!(
            serde_json::from_value::<CallTarget>(
                json!({"kind": "playbook", "playbook": "x", "extra": 1})
            )
            .is_err(),
            "an unknown key is refused"
        );
    }

    #[test]
    fn lock_round_trips_and_checks_its_digest() {
        let mut calls = BTreeMap::new();
        calls.insert(
            CallAlias::parse("release").unwrap(),
            LockedCall::new(github_pin(), interface()).unwrap(),
        );
        calls.insert(
            CallAlias::parse("triage").unwrap(),
            LockedCall::new(
                Pin::Revision {
                    playbook: "triage".into(),
                    digest: "sha256:aa".into(),
                },
                interface(),
            )
            .unwrap(),
        );
        let lock = CallsLock {
            version: LOCK_VERSION,
            calls,
        };
        let text = lock.render().unwrap();
        assert_eq!(CallsLock::parse(&text).unwrap(), lock);

        let mut tampered: Value = serde_json::from_str(&text).unwrap();
        tampered["calls"]["release"]["interface"]["result"] = json!([]);
        assert!(matches!(
            CallsLock::parse(&tampered.to_string()),
            Err(LockError::Digest { .. })
        ));
        let mut wrong_kind: Value = serde_json::from_str(&text).unwrap();
        wrong_kind["calls"]["release"]["kind"] = json!("playbook");
        assert!(matches!(
            CallsLock::parse(&wrong_kind.to_string()),
            Err(LockError::KindMismatch { .. })
        ));
        let mut future: Value = serde_json::from_str(&text).unwrap();
        future["version"] = json!(2);
        assert!(matches!(
            CallsLock::parse(&future.to_string()),
            Err(LockError::Version { got: 2 })
        ));
    }

    #[test]
    fn interface_digest_ignores_key_order() {
        let mut a = interface();
        a.params = json!({"type": "object", "properties": {"a": {"type": "string"}, "b": {}}});
        let mut b = interface();
        b.params = json!({"properties": {"b": {}, "a": {"type": "string"}}, "type": "object"});
        assert_eq!(a.digest().unwrap(), b.digest().unwrap());
        b.params = json!({"type": "object"});
        assert_ne!(a.digest().unwrap(), b.digest().unwrap());
    }

    #[test]
    fn lock_check_matches_the_manifest() {
        let release = CallAlias::parse("release").unwrap();
        let lock = CallsLock {
            version: LOCK_VERSION,
            calls: BTreeMap::from([(
                release.clone(),
                LockedCall::new(github_pin(), interface()).unwrap(),
            )]),
        };
        let target = |git_ref: &str| CallTarget::GithubWorkflow {
            repo: GithubRepo::parse("acme/app").unwrap(),
            workflow: WorkflowPath::parse(".github/workflows/release.yml").unwrap(),
            git_ref: GitRefName::parse(git_ref).unwrap(),
            outputs: GithubOutputs {
                artifact: "result".into(),
                fields: BTreeMap::new(),
                files: vec![],
            },
        };
        assert!(
            lock.check(&BTreeMap::from([(release.clone(), target("main"))]))
                .is_ok()
        );
        assert!(matches!(
            lock.check(&BTreeMap::from([(release.clone(), target("v2"))])),
            Err(LockError::Stale(_))
        ));
        assert!(matches!(
            lock.check(&BTreeMap::new()),
            Err(LockError::Undeclared(_))
        ));
        let other = CallAlias::parse("other").unwrap();
        assert!(matches!(
            lock.check(&BTreeMap::from([
                (release, target("main")),
                (other, target("main"))
            ])),
            Err(LockError::Unlocked(_))
        ));
        assert!(github_pin().moves());
        assert!(
            !Pin::Revision {
                playbook: "p".into(),
                digest: "d".into()
            }
            .moves()
        );
    }

    #[test]
    fn node_status_is_one_token() {
        for (status, token) in [
            (NodeStatus::Pending, "pending"),
            (NodeStatus::Running, "running"),
            (NodeStatus::Settled(TaskStatus::Pass), "pass"),
            (NodeStatus::Settled(TaskStatus::NotTaken), "not_taken"),
        ] {
            assert_eq!(serde_json::to_value(status).unwrap(), json!(token));
            assert_eq!(round_trip(&status), status);
        }
        assert!(serde_json::from_value::<NodeStatus>(json!("queued")).is_err());
    }

    #[test]
    fn progress_graph_round_trips_with_detail_and_links() {
        let graph = ProgressGraph {
            nodes: vec![
                ProgressNode {
                    group: Some("test".into()),
                    detail: Some(NodeDetail::GithubJob {
                        steps: vec![JobStep {
                            name: "checkout".into(),
                            status: NodeStatus::Settled(TaskStatus::Pass),
                            started: None,
                            ended: Some("2026-11-25T10:01:00Z".into()),
                        }],
                    }),
                    link: Some(NodeLink::External {
                        url: ExternalUrl::new("https://github.com/acme/app/actions/runs/1/job/2")
                            .unwrap(),
                    }),
                    ..node("test (linux)", NodeStatus::Running)
                },
                ProgressNode {
                    detail: Some(NodeDetail::PlaybookTask {
                        task_kind: "agent".into(),
                    }),
                    link: Some(NodeLink::Run {
                        run: "run-1".into(),
                        task: Some("fix".into()),
                    }),
                    ..node("fix", NodeStatus::Settled(TaskStatus::Fail))
                },
            ],
            edges: vec![ProgressEdge {
                from: "test (linux)".into(),
                to: "fix".into(),
            }],
            omitted: 0,
        };
        assert_eq!(round_trip(&graph), graph);
        let unlinked = graph.unlinked();
        assert!(
            unlinked
                .nodes
                .iter()
                .all(|n| n.link.is_none() && n.detail.is_none())
        );
        let pending = graph.pending();
        assert!(
            pending
                .nodes
                .iter()
                .all(|n| n.status == NodeStatus::Pending && n.started.is_none())
        );
        assert_eq!(pending.edges, graph.edges);
    }

    #[test]
    fn progress_graph_is_bounded() {
        let nodes: Vec<ProgressNode> = (0..MAX_PROGRESS_NODES + 3)
            .map(|i| node(&format!("n{i}"), NodeStatus::Pending))
            .collect();
        let edges = vec![
            ProgressEdge {
                from: "n0".into(),
                to: "n1".into(),
            },
            ProgressEdge {
                from: "n1".into(),
                to: format!("n{}", MAX_PROGRESS_NODES + 1),
            },
        ];
        let graph = ProgressGraph::bounded(nodes, edges);
        assert_eq!(graph.nodes.len(), MAX_PROGRESS_NODES);
        assert_eq!(graph.omitted, 3);
        assert_eq!(graph.edges.len(), 1);
    }

    #[test]
    fn call_api_bodies_round_trip() {
        let request = DispatchRequest {
            task: "ship".into(),
            instance: Some("pr-7".into()),
            attempt: 1,
            alias: CallAlias::parse("release").unwrap(),
            interface_digest: "sha256:ab".into(),
            values: BTreeMap::from([
                ("version".into(), json!("1.2.0")),
                ("matrix".into(), json!({"os": ["linux"]})),
            ]),
            files: BTreeMap::from([(
                "notes".into(),
                FileRef {
                    digest: "sha256:cd".into(),
                    size: 12,
                    path: Some("notes.md".into()),
                    media_type: Some("text/markdown".into()),
                },
            )]),
            budget: Some(CallBudget {
                cost_usd: Some(2.5),
                wall_clock_secs: Some(600),
            }),
        };
        assert_eq!(round_trip(&request), request);
        let minimal: DispatchRequest = serde_json::from_value(json!({
            "task": "ship", "attempt": 1, "alias": "release", "interface_digest": "sha256:ab"
        }))
        .unwrap();
        assert!(minimal.values.is_empty() && minimal.files.is_empty() && minimal.budget.is_none());

        let handle = TargetHandle::GithubWorkflow {
            repo: GithubRepo::parse("acme/app").unwrap(),
            run_id: 1234,
        };
        assert_eq!(handle.kind(), CallKind::GithubWorkflow);
        let response = DispatchResponse {
            call: CallId("c-1".into()),
            handle: handle.clone(),
            link: Some(NodeLink::External {
                url: ExternalUrl::new("https://github.com/acme/app/actions/runs/1234").unwrap(),
            }),
        };
        assert_eq!(round_trip(&response), response);

        let state = CallState {
            run_number: Some(12),
            links: vec![NamedLink {
                label: "Commit".into(),
                url: ExternalUrl::new("https://github.com/acme/app/commit/abc").unwrap(),
            }],
            call: CallId("c-1".into()),
            status: CallStatus::Passed,
            note: String::new(),
            result: json!({"version": "1.2.0"}),
            result_files: vec![FileRef {
                digest: "sha256:ef".into(),
                size: 3,
                path: Some("notes.md".into()),
                media_type: None,
            }],
            progress: interface().graph,
            handle,
            link: None,
        };
        assert!(state.status.ended());
        assert!(!CallStatus::Running.ended());
        assert_eq!(round_trip(&state), state);
        let view = CallView {
            task: "ship".into(),
            instance: None,
            attempt: 1,
            alias: CallAlias::parse("release").unwrap(),
            kind: CallKind::GithubWorkflow,
            state,
        };
        assert_eq!(round_trip(&view), view);
        assert_eq!(round_trip(&CallRefused { note: "x".into() }).note, "x");
    }

    #[test]
    fn url_escapes_segments() {
        assert_eq!(
            url(
                "http://127.0.0.1:8870/",
                CALL_PATH,
                "run 1",
                Some(&CallId("c/1".into()))
            ),
            "http://127.0.0.1:8870/api/runs/run%201/calls/c%2F1"
        );
        assert_eq!(
            url("http://h", CALLS_PATH, "r", None),
            "http://h/api/runs/r/calls"
        );
    }

    #[test]
    fn every_kind_says_how_it_is_drawn() {
        for kind in CallKind::ALL {
            let look = kind.presentation();
            assert!(!look.name.is_empty() && !look.badge.is_empty() && !look.open.is_empty());
            if look.nodes == NodeLook::Jobs {
                assert!(
                    look.accent.is_some(),
                    "{kind:?} draws jobs in its own colour"
                );
                assert!(
                    look.icon.is_some(),
                    "{kind:?} opens its runs under its own mark"
                );
            }
        }
        assert_eq!(CallKind::Playbook.presentation().nodes, NodeLook::Tasks);
        assert_eq!(
            CallKind::GithubWorkflow.presentation().accent,
            Some(Rgb(0x82, 0x50, 0xdf))
        );
        assert_eq!(
            CallKind::GitlabPipeline.presentation().accent,
            Some(Rgb(0xfc, 0x6d, 0x26))
        );
        assert_eq!(
            CallKind::GitlabPipeline.presentation().icon,
            Some(BackendIcon::Gitlab)
        );
    }

    #[test]
    fn a_gitlab_pipeline_target_pins_and_names_its_runs() {
        let target: CallTarget = serde_json::from_value(serde_json::json!({
            "kind": "gitlab_pipeline",
            "host": "gitlab.example.com",
            "project": "group/app",
            "ref": "main",
            "outputs": {"job": "report", "fields": {"version": "string"}},
        }))
        .unwrap();
        assert_eq!(target.kind(), CallKind::GitlabPipeline);
        assert!(!target.watches());
        let pin = Pin::GitlabRef {
            host: "gitlab.example.com".into(),
            project: "group/app".into(),
            git_ref: GitRefName::parse("main").unwrap(),
            commit: "a".repeat(40),
        };
        assert!(pin.pins(&target));
        assert!(pin.moves());
        assert_eq!(pin.kind(), CallKind::GitlabPipeline);
        let elsewhere = Pin::GitlabRef {
            host: "gitlab.example.com".into(),
            project: "group/other".into(),
            git_ref: GitRefName::parse("main").unwrap(),
            commit: "a".repeat(40),
        };
        assert!(!elsewhere.pins(&target));
        let bare: CallTarget = serde_json::from_value(serde_json::json!({
            "kind": "gitlab_pipeline",
            "host": "gitlab.com",
            "project": "g/p",
            "ref": "main",
        }))
        .unwrap();
        assert!(
            matches!(bare, CallTarget::GitlabPipeline { outputs, .. } if outputs == GitlabOutputs::default())
        );
        let handle = TargetHandle::GitlabPipeline {
            host: "gitlab.com".into(),
            project: "g/p".into(),
            pipeline_id: 9,
        };
        assert_eq!(handle.kind(), CallKind::GitlabPipeline);
        assert_eq!(
            serde_json::to_value(&handle).unwrap()["kind"],
            "gitlab_pipeline"
        );
    }
}
