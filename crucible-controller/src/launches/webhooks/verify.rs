//! Webhook secrets and delivery verification (RFC-0003:C-WEBHOOK-DELIVERY).

use crate::identity::api_key::constant_time_eq;
use crate::identity::oidc::credentials::CredentialKeys;
use crate::wire_enum::wire_enum;
use aws_lc_rs::hmac;
use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Random bytes in every minted secret.
const SECRET_BYTES: usize = 32;

/// Which proof a webhook asks of a delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum VerifierKind {
    /// A shared token as the last delivery path segment (quay.io).
    PathToken,
    /// `sha256=` and the lowercase hex HMAC-SHA256 of the body in a named header (GitHub's
    /// `X-Hub-Signature-256`).
    HmacSha256,
}

wire_enum!(VerifierKind, "webhook verifier", both, {
    VerifierKind::PathToken => "path_token",
    VerifierKind::HmacSha256 => "hmac_sha256",
});

impl VerifierKind {
    /// Whether the kind reads a signature from a named header, which means the controller keeps
    /// the secret sealed rather than as a digest.
    pub(crate) fn signs(self) -> bool {
        self == VerifierKind::HmacSha256
    }
}

/// A webhook's verifier: its kind and, for `hmac_sha256`, the header the signature is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verifier {
    pub kind: VerifierKind,
    /// Lowercase header name.
    pub header: Option<String>,
}

/// What the controller checks a delivery against.
pub(crate) enum Material {
    /// The SHA-256 digest of a path token, lowercase hex.
    TokenDigest(String),
    /// An HMAC secret, opened from its sealed form.
    Secret(String),
}

/// How a minted secret is kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stored {
    Digest(String),
    Sealed { sealed: String, key_id: String },
}

/// A freshly minted secret: the text the sender is configured with, shown once, and what the row
/// keeps.
pub(crate) struct Minted {
    pub shown: String,
    pub stored: Stored,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum MintError {
    #[error(
        "the {0} verifier needs a webhook key to keep its secret, and this controller has none (CONTROLLER_WEBHOOK_KEY)"
    )]
    NoWebhookKey(&'static str),
    #[error("the system random source refused to mint a webhook secret")]
    Random,
    #[error("sealing a webhook secret: {0:#}")]
    Seal(anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    #[error("webhook {0} has a sealed secret and no webhook key is mounted")]
    NoWebhookKey(String),
    #[error("opening webhook {id}'s secret: {source:#}")]
    Open { id: String, source: anyhow::Error },
}

/// The additional data a webhook's sealed secret is bound to.
fn seal_subject(webhook_id: &str) -> String {
    format!("webhook:{webhook_id}")
}

/// Mint a secret for webhook `id`.
pub(crate) fn mint(
    kind: VerifierKind,
    id: &str,
    keys: Option<&CredentialKeys>,
) -> Result<Minted, MintError> {
    let mut bytes = [0u8; SECRET_BYTES];
    aws_lc_rs::rand::fill(&mut bytes).map_err(|_| MintError::Random)?;
    let shown = URL_SAFE_NO_PAD.encode(bytes);
    let stored = if kind.signs() {
        let keys = keys.ok_or(MintError::NoWebhookKey(kind.as_str()))?;
        let (sealed, key_id) = keys
            .seal(&seal_subject(id), &shown)
            .map_err(MintError::Seal)?;
        Stored::Sealed { sealed, key_id }
    } else {
        Stored::Digest(token_digest(&shown))
    };
    Ok(Minted { shown, stored })
}

/// Open a stored secret into what [`verify`] checks against.
pub(crate) fn material(
    id: &str,
    stored: &Stored,
    keys: Option<&CredentialKeys>,
) -> Result<Material, OpenError> {
    match stored {
        Stored::Digest(digest) => Ok(Material::TokenDigest(digest.clone())),
        Stored::Sealed { sealed, key_id } => {
            let keys = keys.ok_or_else(|| OpenError::NoWebhookKey(id.to_string()))?;
            keys.open(&seal_subject(id), key_id, sealed)
                .map(Material::Secret)
                .map_err(|source| OpenError::Open {
                    id: id.to_string(),
                    source,
                })
        }
    }
}

pub(crate) fn token_digest(token: &str) -> String {
    lower_hex(&Sha256::digest(token.as_bytes()))
}

fn lower_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What a delivery presented.
pub(crate) struct Presented<'a> {
    /// The segment after the webhook id, when the delivery was addressed with one.
    pub path_token: Option<&'a str>,
    pub headers: &'a HeaderMap,
    pub body: &'a [u8],
}

/// Whether `p` proves itself to a webhook with this verifier and material.
pub(crate) fn verify(verifier: &Verifier, material: &Material, p: &Presented<'_>) -> bool {
    match (verifier.kind, material) {
        (VerifierKind::PathToken, Material::TokenDigest(stored)) => {
            p.path_token.is_some_and(|token| {
                !token.is_empty()
                    && constant_time_eq(token_digest(token).as_bytes(), stored.as_bytes())
            })
        }
        (VerifierKind::HmacSha256, Material::Secret(secret)) if p.path_token.is_none() => {
            let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
            let expected = lower_hex(hmac::sign(&key, p.body).as_ref());
            verifier
                .header
                .as_deref()
                .and_then(|name| p.headers.get(name))
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("sha256="))
                .is_some_and(|hex| constant_time_eq(hex.as_bytes(), expected.as_bytes()))
        }
        _ => false,
    }
}

/// The text headers a delivery is recorded with, minus every one that can carry a credential.
pub(crate) fn recorded_headers(
    verifier: &Verifier,
    headers: &HeaderMap,
) -> serde_json::Map<String, serde_json::Value> {
    const ALWAYS_WITHHELD: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "cookie",
        "x-hub-signature",
        "x-hub-signature-256",
    ];
    let mut out = serde_json::Map::new();
    for name in headers.keys() {
        let lower = name.as_str();
        if ALWAYS_WITHHELD.contains(&lower) || verifier.header.as_deref() == Some(lower) {
            continue;
        }
        let values: Vec<&str> = headers
            .get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .collect();
        if !values.is_empty() {
            out.insert(
                lower.to_string(),
                serde_json::Value::String(values.join(", ")),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use crate::identity::oidc::credentials::CredentialKeys;
    use crate::launches::webhooks::verify::{
        Material, Presented, Stored, Verifier, VerifierKind, material, mint, recorded_headers,
        verify,
    };
    use aws_lc_rs::hmac;
    use axum::http::{HeaderMap, HeaderValue};

    const BODY: &[u8] = br#"{"repository":"org/img","updated_tags":["latest"]}"#;

    fn keys() -> CredentialKeys {
        CredentialKeys::new(vec![vec![7u8; 32]]).expect("keys")
    }

    fn opened(kind: VerifierKind, id: &str, keys: &CredentialKeys) -> (String, Material) {
        let minted = mint(kind, id, Some(keys)).expect("mint");
        let material = material(id, &minted.stored, Some(keys)).expect("open");
        (minted.shown, material)
    }

    fn presented<'a>(
        path_token: Option<&'a str>,
        headers: &'a HeaderMap,
        body: &'a [u8],
    ) -> Presented<'a> {
        Presented {
            path_token,
            headers,
            body,
        }
    }

    fn github_signature(secret: &str, body: &[u8]) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
        let tag = hmac::sign(&key, body);
        format!(
            "sha256={}",
            tag.as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    }

    fn path() -> Verifier {
        Verifier {
            kind: VerifierKind::PathToken,
            header: None,
        }
    }

    fn signed() -> Verifier {
        Verifier {
            kind: VerifierKind::HmacSha256,
            header: Some("x-hub-signature-256".to_string()),
        }
    }

    #[test]
    fn minted_secrets_carry_at_least_128_bits_and_tokens_are_kept_as_digests() {
        let keys = keys();
        let token = mint(VerifierKind::PathToken, "w1", None).expect("token needs no key");
        assert!(token.shown.len() >= 43, "{}", token.shown);
        assert!(matches!(token.stored, Stored::Digest(ref d) if d.len() == 64));
        let other = mint(VerifierKind::PathToken, "w1", None).expect("mint");
        assert_ne!(token.shown, other.shown);

        let sealed = mint(VerifierKind::HmacSha256, "w1", Some(&keys)).expect("mint");
        assert!(matches!(sealed.stored, Stored::Sealed { .. }));
        assert!(
            mint(VerifierKind::HmacSha256, "w1", None).is_err(),
            "an HMAC secret is never kept without a credential key"
        );
    }

    #[test]
    fn a_sealed_secret_opens_only_for_its_own_webhook() {
        let keys = keys();
        let minted = mint(VerifierKind::HmacSha256, "w1", Some(&keys)).expect("mint");
        assert!(material("w1", &minted.stored, Some(&keys)).is_ok());
        assert!(material("w2", &minted.stored, Some(&keys)).is_err());
    }

    #[test]
    fn a_path_token_proves_only_on_the_token_address() {
        let (token, material) = opened(VerifierKind::PathToken, "w1", &keys());
        let headers = HeaderMap::new();
        assert!(verify(
            &path(),
            &material,
            &presented(Some(&token), &headers, BODY)
        ));
        assert!(!verify(
            &path(),
            &material,
            &presented(Some("wrong"), &headers, BODY)
        ));
        assert!(!verify(
            &path(),
            &material,
            &presented(Some(""), &headers, BODY)
        ));
        assert!(!verify(
            &path(),
            &material,
            &presented(None, &headers, BODY)
        ));
    }

    #[test]
    fn an_hmac_signature_proves_the_exact_body() {
        let (secret, material) = opened(VerifierKind::HmacSha256, "w1", &keys());
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&github_signature(&secret, BODY)).expect("hv"),
        );
        assert!(verify(
            &signed(),
            &material,
            &presented(None, &headers, BODY)
        ));
        assert!(!verify(
            &signed(),
            &material,
            &presented(None, &headers, b"{}")
        ));
        assert!(
            !verify(
                &signed(),
                &material,
                &presented(Some(&secret), &headers, BODY)
            ),
            "a signed webhook is never addressed with a path token"
        );
        let upper = github_signature(&secret, BODY)
            .to_uppercase()
            .replace("SHA256=", "sha256=");
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&upper).expect("hv"),
        );
        assert!(
            !verify(&signed(), &material, &presented(None, &headers, BODY)),
            "the digest is lowercase hex"
        );
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&github_signature("another secret", BODY)).expect("hv"),
        );
        assert!(!verify(
            &signed(),
            &material,
            &presented(None, &headers, BODY)
        ));
        assert!(!verify(
            &signed(),
            &material,
            &presented(None, &HeaderMap::new(), BODY)
        ));
    }

    #[test]
    fn a_mismatched_material_never_proves() {
        let keys = keys();
        let (token, token_material) = opened(VerifierKind::PathToken, "w1", &keys);
        let (_, secret_material) = opened(VerifierKind::HmacSha256, "w1", &keys);
        let headers = HeaderMap::new();
        assert!(!verify(
            &signed(),
            &token_material,
            &presented(None, &headers, BODY)
        ));
        assert!(!verify(
            &path(),
            &secret_material,
            &presented(Some(&token), &headers, BODY)
        ));
    }

    #[test]
    fn recorded_headers_withhold_every_credential() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("content-type", "application/json"),
            ("x-github-event", "push"),
            ("x-custom-signature", "sha256=abc"),
            ("x-hub-signature-256", "sha256=abc"),
            ("authorization", "Bearer x"),
            ("cookie", "a=b"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        let recorded = recorded_headers(
            &Verifier {
                kind: VerifierKind::HmacSha256,
                header: Some("x-custom-signature".to_string()),
            },
            &headers,
        );
        let mut names: Vec<&str> = recorded.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["content-type", "x-github-event"]);
    }
}
