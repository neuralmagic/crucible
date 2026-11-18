//! `buildit cache`: a blob store over HTTP that builds share. A key is a path of plain segments.
//! GET and HEAD are open; PUT and DELETE need the write token, as a bearer token or as a
//! basic-auth password, which is how ccache's http backend sends a URL's userinfo.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use clap::Args;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_SEGMENTS: usize = 8;
const MAX_SEGMENT: usize = 128;
const READ_CHUNK: usize = 256 * 1024;
const GIB: u64 = 1024 * 1024 * 1024;
const PRUNE_EVERY: Duration = Duration::from_secs(60);
const STALE_UPLOAD: Duration = Duration::from_secs(3600);

#[derive(Args, Debug, Clone)]
pub struct CacheArgs {
    /// Listen address
    #[arg(long, env = "BUILDIT_CACHE_BIND", default_value = "0.0.0.0:8850")]
    pub bind: String,
    /// Directory the blobs are kept in
    #[arg(long, env = "BUILDIT_CACHE_DIR")]
    pub dir: PathBuf,
    /// File holding the write token, re-read on every write
    #[arg(long, env = "BUILDIT_CACHE_TOKEN_FILE")]
    pub token_file: PathBuf,
    /// GiB kept before the least recently used blobs are dropped
    #[arg(long, env = "BUILDIT_CACHE_MAX_GIB", default_value_t = 200)]
    pub max_gib: u64,
    /// Largest blob accepted, in GiB
    #[arg(long, env = "BUILDIT_CACHE_MAX_BLOB_GIB", default_value_t = 8)]
    pub max_blob_gib: u64,
}

pub struct Store {
    blobs: PathBuf,
    uploads: PathBuf,
    token_file: PathBuf,
    max_bytes: u64,
    max_blob: u64,
    upload_seq: AtomicU64,
}

impl Store {
    pub fn open(dir: &Path, token_file: PathBuf, max_bytes: u64, max_blob: u64) -> Result<Self> {
        let store = Self {
            blobs: dir.join("blobs"),
            uploads: dir.join("uploads"),
            token_file,
            max_bytes,
            max_blob,
            upload_seq: AtomicU64::new(0),
        };
        for d in [&store.blobs, &store.uploads] {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(store)
    }

    fn path(&self, key: &Key) -> PathBuf {
        key.0.iter().fold(self.blobs.clone(), |p, s| p.join(s))
    }

    fn upload_path(&self) -> PathBuf {
        let seq = self.upload_seq.fetch_add(1, Ordering::Relaxed);
        self.uploads.join(format!("{}-{seq}", std::process::id()))
    }

    fn may_write(&self, headers: &HeaderMap) -> bool {
        let Some(presented) = presented_token(headers) else {
            return false;
        };
        let Ok(token) = std::fs::read_to_string(&self.token_file) else {
            tracing::warn!("cannot read the write token {}", self.token_file.display());
            return false;
        };
        let token = token.trim();
        !token.is_empty() && Sha256::digest(token) == Sha256::digest(presented)
    }
}

/// A blob's key: 1 to 8 segments of letters, digits, `.`, `_` and `-`, none starting with `.`.
#[derive(Debug, PartialEq)]
pub struct Key(Vec<String>);

impl Key {
    pub fn parse(raw: &str) -> Option<Self> {
        let segments: Vec<String> = raw.split('/').map(str::to_string).collect();
        let plain = |s: &String| {
            !s.is_empty()
                && s.len() <= MAX_SEGMENT
                && s.as_bytes()[0].is_ascii_alphanumeric()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        };
        (segments.len() <= MAX_SEGMENTS && segments.iter().all(plain)).then_some(Self(segments))
    }
}

fn presented_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.split_once(' ')?;
    match scheme.to_ascii_lowercase().as_str() {
        "bearer" => Some(rest.trim().to_string()),
        "basic" => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(rest.trim())
                .ok()?;
            let pair = String::from_utf8(decoded).ok()?;
            Some(pair.split_once(':')?.1.to_string())
        }
        _ => None,
    }
}

pub async fn serve(args: CacheArgs) -> Result<()> {
    let store = Arc::new(Store::open(
        &args.dir,
        args.token_file,
        args.max_gib.saturating_mul(GIB),
        args.max_blob_gib.saturating_mul(GIB),
    )?);
    let pruning = store.clone();
    tokio::spawn(async move {
        loop {
            let store = pruning.clone();
            match tokio::task::spawn_blocking(move || prune(&store, SystemTime::now())).await {
                Ok(Ok(dropped)) if dropped > 0 => tracing::info!("dropped {dropped} blobs"),
                Ok(Err(e)) => tracing::warn!("pruning the cache: {e:#}"),
                Err(e) => tracing::warn!("pruning the cache: {e}"),
                Ok(Ok(_)) => {}
            }
            tokio::time::sleep(PRUNE_EVERY).await;
        }
    });
    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    tracing::info!(
        "buildit cache listening on http://{} ({})",
        args.bind,
        args.dir.display()
    );
    axum::serve(listener, router(store))
        .await
        .context("serving buildit cache")
}

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/_health", get(|| async { "ok" }))
        .route(
            "/{*key}",
            get(read_blob).put(write_blob).delete(remove_blob),
        )
        .with_state(store)
}

async fn read_blob(State(store): State<Arc<Store>>, UrlPath(raw): UrlPath<String>) -> Response {
    let Some(key) = Key::parse(&raw) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let path = store.path(&key);
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return StatusCode::NOT_FOUND.into_response();
        }
        Err(e) => return internal(&path, e),
    };
    let len = match file.metadata().await {
        Ok(m) if m.is_file() => m.len(),
        Ok(_) => return StatusCode::NOT_FOUND.into_response(),
        Err(e) => return internal(&path, e),
    };
    tokio::task::spawn_blocking(move || mark_used(&path));
    let body = futures_util::stream::try_unfold(file, |mut f| async move {
        let mut buf = vec![0u8; READ_CHUNK];
        let n = f.read(&mut buf).await?;
        if n == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buf.truncate(n);
        Ok(Some((Bytes::from(buf), f)))
    });
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        Body::from_stream(body),
    )
        .into_response()
}

async fn write_blob(
    State(store): State<Arc<Store>>,
    UrlPath(raw): UrlPath<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(key) = Key::parse(&raw) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !store.may_write(&headers) {
        drain(body, store.max_blob).await;
        return unauthorized();
    }
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|n| n > store.max_blob) {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let upload = store.upload_path();
    let written = receive(&upload, body, store.max_blob).await;
    let dest = store.path(&key);
    let placed = match written {
        Ok(Received::Whole) => place(&upload, &dest).await,
        Ok(Received::TooLarge) => {
            let _ = tokio::fs::remove_file(&upload).await;
            return StatusCode::PAYLOAD_TOO_LARGE.into_response();
        }
        Err(e) => Err(e),
    };
    match placed {
        Ok(()) => StatusCode::CREATED.into_response(),
        Err(e) => {
            let _ = tokio::fs::remove_file(&upload).await;
            internal(&dest, e)
        }
    }
}

async fn remove_blob(
    State(store): State<Arc<Store>>,
    UrlPath(raw): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(key) = Key::parse(&raw) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if !store.may_write(&headers) {
        return unauthorized();
    }
    let path = store.path(&key);
    match tokio::fs::remove_file(&path).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal(&path, e),
    }
}

enum Received {
    Whole,
    TooLarge,
}

/// Read and discard up to `max` bytes of a refused upload.
async fn drain(body: Body, max: u64) {
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    while let Some(Ok(chunk)) = stream.next().await {
        total += chunk.len() as u64;
        if total > max {
            return;
        }
    }
}

async fn receive(upload: &Path, body: Body, max: u64) -> std::io::Result<Received> {
    let mut file = tokio::fs::File::create(upload).await?;
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(std::io::Error::other)?;
        total += chunk.len() as u64;
        if total > max {
            return Ok(Received::TooLarge);
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(Received::Whole)
}

async fn place(upload: &Path, dest: &Path) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(upload, dest).await
}

fn mark_used(path: &Path) {
    if let Ok(f) = std::fs::File::options().append(true).open(path) {
        let _ = f.set_modified(SystemTime::now());
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"buildit-cache\"")],
    )
        .into_response()
}

fn internal(path: &Path, e: std::io::Error) -> Response {
    tracing::warn!("{}: {e}", path.display());
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// Drop the least recently used blobs until the store fits in `max_bytes`, and uploads abandoned
/// for an hour. Returns how many blobs it dropped.
pub fn prune(store: &Store, now: SystemTime) -> Result<usize> {
    for entry in std::fs::read_dir(&store.uploads)? {
        let entry = entry?;
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > STALE_UPLOAD));
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let mut blobs = Vec::new();
    let mut dirs = vec![store.blobs.clone()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() {
                dirs.push(entry.path());
            } else if meta.is_file() {
                blobs.push((meta.modified()?, meta.len(), entry.path()));
            }
        }
    }
    let mut total: u64 = blobs.iter().map(|(_, len, _)| len).sum();
    if total <= store.max_bytes {
        return Ok(0);
    }
    blobs.sort();
    let mut dropped = 0;
    for (_, len, path) in blobs {
        if total <= store.max_bytes {
            break;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                total -= len;
                dropped += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("dropping {}: {e}", path.display()),
        }
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use crate::cache::{Key, Store, presented_token, prune, router};
    use axum::http::{HeaderMap, HeaderValue, header};
    use base64::Engine;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    const TOKEN: &str = "s3cret-write-token";

    fn store(dir: &Path, max_bytes: u64, max_blob: u64) -> Store {
        let token = dir.join("token");
        std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
        Store::open(&dir.join("data"), token, max_bytes, max_blob).unwrap()
    }

    async fn serve(store: Store) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(Arc::new(store))).await });
        format!("http://{addr}")
    }

    fn basic(user: &str, password: &str) -> String {
        let pair = format!("{user}:{password}");
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(pair)
        )
    }

    #[test]
    fn keys_are_plain_segments() {
        for ok in [
            "ccache/ab/cdef0123",
            "store/0f3c.tar",
            "a",
            "a/b/c/d/e/f/g/h",
            "x-y_z.1",
        ] {
            assert!(Key::parse(ok).is_some(), "{ok}");
        }
        let long = "a".repeat(129);
        for bad in [
            "",
            "/a",
            "a/",
            "a//b",
            "../a",
            "a/../b",
            ".hidden",
            "a/.git",
            "_health",
            "a b",
            "a\\b",
            "a/b/c/d/e/f/g/h/i",
            long.as_str(),
        ] {
            assert_eq!(Key::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_write_token_comes_as_a_bearer_token_or_a_basic_password() {
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::AUTHORIZATION, HeaderValue::from_str(v).unwrap());
            presented_token(&h)
        };
        assert_eq!(with("Bearer abc").as_deref(), Some("abc"));
        assert_eq!(with("bearer  abc ").as_deref(), Some("abc"));
        assert_eq!(with(&basic("ccache", "abc")).as_deref(), Some("abc"));
        assert_eq!(with(&basic("", "a:b")).as_deref(), Some("a:b"));
        assert_eq!(with("Basic not-base64!"), None);
        assert_eq!(with("Digest abc"), None);
        assert_eq!(with("abc"), None);
        assert_eq!(presented_token(&HeaderMap::new()), None);
    }

    #[tokio::test]
    async fn a_refused_upload_is_read_whole_so_its_sender_sees_the_refusal() {
        let tmp = tempfile::tempdir().unwrap();
        let url = serve(store(tmp.path(), u64::MAX, 64 << 20)).await;
        let addr = url.trim_start_matches("http://").to_string();
        let status = tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let body = vec![b'x'; 8 << 20];
            let mut conn = std::net::TcpStream::connect(&addr).unwrap();
            write!(
                conn,
                "PUT /store/k1.tar HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer nope\r\n\
                 Content-Length: {}\r\n\r\n",
                body.len()
            )
            .unwrap();
            conn.write_all(&body).expect("the whole body is taken");
            let mut head = [0u8; 12];
            conn.read_exact(&mut head).unwrap();
            String::from_utf8_lossy(&head).to_string()
        })
        .await
        .unwrap();
        assert_eq!(status, "HTTP/1.1 401");
    }

    #[tokio::test]
    async fn reads_are_open_and_writes_need_the_token() {
        let tmp = tempfile::tempdir().unwrap();
        let url = serve(store(tmp.path(), u64::MAX, 1 << 20)).await;
        let http = reqwest::Client::new();
        let blob = format!("{url}/ccache/ab/cdef");

        assert_eq!(http.get(&blob).send().await.unwrap().status(), 404);
        assert_eq!(http.head(&blob).send().await.unwrap().status(), 404);
        let anonymous = http.put(&blob).body("x").send().await.unwrap();
        assert_eq!(anonymous.status(), 401);
        assert!(anonymous.headers().contains_key("www-authenticate"));
        let wrong = http
            .put(&blob)
            .bearer_auth("nope")
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 401);
        assert_eq!(http.get(&blob).send().await.unwrap().status(), 404);

        let put = http
            .put(&blob)
            .bearer_auth(TOKEN)
            .body("object-1")
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), 201);
        let got = http.get(&blob).send().await.unwrap();
        assert_eq!(got.status(), 200);
        assert_eq!(got.bytes().await.unwrap().as_ref(), b"object-1");
        let head = http.head(&blob).send().await.unwrap();
        assert_eq!(head.status(), 200);
        assert_eq!(head.headers()["content-length"], "8");

        let replaced = http
            .put(&blob)
            .header("authorization", basic("ccache", TOKEN))
            .body("object-2")
            .send()
            .await
            .unwrap();
        assert_eq!(replaced.status(), 201);
        let got = http.get(&blob).send().await.unwrap().bytes().await.unwrap();
        assert_eq!(got.as_ref(), b"object-2");

        assert_eq!(http.delete(&blob).send().await.unwrap().status(), 401);
        let deleted = http.delete(&blob).bearer_auth(TOKEN).send().await.unwrap();
        assert_eq!(deleted.status(), 204);
        assert_eq!(http.get(&blob).send().await.unwrap().status(), 404);
        let again = http.delete(&blob).bearer_auth(TOKEN).send().await.unwrap();
        assert_eq!(again.status(), 404);

        assert_eq!(
            http.get(format!("{url}/_health"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        let hidden = http.get(format!("{url}/a/.git")).send().await.unwrap();
        assert_eq!(hidden.status(), 400);
        for path in ["/a/../b", "/a/%2E%2E/b", "/a//b"] {
            assert_eq!(raw_status(&url, "GET", path).await, 400, "{path}");
            assert_eq!(raw_status(&url, "PUT", path).await, 400, "{path}");
        }
    }

    async fn raw_status(url: &str, method: &str, path: &str) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let host = url.trim_start_matches("http://");
        let mut conn = tokio::net::TcpStream::connect(host).await.unwrap();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx"
        );
        conn.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        conn.read_to_string(&mut reply).await.unwrap();
        reply
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap()
    }

    #[tokio::test]
    async fn a_rotated_token_takes_effect_and_an_empty_one_refuses_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let url = serve(store(tmp.path(), u64::MAX, 1 << 20)).await;
        let http = reqwest::Client::new();
        let blob = format!("{url}/store/k.tar");
        std::fs::write(tmp.path().join("token"), "rotated").unwrap();
        let old = http
            .put(&blob)
            .bearer_auth(TOKEN)
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(old.status(), 401);
        let new = http
            .put(&blob)
            .bearer_auth("rotated")
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(new.status(), 201);
        std::fs::write(tmp.path().join("token"), "\n").unwrap();
        let empty = http
            .put(&blob)
            .bearer_auth("")
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(empty.status(), 401);
        std::fs::remove_file(tmp.path().join("token")).unwrap();
        let missing = http
            .put(&blob)
            .bearer_auth("rotated")
            .body("x")
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 401);
    }

    #[tokio::test]
    async fn an_oversized_blob_is_refused_and_leaves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let url = serve(store(tmp.path(), u64::MAX, 1024)).await;
        let http = reqwest::Client::new();
        let blob = format!("{url}/store/big.tar");
        let declared = http
            .put(&blob)
            .bearer_auth(TOKEN)
            .body(vec![7u8; 2048])
            .send()
            .await;
        assert_eq!(declared.unwrap().status(), 413);
        let chunks =
            futures_util::stream::iter((0..4).map(|_| Ok::<_, std::io::Error>(vec![7u8; 512])));
        let streamed = http
            .put(&blob)
            .bearer_auth(TOKEN)
            .body(reqwest::Body::wrap_stream(chunks))
            .send()
            .await
            .unwrap();
        assert_eq!(streamed.status(), 413);
        assert_eq!(http.get(&blob).send().await.unwrap().status(), 404);
        let uploads = std::fs::read_dir(tmp.path().join("data/uploads"))
            .unwrap()
            .count();
        assert_eq!(uploads, 0, "a refused upload leaves no partial file");
        let fits = http
            .put(&blob)
            .bearer_auth(TOKEN)
            .body(vec![7u8; 1024])
            .send()
            .await;
        assert_eq!(fits.unwrap().status(), 201);
    }

    #[tokio::test]
    async fn a_large_blob_streams_through_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let url = serve(store(tmp.path(), u64::MAX, 64 << 20)).await;
        let http = reqwest::Client::new();
        let blob = format!("{url}/store/outputs.tar");
        let data: Vec<u8> = (0..(9 << 20)).map(|i: u32| (i % 251) as u8).collect();
        let put = http
            .put(&blob)
            .bearer_auth(TOKEN)
            .body(data.clone())
            .send()
            .await;
        assert_eq!(put.unwrap().status(), 201);
        let got = http.get(&blob).send().await.unwrap().bytes().await.unwrap();
        assert_eq!(got.len(), data.len());
        assert!(got.as_ref() == data.as_slice());
    }

    #[test]
    fn pruning_drops_the_least_recently_used_blobs_and_stale_uploads() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(tmp.path(), 250, u64::MAX);
        let now = SystemTime::now();
        let blob = |name: &str, age_s: u64| {
            let path = tmp.path().join("data/blobs/ccache").join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, vec![0u8; 100]).unwrap();
            let f = std::fs::File::options().append(true).open(&path).unwrap();
            f.set_modified(now - Duration::from_secs(age_s)).unwrap();
            path
        };
        let oldest = blob("aa/1", 300);
        let older = blob("bb/2", 200);
        let newer = blob("cc/3", 100);
        let stale = tmp.path().join("data/uploads/1-0");
        let fresh = tmp.path().join("data/uploads/1-1");
        for (path, age) in [(&stale, 7200), (&fresh, 10)] {
            std::fs::write(path, b"partial").unwrap();
            let f = std::fs::File::options().append(true).open(path).unwrap();
            f.set_modified(now - Duration::from_secs(age)).unwrap();
        }
        assert_eq!(prune(&store, now).unwrap(), 1);
        assert!(!oldest.exists());
        assert!(older.exists() && newer.exists());
        assert!(!stale.exists() && fresh.exists());
        assert_eq!(prune(&store, now).unwrap(), 0, "it fits now");
    }

    #[tokio::test]
    async fn reading_a_blob_marks_it_used() {
        let tmp = tempfile::tempdir().unwrap();
        let url = serve(store(tmp.path(), u64::MAX, 1 << 20)).await;
        let http = reqwest::Client::new();
        let put = http
            .put(format!("{url}/ccache/ab/c"))
            .bearer_auth(TOKEN)
            .body("o");
        assert_eq!(put.send().await.unwrap().status(), 201);
        let path = tmp.path().join("data/blobs/ccache/ab/c");
        let long_ago = SystemTime::now() - Duration::from_secs(86_400);
        std::fs::File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        http.get(format!("{url}/ccache/ab/c"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let mut used = long_ago;
        for _ in 0..50 {
            used = std::fs::metadata(&path).unwrap().modified().unwrap();
            if used > long_ago {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(used > long_ago + Duration::from_secs(3600));
    }

    fn ccache_stat(stats: &str, names: &[&str]) -> u64 {
        stats
            .lines()
            .filter_map(|l| l.split_once('\t'))
            .filter(|(k, _)| names.contains(k))
            .filter_map(|(_, v)| v.trim().parse::<u64>().ok())
            .sum()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ccache_shares_objects_through_it_and_a_read_only_client_writes_nothing() {
        let found = |tool: &str| {
            std::process::Command::new(tool)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if !found("ccache") || !found("cc") {
            eprintln!("skipping: no ccache or cc");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let url = serve(store(tmp.path(), u64::MAX, 1 << 30)).await;
        let host = url.trim_start_matches("http://");
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.c"), "int a(int x) { return x * 3; }\n").unwrap();
        std::fs::write(src.join("b.c"), "int b(int x) { return x * 5; }\n").unwrap();
        let blobs = || {
            let mut n = 0;
            let mut dirs = vec![tmp.path().join("data/blobs")];
            while let Some(d) = dirs.pop() {
                for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    if e.path().is_dir() {
                        dirs.push(e.path());
                    } else {
                        n += 1;
                    }
                }
            }
            n
        };
        let compile = |local: &str, remote: &str, file: &str| {
            let dir = tmp.path().join(local);
            let run = |args: &[&str]| {
                let out = std::process::Command::new("ccache")
                    .args(args)
                    .current_dir(&src)
                    .env("CCACHE_DIR", &dir)
                    .env("CCACHE_REMOTE_STORAGE", remote)
                    .env("CCACHE_SECONDARY_STORAGE", remote)
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                String::from_utf8_lossy(&out.stdout).to_string()
            };
            run(&["-z"]);
            run(&["cc", "-c", file, "-o", &format!("{file}.o")]);
            run(&["--print-stats"])
        };
        let hits = ["remote_storage_hit", "secondary_storage_hit"];
        let writer = format!("http://ccache:{TOKEN}@{host}/ccache");
        let reader = format!("http://{host}/ccache|read-only");

        let first = compile("local-1", &writer, "a.c");
        assert_eq!(ccache_stat(&first, &hits), 0, "{first}");
        let stored = blobs();
        assert!(stored > 0, "the writer stored its objects");

        let second = compile("local-2", &reader, "a.c");
        assert!(
            ccache_stat(&second, &hits) >= 1,
            "a fresh client hits remotely: {second}"
        );
        assert_eq!(blobs(), stored);

        compile("local-3", &reader, "b.c");
        assert_eq!(blobs(), stored, "a read-only client stores nothing");
        let anonymous = format!("http://{host}/ccache");
        compile("local-4", &anonymous, "b.c");
        assert_eq!(blobs(), stored, "a client without the token cannot store");
    }
}
