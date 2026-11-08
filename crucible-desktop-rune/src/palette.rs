//! The command palette's list: the app's commands, the open run's tasks, and the playbooks and
//! packs to switch to, ranked against what the person typed.

use rune::TextEdit;

use crate::keymap::Command;

/// The most entries the palette lists.
pub const SHOWN: usize = 60;

/// What picking an entry does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Run(Command),
    Task(String),
    Playbook(String),
    Pack(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub item: Item,
    pub title: String,
    /// What kind of entry it is, shown beside it.
    pub kind: &'static str,
}

/// The palette while it is open.
#[derive(Clone, Debug, Default)]
pub struct Palette {
    pub query: TextEdit,
    /// The highlighted row, an index into the ranked list.
    pub selected: usize,
}

/// Everything the palette can offer: the open run's tasks, the commands, then playbooks and packs.
pub fn entries(tasks: &[&str], playbooks: &[&str], packs: &[&str]) -> Vec<Entry> {
    let entry = |item: Item, title: String, kind: &'static str| Entry { item, title, kind };
    tasks
        .iter()
        .map(|t| entry(Item::Task(t.to_string()), t.to_string(), "task"))
        .chain(
            Command::ALL
                .iter()
                .filter(|c| **c != Command::Palette)
                .map(|c| entry(Item::Run(*c), c.title(), "command")),
        )
        .chain(
            playbooks
                .iter()
                .map(|p| entry(Item::Playbook(p.to_string()), p.to_string(), "playbook")),
        )
        .chain(
            packs
                .iter()
                .map(|p| entry(Item::Pack(p.to_string()), p.to_string(), "pack")),
        )
        .collect()
}

/// `entries` matching `query`, best first; all of them, in order, for an empty query.
pub fn rank<'a>(entries: &'a [Entry], query: &str) -> Vec<&'a Entry> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return entries.iter().take(SHOWN).collect();
    }
    let mut scored: Vec<(i32, usize, &Entry)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| score(&query, &e.title.to_lowercase()).map(|s| (s, i, e)))
        .collect();
    scored.sort_by_key(|(s, i, _)| (std::cmp::Reverse(*s), *i));
    scored.into_iter().take(SHOWN).map(|(_, _, e)| e).collect()
}

/// How well `query` matches `text`, both lowercase: every query character in order, scored up
/// for runs, word starts and an early first hit, down for gaps. `None` when it does not match.
fn score(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.chars().collect();
    let mut at = 0;
    let mut total = 0;
    let mut last: Option<usize> = None;
    for q in query.chars().filter(|c| !c.is_whitespace()) {
        let found = (at..text.len()).find(|&i| text[i] == q)?;
        let word_start = found == 0 || matches!(text[found - 1], ' ' | '-' | '_' | '/' | '.' | ':');
        total += 10;
        if last.is_some_and(|l| l + 1 == found) {
            total += 25;
        }
        if word_start {
            total += 15;
        }
        total -= (found - at) as i32;
        last = Some(found);
        at = found + 1;
    }
    Some(total - text.len() as i32 / 8)
}

#[cfg(test)]
mod tests {
    use crate::keymap::Command;
    use crate::palette::{Item, SHOWN, entries, rank, score};
    use crate::workspace::PaneKind;

    #[test]
    fn a_query_matches_its_characters_in_order_preferring_runs_and_word_starts() {
        assert!(score("abc", "xaxbxc").is_some());
        assert!(score("abd", "abc").is_none());
        assert!(score("sl", "show log") > score("sl", "settings panel"));
        assert!(score("log", "show log") > score("log", "l o g"));
        assert!(score("cve", "cve-triage") > score("cve", "recover everything"));
    }

    #[test]
    fn the_list_offers_tasks_then_commands_then_places_and_ranks_by_the_query() {
        let all = entries(&["analyze", "report"], &["cve-triage-llm-d"], &["playbook"]);
        assert_eq!(all[0].item, Item::Task("analyze".into()));
        assert!(!all.iter().any(|e| e.item == Item::Run(Command::Palette)));
        assert_eq!(
            all.last().map(|e| &e.item),
            Some(&Item::Pack("playbook".into()))
        );
        assert_eq!(rank(&all, "").len(), all.len().min(SHOWN));
        let first = |q: &str| rank(&all, q).first().map(|e| e.item.clone());
        assert_eq!(
            first("cve"),
            Some(Item::Playbook("cve-triage-llm-d".into()))
        );
        assert_eq!(
            first("show log"),
            Some(Item::Run(Command::Show(PaneKind::Log)))
        );
        assert_eq!(first("anal"), Some(Item::Task("analyze".into())));
        assert!(rank(&all, "zzzz").is_empty());
    }
}
