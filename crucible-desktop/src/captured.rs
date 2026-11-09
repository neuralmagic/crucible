//! The files a run's tasks captured (their `emits_files`), as the controller lists and serves
//! them, and how each is shown.

use std::path::PathBuf;

/// The largest file the Files pane fetches and draws.
pub const SHOWN_BYTES: u64 = 8 * 1024 * 1024;

/// One captured file, as `GET /api/runs/{run}/files` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    pub task: String,
    /// The fan-out instance key, for a file an instance of a mapped task captured.
    pub instance: Option<String>,
    /// The path the task declared.
    pub path: String,
    pub size: u64,
    /// `<task dir>/<path>`: what fetches it.
    pub key: String,
}

impl From<crux::dto::RunFile> for Captured {
    fn from(file: crux::dto::RunFile) -> Self {
        Captured {
            task: file.task,
            instance: file.instance,
            path: file.path,
            size: file.size_bytes,
            key: file.key,
        }
    }
}

impl Captured {
    /// The task, with its instance when it has one.
    pub fn owner(&self) -> String {
        match &self.instance {
            Some(instance) => format!("{}[{instance}]", self.task),
            None => self.task.clone(),
        }
    }

    pub fn shown(&self) -> bool {
        self.size <= SHOWN_BYTES
    }
}

/// The file under `key`, else the first listed.
pub fn pick<'a>(files: &'a [Captured], key: Option<&str>) -> Option<&'a Captured> {
    key.and_then(|key| files.iter().find(|file| file.key == key))
        .or_else(|| files.first())
}

/// The listing grouped by owner, in the order the controller listed them.
pub fn by_owner(files: &[Captured]) -> Vec<(String, Vec<&Captured>)> {
    let mut groups: Vec<(String, Vec<&Captured>)> = Vec::new();
    for file in files {
        let owner = file.owner();
        match groups.iter_mut().find(|(name, _)| *name == owner) {
            Some((_, list)) => list.push(file),
            None => groups.push((owner, vec![file])),
        }
    }
    groups
}

/// How a file is drawn, by its extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Markdown,
    Patch,
    Json,
    JsonLines,
    Text,
}

impl Kind {
    pub fn of(path: &str) -> Kind {
        let ext = path
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "md" | "markdown" => Kind::Markdown,
            "patch" | "diff" => Kind::Patch,
            "json" => Kind::Json,
            "jsonl" | "ndjson" => Kind::JsonLines,
            _ => Kind::Text,
        }
    }
}

/// A fetched file's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Text(String),
    /// Not UTF-8: only its length is shown.
    Binary(usize),
}

impl Body {
    pub fn from_bytes(bytes: Vec<u8>) -> Body {
        match String::from_utf8(bytes) {
            Ok(text) => Body::Text(text),
            Err(error) => Body::Binary(error.as_bytes().len()),
        }
    }
}

/// One file the worker fetched for the Files pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub run: String,
    pub key: String,
    pub body: Result<Body, String>,
    /// The copy on disk the editor opens and the copy button hands out.
    pub disk: Option<PathBuf>,
}

/// Where a run's fetched file is kept: the user's cache directory, by run and key.
pub fn cache_path(run: &str, key: &str) -> Option<PathBuf> {
    let mut path = crate::httpcache::cache_root()?
        .join("run-files")
        .join(safe(run));
    for segment in key.split('/').filter(|s| !s.is_empty() && *s != "..") {
        path.push(safe(segment));
    }
    Some(path)
}

fn safe(segment: &str) -> String {
    segment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.[]=,".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `2.0 KiB`, `3.9 KiB`, `512 B`.
pub fn size_label(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `text` as drawn for its kind: JSON indented, each JSON Lines record indented with a blank
/// line between records, the rest as it is. Keys keep their order. Text that does not parse
/// is left alone.
pub fn display(kind: Kind, text: &str) -> String {
    match kind {
        Kind::Json => indent(text).unwrap_or_else(|| text.to_string()),
        Kind::JsonLines => text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| indent(line).unwrap_or_else(|| line.to_string()))
            .collect::<Vec<_>>()
            .join("\n\n"),
        Kind::Markdown | Kind::Patch | Kind::Text => text.to_string(),
    }
}

/// Valid JSON re-indented by two spaces, keeping key order and every string as written.
fn indent(json: &str) -> Option<String> {
    serde_json::from_str::<serde::de::IgnoredAny>(json).ok()?;
    let mut out = String::with_capacity(json.len() * 2);
    let mut depth = 0usize;
    let mut chars = json.trim().chars().peekable();
    let newline = |out: &mut String, depth: usize| {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    };
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                out.push('"');
                while let Some(c) = chars.next() {
                    out.push(c);
                    match c {
                        '\\' => out.extend(chars.next()),
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '{' | '[' => {
                let close = if c == '{' { '}' } else { ']' };
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                if chars.peek() == Some(&close) {
                    chars.next();
                    out.push(c);
                    out.push(close);
                } else {
                    out.push(c);
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(',');
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use crate::captured::{Body, Captured, Kind, by_owner, cache_path, display, size_label};

    const LISTING: &str = include_str!("../fixtures/run-files/listing.json");
    const REPORT: &str = include_str!("../fixtures/run-files/climb/report.json");
    const RESULTS: &str = include_str!("../fixtures/run-files/climb/results.jsonl");

    fn listing() -> Vec<Captured> {
        let files: crux::dto::RunFiles = serde_json::from_str(LISTING).unwrap();
        files.files.into_iter().map(Captured::from).collect()
    }

    #[test]
    fn a_hill_climb_listing_groups_by_task_in_listed_order() {
        let files = listing();
        let groups = by_owner(&files);
        let shape: Vec<(&str, Vec<&str>)> = groups
            .iter()
            .map(|(owner, files)| {
                (
                    owner.as_str(),
                    files.iter().map(|f| f.path.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                (
                    "climb",
                    vec![
                        "best.patch",
                        "report.json",
                        "results.jsonl",
                        "timeline.jsonl"
                    ]
                ),
                ("report", vec!["summary.md"]),
            ]
        );
        assert_eq!(files[4].key, "report/summary.md");
        assert_eq!(files[4].size, 2013);
        assert!(files.iter().all(Captured::shown));
    }

    #[test]
    fn an_instance_file_is_owned_by_its_instance() {
        let mut file = listing().remove(0);
        file.instance = Some("kern1".into());
        assert_eq!(file.owner(), "climb[kern1]");
        file.size = crate::captured::SHOWN_BYTES + 1;
        assert!(!file.shown());
    }

    #[test]
    fn each_captured_file_is_drawn_by_its_extension() {
        let kinds: Vec<Kind> = listing().iter().map(|f| Kind::of(&f.path)).collect();
        assert_eq!(
            kinds,
            [
                Kind::Patch,
                Kind::Json,
                Kind::JsonLines,
                Kind::JsonLines,
                Kind::Markdown
            ]
        );
        assert_eq!(Kind::of("trace.LOG"), Kind::Text);
        assert_eq!(Kind::of("Makefile"), Kind::Text);
        assert_eq!(Kind::of("a.diff"), Kind::Patch);
    }

    #[test]
    fn json_is_indented_in_its_own_key_order() {
        let shown = display(Kind::Json, REPORT);
        let original: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        let reparsed: serde_json::Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(original, reparsed);
        assert!(shown.starts_with("{\n  \"best\": \"kern1\",\n  \"candidates\": [\n    {\n"));
        let label = shown.find("\"label\": \"baseline\"").unwrap();
        let round = shown.find("\"round\": 0").unwrap();
        assert!(label < round, "label is written before round in the file");
    }

    #[test]
    fn indenting_keeps_strings_and_empty_containers_whole() {
        assert_eq!(
            display(
                Kind::Json,
                r#"{"a":"x, {y}: \"z\"","b":[],"c":{ },"d":[1,2]}"#
            ),
            "{\n  \"a\": \"x, {y}: \\\"z\\\"\",\n  \"b\": [],\n  \"c\": {},\n  \"d\": [\n    1,\n    2\n  ]\n}"
        );
        assert_eq!(display(Kind::Json, "{not json"), "{not json");
        assert_eq!(display(Kind::Text, "{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn json_lines_indent_each_record_and_keep_a_bad_line() {
        let shown = display(Kind::JsonLines, RESULTS);
        let records: Vec<&str> = shown.split("\n\n").collect();
        assert_eq!(records.len(), RESULTS.lines().count());
        for (record, line) in records.iter().zip(RESULTS.lines()) {
            let a: serde_json::Value = serde_json::from_str(record).unwrap();
            let b: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(a, b);
        }
        assert_eq!(
            display(Kind::JsonLines, "{\"a\":1}\noops\n\n"),
            "{\n  \"a\": 1\n}\n\noops"
        );
    }

    #[test]
    fn bytes_that_are_not_utf8_are_binary() {
        assert_eq!(
            Body::from_bytes(b"# hi\n".to_vec()),
            Body::Text("# hi\n".into())
        );
        assert_eq!(Body::from_bytes(vec![0xff, 0xfe, 0x00]), Body::Binary(3));
    }

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(size_label(0), "0 B");
        assert_eq!(size_label(1023), "1023 B");
        assert_eq!(size_label(2013), "2.0 KiB");
        assert_eq!(size_label(3955), "3.9 KiB");
        assert_eq!(size_label(5 * 1024 * 1024), "5.0 MiB");
    }

    #[test]
    fn a_cache_path_cannot_leave_the_runs_directory() {
        let path = cache_path("run/1", "../../etc/passwd").unwrap();
        let root = crate::httpcache::cache_root().unwrap().join("run-files");
        assert_eq!(path, root.join("run_1").join("etc").join("passwd"));
        let path = cache_path("r", "climb[kern1]/best.patch").unwrap();
        assert!(path.ends_with("r/climb[kern1]/best.patch"));
    }
}
