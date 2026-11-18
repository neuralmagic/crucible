//! The Issues page's choices: which statuses it lists and what it calls an issue's source.

use crux::dto::{InputKind, Issue};

/// The statuses an issue moves through, as the controller spells them.
pub const STATUSES: [&str; 8] = [
    "new",
    "scoped",
    "awaiting-approval",
    "building",
    "running",
    "pr-open",
    "parked",
    "done",
];

/// The status the page lists first.
pub const FIRST: &str = "parked";

/// How many issues the list shows.
pub const SHOWN: usize = 200;

/// Where an issue came from, as its tracker names it.
pub fn source(issue: &Issue) -> String {
    match &issue.kind {
        InputKind::Github {
            owner,
            repo,
            number,
        } => format!("{owner}/{repo}#{number}"),
        InputKind::Scenario { id } => format!("scenario {id}"),
        InputKind::Jira {
            project, number, ..
        } => format!("{project}-{number}"),
        InputKind::Unknown { tag } => tag.clone(),
        InputKind::Other => issue.key.clone(),
    }
}

/// The newest `SHOWN` of `issues`, newest first, and how many there are in all.
pub fn newest(issues: &[Issue]) -> (Vec<&Issue>, usize) {
    let mut sorted: Vec<&Issue> = issues.iter().collect();
    sorted.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.key.cmp(&b.key)));
    sorted.truncate(SHOWN);
    (sorted, issues.len())
}

#[cfg(test)]
mod tests {
    use crux::dto::Issue;

    use crate::issues::{SHOWN, newest, source};

    fn issue(key: &str, kind: &str, updated_at: &str) -> Issue {
        serde_json::from_str(&format!(
            r#"{{"key":"{key}","kind":{kind},"status":"parked","updated_at":"{updated_at}"}}"#
        ))
        .unwrap()
    }

    #[test]
    fn a_source_reads_as_its_tracker_names_it() {
        let github = r#"{"type":"github","owner":"vllm-project","repo":"vllm","number":12}"#;
        let jira = r#"{"type":"jira","site":"example","project":"INFERENG","number":7}"#;
        let named = [
            (github, "vllm-project/vllm#12"),
            (jira, "INFERENG-7"),
            (
                r#"{"type":"scenario","id":"cold-start"}"#,
                "scenario cold-start",
            ),
            (r#"{"type":"something-new"}"#, "the-key"),
        ];
        for (kind, expected) in named {
            assert_eq!(source(&issue("the-key", kind, "")), expected, "{kind}");
        }
    }

    #[test]
    fn the_newest_come_first_and_the_list_is_capped() {
        let kind = r#"{"type":"scenario","id":"s"}"#;
        let issues: Vec<Issue> = (0..SHOWN + 5)
            .map(|n| {
                issue(
                    &format!("k{n:04}"),
                    kind,
                    &format!("2026-11-10T00:00:{:02}Z", n % 60),
                )
            })
            .collect();
        let (shown, total) = newest(&issues);
        assert_eq!((shown.len(), total), (SHOWN, SHOWN + 5));
        assert_eq!(shown[0].updated_at, "2026-11-10T00:00:59Z");
        assert_eq!(
            (shown[0].key.as_str(), shown[1].key.as_str()),
            ("k0059", "k0119")
        );
    }
}
