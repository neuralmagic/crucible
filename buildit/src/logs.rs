#![allow(clippy::disallowed_macros)]

use std::fmt;

use anyhow::{Context, Result, bail};
use regex::{Regex, RegexBuilder};
use serde::Serialize;

use crate::pod::BuilderPod;

pub const LOG_DIR: &str = "/tmp/buildit-logs";
pub const MAX_READ: u64 = 4 * 1024 * 1024;
const LINE_CAP: usize = 1024;
const REGEX_SIZE: usize = 1024 * 1024;
const MAX_CONTEXT: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogName {
    Build,
    Run(u32),
}

impl LogName {
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "build" => Ok(Self::Build),
            _ => match s.strip_prefix("run:").and_then(|n| n.parse().ok()) {
                Some(n) => Ok(Self::Run(n)),
                None => bail!("which must be build or run:<n>, got {s:?}"),
            },
        }
    }

    pub fn path(self) -> String {
        match self {
            Self::Build => format!("{LOG_DIR}/build.log"),
            Self::Run(n) => format!("{LOG_DIR}/run-{n}.log"),
        }
    }
}

impl fmt::Display for LogName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Build => f.write_str("build"),
            Self::Run(n) => write!(f, "run:{n}"),
        }
    }
}

#[derive(Debug)]
pub enum Query {
    Tail { lines: usize },
    Range { from: usize, to: Option<usize> },
    Grep { pattern: Regex, context: usize },
}

impl Query {
    pub fn grep(pattern: &str, context: usize) -> Result<Self> {
        let pattern = RegexBuilder::new(pattern)
            .size_limit(REGEX_SIZE)
            .build()
            .with_context(|| format!("bad grep pattern {pattern:?}"))?;
        Ok(Self::Grep {
            pattern,
            context: context.min(MAX_CONTEXT),
        })
    }
}

#[derive(Debug, Serialize)]
pub struct Page {
    pub lines: String,
    pub total_lines: usize,
    pub matched: usize,
    pub truncated: bool,
    pub truncated_head: bool,
}

pub struct Log {
    text: String,
    truncated_head: bool,
}

impl Log {
    pub async fn fetch(pod: &BuilderPod, name: LogName, window: u64) -> Result<Self> {
        let read = (window + 1).to_string();
        let argv = ["tail", "-c", &read, "--", &name.path()].map(String::from);
        let out = pod.exec_output(&argv, window + 1).await?;
        if out.code != Some(0) {
            bail!(
                "reading the {name} log: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(Self::from_bytes(out.stdout, window))
    }

    pub async fn from_container(pod: &BuilderPod, window: u64) -> Result<Self> {
        Ok(Self::last(pod.container_log().await?.into_bytes(), window))
    }

    fn last(mut bytes: Vec<u8>, window: u64) -> Self {
        let keep = usize::try_from(window.saturating_add(1)).unwrap_or(usize::MAX);
        bytes.drain(..bytes.len().saturating_sub(keep));
        Self::from_bytes(bytes, window)
    }

    fn from_bytes(mut bytes: Vec<u8>, window: u64) -> Self {
        let truncated_head = bytes.len() as u64 > window;
        if truncated_head {
            let cut = bytes
                .iter()
                .position(|b| *b == b'\n')
                .map_or(bytes.len(), |i| i + 1);
            bytes.drain(..cut);
        }
        Self {
            text: String::from_utf8_lossy(&bytes).into_owned(),
            truncated_head,
        }
    }

    pub fn query(&self, query: &Query, max_bytes: usize) -> Page {
        let total = self.text.lines().count();
        let mut out = Budget::new(max_bytes);
        let matched = match query {
            Query::Tail { lines: n } => {
                let picks = (*n).min(total);
                let numbered = (0..total).rev().zip(self.text.lines().rev());
                for (i, line) in numbered.take(picks) {
                    if !out.push(render(i, ':', line)) {
                        break;
                    }
                }
                out.reverse();
                picks
            }
            Query::Range { from, to } => {
                let start = from.max(&1) - 1;
                let picks = to.unwrap_or(total).min(total).saturating_sub(start);
                let numbered = self.text.lines().enumerate().skip(start);
                for (i, line) in numbered.take(picks) {
                    if !out.push(render(i, ':', line)) {
                        break;
                    }
                }
                picks
            }
            Query::Grep { pattern, context } => {
                let hits: Vec<bool> = self.text.lines().map(|l| pattern.is_match(l)).collect();
                let mut next_hit = None;
                let mut next_kept: Option<usize> = None;
                let numbered = (0..total).rev().zip(self.text.lines().rev());
                for (i, line) in numbered {
                    if hits[i] {
                        next_hit = Some(i);
                    }
                    let kept = next_hit.is_some_and(|h| h - i <= *context)
                        || hits[i.saturating_sub(*context)..i].contains(&true);
                    if !kept {
                        continue;
                    }
                    let mut row = render(i, if hits[i] { ':' } else { '-' }, line);
                    if next_kept.is_some_and(|k| k > i + 1) {
                        row.push_str("\n--");
                    }
                    if !out.push(row) {
                        break;
                    }
                    next_kept = Some(i);
                }
                out.reverse();
                hits.iter().filter(|h| **h).count()
            }
        };
        let (lines, truncated) = out.finish();
        Page {
            lines,
            total_lines: total,
            matched,
            truncated,
            truncated_head: self.truncated_head,
        }
    }
}

fn render(i: usize, mark: char, line: &str) -> String {
    if line.len() <= LINE_CAP {
        return format!("{}{mark}{line}", i + 1);
    }
    let cut = line.floor_char_boundary(LINE_CAP);
    format!(
        "{}{mark}{}... [{} bytes cut]",
        i + 1,
        &line[..cut],
        line.len() - cut
    )
}

struct Budget {
    rows: Vec<String>,
    used: usize,
    max: usize,
    truncated: bool,
}

impl Budget {
    fn new(max: usize) -> Self {
        Self {
            rows: Vec::new(),
            used: 0,
            max,
            truncated: false,
        }
    }

    fn push(&mut self, row: String) -> bool {
        self.used += row.len() + 1;
        self.truncated = self.used > self.max;
        if !self.truncated {
            self.rows.push(row);
        }
        !self.truncated
    }

    fn reverse(&mut self) {
        self.rows.reverse();
    }

    fn finish(self) -> (String, bool) {
        let mut out = self.rows.join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        (out, self.truncated)
    }
}

#[cfg(test)]
mod tests {
    use crate::logs::{Log, LogName, MAX_READ, Query};

    fn log(text: &str) -> Log {
        Log::from_bytes(text.as_bytes().to_vec(), MAX_READ)
    }

    fn numbered(n: usize) -> Log {
        log(&(1..=n).map(|i| format!("line {i}\n")).collect::<String>())
    }

    #[test]
    fn log_names_are_build_or_numbered_runs() {
        assert_eq!(
            LogName::parse("build").unwrap().path(),
            "/tmp/buildit-logs/build.log"
        );
        let run = LogName::parse("run:7").unwrap();
        assert_eq!(run.path(), "/tmp/buildit-logs/run-7.log");
        assert_eq!(run.to_string(), "run:7");
        for bad in [
            "",
            "Build",
            "run:",
            "run:-1",
            "run:1/../x",
            "../build",
            "run-1",
        ] {
            assert!(LogName::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn tail_and_range_number_lines_from_one() {
        let log = numbered(10);
        let page = log.query(&Query::Tail { lines: 2 }, 1024);
        assert_eq!(page.lines, "9:line 9\n10:line 10\n");
        assert_eq!(
            (page.total_lines, page.matched, page.truncated),
            (10, 2, false)
        );
        let page = log.query(
            &Query::Range {
                from: 3,
                to: Some(4),
            },
            1024,
        );
        assert_eq!(page.lines, "3:line 3\n4:line 4\n");
        let page = log.query(
            &Query::Range {
                from: 0,
                to: Some(1),
            },
            1024,
        );
        assert_eq!(page.lines, "1:line 1\n");
        let page = log.query(&Query::Range { from: 9, to: None }, 1024);
        assert_eq!(page.lines, "9:line 9\n10:line 10\n");
        let page = log.query(
            &Query::Range {
                from: 20,
                to: Some(30),
            },
            1024,
        );
        assert_eq!((page.lines.as_str(), page.matched), ("", 0));
        assert_eq!(log.query(&Query::Tail { lines: 99 }, 1024).matched, 10);
    }

    #[test]
    fn grep_marks_matches_context_and_gaps() {
        let log = numbered(12);
        let page = log.query(&Query::grep("^line (2|4|10)$", 1).unwrap(), 1024);
        assert_eq!(
            page.lines,
            "1-line 1\n2:line 2\n3-line 3\n4:line 4\n5-line 5\n--\n9-line 9\n10:line 10\n11-line 11\n"
        );
        assert_eq!((page.total_lines, page.matched), (12, 3));
        let page = log.query(&Query::grep("line 1[12]", 0).unwrap(), 1024);
        assert_eq!(page.lines, "11:line 11\n12:line 12\n");
        let page = log.query(&Query::grep("nope", 3).unwrap(), 1024);
        assert_eq!((page.lines.as_str(), page.matched), ("", 0));
    }

    #[test]
    fn grep_rejects_bad_and_oversized_patterns() {
        assert!(Query::grep("(", 0).is_err());
        assert!(Query::grep(r"\w{1000}{1000}", 0).is_err());
        let Query::Grep { context, .. } = Query::grep("x", 1000).unwrap() else {
            panic!("not a grep");
        };
        assert_eq!(context, 20);
    }

    #[test]
    fn output_keeps_whole_lines_within_max_bytes() {
        let log = numbered(10);
        let page = log.query(&Query::Tail { lines: 10 }, 20);
        assert_eq!(
            (page.lines.as_str(), page.truncated),
            ("9:line 9\n10:line 10\n", true)
        );
        let page = log.query(&Query::Range { from: 1, to: None }, 20);
        assert_eq!(
            (page.lines.as_str(), page.truncated),
            ("1:line 1\n2:line 2\n", true)
        );
        assert!(page.lines.len() <= 20);
    }

    #[test]
    fn grep_overflow_keeps_the_newest_matches() {
        let log = numbered(12);
        let page = log.query(&Query::grep("^line (2|4|10|11)$", 1).unwrap(), 54);
        assert_eq!(
            (page.lines.as_str(), page.truncated, page.matched),
            (
                "5-line 5\n--\n9-line 9\n10:line 10\n11:line 11\n12-line 12\n",
                true,
                4
            )
        );
    }

    #[test]
    fn huge_logs_of_short_lines_render_only_what_fits() {
        let lines = 2 * 1024 * 1024;
        let log = log(&"x\n".repeat(lines));
        let started = std::time::Instant::now();
        let tail = log.query(&Query::Tail { lines: usize::MAX }, 20);
        assert_eq!(
            (tail.lines.as_str(), tail.matched, tail.truncated),
            ("2097151:x\n2097152:x\n", lines, true)
        );
        let range = log.query(&Query::Range { from: 1, to: None }, 8);
        assert_eq!(
            (range.lines.as_str(), range.truncated),
            ("1:x\n2:x\n", true)
        );
        let grep = log.query(&Query::grep("x", 20).unwrap(), 10);
        assert_eq!(
            (grep.lines.as_str(), grep.matched, grep.truncated),
            ("2097152:x\n", lines, true)
        );
        assert_eq!(grep.total_lines, lines);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn long_lines_are_clipped_on_a_char_boundary() {
        let line = format!("{}é{}", "a".repeat(1023), "b".repeat(500));
        let page = log(&line).query(&Query::Tail { lines: 1 }, 64 * 1024);
        assert_eq!(
            page.lines,
            format!("1:{}... [502 bytes cut]\n", "a".repeat(1023))
        );
    }

    #[test]
    fn reads_past_the_window_drop_the_partial_first_line() {
        let window = usize::try_from(MAX_READ).unwrap();
        let mut bytes = b"partial\nwhole\n".to_vec();
        bytes.resize(window + 1, b'x');
        let log = Log::from_bytes(bytes, MAX_READ);
        let page = log.query(
            &Query::Range {
                from: 1,
                to: Some(1),
            },
            64 * 1024,
        );
        assert!(page.truncated_head);
        assert_eq!(page.lines, "1:whole\n");
        assert_eq!(page.total_lines, 2);

        let page =
            Log::from_bytes(b"ok \xff\n".to_vec(), MAX_READ).query(&Query::Tail { lines: 1 }, 1024);
        assert!(!page.truncated_head);
        assert_eq!(page.lines, "1:ok \u{fffd}\n");
    }

    #[test]
    fn container_logs_keep_the_newest_window() {
        let page = |log: Log| log.query(&Query::Range { from: 1, to: None }, 1024);
        let short = page(Log::last(b"a\nb\n".to_vec(), 16));
        assert!(!short.truncated_head);
        assert_eq!(short.lines, "1:a\n2:b\n");
        let long = page(Log::last(b"first\nsecond\nthird\n".to_vec(), 9));
        assert!(long.truncated_head);
        assert_eq!(long.lines, "1:third\n");
        let exact = page(Log::last(b"0123456789".to_vec(), 9));
        assert!(exact.truncated_head);
        assert_eq!(exact.lines, "");
        assert_eq!(page(Log::last(Vec::new(), 9)).total_lines, 0);
    }
}
