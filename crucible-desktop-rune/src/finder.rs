//! The run finder: asks the cache's search index on a worker thread and, while it answers, pulls
//! the runs of the pack being searched into the cache, so the index covers every run and not only
//! the ones opened.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use rune::{TextEdit, Waker};
use serde_json::Value;

use crate::live::{Cache, Connection};
use crate::search::Hit;

/// Hits read for one query.
const HITS: usize = 2_000;
/// Lines shown under each launch; the rest are counted.
const PER_LAUNCH: usize = 5;

/// The finder overlay, while open.
pub struct Finder {
    pub query: TextEdit,
    /// The playbook searched; empty searches every one.
    pub playbook: String,
    /// The highlighted hit, an index into `found.hits`.
    pub selected: usize,
    pub found: Found,
    seq: u64,
}

/// What the worker last answered.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    pub seq: u64,
    pub hits: Vec<Hit>,
    /// How many lines matched in each launch, shown or not.
    pub matched: Vec<(String, usize)>,
    /// Launches read into the index of those searched.
    pub indexed: usize,
    pub total: usize,
    pub error: Option<String>,
}

struct Ask {
    seq: u64,
    query: String,
    playbook: String,
    /// The launches to read into the index, newest first.
    launches: Vec<String>,
}

/// The UI-side handle to the worker. Dropping it stops the thread.
pub struct Client {
    tx: Sender<Ask>,
    shared: Arc<Mutex<(u64, Found, Option<Waker>)>>,
}

impl Client {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new((0, Found::default(), None)));
        let worker = Arc::clone(&shared);
        let _ = std::thread::Builder::new()
            .name("crucible-playbooks-finder".into())
            .spawn(move || run(rx, worker));
        Client { tx, shared }
    }

    pub fn set_waker(&self, waker: Waker) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.2 = Some(waker);
        }
    }

    fn ask(&self, ask: Ask) {
        let _ = self.tx.send(ask);
    }

    pub fn take(&self, seen: u64) -> Option<(u64, Found)> {
        let shared = self.shared.lock().ok()?;
        (shared.0 > seen).then(|| (shared.0, shared.1.clone()))
    }
}

fn publish(shared: &Mutex<(u64, Found, Option<Waker>)>, found: Found) {
    let waker = match shared.lock() {
        Ok(mut shared) => {
            shared.0 += 1;
            shared.1 = found;
            shared.2.clone()
        }
        Err(_) => return,
    };
    if let Some(waker) = waker {
        waker();
    }
}

fn run(rx: Receiver<Ask>, shared: Arc<Mutex<(u64, Found, Option<Waker>)>>) {
    let Some(cache) = Cache::open() else {
        publish(
            &shared,
            Found {
                error: Some("no cache to search".to_string()),
                ..Found::default()
            },
        );
        return;
    };
    let mut connection: Option<Connection> = None;
    let mut pending: Vec<String> = Vec::new();
    let mut ask: Option<Ask> = None;
    loop {
        let mut newest = if pending.is_empty() {
            match rx.recv() {
                Ok(next) => Some(next),
                Err(_) => return,
            }
        } else {
            None
        };
        while let Ok(next) = rx.try_recv() {
            newest = Some(next);
        }
        if let Some(next) = newest {
            pending = next.launches.clone();
            ask = Some(next);
        }
        let Some(current) = &ask else { continue };
        if connection.is_none() {
            match Connection::open(Some(&cache)) {
                Ok(open) => connection = Some(open),
                Err(why) => {
                    publish(
                        &shared,
                        Found {
                            seq: current.seq,
                            error: Some(why),
                            ..Found::default()
                        },
                    );
                    pending.clear();
                    continue;
                }
            }
        }
        let Some(c) = &connection else { continue };
        let base = c.client.base().to_string();
        if let Some(key) = pending.first().cloned() {
            pull(c, &cache, &base, &key);
            pending.remove(0);
        }
        let total = current.launches.len();
        publish(
            &shared,
            Found {
                seq: current.seq,
                hits: cache
                    .store
                    .search(&base, &current.playbook, &current.query, HITS),
                indexed: total - pending.len(),
                total,
                ..Found::default()
            },
        );
    }
}

/// Read launch `key`'s record, and each of its runs' session and console, through the cache,
/// which indexes them as it stores them. What the index already holds is not asked again.
fn pull(c: &Connection, cache: &Cache, base: &str, key: &str) {
    let encode = crux::client::encode;
    let record_path = format!("/api/playbook-runs/{}", encode(key));
    let record: Option<Value> = if cache.store.indexed(base, &record_path) {
        cache
            .store
            .get(base, &record_path)
            .and_then(|(body, _)| serde_json::from_slice(&body).ok())
    } else {
        c.call("the launch", c.client.playbook_run::<Value>(key))
            .ok()
    };
    let runs: Vec<String> = record
        .as_ref()
        .and_then(|r| r["runs"].as_array())
        .map(|runs| {
            runs.iter()
                .filter_map(|run| run["run_id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for run in runs {
        let session = format!("/api/runs/{}/artifacts/session.jsonl", encode(&run));
        if !cache.store.indexed(base, &session) {
            let _ = c.call("the session", c.client.run_session(&run));
        }
        let log = format!("/api/runs/{}/log", encode(&run));
        if !cache.store.indexed(base, &log) {
            let _ = c.call("the console", c.client.run_log::<Value>(&run));
        }
    }
}

/// `hits` grouped by launch, at most [`PER_LAUNCH`] each, and how many each launch matched.
fn shown(hits: &[Hit]) -> (Vec<Hit>, Vec<(String, usize)>) {
    let groups = crate::search::by_launch(hits);
    let matched = groups
        .iter()
        .map(|(launch, hits)| (launch.to_string(), hits.len()))
        .collect();
    let shown = groups
        .into_iter()
        .flat_map(|(_, hits)| hits.into_iter().take(PER_LAUNCH).cloned())
        .collect();
    (shown, matched)
}

impl crate::App {
    /// Open the finder on the selected playbook, or close it.
    pub fn toggle_finder(&mut self) {
        if self.finder.take().is_some() {
            return;
        }
        let playbook = match &self.selection {
            Some(crate::Selection::Live { playbook, .. }) => playbook.clone(),
            _ => String::new(),
        };
        let mut query = TextEdit::new("");
        query.focused = true;
        self.finder = Some(Finder {
            query,
            playbook,
            selected: 0,
            found: Found::default(),
            seq: 0,
        });
        self.ask_finder();
    }

    /// Search the open finder's query, and read its launches into the index.
    pub fn ask_finder(&mut self) {
        let launches: Vec<String> = match &self.finder {
            Some(finder) => {
                let mut launches: Vec<&crate::live::LivePlaybook> = self
                    .live
                    .launches()
                    .iter()
                    .filter(|l| finder.playbook.is_empty() || l.id == finder.playbook)
                    .collect();
                launches.sort_by_key(|l| std::cmp::Reverse(l.landed));
                launches.into_iter().map(|l| l.key.clone()).collect()
            }
            None => return,
        };
        let Some(finder) = &mut self.finder else {
            return;
        };
        finder.seq += 1;
        finder.selected = 0;
        self.finder_client.ask(Ask {
            seq: finder.seq,
            query: finder.query.text.clone(),
            playbook: finder.playbook.clone(),
            launches,
        });
    }

    pub fn apply_found(&mut self, found: Found) {
        if let Some(finder) = &mut self.finder
            && found.seq == finder.seq
        {
            let mut found = found;
            let (hits, matched) = shown(&found.hits);
            found.hits = hits;
            found.matched = matched;
            finder.selected = finder.selected.min(found.hits.len().saturating_sub(1));
            finder.found = found;
        }
    }

    pub fn step_finder(&mut self, by: i32) {
        if let Some(finder) = &mut self.finder {
            let last = finder.found.hits.len().saturating_sub(1) as i32;
            finder.selected = (finder.selected as i32 + by).clamp(0, last) as usize;
        }
    }

    /// Open the run a hit came from, at its task, with the log filtered to the query.
    pub fn open_hit(&mut self, index: Option<usize>) {
        let Some(finder) = self.finder.take() else {
            return;
        };
        let Some(hit) = finder
            .found
            .hits
            .get(index.unwrap_or(finder.selected))
            .cloned()
        else {
            return;
        };
        let playbook = crate::search::playbook_of(&hit.launch);
        if playbook.is_empty() {
            return;
        }
        self.backend = crate::Backend::Controller;
        self.select(crate::Selection::Live {
            playbook,
            launch: Some(hit.launch.clone()),
        });
        if !hit.task.is_empty() {
            self.expanded = Some(hit.task.clone());
        }
        let pane = match hit.source {
            crate::search::Source::Console => {
                self.log_source = crate::LogSource::Console;
                crate::workspace::PaneKind::Log
            }
            crate::search::Source::Transcript => {
                self.log_source = crate::LogSource::Transcript;
                crate::workspace::PaneKind::Log
            }
            crate::search::Source::Output => crate::workspace::PaneKind::Data,
            crate::search::Source::Params => crate::workspace::PaneKind::Launch,
        };
        if matches!(pane, crate::workspace::PaneKind::Log) {
            self.log_search = TextEdit::new(finder.query.text.trim());
        }
        self.show_pane(pane);
    }
}

#[cfg(test)]
mod tests {
    use crate::finder::{PER_LAUNCH, shown};
    use crate::search::{Hit, Source};

    fn hit(launch: &str, n: usize) -> Hit {
        Hit {
            launch: launch.into(),
            run: "r".into(),
            task: "t".into(),
            source: Source::Output,
            n,
            text: format!("line {n}"),
            exact: true,
        }
    }

    #[test]
    fn one_busy_launch_does_not_crowd_out_the_others() {
        let mut hits: Vec<Hit> = (0..40).map(|n| hit("a", n)).collect();
        hits.push(hit("b", 0));
        hits.push(hit("a", 99));
        let (shown, matched) = shown(&hits);
        assert_eq!(matched, [("a".to_string(), 41), ("b".to_string(), 1)]);
        assert_eq!(shown.len(), PER_LAUNCH + 1);
        assert!(shown[..PER_LAUNCH].iter().all(|h| h.launch == "a"));
        assert_eq!(shown[PER_LAUNCH].launch, "b");
    }
}
