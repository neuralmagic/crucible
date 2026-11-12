//! Tool downloads of stored packs. A download is stored in `tool_blobs` under the sha256 a lock
//! pins, `pack_tree_tools` keeps it for as long as a stored pack tree's lock names it, and a run
//! fetches the ones its pack locks from `GET /api/tools/{sha256}` with its pod token.

use anyhow::Context as _;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use crucible::plan::deps::LockedTool;
use sqlx::PgPool;

use crate::client::Db;
use crate::playbooks::deps::{Remotes, ResolveError};
use crate::runs::pod_token::Subject;

/// Store every download in `tools` that is not stored yet, refusing one whose bytes differ from
/// the lock. A stored one is made new again, so collection leaves it for the save that follows.
pub(crate) async fn resolve(
    pool: &PgPool,
    tools: &[LockedTool],
    remotes: &Remotes,
) -> Result<(), ResolveError> {
    for tool in tools {
        let stored = sqlx::query("UPDATE tool_blobs SET created_at = $2 WHERE sha256 = $1")
            .bind(&tool.sha256)
            .bind(crate::clock::now_rfc3339())
            .execute(pool)
            .await
            .context("looking up a stored tool")?
            .rows_affected();
        if stored > 0 {
            continue;
        }
        let auth = remotes
            .auth(&format!("tool \"{}\"", tool.name), &tool.url)
            .await?;
        let bytes = {
            let tool = tool.clone();
            tokio::task::spawn_blocking(move || crucible::plan::deps::download_tool(&tool, &auth))
                .await
                .context("joining the tool download worker")?
                .map_err(|error| ResolveError::Refused(error.to_string()))?
        };
        sqlx::query(
            "INSERT INTO tool_blobs (sha256, bytes, created_at) VALUES ($1, $2, $3)
             ON CONFLICT (sha256) DO NOTHING",
        )
        .bind(&tool.sha256)
        .bind(&bytes)
        .bind(crate::clock::now_rfc3339())
        .execute(pool)
        .await
        .with_context(|| format!("storing tool {}", tool.name))?;
    }
    Ok(())
}

/// Let `run_id`'s token fetch the downloads the lock of the pack at `pack_dir` pins.
pub(crate) async fn grant(
    pool: &PgPool,
    run_id: &str,
    pack_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let Some(lock) = crucible::plan::deps::Lock::read(pack_dir)
        .with_context(|| format!("reading the lock of {}", pack_dir.display()))?
    else {
        return Ok(());
    };
    if lock.tools.is_empty() {
        return Ok(());
    }
    let digests: Vec<&str> = lock.tools.iter().map(|tool| tool.sha256.as_str()).collect();
    sqlx::query(
        "INSERT INTO run_tools (run_id, sha256) SELECT $1, UNNEST($2::TEXT[])
         ON CONFLICT (run_id, sha256) DO NOTHING",
    )
    .bind(run_id)
    .bind(&digests)
    .execute(pool)
    .await
    .context("recording the run's tools")?;
    Ok(())
}

pub(crate) fn router(db: Db) -> Router {
    Router::new()
        .route(
            &format!("{}/{{sha256}}", crucible_contract::TOOLS_PATH),
            get(fetch),
        )
        .with_state(db)
}

fn refuse(status: StatusCode, error: &str) -> Response {
    (status, Json(serde_json::json!({ "error": error }))).into_response()
}

/// `GET /api/tools/{sha256}`: the stored download with that hex digest, to the token of a run
/// whose pack locks it.
async fn fetch(
    State(db): State<Db>,
    Path(sha256): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| crate::runs::pod_token::looks_like_token(token));
    let Some(presented) = presented else {
        return refuse(StatusCode::UNAUTHORIZED, "a run's pod token fetches tools");
    };
    let run_id = match crate::runs::pod_token::verify(db.pool(), presented).await {
        Ok(grant) => match grant.subject {
            Subject::Run(run_id) => run_id,
            Subject::Turn(_) => {
                return refuse(StatusCode::FORBIDDEN, "a turn's token fetches no tools");
            }
        },
        Err(refusal) => return refuse(StatusCode::UNAUTHORIZED, &refusal.to_string()),
    };
    let bytes: Result<Option<Vec<u8>>, sqlx::Error> = sqlx::query_scalar(
        "SELECT b.bytes FROM run_tools r JOIN tool_blobs b ON b.sha256 = r.sha256
         WHERE r.run_id = $1 AND r.sha256 = $2",
    )
    .bind(&run_id)
    .bind(format!("sha256:{sha256}"))
    .fetch_optional(db.pool())
    .await;
    match bytes {
        Ok(Some(bytes)) => {
            ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
        }
        Ok(None) => refuse(
            StatusCode::NOT_FOUND,
            "this run's pack locks no stored tool with that digest",
        ),
        Err(error) => {
            tracing::error!(%run_id, %sha256, %error, "tool fetch failed");
            refuse(StatusCode::INTERNAL_SERVER_ERROR, "reading the tool failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::client::Db;
    use crate::playbooks::deps::{Remotes, ResolveError};
    use crate::playbooks::pack_trees::{EncodedPack, collect, put_tree};
    use crate::playbooks::tools::{grant, resolve, router};
    use crate::runs::pod_token::{Subject, mint};
    use crucible::plan::deps::{Cache, Controller, LOCK_FILE, Platform, fetch_tools_from};
    use crucible_contract::pack_tree::PackTree;
    use sqlx::PgPool;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn locked(tree: &PackTree) -> Vec<crucible::plan::deps::LockedTool> {
        crate::playbooks::lock::lock_of(tree).map_or_else(Vec::new, |lock| lock.tools)
    }

    const HOUR: Duration = Duration::from_secs(3600);
    const GREET: &[u8] = b"#!/bin/sh\necho hello\n";

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await });
        format!("http://{addr}")
    }

    /// A server holding `GREET` at `/greet`, and a pack tree whose lock pins it there for this
    /// machine as the tool `greet`, at the digest of `pinned`.
    async fn origin_and_pack(pinned: &[u8]) -> PackTree {
        let origin =
            serve(axum::Router::new().route("/greet", axum::routing::get(|| async { GREET })))
                .await;
        let lock = format!(
            "version = 1\n\n[[tool]]\nname = \"greet\"\nplatform = \"{}\"\nurl = \"{origin}/greet\"\nsha256 = \"{}\"\n",
            Platform::host().expect("host platform").as_str(),
            crucible_contract::content_digest(pinned),
        );
        let files = BTreeMap::from([
            (
                "crucible.toml".parse().expect("path"),
                b"[agent]\n".to_vec(),
            ),
            (LOCK_FILE.parse().expect("path"), lock.into_bytes()),
        ]);
        PackTree::new(files).expect("tree")
    }

    async fn stored_tools(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM tool_blobs")
            .fetch_one(pool)
            .await
            .expect("count")
    }

    async fn age_every_tool(pool: &PgPool) {
        sqlx::query("UPDATE tool_blobs SET created_at = '2000-01-01T00:00:00Z'")
            .execute(pool)
            .await
            .expect("age tools");
        sqlx::query("UPDATE pack_trees SET created_at = '2000-01-01T00:00:00Z'")
            .execute(pool)
            .await
            .expect("age trees");
    }

    /// A run pod's engine fetches the tool its pack locks from the controller with the run's
    /// token and installs it, and no other caller gets the bytes.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_run_fetches_the_tools_its_pack_locks_and_nobody_else_does(pool: PgPool) {
        let tree = origin_and_pack(GREET).await;
        let tools = locked(&tree);
        resolve(&pool, &tools, &Remotes::Any)
            .await
            .expect("resolve");
        resolve(&pool, &tools, &Remotes::Any).await.expect("again");
        assert_eq!(stored_tools(&pool).await, 1);

        let pack = tempfile::tempdir().expect("pack dir");
        for (path, bytes) in tree.files() {
            std::fs::write(pack.path().join(path.as_str()), bytes).expect("pack file");
        }
        grant(&pool, "run-a", pack.path()).await.expect("grant");
        let token_of = |run: &'static str| {
            let pool = pool.clone();
            async move {
                mint(&pool, &Subject::Run(run.to_string()), None, HOUR)
                    .await
                    .expect("mint")
            }
        };
        let url = serve(router(Db::new(pool.clone()))).await;
        let fetch = |token: String| {
            let (url, pack) = (url.clone(), pack.path().to_path_buf());
            async move {
                let cache = tempfile::tempdir().expect("cache dir");
                let fetched = tokio::task::spawn_blocking({
                    let cache = Cache::at(cache.path());
                    move || fetch_tools_from(&pack, &cache, &Controller { url, token })
                })
                .await
                .expect("join");
                (cache, fetched)
            }
        };

        let (cache, fetched) = fetch(token_of("run-a").await).await;
        let fetched = fetched.expect("fetch");
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].as_str(), "greet");
        let installed = Cache::at(cache.path())
            .tool_dir(&tools[0].sha256)
            .join("greet");
        assert_eq!(std::fs::read(installed).expect("installed"), GREET);

        let (_cache, other) = fetch(token_of("run-b").await).await;
        let refused = other.expect_err("another run").to_string();
        assert!(
            refused.contains("the controller does not hold it"),
            "{refused}"
        );

        let hex = tools[0].sha256.strip_prefix("sha256:").expect("hex");
        let status = |bearer: Option<String>| {
            let target = format!("{url}/api/tools/{hex}");
            async move {
                let mut request = reqwest::Client::new().get(target);
                if let Some(bearer) = bearer {
                    request = request.bearer_auth(bearer);
                }
                request.send().await.expect("send").status().as_u16()
            }
        };
        assert_eq!(status(None).await, 401);
        assert_eq!(status(Some("crr_nobody_nothing".to_string())).await, 401);
        let turn = mint(&pool, &Subject::Turn("turn-pod".to_string()), None, HOUR)
            .await
            .expect("mint turn");
        assert_eq!(status(Some(turn)).await, 403);
        assert_eq!(status(Some(token_of("run-a").await)).await, 200);
    }

    /// Saving a draft whose lock pins a tool stores the download and pins it to the saved tree.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn saving_a_draft_stores_and_pins_the_tools_its_lock_names(pool: PgPool) {
        use crate::playbooks::drafts::{
            DraftSeed, SKELETON_MANIFEST, SKELETON_WORKFLOW, create, resolve_locked, save_version,
        };
        create(
            &pool,
            "studio",
            "a drafted pack",
            DraftSeed::Skeleton,
            Some("wren"),
            &crate::authz::model::Principal::platform(),
        )
        .await
        .expect("create");
        let tree = origin_and_pack(GREET).await;
        let lock = String::from_utf8(tree.files()[&LOCK_FILE.parse().expect("path")].clone())
            .expect("lock text");
        let files = BTreeMap::from([
            ("crucible.toml".to_string(), SKELETON_MANIFEST.to_string()),
            ("workflow.star".to_string(), SKELETON_WORKFLOW.to_string()),
            (LOCK_FILE.to_string(), lock),
        ]);

        resolve_locked(&pool, &files, &Remotes::Any)
            .await
            .expect("resolve");
        let saved = save_version(&pool, "studio", files, None, None)
            .await
            .expect("save");
        assert!(saved.diagnostics.is_empty(), "{:?}", saved.diagnostics);
        let pinned: Vec<(String, String)> =
            sqlx::query_as("SELECT name, sha256 FROM pack_tree_tools")
                .fetch_all(&pool)
                .await
                .expect("pins");
        assert_eq!(
            pinned,
            [(
                "greet".to_string(),
                crucible_contract::content_digest(GREET)
            )]
        );
    }

    /// A download whose bytes differ from the lock is refused and not stored.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_download_that_differs_from_the_lock_is_refused(pool: PgPool) {
        let tree = origin_and_pack(b"what the author locked").await;
        let error = resolve(&pool, &locked(&tree), &Remotes::Any)
            .await
            .expect_err("mismatch");
        let ResolveError::Refused(message) = error else {
            panic!("{error:?}");
        };
        assert!(message.contains("tool \"greet\""), "{message}");
        assert_eq!(stored_tools(&pool).await, 0);

        let https_only = Remotes::Https { github_app: None };
        let error = resolve(&pool, &locked(&tree), &https_only)
            .await
            .expect_err("plain http");
        assert!(
            error.to_string().contains("is not an https:// URL"),
            "{error}"
        );
    }

    /// A stored download outlives collection while a stored pack tree locks it and goes with
    /// the last such tree.
    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn a_stored_tool_is_kept_while_a_stored_pack_locks_it(pool: PgPool) {
        let tree = origin_and_pack(GREET).await;
        resolve(&pool, &locked(&tree), &Remotes::Any)
            .await
            .expect("resolve");
        let mut conn = pool.acquire().await.expect("conn");
        let digest = put_tree(&mut conn, &EncodedPack::new(tree).expect("encode"))
            .await
            .expect("put");
        drop(conn);
        sqlx::query("INSERT INTO pack_tarballs (issue_slug, tar_gz, digest, bytes, created_at, tree_digest) VALUES ('held', ''::bytea, 'x', 0, 'now', $1)")
            .bind(digest.as_str())
            .execute(&pool)
            .await
            .expect("hold the tree");
        age_every_tool(&pool).await;

        assert_eq!(collect(&pool).await.expect("collect").tools, 0);
        assert_eq!(stored_tools(&pool).await, 1);

        sqlx::query("DELETE FROM pack_tarballs WHERE issue_slug = 'held'")
            .execute(&pool)
            .await
            .expect("release the tree");
        let collected = collect(&pool).await.expect("collect");
        assert_eq!((collected.trees, collected.tools), (1, 1));
        assert_eq!(stored_tools(&pool).await, 0);
    }
}
