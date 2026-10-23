//! Signature and registered-claim checks for JWTs from one issuer, against that issuer's JWKS.

use crate::jwks::{Jwks, JwksError};
use crate::principal::ClaimsError;
use jsonwebtoken::{Algorithm, Validation};
use serde_json::{Map, Value};

/// Clock skew tolerated on `exp` and `nbf`.
pub const LEEWAY_SECS: u64 = 30;

/// The signature algorithms a token may use; symmetric and unsigned ones never verify.
pub const ACCEPTED_ALGORITHMS: &[Algorithm] = &[
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("malformed token: {0}")]
    Malformed(jsonwebtoken::errors::Error),
    #[error("token header has no kid")]
    NoKid,
    #[error("algorithm {0:?} is not accepted")]
    Algorithm(Algorithm),
    #[error("token algorithm {token:?} differs from its key's {key:?}")]
    AlgorithmMismatch { token: Algorithm, key: Algorithm },
    #[error("token issuer {0:?} is not trusted")]
    UnknownIssuer(String),
    #[error(transparent)]
    Key(#[from] JwksError),
    #[error("token rejected: {0}")]
    Rejected(jsonwebtoken::errors::Error),
    #[error(transparent)]
    Claims(#[from] ClaimsError),
}

/// Verifies tokens minted by one issuer.
pub struct Verifier {
    issuer: String,
    jwks: Jwks,
}

impl Verifier {
    /// `issuer` is compared to `iss` as an exact string (RFC 8414 §3.3), so no URL normalization.
    pub fn new(issuer: impl Into<String>, jwks: Jwks) -> Self {
        Self {
            issuer: issuer.into(),
            jwks,
        }
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// The claims of `token` once its signature, `iss`, `exp`, `nbf`, and `aud` (naming at least
    /// one of `audiences`) check out. `exp`, `iss`, `aud`, and `sub` must be present.
    pub async fn verify(
        &self,
        token: &str,
        audiences: &[&str],
    ) -> Result<Map<String, Value>, TokenError> {
        let header = jsonwebtoken::decode_header(token).map_err(TokenError::Malformed)?;
        if !ACCEPTED_ALGORITHMS.contains(&header.alg) {
            return Err(TokenError::Algorithm(header.alg));
        }
        let kid = header.kid.ok_or(TokenError::NoKid)?;
        let key = self.jwks.key(&kid).await?;
        if let Some(key_alg) = key.algorithm
            && key_alg != header.alg
        {
            return Err(TokenError::AlgorithmMismatch {
                token: header.alg,
                key: key_alg,
            });
        }
        let mut validation = Validation::new(header.alg);
        validation.leeway = LEEWAY_SECS;
        validation.validate_nbf = true;
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(audiences);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let data = jsonwebtoken::decode::<Map<String, Value>>(token, &key.key, &validation)
            .map_err(TokenError::Rejected)?;
        Ok(data.claims)
    }
}

/// The claims of a compact JWS, read without verifying anything. Only for choosing which
/// [`Verifier`] checks it.
pub fn unverified_claims(token: &str) -> Result<Map<String, Value>, TokenError> {
    Ok(
        jsonwebtoken::dangerous::insecure_decode::<Map<String, Value>>(token)
            .map_err(TokenError::Malformed)?
            .claims,
    )
}

/// The `iss` a compact JWS claims, read without verifying anything. Only for choosing which
/// [`Verifier`] checks it; that verifier then enforces the same `iss` on the verified claims.
pub fn unverified_issuer(token: &str) -> Result<String, TokenError> {
    match unverified_claims(token)?.get("iss") {
        Some(Value::String(iss)) => Ok(iss.clone()),
        _ => Err(TokenError::Claims(ClaimsError::NotString("iss"))),
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{self, JwksServer, jwk, run_claims, sign};
    use crate::verify::*;
    use serde_json::json;

    #[test]
    fn the_unverified_issuer_is_read_from_the_payload() {
        assert_eq!(
            unverified_issuer(&sign("a", &run_claims())).unwrap(),
            testing::ISSUER
        );
        let mut claims = run_claims();
        claims["iss"] = json!(7);
        assert!(matches!(
            unverified_issuer(&sign("a", &claims)),
            Err(TokenError::Claims(ClaimsError::NotString("iss")))
        ));
        assert!(matches!(
            unverified_issuer("not-a-jwt"),
            Err(TokenError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn issuers_compare_as_exact_strings() {
        let jwks = JwksServer::start(vec![jwk("a")]).await;
        let origin = "https://oidc-discovery.example";
        let verifier = Verifier::new(origin, Jwks::new(reqwest::Client::new(), jwks.url.clone()));
        let mut claims = run_claims();
        claims["iss"] = json!(origin);
        verifier
            .verify(&sign("a", &claims), &[testing::RESOURCE_SVC])
            .await
            .unwrap();
        claims["iss"] = json!(format!("{origin}/"));
        let err = verifier
            .verify(&sign("a", &claims), &[testing::RESOURCE_SVC])
            .await
            .unwrap_err();
        assert!(matches!(err, TokenError::Rejected(_)), "{err}");
    }
}
