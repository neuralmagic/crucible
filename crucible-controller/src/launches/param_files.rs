//! File parameter values (RFC-0008 C-PARAM-TYPES): a launcher's uploads, the blobs a launch
//! owns, staging beside the pack on this controller's disk, and the pod-token route a run reads
//! its launch's files through when they are not on its disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use crucible::plan::starlark::params::{MAX_BYTES_KEY, MEDIA_TYPE_KEY, ParamType};
use crucible_contract::calls::MAX_FILE_PARAM_BYTES;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::state::{ApiError, ApiState, ErrorBody};
use crate::playbooks::registry::FieldError;
use crate::runs::pod_token::Grant;

/// The route a launcher uploads a file parameter's bytes to before the launch that names them.
pub(crate) const UPLOAD_ROUTE: &str = "/api/param-files/{digest}";

/// The most bytes one launcher's pending uploads may hold.
pub(crate) const MAX_UPLOADS_BYTES: i64 = 64 * 1024 * 1024;

/// How long an upload no launch took is kept.
const UPLOAD_KEPT_FOR: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// One file parameter value a launch owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParamFile {
    pub digest: String,
    pub size: u64,
    pub media_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// A file parameter value before admission checked it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Supplied {
    pub bytes: Vec<u8>,
    /// The file name it is staged under.
    pub name: Option<String>,
    pub media_type: Option<String>,
}

/// Where a launch's file parameter values come from.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FileValues<'a> {
    /// None: a value given for a file parameter is refused.
    None,
    /// A launcher's form: a file parameter's value is an [`UploadRef`] to one of `uploader`'s
    /// uploads.
    Uploaded { uploader: &'a str },
    /// A call's dispatch: each file parameter's bytes, read by the controller.
    Supplied(&'a BTreeMap<String, Supplied>),
}

/// What a launch form sends as a file parameter's value, after uploading the bytes to
/// [`UPLOAD_ROUTE`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UploadRef {
    /// `sha256:<hex>` of the uploaded bytes.
    pub digest: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub media_type: Option<String>,
}

/// What a file parameter's declaration admits.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSpec {
    max_bytes: u64,
    media_types: Vec<String>,
}

fn file_specs(schema: &Value) -> BTreeMap<String, FileSpec> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, p)| ParamType::from_schema(p) == ParamType::File)
        .map(|(name, p)| {
            let max_bytes = p
                .get(MAX_BYTES_KEY)
                .and_then(Value::as_u64)
                .unwrap_or(MAX_FILE_PARAM_BYTES)
                .min(MAX_FILE_PARAM_BYTES);
            let media_types = p
                .get(MEDIA_TYPE_KEY)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            (
                name.clone(),
                FileSpec {
                    max_bytes,
                    media_types,
                },
            )
        })
        .collect()
}

fn field(name: &str, message: String) -> FieldError {
    FieldError {
        field: name.to_string(),
        message,
    }
}

fn check(name: &str, spec: &FileSpec, file: &Supplied) -> Result<(), FieldError> {
    let size = u64::try_from(file.bytes.len()).unwrap_or(u64::MAX);
    if size > spec.max_bytes {
        return Err(field(
            name,
            format!("is {size} bytes; at most {} are allowed", spec.max_bytes),
        ));
    }
    if spec.media_types.is_empty() {
        return Ok(());
    }
    match &file.media_type {
        Some(media) if spec.media_types.contains(media) => Ok(()),
        Some(media) => Err(field(
            name,
            format!("is {media}; must be one of {}", spec.media_types.join(", ")),
        )),
        None => Err(field(
            name,
            format!(
                "names no media type; must be one of {}",
                spec.media_types.join(", ")
            ),
        )),
    }
}

/// The file parameter values of a launch against its params `schema`: `params` without them,
/// and each one checked against its declaration. A value for a parameter that is not a file
/// parameter, a text value for one that is, and a file past its declaration are refused.
pub(crate) async fn split(
    pool: &sqlx::PgPool,
    schema: &Value,
    params: &BTreeMap<String, String>,
    files: FileValues<'_>,
) -> anyhow::Result<Result<(BTreeMap<String, String>, BTreeMap<String, Supplied>), Vec<FieldError>>>
{
    let specs = file_specs(schema);
    let mut text = BTreeMap::new();
    let mut found = BTreeMap::new();
    let mut errors = Vec::new();
    for (name, value) in params {
        if !specs.contains_key(name) {
            text.insert(name.clone(), value.clone());
            continue;
        }
        let FileValues::Uploaded { uploader } = files else {
            errors.push(field(name, "takes an uploaded file, not text".to_string()));
            continue;
        };
        let reference = match crucible_contract::json::from_str::<UploadRef>(value) {
            Ok(reference) => reference,
            Err(e) => {
                errors.push(field(
                    name,
                    format!("is not an uploaded file reference: {e}"),
                ));
                continue;
            }
        };
        match upload(pool, uploader, &reference.digest).await? {
            Some(bytes) => {
                found.insert(
                    name.clone(),
                    Supplied {
                        bytes,
                        name: reference.name,
                        media_type: reference.media_type,
                    },
                );
            }
            None => errors.push(field(
                name,
                format!("names {}, which you have not uploaded", reference.digest),
            )),
        }
    }
    if let FileValues::Supplied(supplied) = files {
        for (name, file) in supplied {
            match specs.contains_key(name) {
                true => {
                    found.insert(name.clone(), file.clone());
                }
                false => errors.push(field(name, "is not a file parameter".to_string())),
            }
        }
    }
    for (name, file) in &found {
        if let Some(spec) = specs.get(name)
            && let Err(e) = check(name, spec, file)
        {
            errors.push(e);
        }
    }
    Ok(match errors.is_empty() {
        true => Ok((text, found)),
        false => Err(errors),
    })
}

/// A file name safe to stage under: the last component of `name`, with anything but letters,
/// digits, `.`, `-` and `_` replaced.
fn staged_name(name: Option<&str>, param: &str) -> String {
    let base = name
        .and_then(|n| n.rsplit(['/', '\\']).next())
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .unwrap_or(param);
    let clean: String = base
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                true => c,
                false => '_',
            },
        )
        .collect();
    match clean.trim_start_matches('.') {
        "" => "file".to_string(),
        rest => rest.to_string(),
    }
}

/// Write `file` under the controller's scratch root, addressed by its digest, and answer the
/// launch value the engine reads it by and the blob the launch owns.
pub(crate) async fn stage(
    scratch_root: &Path,
    param: &str,
    file: Supplied,
) -> anyhow::Result<(String, ParamFile)> {
    let digest = crucible_contract::artifact::content_digest(&file.bytes);
    let hex = digest.trim_start_matches("sha256:");
    let dir: PathBuf = std::path::absolute(scratch_root.join("param-files").join(hex))
        .context("resolving the staging directory")?;
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(staged_name(file.name.as_deref(), param));
    tokio::fs::write(&path, &file.bytes)
        .await
        .with_context(|| format!("staging {}", path.display()))?;
    let size = u64::try_from(file.bytes.len()).context("a file past u64")?;
    let mut reference = json!({
        "path": path.to_string_lossy(),
        "digest": digest,
        "size": size,
    });
    if let Some(media_type) = &file.media_type {
        reference["media_type"] = json!(media_type);
    }
    Ok((
        reference.to_string(),
        ParamFile {
            digest,
            size,
            media_type: file.media_type,
            bytes: file.bytes,
        },
    ))
}

/// Record the file values launch `launch` owns, in the transaction that mints it.
pub(crate) async fn own(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    launch: &str,
    files: &[ParamFile],
) -> anyhow::Result<()> {
    let now = crate::clock::now_rfc3339();
    for file in files {
        sqlx::query(
            "INSERT INTO launch_param_files (launch, digest, size, media_type, bytes, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (launch, digest) DO NOTHING",
        )
        .bind(launch)
        .bind(&file.digest)
        .bind(i64::try_from(file.size).context("a file past i64")?)
        .bind(&file.media_type)
        .bind(&file.bytes)
        .bind(&now)
        .execute(&mut **tx)
        .await
        .context("recording a launch's file parameter")?;
    }
    Ok(())
}

/// The bytes of the file of `digest` that the launch of run `run` owns.
pub(crate) async fn of_run(
    pool: &sqlx::PgPool,
    run: &str,
    digest: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    sqlx::query_scalar(
        "SELECT f.bytes FROM runs r JOIN launch_param_files f ON f.launch = r.issue \
         WHERE r.run_id = $1 AND f.digest = $2",
    )
    .bind(run)
    .bind(digest)
    .fetch_optional(pool)
    .await
    .context("reading a run's file parameter")
}

/// Store `bytes` as an upload of `uploader`. `Ok(false)` when it would take the uploader past
/// [`MAX_UPLOADS_BYTES`]. Uploads older than a day are dropped first.
pub(crate) async fn put_upload(
    pool: &sqlx::PgPool,
    uploader: &str,
    digest: &str,
    bytes: &[u8],
) -> anyhow::Result<bool> {
    let now = jiff::Timestamp::now();
    let stale = now
        .checked_sub(jiff::SignedDuration::try_from(UPLOAD_KEPT_FOR)?)
        .context("an upload cutoff before the epoch")?;
    sqlx::query("DELETE FROM param_file_uploads WHERE created_at < $1")
        .bind(stale.to_string())
        .execute(pool)
        .await
        .context("dropping stale uploads")?;
    let size = i64::try_from(bytes.len()).context("an upload past i64")?;
    let stored = sqlx::query(
        "INSERT INTO param_file_uploads (uploader, digest, size, bytes, created_at) \
         SELECT $1, $2, $3, $4, $5 \
         WHERE (SELECT COALESCE(SUM(size), 0) FROM param_file_uploads WHERE uploader = $1) + $3 \
               <= $6 \
         ON CONFLICT (uploader, digest) DO UPDATE SET created_at = EXCLUDED.created_at",
    )
    .bind(uploader)
    .bind(digest)
    .bind(size)
    .bind(bytes)
    .bind(now.to_string())
    .bind(MAX_UPLOADS_BYTES)
    .execute(pool)
    .await
    .context("storing an upload")?;
    Ok(stored.rows_affected() == 1)
}

/// The bytes of `uploader`'s upload of `digest`.
pub(crate) async fn upload(
    pool: &sqlx::PgPool,
    uploader: &str,
    digest: &str,
) -> anyhow::Result<Option<Vec<u8>>> {
    sqlx::query_scalar("SELECT bytes FROM param_file_uploads WHERE uploader = $1 AND digest = $2")
        .bind(uploader)
        .bind(digest)
        .fetch_optional(pool)
        .await
        .context("reading an upload")
}

fn refuse(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody::new(msg))).into_response()
}

/// `GET` [`crucible_contract::calls::PARAM_FILE_PATH`]: a file parameter value of the run's own
/// launch, to the run's own pod token.
pub(crate) async fn serve(
    Extension(grant): Extension<Grant>,
    State(state): State<ApiState>,
    UrlPath((run, digest)): UrlPath<(String, String)>,
) -> Response {
    if !grant.may_write_run(&run) {
        return refuse(
            StatusCode::FORBIDDEN,
            "this pod token reaches file parameters only of its own run",
        );
    }
    match of_run(state.db.pool(), &run, &digest).await {
        Ok(Some(bytes)) => (
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            bytes,
        )
            .into_response(),
        Ok(None) => refuse(
            StatusCode::NOT_FOUND,
            format!("run {run}'s launch holds no file {digest}"),
        ),
        Err(e) => ApiError::from(e).into_response(),
    }
}

/// What an upload stored.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct UploadAck {
    pub digest: String,
    pub size: u64,
}

/// `PUT /api/param-files/{digest}`: upload a file parameter's bytes, addressed by their
/// `sha256:<hex>` digest, for a launch to name within a day.
#[utoipa::path(
    put,
    path = "/api/param-files/{digest}",
    params(("digest" = String, Path, description = "sha256:<hex> of the body")),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 201, description = "Stored", body = UploadAck),
        (status = 401, description = "No identity to own the upload", body = ErrorBody),
        (status = 413, description = "Past the per-file or per-launcher cap", body = ErrorBody),
        (status = 422, description = "The bytes do not digest to the path's digest", body = ErrorBody)
    )
)]
pub(crate) async fn put(
    State(state): State<ApiState>,
    caller: crate::authz::Caller,
    UrlPath(digest): UrlPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let Some(uploader) = caller.subject_principal().map(|p| p.to_string()) else {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "an upload needs an identity to own it",
        );
    };
    let actual = crucible_contract::artifact::content_digest(&body);
    if actual != digest {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the bytes digest to {actual}, not {digest}"),
        );
    }
    match put_upload(state.db.pool(), &uploader, &digest, &body).await {
        Ok(true) => (
            StatusCode::CREATED,
            Json(UploadAck {
                digest,
                size: u64::try_from(body.len()).unwrap_or(u64::MAX),
            }),
        )
            .into_response(),
        Ok(false) => refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("your uploads would pass {MAX_UPLOADS_BYTES} bytes; launch or wait a day"),
        ),
        Err(e) => ApiError::from(e).into_response(),
    }
}

/// The body cap of [`UPLOAD_ROUTE`].
pub(crate) fn upload_limit() -> axum::extract::DefaultBodyLimit {
    axum::extract::DefaultBodyLimit::max(
        usize::try_from(MAX_FILE_PARAM_BYTES).unwrap_or(usize::MAX),
    )
}

#[cfg(test)]
mod tests {
    use crate::launches::param_files::*;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "label": {"type": "string"},
                "notes": {"type": "object", "x-param-type": "file", "x-max-bytes": 8,
                          "x-media-type": ["text/markdown"]},
                "blob": {"type": "object", "x-param-type": "file", "x-max-bytes": 64},
            },
        })
    }

    fn supplied(bytes: &[u8], media: Option<&str>) -> Supplied {
        Supplied {
            bytes: bytes.to_vec(),
            name: Some("notes.md".into()),
            media_type: media.map(str::to_string),
        }
    }

    #[test]
    fn a_file_declaration_bounds_its_size_and_media_type() {
        let specs = file_specs(&schema());
        assert_eq!(specs.len(), 2);
        let notes = &specs["notes"];
        assert!(check("notes", notes, &supplied(b"# hi", Some("text/markdown"))).is_ok());
        assert!(
            check(
                "notes",
                notes,
                &supplied(b"123456789", Some("text/markdown"))
            )
            .expect_err("too big")
            .message
            .contains("at most 8")
        );
        assert!(
            check("notes", notes, &supplied(b"x", Some("image/png")))
                .expect_err("media")
                .message
                .contains("text/markdown")
        );
        assert!(
            check("notes", notes, &supplied(b"x", None))
                .expect_err("no media")
                .message
                .contains("names no media type")
        );
        assert!(check("blob", &specs["blob"], &supplied(b"x", None)).is_ok());
    }

    #[test]
    fn a_staged_name_cannot_leave_its_directory() {
        assert_eq!(staged_name(Some("notes.md"), "p"), "notes.md");
        assert_eq!(staged_name(Some("../../etc/passwd"), "p"), "passwd");
        assert_eq!(staged_name(Some(".."), "p"), "p");
        assert_eq!(staged_name(Some("a b$.txt"), "p"), "a_b_.txt");
        assert_eq!(staged_name(Some(".hidden"), "p"), "hidden");
        assert_eq!(staged_name(None, "notes"), "notes");
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn values_split_into_text_and_checked_files(pool: sqlx::PgPool) {
        let params = BTreeMap::from([
            ("label".to_string(), "hi".to_string()),
            (
                "notes".to_string(),
                json!({"digest": crucible_contract::artifact::content_digest(b"# hi"),
                       "name": "notes.md", "media_type": "text/markdown"})
                .to_string(),
            ),
        ]);
        assert!(
            put_upload(
                &pool,
                "user:wren",
                &crucible_contract::artifact::content_digest(b"# hi"),
                b"# hi"
            )
            .await
            .expect("upload")
        );
        let (text, files) = split(
            &pool,
            &schema(),
            &params,
            FileValues::Uploaded {
                uploader: "user:wren",
            },
        )
        .await
        .expect("split")
        .expect("admitted");
        assert_eq!(
            text,
            BTreeMap::from([("label".to_string(), "hi".to_string())])
        );
        assert_eq!(files["notes"].bytes, b"# hi");

        let refused = split(
            &pool,
            &schema(),
            &params,
            FileValues::Uploaded {
                uploader: "user:mallory",
            },
        )
        .await
        .expect("split")
        .expect_err("another's upload");
        assert!(refused[0].message.contains("not uploaded"), "{refused:?}");
        let refused = split(&pool, &schema(), &params, FileValues::None)
            .await
            .expect("split")
            .expect_err("text for a file");
        assert_eq!(refused[0].field, "notes");

        let wrong = BTreeMap::from([("label".to_string(), supplied(b"x", None))]);
        let refused = split(
            &pool,
            &schema(),
            &BTreeMap::new(),
            FileValues::Supplied(&wrong),
        )
        .await
        .expect("split")
        .expect_err("not a file parameter");
        assert!(refused[0].message.contains("not a file parameter"));
        let big = BTreeMap::from([(
            "notes".to_string(),
            supplied(b"123456789", Some("text/markdown")),
        )]);
        assert!(
            split(
                &pool,
                &schema(),
                &BTreeMap::new(),
                FileValues::Supplied(&big)
            )
            .await
            .expect("split")
            .is_err()
        );
    }

    #[sqlx::test(migrator = "crucible_controller::MIGRATOR")]
    async fn one_launchers_uploads_are_capped(pool: sqlx::PgPool) {
        let big = vec![7u8; usize::try_from(MAX_UPLOADS_BYTES / 2).expect("size")];
        let mut other = big.clone();
        other[0] = 8;
        let mut third = big.clone();
        third[0] = 9;
        let d = |b: &[u8]| crucible_contract::artifact::content_digest(b);
        assert!(
            put_upload(&pool, "user:wren", &d(&big), &big)
                .await
                .expect("first")
        );
        assert!(
            put_upload(&pool, "user:wren", &d(&other), &other)
                .await
                .expect("second")
        );
        assert!(
            !put_upload(&pool, "user:wren", &d(&third), &third)
                .await
                .expect("third"),
            "a third half-cap upload passes the cap"
        );
        assert!(
            put_upload(&pool, "user:ash", &d(&third), &third)
                .await
                .expect("ash"),
            "the cap is per launcher"
        );
        sqlx::query("UPDATE param_file_uploads SET created_at = '2020-01-01T00:00:00Z'")
            .execute(&pool)
            .await
            .expect("age");
        assert!(
            put_upload(&pool, "user:wren", &d(&third), &third)
                .await
                .expect("after a day"),
            "stale uploads leave the cap"
        );
    }

    #[tokio::test]
    async fn a_staged_file_is_named_by_its_digest_and_read_back_by_the_engine_reference() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (value, owned) = stage(
            dir.path(),
            "notes",
            supplied(b"# hi", Some("text/markdown")),
        )
        .await
        .expect("staged");
        let reference: Value = serde_json::from_str(&value).expect("json");
        assert_eq!(
            reference["digest"],
            json!(crucible_contract::artifact::content_digest(b"# hi"))
        );
        assert_eq!(reference["size"], json!(4));
        assert_eq!(reference["media_type"], json!("text/markdown"));
        let path = reference["path"].as_str().expect("path");
        assert!(path.ends_with("/notes.md"), "{path}");
        assert_eq!(std::fs::read(path).expect("read"), b"# hi");
        assert_eq!(owned.digest, reference["digest"].as_str().expect("digest"));
    }
}
