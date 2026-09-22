//! `crucible-controller oidc-claims`: a one-shot device-flow login that prints every claim the
//! issuer hands this client, from the ID token, the access token, and the userinfo endpoint, so an
//! operator can see where (and whether) `groups` arrives before flipping the auth mode.

#![allow(clippy::disallowed_macros)]

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::Deserialize;
use std::io::Write;
use std::time::{Duration, Instant};

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub struct ProbeCfg {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: String,
}

impl ProbeCfg {
    /// `OIDC_*` first, then the controller's own `CONTROLLER_OIDC_*`, so the Job can reuse the
    /// deployment's env verbatim.
    pub fn from_env(scopes: String) -> Result<Self> {
        let pick = |names: &[&str]| {
            names
                .iter()
                .find_map(|n| std::env::var(n).ok())
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Ok(ProbeCfg {
            issuer: pick(&["OIDC_ISSUER", "CONTROLLER_OIDC_ISSUER"])
                .context("OIDC_ISSUER is unset")?
                .trim_end_matches('/')
                .to_string(),
            client_id: pick(&["OIDC_CLIENT_ID", "CONTROLLER_OIDC_CLIENT_ID"])
                .context("OIDC_CLIENT_ID is unset")?,
            client_secret: pick(&["OIDC_CLIENT_SECRET", "CONTROLLER_OIDC_CLIENT_SECRET"]),
            scopes,
        })
    }
}

#[derive(Deserialize)]
struct Discovery {
    device_authorization_endpoint: Option<String>,
    token_endpoint: String,
    userinfo_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct DeviceStart {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_expires_in() -> u64 {
    600
}

fn default_interval() -> u64 {
    5
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

/// `application/x-www-form-urlencoded`, built through the URL serializer since this reqwest is
/// built without its `form` feature.
fn form_body(pairs: &[(&str, String)]) -> Result<String> {
    let mut url = reqwest::Url::parse("http://form.invalid/").context("form base url")?;
    url.query_pairs_mut()
        .extend_pairs(pairs.iter().map(|(k, v)| (*k, v.as_str())));
    Ok(url.query().unwrap_or_default().to_string())
}

/// The payload of a JWT, decoded without verifying the signature: this is a diagnostic that
/// prints what the issuer sent, not an authentication.
pub fn decode_jwt_payload(token: &str) -> Result<serde_json::Value> {
    let payload = token
        .split('.')
        .nth(1)
        .context("not a JWT: no payload segment")?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .context("the JWT payload is not base64url")?;
    serde_json::from_slice(&bytes).context("the JWT payload is not JSON")
}

/// Where a claim lives across the three sources, for the summary at the end.
#[derive(Debug, PartialEq, Eq)]
pub struct ClaimSighting {
    pub source: &'static str,
    pub value: serde_json::Value,
}

/// Every source that carries `groups` (top level) or `realm_access.roles`.
pub fn group_sightings(sources: &[(&'static str, &serde_json::Value)]) -> Vec<ClaimSighting> {
    let mut out = Vec::new();
    for (source, claims) in sources {
        if let Some(groups) = claims.get("groups") {
            out.push(ClaimSighting {
                source,
                value: groups.clone(),
            });
        }
        if let Some(roles) = claims.pointer("/realm_access/roles") {
            out.push(ClaimSighting {
                source: match *source {
                    "id_token" => "id_token realm_access.roles",
                    "access_token" => "access_token realm_access.roles",
                    _ => "userinfo realm_access.roles",
                },
                value: roles.clone(),
            });
        }
    }
    out
}

pub async fn run(cfg: &ProbeCfg, out: &mut impl Write) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building the http client")?;
    let discovery_url = format!("{}/.well-known/openid-configuration", cfg.issuer);
    let discovery: Discovery = http
        .get(&discovery_url)
        .send()
        .await
        .with_context(|| format!("fetching {discovery_url}"))?
        .error_for_status()
        .with_context(|| format!("discovery at {discovery_url}"))?
        .json()
        .await
        .context("parsing the discovery document")?;
    let device_endpoint = discovery
        .device_authorization_endpoint
        .context("the issuer advertises no device_authorization_endpoint")?;

    let mut form = vec![
        ("client_id", cfg.client_id.clone()),
        ("scope", cfg.scopes.clone()),
    ];
    if let Some(secret) = &cfg.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let resp = http
        .post(&device_endpoint)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form_body(&form)?)
        .send()
        .await
        .context("starting the device authorization")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "device authorization refused ({status}): {body}\n\
             the client needs 'OAuth 2.0 Device Authorization Grant' enabled on the issuer"
        );
    }
    let start: DeviceStart =
        serde_json::from_str(&body).context("parsing the device authorization response")?;

    writeln!(out, "open this URL and approve the login:")?;
    writeln!(
        out,
        "  {}",
        start
            .verification_uri_complete
            .as_deref()
            .unwrap_or(&start.verification_uri)
    )?;
    writeln!(out, "  code: {}", start.user_code)?;
    writeln!(out, "waiting up to {}s ...", start.expires_in)?;
    out.flush()?;

    let deadline = Instant::now() + Duration::from_secs(start.expires_in);
    let mut interval = Duration::from_secs(start.interval.max(1));
    let tokens: TokenResponse = loop {
        if Instant::now() >= deadline {
            bail!("the device code expired before it was approved");
        }
        tokio::time::sleep(interval).await;
        let mut form = vec![
            ("grant_type", DEVICE_GRANT.to_string()),
            ("device_code", start.device_code.clone()),
            ("client_id", cfg.client_id.clone()),
        ];
        if let Some(secret) = &cfg.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let resp = http
            .post(&discovery.token_endpoint)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_body(&form)?)
            .send()
            .await
            .context("polling the token endpoint")?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() {
            break serde_json::from_str(&body).context("parsing the token response")?;
        }
        let err: TokenError = serde_json::from_str(&body)
            .with_context(|| format!("token endpoint answered {status}: {body}"))?;
        match err.error.as_str() {
            "authorization_pending" => {}
            "slow_down" => interval += Duration::from_secs(5),
            other => bail!(
                "login failed: {other}: {}",
                err.error_description.unwrap_or_default()
            ),
        }
    };

    let access = decode_jwt_payload(&tokens.access_token).context("access token")?;
    let id = match &tokens.id_token {
        Some(t) => decode_jwt_payload(t).context("id token")?,
        None => serde_json::Value::Null,
    };
    let userinfo = match &discovery.userinfo_endpoint {
        Some(url) => http
            .get(url)
            .bearer_auth(&tokens.access_token)
            .send()
            .await
            .context("calling userinfo")?
            .error_for_status()
            .context("userinfo")?
            .json::<serde_json::Value>()
            .await
            .context("parsing userinfo")?,
        None => serde_json::Value::Null,
    };

    for (name, claims) in [
        ("id_token", &id),
        ("access_token", &access),
        ("userinfo", &userinfo),
    ] {
        writeln!(out, "\n== {name} (decoded, unverified)")?;
        writeln!(out, "{}", serde_json::to_string_pretty(claims)?)?;
    }

    let sightings = group_sightings(&[
        ("id_token", &id),
        ("access_token", &access),
        ("userinfo", &userinfo),
    ]);
    writeln!(out, "\n== groups summary")?;
    for key in ["preferred_username", "email", "sub"] {
        writeln!(
            out,
            "  {key}: {}",
            id.get(key)
                .or_else(|| access.get(key))
                .unwrap_or(&serde_json::Value::Null)
        )?;
    }
    if sightings.is_empty() {
        writeln!(
            out,
            "  groups: ABSENT from the id token, the access token, and userinfo; the client \
             needs a group membership mapper"
        )?;
        bail!("no groups claim reached this client");
    }
    for s in &sightings {
        writeln!(out, "  {}: {}", s.source, s.value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(payload: serde_json::Value) -> String {
        let enc = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        format!(
            "{}.{}.sig",
            enc(br#"{"alg":"RS256"}"#),
            enc(payload.to_string().as_bytes())
        )
    }

    #[test]
    fn the_form_body_percent_encodes() {
        let body = form_body(&[
            ("scope", "openid email".to_string()),
            ("a&b", "c=d".to_string()),
        ])
        .expect("body");
        assert_eq!(body, "scope=openid+email&a%26b=c%3Dd");
    }

    #[test]
    fn the_payload_decodes_without_a_signature() {
        let v = decode_jwt_payload(&jwt(serde_json::json!({"sub": "u1", "groups": ["/a"]})))
            .expect("payload");
        assert_eq!(v["sub"], "u1");
        assert_eq!(v["groups"][0], "/a");
        assert!(decode_jwt_payload("not-a-jwt").is_err());
        assert!(decode_jwt_payload("a.!!!.c").is_err());
    }

    #[test]
    fn group_sightings_name_every_source_that_carries_them() {
        let id = serde_json::json!({"sub": "u1"});
        let access = serde_json::json!({"realm_access": {"roles": ["offline_access"]}});
        let userinfo = serde_json::json!({"groups": ["/groups/team-x"]});
        let got = group_sightings(&[
            ("id_token", &id),
            ("access_token", &access),
            ("userinfo", &userinfo),
        ]);
        assert_eq!(
            got,
            vec![
                ClaimSighting {
                    source: "access_token realm_access.roles",
                    value: serde_json::json!(["offline_access"]),
                },
                ClaimSighting {
                    source: "userinfo",
                    value: serde_json::json!(["/groups/team-x"]),
                },
            ]
        );
        assert!(group_sightings(&[("id_token", &id)]).is_empty());
    }
}
