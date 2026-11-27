//! Matching the Pipelines-as-Code PipelineRuns a commit started.

use std::collections::BTreeMap;

use crate::commit::CommitId;
use crate::tekton::model::PipelineRun;

pub const SHA_LABEL: &str = "pipelinesascode.tekton.dev/sha";
pub const REPOSITORY_LABEL: &str = "pipelinesascode.tekton.dev/repository";
pub const EVENT_LABEL: &str = "pipelinesascode.tekton.dev/event-type";
pub const PULL_REQUEST_LABEL: &str = "pipelinesascode.tekton.dev/pull-request";
pub const ORIGINAL_NAME_LABEL: &str = "pipelinesascode.tekton.dev/original-prname";
pub const LOG_URL_ANNOTATION: &str = "pipelinesascode.tekton.dev/log-url";
pub const RESULTS_RECORD_ANNOTATION: &str = "results.tekton.dev/record";

/// Which Pipelines-as-Code events a watch follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Push,
    PullRequest,
}

/// What a watch asks for: the runs for one commit of one Pipelines-as-Code Repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filter {
    pub commit: CommitId,
    pub repository: String,
    pub event: Option<Event>,
}

impl Filter {
    pub fn selector(&self) -> String {
        let mut terms = vec![
            format!("{SHA_LABEL}={}", self.commit),
            format!("{REPOSITORY_LABEL}={}", self.repository),
        ];
        match self.event {
            Some(Event::Push) => terms.push(format!("{EVENT_LABEL}=push")),
            Some(Event::PullRequest) => terms.push(PULL_REQUEST_LABEL.to_owned()),
            None => {}
        }
        terms.join(",")
    }

    pub fn admits(&self, run: &PipelineRun) -> bool {
        let labels = &run.metadata.labels;
        let label = |key: &str| labels.get(key).map(String::as_str);
        let event = match self.event {
            None => true,
            Some(Event::Push) => label(EVENT_LABEL) == Some("push"),
            Some(Event::PullRequest) => labels.contains_key(PULL_REQUEST_LABEL),
        };
        label(SHA_LABEL) == Some(self.commit.as_str())
            && label(REPOSITORY_LABEL) == Some(self.repository.as_str())
            && event
    }
}

pub fn run_name(run: &PipelineRun) -> &str {
    run.metadata
        .labels
        .get(ORIGINAL_NAME_LABEL)
        .map(String::as_str)
        .unwrap_or(&run.metadata.name)
}

pub fn latest_by_name(runs: &[PipelineRun]) -> BTreeMap<String, &PipelineRun> {
    let mut latest: BTreeMap<String, &PipelineRun> = BTreeMap::new();
    for run in runs {
        let key = run_name(run).to_owned();
        let newer = match latest.get(&key) {
            None => true,
            Some(held) => attempt_order(run) > attempt_order(held),
        };
        if newer {
            latest.insert(key, run);
        }
    }
    latest
}

fn attempt_order(run: &PipelineRun) -> (Option<jiff::Timestamp>, &str) {
    let created = run
        .metadata
        .creation_timestamp
        .as_deref()
        .and_then(|t| t.parse::<jiff::Timestamp>().ok());
    (created, run.metadata.name.as_str())
}

#[cfg(test)]
mod tests {
    use crate::commit::CommitId;
    use crate::tekton::find::{Event, Filter, latest_by_name, run_name};
    use crate::tekton::model::{List, Meta, PipelineRun};

    const OPENSHELL: &str = "6a7acf93315fcccf66676784f1eb1ab1b3dae12a";
    const MLSERVER: &str = "df2bd6a9d89c7046fb053266ec876175a5328737";

    fn fixture() -> Vec<PipelineRun> {
        let raw = include_str!("../../testdata/tekton/pipelineruns.json");
        let list: List<PipelineRun> = serde_json::from_str(raw).expect("fixture parses");
        list.items
    }

    fn filter(commit: &str, repository: &str, event: Option<Event>) -> Filter {
        Filter {
            commit: CommitId::parse(commit).expect("full commit"),
            repository: repository.to_owned(),
            event,
        }
    }

    fn matched(f: &Filter) -> Vec<String> {
        fixture()
            .iter()
            .filter(|r| f.admits(r))
            .map(|r| run_name(r).to_owned())
            .collect()
    }

    fn run(name: &str, original: &str, created: &str) -> PipelineRun {
        let mut meta = Meta {
            name: name.to_owned(),
            creation_timestamp: Some(created.to_owned()),
            ..Meta::default()
        };
        meta.labels.insert(
            "pipelinesascode.tekton.dev/original-prname".into(),
            original.into(),
        );
        PipelineRun {
            metadata: meta,
            status: None,
        }
    }

    #[test]
    fn the_selector_names_commit_repository_and_event() {
        let push = filter(OPENSHELL, "openshell", Some(Event::Push));
        assert_eq!(
            push.selector(),
            format!(
                "pipelinesascode.tekton.dev/sha={OPENSHELL},\
                 pipelinesascode.tekton.dev/repository=openshell,\
                 pipelinesascode.tekton.dev/event-type=push"
            )
        );
        assert!(
            filter(OPENSHELL, "openshell", Some(Event::PullRequest))
                .selector()
                .ends_with(",pipelinesascode.tekton.dev/pull-request")
        );
        assert_eq!(
            filter(OPENSHELL, "openshell", None)
                .selector()
                .matches(',')
                .count(),
            1
        );
    }

    #[test]
    fn a_push_matches_every_run_it_started_and_nothing_else() {
        let repo = "github-com-opendatahub-io-openshell";
        let expected = [
            "odh-openshell-cli-on-push",
            "odh-openshell-e2e-on-push",
            "odh-openshell-gateway-on-push",
            "odh-openshell-sandbox-on-push",
            "odh-openshell-sandbox-openclaw-on-push",
            "odh-openshell-supervisor-on-push",
        ];
        assert_eq!(
            matched(&filter(OPENSHELL, repo, Some(Event::Push))),
            expected
        );
        assert_eq!(matched(&filter(OPENSHELL, repo, None)), expected);
        assert!(matched(&filter(OPENSHELL, repo, Some(Event::PullRequest))).is_empty());
        assert!(matched(&filter(OPENSHELL, "mlserver", None)).is_empty());
        assert!(matched(&filter(MLSERVER, repo, None)).is_empty());
    }

    #[test]
    fn a_pull_request_watch_includes_the_runs_its_comments_started() {
        let runs = matched(&filter(MLSERVER, "mlserver", Some(Event::PullRequest)));
        assert_eq!(
            runs,
            ["early-gate-ci-build", "odh-mlserver-cuda-on-pull-request"]
        );
        assert!(matched(&filter(MLSERVER, "mlserver", Some(Event::Push))).is_empty());
    }

    #[test]
    fn a_later_attempt_replaces_an_earlier_one_under_the_same_name() {
        let runs = vec![
            run("build-a", "build", "2026-11-26T00:16:25Z"),
            run("build-b", "build", "2026-11-26T01:00:00Z"),
            run("test-a", "test", "2026-11-26T00:16:25Z"),
        ];
        let latest = latest_by_name(&runs);
        let picked: Vec<(&str, &str)> = latest
            .iter()
            .map(|(k, v)| (k.as_str(), v.metadata.name.as_str()))
            .collect();
        assert_eq!(picked, [("build", "build-b"), ("test", "test-a")]);
        let reversed: Vec<PipelineRun> = runs.into_iter().rev().collect();
        assert_eq!(
            latest_by_name(&reversed)
                .get("build")
                .map(|r| r.metadata.name.as_str()),
            Some("build-b")
        );
    }

    #[test]
    fn a_run_without_an_original_name_goes_by_its_own() {
        let unlabelled = PipelineRun {
            metadata: Meta {
                name: "hand-made".to_owned(),
                ..Meta::default()
            },
            status: None,
        };
        assert_eq!(run_name(&unlabelled), "hand-made");
    }
}
