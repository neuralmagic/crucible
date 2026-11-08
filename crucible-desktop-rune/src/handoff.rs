//! A failed run, written up for a coding agent: what failed, where the pack is, and the `crux`
//! commands that save a fix and run again from the failure.

use std::fmt::Write;
use std::path::{Path, PathBuf};

/// Log lines a brief carries, newest last.
pub const LOG_LINES: usize = 60;

/// Where the fix goes: the draft the run came from, or a new draft of the registered playbook.
#[derive(Clone, Debug, PartialEq)]
pub enum Fix {
    Draft { draft: String, pulled: bool },
    Registered { playbook: String, draft: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Handoff {
    pub playbook: String,
    pub key: String,
    pub run: Option<String>,
    pub tracker: String,
    /// The task the brief is about; `None` is the whole run.
    pub task: Option<String>,
    /// Why the launch stopped, as the controller says it.
    pub why: Option<String>,
    /// Each task that did not pass, with its note.
    pub failed: Vec<(String, String)>,
    pub log: Vec<String>,
    /// What crucible-lsp says of the pack, `file:line:col severity: message`.
    pub problems: Vec<String>,
    pub fix: Fix,
    /// Where the pack is, or is pulled to.
    pub dir: PathBuf,
    pub max_cost: f64,
    pub max_time: String,
}

fn shell(path: &Path) -> String {
    let text = path.display().to_string();
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// The brief, as Markdown.
pub fn brief(h: &Handoff) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Fix a failed crucible playbook run\n");
    let run = h
        .run
        .as_deref()
        .map(|run| format!(" (run `{run}`)"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "`{}` launch `{}`{run} did not pass. It is open in the Crucible Playbooks tracker: {}\n",
        h.playbook, h.key, h.tracker
    );

    let _ = writeln!(out, "## What failed\n");
    if let Some(why) = &h.why {
        let _ = writeln!(out, "- The launch: {why}");
    }
    for (task, note) in &h.failed {
        let mark = if h.task.as_deref() == Some(task.as_str()) {
            " (this brief is about it)"
        } else {
            ""
        };
        let note = if note.is_empty() { "no note" } else { note };
        let _ = writeln!(out, "- `{task}`{mark}: {note}");
    }
    if h.why.is_none() && h.failed.is_empty() {
        let _ = writeln!(out, "- Nothing recorded a failure; read the log below.");
    }

    if !h.log.is_empty() {
        let scope = h
            .task
            .as_deref()
            .map(|task| format!(" from `{task}`"))
            .unwrap_or_default();
        let _ = writeln!(out, "\n## Last log lines{scope}\n\n```text");
        for line in &h.log {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out, "```");
    }

    if !h.problems.is_empty() {
        let _ = writeln!(out, "\n## crucible-lsp on the pack\n");
        for problem in &h.problems {
            let _ = writeln!(out, "- {problem}");
        }
    }

    let dir = shell(&h.dir);
    let from = h
        .task
        .as_deref()
        .map(|task| format!(" --from {task}"))
        .unwrap_or_default();
    let caps = format!("--max-cost {:.2} --max-time {}", h.max_cost, h.max_time);
    let _ = writeln!(out, "\n## Fix it\n");
    match &h.fix {
        Fix::Draft { draft, pulled } => {
            if *pulled {
                let _ = writeln!(
                    out,
                    "1. The pack is draft `{draft}`, checked out at {dir}. Edit it there."
                );
            } else {
                let _ = writeln!(
                    out,
                    "1. The pack is draft `{draft}`. Pull it: `crux draft-pull {draft} {dir}`"
                );
            }
            let _ = writeln!(
                out,
                "2. Find the cause in the pack files and fix it. Keep the change to what the failure needs.\n\
                 3. Save the directory as the next version: `crux draft-push {draft} {dir} --base-version <n>`, \
                 where `<n>` is the version `crux draft-files {draft}` prints first. A refused save means \
                 someone saved since: pull again, reapply the fix, push.\n\
                 4. Run again from the failure, reusing what passed: \
                 `crux draft-launch {draft} {caps}{}{from}`. It prints the new launch's tracker link.",
                h.run
                    .as_deref()
                    .map(|run| format!(" --rerun-of {run}"))
                    .unwrap_or_default()
            );
        }
        Fix::Registered { playbook, draft } => {
            let _ = writeln!(
                out,
                "1. `{playbook}` is a registered playbook, so the fix goes in a draft of it: \
                 `crux draft-create {draft} --description 'Fix {playbook}' --template {playbook}` \
                 (skip if it exists), then `crux draft-pull {draft} {dir}`.\n\
                 2. Find the cause in the pack files and fix it. Keep the change to what the failure needs.\n\
                 3. Save: `crux draft-push {draft} {dir} --base-version <n>`, where `<n>` is the version \
                 `crux draft-files {draft}` prints first.\n\
                 4. Launch the draft: `crux draft-launch {draft} {caps}`. A registered playbook's run can't \
                 be rerun as a draft, so this runs from the start. It prints the launch's tracker link."
            );
        }
    }
    let _ = writeln!(
        out,
        "5. Give me the tracker link it printed on a line of its own, and open it with `open '<link>'` \
         so I can watch the run."
    );
    out.trim_end().to_string() + "\n"
}

/// Write `brief` for launch `key` under `root`, named so a task's brief sits beside the run's.
pub fn save(root: &Path, key: &str, task: Option<&str>, brief: &str) -> std::io::Result<PathBuf> {
    let name: String = format!("{key}{}", task.map(|t| format!("-{t}")).unwrap_or_default())
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::fs::create_dir_all(root)?;
    let path = root.join(format!("{name}.md"));
    std::fs::write(&path, brief)?;
    Ok(path)
}

/// Where briefs go: `$CRUCIBLE_HANDOFF_DIR`, else the app's cache.
pub fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CRUCIBLE_HANDOFF_DIR") {
        return Some(PathBuf::from(dir));
    }
    Some(crate::httpcache::cache_root()?.join("handoff"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::handoff::{Fix, Handoff, brief, save};

    fn failed() -> Handoff {
        Handoff {
            playbook: "rune-rerun-smoke".into(),
            key: "playbook:rune-rerun-smoke:01a1".into(),
            run: Some("playbook_rune-rerun-smoke_01a1-1".into()),
            tracker: "crucible-playbooks://playbook/rune-rerun-smoke/launch/playbook%3Arune-rerun-smoke%3A01a1?task=two".into(),
            task: Some("two".into()),
            why: Some("run errored: short-circuited at two".into()),
            failed: vec![("two".into(), "exit 1: two is broken".into())],
            log: vec!["TASK two fail".into()],
            problems: vec!["workflow.star:2:1 warning: unused".into()],
            fix: Fix::Draft {
                draft: "rune-rerun-smoke".into(),
                pulled: true,
            },
            dir: PathBuf::from("/Users/me/Library/Application Support/crucible-playbooks/drafts/rune-rerun-smoke"),
            max_cost: 1.0,
            max_time: "10m".into(),
        }
    }

    #[test]
    fn a_draft_runs_brief_says_what_failed_and_how_to_rerun_from_it() {
        let text = brief(&failed());
        for want in [
            "`rune-rerun-smoke` launch `playbook:rune-rerun-smoke:01a1` (run `playbook_rune-rerun-smoke_01a1-1`)",
            "crucible-playbooks://playbook/rune-rerun-smoke/launch/",
            "- The launch: run errored: short-circuited at two",
            "- `two` (this brief is about it): exit 1: two is broken",
            "```text\nTASK two fail\n```",
            "- workflow.star:2:1 warning: unused",
            "checked out at '/Users/me/Library/Application Support/crucible-playbooks/drafts/rune-rerun-smoke'",
            "crux draft-push rune-rerun-smoke '/Users/me/Library/Application Support/crucible-playbooks/drafts/rune-rerun-smoke' --base-version <n>",
            "crux draft-launch rune-rerun-smoke --max-cost 1.00 --max-time 10m --rerun-of playbook_rune-rerun-smoke_01a1-1 --from two",
            "open '<link>'",
        ] {
            assert!(text.contains(want), "missing {want:?} in:\n{text}");
        }
    }

    #[test]
    fn a_registered_runs_brief_drafts_from_it_and_launches_fresh() {
        let mut h = failed();
        h.fix = Fix::Registered {
            playbook: "triage".into(),
            draft: "triage-weaton".into(),
        };
        h.task = None;
        let text = brief(&h);
        assert!(text.contains("--template triage"), "{text}");
        assert!(
            text.contains("crux draft-launch triage-weaton --max-cost 1.00 --max-time 10m`"),
            "{text}"
        );
        assert!(!text.contains("--rerun-of"), "{text}");
        assert!(!text.contains("this brief is about it"), "{text}");
    }

    #[test]
    fn an_unpulled_draft_is_pulled_first_and_a_path_with_a_quote_stays_one_argument() {
        let mut h = failed();
        h.fix = Fix::Draft {
            draft: "d".into(),
            pulled: false,
        };
        h.dir = PathBuf::from("/tmp/it's here");
        let text = brief(&h);
        assert!(
            text.contains(r"`crux draft-pull d '/tmp/it'\''s here'`"),
            "{text}"
        );
    }

    #[test]
    fn briefs_save_under_a_safe_name() {
        let root = tempfile::tempdir().unwrap();
        let path = save(root.path(), "playbook:a:1", Some("edit[x]"), "hi").unwrap();
        assert_eq!(path, root.path().join("playbook_a_1-edit_x_.md"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "hi");
    }
}
