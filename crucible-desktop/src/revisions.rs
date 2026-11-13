//! A draft's saves read as changes: which two saves the History pane compares, and the moves
//! that narrow that range to the one save that made a change.

use crux::dto::{DraftDiff, DraftHistoryEntry, FileChange, PlaybookRun};

use crate::live::LaunchStatus;

/// Two saves to compare. `from` is `None` only for the first save, compared against an empty
/// pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Range {
    pub from: Option<i64>,
    pub to: i64,
}

impl Range {
    pub fn label(&self) -> String {
        match self.from {
            Some(from) => format!("v{from} \u{2192} v{}", self.to),
            None => format!("v{}", self.to),
        }
    }

    /// `2..5`, or `5` for one save, as `CRUCIBLE_RANGE` writes it.
    pub fn parse(text: &str) -> Option<Range> {
        match text.split_once("..") {
            Some((from, to)) => Some(Range {
                from: Some(from.trim().parse().ok()?),
                to: to.trim().parse().ok()?,
            }),
            None => Some(Range {
                from: None,
                to: text.trim().parse().ok()?,
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fire {
    pub key: String,
    pub version: i64,
    pub status: LaunchStatus,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Read {
    pub history: Vec<DraftHistoryEntry>,
    pub landed: Vec<Landed>,
    pub fires: Vec<Fire>,
    pub range: Range,
    pub diff: DraftDiff,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Landed {
    pub at: Option<i64>,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub draft: String,
    pub range: Option<Range>,
    pub ran: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Revisions {
    pub wanted: Wanted,
    pub result: Result<Read, String>,
}

/// The draft test-fires among `runs`, newest first as the controller lists them.
pub fn fires(runs: Vec<PlaybookRun>) -> Vec<Fire> {
    runs.into_iter()
        .filter_map(|run| {
            Some(Fire {
                version: run.draft_version?,
                status: LaunchStatus::parse(&run.status, run.parked_reason.as_deref()),
                key: run.key,
            })
        })
        .collect()
}

fn holds(history: &[DraftHistoryEntry], version: i64) -> bool {
    history.iter().any(|entry| entry.version == version)
}

pub fn single(history: &[DraftHistoryEntry], version: i64) -> Range {
    Range {
        from: history
            .iter()
            .find(|entry| entry.version == version)
            .and_then(|entry| entry.previous_version),
        to: version,
    }
}

/// `range` when the draft holds both its saves.
pub fn held(history: &[DraftHistoryEntry], range: Range) -> Option<Range> {
    let from = range.from.is_none_or(|from| holds(history, from));
    (from && holds(history, range.to)).then_some(match range.from {
        Some(_) => range,
        None => single(history, range.to),
    })
}

pub fn verdict(fires: &[Fire], version: i64) -> Option<bool> {
    fires
        .iter()
        .filter(|fire| fire.version == version)
        .find_map(|fire| match fire.status {
            LaunchStatus::Done => Some(true),
            LaunchStatus::Error => Some(false),
            _ => None,
        })
}

/// The range a pane opens on. A run that failed opens on everything saved since a test-fire
/// last passed, which is where its regression is; anything else opens on what its save changed.
/// `None` while the draft has no saves.
pub fn opening(history: &[DraftHistoryEntry], fires: &[Fire], ran: Option<i64>) -> Option<Range> {
    let newest = history.first()?.version;
    let to = ran.filter(|ran| holds(history, *ran)).unwrap_or(newest);
    if ran.is_some() && verdict(fires, to) == Some(false) {
        let passed = history
            .iter()
            .map(|entry| entry.version)
            .filter(|version| *version < to)
            .find(|version| verdict(fires, *version) == Some(true));
        if let Some(passed) = passed {
            return Some(Range {
                from: Some(passed),
                to,
            });
        }
    }
    Some(single(history, to))
}

/// The saves a forward range covers: every version after `from` through `to`, oldest first.
/// Empty for a range that runs backward or compares a save with itself.
pub fn saves_in(history: &[DraftHistoryEntry], range: Range) -> Vec<i64> {
    let mut saves: Vec<i64> = history
        .iter()
        .map(|entry| entry.version)
        .filter(|version| range.from.is_none_or(|from| *version > from) && *version <= range.to)
        .collect();
    saves.sort_unstable();
    saves
}

/// Both halves of a range that covers more than one save, split at its middle save: the older
/// half and the newer one.
pub fn halves(history: &[DraftHistoryEntry], range: Range) -> Option<(Range, Range)> {
    let saves = saves_in(history, range);
    if saves.len() < 2 {
        return None;
    }
    let middle = *saves.get(saves.len().div_ceil(2) - 1)?;
    Some((
        Range {
            from: range.from,
            to: middle,
        },
        Range {
            from: Some(middle),
            to: range.to,
        },
    ))
}

pub fn neighbours(history: &[DraftHistoryEntry], range: Range) -> (Option<Range>, Option<Range>) {
    let mut versions: Vec<i64> = history.iter().map(|entry| entry.version).collect();
    versions.sort_unstable();
    let Some(at) = versions.iter().position(|version| *version == range.to) else {
        return (None, None);
    };
    let step = |to: Option<&i64>| to.map(|to| single(history, *to));
    (
        step(at.checked_sub(1).and_then(|at| versions.get(at))),
        step(versions.get(at + 1)),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    From,
    To,
    Inside,
    Outside,
}

pub fn placement(range: Range, version: i64) -> Placement {
    if version == range.to {
        return Placement::To;
    }
    let Some(from) = range.from else {
        return Placement::Outside;
    };
    if version == from {
        return Placement::From;
    }
    if version > from.min(range.to) && version < from.max(range.to) {
        Placement::Inside
    } else {
        Placement::Outside
    }
}

/// `diff` as one patch for the diff view: each file's unified diff under a `diff --git` line,
/// which is also what marks a file that is not text.
pub fn patch(diff: &DraftDiff) -> String {
    let mut out = String::new();
    for file in &diff.files {
        out.push_str(&format!("diff --git a/{0} b/{0}\n", file.path));
        match (&file.unified, file.change) {
            (Some(unified), _) => out.push_str(unified),
            (None, FileChange::Added) => {
                out.push_str("new file mode 100644\nBinary files differ\n")
            }
            (None, FileChange::Removed) => {
                out.push_str("deleted file mode 100644\nBinary files differ\n");
            }
            (None, FileChange::Modified) => out.push_str("Binary files differ\n"),
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

pub fn totals(diff: &DraftDiff) -> (u64, u64) {
    diff.files.iter().fold((0, 0), |(added, removed), file| {
        (added + file.additions, removed + file.deletions)
    })
}

#[cfg(test)]
mod tests {
    use crate::revisions::*;

    fn entry(version: i64, previous: Option<i64>) -> DraftHistoryEntry {
        serde_json::from_value(serde_json::json!({
            "version": version,
            "previous_version": previous,
            "created_by": "wren",
            "created_at": "2026-08-22T09:00:00Z",
            "schema_digest": "sha256:aa",
            "diagnostics": 0,
            "changes": [],
        }))
        .expect("entry")
    }

    /// Versions 1, 2, 3, 5, 6, 7, newest first, with a gap where a save was never stored.
    fn history() -> Vec<DraftHistoryEntry> {
        vec![
            entry(7, Some(6)),
            entry(6, Some(5)),
            entry(5, Some(3)),
            entry(3, Some(2)),
            entry(2, Some(1)),
            entry(1, None),
        ]
    }

    fn range(from: i64, to: i64) -> Range {
        Range {
            from: Some(from),
            to,
        }
    }

    fn fire(version: i64, status: LaunchStatus) -> Fire {
        Fire {
            key: format!("playbook:studio:{version}"),
            version,
            status,
        }
    }

    #[test]
    fn a_range_reads_and_labels_one_save_or_two() {
        assert_eq!(Range::parse("2..6"), Some(range(2, 6)));
        assert_eq!(Range::parse("6..2"), Some(range(6, 2)));
        assert_eq!(Range::parse("4"), Some(Range { from: None, to: 4 }));
        assert_eq!(Range::parse("two..6"), None);
        assert_eq!(range(2, 6).label(), "v2 \u{2192} v6");
        assert_eq!(Range { from: None, to: 1 }.label(), "v1");
    }

    #[test]
    fn a_picked_range_is_kept_only_while_the_draft_holds_both_saves() {
        let history = history();
        assert_eq!(held(&history, range(2, 6)), Some(range(2, 6)));
        assert_eq!(held(&history, range(6, 2)), Some(range(6, 2)));
        assert_eq!(held(&history, range(4, 6)), None);
        assert_eq!(held(&history, range(2, 9)), None);
        assert_eq!(
            held(&history, Range { from: None, to: 5 }),
            Some(range(3, 5)),
            "one save is compared against the save before it"
        );
        assert_eq!(
            held(&history, Range { from: None, to: 1 }),
            Some(Range { from: None, to: 1 })
        );
    }

    #[test]
    fn a_failed_run_opens_on_everything_saved_since_a_test_fire_last_passed() {
        let history = history();
        let fires = [
            fire(6, LaunchStatus::Error),
            fire(3, LaunchStatus::Done),
            fire(2, LaunchStatus::Done),
        ];
        assert_eq!(opening(&history, &fires, Some(6)), Some(range(3, 6)));
        assert_eq!(
            opening(&history, &fires, Some(3)),
            Some(range(2, 3)),
            "a run that passed opens on what its save changed"
        );
        assert_eq!(
            opening(&history, &fires, None),
            Some(range(6, 7)),
            "with no run to read from, the newest save"
        );
        assert_eq!(
            opening(&history, &[fire(6, LaunchStatus::Error)], Some(6)),
            Some(range(5, 6)),
            "nothing ever passed, so there is no good end to start from"
        );
        assert_eq!(opening(&history, &fires, Some(40)), Some(range(6, 7)));
        assert_eq!(opening(&[], &fires, Some(6)), None);
    }

    #[test]
    fn a_saves_newest_finished_test_fire_is_its_verdict() {
        let fires = [
            fire(4, LaunchStatus::Running),
            fire(4, LaunchStatus::Error),
            fire(4, LaunchStatus::Done),
            fire(2, LaunchStatus::Cancelled),
        ];
        assert_eq!(verdict(&fires, 4), Some(false));
        assert_eq!(verdict(&fires, 2), None);
        assert_eq!(verdict(&fires, 9), None);
    }

    #[test]
    fn halving_narrows_a_range_to_one_save() {
        let history = history();
        assert_eq!(saves_in(&history, range(2, 6)), vec![3, 5, 6]);
        assert!(saves_in(&history, range(6, 2)).is_empty());
        assert!(saves_in(&history, range(3, 3)).is_empty());

        assert_eq!(
            halves(&history, range(1, 7)),
            Some((range(1, 5), range(5, 7)))
        );
        assert_eq!(
            halves(&history, range(5, 7)),
            Some((range(5, 6), range(6, 7)))
        );
        assert_eq!(halves(&history, range(6, 7)), None);
        assert_eq!(halves(&history, range(7, 1)), None);
        assert_eq!(halves(&history, Range { from: None, to: 1 }), None);
    }

    #[test]
    fn stepping_moves_one_save_either_way_and_stops_at_the_ends() {
        let history = history();
        assert_eq!(
            neighbours(&history, range(1, 5)),
            (Some(range(2, 3)), Some(range(5, 6)))
        );
        assert_eq!(neighbours(&history, single(&history, 1)).0, None);
        assert_eq!(neighbours(&history, single(&history, 7)).1, None);
        assert_eq!(neighbours(&history, range(1, 4)), (None, None));
    }

    #[test]
    fn a_save_is_placed_against_the_range_whichever_way_it_runs() {
        for range in [range(2, 6), range(6, 2)] {
            assert_eq!(placement(range, range.to), Placement::To);
            assert_eq!(placement(range, range.from.expect("from")), Placement::From);
            assert_eq!(placement(range, 3), Placement::Inside);
            assert_eq!(placement(range, 7), Placement::Outside);
            assert_eq!(placement(range, 1), Placement::Outside);
        }
        assert_eq!(
            placement(Range { from: None, to: 1 }, 2),
            Placement::Outside
        );
    }

    #[test]
    fn a_diff_becomes_one_patch_the_diff_view_reads_file_by_file() {
        let diff: DraftDiff = serde_json::from_value(serde_json::json!({
            "from": 2, "to": 4,
            "files": [
                {"path": "logo.png", "change": "modified", "additions": 0, "deletions": 0,
                 "unified": null},
                {"path": "notes.md", "change": "added", "additions": 1, "deletions": 0,
                 "unified": "--- /dev/null\n+++ b/notes.md\n@@ -0,0 +1 @@\n+todo\n"},
                {"path": "wf.star", "change": "modified", "additions": 1, "deletions": 1,
                 "unified": "--- a/wf.star\n+++ b/wf.star\n@@ -1 +1 @@\n-a = 1\n+a = 2\n"},
            ],
        }))
        .expect("diff");
        assert_eq!(totals(&diff), (2, 1));
        let files = rune_kit::diff::parse(&patch(&diff));
        let read: Vec<_> = files
            .iter()
            .map(|file| {
                (
                    file.path(),
                    file.status,
                    file.binary,
                    file.additions(),
                    file.deletions(),
                )
            })
            .collect();
        use rune_kit::diff::FileStatus;
        assert_eq!(
            read,
            vec![
                ("logo.png", FileStatus::Modified, true, 0, 0),
                ("notes.md", FileStatus::Added, false, 1, 0),
                ("wf.star", FileStatus::Modified, false, 1, 1),
            ]
        );
    }
}
