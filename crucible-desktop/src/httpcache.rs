//! A controller transport that keeps what it reads in SQLite, so the app opens a finished run
//! without asking again and still shows what it last saw when the controller cannot be reached.
//!
//! ```text
//!   GET path ──► settled copy on disk? ──yes──► served from disk
//!                    │ no
//!                    ▼
//!               controller ──ok──► stored, served
//!                    │ unreachable
//!                    ▼
//!               last copy on disk ──► served, offline flag set
//! ```
//!
//! A copy is settled when it can never change: a draft's files at a given version, or the
//! session of a run that has shut down.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use crux::client::{Credential, Endpoint, Transport};
use crux::sse::SseEvent;
use reqwest::{Method, StatusCode};
use rusqlite::{Connection, OptionalExtension, params};

use crate::search::{self, Hit, Source};

/// The most bytes the cache keeps before it drops what was used longest ago.
const BUDGET: i64 = 512 * 1024 * 1024;

/// Where the app keeps what it can rebuild: `~/Library/Caches` on macOS, else
/// `$XDG_CACHE_HOME` or `~/.cache`.
pub fn cache_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home?.join("Library/Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".cache")))?
    };
    Some(base.join("crucible-playbooks"))
}

/// Responses by controller and path, with when each was last used.
pub struct Store {
    db: Mutex<Connection>,
    budget: i64,
    /// Bytes of bodies held, counted once at open and kept in step under the `db` lock.
    held: AtomicI64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        Store::with_budget(path, BUDGET)
    }

    fn with_budget(path: &Path, budget: i64) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS responses (
                 base TEXT NOT NULL,
                 path TEXT NOT NULL,
                 body BLOB NOT NULL,
                 settled INTEGER NOT NULL,
                 used INTEGER NOT NULL,
                 PRIMARY KEY (base, path)
             );
             CREATE INDEX IF NOT EXISTS responses_used ON responses (used);
             CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
                 text, task UNINDEXED, source UNINDEXED, n UNINDEXED, base UNINDEXED,
                 path UNINDEXED, run UNINDEXED, launch UNINDEXED, playbook UNINDEXED,
                 tokenize = 'trigram'
             );
             CREATE TABLE IF NOT EXISTS search_runs (
                 base TEXT NOT NULL,
                 run TEXT NOT NULL,
                 launch TEXT NOT NULL,
                 playbook TEXT NOT NULL,
                 PRIMARY KEY (base, run)
             );
             CREATE TABLE IF NOT EXISTS search_indexed (
                 base TEXT NOT NULL,
                 path TEXT NOT NULL,
                 PRIMARY KEY (base, path)
             );",
        )?;
        backfill(&db)?;
        let held = db.query_row(
            "SELECT COALESCE(SUM(LENGTH(body)), 0) FROM responses",
            [],
            |row| row.get(0),
        )?;
        Ok(Store {
            db: Mutex::new(db),
            budget,
            held: AtomicI64::new(held),
        })
    }

    /// The lines of `playbook`'s runs on `base` that hold `query`, at most `limit`. Only when
    /// none does are near misses offered: lines holding most of its trigrams, for a typo.
    pub fn search(&self, base: &str, playbook: &str, query: &str, limit: usize) -> Vec<Hit> {
        let db = self.db();
        if let Some(fts) = search::phrase(query) {
            let exact = query_hits(&db, base, playbook, &fts, limit, true);
            if !exact.is_empty() {
                return exact;
            }
        }
        let Some(fts) = search::near(query) else {
            return Vec::new();
        };
        query_hits(&db, base, playbook, &fts, limit * 4, false)
            .into_iter()
            .filter(|hit| search::close(query, &hit.text))
            .take(limit)
            .collect()
    }

    /// Whether `path` on `base` has been read into the index.
    pub fn indexed(&self, base: &str, path: &str) -> bool {
        self.db()
            .query_row(
                "SELECT 1 FROM search_indexed WHERE base = ?1 AND path = ?2",
                params![base, path],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The stored body and whether it is settled, marking it used.
    pub fn get(&self, base: &str, path: &str) -> Option<(Vec<u8>, bool)> {
        let db = self.db();
        let hit = db
            .query_row(
                "SELECT body, settled FROM responses WHERE base = ?1 AND path = ?2",
                params![base, path],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, bool>(1)?)),
            )
            .optional()
            .ok()
            .flatten()?;
        let _ = db.execute(
            "UPDATE responses SET used = ?3 WHERE base = ?1 AND path = ?2",
            params![base, path, now()],
        );
        Some(hit)
    }

    /// Keep `body`, then drop what was used longest ago until the cache fits its budget. A body
    /// the cache already holds is only marked used: nothing is written or indexed again.
    pub fn put(&self, base: &str, path: &str, body: &[u8], settled: bool) {
        let db = self.db();
        let before: Option<(i64, bool)> = db
            .query_row(
                "SELECT LENGTH(body), body = ?3 FROM responses WHERE base = ?1 AND path = ?2",
                params![base, path, body],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .ok()
            .flatten();
        if let Some((_, true)) = before {
            let _ = db.execute(
                "UPDATE responses SET used = ?3 WHERE base = ?1 AND path = ?2",
                params![base, path, now()],
            );
            return;
        }
        let stored = db.execute(
            "INSERT INTO responses (base, path, body, settled, used) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (base, path) DO UPDATE
             SET body = excluded.body, settled = excluded.settled, used = excluded.used",
            params![base, path, body, settled, now()],
        );
        if stored.is_err() {
            return;
        }
        let grew = body.len() as i64 - before.map_or(0, |(length, _)| length);
        let mut held = self.held.fetch_add(grew, Ordering::Relaxed) + grew;
        let _ = index(&db, base, path, body);
        while held > self.budget {
            let dropped: Option<i64> = db
                .query_row(
                    "DELETE FROM responses WHERE rowid IN
                     (SELECT rowid FROM responses ORDER BY used LIMIT 1)
                     RETURNING LENGTH(body)",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .ok()
                .flatten();
            let Some(dropped) = dropped else { break };
            held = self.held.fetch_sub(dropped, Ordering::Relaxed) - dropped;
        }
    }
}

/// Put the searchable lines of `body`, cached at `path`, in the index in place of what it held.
fn index(db: &Connection, base: &str, path: &str, body: &[u8]) -> rusqlite::Result<()> {
    let Some(indexed) = search::docs(path, body) else {
        return Ok(());
    };
    let tx = db.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM search WHERE base = ?1 AND path = ?2",
        params![base, path],
    )?;
    {
        let mut insert = tx.prepare_cached(
            "INSERT INTO search (text, task, source, n, base, path, run, launch, playbook)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for doc in &indexed.docs {
            insert.execute(params![
                doc.text,
                doc.task,
                doc.source.id(),
                doc.n as i64,
                base,
                path,
                indexed.run,
                indexed.launch,
                indexed.playbook,
            ])?;
        }
    }
    for run in &indexed.runs {
        tx.execute(
            "INSERT OR REPLACE INTO search_runs (base, run, launch, playbook)
             VALUES (?1, ?2, ?3, ?4)",
            params![base, run, indexed.launch, indexed.playbook],
        )?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO search_indexed (base, path) VALUES (?1, ?2)",
        params![base, path],
    )?;
    tx.commit()
}

/// Index what the cache already holds and the index does not.
fn backfill(db: &Connection) -> rusqlite::Result<()> {
    let missing: Vec<(String, String, Vec<u8>)> = {
        let mut rows = db.prepare(
            "SELECT r.base, r.path, r.body FROM responses r
             LEFT JOIN search_indexed i ON i.base = r.base AND i.path = r.path
             WHERE i.path IS NULL",
        )?;
        rows.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (base, path, body) in missing {
        if search::docs(&path, &body).is_some() {
            index(db, &base, &path, &body)?;
        }
    }
    Ok(())
}

fn query_hits(
    db: &Connection,
    base: &str,
    playbook: &str,
    fts: &str,
    limit: usize,
    exact: bool,
) -> Vec<Hit> {
    let Ok(mut rows) = db.prepare_cached(
        "SELECT s.text, s.task, s.source, s.n, s.run,
                COALESCE(NULLIF(s.launch, ''), r.launch, '') AS launch
         FROM search s
         LEFT JOIN search_runs r ON r.base = s.base AND r.run = s.run
         WHERE search MATCH ?1 AND s.base = ?2
           AND (?3 = '' OR COALESCE(NULLIF(s.playbook, ''), r.playbook, '') = ?3)
         ORDER BY rank
         LIMIT ?4",
    ) else {
        return Vec::new();
    };
    rows.query_map(params![fts, base, playbook, limit as i64], |row| {
        Ok(Hit {
            text: row.get(0)?,
            task: row.get(1)?,
            source: Source::from_id(&row.get::<_, String>(2)?),
            n: row.get::<_, i64>(3)? as usize,
            run: row.get(4)?,
            launch: row.get(5)?,
            exact,
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

/// How many of a session's last lines are read for its shutdown event.
const TAIL_LINES: usize = 16;

/// Whether a response at `path` can never change.
fn settled(path: &str, body: &[u8]) -> bool {
    let draft_at_version = path.starts_with("/api/playbook-drafts/")
        && path.contains("/files?")
        && path.contains("version=");
    draft_at_version || (path.ends_with("/artifacts/session.jsonl") && shut_down(body))
}

/// Whether one of a session's last events is its shutdown.
fn shut_down(session: &[u8]) -> bool {
    session
        .rsplit(|byte| *byte == b'\n')
        .filter(|line| !line.trim_ascii().is_empty())
        .take(TAIL_LINES)
        .any(|line| {
            serde_json::from_slice::<serde_json::Value>(line)
                .is_ok_and(|event| event["kind"] == "shutdown")
        })
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `inner` with every GET read through [`Store`].
pub struct CachedTransport {
    inner: Arc<dyn Transport>,
    store: Arc<Store>,
    /// Set while answers come from disk because the controller could not be reached.
    offline: Arc<AtomicBool>,
}

impl CachedTransport {
    pub fn new(inner: Arc<dyn Transport>, store: Arc<Store>, offline: Arc<AtomicBool>) -> Self {
        CachedTransport {
            inner,
            store,
            offline,
        }
    }

    /// The stored copy of `path`.
    async fn stored(&self, path: &str) -> Option<(Vec<u8>, bool)> {
        let (store, base, path) = (
            Arc::clone(&self.store),
            self.inner.base().to_string(),
            path.to_string(),
        );
        tokio::task::spawn_blocking(move || store.get(&base, &path))
            .await
            .ok()
            .flatten()
    }

    /// Store `body` as `path`'s copy, and hand it back.
    async fn keep(&self, path: &str, body: Vec<u8>) -> Vec<u8> {
        let (store, base, path) = (
            Arc::clone(&self.store),
            self.inner.base().to_string(),
            path.to_string(),
        );
        let body = Arc::new(body);
        let stored = Arc::clone(&body);
        let _ = tokio::task::spawn_blocking(move || {
            store.put(&base, &path, &stored, settled(&path, &stored));
        })
        .await;
        Arc::try_unwrap(body).unwrap_or_else(|shared| shared.to_vec())
    }

    async fn read(&self, path: &str) -> Result<(StatusCode, Vec<u8>)> {
        if let Some((body, true)) = self.stored(path).await {
            return Ok((StatusCode::OK, body));
        }
        match self.inner.get_bytes(path).await {
            Ok((status, body)) => {
                self.offline.store(false, Ordering::Relaxed);
                let body = if status.is_success() {
                    self.keep(path, body).await
                } else {
                    body
                };
                Ok((status, body))
            }
            Err(error) => match self.stored(path).await {
                Some((body, _)) => {
                    self.offline.store(true, Ordering::Relaxed);
                    Ok((StatusCode::OK, body))
                }
                None => Err(error),
            },
        }
    }
}

#[async_trait::async_trait]
impl Transport for CachedTransport {
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(StatusCode, String)> {
        if method != Method::GET {
            return self.inner.send(method, path, body).await;
        }
        let (status, bytes) = self.read(path).await?;
        Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
    }

    async fn get_bytes(&self, path: &str) -> Result<(StatusCode, Vec<u8>)> {
        self.read(path).await
    }

    async fn events(
        &self,
        path: &str,
        last_event_id: Option<&str>,
        on_event: &mut (dyn FnMut(SseEvent) -> Result<()> + Send),
    ) -> Result<(StatusCode, String)> {
        self.inner.events(path, last_event_id, on_event).await
    }

    fn base(&self) -> &str {
        self.inner.base()
    }

    fn endpoint(&self) -> Endpoint {
        self.inner.endpoint()
    }

    fn credential(&self) -> Credential {
        self.inner.credential()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crux::client::{HttpTransport, Transport};
    use crux::config::{Auth, Config};

    use crate::httpcache::{CachedTransport, Store, settled};
    use crate::search::{Hit, Source};

    /// A real HTTP server on a loopback port answering every request with `body`, counting them.
    fn serve(body: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counted = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                counted.fetch_add(1, Ordering::SeqCst);
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, hits)
    }

    fn transport(url: &str, store: Arc<Store>) -> (CachedTransport, Arc<AtomicBool>) {
        let inner = HttpTransport::connect(&Config {
            url: url.to_string(),
            auth: Auth::None,
        })
        .unwrap();
        let offline = Arc::new(AtomicBool::new(false));
        (
            CachedTransport::new(Arc::new(inner), store, offline.clone()),
            offline,
        )
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn a_settled_copy_is_served_without_asking_and_a_live_one_is_asked_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("cache.sqlite")).unwrap());
        let (url, hits) = serve(r#"{"v":1,"kind":"shutdown","outcome":"ok","reason":""}"#);
        let (cached, offline) = transport(&url, store);
        let rt = runtime();
        let session = "/api/runs/r1/artifacts/session.jsonl";
        for _ in 0..3 {
            let (status, body) = rt.block_on(cached.get_bytes(session)).unwrap();
            assert!(status.is_success() && body.starts_with(b"{\"v\":1"));
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "an ended session is read once"
        );
        for _ in 0..2 {
            rt.block_on(cached.send(reqwest::Method::GET, "/api/playbook-runs", None))
                .unwrap();
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            3,
            "a list is always asked again"
        );
        assert!(!offline.load(Ordering::SeqCst));
    }

    #[test]
    fn an_unreachable_controller_serves_the_last_copy_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("cache.sqlite")).unwrap());
        let (url, _) = serve(r#"[{"key":"a"}]"#);
        let rt = runtime();
        let (online, _) = transport(&url, store.clone());
        rt.block_on(online.send(reqwest::Method::GET, "/api/playbook-runs", None))
            .unwrap();

        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let gone = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let (away, offline) = transport(&gone, store.clone());
        assert!(
            rt.block_on(away.send(reqwest::Method::GET, "/api/playbook-runs", None))
                .is_err(),
            "another controller's copy is not this one's"
        );
        assert!(!offline.load(Ordering::SeqCst));
        store.put(&gone, "/api/playbook-runs", br#"[{"key":"b"}]"#, false);
        let (status, body) = rt
            .block_on(away.send(reqwest::Method::GET, "/api/playbook-runs", None))
            .unwrap();
        assert!(status.is_success());
        assert_eq!(body, r#"[{"key":"b"}]"#);
        assert!(offline.load(Ordering::SeqCst));
    }

    #[test]
    fn the_budget_drops_what_was_used_longest_ago() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::with_budget(&dir.path().join("cache.sqlite"), 25).unwrap();
        store.put("b", "/old", b"0123456789", false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.put("b", "/mid", b"0123456789", false);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(
            store.get("b", "/old").is_some(),
            "reading it makes it recent"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.put("b", "/new", b"0123456789", true);
        assert!(store.get("b", "/mid").is_none());
        assert!(store.get("b", "/old").is_some());
        assert_eq!(store.get("b", "/new"), Some((b"0123456789".to_vec(), true)));
    }

    #[test]
    fn only_versioned_drafts_and_ended_sessions_are_settled() {
        let ended = br#"{"v":1,"kind":"shutdown","outcome":"ok"}"#;
        assert!(settled("/api/runs/r/artifacts/session.jsonl", ended));
        assert!(!settled(
            "/api/runs/r/artifacts/session.jsonl",
            b"{\"kind\":\"task_started\"}"
        ));
        let quoted =
            br#"{"v":1,"kind":"note","text":"saw {\"kind\":\"shutdown\"} in the old log"}"#;
        assert!(
            !settled("/api/runs/r/artifacts/session.jsonl", quoted),
            "an event that quotes a shutdown is not one"
        );
        let mut running = ended.to_vec();
        running.extend_from_slice(b"\nnot json");
        assert!(settled("/api/runs/r/artifacts/session.jsonl", &running));
        let mut long = ended.to_vec();
        for _ in 0..40 {
            long.extend_from_slice(b"\n{\"kind\":\"task_started\"}");
        }
        assert!(
            !settled("/api/runs/r/artifacts/session.jsonl", &long),
            "a shutdown far from the end is not the run's last word"
        );
        assert!(settled("/api/playbook-drafts/d/files?version=4", b""));
        assert!(!settled("/api/playbook-drafts/d/files", b""));
        assert!(!settled("/api/runs/r/log", ended));
    }

    fn held(store: &Store) -> (i64, i64) {
        let summed = store
            .db()
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(body)), 0) FROM responses",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (store.held.load(Ordering::Relaxed), summed)
    }

    #[test]
    fn the_bytes_held_are_counted_through_replacing_dropping_and_reopening() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        {
            let store = Store::with_budget(&path, 40).unwrap();
            store.put("b", "/a", b"0123456789", false);
            store.put("b", "/b", b"0123456789", false);
            assert_eq!(held(&store), (20, 20));
            store.put("b", "/a", b"01234", false);
            assert_eq!(held(&store), (15, 15), "a shorter body gives bytes back");
            store.put("b", "/a", b"01234", false);
            assert_eq!(
                held(&store),
                (15, 15),
                "the same body again changes nothing"
            );
            store.put("b", "/c", &[b'x'; 30], false);
            let (counted, summed) = held(&store);
            assert_eq!(counted, summed);
            assert!(summed <= 40, "over budget, the oldest went: {summed}");
        }
        let reopened = Store::with_budget(&path, 40).unwrap();
        let (counted, summed) = held(&reopened);
        assert_eq!(counted, summed);
        assert!(summed > 0);
    }

    #[test]
    fn a_body_already_held_is_not_written_or_indexed_again() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cache.sqlite")).unwrap();
        let path = format!("/api/runs/{RUN}/artifacts/session.jsonl");
        let changes = |store: &Store| store.db().total_changes();
        let used = |store: &Store| -> i64 {
            store
                .db()
                .query_row("SELECT used FROM responses", [], |row| row.get(0))
                .unwrap()
        };
        let empty = changes(&store);
        store.put("b", &path, &session(), false);
        let (first, first_used) = (changes(&store), used(&store));
        assert!(first - empty > 3, "the body and its lines are written");

        std::thread::sleep(std::time::Duration::from_millis(5));
        store.put("b", &path, &session(), false);
        assert_eq!(
            changes(&store) - first,
            1,
            "the same body again only marks the row used"
        );
        assert!(used(&store) > first_used);

        let again = changes(&store);
        let mut grown = session();
        grown.extend_from_slice(b"\n{\"v\":1,\"kind\":\"shutdown\",\"outcome\":\"ok\"}");
        store.put("b", &path, &grown, true);
        assert!(
            changes(&store) - again > 3,
            "a changed body is written whole"
        );
        assert_eq!(store.get("b", &path), Some((grown, true)));
    }

    const RUN: &str = "playbook_cve_01a2-1";
    const LAUNCH: &str = "playbook:cve:01a2";

    fn session() -> Vec<u8> {
        [
            r#"{"v":1,"kind":"plan_admitted","plan_version":1,"budget_usd":1.0,"tasks":[{"name":"triage","kind":"command","depends_on":[],"required":true}]}"#,
            r#"{"v":1,"kind":"task_started","task":"triage","attempt":1,"estimate_usd":0.0}"#,
            r#"{"v":1,"kind":"task_result","task":"triage","status":"pass","attempts":1,"cost_usd":0.0,"output":{"tickets":["XYZ-69","XYZ-70"]},"note":""}"#,
        ]
        .join("\n")
        .into_bytes()
    }

    fn filled(store: &Store) {
        store.put(
            "b",
            &format!("/api/runs/{RUN}/artifacts/session.jsonl"),
            &session(),
            true,
        );
        store.put(
            "b",
            &format!("/api/runs/{RUN}/log"),
            br#"{"text":"[triage] fetched XYZ-69 from jira\n[triage] done\n"}"#,
            false,
        );
        store.put(
            "b",
            "/api/runs/playbook_other_01b3-1/log",
            br#"{"text":"[scan] XYZ-69 seen here too\n"}"#,
            false,
        );
    }

    #[test]
    fn a_ticket_is_found_in_a_runs_output_and_console_and_names_its_launch() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cache.sqlite")).unwrap();
        filled(&store);
        let hits: Vec<Hit> = store
            .search("b", "cve", "xyz-69", 20)
            .into_iter()
            .filter(|h| h.exact)
            .collect();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits.iter().all(|h| h.launch == LAUNCH && h.run == RUN));
        let output = hits
            .iter()
            .find(|h| h.source == Source::Output)
            .expect("output");
        assert_eq!(output.text, "tickets[0]: XYZ-69");
        assert_eq!(output.task, "triage");
        let console = hits
            .iter()
            .find(|h| h.source == Source::Console)
            .expect("console");
        assert_eq!((console.task.as_str(), console.n), ("triage", 0));
        let everywhere = store.search("b", "", "XYZ-69", 20);
        assert_eq!(everywhere.len(), 3, "no playbook searches every one");
        assert!(
            everywhere.iter().all(|h| h.exact),
            "near misses only when nothing holds the query"
        );
        assert!(
            store
                .search("other-controller", "", "XYZ-69", 20)
                .is_empty()
        );
    }

    #[test]
    fn a_near_miss_ranks_after_exact_hits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cache.sqlite")).unwrap();
        filled(&store);
        let hits = store.search("b", "cve", "XYZ-7O", 5);
        assert_eq!(
            hits.len(),
            1,
            "a typo finds its line and not ones sharing a prefix: {hits:?}"
        );
        assert!(!hits[0].exact);
        assert_eq!(hits[0].text, "tickets[1]: XYZ-70");
    }

    #[test]
    fn storing_a_response_again_replaces_its_lines() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cache.sqlite")).unwrap();
        let path = format!("/api/runs/{RUN}/log");
        store.put("b", &path, br#"{"text":"[t] XYZ-69\n"}"#, false);
        store.put("b", &path, br#"{"text":"[t] ABC-1\n"}"#, false);
        assert!(store.search("b", "", "XYZ-69", 5).is_empty());
        assert_eq!(store.search("b", "", "ABC-1", 5).len(), 1);
    }

    #[test]
    fn a_launch_record_ties_runs_its_id_does_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cache.sqlite")).unwrap();
        store.put(
            "b",
            "/api/runs/odd-run/log",
            br#"{"text":"XYZ-69\n"}"#,
            false,
        );
        let record = serde_json::json!({
            "launch": {"params": {}, "schema_digest": "", "schema_drifted": false,
                "max_cost": 1.0, "max_time": "5m"},
            "source_exists": true, "dispatch": {}, "runs": [{"run_id": "odd-run"}]
        });
        store.put(
            "b",
            "/api/playbook-runs/playbook%3Acve%3A01c4",
            &serde_json::to_vec(&record).unwrap(),
            false,
        );
        let hits = store.search("b", "cve", "XYZ-69", 5);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].launch, "playbook:cve:01c4");
    }

    #[test]
    fn opening_a_cache_indexes_what_it_already_held() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.sqlite");
        {
            let store = Store::open(&path).unwrap();
            filled(&store);
            let db = store.db();
            db.execute_batch("DELETE FROM search; DELETE FROM search_indexed;")
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let exact = store
            .search("b", "cve", "XYZ-69", 20)
            .iter()
            .filter(|h| h.exact)
            .count();
        assert_eq!(exact, 2);
        assert!(store.indexed("b", &format!("/api/runs/{RUN}/log")));
    }
}
