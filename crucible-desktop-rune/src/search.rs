//! Searching every run of a pack: what the cache stores for a run (its session, its console, its
//! launch's params) cut into lines an FTS5 trigram index matches by substring, case-insensitive,
//! with near misses ranked after exact hits.
//!
//! ```text
//!   cached response ──► docs(path, body) ──► search table (fts5, trigram)
//!                                             │
//!   query "XYZ-69" ──► "XYZ-69" phrase ───────┤──► exact hits, best first
//!                  └─► trigrams OR'd ─────────┘──► then near misses
//! ```

use serde_json::Value;

use crate::logline::LogLine;
use crate::session::RunFold;

/// Lines kept from one response, so a huge console cannot crowd out everything else.
const DOC_LINES: usize = 20_000;
/// Characters kept of one line.
const LINE_CHARS: usize = 600;

/// One searchable line and where it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    pub text: String,
    /// The task or fan-out instance it belongs to, when it has one.
    pub task: String,
    pub source: Source,
    /// Its position within the response, for opening it.
    pub n: usize,
}

/// What a line was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Output,
    Transcript,
    Console,
    Params,
}

impl Source {
    pub fn id(self) -> &'static str {
        match self {
            Source::Output => "output",
            Source::Transcript => "transcript",
            Source::Console => "console",
            Source::Params => "params",
        }
    }

    pub fn from_id(id: &str) -> Source {
        match id {
            "output" => Source::Output,
            "transcript" => Source::Transcript,
            "params" => Source::Params,
            _ => Source::Console,
        }
    }
}

/// What one cached response adds to the index.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Indexed {
    /// The run the lines belong to; empty for a launch's own record.
    pub run: String,
    /// The launch and its playbook, when the response names them.
    pub launch: String,
    pub playbook: String,
    /// The runs a launch record lists, to tie their lines to it.
    pub runs: Vec<String>,
    pub docs: Vec<Doc>,
}

/// The lines `body`, cached at `path`, adds; `None` for a response nothing searches.
pub fn docs(path: &str, body: &[u8]) -> Option<Indexed> {
    let rest = path.strip_prefix("/api/")?;
    if let Some(run) = rest
        .strip_prefix("runs/")
        .and_then(|r| r.strip_suffix("/artifacts/session.jsonl"))
    {
        return Some(with_launch(session_docs(&decode(run), body)));
    }
    if let Some(run) = rest
        .strip_prefix("runs/")
        .and_then(|r| r.strip_suffix("/log"))
    {
        let log: crux::dto::RunLog = serde_json::from_slice(body).ok()?;
        let docs = log
            .text
            .unwrap_or_default()
            .lines()
            .take(DOC_LINES)
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(n, line)| {
                let parsed = LogLine::parse(line);
                Doc {
                    text: clip(line),
                    task: parsed.task.unwrap_or_default(),
                    source: Source::Console,
                    n,
                }
            })
            .collect();
        return Some(with_launch(Indexed {
            run: decode(run),
            docs,
            ..Indexed::default()
        }));
    }
    let key = decode(rest.strip_prefix("playbook-runs/")?);
    let record: Value = serde_json::from_slice(body).ok()?;
    let mut docs = Vec::new();
    flatten("", &record["launch"]["params"], &mut docs);
    let runs = record["runs"]
        .as_array()
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run["run_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let docs = docs
        .into_iter()
        .enumerate()
        .map(|(n, text)| Doc {
            text: clip(&text),
            task: String::new(),
            source: Source::Params,
            n,
        })
        .collect();
    Some(Indexed {
        playbook: playbook_of(&key),
        launch: key,
        runs,
        docs,
        ..Indexed::default()
    })
}

/// A run's agent transcript and every task's result, as lines.
fn session_docs(run: &str, body: &[u8]) -> Indexed {
    let text = String::from_utf8_lossy(body);
    let mut fold = RunFold::default();
    for line in text.lines() {
        fold.apply_line(line);
    }
    let mut docs = Vec::new();
    for line in fold.transcript.iter().take(DOC_LINES) {
        if line.raw.trim().is_empty() {
            continue;
        }
        docs.push(Doc {
            text: clip(&line.raw),
            task: line.task.clone().unwrap_or_default(),
            source: Source::Transcript,
            n: docs.len(),
        });
    }
    for task in &fold.tasks {
        let results = task.result.iter().map(|r| (task.name.clone(), r)).chain(
            task.instances
                .iter()
                .filter_map(|i| Some((format!("{}[{}]", task.name, i.key), i.result.as_ref()?))),
        );
        for (name, result) in results {
            let mut lines = Vec::new();
            if !result.note.trim().is_empty() {
                lines.push(result.note.clone());
            }
            if let Some(output) = &result.output {
                flatten("", output, &mut lines);
            }
            for text in lines {
                docs.push(Doc {
                    text: clip(&text),
                    task: name.clone(),
                    source: Source::Output,
                    n: docs.len(),
                });
            }
        }
    }
    Indexed {
        run: run.to_string(),
        docs,
        ..Indexed::default()
    }
}

/// `value`'s leaves as `path: value` lines.
fn flatten(path: &str, value: &Value, out: &mut Vec<String>) {
    if out.len() >= DOC_LINES {
        return;
    }
    match value {
        Value::Object(fields) => {
            for (name, field) in fields {
                let path = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path}.{name}")
                };
                flatten(&path, field, out);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                flatten(&format!("{path}[{i}]"), item, out);
            }
        }
        Value::String(text) => out.push(format!("{path}: {text}")),
        Value::Null => {}
        other => out.push(format!("{path}: {other}")),
    }
}

fn clip(text: &str) -> String {
    match text.char_indices().nth(LINE_CHARS) {
        Some((at, _)) => format!("{}\u{2026}", &text[..at]),
        None => text.to_string(),
    }
}

/// A path segment, percent-decoded.
fn decode(segment: &str) -> String {
    crate::link::decode(segment).unwrap_or_else(|| segment.to_string())
}

/// `indexed` with the launch its run id names, when it names one.
fn with_launch(mut indexed: Indexed) -> Indexed {
    if let Some((launch, playbook)) = launch_of_run(&indexed.run) {
        indexed.launch = launch;
        indexed.playbook = playbook;
    }
    indexed
}

/// The launch key and playbook a run id (`playbook_<id>_<launch>-<n>`) carries.
pub fn launch_of_run(run: &str) -> Option<(String, String)> {
    let rest = run.strip_prefix("playbook_")?;
    let (playbook, launch) = rest.rsplit_once('_')?;
    let (launch, _) = launch.rsplit_once('-')?;
    (!playbook.is_empty() && !launch.is_empty()).then(|| {
        (
            format!("playbook:{playbook}:{launch}"),
            playbook.to_string(),
        )
    })
}

/// The playbook a launch key (`playbook:<id>:<launch>`) names.
pub fn playbook_of(key: &str) -> String {
    key.strip_prefix("playbook:")
        .and_then(|rest| rest.rsplit_once(':'))
        .map(|(playbook, _)| playbook.to_string())
        .unwrap_or_default()
}

/// The FTS5 query that matches `query` as one phrase, quoted so its punctuation is literal.
pub fn phrase(query: &str) -> Option<String> {
    let query = query.trim();
    (query.chars().count() >= 3).then(|| format!("\"{}\"", query.replace('"', "\"\"")))
}

/// The FTS5 query matching any of `query`'s trigrams, so a line sharing most of them ranks
/// first: a misspelt or partly remembered key still finds its lines.
pub fn near(query: &str) -> Option<String> {
    let chars: Vec<char> = query.trim().to_lowercase().chars().collect();
    if chars.len() < 4 {
        return None;
    }
    let mut grams: Vec<String> = chars
        .windows(3)
        .map(|w| w.iter().collect::<String>())
        .filter(|g| !g.trim().is_empty())
        .map(|g| format!("\"{}\"", g.replace('"', "\"\"")))
        .collect();
    grams.dedup();
    (!grams.is_empty()).then(|| grams.join(" OR "))
}

/// Whether `text` holds at least seven in ten of `query`'s trigrams, ignoring case.
pub fn close(query: &str, text: &str) -> bool {
    let query: Vec<char> = query.trim().to_lowercase().chars().collect();
    let text = text.to_lowercase();
    let grams: Vec<String> = query.windows(3).map(|w| w.iter().collect()).collect();
    if grams.is_empty() {
        return false;
    }
    let held = grams.iter().filter(|g| text.contains(g.as_str())).count();
    held * 10 >= grams.len() * 7
}

/// One line that matched, and the run and launch it belongs to.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub launch: String,
    pub run: String,
    pub task: String,
    pub source: Source,
    pub n: usize,
    pub text: String,
    /// Matched the whole query, not only some of its trigrams.
    pub exact: bool,
}

/// Hits grouped by launch, launches in the order their best hit came.
pub fn by_launch(hits: &[Hit]) -> Vec<(&str, Vec<&Hit>)> {
    let mut groups: Vec<(&str, Vec<&Hit>)> = Vec::new();
    for hit in hits {
        match groups.iter_mut().find(|(launch, _)| *launch == hit.launch) {
            Some((_, group)) => group.push(hit),
            None => groups.push((&hit.launch, vec![hit])),
        }
    }
    groups
}

#[cfg(test)]
mod tests {
    use crate::search::*;

    #[test]
    fn a_launch_record_indexes_its_params_and_ties_its_runs() {
        let body = serde_json::json!({
            "launch": {"params": {"ticket": "XYZ-69", "repos": ["a/b", "c/d"], "deep": {"n": 3}},
                "schema_digest": "d", "schema_drifted": false, "max_cost": 1.0,
                "max_time": "10m"},
            "source_exists": true,
            "dispatch": {},
            "runs": [{"run_id": "run-1"}]
        });
        let indexed = docs(
            "/api/playbook-runs/playbook%3Acve%3A01a2",
            &serde_json::to_vec(&body).unwrap(),
        );
        let Some(indexed) = indexed else {
            panic!("a launch record is indexed")
        };
        assert_eq!(indexed.launch, "playbook:cve:01a2");
        assert_eq!(indexed.playbook, "cve");
        assert_eq!(indexed.runs, ["run-1"]);
        let lines: Vec<&str> = indexed.docs.iter().map(|d| d.text.as_str()).collect();
        assert!(lines.contains(&"ticket: XYZ-69"), "{lines:?}");
        assert!(lines.contains(&"repos[1]: c/d"), "{lines:?}");
        assert!(lines.contains(&"deep.n: 3"), "{lines:?}");
    }

    #[test]
    fn a_console_indexes_its_lines_with_their_task() {
        let body = serde_json::json!({"text": "[crucible] start\n\n[triage] looked at XYZ-69\n"});
        let Some(indexed) = docs("/api/runs/run-1/log", &serde_json::to_vec(&body).unwrap()) else {
            panic!("a console is indexed")
        };
        assert_eq!(indexed.run, "run-1");
        assert_eq!(indexed.docs.len(), 2, "blank lines are dropped");
        assert_eq!(indexed.docs[1].task, "triage");
        assert_eq!(indexed.docs[1].n, 2, "its line in the console");
    }

    #[test]
    fn what_nothing_searches_is_not_indexed() {
        assert_eq!(docs("/api/secrets", b"[]"), None);
        assert_eq!(docs("/api/runs/run-1/files", b"[]"), None);
    }

    #[test]
    fn a_query_is_a_quoted_phrase_and_near_misses_share_trigrams() {
        assert_eq!(phrase("XYZ-69"), Some("\"XYZ-69\"".to_string()));
        assert_eq!(phrase("say \"hi\""), Some("\"say \"\"hi\"\"\"".to_string()));
        assert_eq!(phrase("ab"), None, "trigrams need three characters");
        assert_eq!(
            near("XYZ-6"),
            Some("\"xyz\" OR \"yz-\" OR \"z-6\"".to_string())
        );
        assert_eq!(near("abc"), None);
    }

    #[test]
    fn a_near_miss_holds_most_of_the_query() {
        assert!(close("RHAI-21041", "ticket RHAI-21014"), "a transposition");
        assert!(
            !close("RHAI-21041", "members[8]: RHAI-20416"),
            "a shared prefix is not enough"
        );
        assert!(!close("RHAI-21041", "thinking 1041 chars"));
        assert!(!close("ab", "ab"));
    }

    #[test]
    fn run_ids_name_their_launch() {
        assert_eq!(
            launch_of_run(
                "playbook_cve-triage-llm-d_01a1bd9a-c83b-713f-8d50-bfd931b79f2b-1794182400"
            ),
            Some((
                "playbook:cve-triage-llm-d:01a1bd9a-c83b-713f-8d50-bfd931b79f2b".to_string(),
                "cve-triage-llm-d".to_string()
            ))
        );
        assert_eq!(launch_of_run("run-1"), None);
    }

    #[test]
    fn launch_keys_name_their_playbook() {
        assert_eq!(
            playbook_of("playbook:cve-triage-llm-d:01a2"),
            "cve-triage-llm-d"
        );
        assert_eq!(playbook_of("nonsense"), "");
    }
}
