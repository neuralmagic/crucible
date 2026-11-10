//! A log grouped by the task each line came from, for reading many concurrent tasks one at a
//! time: a block per task in the order each first spoke, a fan-out's instances nested inside its
//! block, and the lines no task owns in a block of their own.

use std::collections::HashMap;

use crate::logline::{Body, LogLine};

/// Tasks or instances speaking in one log before it reads better grouped than streamed.
const GROUPED_FROM: usize = 5;

/// How the Log pane lays lines out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogMode {
    /// One stream, in the order the lines came.
    Stream,
    /// A block per task.
    ByTask,
}

/// One task's lines, or one instance's.
#[derive(Clone, Debug, PartialEq)]
pub struct Block<'a> {
    /// `task`, `task[key]`, or empty for the lines no task owns.
    pub name: String,
    /// Its own lines, its instances' left out.
    pub lines: Vec<&'a LogLine>,
    /// A fan-out's instances, each a block, in the order each first spoke.
    pub instances: Vec<Block<'a>>,
    /// Its own lines and its instances', in the order they came.
    pub all: Vec<&'a LogLine>,
}

impl<'a> Block<'a> {
    fn new(name: &str) -> Self {
        Block {
            name: name.to_string(),
            lines: Vec::new(),
            instances: Vec::new(),
            all: Vec::new(),
        }
    }

    pub fn problems(&self) -> usize {
        self.all.iter().filter(|line| line.is_problem()).count()
    }

    /// Its last `n` lines, its instances' included.
    pub fn tail(&self, n: usize) -> &[&'a LogLine] {
        &self.all[self.all.len().saturating_sub(n)..]
    }
}

/// Who a line belongs to: its task and, for a fan-out instance, its full `task[key]` name.
fn owner(line: &LogLine) -> Option<(&str, Option<&str>)> {
    let name = match (&line.task, &line.body) {
        (Some(name), _) => name.as_str(),
        (None, Body::TaskRow { name, .. }) => name.as_str(),
        _ => return None,
    };
    Some(match name.split_once('[') {
        Some((task, _)) => (task, Some(name)),
        None => (name, None),
    })
}

/// `lines` grouped by task: the tasks `plan` names in its order, then the rest in the order each
/// first spoke; lines no task owns come last, as a block named `""`.
pub fn blocks<'a>(lines: &[&'a LogLine], plan: &[&str]) -> Vec<Block<'a>> {
    let mut order: Vec<Block<'a>> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut loose = Block::new("");
    for line in lines {
        let Some((task, instance)) = owner(line) else {
            loose.lines.push(line);
            loose.all.push(line);
            continue;
        };
        let i = *at.entry(task.to_string()).or_insert_with(|| {
            order.push(Block::new(task));
            order.len() - 1
        });
        let block = &mut order[i];
        block.all.push(line);
        let Some(name) = instance else {
            block.lines.push(line);
            continue;
        };
        let found = match block.instances.iter().position(|b| b.name == name) {
            Some(found) => found,
            None => {
                block.instances.push(Block::new(name));
                block.instances.len() - 1
            }
        };
        block.instances[found].lines.push(line);
        block.instances[found].all.push(line);
    }
    order.sort_by_key(|block| {
        plan.iter()
            .position(|task| *task == block.name)
            .unwrap_or(plan.len())
    });
    if !loose.all.is_empty() {
        order.push(loose);
    }
    order
}

/// The layout a log takes unless one was picked: grouped once enough tasks or instances speak.
pub fn default_mode(lines: &[&LogLine]) -> LogMode {
    let mut speakers: Vec<&str> = lines
        .iter()
        .filter_map(|line| owner(line).map(|(task, instance)| instance.unwrap_or(task)))
        .collect();
    speakers.sort_unstable();
    speakers.dedup();
    if speakers.len() >= GROUPED_FROM {
        LogMode::ByTask
    } else {
        LogMode::Stream
    }
}

#[cfg(test)]
mod tests {
    use crate::logline::LogLine;
    use crate::unfurl::{LogMode, blocks, default_mode};

    fn line(task: Option<&str>, raw: &str) -> LogLine {
        let mut line = LogLine::parse(raw);
        line.task = task.map(ToString::to_string);
        line
    }

    #[test]
    fn lines_group_by_task_with_instances_nested_and_loose_lines_last() {
        let lines = [
            line(None, "plan v1: admitted"),
            line(Some("scan"), "reading"),
            line(Some("fix[b]"), "patching b"),
            line(Some("fix[a]"), "patching a"),
            line(Some("scan"), "done"),
            line(Some("fix[b]"), "pushed b"),
            line(Some("fix"), "all instances done"),
            line(None, "verdict: valid"),
        ];
        let refs: Vec<&LogLine> = lines.iter().collect();
        let grouped = blocks(&refs, &[]);
        let planned: Vec<String> = blocks(&refs, &["fix", "other"])
            .iter()
            .map(|b| b.name.clone())
            .collect();
        assert_eq!(planned, ["fix", "scan", ""]);
        type Shape<'a> = (&'a str, usize, Vec<(&'a str, usize)>);
        let shape: Vec<Shape> = grouped
            .iter()
            .map(|b| {
                (
                    b.name.as_str(),
                    b.lines.len(),
                    b.instances
                        .iter()
                        .map(|i| (i.name.as_str(), i.lines.len()))
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("scan", 2, vec![]),
                ("fix", 1, vec![("fix[b]", 2), ("fix[a]", 1)]),
                ("", 2, vec![]),
            ]
        );
        assert_eq!(grouped[1].all.len(), 4);
        let tail: Vec<&str> = grouped[1].tail(2).iter().map(|l| l.raw.as_str()).collect();
        assert_eq!(tail, ["pushed b", "all instances done"]);
        assert_eq!(grouped[0].tail(9).len(), 2);
    }

    #[test]
    fn a_block_counts_its_instances_problems() {
        let lines = [
            LogLine::parse("[fix[a]] \u{1f527} mcp__buildit__build failed: exec `sh` in pod x"),
            LogLine::parse(
                "[fix[b]] result error_max_turns turns=40 cost=$1.2000 error: max turns",
            ),
            line(Some("fix"), "fine"),
        ];
        let refs: Vec<&LogLine> = lines.iter().collect();
        let fix = &blocks(&refs, &[])[0];
        assert_eq!((fix.name.as_str(), fix.instances.len()), ("fix", 2));
        assert_eq!(fix.problems(), 2);
    }

    #[test]
    fn many_speakers_read_grouped_and_a_few_stream() {
        let few = [line(Some("a"), "x"), line(Some("b"), "y"), line(None, "z")];
        let many: Vec<LogLine> = (0..5)
            .map(|i| line(Some(&format!("fix[{i}]")), "x"))
            .collect();
        let few: Vec<&LogLine> = few.iter().collect();
        let many: Vec<&LogLine> = many.iter().collect();
        assert_eq!(default_mode(&few), LogMode::Stream);
        assert_eq!(default_mode(&many), LogMode::ByTask);
    }
}
