//! Artifact payloads in an S3-compatible bucket. The `artifacts` row stays in Postgres and names
//! the object by `object_key`; this module only moves bytes and never decides what a row holds.

use crate::runs::blob_store::{Chunker, PutArtifactError};
use anyhow::{Context, Result, anyhow};
use aws_sdk_s3::config::{RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::DisplayErrorContext;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use futures_util::{Stream, StreamExt};
use std::str::FromStr;

/// Multipart part size. S3 refuses a part under 5 MiB unless it is the last one.
const PART_BYTES: usize = 8 * 1024 * 1024;

/// Why a `--evidence-bucket` value was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BucketUriError {
    #[error("evidence bucket must be an s3:// URI like s3://bucket/prefix, got `{uri}`")]
    NotS3 { uri: String },
    #[error("evidence bucket URI names no bucket: `{uri}`")]
    NoBucket { uri: String },
}

/// `s3://bucket[/prefix]`: the bucket payloads go to and the key prefix under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketUri {
    bucket: String,
    prefix: String,
}

impl BucketUri {
    pub fn bucket(&self) -> &str {
        &self.bucket
    }
}

impl FromStr for BucketUri {
    type Err = BucketUriError;

    fn from_str(uri: &str) -> Result<Self, Self::Err> {
        let rest = uri
            .strip_prefix("s3://")
            .ok_or_else(|| BucketUriError::NotS3 {
                uri: uri.to_string(),
            })?;
        let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
        if bucket.is_empty() {
            return Err(BucketUriError::NoBucket {
                uri: uri.to_string(),
            });
        }
        Ok(BucketUri {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
        })
    }
}

/// A bucket client plus where in it payloads live. Cheap to clone.
#[derive(Clone, Debug)]
pub struct ObjectStore {
    client: aws_sdk_s3::Client,
    location: BucketUri,
}

/// What [`ObjectStore::upload`] wrote.
#[derive(Debug)]
pub(crate) struct Uploaded {
    pub(crate) digest: String,
    pub(crate) bytes: u64,
}

impl ObjectStore {
    /// Credentials, region and endpoint from the standard AWS environment and profile chain.
    pub async fn from_env(location: BucketUri) -> Self {
        let conf = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        ObjectStore::from_sdk_config(&conf, location)
    }

    /// A custom endpoint (`AWS_ENDPOINT_URL`) switches to path-style addressing. Request
    /// checksums are sent only when an operation requires them: some S3-compatible servers reject
    /// the SDK's default trailing checksums.
    pub fn from_sdk_config(conf: &aws_config::SdkConfig, location: BucketUri) -> Self {
        let s3 = aws_sdk_s3::config::Builder::from(conf)
            .force_path_style(conf.endpoint_url().is_some())
            .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
            .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
            .build();
        ObjectStore {
            client: aws_sdk_s3::Client::from_conf(s3),
            location,
        }
    }

    /// Fail at startup, not on the first upload, when the bucket is unreachable or unauthorized.
    pub async fn check(&self) -> Result<()> {
        self.client
            .head_bucket()
            .bucket(&self.location.bucket)
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "evidence bucket {} is not reachable: {}",
                    self.location.bucket,
                    DisplayErrorContext(e)
                )
            })?;
        Ok(())
    }

    /// A fresh key for one payload write. Keys are never reused, so a replaced payload's object
    /// can be deleted after the row moves without racing a reader of the new one.
    pub(crate) fn new_key(&self) -> String {
        let id = uuid::Uuid::now_v7();
        match self.location.prefix.as_str() {
            "" => format!("artifacts/{id}"),
            prefix => format!("{prefix}/artifacts/{id}"),
        }
    }

    /// Stream a payload to `key`, hashing inline and refusing past `max_bytes`. A failed upload
    /// aborts its multipart upload, so nothing is left at `key`.
    pub(crate) async fn upload<S, B, E>(
        &self,
        key: &str,
        max_bytes: u64,
        stream: S,
    ) -> Result<Uploaded, PutArtifactError>
    where
        S: Stream<Item = Result<B, E>> + Unpin,
        B: AsRef<[u8]>,
        E: std::fmt::Display,
    {
        let mut upload = Upload {
            store: self,
            key,
            multipart: None,
        };
        let result = upload.run(max_bytes, stream).await;
        if result.is_err() {
            upload.abort().await;
        }
        result
    }

    pub(crate) async fn get(&self, key: &str) -> Result<Vec<u8>> {
        let out = self
            .client
            .get_object()
            .bucket(&self.location.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| anyhow!("GetObject {key}: {}", DisplayErrorContext(e)))?;
        let body = out
            .body
            .collect()
            .await
            .with_context(|| format!("reading object {key}"))?;
        Ok(body.into_bytes().to_vec())
    }

    pub(crate) async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.location.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| anyhow!("DeleteObject {key}: {}", DisplayErrorContext(e)))?;
        Ok(())
    }
}

struct Multipart {
    upload_id: String,
    parts: Vec<CompletedPart>,
}

struct Upload<'a> {
    store: &'a ObjectStore,
    key: &'a str,
    multipart: Option<Multipart>,
}

impl Upload<'_> {
    async fn run<S, B, E>(
        &mut self,
        max_bytes: u64,
        mut stream: S,
    ) -> Result<Uploaded, PutArtifactError>
    where
        S: Stream<Item = Result<B, E>> + Unpin,
        B: AsRef<[u8]>,
        E: std::fmt::Display,
    {
        let mut chunker = Chunker::new(PART_BYTES);
        while let Some(piece) = stream.next().await {
            let piece = piece.map_err(|e| PutArtifactError::Source(e.to_string()))?;
            let piece = piece.as_ref();
            if chunker.total() + piece.len() as u64 > max_bytes {
                return Err(PutArtifactError::TooLarge { limit: max_bytes });
            }
            for part in chunker.push(piece) {
                self.put_part(part).await?;
            }
        }
        let (tail, digest, bytes) = chunker.finish();
        if self.multipart.is_none() {
            self.put_whole(tail.unwrap_or_default()).await?;
        } else {
            if let Some(tail) = tail {
                self.put_part(tail).await?;
            }
            self.complete().await?;
        }
        Ok(Uploaded { digest, bytes })
    }

    async fn put_whole(&self, data: Vec<u8>) -> Result<()> {
        let s = self.store;
        s.client
            .put_object()
            .bucket(&s.location.bucket)
            .key(self.key)
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(|e| anyhow!("PutObject {}: {}", self.key, DisplayErrorContext(e)))?;
        Ok(())
    }

    async fn put_part(&mut self, data: Vec<u8>) -> Result<()> {
        let s = self.store;
        let multipart = match &mut self.multipart {
            Some(multipart) => multipart,
            None => {
                let created = s
                    .client
                    .create_multipart_upload()
                    .bucket(&s.location.bucket)
                    .key(self.key)
                    .send()
                    .await
                    .map_err(|e| {
                        anyhow!(
                            "CreateMultipartUpload {}: {}",
                            self.key,
                            DisplayErrorContext(e)
                        )
                    })?;
                let upload_id = created.upload_id.with_context(|| {
                    format!("CreateMultipartUpload {} returned no upload id", self.key)
                })?;
                self.multipart.insert(Multipart {
                    upload_id,
                    parts: Vec::new(),
                })
            }
        };
        let part_number =
            i32::try_from(multipart.parts.len() + 1).context("multipart part count exceeds i32")?;
        let out = s
            .client
            .upload_part()
            .bucket(&s.location.bucket)
            .key(self.key)
            .upload_id(&multipart.upload_id)
            .part_number(part_number)
            .body(ByteStream::from(data))
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "UploadPart {} #{part_number}: {}",
                    self.key,
                    DisplayErrorContext(e)
                )
            })?;
        multipart.parts.push(
            CompletedPart::builder()
                .part_number(part_number)
                .set_e_tag(out.e_tag)
                .build(),
        );
        Ok(())
    }

    async fn complete(&mut self) -> Result<()> {
        let s = self.store;
        let Some(multipart) = &self.multipart else {
            return Ok(());
        };
        s.client
            .complete_multipart_upload()
            .bucket(&s.location.bucket)
            .key(self.key)
            .upload_id(&multipart.upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(multipart.parts.clone()))
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                anyhow!(
                    "CompleteMultipartUpload {}: {}",
                    self.key,
                    DisplayErrorContext(e)
                )
            })?;
        self.multipart = None;
        Ok(())
    }

    async fn abort(&mut self) {
        let Some(multipart) = self.multipart.take() else {
            return;
        };
        let s = self.store;
        if let Err(e) = s
            .client
            .abort_multipart_upload()
            .bucket(&s.location.bucket)
            .key(self.key)
            .upload_id(&multipart.upload_id)
            .send()
            .await
        {
            tracing::warn!(
                key = self.key,
                error = %DisplayErrorContext(e),
                "aborting a failed multipart upload"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::runs::blob_store::objects::*;

    #[test]
    fn a_bucket_uri_splits_bucket_and_trimmed_prefix() {
        let uri: BucketUri = "s3://evidence/crucible/prod/".parse().expect("parse");
        assert_eq!(uri.bucket(), "evidence");
        assert_eq!(uri.prefix, "crucible/prod");
        let bare: BucketUri = "s3://evidence".parse().expect("parse");
        assert_eq!(bare.prefix, "");
    }

    #[test]
    fn a_bucket_uri_refuses_other_schemes_and_a_missing_bucket() {
        assert_eq!(
            "https://evidence".parse::<BucketUri>(),
            Err(BucketUriError::NotS3 {
                uri: "https://evidence".to_string()
            })
        );
        assert_eq!(
            "s3:///prefix".parse::<BucketUri>(),
            Err(BucketUriError::NoBucket {
                uri: "s3:///prefix".to_string()
            })
        );
    }
}
