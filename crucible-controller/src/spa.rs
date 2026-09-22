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
use axum::http::{StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "ui/dist"]
#[prefix = ""]
struct SpaAssets;

/// Where the SPA is read from.
#[derive(Debug, Clone)]
pub(crate) enum Source {
    /// The bundle compiled into this binary.
    Embedded,
    /// A directory read per request, with the embedded bundle behind it.
    Dir(PathBuf),
}

impl Source {
    pub(crate) fn from_env() -> Self {
        match std::env::var_os("CONTROLLER_UI_DIR") {
            Some(dir) if !dir.is_empty() => {
                let dir = PathBuf::from(dir);
                tracing::info!(dir = %dir.display(), "serving the SPA from a directory");
                Self::Dir(dir)
            }
            _ => Self::Embedded,
        }
    }
}

pub(crate) fn router(source: Source) -> Router {
    Router::new()
        .fallback(get(serve_spa))
        .with_state(Arc::new(source))
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

    match source.as_ref() {
        Source::Embedded => embedded(path),
        Source::Dir(root) => from_dir(root, path).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get_path(source: Source, path: &str) -> (StatusCode, String) {
        let answer = router(source)
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
}
