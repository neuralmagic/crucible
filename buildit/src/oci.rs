use anyhow::{Context, Result};
use oci_client::Reference;
use oci_client::client::{Client, ClientConfig, Config, ImageLayer};
use oci_client::secrets::RegistryAuth;
use sha2::{Digest, Sha256};

// quay.io/foo:tag -> quay.io/foo
pub fn repo_of(image: &str) -> &str {
    match (image.rfind(':'), image.rfind('/')) {
        (Some(colon), Some(slash)) if colon > slash => &image[..colon],
        (Some(colon), None) => &image[..colon],
        _ => image,
    }
}

// content-addressed: same tree -> same tag -> free dedupe on re-push
pub fn context_reference(image: &str, tar: &[u8]) -> String {
    let hash = hex(&Sha256::digest(tar));
    format!("{}:buildit-ctx-{}", repo_of(image), &hash[..12])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn client() -> Client {
    Client::new(ClientConfig::default())
}

// quay auto-prunes on the quay.expires-after label, which is exactly what
// throwaway context images want. best effort: other registries get no default.
pub fn default_context_labels(registry: &str) -> Vec<(String, String)> {
    if registry == "quay.io" || registry.starts_with("quay.") {
        vec![("quay.expires-after".to_string(), "2w".to_string())]
    } else {
        Vec::new()
    }
}

// push the context tar as a single-layer image so `crane export` in the
// job's initContainer can pull it back out. layer is UNCOMPRESSED so the
// blob digest doubles as the config's diff_id.
pub async fn push_context(
    ctx_ref: &str,
    tar: Vec<u8>,
    auth: &RegistryAuth,
    labels: &[(String, String)],
) -> Result<()> {
    let reference: Reference = ctx_ref.parse().context("parsing context reference")?;
    let client = client();
    // dedupe keys on the tar-derived tag alone; a label change with an
    // unchanged tree keeps the already-pushed image's labels
    if client.fetch_manifest_digest(&reference, auth).await.is_ok() {
        tracing::info!("context {ctx_ref} already in registry, skipping push");
        return Ok(());
    }
    let diff_id = format!("sha256:{}", hex(&Sha256::digest(&tar)));
    let label_map: serde_json::Map<String, serde_json::Value> = labels
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::json!(v)))
        .collect();
    let config_json = serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": { "Labels": label_map },
        "rootfs": { "type": "layers", "diff_ids": [diff_id] }
    });
    let layer = ImageLayer::oci_v1(tar, None);
    let config = Config::oci_v1(serde_json::to_vec(&config_json)?, None);
    client
        .push(&reference, &[layer], config, auth, None)
        .await
        .with_context(|| format!("pushing context image {ctx_ref}"))?;
    tracing::info!("pushed context {ctx_ref}");
    Ok(())
}

// fallback digest resolution when the termination message is gone
pub async fn resolve_digest(image: &str, auth: &RegistryAuth) -> Result<String> {
    let reference: Reference = image.parse().context("parsing image reference")?;
    client()
        .fetch_manifest_digest(&reference, auth)
        .await
        .with_context(|| format!("resolving digest for {image}"))
}

#[cfg(test)]
mod tests {
    use crate::oci::{context_reference, repo_of};

    #[test]
    fn repo_strips_tag_not_port() {
        assert_eq!(repo_of("quay.io/acme/foo:tag"), "quay.io/acme/foo");
        assert_eq!(repo_of("localhost:5000/foo"), "localhost:5000/foo");
        assert_eq!(repo_of("quay.io/acme/foo"), "quay.io/acme/foo");
    }

    #[test]
    fn quay_gets_expiry_default() {
        assert_eq!(
            crate::oci::default_context_labels("quay.io"),
            vec![("quay.expires-after".to_string(), "2w".to_string())]
        );
        assert_eq!(
            crate::oci::default_context_labels("quay.corp.example.com").len(),
            1
        );
        assert!(crate::oci::default_context_labels("ghcr.io").is_empty());
        assert!(crate::oci::default_context_labels("notquay.io").is_empty());
    }

    #[test]
    fn context_ref_is_content_addressed() {
        let a = context_reference("quay.io/acme/foo:v1", b"tree");
        let b = context_reference("quay.io/acme/foo:v2", b"tree");
        let c = context_reference("quay.io/acme/foo:v1", b"other");
        assert_eq!(a, b, "same content, same ctx tag regardless of target tag");
        assert_ne!(a, c);
        assert!(a.starts_with("quay.io/acme/foo:buildit-ctx-"));
        let tag = a.rsplit(':').next().unwrap();
        assert_eq!(tag.len(), "buildit-ctx-".len() + 12);
    }
}
