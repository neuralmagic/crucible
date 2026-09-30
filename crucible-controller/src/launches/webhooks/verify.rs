//! Webhook secrets and delivery verification (RFC-0003:C-WEBHOOK-DELIVERY).

use crate::identity::api_key::constant_time_eq;
use crate::identity::oidc::credentials::CredentialKeys;
use crate::wire_enum::wire_enum;
use aws_lc_rs::hmac;
use axum::http::HeaderMap;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use jiff::Timestamp;
use sha2::{Digest, Sha256};

/// How far a Standard Webhooks timestamp may sit from the controller's clock.
pub(crate) const TIMESTAMP_TOLERANCE_SECS: i64 = 300;

/// Random bytes in every minted secret.
const SECRET_BYTES: usize = 32;

const STANDARD_WEBHOOKS_PREFIX: &str = "whsec_";

/// The key [`verify`] MACs under when it has no webhook secret.
const DUMMY_KEY: &[u8] = b"crucible-webhook-verification-with-no-webhook-behind-it";

/// Which proof a webhook asks of a delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum VerifierKind {
    /// A shared token as the last delivery path segment (quay.io).
    PathToken,
    /// A shared token in a named header (GitLab's `X-Gitlab-Token`).
    HeaderToken,
    /// `sha256=` and the lowercase hex HMAC-SHA256 of the body in a named header (GitHub's
    /// `X-Hub-Signature-256`).
    HmacSha256,
    /// A Standard Webhooks v1 signature over the delivery id, timestamp, and body.
    StandardWebhooks,
}

wire_enum!(VerifierKind, "webhook verifier", both, {
    VerifierKind::PathToken => "path_token",
    VerifierKind::HeaderToken => "header_token",
    VerifierKind::HmacSha256 => "hmac_sha256",
    VerifierKind::StandardWebhooks => "standard_webhooks",
});

impl VerifierKind {
    /// Whether the kind reads its proof from a named header.
    pub(crate) fn reads_header(self) -> bool {
        matches!(self, VerifierKind::HeaderToken | VerifierKind::HmacSha256)
    }

    /// Whether the controller must hold the secret itself (to MAC with it) rather than a digest.
    pub(crate) fn needs_plaintext(self) -> bool {
        matches!(
            self,
            VerifierKind::HmacSha256 | VerifierKind::StandardWebhooks
        )
    }
}

/// A webhook's verifier: its kind and, for the kinds that read one, the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verifier {
    pub kind: VerifierKind,
    /// Lowercase header name.
    pub header: Option<String>,
}

/// What the controller checks a delivery against.
pub(crate) enum Material {
    /// The SHA-256 digest of a token, lowercase hex.
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
        "the {0} verifier needs a credential key to keep its secret, and this controller has none mounted"
    )]
    NoCredentialKey(&'static str),
    #[error("the system random source refused to mint a webhook secret")]
    Random,
    #[error("sealing a webhook secret: {0:#}")]
    Seal(anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OpenError {
    #[error("webhook {0} has a sealed secret and no credential key is mounted")]
    NoCredentialKey(String),
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
    let shown = match kind {
        VerifierKind::StandardWebhooks => {
            format!("{STANDARD_WEBHOOKS_PREFIX}{}", STANDARD.encode(bytes))
        }
        _ => URL_SAFE_NO_PAD.encode(bytes),
    };
    let stored = if kind.needs_plaintext() {
        let keys = keys.ok_or(MintError::NoCredentialKey(kind.as_str()))?;
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
            let keys = keys.ok_or_else(|| OpenError::NoCredentialKey(id.to_string()))?;
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
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
    pub now: Timestamp,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// The Standard Webhooks content a signature covers, or `None` when the id or timestamp header is
/// missing.
fn standard_signed_content(p: &Presented<'_>) -> Option<(i64, Vec<u8>)> {
    let id = header(p.headers, "webhook-id")?;
    let ts_raw = header(p.headers, "webhook-timestamp")?;
    let ts: i64 = ts_raw.parse().ok()?;
    let mut content = Vec::with_capacity(id.len() + ts_raw.len() + p.body.len() + 2);
    content.extend_from_slice(id.as_bytes());
    content.push(b'.');
    content.extend_from_slice(ts_raw.as_bytes());
    content.push(b'.');
    content.extend_from_slice(p.body);
    Some((ts, content))
}

/// Whether `p` proves itself to a webhook with this verifier and material. Does the same work,
/// and returns false, when either is `None`.
pub(crate) fn verify(
    verifier: Option<&Verifier>,
    material: Option<&Material>,
    p: &Presented<'_>,
) -> bool {
    let kind = verifier.map(|v| v.kind);
    let named = verifier
        .and_then(|v| v.header.as_deref())
        .and_then(|name| header(p.headers, name));
    let presented_token = match kind {
        Some(VerifierKind::PathToken) => p.path_token,
        Some(VerifierKind::HeaderToken) => named,
        _ => None,
    };
    let presented_digest = token_digest(presented_token.unwrap_or_default());

    let standard = matches!(kind, Some(VerifierKind::StandardWebhooks))
        .then(|| standard_signed_content(p))
        .flatten();
    let key_bytes: Vec<u8> = match (kind, material) {
        (Some(VerifierKind::HmacSha256), Some(Material::Secret(secret))) => {
            secret.as_bytes().to_vec()
        }
        (Some(VerifierKind::StandardWebhooks), Some(Material::Secret(secret))) => secret
            .strip_prefix(STANDARD_WEBHOOKS_PREFIX)
            .and_then(|b64| STANDARD.decode(b64).ok())
            .unwrap_or_else(|| DUMMY_KEY.to_vec()),
        _ => DUMMY_KEY.to_vec(),
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, &key_bytes);
    let signed = standard.as_ref().map_or(p.body, |(_, content)| content);
    let tag = hmac::sign(&key, signed);

    let addressed_right = p.path_token.is_some() == matches!(kind, Some(VerifierKind::PathToken));
    let proven = match (kind, material) {
        (
            Some(VerifierKind::PathToken | VerifierKind::HeaderToken),
            Some(Material::TokenDigest(stored)),
        ) => {
            presented_token.is_some_and(|t| !t.is_empty())
                && constant_time_eq(presented_digest.as_bytes(), stored.as_bytes())
        }
        (Some(VerifierKind::HmacSha256), Some(Material::Secret(_))) => named
            .and_then(|v| v.strip_prefix("sha256="))
            .is_some_and(|hex| {
                constant_time_eq(hex.as_bytes(), lower_hex(tag.as_ref()).as_bytes())
            }),
        (Some(VerifierKind::StandardWebhooks), Some(Material::Secret(_))) => match &standard {
            Some((ts, _)) if (p.now.as_second() - ts).abs() <= TIMESTAMP_TOLERANCE_SECS => {
                let expected = STANDARD.encode(tag.as_ref());
                header(p.headers, "webhook-signature").is_some_and(|sigs| {
                    sigs.split(' ')
                        .filter_map(|s| s.strip_prefix("v1,"))
                        .fold(false, |hit, sig| {
                            constant_time_eq(sig.as_bytes(), expected.as_bytes()) | hit
                        })
                })
            }
            _ => false,
        },
        _ => false,
    };
    addressed_right && proven
}

/// The text headers a delivery is recorded with, minus every one that can carry a credential:
/// the verifier's header, every header a past verifier read, and the ones any sender signs with.
pub(crate) fn recorded_headers(
    verifier: &Verifier,
    withheld: &[String],
    headers: &HeaderMap,
) -> serde_json::Map<String, serde_json::Value> {
    const ALWAYS_WITHHELD: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "cookie",
        "webhook-signature",
        "x-hub-signature",
        "x-hub-signature-256",
    ];
    let mut out = serde_json::Map::new();
    for name in headers.keys() {
        let lower = name.as_str();
        if ALWAYS_WITHHELD.contains(&lower)
            || verifier.header.as_deref() == Some(lower)
            || withheld.iter().any(|w| w == lower)
        {
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
        Material, Presented, Stored, TIMESTAMP_TOLERANCE_SECS, Verifier, VerifierKind, material,
        mint, recorded_headers, verify,
    };
    use aws_lc_rs::hmac;
    use axum::http::{HeaderMap, HeaderValue};
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use jiff::Timestamp;

    const BODY: &[u8] = br#"{"repository":"org/img","updated_tags":["latest"]}"#;

    fn keys() -> CredentialKeys {
        CredentialKeys::new(vec![vec![7u8; 32]]).expect("keys")
    }

    fn verifier(kind: VerifierKind, header: Option<&str>) -> Verifier {
        Verifier {
            kind,
            header: header.map(str::to_string),
        }
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
            now: Timestamp::from_second(1_760_000_000).expect("now"),
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

    fn standard_headers(secret: &str, id: &str, ts: i64, body: &[u8]) -> HeaderMap {
        let key_bytes = STANDARD
            .decode(secret.strip_prefix("whsec_").expect("prefix"))
            .expect("b64");
        let key = hmac::Key::new(hmac::HMAC_SHA256, &key_bytes);
        let mut content = format!("{id}.{ts}.").into_bytes();
        content.extend_from_slice(body);
        let sig = STANDARD.encode(hmac::sign(&key, &content).as_ref());
        let mut headers = HeaderMap::new();
        headers.insert("webhook-id", HeaderValue::from_str(id).expect("id"));
        headers.insert(
            "webhook-timestamp",
            HeaderValue::from_str(&ts.to_string()).expect("ts"),
        );
        headers.insert(
            "webhook-signature",
            HeaderValue::from_str(&format!("v1,bm90IHRoaXMgb25l v1,{sig}")).expect("sig"),
        );
        headers
    }

    #[test]
    fn minted_secrets_carry_at_least_128_bits_and_tokens_are_kept_as_digests() {
        let keys = keys();
        let token = mint(VerifierKind::PathToken, "w1", None).expect("token needs no key");
        assert!(token.shown.len() >= 43, "{}", token.shown);
        assert!(matches!(token.stored, Stored::Digest(ref d) if d.len() == 64));
        let other = mint(VerifierKind::PathToken, "w1", None).expect("mint");
        assert_ne!(token.shown, other.shown);

        let standard = mint(VerifierKind::StandardWebhooks, "w1", Some(&keys)).expect("mint");
        assert!(standard.shown.starts_with("whsec_"));
        assert!(matches!(standard.stored, Stored::Sealed { .. }));
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
        let v = verifier(VerifierKind::PathToken, None);
        let headers = HeaderMap::new();
        assert!(verify(
            Some(&v),
            Some(&material),
            &presented(Some(&token), &headers, BODY)
        ));
        assert!(!verify(
            Some(&v),
            Some(&material),
            &presented(Some("wrong"), &headers, BODY)
        ));
        assert!(!verify(
            Some(&v),
            Some(&material),
            &presented(Some(""), &headers, BODY)
        ));
        assert!(!verify(
            Some(&v),
            Some(&material),
            &presented(None, &headers, BODY)
        ));
    }

    #[test]
    fn a_header_token_proves_from_its_named_header() {
        let (token, material) = opened(VerifierKind::HeaderToken, "w1", &keys());
        let v = verifier(VerifierKind::HeaderToken, Some("x-gitlab-token"));
        let mut headers = HeaderMap::new();
        headers.insert("x-gitlab-token", HeaderValue::from_str(&token).expect("hv"));
        assert!(verify(
            Some(&v),
            Some(&material),
            &presented(None, &headers, BODY)
        ));
        assert!(
            !verify(
                Some(&v),
                Some(&material),
                &presented(Some(&token), &headers, BODY)
            ),
            "a header-token webhook is never addressed with a path token"
        );
        headers.insert("x-gitlab-token", HeaderValue::from_static("wrong"));
        assert!(!verify(
            Some(&v),
            Some(&material),
            &presented(None, &headers, BODY)
        ));
        assert!(!verify(
            Some(&v),
            Some(&material),
            &presented(None, &HeaderMap::new(), BODY)
        ));
    }

    #[test]
    fn an_hmac_signature_proves_the_exact_body() {
        let (secret, material) = opened(VerifierKind::HmacSha256, "w1", &keys());
        let v = verifier(VerifierKind::HmacSha256, Some("x-hub-signature-256"));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&github_signature(&secret, BODY)).expect("hv"),
        );
        assert!(verify(
            Some(&v),
            Some(&material),
            &presented(None, &headers, BODY)
        ));
        assert!(!verify(
            Some(&v),
            Some(&material),
            &presented(None, &headers, b"{}")
        ));
        let upper = github_signature(&secret, BODY)
            .to_uppercase()
            .replace("SHA256=", "sha256=");
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&upper).expect("hv"),
        );
        assert!(
            !verify(Some(&v), Some(&material), &presented(None, &headers, BODY)),
            "the digest is lowercase hex"
        );
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&github_signature("another secret", BODY)).expect("hv"),
        );
        assert!(!verify(
            Some(&v),
            Some(&material),
            &presented(None, &headers, BODY)
        ));
    }

    #[test]
    fn a_standard_webhooks_signature_proves_within_the_tolerance() {
        let (secret, material) = opened(VerifierKind::StandardWebhooks, "w1", &keys());
        let v = verifier(VerifierKind::StandardWebhooks, None);
        let now = 1_760_000_000;
        let fresh = standard_headers(&secret, "msg_1", now - 10, BODY);
        assert!(verify(
            Some(&v),
            Some(&material),
            &presented(None, &fresh, BODY)
        ));
        assert!(!verify(
            Some(&v),
            Some(&material),
            &presented(None, &fresh, b"{}")
        ));
        for ts in [
            now - TIMESTAMP_TOLERANCE_SECS - 1,
            now + TIMESTAMP_TOLERANCE_SECS + 1,
        ] {
            let stale = standard_headers(&secret, "msg_1", ts, BODY);
            assert!(
                !verify(Some(&v), Some(&material), &presented(None, &stale, BODY)),
                "timestamp {ts} is outside the tolerance"
            );
        }
        let mut forged = fresh.clone();
        forged.insert("webhook-id", HeaderValue::from_static("msg_2"));
        assert!(
            !verify(Some(&v), Some(&material), &presented(None, &forged, BODY)),
            "the signature covers the delivery id"
        );
    }

    #[test]
    fn no_webhook_and_a_mismatched_material_never_prove() {
        let headers = HeaderMap::new();
        for kind in [
            VerifierKind::PathToken,
            VerifierKind::HeaderToken,
            VerifierKind::HmacSha256,
            VerifierKind::StandardWebhooks,
        ] {
            let v = verifier(kind, kind.reads_header().then_some("x-token"));
            assert!(!verify(
                Some(&v),
                None,
                &presented(Some("t"), &headers, BODY)
            ));
            assert!(!verify(None, None, &presented(Some("t"), &headers, BODY)));
        }
        let (_, token_material) = opened(VerifierKind::PathToken, "w1", &keys());
        let v = verifier(VerifierKind::HmacSha256, Some("x-hub-signature-256"));
        assert!(!verify(
            Some(&v),
            Some(&token_material),
            &presented(None, &headers, BODY)
        ));
    }

    #[test]
    fn recorded_headers_withhold_every_credential() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("content-type", "application/json"),
            ("x-gitlab-event", "Push Hook"),
            ("x-gitlab-token", "secret-token"),
            ("x-hub-signature-256", "sha256=abc"),
            ("webhook-signature", "v1,abc"),
            ("authorization", "Bearer x"),
            ("cookie", "a=b"),
        ] {
            headers.insert(name, HeaderValue::from_static(value));
        }
        let recorded = recorded_headers(
            &verifier(VerifierKind::HeaderToken, Some("x-gitlab-token")),
            &[],
            &headers,
        );
        let mut names: Vec<&str> = recorded.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["content-type", "x-gitlab-event"]);
        let moved = recorded_headers(
            &verifier(VerifierKind::PathToken, None),
            &["x-gitlab-token".to_string()],
            &headers,
        );
        assert!(
            !moved.contains_key("x-gitlab-token"),
            "a header a past verifier read is never recorded"
        );
    }
}
