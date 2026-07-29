//! Native-OCI image derivation: append a file-overlay layer onto a base image and push the result,
//! WITHOUT a container runtime. The buildah-free half of the build machinery, for the case where the
//! candidate is "the base image plus one or two edited files": the layer-append path for
//! interpreted languages; compiled domains still go through buildah.
//!
//! ```text
//!   pull base manifest+config  ->  build one tar+gzip overlay layer (the edited files)
//!   copy base layers into the target repo: server-side MOUNT when registries match (instant),
//!     else stream pull->push (low memory, never the whole layer in RAM)
//!   push the overlay blob + the rewritten config (new diff_id) + the new manifest
//!   -> target_repo@<digest>   (a real immutable image; set image, no runtime mount)
//! ```

use crate::layer::{BuiltLayer, LayerEntry, build_layer_tar};
use anyhow::{Context, Result};
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::{Engine, alphabet};
use futures_util::TryStreamExt;
use oci_client::client::{ClientConfig, ClientProtocol, linux_amd64_resolver};
use oci_client::errors::OciDistributionError;
use oci_client::manifest::{OciDescriptor, OciImageManifest, OciManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference, RegistryOperation};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Authfile `auth` tokens are standard base64, but some writers omit padding; decode either.
const AUTHFILE_B64: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// Registry ops ride real networks; give each one two extra attempts with a short backoff before
/// failing the whole derivation. Auth errors won't heal, but the retries are cheap next to losing
/// an entire apply to a transient 5xx.
pub(crate) const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(500), Duration::from_secs(2)];

pub(crate) async fn with_retry<T, Fut>(
    what: &str,
    delays: &[Duration],
    mut op: impl FnMut() -> Fut,
) -> Result<T>
where
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(e) if attempt < delays.len() => {
                tracing::warn!(
                    attempt = attempt + 1,
                    error = format!("{e:#}"),
                    "{what}: retrying"
                );
                tokio::time::sleep(delays[attempt]).await;
                attempt += 1;
            }
            Err(e) => return Err(e.context(format!("{what} (after {} attempts)", attempt + 1))),
        }
    }
}

/// A file to bake into the overlay layer: its path INSIDE the image (no leading slash, e.g.
/// `usr/local/lib/python3.12/dist-packages/app/metrics.py`) + its bytes.
pub struct OverlayFile {
    pub path: String,
    pub contents: Vec<u8>,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Build one deterministic layer holding `files` at their in-image paths (permissions 0644). The
/// "base image + a couple edited files" case; the diff-capture path in [`crate::capture`] builds its
/// [`BuiltLayer`] from a scanned overlayfs upperdir instead, then pushes through the same core.
fn build_overlay_layer(files: &[OverlayFile]) -> Result<BuiltLayer> {
    let entries: Vec<LayerEntry> = files
        .iter()
        .map(|f| LayerEntry::File {
            path: f.path.clone(),
            mode: 0o644,
            uid: 0,
            gid: 0,
            contents: f.contents.clone(),
        })
        .collect();
    let tar = build_layer_tar(&entries)?;
    BuiltLayer::from_tar(&tar)
}

/// Append `diff_id` to the base config's `rootfs.diff_ids` (and a history breadcrumb carrying
/// `comment`), so the config's layer chain matches the manifest's. Returns the serialized config bytes
/// + its digest + size.
fn rewrite_config(
    config_json: &str,
    diff_id: &str,
    comment: &str,
) -> Result<(Vec<u8>, String, i64)> {
    let mut cfg: serde_json::Value =
        serde_json::from_str(config_json).context("parsing base image config")?;
    let diffs = cfg
        .pointer_mut("/rootfs/diff_ids")
        .and_then(|v| v.as_array_mut())
        .context("base image config has no rootfs.diff_ids array")?;
    diffs.push(serde_json::Value::String(diff_id.to_string()));
    if let Some(history) = cfg.get_mut("history").and_then(|v| v.as_array_mut()) {
        history.push(serde_json::json!({
            "created_by": "forge::oci",
            "comment": comment,
        }));
    }
    let bytes = serde_json::to_vec(&cfg).context("serializing rewritten config")?;
    let digest = format!("sha256:{}", sha256_hex(&bytes));
    let size = bytes.len() as i64;
    Ok((bytes, digest, size))
}

/// The authfile keys that may hold `registry`'s creds. Docker Hub is the special case: `docker login`
/// historically writes the legacy `https://index.docker.io/v1/` key, so a `docker.io` ref must check
/// the whole alias family. Other registries get the plain host plus the `https://`-prefixed spelling
/// some tools write.
fn authfile_keys(registry: &str) -> Vec<String> {
    const DOCKER_ALIASES: [&str; 4] = [
        "docker.io",
        "index.docker.io",
        "registry-1.docker.io",
        "https://index.docker.io/v1/",
    ];
    if DOCKER_ALIASES.contains(&registry) {
        DOCKER_ALIASES.iter().map(|s| s.to_string()).collect()
    } else {
        vec![registry.to_string(), format!("https://{registry}")]
    }
}

/// The auth in one `auths.<key>` entry: `auth` (base64 `user:pass`, the common form), else explicit
/// `username`/`password` fields. `None` when the entry carries neither (e.g. an identitytoken-only
/// entry; that needs an OAuth exchange oci-client doesn't do, so we don't pretend).
fn entry_auth(entry: &serde_json::Value) -> Result<Option<RegistryAuth>> {
    if let Some(b64) = entry.get("auth").and_then(|v| v.as_str()) {
        let decoded = AUTHFILE_B64
            .decode(b64.trim())
            .context("decoding the auth token")?;
        let creds = String::from_utf8(decoded).context("auth token is not utf8")?;
        let (user, pass) = creds
            .split_once(':')
            .context("auth token is not user:pass")?;
        return Ok(Some(RegistryAuth::Basic(
            user.to_string(),
            pass.to_string(),
        )));
    }
    let user = entry.get("username").and_then(|v| v.as_str());
    let pass = entry.get("password").and_then(|v| v.as_str());
    if let (Some(user), Some(pass)) = (user, pass) {
        return Ok(Some(RegistryAuth::Basic(
            user.to_string(),
            pass.to_string(),
        )));
    }
    Ok(None)
}

/// Build a [`RegistryAuth`] for `registry` from a docker `config.json`-format authfile (the same file
/// buildah uses), checking every key alias the registry is known by (`docker.io` creds usually live
/// under the legacy `https://index.docker.io/v1/`). Returns `Anonymous` if the file or the entry is
/// absent, so public bases just work.
fn auth_from_authfile(authfile: &Path, registry: &str) -> Result<RegistryAuth> {
    let Ok(text) = std::fs::read_to_string(authfile) else {
        return Ok(RegistryAuth::Anonymous);
    };
    let cfg: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing authfile {}", authfile.display()))?;
    let Some(auths) = cfg.get("auths").and_then(|v| v.as_object()) else {
        return Ok(RegistryAuth::Anonymous);
    };
    for key in authfile_keys(registry) {
        if let Some(entry) = auths.get(&key)
            && let Some(auth) = entry_auth(entry)
                .with_context(|| format!("authfile entry `{key}` in {}", authfile.display()))?
        {
            return Ok(auth);
        }
    }
    Ok(RegistryAuth::Anonymous)
}

/// Resolve creds for `registry` the way docker/crane do: the explicit authfile when given, else the
/// operator's default (`$REGISTRY_AUTH_FILE` / `~/.docker/config.json`), else anonymous.
pub fn resolve_auth(authfile: Option<&Path>, registry: &str) -> Result<RegistryAuth> {
    match authfile.map(Path::to_path_buf).or_else(default_authfile) {
        Some(f) => auth_from_authfile(&f, registry),
        None => Ok(RegistryAuth::Anonymous),
    }
}

/// Resolve `image_ref` (`registry/repo:tag`) to a digest-pinned ref (`registry/repo@sha256:…`) via the
/// registry's distribution API, with no `crane`/`skopeo` shell-out. `authfile`, when given, supplies creds
/// for a private repo (same docker-config format buildah uses); absent/anonymous otherwise. Blocking
/// wrapper around the async HEAD so it drops into crucible's sync deploy renderer.
pub fn pin_digest(image_ref: &str, authfile: Option<&Path>) -> Result<String> {
    let reference: Reference = image_ref
        .parse()
        .with_context(|| format!("parsing image ref {image_ref}"))?;
    // Already pinned: nothing to resolve, and the registry round-trip would demand creds a
    // render host may not have (a digest ref is immutable by definition).
    if let Some(digest) = reference.digest() {
        let repo = format!("{}/{}", reference.registry(), reference.repository());
        return Ok(format!("{repo}@{digest}"));
    }
    // A private image needs creds; anonymous HEADs 401.
    let auth = resolve_auth(authfile, reference.registry())?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the digest-resolve runtime")?;
    let digest = rt.block_on(async {
        let client = Client::new(ClientConfig::default());
        with_retry(
            &format!("fetching manifest digest for {image_ref}"),
            &RETRY_DELAYS,
            || async {
                client
                    .fetch_manifest_digest(&reference, &auth)
                    .await
                    .map_err(anyhow::Error::from)
            },
        )
        .await
    })?;
    // Pin against the repo (registry/repo, no tag): `registry/repo:tag` -> `registry/repo@sha256:…`.
    let repo = format!("{}/{}", reference.registry(), reference.repository());
    Ok(format!("{repo}@{digest}"))
}

/// The registry host of an image ref, parsed per the OCI distribution spec (Docker Hub shorthand like
/// `ubuntu` resolves to `docker.io`). For keying a per-registry credential lookup off an image ref
/// with a proper parse instead of string-splitting.
pub fn registry_of(image_ref: &str) -> Result<String> {
    let reference: Reference = image_ref
        .parse()
        .with_context(|| format!("parsing image ref {image_ref}"))?;
    Ok(reference.registry().to_string())
}

/// The tag of an image ref, parsed per the OCI spec (`registry/repo:tag` -> `tag`); `latest` when the ref
/// carries no tag (e.g. a digest ref or a bare repo) or doesn't parse. Used to label a build's COMMIT_SHA.
pub(crate) fn tag_of(image_ref: &str) -> String {
    image_ref
        .parse::<Reference>()
        .ok()
        .and_then(|r| r.tag().map(str::to_string))
        .unwrap_or_else(|| "latest".to_string())
}

/// The operator's default registry authfile, the way docker/crane find it: `$REGISTRY_AUTH_FILE`, else
/// `$DOCKER_CONFIG/config.json`, else `~/.docker/config.json`. `None` if none exists (anonymous pull).
fn default_authfile() -> Option<PathBuf> {
    if let Ok(f) = std::env::var("REGISTRY_AUTH_FILE") {
        let p = PathBuf::from(f);
        if p.exists() {
            return Some(p);
        }
    }
    let docker_config = std::env::var("DOCKER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".docker")
        });
    let p = docker_config.join("config.json");
    p.exists().then_some(p)
}

/// Derive `target_ref` = `base_ref` + an overlay layer of `files`, pushing the result and returning its
/// digest-pinned ref. `base_auth` pulls the base (anonymous for a public upstream, real creds for a
/// private base, including the same-registry mirror, where it matches `target_auth`). Async: the caller
/// owns the runtime and drives this on it.
pub async fn derive_layer(
    base_ref: &str,
    target_ref: &str,
    files: &[OverlayFile],
    base_auth: &RegistryAuth,
    target_auth: &RegistryAuth,
) -> Result<String> {
    let layer = build_overlay_layer(files)?;
    push_layer(
        base_ref,
        target_ref,
        layer,
        "agentic candidate file overlay",
        base_auth,
        target_auth,
    )
    .await
}

/// An OCI client whose platform resolver always picks the linux/amd64 entry of a manifest list: the
/// target cluster runs amd64 regardless of where this runs (an arm64 laptop's current-platform resolver would pick the
/// wrong arch or miss). Registries named in `FORGE_INSECURE_REGISTRIES` (comma-separated, e.g. a local
/// `registry:2` in an e2e run) are contacted over plain HTTP; everything else stays HTTPS.
fn amd64_client() -> Client {
    let mut cfg = ClientConfig {
        platform_resolver: Some(Box::new(linux_amd64_resolver)),
        ..Default::default()
    };
    if let Ok(list) = std::env::var("FORGE_INSECURE_REGISTRIES")
        && !list.trim().is_empty()
    {
        cfg.protocol =
            ClientProtocol::HttpsExcept(list.split(',').map(|s| s.trim().to_string()).collect());
    }
    Client::new(cfg)
}

/// Derive `target_ref` = `base_ref` + a single pre-built [`BuiltLayer`], pushing the result and
/// returning its digest-pinned ref. The shared append-and-push core: the derive-file path and the
/// diff-capture path both build a [`BuiltLayer`] their own way, then hand it here.
pub(crate) async fn push_layer(
    base_ref: &str,
    target_ref: &str,
    layer: BuiltLayer,
    comment: &str,
    base_auth: &RegistryAuth,
    target_auth: &RegistryAuth,
) -> Result<String> {
    let base: Reference = base_ref
        .parse()
        .with_context(|| format!("parsing base ref {base_ref}"))?;
    let target: Reference = target_ref
        .parse()
        .with_context(|| format!("parsing target ref {target_ref}"))?;
    let client = amd64_client();
    tracing::info!(
        base = base_ref,
        target = target_ref,
        layer_bytes = layer.gzip.len(),
        diff_id = layer.diff_id(),
        "pushing derived image"
    );

    // 1. Base manifest + config, with the caller's base creds (resolved per-registry from the same
    //    authfile as the push creds; identical to target_auth for the seed-and-mount mirror case).
    let same_registry = base.registry() == target.registry();
    let (manifest, _digest, config_json) = with_retry(
        &format!("pulling base manifest/config for {base_ref}"),
        &RETRY_DELAYS,
        || async {
            client
                .pull_manifest_and_config(&base, base_auth)
                .await
                .map_err(anyhow::Error::from)
        },
    )
    .await?;

    // 2. Match the base layers' media type so the image stays internally consistent.
    let layer_media = manifest
        .layers
        .last()
        .map(|l| l.media_type.clone())
        .unwrap_or_else(|| "application/vnd.oci.image.layer.v1.tar+gzip".to_string());

    // Auth the target for push (the low-level blob/manifest calls use the stored token), and the base for
    // pull when we have to stream across registries.
    with_retry(
        "authenticating to the target registry for push",
        &RETRY_DELAYS,
        || async {
            client
                .auth(&target, target_auth, RegistryOperation::Push)
                .await
                .map_err(anyhow::Error::from)
        },
    )
    .await?;
    if !same_registry {
        with_retry(
            "authenticating to the base registry for pull",
            &RETRY_DELAYS,
            || async {
                client
                    .auth(&base, base_auth, RegistryOperation::Pull)
                    .await
                    .map_err(anyhow::Error::from)
            },
        )
        .await?;
    }

    // 3. Get the base layer blobs into the target repo: server-side MOUNT when registries match (no
    //    transfer), else stream pull->push (low memory). Registries dedup by digest, so re-runs skip
    //    them, which also makes retrying a whole cross-registry copy safe (a half-pushed blob is
    //    simply re-uploaded).
    for l in &manifest.layers {
        if same_registry {
            with_retry(
                &format!("mounting base layer {}", l.digest),
                &RETRY_DELAYS,
                || async {
                    client
                        .mount_blob(&target, &base, &l.digest)
                        .await
                        .map_err(anyhow::Error::from)
                },
            )
            .await?;
        } else {
            with_retry(
                &format!("copying base layer {} to the target repo", l.digest),
                &RETRY_DELAYS,
                || async {
                    let sized = client
                        .pull_blob_stream(&base, l)
                        .await
                        .context("streaming from the base repo")?;
                    // SizedStream yields io::Error; push_blob_stream wants the oci error, so map it across.
                    let stream = sized
                        .stream
                        .map_err(|e| OciDistributionError::GenericError(Some(e.to_string())));
                    client
                        .push_blob_stream(&target, stream, &l.digest)
                        .await
                        .context("pushing to the target repo")?;
                    Ok(())
                },
            )
            .await?;
        }
    }

    // 4. Push the overlay blob + the rewritten config blob.
    let (config_bytes, config_digest, config_size) =
        rewrite_config(&config_json, &layer.diff_id, comment)?;
    let config_media = manifest.config.media_type.clone();
    with_retry("pushing the overlay layer blob", &RETRY_DELAYS, || async {
        client
            .push_blob(&target, layer.gzip.clone(), &layer.blob_digest)
            .await
            .map_err(anyhow::Error::from)
    })
    .await?;
    with_retry(
        "pushing the rewritten config blob",
        &RETRY_DELAYS,
        || async {
            client
                .push_blob(&target, config_bytes.clone(), &config_digest)
                .await
                .map_err(anyhow::Error::from)
        },
    )
    .await?;

    // 5. Assemble + push the new manifest: base layers + the overlay, pointing at the rewritten config.
    let mut new_manifest: OciImageManifest = manifest.clone();
    new_manifest.config = OciDescriptor {
        media_type: config_media,
        digest: config_digest,
        size: config_size,
        ..Default::default()
    };
    new_manifest.layers.push(OciDescriptor {
        media_type: layer_media,
        digest: layer.blob_digest.clone(),
        size: layer.size,
        ..Default::default()
    });
    let to_push = OciManifest::Image(new_manifest);
    let pushed = with_retry("pushing the derived manifest", &RETRY_DELAYS, || async {
        client
            .push_manifest(&target, &to_push)
            .await
            .map_err(anyhow::Error::from)
    })
    .await?;

    // push_manifest returns the manifest URL (…/manifests/sha256:…); pull the digest out for a clean ref.
    let digest = pushed
        .rsplit_once("sha256:")
        .map(|(_, hex)| format!("sha256:{hex}"))
        .unwrap_or(pushed);
    let pinned = format!("{}/{}@{}", target.registry(), target.repository(), digest);
    tracing::info!(pinned, "derived image pushed");
    Ok(pinned)
}

/// The base image's own environment (`config.Env`, each `K=V`), so a captured command runs under the
/// image's PATH / VIRTUAL_ENV / etc. rather than an empty chroot environment. Empty when the image
/// declares none.
pub(crate) async fn image_env(
    base_ref: &str,
    auth: &RegistryAuth,
) -> Result<Vec<(String, String)>> {
    let reference: Reference = base_ref
        .parse()
        .with_context(|| format!("parsing base ref {base_ref}"))?;
    let client = amd64_client();
    let (_manifest, _digest, config_json) = with_retry(
        &format!("pulling base config for {base_ref}"),
        &RETRY_DELAYS,
        || async {
            client
                .pull_manifest_and_config(&reference, auth)
                .await
                .map_err(anyhow::Error::from)
        },
    )
    .await?;
    let cfg: serde_json::Value =
        serde_json::from_str(&config_json).context("parsing base image config")?;
    let mut env = Vec::new();
    if let Some(list) = cfg.pointer("/config/Env").and_then(|v| v.as_array()) {
        for item in list {
            if let Some(s) = item.as_str()
                && let Some((k, v)) = s.split_once('=')
            {
                env.push((k.to_string(), v.to_string()));
            }
        }
    }
    Ok(env)
}

/// The linux/amd64 manifest digest of `base_ref` (`sha256:…`), resolving a manifest list to its amd64
/// entry. Cheap (one manifest fetch, no blobs), so it's the cache key a rootfs unpack is stored under.
pub(crate) async fn resolve_platform_digest(base_ref: &str, auth: &RegistryAuth) -> Result<String> {
    let reference: Reference = base_ref
        .parse()
        .with_context(|| format!("parsing base ref {base_ref}"))?;
    let client = amd64_client();
    let (_manifest, digest) = with_retry(
        &format!("resolving the amd64 manifest digest for {base_ref}"),
        &RETRY_DELAYS,
        || async {
            client
                .pull_manifest(&reference, auth)
                .await
                .map_err(anyhow::Error::from)
        },
    )
    .await?;
    Ok(digest)
}

/// Unpack `base_ref`'s linux/amd64 layers into `dest`, stacking them lowest-to-highest and honoring the
/// OCI whiteouts between them, so `dest` ends up as the image's flattened root filesystem. Each layer
/// blob is streamed to a scratch file (never held whole in RAM) then applied. `dest` is created if
/// absent; the caller keys it by [`resolve_platform_digest`] and marks completion.
pub(crate) async fn unpack_rootfs(base_ref: &str, dest: &Path, auth: &RegistryAuth) -> Result<()> {
    let reference: Reference = base_ref
        .parse()
        .with_context(|| format!("parsing base ref {base_ref}"))?;
    let client = amd64_client();
    let (manifest, _digest, _config) = with_retry(
        &format!("pulling base manifest for {base_ref}"),
        &RETRY_DELAYS,
        || async {
            client
                .pull_manifest_and_config(&reference, auth)
                .await
                .map_err(anyhow::Error::from)
        },
    )
    .await?;
    std::fs::create_dir_all(dest).with_context(|| format!("creating rootfs dir {dest:?}"))?;

    for (i, l) in manifest.layers.iter().enumerate() {
        tracing::info!(
            layer = i + 1,
            of = manifest.layers.len(),
            digest = l.digest,
            bytes = l.size,
            "unpacking base layer"
        );
        let scratch =
            std::env::temp_dir().join(format!("forge-rootfs-{}-{}.tar.gz", std::process::id(), i));
        with_retry(
            &format!(
                "downloading base layer {} ({}/{})",
                l.digest,
                i + 1,
                manifest.layers.len()
            ),
            &RETRY_DELAYS,
            || async {
                let sized = client
                    .pull_blob_stream(&reference, l)
                    .await
                    .context("opening the base layer blob stream")?;
                let mut file = tokio::fs::File::create(&scratch)
                    .await
                    .with_context(|| format!("creating scratch {scratch:?}"))?;
                let mut stream = std::pin::pin!(sized.stream);
                while let Some(chunk) =
                    stream.try_next().await.context("streaming a layer chunk")?
                {
                    file.write_all(&chunk)
                        .await
                        .context("writing a layer chunk")?;
                }
                file.flush().await.context("flushing the scratch layer")?;
                Ok(())
            },
        )
        .await?;

        let f = std::fs::File::open(&scratch)
            .with_context(|| format!("opening scratch {scratch:?}"))?;
        let gz = flate2::read::GzDecoder::new(std::io::BufReader::new(f));
        crate::layer::apply_layer_reader(gz, dest)
            .with_context(|| format!("applying base layer {}", l.digest))?;
        let _ = std::fs::remove_file(&scratch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn overlay_layer_contains_the_file_and_is_deterministic() {
        let files = vec![OverlayFile {
            path: "usr/local/lib/python3.12/dist-packages/app/metrics.py".into(),
            contents: b"# edited by the agent\n".to_vec(),
        }];
        let a = build_overlay_layer(&files).unwrap();
        let b = build_overlay_layer(&files).unwrap();
        // Deterministic (mtime zeroed): same edits => same blob digest.
        assert_eq!(a.blob_digest, b.blob_digest);
        assert!(a.blob_digest.starts_with("sha256:"));
        // The compressed-blob digest and the uncompressed diff_id are necessarily different values.
        assert_ne!(a.blob_digest, a.diff_id);
        assert!(a.diff_id.starts_with("sha256:"));
        assert_eq!(a.size as usize, a.gzip.len());

        // The gzip really untars to the file at its in-image path with the right bytes.
        let dec = flate2::read::GzDecoder::new(a.gzip.as_slice());
        let mut ar = tar::Archive::new(dec);
        let mut found = None;
        for entry in ar.entries().unwrap() {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            if path == "usr/local/lib/python3.12/dist-packages/app/metrics.py" {
                let mut s = String::new();
                entry.read_to_string(&mut s).unwrap();
                found = Some(s);
            }
        }
        assert_eq!(found.as_deref(), Some("# edited by the agent\n"));
    }

    #[test]
    fn rewrite_config_appends_the_diff_id_and_a_history_entry() {
        let base = r#"{"architecture":"amd64","os":"linux",
            "rootfs":{"type":"layers","diff_ids":["sha256:aaa","sha256:bbb"]},
            "history":[{"created_by":"base layer"}]}"#;
        let (bytes, digest, size) = rewrite_config(base, "sha256:ccc", "test").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let diffs = v["rootfs"]["diff_ids"].as_array().unwrap();
        assert_eq!(diffs.len(), 3);
        assert_eq!(diffs[2], "sha256:ccc");
        assert_eq!(v["history"].as_array().unwrap().len(), 2);
        assert!(digest.starts_with("sha256:"));
        assert_eq!(size as usize, bytes.len());
    }

    #[test]
    fn rewrite_config_errors_on_a_config_without_diff_ids() {
        assert!(rewrite_config(r#"{"rootfs":{"type":"layers"}}"#, "sha256:x", "c").is_err());
    }

    #[test]
    fn auth_from_missing_authfile_is_anonymous() {
        let a = auth_from_authfile(Path::new("/no/such/authfile.json"), "quay.io").unwrap();
        assert!(matches!(a, RegistryAuth::Anonymous));
    }

    /// Write `contents` to a unique temp authfile; the caller keeps the returned guard alive.
    fn temp_authfile(contents: &str) -> (PathBuf, TempFile) {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "oci-authfile-test-{}-{}.json",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&p, contents).unwrap();
        (p.clone(), TempFile(p))
    }
    struct TempFile(PathBuf);
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn auth_from_authfile_reads_the_plain_registry_key() {
        // "user:pass" -> dXNlcjpwYXNz (padded, the docker-written form).
        let (p, _g) = temp_authfile(r#"{"auths":{"quay.io":{"auth":"dXNlcjpwYXNz"}}}"#);
        let a = auth_from_authfile(&p, "quay.io").unwrap();
        assert!(matches!(a, RegistryAuth::Basic(u, pw) if u == "user" && pw == "pass"));
        // A registry with no entry stays anonymous.
        let a = auth_from_authfile(&p, "ghcr.io").unwrap();
        assert!(matches!(a, RegistryAuth::Anonymous));
    }

    #[test]
    fn auth_from_authfile_finds_docker_io_under_the_legacy_key() {
        // `docker login` writes the legacy v1 URL; a docker.io ref must still find it.
        let (p, _g) =
            temp_authfile(r#"{"auths":{"https://index.docker.io/v1/":{"auth":"dXNlcjpwYXNz"}}}"#);
        let a = auth_from_authfile(&p, "docker.io").unwrap();
        assert!(matches!(a, RegistryAuth::Basic(u, pw) if u == "user" && pw == "pass"));
    }

    #[test]
    fn auth_from_authfile_takes_explicit_username_password_fields() {
        let (p, _g) =
            temp_authfile(r#"{"auths":{"quay.io":{"username":"bob","password":"s3cret"}}}"#);
        let a = auth_from_authfile(&p, "quay.io").unwrap();
        assert!(matches!(a, RegistryAuth::Basic(u, pw) if u == "bob" && pw == "s3cret"));
    }

    #[test]
    fn auth_token_decodes_without_padding() {
        // "user:pass" unpadded would be rejected by a canonical-padding decoder; some writers omit it.
        let (p, _g) = temp_authfile(r#"{"auths":{"quay.io":{"auth":"dXNlcjpwYXNz"}}}"#);
        let padded = auth_from_authfile(&p, "quay.io").unwrap();
        // "user:passX" -> dXNlcjpwYXNzWA (unpadded: canonical would be ...WA==).
        let (p2, _g2) = temp_authfile(r#"{"auths":{"quay.io":{"auth":"dXNlcjpwYXNzWA"}}}"#);
        let unpadded = auth_from_authfile(&p2, "quay.io").unwrap();
        assert!(matches!(padded, RegistryAuth::Basic(u, pw) if u == "user" && pw == "pass"));
        assert!(matches!(unpadded, RegistryAuth::Basic(u, pw) if u == "user" && pw == "passX"));
    }

    #[tokio::test]
    async fn with_retry_recovers_from_transient_failures_then_gives_up() {
        use std::sync::atomic::{AtomicU32, Ordering};
        // Fails once, then succeeds: the retry absorbs the blip.
        let calls = AtomicU32::new(0);
        let v = with_retry("flaky op", &[Duration::ZERO, Duration::ZERO], || async {
            match calls.fetch_add(1, Ordering::SeqCst) {
                0 => Err(anyhow::anyhow!("transient")),
                n => Ok(n),
            }
        })
        .await
        .unwrap();
        assert_eq!(v, 1, "second attempt's value");
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // Always fails: gives up after 1 + delays.len() attempts with the attempt count in the error.
        let calls = AtomicU32::new(0);
        let err = with_retry("doomed op", &[Duration::ZERO, Duration::ZERO], || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(anyhow::anyhow!("permanent"))
        })
        .await
        .unwrap_err();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(err.to_string().contains("after 3 attempts"), "{err:#}");
    }

    #[test]
    fn tag_of_parses_per_spec() {
        assert_eq!(tag_of("quay.io/acme/candidate:abc"), "abc");
        // A digest ref carries no tag -> latest; a bare repo -> latest.
        let pinned = format!("quay.io/acme/candidate@sha256:{}", "a".repeat(64));
        assert_eq!(tag_of(&pinned), "latest");
        assert_eq!(tag_of("quay.io/acme/candidate"), "latest");
    }

    #[test]
    fn registry_of_parses_per_spec() {
        assert_eq!(registry_of("quay.io/acme/img:m4").unwrap(), "quay.io");
        let pinned = format!("quay.io/acme/img@sha256:{}", "a".repeat(64));
        assert_eq!(registry_of(&pinned).unwrap(), "quay.io");
        assert_eq!(
            registry_of("registry:5000/ns/img:tag").unwrap(),
            "registry:5000"
        );
        // Docker Hub shorthand resolves to docker.io.
        assert_eq!(registry_of("library/ubuntu:24.04").unwrap(), "docker.io");
        assert_eq!(registry_of("ubuntu").unwrap(), "docker.io");
    }

    #[test]
    fn pin_digest_short_circuits_an_already_pinned_ref() {
        // No registry, no creds: a digest ref must round-trip without any network I/O
        // (the host resolves only if contacted, and nothing serves on this name).
        let pinned = "registry.invalid/team/app@sha256:9ff216e487ce3574b35003fc4cb98ba307d22cf5ae3c9066d36ebcb6f153bd5b";
        assert_eq!(pin_digest(pinned, None).unwrap(), pinned);
    }
}
