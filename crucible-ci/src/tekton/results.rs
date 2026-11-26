//! Tekton Results, which keeps PipelineRuns and TaskRuns after the cluster prunes them. A
//! record's id is the run's uid, and a PipelineRun's result holds its TaskRuns.
//!
//! ```text
//!   GET {base}/apis/results.tekton.dev/v1alpha2/parents/{ns}/results/-/records
//!         ?filter=data_type=="tekton.dev/v1.PipelineRun" && data.metadata.labels[sha]=="…"
//!   GET {base}/…/parents/{ns}/results/{run uid}/records?filter=data_type=="tekton.dev/v1.TaskRun"
//! ```

use base64::Engine;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use url::Url;

use crate::tekton::find::{Filter, REPOSITORY_LABEL, SHA_LABEL};
use crate::tekton::model::{PipelineRun, TaskRun};
use crate::tekton::{Namespace, TektonError};
use crate::token::BearerToken;

const PIPELINE_RUN_TYPE: &str = "tekton.dev/v1.PipelineRun";
const TASK_RUN_TYPE: &str = "tekton.dev/v1.TaskRun";
const PAGE_SIZE: u32 = 50;
pub const MAX_PAGES: u32 = 10;
const ERR_BODY_CHARS: usize = 300;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Records {
    #[serde(default)]
    records: Vec<Record>,
    #[serde(default)]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct Record {
    data: RecordData,
}

#[derive(Deserialize)]
struct RecordData {
    #[serde(rename = "type")]
    kind: String,
    value: String,
}

/// A cluster's Tekton Results API, reached with the caller's client.
#[derive(Clone, Debug)]
pub struct Results {
    http: reqwest::Client,
    base: String,
}

impl Results {
    pub fn new(http: reqwest::Client, base: &str) -> Result<Self, TektonError> {
        let url = Url::parse(base).map_err(|_| TektonError::Api(base.to_owned()))?;
        if url.scheme() != "https" || url.host_str().is_none() {
            return Err(TektonError::Api(base.to_owned()));
        }
        Ok(Results {
            http,
            base: base.trim_end_matches('/').to_owned(),
        })
    }

    pub async fn find(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        filter: &Filter,
    ) -> Result<Vec<PipelineRun>, TektonError> {
        let cel = format!(
            "data_type==\"{PIPELINE_RUN_TYPE}\" && \
             data.metadata.labels[\"{SHA_LABEL}\"]==\"{}\" && \
             data.metadata.labels[\"{REPOSITORY_LABEL}\"]==\"{}\"",
            filter.commit,
            cel_string(&filter.repository),
        );
        let runs: Vec<PipelineRun> = self
            .records(token, ns, "-", &cel, PIPELINE_RUN_TYPE)
            .await?;
        Ok(runs.into_iter().filter(|r| filter.admits(r)).collect())
    }

    pub async fn task_runs(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        run_uid: &str,
    ) -> Result<Vec<TaskRun>, TektonError> {
        let cel = format!("data_type==\"{TASK_RUN_TYPE}\"");
        self.records(token, ns, run_uid, &cel, TASK_RUN_TYPE).await
    }

    async fn records<T: DeserializeOwned>(
        &self,
        token: &BearerToken,
        ns: &Namespace,
        result: &str,
        cel: &str,
        kind: &str,
    ) -> Result<Vec<T>, TektonError> {
        let doing = format!(
            "reading Tekton Results records of {result} in {}",
            ns.as_str()
        );
        let base = format!(
            "{}/apis/results.tekton.dev/v1alpha2/parents/{}/results/{}/records",
            self.base,
            ns.as_str(),
            result
        );
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut url = Url::parse(&base).map_err(|_| TektonError::Api(base.clone()))?;
            {
                let mut q = url.query_pairs_mut();
                q.append_pair("page_size", &PAGE_SIZE.to_string());
                q.append_pair("filter", cel);
                if let Some(next) = &page {
                    q.append_pair("page_token", next);
                }
            }
            let response = self
                .http
                .get(url)
                .bearer_auth(token.expose())
                .send()
                .await
                .map_err(|source| TektonError::Http {
                    doing: doing.clone(),
                    source,
                })?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(TektonError::Status {
                    doing,
                    status: status.as_u16(),
                    body: body.chars().take(ERR_BODY_CHARS).collect(),
                });
            }
            let records: Records = response.json().await.map_err(|source| TektonError::Http {
                doing: doing.clone(),
                source,
            })?;
            out.extend(decode(&records.records, kind, &doing)?);
            match records.next_page_token.filter(|t| !t.is_empty()) {
                Some(next) => page = Some(next),
                None => return Ok(out),
            }
        }
        Ok(out)
    }
}

fn decode<T: DeserializeOwned>(
    records: &[Record],
    kind: &str,
    doing: &str,
) -> Result<Vec<T>, TektonError> {
    records
        .iter()
        .filter(|r| r.data.kind == kind)
        .map(|r| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&r.data.value)
                .map_err(|e| TektonError::Body {
                    doing: doing.to_owned(),
                    message: format!("a record is not base64: {e}"),
                })?;
            serde_json::from_slice(&bytes).map_err(|e| TektonError::Body {
                doing: doing.to_owned(),
                message: format!("a record is not a {kind}: {e}"),
            })
        })
        .collect()
}

fn cel_string(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use crate::tekton::graph::run_graph;
    use crate::tekton::model::{PipelineRun, TaskRun};
    use crate::tekton::outcome::{RunState, Verdict, run_state};
    use crate::tekton::results::{PIPELINE_RUN_TYPE, Records, TASK_RUN_TYPE, cel_string, decode};

    fn pipeline_runs() -> Vec<PipelineRun> {
        let raw = include_str!("../../testdata/tekton/results-pipelineruns.json");
        let records: Records = serde_json::from_str(raw).expect("records parse");
        decode(&records.records, PIPELINE_RUN_TYPE, "test").expect("records decode")
    }

    fn task_runs() -> Vec<TaskRun> {
        let raw = include_str!("../../testdata/tekton/results-taskruns-modelexpress-pr.json");
        let records: Records = serde_json::from_str(raw).expect("records parse");
        decode(&records.records, TASK_RUN_TYPE, "test").expect("records decode")
    }

    #[test]
    fn a_pruned_pull_request_reads_back_with_its_digests() {
        let runs = pipeline_runs();
        let names: Vec<&str> = runs.iter().map(|r| r.metadata.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("odh-modelexpress-on-pull-request-"))
        );
        assert!(
            names
                .iter()
                .any(|n| n.starts_with("odh-modelexpress-operator-on-pull-request-"))
        );
        for run in &runs {
            let RunState::Ended(ended) = run_state(run) else {
                panic!("{} ended before it was pruned", run.metadata.name);
            };
            assert_eq!(ended.verdict, Verdict::Passed);
            assert!(
                ended
                    .results
                    .get("IMAGE_DIGEST")
                    .and_then(|v| v.as_str())
                    .is_some_and(|d| d.starts_with("sha256:"))
            );
        }
    }

    #[test]
    fn a_pruned_run_draws_from_its_recorded_task_runs() {
        let run = pipeline_runs()
            .into_iter()
            .find(|r| r.metadata.uid.as_deref() == Some("e8f34e01-a625-485a-8e57-c99cd9b46871"))
            .expect("the server run is recorded");
        let trs = task_runs();
        assert!(!trs.is_empty());
        let g = run_graph(&run, &trs);
        let st = run.status.as_ref().expect("status");
        assert_eq!(
            g.graph.nodes.len(),
            st.child_references.len() + st.skipped_tasks.len()
        );
        assert_eq!(g.tasks.len(), st.child_references.len());
    }

    #[test]
    fn a_repository_name_cannot_break_out_of_the_cel_string() {
        assert_eq!(cel_string(r#"a" || true || "b"#), r#"a\" || true || \"b"#);
        assert_eq!(cel_string(r"a\"), r"a\\");
    }
}
