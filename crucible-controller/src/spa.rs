//! The React SPA embedded in the controller binary (`crucible-controller/ui/`).
//!
//! Serves the built SPA from `/` with client-side routing fallback. When `ui/dist` was not built
//! (a machine with no Node), returns a clear "UI not built" message instead of a panic.
//!
//! `CONTROLLER_UI_DIR` points the surface at a directory instead. The directory is read per
//! request, so a bundle copied into a running pod is live without a rollout, and a directory with
//! no `index.html` falls through to the embedded bundle — an override that is absent, half-copied,
//! or deleted serves exactly what the image shipped.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use rust_embed::Embed;
use sha2::Digest;

#[derive(Embed)]
#[folder = "ui/dist"]
#[prefix = ""]
struct SpaAssets;

/// The Rune web build of `crucible-desktop`, served under [`NEXT`].
#[derive(Embed)]
#[folder = "next-dist"]
#[prefix = ""]
struct NextAssets;

/// Where the Rune web build is mounted.
const NEXT: &str = "/next";

/// The page that loads the Rune web build. Its `<base>` keeps the bundle's relative imports under
/// [`NEXT`] from any path the index fallback answers.
const NEXT_SHELL: &str = include_str!("next.html");

/// Where the SPA is read from.
#[derive(Debug, Clone)]
pub(crate) enum Source {
    /// The bundle compiled into this binary.
    Embedded,
    /// A directory read per request, with the embedded bundle behind it.
    Dir(PathBuf),
}

impl Source {
    /// The React SPA's source: `CONTROLLER_UI_DIR`, else embedded.
    pub(crate) fn from_env() -> Self {
        Self::from_var("CONTROLLER_UI_DIR")
    }

    /// The Rune web build's source: `CONTROLLER_NEXT_DIR`, else embedded. Unlike the SPA's, a
    /// directory here stands alone.
    pub(crate) fn next_from_env() -> Self {
        Self::from_var("CONTROLLER_NEXT_DIR")
    }

    fn from_var(var: &str) -> Self {
        match std::env::var_os(var) {
            Some(dir) if !dir.is_empty() => {
                let dir = PathBuf::from(dir);
                tracing::info!(dir = %dir.display(), var, "serving a UI from a directory");
                Self::Dir(dir)
            }
            _ => Self::Embedded,
        }
    }
}

/// The React SPA from `ui`, and the Rune web build from `next` under [`NEXT`].
pub(crate) fn router(ui: Source, next: Source) -> Router {
    let next = Router::new()
        .route(NEXT, get(|| async { Redirect::permanent("/next/") }))
        .route("/next/", get(serve_next))
        .route("/next/{*path}", get(serve_next))
        .with_state(Arc::new(next));
    Router::new()
        .fallback(get(serve_spa))
        .with_state(Arc::new(ui))
        .merge(next)
        .layer(tower_http::compression::CompressionLayer::new())
}

/// The file a request path names under `root`, or `None` when the path does not name one.
///
/// The request path is never percent-decoded, so no encoding of `..` can become a traversal: an
/// undecoded component is matched as the literal filename it spells, which a build output does not
/// carry. Traversal, absolute paths, and Windows separators are refused outright.
fn resolve(root: &Path, path: &str) -> Option<PathBuf> {
    let mut file = root.to_path_buf();
    for part in path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains('\\') || part.contains('\0') {
            return None;
        }
        file.push(part);
    }
    (file != root).then_some(file)
}

/// Vite content-hashes everything under `assets/`, so those bytes can never change under their
/// name; everything else (index.html, fonts, favicon) must revalidate, or a cached shell keeps
/// referencing chunks a redeploy deleted.
fn cache_control(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

fn body(path: &str, bytes: Vec<u8>) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    (
        [
            (header::CONTENT_TYPE, mime.as_ref()),
            (header::CACHE_CONTROL, cache_control(path)),
        ],
        bytes,
    )
        .into_response()
}

/// The embedded bundle: the asset a path names, else its `index.html` for a client-side route.
fn embedded(path: &str) -> Response {
    match SpaAssets::get(path) {
        Some(asset) => body(path, asset.data.into_owned()),
        None => match SpaAssets::get("index.html") {
            Some(index) => (
                [(header::CACHE_CONTROL, "no-cache")],
                Html(index.data.into_owned()),
            )
                .into_response(),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                "UI not built. Run: cd crucible-controller/ui && bun install && bun run build",
            )
                .into_response(),
        },
    }
}

/// A directory override: the asset it holds, then its own `index.html` for a client-side route,
/// then the embedded bundle when it holds neither.
async fn from_dir(root: &Path, path: &str) -> Response {
    if let Some(file) = resolve(root, path)
        && let Ok(bytes) = tokio::fs::read(&file).await
    {
        return body(path, bytes);
    }
    match tokio::fs::read(root.join("index.html")).await {
        Ok(index) => ([(header::CACHE_CONTROL, "no-cache")], Html(index)).into_response(),
        Err(_) => embedded(path),
    }
}

async fn serve_spa(State(source): State<Arc<Source>>, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    // An unmatched /api/* path is a missing endpoint, not a client-side route — never
    // answer it with index.html.
    if path.starts_with("api/") {
        return StatusCode::NOT_FOUND.into_response();
    }

    let response = match source.as_ref() {
        Source::Embedded => embedded(path),
        Source::Dir(root) => from_dir(root, path).await,
    };
    unframed(response)
}

/// The consent page is one of the SPA's routes: framed by another page, its Allow button could
/// be clicked through a disguise.
fn unframed(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    response
}

/// A file of the Rune web build, else the shell for any other path under [`NEXT`]. The bundle's
/// names carry no content hash, so every file revalidates against its digest.
async fn serve_next(State(next): State<Arc<Source>>, uri: Uri, request: HeaderMap) -> Response {
    let path = uri
        .path()
        .strip_prefix(NEXT)
        .unwrap_or_default()
        .trim_start_matches('/');
    let response = match next_file(&next, path).await {
        Some((bytes, digest)) => {
            let etag = format!("\"{}\"", hex(&digest));
            let fresh = request
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == etag);
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            let headers = [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::CACHE_CONTROL, "no-cache".to_string()),
                (header::ETAG, etag),
            ];
            if fresh {
                (StatusCode::NOT_MODIFIED, headers).into_response()
            } else {
                (headers, bytes).into_response()
            }
        }
        None if next_file(&next, NEXT_WASM).await.is_some() => {
            ([(header::CACHE_CONTROL, "no-cache")], Html(NEXT_SHELL)).into_response()
        }
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Next UI not built. Run: just controller-next",
        )
            .into_response(),
    };
    unframed(response)
}

/// The module the shell loads; a bundle without it is not built.
const NEXT_WASM: &str = "app_bg.wasm";

/// A file of the Rune web build and its SHA-256.
async fn next_file(source: &Source, path: &str) -> Option<(Vec<u8>, [u8; 32])> {
    match source {
        Source::Embedded => {
            let asset = NextAssets::get(path)?;
            Some((asset.data.into_owned(), asset.metadata.sha256_hash()))
        }
        Source::Dir(root) => {
            let bytes = tokio::fs::read(resolve(root, path)?).await.ok()?;
            let digest = sha2::Sha256::digest(&bytes).into();
            Some((bytes, digest))
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use crate::spa::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get_path(source: Source, path: &str) -> (StatusCode, String) {
        let answer = router(source, Source::Embedded)
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("a response");
        let status = answer.status();
        let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("a body");
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `/next/app_bg.wasm` from a directory holding `wasm`, asked for in `encoding`.
    async fn get_wasm(wasm: &[u8], encoding: Option<&str>) -> (Option<String>, Vec<u8>) {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join(NEXT_WASM), wasm).expect("write wasm");
        let mut request = Request::builder().uri("/next/app_bg.wasm");
        if let Some(encoding) = encoding {
            request = request.header(header::ACCEPT_ENCODING, encoding);
        }
        let answer = router(Source::Embedded, Source::Dir(dir.path().to_path_buf()))
            .oneshot(request.body(Body::empty()).expect("a request"))
            .await
            .expect("a response");
        assert_eq!(answer.status(), StatusCode::OK);
        let encoded = answer
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("a body");
        (encoded, bytes.to_vec())
    }

    fn wasm() -> Vec<u8> {
        b"\0asm".iter().copied().cycle().take(64 * 1024).collect()
    }

    #[tokio::test]
    async fn the_next_bundle_is_gzipped_for_a_client_that_takes_gzip() {
        let (encoded, body) = get_wasm(&wasm(), Some("gzip")).await;
        assert_eq!(encoded.as_deref(), Some("gzip"));
        assert!(body.len() < wasm().len() / 10, "{} bytes", body.len());
        let mut plain = Vec::new();
        std::io::Read::read_to_end(
            &mut flate2::read::GzDecoder::new(body.as_slice()),
            &mut plain,
        )
        .expect("gzip");
        assert_eq!(plain, wasm());
    }

    #[tokio::test]
    async fn the_next_bundle_is_zstd_for_a_client_that_takes_zstd() {
        let (encoded, body) = get_wasm(&wasm(), Some("gzip, deflate, br, zstd")).await;
        assert_eq!(encoded.as_deref(), Some("zstd"));
        assert!(body.len() < wasm().len() / 10, "{} bytes", body.len());
        assert_eq!(zstd::decode_all(body.as_slice()).expect("zstd"), wasm());
    }

    #[tokio::test]
    async fn the_next_bundle_is_sent_as_is_to_a_client_that_names_no_encoding() {
        let (encoded, body) = get_wasm(&wasm(), None).await;
        assert_eq!(encoded, None);
        assert_eq!(body, wasm());
    }

    #[tokio::test]
    async fn the_spa_is_compressed_too() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let page = "<p>crucible</p>".repeat(100);
        std::fs::write(dir.path().join("index.html"), &page).expect("write index");
        let answer = router(Source::Dir(dir.path().to_path_buf()), Source::Embedded)
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("a response");
        assert_eq!(
            answer
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("gzip")
        );
        let body = axum::body::to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("a body");
        let mut plain = String::new();
        std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&body[..]), &mut plain)
            .expect("gzip");
        assert_eq!(plain, page);
    }

    #[test]
    fn resolves_a_nested_asset() {
        let file = resolve(Path::new("/srv/ui"), "assets/index-a1b2.js");
        assert_eq!(file, Some(PathBuf::from("/srv/ui/assets/index-a1b2.js")));
    }

    #[test]
    fn refuses_to_climb_out_of_the_root() {
        assert_eq!(resolve(Path::new("/srv/ui"), "../../etc/passwd"), None);
        assert_eq!(
            resolve(Path::new("/srv/ui"), "assets/../../etc/passwd"),
            None
        );
        assert_eq!(resolve(Path::new("/srv/ui"), ".."), None);
    }

    #[test]
    fn an_encoded_traversal_stays_a_filename() {
        // Never decoded, so this names a file the build output does not carry rather than a parent.
        assert_eq!(
            resolve(Path::new("/srv/ui"), "%2e%2e/etc/passwd"),
            Some(PathBuf::from("/srv/ui/%2e%2e/etc/passwd"))
        );
    }

    #[test]
    fn refuses_a_windows_separator_and_a_nul() {
        assert_eq!(resolve(Path::new("/srv/ui"), "..\\etc\\passwd"), None);
        assert_eq!(resolve(Path::new("/srv/ui"), "assets\\x.js"), None);
        assert_eq!(resolve(Path::new("/srv/ui"), "assets/x\0.js"), None);
    }

    #[test]
    fn an_absolute_path_stays_under_the_root() {
        // A leading slash splits to an empty first component, which is skipped, not rooted.
        assert_eq!(
            resolve(Path::new("/srv/ui"), "/etc/passwd"),
            Some(PathBuf::from("/srv/ui/etc/passwd"))
        );
    }

    #[test]
    fn names_no_file_when_the_path_is_all_separators() {
        assert_eq!(resolve(Path::new("/srv/ui"), "///"), None);
        assert_eq!(resolve(Path::new("/srv/ui"), "./."), None);
    }

    #[tokio::test]
    async fn a_directory_without_an_index_falls_through_to_the_embedded_bundle() {
        let empty = tempfile::tempdir().expect("a temp dir");
        let answer = from_dir(empty.path(), "index.html").await;
        // ui/dist is empty in a Node-less checkout, so the embedded miss is the 503 — either way
        // the override contributed nothing, which is the contract under test.
        assert!(matches!(
            answer.status(),
            StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
        ));
    }

    #[tokio::test]
    async fn a_directory_serves_its_own_asset_and_its_own_index() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("index.html"), "<h1>override</h1>").expect("write index");
        std::fs::create_dir(dir.path().join("assets")).expect("mkdir assets");
        std::fs::write(dir.path().join("assets/app.js"), "console.log(1)").expect("write asset");

        let asset = from_dir(dir.path(), "assets/app.js").await;
        assert_eq!(asset.status(), StatusCode::OK);
        assert_eq!(
            asset
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("text/javascript")
        );

        // A client-side route the directory has no file for still reaches the override's index.
        let route = from_dir(dir.path(), "runs/RUN-0412").await;
        assert_eq!(route.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn the_override_serves_a_client_side_route_and_still_404s_a_missing_endpoint() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("index.html"), "<h1>override</h1>").expect("write index");
        let source = Source::Dir(dir.path().to_path_buf());

        let (status, body) = get_path(source.clone(), "/runs/RUN-0412").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "<h1>override</h1>");

        let (status, _) = get_path(source.clone(), "/").await;
        assert_eq!(status, StatusCode::OK);

        // An unmatched API path is a missing endpoint, never a client-side route.
        let (status, _) = get_path(source, "/api/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_override_directory_that_does_not_exist_serves_the_embedded_bundle() {
        let source = Source::Dir(PathBuf::from("/nonexistent/ui"));
        let (status, _) = get_path(source, "/").await;
        // The embedded bundle is empty in a Node-less checkout, so its own miss is the 503.
        assert!(matches!(
            status,
            StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
        ));
    }

    #[test]
    fn hashed_assets_cache_immutably_and_everything_else_revalidates() {
        assert_eq!(
            cache_control("assets/index-a1b2.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("index.html"), "no-cache");
        assert_eq!(
            cache_control("fonts/IoskeleyMono-Regular.woff2"),
            "no-cache"
        );
    }

    #[tokio::test]
    async fn no_page_of_the_spa_may_be_framed() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("index.html"), "<html>shell</html>").expect("index");
        for path in ["/consent?id=abc", "/settings", "/index.html", "/"] {
            let answer = router(Source::Dir(dir.path().to_path_buf()), Source::Embedded)
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("a request"),
                )
                .await
                .expect("a response");
            assert_eq!(answer.status(), StatusCode::OK, "{path}");
            let headers = answer.headers();
            assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY", "{path}");
            assert_eq!(
                headers[header::CONTENT_SECURITY_POLICY],
                "frame-ancestors 'none'",
                "{path}"
            );
        }
    }

    async fn get_next(next: &Path, path: &str, etag: Option<&str>) -> Response {
        let mut request = Request::builder().uri(path);
        if let Some(etag) = etag {
            request = request.header(header::IF_NONE_MATCH, etag);
        }
        router(Source::Embedded, Source::Dir(next.to_path_buf()))
            .oneshot(request.body(Body::empty()).expect("a request"))
            .await
            .expect("a response")
    }

    async fn text(answer: Response) -> String {
        let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
            .await
            .expect("a body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn built_next() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("app.js"), "export default 1").expect("write app.js");
        std::fs::write(dir.path().join(NEXT_WASM), b"\0asm").expect("write wasm");
        dir
    }

    #[tokio::test]
    async fn next_serves_a_bundle_file_with_its_type_and_digest() {
        let dir = built_next();
        let answer = get_next(dir.path(), "/next/app.js", None).await;
        assert_eq!(answer.status(), StatusCode::OK);
        let headers = answer.headers().clone();
        assert_eq!(headers[header::CONTENT_TYPE], "text/javascript");
        assert_eq!(headers[header::CACHE_CONTROL], "no-cache");
        assert_eq!(headers[header::X_FRAME_OPTIONS], "DENY");
        assert_eq!(text(answer).await, "export default 1");

        let wasm = get_next(dir.path(), "/next/app_bg.wasm", None).await;
        assert_eq!(wasm.headers()[header::CONTENT_TYPE], "application/wasm");

        let etag = headers[header::ETAG].to_str().expect("an ascii etag");
        let again = get_next(dir.path(), "/next/app.js", Some(etag)).await;
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(text(again).await, "");

        let stale = get_next(dir.path(), "/next/app.js", Some("\"other\"")).await;
        assert_eq!(stale.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn next_answers_any_other_path_with_the_shell() {
        let dir = built_next();
        for path in ["/next/", "/next/runs/RUN-0412", "/next/index.html"] {
            let answer = get_next(dir.path(), path, None).await;
            assert_eq!(answer.status(), StatusCode::OK, "{path}");
            assert_eq!(answer.headers()[header::X_FRAME_OPTIONS], "DENY", "{path}");
            let page = text(answer).await;
            assert!(page.contains(r#"<base href="/next/">"#), "{path}");
            assert!(page.contains("import init from './app.js'"), "{path}");
        }
    }

    #[tokio::test]
    async fn next_without_a_slash_redirects_to_the_mount() {
        let dir = built_next();
        let answer = get_next(dir.path(), "/next", None).await;
        assert_eq!(answer.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(answer.headers()[header::LOCATION], "/next/");
    }

    #[tokio::test]
    async fn next_refuses_to_climb_out_of_its_directory() {
        let dir = built_next();
        let answer = get_next(dir.path(), "/next/../Cargo.toml", None).await;
        assert_eq!(answer.status(), StatusCode::OK);
        assert!(text(answer).await.contains("<base"));
    }

    #[tokio::test]
    async fn next_says_it_is_not_built_when_no_bundle_is_there() {
        let empty = tempfile::tempdir().expect("a temp dir");
        let answer = get_next(empty.path(), "/next/", None).await;
        assert_eq!(answer.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(text(answer).await.contains("just controller-next"));
        let asset = get_next(empty.path(), "/next/app.js", None).await;
        assert_eq!(asset.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn the_react_spa_still_answers_beside_next() {
        let ui = tempfile::tempdir().expect("a temp dir");
        std::fs::write(ui.path().join("index.html"), "<h1>react</h1>").expect("write index");
        let next = built_next();
        let app = router(
            Source::Dir(ui.path().to_path_buf()),
            Source::Dir(next.path().to_path_buf()),
        );
        for (path, wants) in [("/", "<h1>react</h1>"), ("/nextish", "<h1>react</h1>")] {
            let answer = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("a request"),
                )
                .await
                .expect("a response");
            assert_eq!(text(answer).await, wants, "{path}");
        }
    }
}
