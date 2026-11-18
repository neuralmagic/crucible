//! The run finder: asks the cache's search index on a task of its own and, while it answers, pulls
//! the runs of the pack being searched into the cache, so the index covers every run and not only
//! the ones opened.

use std::sync::{Arc, Mutex};

use rune::{TextEdit, Waker};
use serde_json::Value;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::httpcache::Store;
use crate::net::{Connection, Connector, Task, within};
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

/// The UI-side handle to the worker. Dropping it ends its task.
pub struct Client {
    tx: UnboundedSender<Ask>,
    shared: Arc<Mutex<(u64, Found, Option<Waker>)>>,
    _task: Option<Task>,
}

impl Client {
    pub fn spawn(connector: Connector) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Mutex::new((0, Found::default(), None)));
        let task = match connector.runtime() {
            Ok(runtime) => Some(Task::spawn(
                &runtime,
                run(connector, rx, Arc::clone(&shared)),
            )),
            Err(why) => {
                publish(
                    &shared,
                    Found {
                        error: Some(why),
                        ..Found::default()
                    },
                );
                None
            }
        };
        Client {
            tx,
            shared,
            _task: task,
        }
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

/// `work` on the store, on the blocking pool.
async fn on_store<T: Send + 'static>(
    store: &Arc<Store>,
    work: impl FnOnce(&Store) -> T + Send + 'static,
) -> Option<T> {
    let store = Arc::clone(store);
    crate::net::blocking(move || work(&store)).await
}

async fn run(
    connector: Connector,
    mut rx: UnboundedReceiver<Ask>,
    shared: Arc<Mutex<(u64, Found, Option<Waker>)>>,
) {
    let Some(store) = connector.store().cloned() else {
        publish(
            &shared,
            Found {
                error: Some("no cache to search".to_string()),
                ..Found::default()
            },
        );
        return;
    };
    let mut pending: Vec<String> = Vec::new();
    let mut ask: Option<Ask> = None;
    loop {
        let mut newest = if pending.is_empty() {
            match rx.recv().await {
                Some(next) => Some(next),
                None => return,
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
        let c = match connector.connect().await {
            Ok(open) => open,
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
        };
        let base = c.client.base().to_string();
        if let Some(key) = pending.first().cloned() {
            pull(&c, &store, &base, &key).await;
            pending.remove(0);
        }
        let total = current.launches.len();
        let (within_base, playbook, query) = (
            base.clone(),
            current.playbook.clone(),
            current.query.clone(),
        );
        let hits = on_store(&store, move |store| {
            store.search(&within_base, &playbook, &query, HITS)
        })
        .await
        .unwrap_or_default();
        publish(
            &shared,
            Found {
                seq: current.seq,
                hits,
                indexed: total - pending.len(),
                total,
                ..Found::default()
            },
        );
    }
}

/// Read launch `key`'s record, and each of its runs' session and console, through the cache,
/// which indexes them as it stores them. What the index already holds is not asked again.
async fn pull(c: &Connection, store: &Arc<Store>, base: &str, key: &str) {
    let encode = crux::client::encode;
    let record_path = format!("/api/playbook-runs/{}", encode(key));
    let (at, path) = (base.to_string(), record_path);
    let cached = on_store(store, move |store| {
        store.indexed(&at, &path).then(|| {
            store
                .get(&at, &path)
                .and_then(|(body, _)| serde_json::from_slice::<Value>(&body).ok())
        })
    })
    .await
    .flatten();
    let record: Option<Value> = match cached {
        Some(record) => record,
        None => within("the launch", c.client.playbook_run::<Value>(key))
            .await
            .ok(),
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
        let log = format!("/api/runs/{}/log", encode(&run));
        let at = base.to_string();
        let (has_session, has_log) = on_store(store, move |store| {
            (store.indexed(&at, &session), store.indexed(&at, &log))
        })
        .await
        .unwrap_or((false, false));
        if !has_session {
            let _ = within("the session", c.client.run_session(&run)).await;
        }
        if !has_log {
            let _ = within("the console", c.client.run_log::<Value>(&run)).await;
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
        if self.remote.finder.take().is_some() {
            return;
        }
        let playbook = match &self.selection {
            Some(crate::Selection::Live { playbook, .. }) => playbook.clone(),
            _ => String::new(),
        };
        let mut query = TextEdit::new("");
        query.focused = true;
        self.remote.finder = Some(Finder {
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
        let launches: Vec<String> = match &self.remote.finder {
            Some(finder) => {
                let mut launches: Vec<&crate::live::LivePlaybook> = self
                    .remote
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
        let Some(finder) = &mut self.remote.finder else {
            return;
        };
        finder.seq += 1;
        finder.selected = 0;
        self.remote.finder_client.ask(Ask {
            seq: finder.seq,
            query: finder.query.text.clone(),
            playbook: finder.playbook.clone(),
            launches,
        });
    }

    pub fn apply_found(&mut self, found: Found) {
        if let Some(finder) = &mut self.remote.finder
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
        if let Some(finder) = &mut self.remote.finder {
            let last = finder.found.hits.len().saturating_sub(1) as i32;
            finder.selected = (finder.selected as i32 + by).clamp(0, last) as usize;
        }
    }

    /// Open the run a hit came from, at its task, with the log filtered to the query.
    pub fn open_hit(&mut self, index: Option<usize>) {
        let Some(finder) = self.remote.finder.take() else {
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
            self.run.expanded = Some(hit.task.clone());
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
