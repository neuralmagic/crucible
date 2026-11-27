//! Running a call task: bind its values from its dependencies, ask the controller that launched
//! this run to dispatch the target as a run of its own, wait for that run to end, and settle
//! with its result. The engine never reaches the target's system itself.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::plan::admission::{Admission, Lane, Refusal};
use crate::plan::attempt::{Attempt, AttemptOutcome, BatchItem, TransportFailure};
use crate::plan::files::CapturedFiles;
use crate::plan::ir::Binding;
use crate::plan::ir::{ITEM_INPUT, Join, PARAMS_INPUT, Task, TaskKind, TaskName};
use crate::plan::task_runner::TaskRunner;
use crucible_contract::TransportCause;
use crucible_contract::calls::{
    CALL_CANCEL_PATH, CALL_INPUT_FILE_PATH, CALL_PATH, CALL_RESULT_FILE_PATH, CALLS_PATH, CallId,
    CallRefused, CallState, CallStatus, DispatchRequest, DispatchResponse, FileRef,
    MAX_FILE_PARAM_BYTES, PARAM_FILE_PATH,
};
use crucible_contract::session::SessionEvent;

pub const POLL_ENV: &str = "CRUCIBLE_CALL_POLL_SECS";
const DEFAULT_POLL: Duration = Duration::from_secs(3);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_POLL_FAILURES: u32 = 20;
const MAX_DISPATCH_TRIES: u32 = 10;
const DISPATCH_BACKOFF: Duration = Duration::from_secs(1);
const MAX_DISPATCH_BACKOFF: Duration = Duration::from_secs(15);

/// The controller that launched this run, reached at its call API with the run's own pod token.
pub struct Controller {
    url: String,
    run: String,
    token: String,
    client: reqwest::blocking::Client,
    interval: Duration,
}

impl std::fmt::Debug for Controller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Controller")
            .field("url", &self.url)
            .field("run", &self.run)
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

/// What a dispatch answered.
#[derive(Debug)]
enum Dispatch {
    Created(DispatchResponse),
    Refused(CallRefused),
}

/// Why a dispatch request got no answer. A retryable one is sent again under the same call
/// identity, which the controller answers with the call it already started.
#[derive(Debug, PartialEq, Eq)]
enum DispatchError {
    Retryable(String),
    Fatal(String),
}

impl DispatchError {
    fn note(&self) -> &str {
        match self {
            DispatchError::Retryable(note) | DispatchError::Fatal(note) => note,
        }
    }

    fn of_status(status: reqwest::StatusCode, body: &str) -> Self {
        let note = format!("HTTP {status}: {}", body.trim());
        if status == reqwest::StatusCode::CONFLICT
            || status == reqwest::StatusCode::TOO_MANY_REQUESTS
            || status.is_server_error()
        {
            DispatchError::Retryable(note)
        } else {
            DispatchError::Fatal(note)
        }
    }
}

impl Controller {
    pub fn from_env() -> Option<Self> {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let interval = env(POLL_ENV)
            .and_then(|v| v.parse().ok())
            .map_or(DEFAULT_POLL, Duration::from_secs);
        Controller::new(
            env(crucible_contract::ENV_CONTROLLER_URL)?,
            env(crucible_contract::ENV_RUN_NAME)?,
            env(crucible_contract::ENV_POD_TOKEN)?,
            interval,
        )
        .ok()
    }

    pub fn new(
        url: String,
        run: String,
        token: String,
        interval: Duration,
    ) -> Result<Self, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Controller {
            url,
            run,
            token,
            client,
            interval,
        })
    }

    fn at(&self, path: &str, call: Option<&CallId>) -> String {
        crucible_contract::calls::url(&self.url, path, &self.run, call)
    }

    fn checked(
        resp: reqwest::Result<reqwest::blocking::Response>,
    ) -> Result<reqwest::blocking::Response, String> {
        let resp = resp.map_err(|e| e.to_string())?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().unwrap_or_default();
        Err(format!("HTTP {status}: {}", body.trim()))
    }

    fn dispatch(&self, request: &DispatchRequest) -> Result<Dispatch, DispatchError> {
        let resp = self
            .client
            .post(self.at(CALLS_PATH, None))
            .bearer_auth(&self.token)
            .json(request)
            .send()
            .map_err(|e| DispatchError::Retryable(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            return resp
                .json()
                .map(Dispatch::Refused)
                .map_err(|e| DispatchError::Retryable(format!("decoding the refusal: {e}")));
        }
        if !status.is_success() {
            let body = resp.text().unwrap_or_default();
            return Err(DispatchError::of_status(status, &body));
        }
        resp.json()
            .map(Dispatch::Created)
            .map_err(|e| DispatchError::Retryable(format!("decoding the dispatch: {e}")))
    }

    fn state(&self, call: &CallId) -> Result<CallState, String> {
        Self::checked(
            self.client
                .get(self.at(CALL_PATH, Some(call)))
                .bearer_auth(&self.token)
                .send(),
        )?
        .json()
        .map_err(|e| format!("decoding the call state: {e}"))
    }

    fn cancel(&self, call: &CallId) -> Result<(), String> {
        Self::checked(
            self.client
                .post(self.at(CALL_CANCEL_PATH, Some(call)))
                .bearer_auth(&self.token)
                .send(),
        )
        .map(drop)
    }

    fn upload(&self, digest: &str, bytes: Vec<u8>) -> Result<(), String> {
        let url = self.at(CALL_INPUT_FILE_PATH, None).replace(
            "{digest}",
            &crucible_contract::artifact::path_segment(digest),
        );
        Self::checked(
            self.client
                .put(url)
                .bearer_auth(&self.token)
                .body(bytes)
                .send(),
        )
        .map(drop)
    }

    pub fn param_file(&self, digest: &str) -> Result<Vec<u8>, String> {
        let url = self.at(PARAM_FILE_PATH, None).replace(
            "{digest}",
            &crucible_contract::artifact::path_segment(digest),
        );
        let resp = Self::checked(self.client.get(url).bearer_auth(&self.token).send())?;
        if resp
            .content_length()
            .is_some_and(|len| len > MAX_FILE_PARAM_BYTES)
        {
            return Err(format!(
                "the controller sent more than {MAX_FILE_PARAM_BYTES} bytes"
            ));
        }
        let bytes = resp.bytes().map_err(|e| e.to_string())?;
        if bytes.len() as u64 > MAX_FILE_PARAM_BYTES {
            return Err(format!(
                "the controller sent more than {MAX_FILE_PARAM_BYTES} bytes"
            ));
        }
        Ok(bytes.to_vec())
    }

    fn result_file(&self, call: &CallId, path: &str) -> Result<Vec<u8>, String> {
        let escaped: Vec<String> = path
            .split('/')
            .map(crucible_contract::artifact::path_segment)
            .collect();
        let url = self
            .at(CALL_RESULT_FILE_PATH, Some(call))
            .replace("{*path}", &escaped.join("/"));
        Self::checked(self.client.get(url).bearer_auth(&self.token).send())?
            .bytes()
            .map(|b| b.to_vec())
            .map_err(|e| e.to_string())
    }
}

/// One call's identity on the controller: the call task, its mapped instance, and the attempt.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CallKey {
    pub task: String,
    pub instance: Option<String>,
    pub attempt: u32,
}

impl CallKey {
    pub fn of(name: &TaskName, attempt: u32) -> Self {
        match name.instance_of() {
            Some((task, key)) => CallKey {
                task: task.to_owned(),
                instance: Some(key.to_owned()),
                attempt,
            },
            None => CallKey {
                task: name.0.clone(),
                instance: None,
                attempt,
            },
        }
    }
}

/// Where a bound file's bytes are read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileSource {
    Captured {
        producer: TaskName,
        path: String,
    },
    Staged {
        path: PathBuf,
        media_type: Option<String>,
    },
}

/// One parameter's value, bound when the call is ready.
#[derive(Clone, Debug, PartialEq)]
pub enum Bound {
    Value(Value),
    File(FileSource),
}

pub fn resolve(
    task: &Task,
    inputs: &BTreeMap<TaskName, Value>,
) -> Result<BTreeMap<String, Bound>, String> {
    let TaskKind::Call {
        bindings,
        file_params,
        ..
    } = &task.task
    else {
        return Err(format!("{} is not a call", task.name));
    };
    let output_of = |producer: &TaskName| {
        let read = inputs.get(producer)?;
        match task.join {
            Join::Settled => read.get("output"),
            Join::All | Join::Passed => Some(read),
        }
    };
    let mut bound = BTreeMap::new();
    for (param, binding) in bindings {
        let value = match binding {
            Binding::Literal { value } => Bound::Value(value.clone()),
            Binding::Field { producer, field } => Bound::Value(
                output_of(producer)
                    .and_then(|output| output.get(&field.0))
                    .cloned()
                    .ok_or_else(|| {
                        format!(
                            "{param}: {producer}.{} is absent from what {producer} emitted",
                            field.0
                        )
                    })?,
            ),
            Binding::File { producer, path } => Bound::File(FileSource::Captured {
                producer: producer.clone(),
                path: path.clone(),
            }),
            Binding::Param { name } => {
                let value = inputs
                    .get(&TaskName::reserved(PARAMS_INPUT))
                    .and_then(|params| params.get(name))
                    .ok_or_else(|| format!("{param}: parameter {name:?} has no value"))?;
                if file_params.contains(param) {
                    let file: FileRef = serde_json::from_value(value.clone())
                        .map_err(|e| format!("{param}: parameter {name:?} is not a file: {e}"))?;
                    let path = file
                        .path
                        .ok_or_else(|| format!("{param}: parameter {name:?} was not staged"))?;
                    Bound::File(FileSource::Staged {
                        path: PathBuf::from(path),
                        media_type: file.media_type,
                    })
                } else {
                    Bound::Value(value.clone())
                }
            }
            Binding::Item { field } => {
                let item = inputs
                    .get(&TaskName::reserved(ITEM_INPUT))
                    .ok_or_else(|| format!("{param}: the call is not mapped"))?;
                let item = match field {
                    Some(field) => item
                        .get(&field.0)
                        .ok_or_else(|| format!("{param}: the item has no field {}", field.0))?,
                    None => item,
                };
                Bound::Value(item.clone())
            }
        };
        bound.insert(param.clone(), value);
    }
    Ok(bound)
}

/// What a call reads besides its inputs.
#[derive(Clone, Copy, Default)]
pub struct CallCtx<'a> {
    pub controller: Option<&'a Controller>,
    pub files: Option<&'a CapturedFiles>,
    pub dispatched: Option<&'a BTreeMap<CallKey, CallId>>,
}

/// A runner that runs call tasks itself and hands every other task to `inner`.
pub struct WithCalls<'r, 'c> {
    inner: &'r mut dyn TaskRunner,
    ctx: CallCtx<'c>,
}

impl<'r, 'c> WithCalls<'r, 'c> {
    pub fn new(inner: &'r mut dyn TaskRunner, ctx: CallCtx<'c>) -> Self {
        WithCalls { inner, ctx }
    }
}

fn is_call(task: &Task) -> bool {
    matches!(task.task, TaskKind::Call { .. })
}

impl TaskRunner for WithCalls<'_, '_> {
    fn prepare(&mut self) {
        self.inner.prepare();
    }

    fn run(
        &mut self,
        task: &Task,
        attempt: u32,
        inputs: &BTreeMap<TaskName, Value>,
        lane: Option<&Lane<'_>>,
    ) -> Attempt {
        if is_call(task) {
            return run(task, attempt, inputs, lane, self.ctx);
        }
        self.inner.run(task, attempt, inputs, lane)
    }

    fn stage(&mut self, task: &Task, producers: &[&Task]) -> Result<(), String> {
        if is_call(task) {
            return Ok(());
        }
        self.inner.stage(task, producers)
    }

    fn settled(&mut self, task: &Task, passed: bool) {
        if !is_call(task) {
            self.inner.settled(task, passed);
        }
    }

    fn budget_left(&mut self, usd: f64) {
        self.inner.budget_left(usd);
    }

    fn run_many(
        &mut self,
        batch: &[BatchItem<'_>],
        admission: &Admission,
    ) -> Vec<Result<Attempt, Refusal>> {
        if !batch.iter().any(|b| is_call(b.task)) {
            return self.inner.run_many(batch, admission);
        }
        let others: Vec<BatchItem<'_>> = batch
            .iter()
            .filter(|b| !is_call(b.task))
            .map(|b| BatchItem {
                task: b.task,
                attempt: b.attempt,
                inputs: b.inputs.clone(),
            })
            .collect();
        let calls: Vec<BatchItem<'_>> = batch
            .iter()
            .filter(|b| is_call(b.task))
            .map(|b| BatchItem {
                task: b.task,
                attempt: b.attempt,
                inputs: b.inputs.clone(),
            })
            .collect();
        let mut others = if others.is_empty() {
            Vec::new()
        } else {
            self.inner.run_many(&others, admission)
        }
        .into_iter();
        let ctx = self.ctx;
        let mut calls = admission
            .run_all(&calls, |b, lane| {
                run(b.task, b.attempt, &b.inputs, Some(lane), ctx)
            })
            .into_iter();
        let missing = || Ok(Attempt::failed(0.0, "no result".to_string()));
        batch
            .iter()
            .map(|b| match is_call(b.task) {
                true => calls.next().unwrap_or_else(missing),
                false => others.next().unwrap_or_else(missing),
            })
            .collect()
    }
}

fn failed(note: String) -> Attempt {
    Attempt::failed(0.0, note)
}

pub fn run(
    task: &Task,
    attempt: u32,
    inputs: &BTreeMap<TaskName, Value>,
    lane: Option<&Lane<'_>>,
    ctx: CallCtx<'_>,
) -> Attempt {
    let TaskKind::Call {
        alias,
        budget,
        interface_digest,
        ..
    } = &task.task
    else {
        return failed(format!("{} is not a call", task.name));
    };
    let Some(controller) = ctx.controller else {
        return failed(format!(
            "call {alias} refused: {} is not set, and a call is dispatched by the controller \
             that launched this run",
            crucible_contract::ENV_CONTROLLER_URL
        ));
    };
    let key = CallKey::of(&task.name, attempt);
    let deadline = lane.and_then(Lane::deadline);
    let recorded = ctx.dispatched.and_then(|d| d.get(&key)).cloned();
    let call = match recorded {
        Some(call) => call,
        None => {
            let bound = match resolve(task, inputs) {
                Ok(bound) => bound,
                Err(note) => return failed(format!("call {alias}: {note}")),
            };
            let mut values = BTreeMap::new();
            let mut files = BTreeMap::new();
            for (param, value) in bound {
                match value {
                    Bound::Value(value) => {
                        values.insert(param, value);
                    }
                    Bound::File(source) => {
                        match send_file(controller, &task.name, &source, ctx.files) {
                            Ok(file) => {
                                files.insert(param, file);
                            }
                            Err(note) => return failed(format!("call {alias}: {param}: {note}")),
                        }
                    }
                }
            }
            let request = DispatchRequest {
                task: key.task.clone(),
                instance: key.instance.clone(),
                attempt,
                alias: alias.clone(),
                interface_digest: interface_digest.clone(),
                values,
                files,
                budget: *budget,
            };
            match dispatch_once(controller, &request, deadline) {
                Ok(Dispatch::Created(created)) => {
                    let event = SessionEvent::CallDispatched {
                        task: key.task.clone(),
                        instance: key.instance.clone(),
                        attempt,
                        alias: alias.clone(),
                        call: created.call.clone(),
                        handle: created.handle,
                        link: created.link,
                    };
                    if let Some(lane) = lane
                        && let Err(error) = lane.journal(&event)
                    {
                        tracing::warn!(task = %task.name, %error, "recording a call's dispatch failed");
                    }
                    created.call
                }
                Ok(Dispatch::Refused(refused)) => {
                    return failed(format!("call {alias} refused: {}", refused.note));
                }
                Err(error) => {
                    return Attempt {
                        outcome: AttemptOutcome::Transport(TransportFailure::new(
                            TransportCause::Orchestrator,
                            format!("dispatching call {alias}: {}", error.note()),
                        )),
                        cost_usd: 0.0,
                        repairs: Vec::new(),
                    };
                }
            }
        }
    };
    let state = match wait(controller, &call, deadline) {
        Waited::Ended(state) => *state,
        Waited::TimedOut(deadline) => {
            if let Err(error) = controller.cancel(&call) {
                tracing::warn!(task = %task.name, %error, "cancelling a timed-out call failed");
            }
            return Attempt {
                outcome: AttemptOutcome::TimedOut(deadline),
                cost_usd: 0.0,
                repairs: Vec::new(),
            };
        }
        Waited::Lost(note) => return failed(format!("call {alias}: {note}")),
    };
    let outcome = match state.status {
        CallStatus::Passed => match publish(controller, task, &state, ctx.files) {
            Ok(()) => AttemptOutcome::Pass(match state.result {
                Value::Null => Value::Object(serde_json::Map::new()),
                result => result,
            }),
            Err(note) => AttemptOutcome::fail(format!("call {alias}: {note}")),
        },
        CallStatus::Cancelled => {
            AttemptOutcome::fail(format!("call {alias} was cancelled{}", noted(&state.note)))
        }
        CallStatus::Failed | CallStatus::Pending | CallStatus::Running => {
            AttemptOutcome::fail(if state.note.is_empty() {
                format!("call {alias}: the target run failed")
            } else {
                state.note
            })
        }
    };
    Attempt {
        outcome,
        cost_usd: 0.0,
        repairs: Vec::new(),
    }
}

fn dispatch_once(
    controller: &Controller,
    request: &DispatchRequest,
    deadline: Option<crate::deadline::Deadline>,
) -> Result<Dispatch, DispatchError> {
    let mut tries = 0;
    loop {
        tries += 1;
        let error = match controller.dispatch(request) {
            Err(error @ DispatchError::Retryable(_)) => error,
            answered => return answered,
        };
        tracing::warn!(task = %request.task, tries, error = error.note(), "dispatching a call failed; retrying");
        if tries >= MAX_DISPATCH_TRIES {
            return Err(error);
        }
        let pause = DISPATCH_BACKOFF
            .saturating_mul(tries)
            .min(MAX_DISPATCH_BACKOFF);
        let pause = match deadline {
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline.at {
                    return Err(error);
                }
                pause.min(deadline.at - now)
            }
            None => pause,
        };
        std::thread::sleep(pause);
    }
}

fn noted(note: &str) -> String {
    if note.is_empty() {
        String::new()
    } else {
        format!(": {note}")
    }
}

enum Waited {
    Ended(Box<CallState>),
    TimedOut(crate::deadline::Deadline),
    Lost(String),
}

fn wait(
    controller: &Controller,
    call: &CallId,
    deadline: Option<crate::deadline::Deadline>,
) -> Waited {
    let mut failures = 0;
    loop {
        match controller.state(call) {
            Ok(state) if state.status.ended() => return Waited::Ended(Box::new(state)),
            Ok(_) => failures = 0,
            Err(error) => {
                failures += 1;
                tracing::warn!(%call, %error, failures, "polling a call failed");
                if failures >= MAX_POLL_FAILURES {
                    return Waited::Lost(format!(
                        "the controller did not answer for {failures} polls in a row: {error}"
                    ));
                }
            }
        }
        let now = Instant::now();
        let pause = match deadline {
            Some(deadline) if now >= deadline.at => return Waited::TimedOut(deadline),
            Some(deadline) => controller.interval.min(deadline.at - now),
            None => controller.interval,
        };
        std::thread::sleep(pause);
    }
}

fn send_file(
    controller: &Controller,
    call: &TaskName,
    source: &FileSource,
    captured: Option<&CapturedFiles>,
) -> Result<FileRef, String> {
    let (bytes, path, media_type) = match source {
        FileSource::Captured { producer, path } => {
            let captured = captured.ok_or("this run captures no files")?;
            let instance = call
                .instance_of()
                .and_then(|(_, key)| crate::plan::ir::ItemKey::new(key).ok())
                .map(|key| TaskName::instance(producer, &key))
                .filter(|instance| captured.has(instance));
            let from = instance.as_ref().unwrap_or(producer);
            let bytes = captured
                .read(from, path)
                .ok_or_else(|| format!("{from} captured no {path:?}"))?;
            (bytes, path.clone(), None)
        }
        FileSource::Staged { path, media_type } => {
            let size = std::fs::metadata(path)
                .map_err(|e| format!("{}: {e}", path.display()))?
                .len();
            if size > MAX_FILE_PARAM_BYTES {
                return Err(format!("{} is larger than a call may send", path.display()));
            }
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            (bytes, name, media_type.clone())
        }
    };
    if bytes.len() as u64 > MAX_FILE_PARAM_BYTES {
        return Err(format!("{path:?} is larger than a call may send"));
    }
    let digest = crucible_contract::artifact::content_digest(&bytes);
    let size = bytes.len() as u64;
    controller
        .upload(&digest, bytes)
        .map_err(|e| format!("uploading {path:?}: {e}"))?;
    Ok(FileRef {
        digest,
        size,
        path: Some(path),
        media_type,
    })
}

fn relative(path: &str) -> Option<PathBuf> {
    let candidate = Path::new(path);
    let clean = candidate
        .components()
        .all(|c| matches!(c, Component::Normal(_)));
    (clean && !path.is_empty()).then(|| candidate.to_path_buf())
}

fn publish(
    controller: &Controller,
    task: &Task,
    state: &CallState,
    captured: Option<&CapturedFiles>,
) -> Result<(), String> {
    if task.emits_files.is_empty() {
        return Ok(());
    }
    let Some(captured) = captured else {
        return Ok(());
    };
    let staging = captured
        .root()
        .join(format!(".call-{}", task.name.dir_name()));
    let _ = std::fs::remove_dir_all(&staging);
    let written = (|| {
        for declared in &task.emits_files {
            let file = state
                .result_files
                .iter()
                .find(|f| f.path.as_deref() == Some(declared.path.as_str()))
                .ok_or_else(|| format!("the target returned no {:?}", declared.path))?;
            let relative = relative(&declared.path)
                .ok_or_else(|| format!("{:?} is not a relative path", declared.path))?;
            let bytes = controller
                .result_file(&state.call, &declared.path)
                .map_err(|e| format!("fetching {:?}: {e}", declared.path))?;
            if crucible_contract::artifact::content_digest(&bytes) != file.digest {
                return Err(format!(
                    "{:?} does not match the digest the target reported",
                    declared.path
                ));
            }
            let target = staging.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&target, bytes).map_err(|e| e.to_string())?;
        }
        let dir = captured.dir(&task.name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&staging, &dir).map_err(|e| e.to_string())
    })();
    if written.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    written
}

#[cfg(test)]
mod tests {
    use crate::plan::call_run::{Bound, CallKey, DispatchError, FileSource, resolve};
    use crate::plan::ir::Binding;
    use crate::plan::ir::{Join, OutputField, Stage, Task, TaskKind, TaskName};
    use crucible_contract::calls::{CallAlias, CallKind};
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};

    fn call(bindings: BTreeMap<String, Binding>, file_params: &[&str], join: Join) -> Task {
        Task {
            name: "ship".into(),
            task: TaskKind::Call {
                alias: CallAlias::parse("release").unwrap(),
                target: CallKind::Playbook,
                bindings,
                budget: None,
                interface_digest: "sha256:00".into(),
                file_params: file_params
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<BTreeSet<_>>(),
            },
            depends_on: vec!["plan".into()],
            session: None,
            needs: "any".into(),
            required: true,
            isolation: None,
            join,
            stage: Stage::Iteration,
            emits: Default::default(),
            emits_files: Vec::new(),
            over: None,
            max_fanout: None,
            keyed: Vec::new(),
            when: None,
            revise: None,
            timeout: None,
            history: None,
            repair: 0,
            optional: Vec::new(),
        }
    }

    fn field(producer: &str, field: &str) -> Binding {
        Binding::Field {
            producer: TaskName(producer.into()),
            field: OutputField(field.into()),
        }
    }

    #[test]
    fn bindings_read_dependency_outputs_params_and_the_item() {
        let task = call(
            BTreeMap::from([
                ("version".into(), field("plan", "version")),
                (
                    "mode".into(),
                    Binding::Literal {
                        value: json!("fast"),
                    },
                ),
                (
                    "matrix".into(),
                    Binding::Param {
                        name: "matrix".into(),
                    },
                ),
                (
                    "notes".into(),
                    Binding::Param {
                        name: "notes".into(),
                    },
                ),
                (
                    "log".into(),
                    Binding::File {
                        producer: "plan".into(),
                        path: "log.txt".into(),
                    },
                ),
                ("repo".into(), Binding::Item { field: None }),
            ]),
            &["notes"],
            Join::All,
        );
        let inputs = BTreeMap::from([
            (TaskName("plan".into()), json!({"version": "1.2"})),
            (
                TaskName("params".into()),
                json!({"matrix": {"os": ["linux"]}, "notes": {"path": "/s/n.md", "size": 1, "digest": "sha256:aa", "media_type": "text/markdown"}}),
            ),
            (TaskName("item".into()), json!("acme/app")),
        ]);
        let bound = resolve(&task, &inputs).unwrap();
        assert_eq!(bound["version"], Bound::Value(json!("1.2")));
        assert_eq!(bound["mode"], Bound::Value(json!("fast")));
        assert_eq!(bound["matrix"], Bound::Value(json!({"os": ["linux"]})));
        assert_eq!(bound["repo"], Bound::Value(json!("acme/app")));
        assert_eq!(
            bound["notes"],
            Bound::File(FileSource::Staged {
                path: "/s/n.md".into(),
                media_type: Some("text/markdown".into())
            })
        );
        assert_eq!(
            bound["log"],
            Bound::File(FileSource::Captured {
                producer: "plan".into(),
                path: "log.txt".into()
            })
        );
    }

    #[test]
    fn a_settled_join_reads_the_output_entry_and_a_missing_field_is_refused() {
        let task = call(
            BTreeMap::from([("version".into(), field("plan", "version"))]),
            &[],
            Join::Settled,
        );
        let settled = BTreeMap::from([(
            TaskName("plan".into()),
            json!({"status": "pass", "output": {"version": "2"}}),
        )]);
        assert_eq!(
            resolve(&task, &settled).unwrap()["version"],
            Bound::Value(json!("2"))
        );
        let empty: BTreeMap<TaskName, Value> =
            BTreeMap::from([(TaskName("plan".into()), json!({"status": "fail"}))]);
        let refused = resolve(&task, &empty).unwrap_err();
        assert!(refused.contains("plan.version is absent"), "{refused}");
        let unmapped = call(
            BTreeMap::from([("repo".into(), Binding::Item { field: None })]),
            &[],
            Join::All,
        );
        assert!(resolve(&unmapped, &BTreeMap::new()).is_err());
    }

    #[test]
    fn a_key_names_the_task_and_its_item() {
        assert_eq!(
            CallKey::of(&TaskName("ship[acme/app]".into()), 2),
            CallKey {
                task: "ship".into(),
                instance: Some("acme/app".into()),
                attempt: 2
            }
        );
        assert_eq!(CallKey::of(&TaskName("ship".into()), 1).instance, None);
    }

    #[test]
    fn only_answers_that_may_come_out_differently_are_resent() {
        use reqwest::StatusCode;
        for status in [
            StatusCode::CONFLICT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert!(
                matches!(
                    DispatchError::of_status(status, ""),
                    DispatchError::Retryable(_)
                ),
                "{status} is resent"
            );
        }
        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
        ] {
            assert!(
                matches!(
                    DispatchError::of_status(status, ""),
                    DispatchError::Fatal(_)
                ),
                "{status} is not resent"
            );
        }
        assert_eq!(
            DispatchError::of_status(StatusCode::FORBIDDEN, " not this run \n").note(),
            "HTTP 403 Forbidden: not this run"
        );
    }
}
