//! `workload_results`: the files a Job's containers left in its results directory, read from the
//! log of the collector container the server added to its pods.

use crate::caller::Caller;
use crate::collector;
use crate::kueue::KubeError;
use crate::manifest::{ManifestError, WorkloadKind};
use crate::report;
use crate::results::{Reader, State, log_cap};
use crate::server::{DEFAULT_LOG_BUDGET, KueueMcp, ToolError};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures_util::{AsyncBufReadExt, StreamExt};
use k8s_openapi::api::core::v1::{ContainerStateTerminated, Pod};
use kube::api::{Api, LogParams};
use rmcp::schemars;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ResultsArgs {
    /// Namespace to act in. A run's token is bound to its own; a person names one they may use.
    #[serde(default)]
    pub namespace: Option<String>,
    /// The submitted object's kind.
    pub kind: WorkloadKind,
    /// metadata.name of the submitted object.
    pub name: String,
    /// One pod's name; omit for the Job's newest pod. `other_pods` names the rest.
    #[serde(default)]
    pub pod: Option<String>,
    /// Files to return the contents of, by the paths the list gives them. Omit for the list alone.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Return the contents of every file.
    #[serde(default)]
    pub all: bool,
    /// Total bytes of file contents to return (default 16384; at most whoami's
    /// results_max_bytes). Files past it are named in `omitted`.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub max_bytes: Option<u64>,
}

/// How the collector container of `pod` ended, if it has.
fn ended(pod: &Pod) -> Option<&ContainerStateTerminated> {
    pod.status
        .as_ref()?
        .init_container_statuses
        .as_ref()?
        .iter()
        .find(|c| c.name == collector::NAME)?
        .state
        .as_ref()?
        .terminated
        .as_ref()
}

fn planned_with_results(object: &Value) -> bool {
    object
        .pointer("/spec/template/spec/initContainers")
        .and_then(Value::as_array)
        .is_some_and(|l| {
            l.iter()
                .any(|c| c.get("name").and_then(Value::as_str) == Some(collector::NAME))
        })
}

fn content(path: &str, data: &[u8]) -> Value {
    match std::str::from_utf8(data) {
        Ok(text) => json!({"path": path, "bytes": data.len(), "text": text}),
        Err(_) => json!({"path": path, "bytes": data.len(), "base64": STANDARD.encode(data)}),
    }
}

/// The contents of the `wanted` files (every file when `None`) within `budget` bytes, the ones
/// past it, and the wanted paths the archive does not hold.
fn select(
    files: &[(String, Vec<u8>)],
    wanted: Option<&[String]>,
    budget: u64,
) -> (Vec<Value>, Vec<Value>, Vec<String>) {
    let mut left = budget;
    let mut contents = Vec::new();
    let mut omitted = Vec::new();
    for (path, data) in files {
        if wanted.is_some_and(|w| !w.contains(path)) {
            continue;
        }
        let bytes = data.len() as u64;
        match bytes <= left {
            true => {
                left -= bytes;
                contents.push(content(path, data));
            }
            false => omitted.push(json!({"path": path, "bytes": bytes})),
        }
    }
    let missing = wanted
        .unwrap_or_default()
        .iter()
        .filter(|w| !files.iter().any(|(path, _)| path == *w))
        .cloned()
        .collect();
    (contents, omitted, missing)
}

impl KueueMcp {
    pub(crate) async fn results(&self, c: &Caller, args: ResultsArgs) -> Result<Value, ToolError> {
        let Some(collector) = &self.config.results else {
            return Err(ManifestError::NoCollector.into());
        };
        let ns = c.namespace.as_str();
        let obj = self.get_object(c, args.kind, &args.name).await?;
        let object = serde_json::to_value(&obj).map_err(|source| KubeError::Decode {
            what: format!("{} {}", args.kind.kind(), args.name),
            source,
        })?;
        if args.kind != WorkloadKind::Job || !planned_with_results(&object) {
            return Err(ToolError::NoResults {
                kind: args.kind.kind(),
                name: args.name,
            });
        }
        let mut pods = self.pods_of(ns, args.kind, &obj).await?;
        pods.retain(collector::collects);
        pods.sort_by(|a, b| {
            (&b.metadata.creation_timestamp, report::pod_name(a))
                .cmp(&(&a.metadata.creation_timestamp, report::pod_name(b)))
        });
        let pod = match &args.pod {
            None => pods.first(),
            Some(want) => Some(
                pods.iter()
                    .find(|p| report::pod_name(p) == want)
                    .ok_or_else(|| ToolError::NotFound {
                        kind: "Pod",
                        name: want.clone(),
                        namespace: ns.to_string(),
                    })?,
            ),
        };
        let Some(pod) = pod else {
            return Ok(json!({
                "state": "pending",
                "note": "the Job has no pod: it has not started, or its pods were deleted and their files with them",
            }));
        };
        let name = report::pod_name(pod);
        let others: Vec<&str> = pods
            .iter()
            .map(report::pod_name)
            .filter(|other| *other != name)
            .collect();
        let mut body = json!({
            "pod": name,
            "phase": report::pod_state(pod),
            "other_pods": others,
        });
        let Some(exit) = ended(pod) else {
            body["state"] = json!("pending");
            body["note"] = json!("the files are collected when the Job's containers exit");
            return Ok(body);
        };

        let wanted = match (args.all, args.paths.is_empty()) {
            (true, _) => Some(None),
            (false, false) => Some(Some(args.paths.as_slice())),
            (false, true) => None,
        };
        let read_all = wanted.is_some() || exit.exit_code != 0;
        let lp = LogParams {
            container: Some(collector::NAME.to_string()),
            limit_bytes: i64::try_from(log_cap(collector.max_bytes)).ok(),
            ..LogParams::default()
        };
        let api: Api<Pod> = Api::namespaced(self.client.clone(), ns);
        let stream = api
            .log_stream(name, &lp)
            .await
            .map_err(|source| KubeError::api("read the results of", name.to_string(), source))?;
        let mut lines = stream.lines();
        let mut reader = Reader::new(collector.max_bytes);
        let mut damage = None;
        while let Some(line) = lines.next().await {
            let read = match line {
                Ok(line) => reader.line(&line).map_err(|e| e.to_string()),
                Err(e) => Err(format!("reading the collector's log: {e}")),
            };
            if let Err(e) = read {
                damage = Some(e);
                break;
            }
            if reader.ended() || (!read_all && reader.listed()) {
                break;
            }
        }

        body["files"] = json!(reader.files);
        body["unread"] = json!(reader.unread);
        body["total_bytes"] = json!(reader.files.iter().map(|f| f.bytes).sum::<u64>());
        let failed = |body: &mut Value, error: String| {
            body["state"] = json!("failed");
            body["error"] = json!(error);
        };
        match (&reader.state, damage) {
            (_, Some(error)) => failed(&mut body, error),
            (State::Withheld(reason), _) => {
                body["state"] = json!("withheld");
                body["note"] = json!(format!(
                    "{reason}; push what is larger to a registry and pass on its location"
                ));
            }
            (State::Failed(error), _) => {
                failed(&mut body, format!("the collector stopped: {error}"))
            }
            (State::Listing, _) => failed(
                &mut body,
                format!(
                    "the collector exited {} before it listed the directory",
                    exit.exit_code
                ),
            ),
            (State::Archive, _) if read_all => failed(
                &mut body,
                format!(
                    "the collector exited {} before it finished the archive",
                    exit.exit_code
                ),
            ),
            (State::Archive | State::Complete, _) => {
                body["state"] = json!("collected");
                if let Some(wanted) = wanted {
                    match reader.unpack() {
                        Err(e) => failed(&mut body, e.to_string()),
                        Ok(files) => {
                            let budget = args
                                .max_bytes
                                .unwrap_or(DEFAULT_LOG_BUDGET as u64)
                                .min(collector.max_bytes);
                            let (contents, omitted, missing) = select(&files, wanted, budget);
                            body["budget_bytes"] = json!(budget);
                            body["truncated"] = json!(!omitted.is_empty());
                            body["contents"] = json!(contents);
                            body["omitted"] = json!(omitted);
                            body["missing"] = json!(missing);
                        }
                    }
                }
            }
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use crate::server::results::*;

    fn files() -> Vec<(String, Vec<u8>)> {
        vec![
            ("a.json".to_string(), br#"{"p50":1}"#.to_vec()),
            ("b.bin".to_string(), vec![0xff, 0xfe, 0x00]),
            ("c.txt".to_string(), b"hello".to_vec()),
        ]
    }

    #[test]
    fn text_comes_back_as_text_and_anything_else_as_base64() {
        let (contents, omitted, missing) = select(&files(), None, 1024);
        assert_eq!(
            contents,
            [
                json!({"path": "a.json", "bytes": 9, "text": "{\"p50\":1}"}),
                json!({"path": "b.bin", "bytes": 3, "base64": "//4A"}),
                json!({"path": "c.txt", "bytes": 5, "text": "hello"}),
            ]
        );
        assert!(omitted.is_empty() && missing.is_empty());
    }

    #[test]
    fn a_file_past_the_budget_is_named_and_not_returned() {
        let (contents, omitted, _) = select(&files(), None, 9);
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["path"], "a.json");
        assert_eq!(
            omitted,
            [
                json!({"path": "b.bin", "bytes": 3}),
                json!({"path": "c.txt", "bytes": 5})
            ]
        );
        let (contents, omitted, _) = select(&files(), None, 13);
        assert_eq!(
            contents.iter().map(|c| &c["path"]).collect::<Vec<_>>(),
            ["a.json", "b.bin"]
        );
        assert_eq!(omitted, [json!({"path": "c.txt", "bytes": 5})]);
    }

    #[test]
    fn only_the_named_paths_are_returned_and_unknown_ones_are_missing() {
        let wanted = ["c.txt".to_string(), "nope".to_string()];
        let (contents, omitted, missing) = select(&files(), Some(&wanted), 1024);
        assert_eq!(
            contents,
            [json!({"path": "c.txt", "bytes": 5, "text": "hello"})]
        );
        assert!(omitted.is_empty());
        assert_eq!(missing, ["nope"]);
    }

    #[test]
    fn a_job_was_planned_with_results_when_it_carries_the_collector() {
        let with = json!({"spec": {"template": {"spec": {"initContainers": [
            {"name": "setup"}, {"name": "crucible-results"}]}}}});
        let without =
            json!({"spec": {"template": {"spec": {"initContainers": [{"name": "setup"}]}}}});
        assert!(planned_with_results(&with));
        assert!(!planned_with_results(&without));
        assert!(!planned_with_results(&json!({})));
    }

    #[test]
    fn the_collector_has_ended_once_its_container_terminated() {
        let pod = |state: Value| -> Pod {
            serde_json::from_value(json!({
                "metadata": {"name": "p"},
                "status": {"initContainerStatuses": [
                    {"name": "setup", "image": "i", "imageID": "", "ready": true, "restartCount": 0,
                     "state": {"terminated": {"exitCode": 0}}},
                    {"name": "crucible-results", "image": "i", "imageID": "", "ready": true,
                     "restartCount": 0, "state": state},
                ]},
            }))
            .unwrap()
        };
        assert!(ended(&pod(json!({"running": {}}))).is_none());
        assert_eq!(
            ended(&pod(json!({"terminated": {"exitCode": 137}}))).map(|t| t.exit_code),
            Some(137)
        );
    }
}
