//! The key set `hmac_sha256` webhook secrets are sealed under: AES-256-GCM, chart-mounted, newest
//! key first so a rotation is "prepend the new key". The webhook's seal subject is the additional
//! data, so a sealed row cannot be replayed under another webhook.

#![allow(clippy::disallowed_macros)]

use anyhow::Context;
use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use base64::Engine;
use std::sync::Arc;

/// AES-256 key material length.
const KEY_LEN: usize = 32;

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// One mounted key: its material and the id a sealed row records so a later open knows which key
/// to try.
struct Key {
    id: String,
    key: LessSafeKey,
}

/// The chart-mounted key set. The first key seals; any key may open, so a rotation is "prepend the
/// new key" rather than "re-create every webhook".
pub struct WebhookKeys {
    keys: Vec<Key>,
}

impl std::fmt::Debug for WebhookKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookKeys")
            .field(
                "key_ids",
                &self.keys.iter().map(|k| &k.id).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// The id a key is recorded under: a truncated digest of the material, so a key never has to be
/// named by hand and two deployments cannot disagree about which id means which bytes.
fn key_id(material: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(material);
    digest
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

impl WebhookKeys {
    /// Build from raw key material, newest first.
    pub fn new(materials: Vec<Vec<u8>>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !materials.is_empty(),
            "a webhook key set needs at least one key"
        );
        let mut keys = Vec::with_capacity(materials.len());
        for material in materials {
            anyhow::ensure!(
                material.len() == KEY_LEN,
                "a webhook key must be {KEY_LEN} bytes, got {}",
                material.len()
            );
            let unbound = UnboundKey::new(&AES_256_GCM, &material)
                .map_err(|_| anyhow::anyhow!("the webhook key was refused by AES-256-GCM"))?;
            keys.push(Key {
                id: key_id(&material),
                key: LessSafeKey::new(unbound),
            });
        }
        Ok(WebhookKeys { keys })
    }

    /// Read the mounted key set: `CONTROLLER_WEBHOOK_KEY_FILE` (a mounted file, one base64 key per
    /// non-empty line, newest first) or `CONTROLLER_WEBHOOK_KEY` (the same, comma- or
    /// whitespace-separated). `None` refuses `hmac_sha256` webhooks.
    pub fn from_env() -> anyhow::Result<Option<Arc<Self>>> {
        Self::from_sources(
            env_value("CONTROLLER_WEBHOOK_KEY_FILE"),
            env_value("CONTROLLER_WEBHOOK_KEY"),
        )
    }

    /// Keys from a key file path or inline material: base64 keys, newest first, one per line or
    /// separated by commas or whitespace. The file wins. `None` when neither is given.
    pub fn from_sources(
        file: Option<String>,
        inline: Option<String>,
    ) -> anyhow::Result<Option<Arc<Self>>> {
        let raw = match (file, inline) {
            (Some(path), _) => std::fs::read_to_string(&path)
                .with_context(|| format!("reading the webhook key file {path}"))?,
            (None, Some(inline)) => inline,
            (None, None) => return Ok(None),
        };
        let materials = raw
            .split(['\n', '\r', ',', ' ', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| b64().decode(s).context("a webhook key is not valid base64"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(
            !materials.is_empty(),
            "the configured webhook key material is empty"
        );
        WebhookKeys::new(materials).map(|k| Some(Arc::new(k)))
    }

    /// Seal a secret under the newest key, bound to `sub`. The stored form is
    /// `base64(nonce || ciphertext || tag)`.
    pub(crate) fn seal(&self, sub: &str, plaintext: &str) -> anyhow::Result<(String, String)> {
        let key = self
            .keys
            .first()
            .ok_or_else(|| anyhow::anyhow!("no webhook key is mounted"))?;
        let mut nonce_bytes = [0u8; NONCE_LEN];
        aws_lc_rs::rand::fill(&mut nonce_bytes)
            .map_err(|_| anyhow::anyhow!("the system random source refused a nonce"))?;
        let mut sealed = plaintext.as_bytes().to_vec();
        key.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(sub.as_bytes()),
                &mut sealed,
            )
            .map_err(|_| anyhow::anyhow!("sealing the webhook secret failed"))?;
        let mut framed = nonce_bytes.to_vec();
        framed.extend_from_slice(&sealed);
        Ok((b64().encode(framed), key.id.clone()))
    }

    /// Open a sealed secret. `key_id` picks the key; a row sealed under a key this deployment no
    /// longer mounts cannot be opened.
    pub(crate) fn open(&self, sub: &str, key_id: &str, sealed: &str) -> anyhow::Result<String> {
        let key = self
            .keys
            .iter()
            .find(|k| k.id == key_id)
            .ok_or_else(|| anyhow::anyhow!("no mounted webhook key has id {key_id}"))?;
        let framed = b64()
            .decode(sealed)
            .context("the sealed secret is not valid base64")?;
        anyhow::ensure!(
            framed.len() > NONCE_LEN,
            "the sealed secret is too short to carry a nonce"
        );
        let (nonce_bytes, body) = framed.split_at(NONCE_LEN);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(nonce_bytes);
        let mut body = body.to_vec();
        let opened = key
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(sub.as_bytes()),
                &mut body,
            )
            .map_err(|_| anyhow::anyhow!("the sealed secret did not authenticate"))?;
        String::from_utf8(opened.to_vec()).context("the sealed secret is not utf-8")
    }
}

/// A set, non-blank environment value, trimmed.
fn env_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use crate::launches::webhooks::keys::*;

    fn keys(count: usize) -> WebhookKeys {
        let materials = (0..count)
            .map(|n| vec![n as u8 + 1; KEY_LEN])
            .collect::<Vec<_>>();
        WebhookKeys::new(materials).expect("keys")
    }

    #[test]
    fn a_sealed_secret_opens_only_under_its_own_subject() {
        let keys = keys(1);
        let (sealed, id) = keys.seal("sub-alice", "webhook-secret").expect("seal");
        assert_eq!(
            keys.open("sub-alice", &id, &sealed).expect("open"),
            "webhook-secret"
        );
        assert!(
            keys.open("sub-mallory", &id, &sealed).is_err(),
            "the subject is additional data, so another subject's row must not open"
        );
    }

    #[test]
    fn two_seals_of_one_secret_differ() {
        let keys = keys(1);
        let (a, _) = keys.seal("sub", "same").expect("seal");
        let (b, _) = keys.seal("sub", "same").expect("seal");
        assert_ne!(a, b, "a fresh nonce per seal, or GCM is broken");
    }

    /// Rotation: the newest key seals, every mounted key still opens, and a key that is no longer
    /// mounted opens nothing.
    #[test]
    fn an_older_key_still_opens_and_a_dropped_one_does_not() {
        let old = WebhookKeys::new(vec![vec![2u8; KEY_LEN]]).expect("old");
        let (sealed, id) = old.seal("sub", "token").expect("seal");

        let rotated =
            WebhookKeys::new(vec![vec![9u8; KEY_LEN], vec![2u8; KEY_LEN]]).expect("rotated");
        assert_eq!(rotated.open("sub", &id, &sealed).expect("open"), "token");
        let (fresh, fresh_id) = rotated.seal("sub", "token").expect("seal");
        assert_ne!(fresh_id, id, "the newest key seals");

        let dropped = WebhookKeys::new(vec![vec![9u8; KEY_LEN]]).expect("dropped");
        assert!(dropped.open("sub", &id, &sealed).is_err());
        assert!(dropped.open("sub", &fresh_id, &fresh).is_ok());
    }

    #[test]
    fn a_tampered_ciphertext_never_opens() {
        let keys = keys(1);
        let (sealed, id) = keys.seal("sub", "token").expect("seal");
        let mut raw = b64().decode(&sealed).expect("decode");
        let last = raw.len() - 1;
        raw[last] ^= 0xff;
        assert!(keys.open("sub", &id, &b64().encode(raw)).is_err());
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused() {
        assert!(WebhookKeys::new(vec![vec![0u8; 16]]).is_err());
        assert!(WebhookKeys::new(vec![]).is_err());
    }

    #[test]
    fn keys_load_from_a_file_or_inline_and_the_file_wins() {
        use base64::Engine;
        let a = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);
        let b = base64::engine::general_purpose::STANDARD.encode([2u8; 32]);
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("webhook.key");
        std::fs::write(&file, format!("{a}\n\n{b}\n")).expect("write");
        let path = file.display().to_string();

        let from_file = crate::launches::webhooks::keys::WebhookKeys::from_sources(
            Some(path),
            Some("not base64!".to_string()),
        )
        .expect("file keys")
        .expect("some");
        let (sealed, _) = from_file.seal("webhook:w1", "s3cret").expect("seal");
        let inline = crate::launches::webhooks::keys::WebhookKeys::from_sources(
            None,
            Some(format!("{a}, {b}")),
        )
        .expect("inline keys")
        .expect("some");
        let key_id = from_file.seal("webhook:w1", "x").expect("seal").1;
        assert_eq!(
            inline.open("webhook:w1", &key_id, &sealed).expect("open"),
            "s3cret",
            "the newest key seals, and the same material opens it however it was given"
        );
        assert!(
            crate::launches::webhooks::keys::WebhookKeys::from_sources(None, None)
                .expect("none")
                .is_none()
        );
        let refused = crate::launches::webhooks::keys::WebhookKeys::from_sources(
            None,
            Some("not base64!".to_string()),
        )
        .map(|_| ())
        .expect_err("refused");
        assert!(refused.to_string().contains("webhook key"), "{refused}");
    }
}
