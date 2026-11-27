//! The web pages a Pipelines-as-Code PipelineRun names: its commit and its pull or merge request.

use crucible_contract::link::ExternalUrl;

use crate::tekton::find::PULL_REQUEST_LABEL;
use crate::tekton::model::PipelineRun;

pub const SHA_URL_ANNOTATION: &str = "pipelinesascode.tekton.dev/sha-url";
pub const REPO_URL_ANNOTATION: &str = "pipelinesascode.tekton.dev/repo-url";
pub const GIT_PROVIDER_ANNOTATION: &str = "pipelinesascode.tekton.dev/git-provider";

fn meta<'a>(run: &'a PipelineRun, key: &str) -> Option<&'a str> {
    run.metadata
        .annotations
        .get(key)
        .or_else(|| run.metadata.labels.get(key))
        .map(String::as_str)
        .filter(|v| !v.is_empty())
}

pub fn commit_url(run: &PipelineRun) -> Option<ExternalUrl> {
    ExternalUrl::new(meta(run, SHA_URL_ANNOTATION)?).ok()
}

pub fn change_url(run: &PipelineRun) -> Option<ExternalUrl> {
    let number = meta(run, PULL_REQUEST_LABEL)?;
    if !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let repo = meta(run, REPO_URL_ANNOTATION)?.trim_end_matches('/');
    let path = match meta(run, GIT_PROVIDER_ANNOTATION) {
        Some("gitlab") => "-/merge_requests",
        Some("github") | None => "pull",
        Some(_) => return None,
    };
    ExternalUrl::new(format!("{repo}/{path}/{number}")).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::tekton::links::{change_url, commit_url};
    use crate::tekton::model::PipelineRun;

    fn run(annotations: serde_json::Value, labels: serde_json::Value) -> PipelineRun {
        serde_json::from_value(json!({
            "metadata": {"name": "r", "annotations": annotations, "labels": labels},
        }))
        .unwrap()
    }

    #[test]
    fn a_run_names_its_commit_and_its_pull_or_merge_request() {
        let github = run(
            json!({
                "pipelinesascode.tekton.dev/sha-url": "https://github.com/o/r/commit/abc",
                "pipelinesascode.tekton.dev/repo-url": "https://github.com/o/r/",
                "pipelinesascode.tekton.dev/git-provider": "github",
            }),
            json!({"pipelinesascode.tekton.dev/pull-request": "77"}),
        );
        assert_eq!(
            commit_url(&github)
                .map(|u| u.as_str().to_owned())
                .as_deref(),
            Some("https://github.com/o/r/commit/abc")
        );
        assert_eq!(
            change_url(&github)
                .map(|u| u.as_str().to_owned())
                .as_deref(),
            Some("https://github.com/o/r/pull/77")
        );
        let gitlab = run(
            json!({
                "pipelinesascode.tekton.dev/repo-url": "https://gitlab.example.com/g/p",
                "pipelinesascode.tekton.dev/git-provider": "gitlab",
                "pipelinesascode.tekton.dev/pull-request": "5",
            }),
            json!({}),
        );
        assert_eq!(
            change_url(&gitlab)
                .map(|u| u.as_str().to_owned())
                .as_deref(),
            Some("https://gitlab.example.com/g/p/-/merge_requests/5")
        );
        let push = run(
            json!({"pipelinesascode.tekton.dev/repo-url": "https://github.com/o/r"}),
            json!({}),
        );
        assert!(change_url(&push).is_none());
        assert!(commit_url(&push).is_none());
        let odd = run(
            json!({
                "pipelinesascode.tekton.dev/repo-url": "https://x.example.com/o/r",
                "pipelinesascode.tekton.dev/git-provider": "bitbucket-cloud",
            }),
            json!({"pipelinesascode.tekton.dev/pull-request": "1"}),
        );
        assert!(change_url(&odd).is_none());
        let junk = run(
            json!({"pipelinesascode.tekton.dev/repo-url": "https://github.com/o/r"}),
            json!({"pipelinesascode.tekton.dev/pull-request": "1/../x"}),
        );
        assert!(change_url(&junk).is_none());
    }
}
