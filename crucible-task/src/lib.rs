//! Write a crucible `command` or `evaluate` task as a typed program.
//!
//! ```no_run
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize)]
//! struct Params {
//!     profile: String,
//! }
//!
//! #[derive(Deserialize)]
//! struct Stage {
//!     members: Vec<String>,
//! }
//!
//! #[derive(Serialize)]
//! struct Out {
//!     tickets: usize,
//! }
//!
//! fn main() -> std::process::ExitCode {
//!     crucible_task::run(|task| {
//!         let params: Params = task.params()?;
//!         let stage: Stage = task.input("stage")?;
//!         let manifest = task.input_file("stage", "triage-run-manifest.json")?;
//!         task.write_file(
//!             "REPORT.md",
//!             format!("{}: {} tickets from {}\n", params.profile, stage.members.len(), manifest.display()),
//!         )?;
//!         Ok::<_, crucible_task::Error>(Out { tickets: stage.members.len() })
//!     })
//! }
//! ```

use std::fmt::Display;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

/// Names the JSON file holding the task's inputs.
pub const INPUTS_FILE_ENV: &str = "CRUCIBLE_INPUTS_FILE";
/// Names the task being run.
pub const TASK_NAME_ENV: &str = "CRUCIBLE_TASK";
/// The key of the inputs file that holds the workflow's bound parameters.
pub const PARAMS_INPUT: &str = "params";
/// The directory, under the task's working directory, holding its dependencies' declared files.
pub const STAGED_INPUTS: &str = "inputs";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{INPUTS_FILE_ENV} is unset: this program runs as a crucible command or evaluate task")]
    NotATask,
    #[error("reading the inputs file {}: {source}", path.display())]
    ReadInputs {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the inputs file {} is not a JSON object: {reason}", path.display())]
    InputsShape { path: PathBuf, reason: String },
    #[error("task {task:?} is not an input of this task; add it to depends_on")]
    NoInput { task: String },
    #[error("the workflow declares no params")]
    NoParams,
    #[error("the output of {from:?} does not have the expected shape: {source}")]
    InputShape {
        from: String,
        source: serde_json::Error,
    },
    #[error("{path:?} must be a relative path with no `..`")]
    UnsafePath { path: PathBuf },
    #[error("task {task:?} declared no file {}; add it to that task's emits_files", path.display())]
    NoInputFile { task: String, path: PathBuf },
    #[error("writing {}: {source}", path.display())]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the result is not serializable as a JSON object: {reason}")]
    ResultShape { reason: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// The task this process is running as: its inputs and its working directory.
#[derive(Debug)]
pub struct Task {
    name: String,
    inputs: Map<String, Value>,
    dir: PathBuf,
}

impl Task {
    /// The task the engine started this process as.
    pub fn from_env() -> Result<Task> {
        let inputs = std::env::var_os(INPUTS_FILE_ENV).ok_or(Error::NotATask)?;
        let name = std::env::var(TASK_NAME_ENV).unwrap_or_default();
        Task::open(name, Path::new(&inputs), Path::new("."))
    }

    /// A task named `name` reading `inputs_file`, working in `dir`.
    pub fn open(name: impl Into<String>, inputs_file: &Path, dir: &Path) -> Result<Task> {
        let text = std::fs::read_to_string(inputs_file).map_err(|source| Error::ReadInputs {
            path: inputs_file.to_path_buf(),
            source,
        })?;
        let inputs = serde_json::from_str::<Map<String, Value>>(&text).map_err(|error| {
            Error::InputsShape {
                path: inputs_file.to_path_buf(),
                reason: error.to_string(),
            }
        })?;
        Ok(Task {
            name: name.into(),
            inputs,
            dir: dir.to_path_buf(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The workflow's parameters as this run bound them.
    pub fn params<T: DeserializeOwned>(&self) -> Result<T> {
        let params = self.inputs.get(PARAMS_INPUT).ok_or(Error::NoParams)?;
        T::deserialize(params).map_err(|source| Error::InputShape {
            from: PARAMS_INPUT.to_owned(),
            source,
        })
    }

    /// The output of `task`, which this task lists in `depends_on`.
    pub fn input<T: DeserializeOwned>(&self, task: &str) -> Result<T> {
        let output = self
            .inputs
            .get(task)
            .filter(|_| task != PARAMS_INPUT)
            .ok_or_else(|| Error::NoInput {
                task: task.to_owned(),
            })?;
        T::deserialize(output).map_err(|source| Error::InputShape {
            from: task.to_owned(),
            source,
        })
    }

    /// The names of the tasks whose outputs this task received.
    pub fn input_names(&self) -> impl Iterator<Item = &str> {
        self.inputs
            .keys()
            .map(String::as_str)
            .filter(|name| *name != PARAMS_INPUT)
    }

    /// The path of a file `task` declared in `emits_files`, as staged for this task.
    pub fn input_file(&self, task: &str, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = relative(path.as_ref())?;
        let staged = self
            .dir
            .join(STAGED_INPUTS)
            .join(relative(Path::new(task))?)
            .join(&path);
        if staged.is_file() {
            Ok(staged)
        } else {
            Err(Error::NoInputFile {
                task: task.to_owned(),
                path,
            })
        }
    }

    /// Write a file this task declares in `emits_files`, creating its directories.
    pub fn write_file(
        &self,
        path: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> Result<PathBuf> {
        let dest = self.dir.join(relative(path.as_ref())?);
        let write = |source| Error::Write {
            path: dest.clone(),
            source,
        };
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(write)?;
        }
        std::fs::write(&dest, contents).map_err(write)?;
        Ok(dest)
    }
}

fn relative(path: &Path) -> Result<PathBuf> {
    let normal = path
        .components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir));
    if normal
        && path
            .components()
            .any(|part| matches!(part, Component::Normal(_)))
    {
        Ok(path.to_path_buf())
    } else {
        Err(Error::UnsafePath {
            path: path.to_path_buf(),
        })
    }
}

/// What an `evaluate` task reports: whether the candidate passed, and what it measured.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Measured {
    #[serde(skip_serializing_if = "Option::is_none")]
    pass: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    #[serde(flatten)]
    fields: Map<String, Value>,
}

impl Measured {
    /// A measurement graded by the task's declared threshold.
    pub fn score(score: f64) -> Self {
        Measured {
            score: Some(score),
            ..Measured::default()
        }
    }

    pub fn passed() -> Self {
        Measured {
            pass: Some(true),
            ..Measured::default()
        }
    }

    /// A failure, with the reason the run records.
    pub fn failed(note: impl Into<String>) -> Self {
        Measured {
            pass: Some(false),
            note: Some(note.into()),
            ..Measured::default()
        }
    }

    pub fn with_score(mut self, score: f64) -> Self {
        self.score = Some(score);
        self
    }

    /// The digest of the candidate that was measured, for the rungs that follow.
    pub fn with_digest(mut self, digest: impl Into<String>) -> Self {
        self.digest = Some(digest.into());
        self
    }

    /// Another field the task declares in `emits`.
    pub fn with(mut self, name: impl Into<String>, value: impl Into<Value>) -> Self {
        self.fields.insert(name.into(), value.into());
        self
    }
}

/// Run `body` as the task and report its result to the engine: the value it returns as the last
/// line of stdout, or its error as a failed result and a nonzero exit.
pub fn run<O, E>(body: impl FnOnce(&Task) -> std::result::Result<O, E>) -> ExitCode
where
    O: Serialize,
    E: Display + From<Error>,
{
    let outcome = Task::from_env()
        .map_err(E::from)
        .and_then(|task| body(&task));
    let stdout = std::io::stdout();
    report(outcome, &mut stdout.lock(), &mut std::io::stderr().lock())
}

fn report<O: Serialize, E: Display>(
    outcome: std::result::Result<O, E>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> ExitCode {
    let line = outcome
        .map_err(|error| error.to_string())
        .and_then(|result| result_line(&result).map_err(|error| error.to_string()));
    match line {
        Ok(line) => match writeln!(out, "{line}") {
            Ok(()) => ExitCode::SUCCESS,
            Err(_) => ExitCode::FAILURE,
        },
        Err(reason) => {
            let _ = writeln!(err, "{reason}");
            let _ = writeln!(
                out,
                "{}",
                serde_json::json!({"pass": false, "note": reason})
            );
            ExitCode::FAILURE
        }
    }
}

/// `result` as the one line the engine reads. The engine takes a JSON object.
fn result_line(result: &impl Serialize) -> Result<String> {
    let shape = |reason: String| Error::ResultShape { reason };
    match serde_json::to_value(result).map_err(|error| shape(error.to_string()))? {
        Value::Object(fields) => Ok(Value::Object(fields).to_string()),
        Value::Null => Ok("{}".to_owned()),
        other => Err(shape(format!("it serializes as {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use crate::{Error, Measured, Task, report, result_line};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, PartialEq)]
    struct Params {
        profile: String,
        limit: u32,
    }

    #[derive(Debug, Deserialize, PartialEq)]
    struct Stage {
        members: Vec<String>,
    }

    fn task(dir: &std::path::Path, inputs: &str) -> Task {
        let file = dir.join("inputs.json");
        std::fs::write(&file, inputs).unwrap();
        Task::open("report", &file, dir).unwrap()
    }

    #[test]
    fn params_and_inputs_decode_into_the_callers_types() {
        let dir = tempfile::tempdir().unwrap();
        let task = task(
            dir.path(),
            r#"{"params":{"profile":"llm-d","limit":5},"stage":{"members":["A-1","A-2"],"extra":1}}"#,
        );
        assert_eq!(task.name(), "report");
        assert_eq!(
            task.params::<Params>().unwrap(),
            Params {
                profile: "llm-d".to_owned(),
                limit: 5
            }
        );
        assert_eq!(
            task.input::<Stage>("stage").unwrap().members,
            ["A-1", "A-2"]
        );
        assert_eq!(task.input_names().collect::<Vec<_>>(), ["stage"]);
    }

    #[test]
    fn a_missing_or_misshapen_input_names_what_to_fix() {
        let dir = tempfile::tempdir().unwrap();
        let task = task(dir.path(), r#"{"stage":{"members":"not a list"}}"#);
        let cases = [
            (
                task.input::<Stage>("apply").unwrap_err(),
                "\"apply\" is not an input of this task; add it to depends_on",
            ),
            (
                task.input::<Stage>("params").unwrap_err(),
                "\"params\" is not an input of this task",
            ),
            (
                task.input::<Stage>("stage").unwrap_err(),
                "the output of \"stage\" does not have the expected shape: invalid type: string",
            ),
            (
                task.params::<Params>().unwrap_err(),
                "the workflow declares no params",
            ),
        ];
        for (error, expected) in cases {
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn files_are_read_from_the_staged_inputs_and_written_beside_them() {
        let dir = tempfile::tempdir().unwrap();
        let task = task(dir.path(), r#"{"stage":{}}"#);
        std::fs::create_dir_all(dir.path().join("inputs/stage/evidence")).unwrap();
        std::fs::write(dir.path().join("inputs/stage/evidence/run.json"), "{}").unwrap();

        assert_eq!(
            task.input_file("stage", "evidence/run.json").unwrap(),
            dir.path().join("inputs/stage/evidence/run.json")
        );
        assert!(matches!(
            task.input_file("stage", "evidence/absent.json")
                .unwrap_err(),
            Error::NoInputFile { .. }
        ));

        let written = task.write_file("out/REPORT.md", "done\n").unwrap();
        assert_eq!(std::fs::read_to_string(written).unwrap(), "done\n");
        for path in ["../escape", "/etc/passwd", "", "."] {
            assert!(
                matches!(
                    task.write_file(path, "x").unwrap_err(),
                    Error::UnsafePath { .. }
                ),
                "{path}"
            );
            assert!(
                matches!(
                    task.input_file("stage", path).unwrap_err(),
                    Error::UnsafePath { .. }
                ),
                "{path}"
            );
        }
        assert!(matches!(
            task.input_file("../stage", "x").unwrap_err(),
            Error::UnsafePath { .. }
        ));
    }

    #[test]
    fn an_inputs_file_that_is_absent_or_not_an_object_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let absent = Task::open("t", &dir.path().join("absent.json"), dir.path()).unwrap_err();
        assert!(matches!(absent, Error::ReadInputs { .. }), "{absent}");
        let file = dir.path().join("inputs.json");
        std::fs::write(&file, "[1, 2]").unwrap();
        let list = Task::open("t", &file, dir.path()).unwrap_err();
        assert!(matches!(list, Error::InputsShape { .. }), "{list}");
    }

    #[derive(Serialize)]
    struct Out {
        tickets: usize,
    }

    fn reported<O: Serialize>(outcome: Result<O, String>) -> (bool, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = report(outcome, &mut out, &mut err);
        (
            code == std::process::ExitCode::SUCCESS,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn a_result_is_one_json_line_and_an_error_is_a_failed_result() {
        assert_eq!(
            reported(Ok(Out { tickets: 3 })),
            (true, "{\"tickets\":3}\n".to_owned(), String::new())
        );
        assert_eq!(reported(Ok(())), (true, "{}\n".to_owned(), String::new()));

        let (ok, out, err) = reported::<Out>(Err("jira said 503\non two lines".to_owned()));
        assert!(!ok);
        assert_eq!(out.matches('\n').count(), 1, "{out:?}");
        assert!(out.ends_with('\n'), "{out:?}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&out).unwrap(),
            serde_json::json!({"pass": false, "note": "jira said 503\non two lines"})
        );
        assert_eq!(err, "jira said 503\non two lines\n");

        let (ok, out, _) = reported(Ok(vec![1, 2]));
        assert!(!ok);
        assert!(out.contains("\"pass\":false"), "{out}");
        assert!(out.contains("not serializable as a JSON object"), "{out}");
    }

    #[test]
    fn a_measurement_carries_only_what_was_set() {
        assert_eq!(
            result_line(&Measured::score(8.5)).unwrap(),
            "{\"score\":8.5}"
        );
        let line = result_line(
            &Measured::passed()
                .with_score(0.31)
                .with_digest("sha256:abc")
                .with("p99_ms", 12),
        )
        .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            serde_json::json!({"pass": true, "score": 0.31, "digest": "sha256:abc", "p99_ms": 12})
        );
        let line = result_line(&Measured::failed("did not build")).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap(),
            serde_json::json!({"pass": false, "note": "did not build"})
        );
    }
}
