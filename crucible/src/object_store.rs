//! Published objects addressed by URI: `s3://bucket[/key]` or an absolute `file:///path`.

use anyhow::{Context, Result};

/// A URI this module cannot address.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ObjectUriError {
    #[error("results bucket must be an s3:// or file:// URI, got `{uri}`")]
    NotAnS3Uri { uri: String },
    #[error("file:// results root must be absolute: `{uri}`")]
    FileRootRelative { uri: String },
    #[error("results bucket URI has no bucket: `{uri}`")]
    NoBucket { uri: String },
    #[error("fetch object URI has no key: `{uri}`")]
    NoKey { uri: String },
}

/// A publish destination: S3 (`s3://bucket[/prefix]`) or a mounted filesystem
/// (`file:///abs/path`, e.g. an artifacts PVC on a cluster with no S3 reach). Both write the
/// exact same key layout, so reporting tools walk either.
pub(crate) enum Backend {
    // The S3 half re-parses the URI where it's used (the async block owns bucket/base), so the
    // variant carries nothing.
    S3,
    File { root: std::path::PathBuf },
}

pub(crate) fn backend(uri: &str) -> Result<Backend, ObjectUriError> {
    match uri.strip_prefix("file://") {
        Some(path) if path.starts_with('/') => Ok(Backend::File {
            root: std::path::PathBuf::from(path),
        }),
        Some(_) => Err(ObjectUriError::FileRootRelative {
            uri: uri.to_owned(),
        }),
        None => parse_s3_uri(uri).map(|_| Backend::S3),
    }
}

/// `s3://bucket[/prefix]` → (bucket, prefix). Prefix is trimmed of slashes and may
/// be empty.
pub(crate) fn parse_s3_uri(uri: &str) -> Result<(String, String), ObjectUriError> {
    let rest = uri
        .strip_prefix("s3://")
        .ok_or_else(|| ObjectUriError::NotAnS3Uri {
            uri: uri.to_owned(),
        })?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    if bucket.is_empty() {
        return Err(ObjectUriError::NoBucket {
            uri: uri.to_owned(),
        });
    }
    Ok((bucket.to_string(), prefix.trim_matches('/').to_string()))
}

/// Download one published object at an exact `s3://bucket/key` URI to a local file, the general
/// GetObject the controller's artifact proxy shells (`crucible fetch`), keeping every S3 client out
/// of `crucible-controller` (that crate has no aws-sdk and never learns the bucket layout). Nothing
/// is appended to the URI: the caller passes the exact key it wants. Reuses the same IRSA creds the
/// publisher uses (GetObject is in the role's policy).
pub fn fetch_object(uri: &str, dest: &std::path::Path) -> Result<()> {
    if let Backend::File { root } = backend(uri)? {
        // The file URI IS the object path; a plain copy is the whole fetch.
        std::fs::copy(&root, dest)
            .with_context(|| format!("copying {} to {}", root.display(), dest.display()))?;
        return Ok(());
    }
    let (bucket, key) = parse_s3_uri(uri)?;
    if key.is_empty() {
        return Err(ObjectUriError::NoKey {
            uri: uri.to_owned(),
        }
        .into());
    }
    crate::agent::engine::handle()?.block_on(async {
        let conf = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        let client = aws_sdk_s3::Client::new(&conf);
        let out = client
            .get_object()
            .bucket(&bucket)
            .key(&key)
            .send()
            .await
            .with_context(|| format!("GetObject s3://{bucket}/{key}"))?;
        let data = out
            .body
            .collect()
            .await
            .context("read object body")?
            .into_bytes();
        std::fs::write(dest, &data)
            .with_context(|| format!("writing {} ({} bytes)", dest.display(), data.len()))?;
        Ok::<(), anyhow::Error>(())
    })
}

#[cfg(test)]
mod tests {
    use crate::object_store::*;

    #[test]
    fn parse_s3_uri_splits_bucket_and_prefix() {
        assert_eq!(
            parse_s3_uri("s3://my-bucket/autoresearch").unwrap(),
            ("my-bucket".into(), "autoresearch".into())
        );
        assert_eq!(
            parse_s3_uri("s3://my-bucket").unwrap(),
            ("my-bucket".into(), String::new())
        );
        assert_eq!(
            parse_s3_uri("s3://my-bucket/a/b/").unwrap(),
            ("my-bucket".into(), "a/b".into())
        );
        assert!(parse_s3_uri("https://nope").is_err());
        assert!(parse_s3_uri("s3:///just-prefix").is_err());
    }

    #[test]
    fn a_file_uri_fetches_by_copy_and_a_relative_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("summary.txt");
        std::fs::write(&src, "investigation").unwrap();
        let dest = dir.path().join("fetched.txt");
        fetch_object(&format!("file://{}", src.display()), &dest).unwrap();
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "investigation");
        assert!(fetch_object("file://relative/path", &dest).is_err());
        assert!(
            fetch_object("s3://bucket", &dest).is_err(),
            "an s3 URI with no key"
        );
    }
}
