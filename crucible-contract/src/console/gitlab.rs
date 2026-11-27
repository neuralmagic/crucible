//! GitLab job traces: lines of console text, folded into the sections the runner marks.
//!
//! ```text
//! 2026-11-27T01:58:22.665106Z 00O+section_start:1795744702:prepare_script\r\e[0K
//! 2026-11-27T01:58:22.665131Z 00O+\e[0K\e[36;1mPreparing environment\e[0;m
//! └─ timestamp ─────────────┘ │ │└ `+` continues the line before, ` ` starts one
//!                             │ └ O stdout, E stderr
//!                             └ stream id
//! ```
//!
//! Runners before the timestamped format write the same lines without the prefix. A section
//! opens at `section_start:<unix>:<name>[collapsed=true]\r\e[0K` and closes at
//! `section_end:<unix>:<name>\r\e[0K`; the text after a start marker is the section's header.

use crate::console::{Pen, Styled};

/// One line of a trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// The RFC 3339 time the runner stamped the line with, on runners that stamp.
    pub at: Option<String>,
    pub stderr: bool,
    pub text: Styled,
}

/// A section the runner marked: its header line and what it printed until it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub header: Line,
    /// Shown folded unless opened, as the job asked with `collapsed=true`.
    pub collapsed: bool,
    /// Seconds from start to end; `None` while it runs or when the job hides it.
    pub duration: Option<u64>,
    pub items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    Line(Line),
    Section(Section),
}

/// `trace` as lines and the sections around them, in order.
pub fn parse(trace: &str) -> Vec<Item> {
    let mut tree = Tree::default();
    let mut pen = Pen::default();
    for (at, stderr, raw) in joined(trace) {
        let mut text = String::new();
        let mut marks = Vec::new();
        let mut rest = raw.as_str();
        while let Some((before, mark, after)) = next_mark(rest) {
            text.push_str(before);
            marks.push(mark);
            rest = after;
        }
        text.push_str(rest);
        let line = Line {
            at,
            stderr,
            text: pen.write(&text),
        };
        tree.add(marks, line);
    }
    tree.finish()
}

/// Physical lines with their stamps read off and `+` continuations joined onto the line before.
fn joined(trace: &str) -> Vec<(Option<String>, bool, String)> {
    let mut lines: Vec<(Option<String>, bool, String)> = Vec::new();
    if trace.is_empty() {
        return lines;
    }
    let body = trace.strip_suffix('\n').unwrap_or(trace);
    for physical in body.split('\n') {
        match stamped(physical) {
            Some(stamp) => match lines.last_mut() {
                Some(last) if stamp.continues => last.2.push_str(stamp.text),
                _ => lines.push((Some(stamp.at.to_string()), stamp.stderr, stamp.text.into())),
            },
            None => lines.push((None, false, physical.to_string())),
        }
    }
    lines
}

struct Stamp<'a> {
    at: &'a str,
    stderr: bool,
    continues: bool,
    text: &'a str,
}

/// `2026-11-27T01:58:22.651300Z 00O+text`.
fn stamped(line: &str) -> Option<Stamp<'_>> {
    let (at, rest) = line.split_once(' ')?;
    let bytes = at.as_bytes();
    if !bytes.first()?.is_ascii_digit() || !at.ends_with('Z') || !at.contains('T') {
        return None;
    }
    let head = rest.as_bytes();
    if head.len() < 3 || !head[0].is_ascii_hexdigit() || !head[1].is_ascii_hexdigit() {
        return None;
    }
    let stderr = match head[2] {
        b'O' => false,
        b'E' => true,
        _ => return None,
    };
    let continues = match head.get(3) {
        Some(b'+') => true,
        Some(b' ') | None => false,
        _ => return None,
    };
    Some(Stamp {
        at,
        stderr,
        continues,
        text: rest.get(4..).unwrap_or(""),
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Mark {
    Start {
        at: u64,
        name: String,
        collapsed: bool,
        hide_duration: bool,
    },
    End {
        at: u64,
        name: String,
    },
}

const MARK_END: &str = "\r\u{1b}[0K";

/// The first section marker in `text`: the text before it, the marker, and the text after.
fn next_mark(text: &str) -> Option<(&str, Mark, &str)> {
    let mut from = 0;
    while let Some(found) = text[from..].find("section_") {
        let at = from + found;
        if let Some((mark, len)) = mark(&text[at..]) {
            return Some((&text[..at], mark, &text[at + len..]));
        }
        from = at + "section_".len();
    }
    None
}

/// A marker at the start of `text`, and its length in bytes.
fn mark(text: &str) -> Option<(Mark, usize)> {
    let (start, rest) = if let Some(rest) = text.strip_prefix("section_start:") {
        (true, rest)
    } else {
        (false, text.strip_prefix("section_end:")?)
    };
    let end = rest.find(MARK_END)?;
    let body = &rest[..end];
    let (at, named) = body.split_once(':')?;
    let at = at.parse().ok()?;
    let (name, options) = match named.split_once('[') {
        Some((name, options)) => (name, Some(options.strip_suffix(']')?)),
        None => (named, None),
    };
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
    {
        return None;
    }
    let flag = |key: &str| {
        options.is_some_and(|options| {
            options
                .split(',')
                .any(|option| option.trim() == format!("{key}=true"))
        })
    };
    let mark = if start {
        Mark::Start {
            at,
            name: name.to_string(),
            collapsed: flag("collapsed"),
            hide_duration: flag("hide_duration"),
        }
    } else {
        if options.is_some() {
            return None;
        }
        Mark::End {
            at,
            name: name.to_string(),
        }
    };
    let len = text.len() - rest.len() + end + MARK_END.len();
    Some((mark, len))
}

struct Open {
    started: u64,
    hide_duration: bool,
    section: Section,
}

#[derive(Default)]
struct Tree {
    root: Vec<Item>,
    open: Vec<Open>,
}

impl Tree {
    fn push(&mut self, item: Item) {
        match self.open.last_mut() {
            Some(open) => open.section.items.push(item),
            None => self.root.push(item),
        }
    }

    /// Applies a line's markers in order, then places its text: as the header of the last
    /// section it opened, else as a line of whatever is open.
    fn add(&mut self, marks: Vec<Mark>, line: Line) {
        let marked = !marks.is_empty();
        let mut header = None;
        for mark in marks {
            match mark {
                Mark::Start {
                    at,
                    name,
                    collapsed,
                    hide_duration,
                } => {
                    if let Some(index) = header.take() {
                        self.fill_header(index, None);
                    }
                    self.open.push(Open {
                        started: at,
                        hide_duration,
                        section: Section {
                            header: Line {
                                at: line.at.clone(),
                                stderr: line.stderr,
                                text: Styled::default(),
                            },
                            name,
                            collapsed,
                            duration: None,
                            items: Vec::new(),
                        },
                    });
                    header = Some(self.open.len() - 1);
                }
                Mark::End { at, name } => {
                    if let Some(index) = header.take() {
                        self.fill_header(index, None);
                    }
                    self.close(&name, Some(at));
                }
            }
        }
        match header {
            Some(index) => self.fill_header(index, Some(line)),
            None if marked && line.text.text.is_empty() => {}
            None => self.push(Item::Line(line)),
        }
    }

    /// The header of open section `index`: `line` when it has text, else the section's name.
    fn fill_header(&mut self, index: usize, line: Option<Line>) {
        let Some(open) = self.open.get_mut(index) else {
            return;
        };
        let header = &mut open.section.header;
        match line {
            Some(line) if !line.text.text.trim().is_empty() => *header = line,
            _ => header.text.text = open.section.name.clone(),
        }
    }

    /// Ends the innermost open section named `name` and every section inside it.
    fn close(&mut self, name: &str, at: Option<u64>) {
        let Some(index) = self.open.iter().rposition(|o| o.section.name == name) else {
            return;
        };
        while self.open.len() > index {
            let Some(open) = self.open.pop() else {
                return;
            };
            let ended = if self.open.len() == index { at } else { None };
            let mut section = open.section;
            section.duration = ended
                .filter(|_| !open.hide_duration)
                .map(|end| end.saturating_sub(open.started));
            self.push(Item::Section(section));
        }
    }

    fn finish(mut self) -> Vec<Item> {
        if let Some(outer) = self.open.first() {
            let name = outer.section.name.clone();
            self.close(&name, None);
        }
        self.root
    }
}

#[cfg(test)]
mod tests {
    use crate::console::gitlab::{Item, Line, Section, parse};
    use crate::console::{Color, Run, Style};

    /// The start of a markdown lint job's trace from a gitlab-runner 19.5 Kubernetes runner,
    /// as `GET /projects/:id/jobs/:job/trace` returns it.
    const STAMPED: &str = "2026-11-27T01:58:22.651300Z 00O \u{1b}[0KRunning with gitlab-runner 19.5.1 (f411f264)\u{1b}[0;m
2026-11-27T01:58:22.651376Z 00O \u{1b}[0K\u{1b}[36;1mResolving secrets\u{1b}[0;m\u{1b}[0;m
2026-11-27T01:58:22.651636Z 00O section_start:1795744702:prepare_executor\r\u{1b}[0K
2026-11-27T01:58:22.651642Z 00O+\u{1b}[0K\u{1b}[36;1mPreparing the \"kubernetes\" executor\u{1b}[0;m\u{1b}[0;m
2026-11-27T01:58:22.651701Z 00O \u{1b}[0K\"CPURequest\" overwritten with \"281m\"\u{1b}[0;m
2026-11-27T01:58:22.664960Z 00O section_end:1795744702:prepare_executor\r\u{1b}[0K
2026-11-27T01:58:22.665106Z 00O+section_start:1795744702:prepare_script\r\u{1b}[0K
2026-11-27T01:58:22.665131Z 00O+\u{1b}[0K\u{1b}[36;1mPreparing environment\u{1b}[0;m\u{1b}[0;m
2026-11-27T01:58:22.797712Z 00E Waiting for pod krd-gitlab-runner--pipeline/runner-jaymve153 to be running, status is Pending
2026-11-27T01:58:26.499725Z 01O Running on runner-jaymve153-project-78877-concurrent-0-tyhghzu4...
2026-11-27T01:58:26.499749Z 01O
2026-11-27T01:58:26.499793Z 00O section_end:1795744706:prepare_script\r\u{1b}[0K
2026-11-27T01:58:26.500895Z 00O+section_start:1795744706:get_sources\r\u{1b}[0K
2026-11-27T01:58:26.500904Z 00O+\u{1b}[0K\u{1b}[36;1mGetting source from Git repository\u{1b}[0;m\u{1b}[0;m
2026-11-27T01:58:26.999742Z 01O \u{1b}[32;1mGitaly correlation ID: 01M8DBK7Q4Y4MR5E5ZP1MRX7NE\u{1b}[0;m
2026-11-27T01:58:26.999769Z 01O Initialized empty Git repository in /builds/releng/konflux-release-data/.git/
2026-11-27T01:58:39.904165Z 00O section_end:1795744719:get_sources\r\u{1b}[0K
2026-11-27T01:58:39.906048Z 00O+section_start:1795744719:step_script\r\u{1b}[0K
2026-11-27T01:58:39.906062Z 00O+\u{1b}[0K\u{1b}[36;1mExecuting \"step_script\" stage of the job script\u{1b}[0;m\u{1b}[0;m
2026-11-27T01:58:40.120000Z 01O \u{1b}[32;1m$ markdownlint-cli2 \"**/*.md\"\u{1b}[0;m
2026-11-27T01:58:41.000000Z 01O Summary: 0 error(s)
2026-11-27T01:59:20.948973Z 00O section_end:1795744760:step_script\r\u{1b}[0K
2026-11-27T01:59:21.459815Z 00O \u{1b}[32;1mJob succeeded\u{1b}[0;m
";

    /// The same shape from a runner that predates timestamps, with a job's own nested
    /// sections, one collapsed and one hiding its duration.
    const CLASSIC: &str = "\u{1b}[0KRunning with gitlab-runner 16.11.0 (91a27b2a)\u{1b}[0;m
section_start:1700000000:prepare_script\r\u{1b}[0K\u{1b}[0K\u{1b}[36;1mPreparing environment\u{1b}[0;m
Running on runner-abc...
section_end:1700000003:prepare_script\r\u{1b}[0Ksection_start:1700000003:step_script\r\u{1b}[0K\u{1b}[0K\u{1b}[36;1mExecuting \"step_script\" stage of the job script\u{1b}[0;m
\u{1b}[32;1m$ echo -e \"section_start:`date +%s`:deps[collapsed=true]\\r\\e[0KInstalling deps\"\u{1b}[0;m
section_start:1700000004:deps[collapsed=true]\r\u{1b}[0KInstalling deps
\u{1b}[33mwarning\u{1b}[0m: pip is old
section_start:1700000005:inner[hide_duration=true]\r\u{1b}[0K
deep
section_end:1700000099:inner\r\u{1b}[0K
section_end:1700000010:deps\r\u{1b}[0K
section_end:1700000020:step_script\r\u{1b}[0K
section_end:1700000021:never_opened\r\u{1b}[0K
\u{1b}[31;1mERROR: Job failed: exit code 1\u{1b}[0;m
";

    fn texts(items: &[Item]) -> Vec<String> {
        let mut out = Vec::new();
        for item in items {
            match item {
                Item::Line(line) => out.push(line.text.text.clone()),
                Item::Section(section) => {
                    out.push(format!(
                        "[{}{}] {}",
                        section.name,
                        if section.collapsed { " folded" } else { "" },
                        section.header.text.text
                    ));
                    out.extend(texts(&section.items).into_iter().map(|t| format!("  {t}")));
                }
            }
        }
        out
    }

    fn section<'a>(items: &'a [Item], name: &str) -> &'a Section {
        items
            .iter()
            .find_map(|item| match item {
                Item::Section(section) if section.name == name => Some(section),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no section {name}"))
    }

    #[test]
    fn a_stamped_trace_folds_into_the_runner_sections() {
        let items = parse(STAMPED);
        assert_eq!(
            texts(&items),
            [
                "Running with gitlab-runner 19.5.1 (f411f264)",
                "Resolving secrets",
                "[prepare_executor] Preparing the \"kubernetes\" executor",
                "  \"CPURequest\" overwritten with \"281m\"",
                "[prepare_script] Preparing environment",
                "  Waiting for pod krd-gitlab-runner--pipeline/runner-jaymve153 to be running, status is Pending",
                "  Running on runner-jaymve153-project-78877-concurrent-0-tyhghzu4...",
                "  ",
                "[get_sources] Getting source from Git repository",
                "  Gitaly correlation ID: 01M8DBK7Q4Y4MR5E5ZP1MRX7NE",
                "  Initialized empty Git repository in /builds/releng/konflux-release-data/.git/",
                "[step_script] Executing \"step_script\" stage of the job script",
                "  $ markdownlint-cli2 \"**/*.md\"",
                "  Summary: 0 error(s)",
                "Job succeeded",
            ]
        );
        let script = section(&items, "step_script");
        assert_eq!(script.duration, Some(41));
        assert!(!script.collapsed);
        assert_eq!(section(&items, "prepare_executor").duration, Some(0));
        assert_eq!(
            script.header.at.as_deref(),
            Some("2026-11-27T01:58:39.904165Z"),
            "a joined line keeps the stamp of its first part"
        );
        let cyan_bold = Style {
            fg: Some(Color::Palette(6)),
            bold: true,
            ..Style::default()
        };
        assert_eq!(
            script.header.text.runs,
            vec![Run {
                start: 0,
                end: script.header.text.text.len(),
                style: cyan_bold
            }]
        );
        let Item::Section(prepare) = &items[3] else {
            panic!("prepare_script is a section")
        };
        let Item::Line(waiting) = &prepare.items[0] else {
            panic!("a line")
        };
        assert!(waiting.stderr);
        assert_eq!(waiting.at.as_deref(), Some("2026-11-27T01:58:22.797712Z"));
        assert!(waiting.text.runs.is_empty());
    }

    #[test]
    fn a_classic_trace_nests_job_sections_and_reads_their_options() {
        let items = parse(CLASSIC);
        assert_eq!(
            texts(&items),
            [
                "Running with gitlab-runner 16.11.0 (91a27b2a)",
                "[prepare_script] Preparing environment",
                "  Running on runner-abc...",
                "[step_script] Executing \"step_script\" stage of the job script",
                "  $ echo -e \"section_start:`date +%s`:deps[collapsed=true]\\r\\e[0KInstalling deps\"",
                "  [deps folded] Installing deps",
                "    warning: pip is old",
                "    [inner] inner",
                "      deep",
                "ERROR: Job failed: exit code 1",
            ]
        );
        let script = section(&items, "step_script");
        let deps = section(&script.items, "deps");
        assert_eq!(deps.duration, Some(6));
        assert_eq!(section(&deps.items, "inner").duration, None, "hidden");
        assert_eq!(section(&items, "prepare_script").duration, Some(3));
        assert!(items.iter().all(|item| match item {
            Item::Line(line) => line.at.is_none(),
            Item::Section(_) => true,
        }));
    }

    #[test]
    fn an_end_closes_the_sections_inside_it_and_a_running_job_stays_open() {
        let items = parse(
            "section_start:1:outer\r\u{1b}[0Kouter\nsection_start:2:inner\r\u{1b}[0Kinner\nx\nsection_end:9:outer\r\u{1b}[0K\nsection_start:10:tail\r\u{1b}[0Ktail\nstill going\n",
        );
        assert_eq!(
            texts(&items),
            [
                "[outer] outer",
                "  [inner] inner",
                "    x",
                "[tail] tail",
                "  still going"
            ]
        );
        let outer = section(&items, "outer");
        assert_eq!(outer.duration, Some(8));
        assert_eq!(section(&outer.items, "inner").duration, None);
        assert_eq!(section(&items, "tail").duration, None);
    }

    #[test]
    fn text_that_only_looks_like_a_marker_stays_a_line() {
        for raw in [
            "section_start:notanumber:x\r\u{1b}[0Ky",
            "section_start:1:has space\r\u{1b}[0Ky",
            "section_start:1:x without the clear",
            "section_end:1:x[collapsed=true]\r\u{1b}[0K",
            "section_start:1:\r\u{1b}[0Ky",
        ] {
            let items = parse(raw);
            assert!(
                matches!(items.as_slice(), [Item::Line(Line { .. })]),
                "{raw:?} gave {items:?}"
            );
        }
    }

    #[test]
    fn stamps_need_the_whole_prefix_and_plus_joins_only_after_a_line() {
        assert_eq!(parse(""), vec![]);
        assert_eq!(parse("\n"), vec![Item::Line(plain_line(""))]);
        let items = parse(
            "2026-11-27T01:58:22Z 00O+first\n2026-11-27T01:58:22Z 00O+ joined\n2026-11-27T01:58:22Z 0xO broken\nno stamp",
        );
        assert_eq!(
            texts(&items),
            [
                "first joined",
                "2026-11-27T01:58:22Z 0xO broken",
                "no stamp"
            ]
        );
        let header = parse("section_start:5:only_marker\r\u{1b}[0K\n");
        assert_eq!(texts(&header), ["[only_marker] only_marker"]);
        let windows = parse("line one\r\nline two\r\n");
        assert_eq!(texts(&windows), ["line one", "line two"]);
        let trimmed = parse("2026-11-27T01:58:26.499749Z 01O\n2026-11-27T01:58:26.499749Z 01E \n");
        assert_eq!(texts(&trimmed), ["", ""]);
    }

    fn plain_line(text: &str) -> Line {
        Line {
            at: None,
            stderr: false,
            text: crate::console::Styled {
                text: text.into(),
                runs: vec![],
            },
        }
    }
}
