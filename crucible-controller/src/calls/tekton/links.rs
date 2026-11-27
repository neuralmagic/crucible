//! The pages a watch names: each matched PipelineRun's, its commit's and its pull or merge
//! request's, and the one its frame opens.

use std::collections::BTreeMap;

use crucible_ci::tekton::graph::run_link;
use crucible_ci::tekton::links::{change_url, commit_url};
use crucible_ci::tekton::model::PipelineRun;
use crucible_contract::calls::{NamedLink, NodeLink};

pub(crate) const COMMIT: &str = "Commit";
pub(crate) const CHANGE: &str = "Pull request";

fn external(link: Option<NodeLink>) -> Option<crucible_contract::link::ExternalUrl> {
    match link? {
        NodeLink::External { url } => Some(url),
        NodeLink::Run { .. } => None,
    }
}

pub(crate) fn watch_links(
    matched: &BTreeMap<String, String>,
    candidates: &[PipelineRun],
    known: &[NamedLink],
) -> (Vec<NamedLink>, Option<NodeLink>) {
    let mut by_label: BTreeMap<String, NamedLink> =
        known.iter().map(|l| (l.label.clone(), l.clone())).collect();
    let mut put = |label: &str, url| {
        by_label.insert(
            label.to_owned(),
            NamedLink {
                label: label.to_owned(),
                url,
            },
        );
    };
    for (name, pipeline_run) in matched {
        let Some(run) = candidates.iter().find(|r| &r.metadata.name == pipeline_run) else {
            continue;
        };
        if let Some(url) = external(run_link(run)) {
            put(name, url);
        }
        if let Some(url) = commit_url(run) {
            put(COMMIT, url);
        }
        if let Some(url) = change_url(run) {
            put(CHANGE, url);
        }
    }
    let frame = match matched.keys().collect::<Vec<_>>().as_slice() {
        [only] => by_label.get(*only),
        _ => by_label.get(CHANGE).or_else(|| by_label.get(COMMIT)),
    }
    .map(|l| NodeLink::External { url: l.url.clone() });
    let runs = matched
        .keys()
        .filter_map(|name| by_label.get(name).cloned());
    let rest = [CHANGE, COMMIT]
        .into_iter()
        .filter_map(|label| by_label.get(label).cloned());
    (runs.chain(rest).collect(), frame)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crucible_ci::tekton::model::PipelineRun;
    use crucible_contract::calls::NodeLink;
    use serde_json::json;

    use crate::calls::tekton::links::*;

    fn run(name: &str) -> PipelineRun {
        serde_json::from_value(json!({
            "metadata": {
                "name": name,
                "namespace": "tenant",
                "labels": {"pipelinesascode.tekton.dev/pull-request": "77"},
                "annotations": {
                    "pipelinesascode.tekton.dev/log-url": format!("https://ui.example.com/ns/tenant/pipelinerun/{name}"),
                    "pipelinesascode.tekton.dev/sha-url": "https://github.com/o/r/commit/abc",
                    "pipelinesascode.tekton.dev/repo-url": "https://github.com/o/r",
                    "pipelinesascode.tekton.dev/git-provider": "github",
                },
            },
        }))
        .unwrap()
    }

    fn frame_url(frame: Option<NodeLink>) -> String {
        match frame {
            Some(NodeLink::External { url }) => url.as_str().to_owned(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn one_run_opens_its_page_and_several_open_their_pull_request() {
        let one = BTreeMap::from([("server".to_owned(), "server-x1".to_owned())]);
        let (links, frame) = watch_links(&one, &[run("server-x1")], &[]);
        assert_eq!(
            frame_url(frame),
            "https://ui.example.com/ns/tenant/pipelinerun/server-x1"
        );
        let labels: Vec<&str> = links.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, ["server", CHANGE, COMMIT]);

        let two = BTreeMap::from([
            ("operator".to_owned(), "operator-y2".to_owned()),
            ("server".to_owned(), "server-x1".to_owned()),
        ]);
        let (links, frame) = watch_links(&two, &[run("server-x1"), run("operator-y2")], &[]);
        assert_eq!(frame_url(frame), "https://github.com/o/r/pull/77");
        let labels: Vec<&str> = links.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, ["operator", "server", CHANGE, COMMIT]);

        let (kept, frame) = watch_links(&two, &[], &links);
        assert_eq!(kept, links);
        assert_eq!(frame_url(frame), "https://github.com/o/r/pull/77");
        let (none, frame) = watch_links(&BTreeMap::new(), &[], &[]);
        assert!(none.is_empty() && frame.is_none());
    }
}
