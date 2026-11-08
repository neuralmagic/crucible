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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::Result;
use crux::client::{Credential, Endpoint, Transport};
use crux::sse::SseEvent;
use reqwest::{Method, StatusCode};
use rusqlite::{Connection, OptionalExtension, params};

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
             CREATE INDEX IF NOT EXISTS responses_used ON responses (used);",
        )?;
        Ok(Store {
            db: Mutex::new(db),
            budget,
        })
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

    /// Keep `body`, then drop what was used longest ago until the cache fits its budget.
    pub fn put(&self, base: &str, path: &str, body: &[u8], settled: bool) {
        let db = self.db();
        let stored = db.execute(
            "INSERT INTO responses (base, path, body, settled, used) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (base, path) DO UPDATE
             SET body = excluded.body, settled = excluded.settled, used = excluded.used",
            params![base, path, body, settled, now()],
        );
        if stored.is_err() {
            return;
        }
        loop {
            let total: i64 = db
                .query_row(
                    "SELECT COALESCE(SUM(LENGTH(body)), 0) FROM responses",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            if total <= self.budget {
                break;
            }
            let dropped = db
                .execute(
                    "DELETE FROM responses WHERE rowid IN
                     (SELECT rowid FROM responses ORDER BY used LIMIT 1)",
                    [],
                )
                .unwrap_or(0);
            if dropped == 0 {
                break;
            }
        }
    }
}

/// Whether a response at `path` can never change.
fn settled(path: &str, body: &[u8]) -> bool {
    let draft_at_version = path.starts_with("/api/playbook-drafts/")
        && path.contains("/files?")
        && path.contains("version=");
    let ended_session = path.ends_with("/artifacts/session.jsonl")
        && body
            .windows(br#""kind":"shutdown""#.len())
            .any(|w| w == br#""kind":"shutdown""#);
    draft_at_version || ended_session
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

    async fn read(&self, path: &str) -> Result<(StatusCode, Vec<u8>)> {
        let base = self.inner.base().to_string();
        if let Some((body, true)) = self.store.get(&base, path) {
            return Ok((StatusCode::OK, body));
        }
        match self.inner.get_bytes(path).await {
            Ok((status, body)) => {
                self.offline.store(false, Ordering::Relaxed);
                if status.is_success() {
                    self.store.put(&base, path, &body, settled(path, &body));
                }
                Ok((status, body))
            }
            Err(error) => match self.store.get(&base, path) {
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
        assert!(settled("/api/playbook-drafts/d/files?version=4", b""));
        assert!(!settled("/api/playbook-drafts/d/files", b""));
        assert!(!settled("/api/runs/r/log", ended));
    }
}
