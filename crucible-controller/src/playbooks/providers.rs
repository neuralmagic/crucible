//! The inference-provider registry, and the chain that turns a dispatch into a harness + model.
//!
//! A provider is one place work can be sent to think: a kind (which harness runs, and which
//! environment variable the key lands in), a curated model list, and optionally the name of a
//! secrets-registry entry that pays for it. A `custom` provider also names the endpoint it is
//! reached at and the API it speaks, the non-secret half OpenShell keeps in a provider's `config`
//! map. Nothing here holds a credential — Postgres holds the reference, Vault holds the bytes.
//!
//! Resolution is a four-step chain, tried in order and answered by the first step that has an
//! answer: the override the launch pinned on its row, the default for the domain it belongs to, the
//! platform default for its workload class, and finally nothing at all. Nothing is a real answer:
//! it means render as before providers existed, letting the pack manifest's `[agent]` table decide.
//! An empty registry therefore changes no dispatch.
//!
//! Resolution runs at dispatch time rather than at insert time, the same way
//! [`crate::runs::dispatch_target`] resolves late: an administrator who repoints a default moves the work
//! that never asked for a specific model, and leaves the work that did where it was put.

#![allow(clippy::disallowed_macros)]

use crate::wire_enum::wire_enum;
use anyhow::{Context, Result, bail};
use crucible::manifest::Harness;
use sqlx::{PgExecutor, PgPool, Row};

/// Which service a provider talks to. The kind, not the display name, decides behaviour: it picks
/// the harness the pod renders with and how a credential is projected.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum ProviderKind {
    Anthropic,
    Vertex,
    // Spelled out, because `snake_case` would render this variant `open_ai` and the wire
    // vocabulary below (and every stored row) says `openai`.
    #[serde(rename = "openai")]
    OpenAi,
    /// A service the operator runs, reached at the endpoint the registration names and speaking
    /// the protocol it names. The protocol, not the kind, decides the harness and the key.
    Custom,
}

wire_enum!(ProviderKind, "provider kind", both, {
    ProviderKind::Anthropic => "anthropic",
    ProviderKind::Vertex => "vertex",
    ProviderKind::OpenAi => "openai",
    ProviderKind::Custom => "custom",
});

impl ProviderKind {
    /// The harness a registration of this kind runs when it names none; `None` for the custom
    /// kind, whose protocol says.
    pub fn default_harness(self) -> Option<Harness> {
        match self {
            ProviderKind::Anthropic | ProviderKind::Vertex => Some(Harness::Claude),
            ProviderKind::OpenAi => Some(Harness::Codex),
            ProviderKind::Custom => None,
        }
    }

    /// Every harness that can run this kind's models, the default first; `None` for the custom
    /// kind, whose protocol says.
    pub fn harnesses(self) -> Option<&'static [Harness]> {
        match self {
            ProviderKind::Anthropic => Some(&[Harness::Claude, Harness::OpenCode, Harness::Pi]),
            ProviderKind::Vertex => Some(&[Harness::Claude, Harness::Hermes]),
            ProviderKind::OpenAi => Some(&[Harness::Codex, Harness::OpenCode, Harness::Pi]),
            ProviderKind::Custom => None,
        }
    }
}

/// The manifest spelling of a harness.
pub fn parse_harness(name: &str) -> Option<Harness> {
    match name.trim().to_ascii_lowercase().as_str() {
        "claude" => Some(Harness::Claude),
        "hermes" => Some(Harness::Hermes),
        "codex" => Some(Harness::Codex),
        "opencode" => Some(Harness::OpenCode),
        "pi" => Some(Harness::Pi),
        _ => None,
    }
}

pub fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "claude",
        Harness::Hermes => "hermes",
        Harness::Codex => "codex",
        Harness::OpenCode => "opencode",
        Harness::Pi => "pi",
    }
}

/// The harness a registration runs when it names none: the kind's, else its protocol's.
pub fn default_harness(kind: ProviderKind, protocol: Option<InferenceProtocol>) -> Harness {
    kind.default_harness()
        .or_else(|| protocol.map(InferenceProtocol::default_harness))
        .unwrap_or_default()
}

/// The harnesses a registration may name, the default first: the kind's, else its protocol's.
pub fn allowed_harnesses(
    kind: ProviderKind,
    protocol: Option<InferenceProtocol>,
) -> &'static [Harness] {
    match kind.harnesses() {
        Some(harnesses) => harnesses,
        None => protocol.map_or(&[Harness::Claude], InferenceProtocol::harnesses),
    }
}

/// The API a custom provider speaks. Each protocol reads its key and base URL from one pair of
/// environment variables and has a default harness (plus the others that can speak it), so the
/// protocol is what a custom registration contributes in place of a fixed kind.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum InferenceProtocol {
    /// The Anthropic Messages API; Claude Code speaks it, as do opencode and pi.
    Messages,
    /// The OpenAI Chat Completions API; opencode and pi speak it. The current Codex CLI does not.
    ChatCompletions,
    /// The OpenAI Responses API (harmony); Codex speaks it over its `responses` wire API, pi too.
    Responses,
}

wire_enum!(InferenceProtocol, "inference protocol", both, {
    InferenceProtocol::Messages => "messages",
    InferenceProtocol::ChatCompletions => "chat_completions",
    InferenceProtocol::Responses => "responses",
});

impl InferenceProtocol {
    /// The harness that serves the protocol when a registration names none.
    pub fn default_harness(self) -> Harness {
        match self {
            InferenceProtocol::Messages => Harness::Claude,
            InferenceProtocol::ChatCompletions => Harness::OpenCode,
            InferenceProtocol::Responses => Harness::Codex,
        }
    }

    /// Every harness that speaks the protocol, the default first.
    pub fn harnesses(self) -> &'static [Harness] {
        match self {
            InferenceProtocol::Messages => &[Harness::Claude, Harness::OpenCode, Harness::Pi],
            InferenceProtocol::ChatCompletions => &[Harness::OpenCode, Harness::Pi],
            InferenceProtocol::Responses => &[Harness::Codex, Harness::Pi],
        }
    }

    pub fn api_key_env(self) -> &'static str {
        match self {
            InferenceProtocol::Messages => "ANTHROPIC_API_KEY",
            InferenceProtocol::ChatCompletions | InferenceProtocol::Responses => "OPENAI_API_KEY",
        }
    }

    /// The environment variable the harness reads its base URL from: OpenShell's config key for
    /// the same provider type, so a route configured there and one configured here agree.
    pub fn base_url_env(self) -> &'static str {
        match self {
            InferenceProtocol::Messages => "ANTHROPIC_BASE_URL",
            InferenceProtocol::ChatCompletions | InferenceProtocol::Responses => "OPENAI_BASE_URL",
        }
    }

    /// The engine's wire-API token for an OpenAI-speaking protocol (Codex's `wire_api`
    /// spelling, which opencode and pi read too), or `None` for Messages.
    pub fn codex_wire_api(self) -> Option<&'static str> {
        match self {
            InferenceProtocol::Messages => None,
            InferenceProtocol::ChatCompletions => Some("chat"),
            InferenceProtocol::Responses => Some("responses"),
        }
    }
}

/// The environment variable a custom provider's Codex wire API travels in. Read by the linked
/// engine when it renders Codex's `config.toml`.
pub const WIRE_API_ENV: &str = "CRUCIBLE_INFERENCE_WIRE_API";

/// Where a custom provider is reached, and what it speaks there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub protocol: InferenceProtocol,
}

/// Upper bound on an endpoint URL. It rides a pod environment variable and a registry row.
pub const ENDPOINT_MAX_LEN: usize = 512;

/// Check a custom provider's endpoint: an absolute `http`/`https` URL with a host, no credentials
/// in it (the key has its own home), and nothing a shell would read in the loop wrapper.
pub fn check_endpoint(raw: &str) -> Result<String, String> {
    let endpoint = raw.trim();
    if endpoint.is_empty() {
        return Err("endpoint must be non-empty".to_string());
    }
    if endpoint.len() > ENDPOINT_MAX_LEN {
        return Err(format!(
            "endpoint must be at most {ENDPOINT_MAX_LEN} characters"
        ));
    }
    let parsed = url::Url::parse(endpoint).map_err(|e| format!("endpoint is not a URL: {e}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "endpoint scheme {:?} is not http or https",
            parsed.scheme()
        ));
    }
    if parsed.host_str().is_none() {
        return Err("endpoint names no host".to_string());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(
            "endpoint carries credentials; register the key as a secret instead".to_string(),
        );
    }
    if endpoint.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, '\'' | '"' | '`' | '$' | ';' | '\\')
    }) {
        return Err("endpoint has a character a shell would read".to_string());
    }
    Ok(endpoint.trim_end_matches('/').to_string())
}

impl ProviderKind {
    /// The model list a provider of this kind starts with, first entry first. A curated list is a
    /// suggestion set: a launch may still name a model outside it. A custom provider serves
    /// whatever the operator loaded, so it starts empty.
    pub fn curated_models(self) -> &'static [&'static str] {
        match self {
            ProviderKind::Anthropic | ProviderKind::Vertex => {
                &["claude-opus-4-6", "claude-sonnet-5"]
            }
            ProviderKind::OpenAi => &["gpt-5.6-luna", "gpt-5.6-sol"],
            ProviderKind::Custom => &[],
        }
    }

    /// The model a provider of this kind takes when its registration names none, or `None` for a
    /// kind whose registration has to name one.
    pub fn default_model(self) -> Option<&'static str> {
        match self {
            ProviderKind::Anthropic | ProviderKind::Vertex => Some("claude-opus-4-6"),
            ProviderKind::OpenAi => Some("gpt-5.6-luna"),
            ProviderKind::Custom => None,
        }
    }
}

/// What kind of work a dispatch is, and therefore which default applies to it. The two classes get
/// separate defaults because they want different models: an autoresearch loop wants the heavier
/// one, a playbook run does not.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum WorkloadClass {
    Playbook,
    Autoresearch,
}

wire_enum!(WorkloadClass, "workload class", both, {
    WorkloadClass::Playbook => "playbook",
    WorkloadClass::Autoresearch => "autoresearch",
});

/// Upper bound on a model name. Long enough for every vendor's fully-qualified spelling, short
/// enough that a stored pin stays an identifier.
pub const MODEL_NAME_MAX_LEN: usize = 128;

/// Check a model name. The value reaches the loop pod as an unquoted `--model=<m>` inside the
/// wrapper script the pod runs under `/bin/sh -c`, so a name outside this charset is shell syntax
/// executing in a pod that mounts the kubeconfig, the push token, and the provider key.
pub fn check_model_name(raw: &str) -> Result<String, String> {
    let model = raw.trim();
    if model.is_empty() {
        return Err("a model name must be non-empty".to_string());
    }
    if model.len() > MODEL_NAME_MAX_LEN {
        return Err(format!(
            "model name must be at most {MODEL_NAME_MAX_LEN} characters"
        ));
    }
    if model.starts_with('-') {
        return Err(format!(
            "model name {model:?} starts with '-', which the in-pod CLI would read as a flag"
        ));
    }
    if !model
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '@'))
    {
        return Err(format!(
            "model name {model:?} has a character outside [A-Za-z0-9._-:/@]"
        ));
    }
    Ok(model.to_string())
}

/// How wide a default reaches. A domain default covers the work of one domain; the platform default
/// covers everything that has no narrower answer.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
#[derive(strum::EnumIter)]
pub enum DefaultScope {
    Platform,
    Domain,
}

wire_enum!(DefaultScope, "dispatch default scope", both, {
    DefaultScope::Platform => "platform",
    DefaultScope::Domain => "domain",
});

/// One `model_providers` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProvider {
    /// The slug every pinned column and API path refers to.
    pub id: String,
    pub display_name: String,
    pub kind: ProviderKind,
    pub models: Vec<String>,
    pub default_model: String,
    /// The secrets-registry entry that pays for this provider, or `None` for the deploy profile's
    /// ambient credentials. The owner is stored with the name because a secret name is unique per
    /// owner and not globally: on the name alone, anyone who may register a secret of their own
    /// could make the reference ambiguous and refuse every dispatch that spends it.
    pub secret: Option<ProviderSecretRef>,
    /// Where a custom provider is reached; `Some` exactly when the kind is custom.
    pub endpoint: Option<Endpoint>,
    /// The harness the registration named, one of [`allowed_harnesses`] for its kind and protocol;
    /// `None` runs the default for them.
    pub harness: Option<Harness>,
    /// A disabled provider may not be chosen at launch, but a launch that already pinned it still
    /// resolves: turning a provider off must not strand the work already committed to it.
    pub enabled: bool,
    pub owner: crate::authz::model::Principal,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

impl ModelProvider {
    /// The model this provider runs when the caller named none.
    pub fn model_or_default(&self, requested: Option<&str>) -> String {
        match requested.map(str::trim).filter(|m| !m.is_empty()) {
            Some(m) => m.to_string(),
            None => self.default_model.clone(),
        }
    }

    /// The agent harness this provider's models run under: the one the registration named, else
    /// the default for its kind (Anthropic and Vertex are two ways to reach the same models, so
    /// both render Claude Code; OpenAI renders Codex) or, for a custom provider, its protocol.
    pub fn harness(&self) -> Harness {
        self.harness.unwrap_or_else(|| {
            default_harness(self.kind, self.endpoint.as_ref().map(|e| e.protocol))
        })
    }

    /// The environment variable a registered key is projected as, or `None` for a provider that
    /// authenticates some other way. Vertex is the `None`: it runs on the deploy profile's ambient
    /// application-default credentials, so a Vertex provider carrying a secret is a
    /// misconfiguration rather than a key with nowhere to go.
    pub fn api_key_env(&self) -> Option<&'static str> {
        match (self.kind, self.endpoint.as_ref()) {
            (ProviderKind::Anthropic, _) => Some("ANTHROPIC_API_KEY"),
            (ProviderKind::OpenAi, _) => Some("OPENAI_API_KEY"),
            (ProviderKind::Vertex, _) => None,
            (ProviderKind::Custom, endpoint) => endpoint.map(|e| e.protocol.api_key_env()),
        }
    }

    /// The non-secret environment a pod running against this provider carries: the base URL under
    /// the name the harness reads, and the Codex wire API when the protocol has one. Empty for a
    /// provider reached at its service's own address.
    pub fn config_env(&self) -> Vec<(String, String)> {
        let Some(endpoint) = self.endpoint.as_ref() else {
            return Vec::new();
        };
        let mut env = vec![(
            endpoint.protocol.base_url_env().to_string(),
            endpoint.url.clone(),
        )];
        if let Some(wire) = endpoint.protocol.codex_wire_api() {
            env.push((WIRE_API_ENV.to_string(), wire.to_string()));
        }
        env
    }
}

/// What a provider registration asks for. `models` empty takes [`ProviderKind::curated_models`],
/// and `default_model` empty takes [`ProviderKind::default_model`].
#[derive(Debug, Clone)]
pub struct NewProvider<'a> {
    pub id: &'a str,
    pub display_name: &'a str,
    pub kind: ProviderKind,
    pub models: &'a [String],
    pub default_model: Option<&'a str>,
    /// The credential reference, name and owning keyspace together, or `None` for ambient
    /// credentials.
    pub secret: Option<&'a ProviderSecretRef>,
    /// Where a custom provider is reached; required for the custom kind, refused for the others.
    pub endpoint: Option<&'a Endpoint>,
    /// The harness to run under, one of [`allowed_harnesses`] for the kind and protocol; `None`
    /// takes their default.
    pub harness: Option<Harness>,
    pub enabled: bool,
    pub created_by: &'a str,
    pub owner: crate::authz::model::Principal,
}

/// The registry entry a provider spends: which name, in whose keyspace. The pair travels together
/// because a secret name is unique only within its owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSecretRef {
    pub name: String,
    pub owner: crate::authz::model::Principal,
}

/// One `dispatch_defaults` row: which provider a scope's dispatches of one class take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchDefault {
    pub scope_kind: DefaultScope,
    /// The domain name, or empty for the platform-wide row.
    pub scope_ref: String,
    pub workload_class: WorkloadClass,
    pub provider_id: String,
    /// `None` takes the provider's own `default_model`.
    pub model: Option<String>,
}

/// What a launch pinned on its row. A model alone is not a choice, because the provider is what
/// says which service that model name belongs to, so the pair travels together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DispatchOverride<'a> {
    pub provider_id: &'a str,
    pub model: Option<&'a str>,
}

impl<'a> DispatchOverride<'a> {
    /// The override a pair of stored columns carries, or `None` when the row pinned nothing. A
    /// model without a provider is refused at the API, so it reads here as no override.
    pub fn from_columns(provider_id: Option<&'a str>, model: Option<&'a str>) -> Option<Self> {
        let provider_id = provider_id.map(str::trim).filter(|p| !p.is_empty())?;
        Some(DispatchOverride {
            provider_id,
            model: model.map(str::trim).filter(|m| !m.is_empty()),
        })
    }
}

/// What a dispatch resolved to: the provider that will serve it, the model it will ask for, and the
/// harness the pod renders with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDispatch {
    pub provider: ModelProvider,
    pub model: String,
    pub harness: Harness,
}

/// What a render is told about the agent: the harness flag and the model flag, or neither. Both
/// `None` is the pre-registry render — no flags reach the pod, and the pack manifest's `[agent]`
/// table decides, which is what makes an empty registry a no-op.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentSelection {
    pub harness: Option<Harness>,
    pub model: Option<String>,
}

impl AgentSelection {
    /// What [`resolve_dispatch`] answered, as render flags. `None` selects nothing.
    pub fn from_resolved(resolved: Option<&ResolvedDispatch>) -> Self {
        match resolved {
            Some(r) => AgentSelection {
                harness: Some(r.harness),
                model: Some(r.model.clone()),
            },
            None => AgentSelection::default(),
        }
    }
}

impl ResolvedDispatch {
    fn new(provider: ModelProvider, model: Option<&str>) -> ResolvedDispatch {
        let model = provider.model_or_default(model);
        let harness = provider.harness();
        ResolvedDispatch {
            provider,
            model,
            harness,
        }
    }
}

const PROVIDER_COLS: &str = "id, display_name, kind, models, default_model, secret_name, \
     secret_owner, endpoint, protocol, harness, enabled, owner, created_by, created_at, \
     updated_at";

fn provider_from_row(row: &sqlx::postgres::PgRow) -> Result<ModelProvider> {
    let kind: String = row.try_get("kind")?;
    let secret_name: Option<String> = row.try_get("secret_name")?;
    let secret_owner: Option<String> = row.try_get("secret_owner")?;
    let secret = match (secret_name, secret_owner) {
        (Some(name), Some(owner)) => Some(ProviderSecretRef {
            name,
            owner: crate::authz::model::Principal::parse(&owner)?,
        }),
        (None, None) => None,
        (name, owner) => bail!(
            "provider row carries a half credential reference (name {name:?}, owner {owner:?})"
        ),
    };
    let endpoint_url: Option<String> = row.try_get("endpoint")?;
    let protocol: Option<String> = row.try_get("protocol")?;
    let endpoint = match (endpoint_url, protocol) {
        (Some(url), Some(protocol)) => Some(Endpoint {
            url,
            protocol: InferenceProtocol::parse(&protocol)?,
        }),
        (None, None) => None,
        (url, protocol) => {
            bail!("provider row carries a half endpoint (url {url:?}, protocol {protocol:?})")
        }
    };
    let harness = match row.try_get::<Option<String>, _>("harness")? {
        None => None,
        Some(name) => match parse_harness(&name) {
            Some(h) => Some(h),
            None => bail!("provider row names an unknown harness {name:?}"),
        },
    };
    Ok(ModelProvider {
        id: row.try_get("id")?,
        display_name: row.try_get("display_name")?,
        kind: ProviderKind::parse(&kind)?,
        models: row.try_get("models")?,
        default_model: row.try_get("default_model")?,
        secret,
        endpoint,
        harness,
        enabled: row.try_get("enabled")?,
        owner: crate::authz::model::Principal::parse(&row.try_get::<String, _>("owner")?)?,
        created_by: row.try_get("created_by")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn default_from_row(row: &sqlx::postgres::PgRow) -> Result<DispatchDefault> {
    let scope_kind: String = row.try_get("scope_kind")?;
    let workload_class: String = row.try_get("workload_class")?;
    Ok(DispatchDefault {
        scope_kind: DefaultScope::parse(&scope_kind)?,
        scope_ref: row.try_get("scope_ref")?,
        workload_class: WorkloadClass::parse(&workload_class)?,
        provider_id: row.try_get("provider_id")?,
        model: row.try_get("model")?,
    })
}

/// The bound columns a registration writes, shared by [`insert`] and [`upsert`] so the two can
/// never bind a different row for the same registration.
struct ProviderBinds {
    models: Vec<String>,
    default_model: String,
    secret_name: Option<String>,
    secret_owner: Option<String>,
    endpoint: Option<String>,
    protocol: Option<&'static str>,
    harness: Option<&'static str>,
}

fn provider_binds(new: &NewProvider<'_>) -> Result<ProviderBinds> {
    let models: Vec<String> = match new.models.is_empty() {
        true => new
            .kind
            .curated_models()
            .iter()
            .map(|m| m.to_string())
            .collect(),
        false => new.models.to_vec(),
    };
    let default_model = match new
        .default_model
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .or(new.kind.default_model())
    {
        Some(m) => m.to_string(),
        None => bail!(
            "a {} provider names no default model and its kind has none",
            new.kind.as_str()
        ),
    };
    match (new.kind, new.endpoint) {
        (ProviderKind::Custom, None) => bail!("a custom provider needs an endpoint"),
        (kind, Some(_)) if kind != ProviderKind::Custom => {
            bail!(
                "a {} provider is reached at its own address and takes no endpoint",
                kind.as_str()
            )
        }
        _ => {}
    }
    let protocol = new.endpoint.map(|e| e.protocol);
    if let Some(harness) = new.harness
        && !allowed_harnesses(new.kind, protocol).contains(&harness)
    {
        bail!(
            "a {} provider{} cannot run under the {} harness; it runs under one of {}",
            new.kind.as_str(),
            protocol.map_or(String::new(), |p| format!(" speaking {}", p.as_str())),
            harness_name(harness),
            harness_list(allowed_harnesses(new.kind, protocol))
        );
    }
    Ok(ProviderBinds {
        models,
        default_model,
        secret_name: new.secret.map(|s| s.name.clone()),
        secret_owner: new.secret.map(|s| s.owner.to_string()),
        endpoint: new.endpoint.map(|e| e.url.clone()),
        protocol: protocol.map(|p| p.as_str()),
        harness: new.harness.map(harness_name),
    })
}

/// The harnesses spelled for a refusal: `claude, opencode, pi`.
pub fn harness_list(harnesses: &[Harness]) -> String {
    harnesses
        .iter()
        .map(|h| harness_name(*h))
        .collect::<Vec<_>>()
        .join(", ")
}

const PROVIDER_INSERT: &str = "INSERT INTO model_providers (id, display_name, kind, models, default_model, secret_name, \
     secret_owner, endpoint, protocol, enabled, created_by, created_at, updated_at, owner, harness) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12, $13, $14)";

const PROVIDER_INSERT_OR_IGNORE: &str =
    const_format::concatcp!(PROVIDER_INSERT, " ON CONFLICT (id) DO NOTHING");

const PROVIDER_UPSERT: &str = const_format::concatcp!(
    PROVIDER_INSERT,
    " ON CONFLICT (id) DO UPDATE SET display_name = EXCLUDED.display_name, \
         kind = EXCLUDED.kind, models = EXCLUDED.models, \
         default_model = EXCLUDED.default_model, secret_name = EXCLUDED.secret_name, \
         secret_owner = EXCLUDED.secret_owner, endpoint = EXCLUDED.endpoint, \
         protocol = EXCLUDED.protocol, harness = EXCLUDED.harness, \
         enabled = EXCLUDED.enabled, updated_at = EXCLUDED.updated_at"
);

async fn write_provider(
    ex: impl PgExecutor<'_>,
    new: &NewProvider<'_>,
    sql: &'static str,
    context: &'static str,
) -> Result<u64> {
    let binds = provider_binds(new)?;
    let written = sqlx::query(sql)
        .bind(new.id)
        .bind(new.display_name)
        .bind(new.kind.as_str())
        .bind(&binds.models)
        .bind(&binds.default_model)
        .bind(&binds.secret_name)
        .bind(&binds.secret_owner)
        .bind(&binds.endpoint)
        .bind(binds.protocol)
        .bind(new.enabled)
        .bind(new.created_by)
        .bind(crate::clock::now_rfc3339())
        .bind(new.owner.to_string())
        .bind(binds.harness)
        .execute(ex)
        .await
        .context(context)?;
    Ok(written.rows_affected())
}

/// Register a provider under an id nothing holds yet. `false` when one already does — the check
/// and the write are the same statement, so two concurrent registrations cannot both believe they
/// created the row.
#[tracing::instrument(name = "db.insert_provider", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", provider = %new.id), err)]
pub async fn insert(ex: impl PgExecutor<'_>, new: &NewProvider<'_>) -> Result<bool> {
    Ok(write_provider(ex, new, PROVIDER_INSERT_OR_IGNORE, "insert_provider").await? > 0)
}

/// Register a provider, or replace the registration under that id.
#[tracing::instrument(name = "db.upsert_provider", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", provider = %new.id), err)]
pub async fn upsert(ex: impl PgExecutor<'_>, new: &NewProvider<'_>) -> Result<()> {
    write_provider(ex, new, PROVIDER_UPSERT, "upsert_provider")
        .await
        .map(|_| ())
}

/// One provider by id, enabled or not.
#[tracing::instrument(name = "db.get_provider", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", provider = %id), err)]
pub async fn get(ex: impl PgExecutor<'_>, id: &str) -> Result<Option<ModelProvider>> {
    let sql = const_format::formatcp!("SELECT {PROVIDER_COLS} FROM model_providers WHERE id = $1");
    let row = sqlx::query(sql)
        .bind(id)
        .fetch_optional(ex)
        .await
        .context("get_provider")?;
    row.as_ref().map(provider_from_row).transpose()
}

/// Every registered provider, id order. `enabled_only` is what the launch pickers ask for.
#[tracing::instrument(name = "db.list_providers", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub async fn list(ex: impl PgExecutor<'_>, enabled_only: bool) -> Result<Vec<ModelProvider>> {
    let sql = const_format::formatcp!(
        "SELECT {PROVIDER_COLS} FROM model_providers WHERE ($1 = FALSE OR enabled) ORDER BY id"
    );
    let rows = sqlx::query(sql)
        .bind(enabled_only)
        .fetch_all(ex)
        .await
        .context("list_providers")?;
    rows.iter().map(provider_from_row).collect()
}

/// What still names a provider, and would be stranded by deregistering it: the issues whose rows
/// pinned it, and the schedules that would keep minting more of them. Defaults are not counted —
/// they cascade with the row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderUsage {
    /// The keys of the issues pinned to it, capped at [`USAGE_SAMPLE`] for the message.
    pub issues: Vec<String>,
    pub issue_count: i64,
    pub schedules: Vec<String>,
    pub schedule_count: i64,
}

impl ProviderUsage {
    pub fn is_empty(&self) -> bool {
        self.issue_count == 0 && self.schedule_count == 0
    }
}

/// How many names a refusal lists before it stops naming them.
pub const USAGE_SAMPLE: i64 = 5;

/// Everything still pinned to a provider. A deregistration that ignored these would leave every
/// one of those dispatches erroring at resolution with no API left to clear the pin.
#[tracing::instrument(name = "db.provider_usage", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", provider = %id), err)]
pub async fn usage(pool: &PgPool, id: &str) -> Result<ProviderUsage> {
    let issues =
        sqlx::query("SELECT key FROM issues WHERE agent_provider = $1 ORDER BY key LIMIT $2")
            .bind(id)
            .bind(USAGE_SAMPLE)
            .fetch_all(pool)
            .await
            .context("issues pinned to a provider")?;
    let issue_count: i64 =
        sqlx::query("SELECT count(*) AS n FROM issues WHERE agent_provider = $1")
            .bind(id)
            .fetch_one(pool)
            .await
            .context("counting issues pinned to a provider")?
            .try_get("n")?;
    let schedules = sqlx::query(
        "SELECT id FROM playbook_standing_launches WHERE agent_provider = $1 ORDER BY id LIMIT $2",
    )
    .bind(id)
    .bind(USAGE_SAMPLE)
    .fetch_all(pool)
    .await
    .context("schedules pinned to a provider")?;
    let schedule_count: i64 = sqlx::query(
        "SELECT count(*) AS n FROM playbook_standing_launches WHERE agent_provider = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .context("counting schedules pinned to a provider")?
    .try_get("n")?;
    Ok(ProviderUsage {
        issues: issues
            .iter()
            .map(|r| r.try_get("key"))
            .collect::<Result<_, _>>()?,
        issue_count,
        schedules: schedules
            .iter()
            .map(|r| r.try_get("id"))
            .collect::<Result<_, _>>()?,
        schedule_count,
    })
}

/// Deregister a provider. `false` when there was none under that id. Its defaults go with it.
#[tracing::instrument(name = "db.delete_provider", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", provider = %id), err)]
pub async fn delete(ex: impl PgExecutor<'_>, id: &str) -> Result<bool> {
    let deleted = sqlx::query("DELETE FROM model_providers WHERE id = $1")
        .bind(id)
        .execute(ex)
        .await
        .context("delete_provider")?;
    Ok(deleted.rows_affected() > 0)
}

/// Point a scope's dispatches of one class at a provider, replacing whatever it pointed at.
#[tracing::instrument(name = "db.set_dispatch_default", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql", provider = %row.provider_id), err)]
pub async fn set_default(ex: impl PgExecutor<'_>, row: &DispatchDefault) -> Result<()> {
    sqlx::query(
        "INSERT INTO dispatch_defaults (scope_kind, scope_ref, workload_class, provider_id, model) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (scope_kind, scope_ref, workload_class) \
         DO UPDATE SET provider_id = EXCLUDED.provider_id, model = EXCLUDED.model",
    )
    .bind(row.scope_kind.as_str())
    .bind(&row.scope_ref)
    .bind(row.workload_class.as_str())
    .bind(&row.provider_id)
    .bind(&row.model)
    .execute(ex)
    .await
    .context("set_dispatch_default")?;
    Ok(())
}

/// Clear one scope's default for one class. `false` when it had none.
#[tracing::instrument(name = "db.clear_dispatch_default", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub async fn clear_default(
    ex: impl PgExecutor<'_>,
    scope_kind: DefaultScope,
    scope_ref: &str,
    class: WorkloadClass,
) -> Result<bool> {
    let deleted = sqlx::query(
        "DELETE FROM dispatch_defaults \
         WHERE scope_kind = $1 AND scope_ref = $2 AND workload_class = $3",
    )
    .bind(scope_kind.as_str())
    .bind(scope_ref)
    .bind(class.as_str())
    .execute(ex)
    .await
    .context("clear_dispatch_default")?;
    Ok(deleted.rows_affected() > 0)
}

const DEFAULT_COLS: &str = "scope_kind, scope_ref, workload_class, provider_id, model";

/// Every default, so the launch pickers can preselect the one that would apply.
#[tracing::instrument(name = "db.list_dispatch_defaults", skip_all, fields(otel.kind = "client", span.type = "sql", db.system = "postgresql"), err)]
pub async fn list_defaults(ex: impl PgExecutor<'_>) -> Result<Vec<DispatchDefault>> {
    let sql = const_format::formatcp!(
        "SELECT {DEFAULT_COLS} FROM dispatch_defaults ORDER BY scope_kind, scope_ref, \
         workload_class"
    );
    let rows = sqlx::query(sql)
        .fetch_all(ex)
        .await
        .context("list_dispatch_defaults")?;
    rows.iter().map(default_from_row).collect()
}

async fn get_default(
    ex: impl PgExecutor<'_>,
    scope_kind: DefaultScope,
    scope_ref: &str,
    class: WorkloadClass,
) -> Result<Option<DispatchDefault>> {
    let sql = const_format::formatcp!(
        "SELECT {DEFAULT_COLS} FROM dispatch_defaults \
         WHERE scope_kind = $1 AND scope_ref = $2 AND workload_class = $3"
    );
    let row = sqlx::query(sql)
        .bind(scope_kind.as_str())
        .bind(scope_ref)
        .bind(class.as_str())
        .fetch_optional(ex)
        .await
        .context("get_dispatch_default")?;
    row.as_ref().map(default_from_row).transpose()
}

/// What this dispatch runs against: the override the launch pinned, else the domain's default for
/// this class, else the platform's, else `None` — dispatch exactly as before providers existed.
/// The pair reaches the pod as the `--harness`/`--model` flags of the loop wrapper or of
/// `crucible plan run`, replacing the pack manifest's `[agent]` defaults.
///
/// A pinned provider that has since been deregistered is an error rather than a silent fall-through
/// to the defaults: the work was committed to a specific service, and quietly running it somewhere
/// else is worse than saying the registration is gone.
pub async fn resolve_dispatch(
    pool: &PgPool,
    over: Option<DispatchOverride<'_>>,
    domain: Option<&str>,
    class: WorkloadClass,
) -> Result<Option<ResolvedDispatch>> {
    if let Some(over) = over {
        let Some(provider) = get(pool, over.provider_id).await? else {
            bail!(
                "dispatch pinned model provider {:?}, which is no longer registered",
                over.provider_id
            );
        };
        return Ok(Some(ResolvedDispatch::new(provider, over.model)));
    }
    let domain = domain.map(str::trim).filter(|d| !d.is_empty());
    let scopes = [
        domain.map(|d| (DefaultScope::Domain, d)),
        Some((DefaultScope::Platform, "")),
    ];
    for (scope_kind, scope_ref) in scopes.into_iter().flatten() {
        let Some(row) = get_default(pool, scope_kind, scope_ref, class).await? else {
            continue;
        };
        let Some(provider) = get(pool, &row.provider_id).await? else {
            bail!(
                "dispatch default {}/{scope_ref} names model provider {:?}, which is no longer \
                 registered",
                scope_kind.as_str(),
                row.provider_id
            );
        };
        // Disabling a provider stops NEW work reaching it. Work that pinned it keeps resolving
        // above; a default is not a pin, so an unpinned dispatch inheriting one is new work.
        if !provider.enabled {
            bail!(
                "dispatch default {}/{scope_ref} names model provider {:?}, which is disabled and \
                 takes no new work",
                scope_kind.as_str(),
                row.provider_id
            );
        }
        return Ok(Some(ResolvedDispatch::new(provider, row.model.as_deref())));
    }
    Ok(None)
}

/// What one issue's dispatch of `class` runs against: the pair its row pinned at launch, else the
/// defaults its repo and class inherit.
///
/// A domain default is keyed by the issue's `owner/repo`, which is the only name a dispatch has to
/// scope by, so [`crate::api::providers`] holds a domain `scope_ref` to that same spelling. Keying
/// one vocabulary and matching another would configure defaults nothing ever inherits.
pub async fn resolve_for_issue(
    pool: &PgPool,
    issue: &crate::issues::model::Issue,
    class: WorkloadClass,
) -> Result<Option<ResolvedDispatch>> {
    let over = DispatchOverride::from_columns(
        issue.agent_provider.as_deref(),
        issue.agent_model.as_deref(),
    );
    resolve_dispatch(pool, over, Some(&issue.repo), class).await
}

#[cfg(test)]
mod tests {
    use crate::playbooks::providers::*;
    use crucible::manifest::Harness;
    use sqlx::PgPool;

    #[test]
    fn an_endpoint_is_an_absolute_http_url_with_nothing_a_shell_reads() {
        assert_eq!(
            check_endpoint(" https://vllm.internal:8000/v1/ ").expect("a URL"),
            "https://vllm.internal:8000/v1"
        );
        assert_eq!(
            check_endpoint("http://10.0.0.7/v1").expect("plain http is fine on-prem"),
            "http://10.0.0.7/v1"
        );
        for (raw, why) in [
            ("", "empty"),
            ("vllm.internal/v1", "no scheme"),
            ("ftp://vllm.internal/v1", "wrong scheme"),
            ("https://user:pw@vllm.internal/v1", "credentials in the URL"),
            ("https://vllm.internal/v1;id", "shell syntax"),
            ("https://vllm.internal/v1 --x", "whitespace"),
        ] {
            assert!(check_endpoint(raw).is_err(), "{why}: {raw:?}");
        }
    }

    #[test]
    fn a_protocol_decides_the_harness_the_key_and_the_base_url() {
        assert_eq!(
            InferenceProtocol::Messages.default_harness(),
            Harness::Claude
        );
        assert_eq!(
            InferenceProtocol::Messages.api_key_env(),
            "ANTHROPIC_API_KEY"
        );
        assert_eq!(
            InferenceProtocol::Messages.base_url_env(),
            "ANTHROPIC_BASE_URL"
        );
        assert_eq!(InferenceProtocol::Messages.codex_wire_api(), None);
        for (protocol, wire, harness) in [
            (
                InferenceProtocol::ChatCompletions,
                "chat",
                Harness::OpenCode,
            ),
            (InferenceProtocol::Responses, "responses", Harness::Codex),
        ] {
            assert_eq!(protocol.default_harness(), harness);
            assert_eq!(protocol.api_key_env(), "OPENAI_API_KEY");
            assert_eq!(protocol.base_url_env(), "OPENAI_BASE_URL");
            assert_eq!(protocol.codex_wire_api(), Some(wire));
        }
    }

    /// A registration runs its kind's (or, for a custom provider, its protocol's) default harness
    /// unless it names one of the harnesses that can speak to the service; anything else is refused
    /// before it is stored.
    #[test]
    fn a_registration_runs_its_default_harness_unless_it_names_an_allowed_one() {
        use InferenceProtocol::{ChatCompletions, Messages, Responses};
        assert_eq!(default_harness(ProviderKind::OpenAi, None), Harness::Codex);
        assert_eq!(
            default_harness(ProviderKind::Anthropic, None),
            Harness::Claude
        );
        assert_eq!(default_harness(ProviderKind::Vertex, None), Harness::Claude);
        assert_eq!(
            default_harness(ProviderKind::Custom, Some(ChatCompletions)),
            Harness::OpenCode
        );
        assert_eq!(
            default_harness(ProviderKind::Custom, Some(Responses)),
            Harness::Codex
        );
        assert_eq!(
            default_harness(ProviderKind::Custom, Some(Messages)),
            Harness::Claude
        );
        for (kind, protocol) in [
            (ProviderKind::OpenAi, None),
            (ProviderKind::Anthropic, None),
            (ProviderKind::Vertex, None),
            (ProviderKind::Custom, Some(ChatCompletions)),
            (ProviderKind::Custom, Some(Responses)),
            (ProviderKind::Custom, Some(Messages)),
        ] {
            assert_eq!(
                allowed_harnesses(kind, protocol)[0],
                default_harness(kind, protocol),
                "the default leads the allowed list for {kind:?}/{protocol:?}"
            );
        }
        assert_eq!(
            allowed_harnesses(ProviderKind::Custom, Some(ChatCompletions)),
            &[Harness::OpenCode, Harness::Pi],
            "the current codex cli speaks no chat completions"
        );
        assert_eq!(
            allowed_harnesses(ProviderKind::Vertex, None),
            &[Harness::Claude, Harness::Hermes]
        );
        assert_eq!(
            harness_list(&[Harness::OpenCode, Harness::Pi]),
            "opencode, pi"
        );

        let endpoint = Endpoint {
            url: "http://vllm.internal:8000/v1".to_string(),
            protocol: ChatCompletions,
        };
        let new = |harness| NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id: "chat",
            display_name: "chat",
            kind: ProviderKind::Custom,
            models: &[],
            default_model: Some("qwen-3-8-27b"),
            secret: None,
            endpoint: Some(&endpoint),
            harness,
            enabled: true,
            created_by: "alice",
        };
        assert_eq!(
            provider_binds(&new(Some(Harness::Pi))).expect("pi").harness,
            Some("pi")
        );
        assert_eq!(provider_binds(&new(None)).expect("default").harness, None);
        let err = match provider_binds(&new(Some(Harness::Codex))) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("codex cannot speak chat completions"),
        };
        assert!(err.contains("opencode, pi"), "{err}");
        assert!(err.contains("chat_completions"), "{err}");
    }

    /// A chat-completions provider round-trips the harness it named and resolves a dispatch to it;
    /// one that named none runs opencode, the protocol's default.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_chat_completions_provider_runs_opencode_or_the_harness_it_named(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let endpoint = Endpoint {
            url: "https://inference.example.com/llm/qwen/v1".to_string(),
            protocol: InferenceProtocol::ChatCompletions,
        };
        for (id, harness) in [("bay", None), ("bay-pi", Some(Harness::Pi))] {
            upsert(
                &pool,
                &NewProvider {
                    owner: crate::authz::model::Principal::platform(),
                    id,
                    display_name: id,
                    kind: ProviderKind::Custom,
                    models: &["qwen-3-8-27b".to_string()],
                    default_model: Some("qwen-3-8-27b"),
                    secret: None,
                    endpoint: Some(&endpoint),
                    harness,
                    enabled: true,
                    created_by: "alice",
                },
            )
            .await?;
        }
        let defaulted = get(&pool, "bay").await?.expect("the row");
        assert_eq!(defaulted.harness, None);
        assert_eq!(defaulted.harness(), Harness::OpenCode);
        assert_eq!(
            defaulted.config_env(),
            vec![
                (
                    "OPENAI_BASE_URL".to_string(),
                    "https://inference.example.com/llm/qwen/v1".to_string()
                ),
                (WIRE_API_ENV.to_string(), "chat".to_string()),
            ]
        );
        let pinned = get(&pool, "bay-pi").await?.expect("the row");
        assert_eq!(pinned.harness, Some(Harness::Pi));
        assert_eq!(pinned.harness(), Harness::Pi);
        let resolved = resolve_dispatch(
            &pool,
            Some(DispatchOverride {
                provider_id: "bay-pi",
                model: None,
            }),
            None,
            WorkloadClass::Playbook,
        )
        .await?
        .expect("resolves");
        assert_eq!(resolved.harness, Harness::Pi);
        assert_eq!(resolved.model, "qwen-3-8-27b");
        Ok(())
    }

    /// A custom provider stores its endpoint and protocol, resolves to the harness its protocol is
    /// spoken by, and hands the pod the base URL and wire API as plain environment.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_custom_provider_round_trips_and_derives_its_env(pool: PgPool) -> anyhow::Result<()> {
        let endpoint = Endpoint {
            url: "http://vllm.internal:8000/v1".to_string(),
            protocol: InferenceProtocol::Responses,
        };
        upsert(
            &pool,
            &NewProvider {
                owner: crate::authz::model::Principal::platform(),
                id: "onprem",
                display_name: "On-prem vLLM",
                kind: ProviderKind::Custom,
                models: &["gpt-oss-120b".to_string()],
                default_model: Some("gpt-oss-120b"),
                secret: None,
                endpoint: Some(&endpoint),
                harness: None,
                enabled: true,
                created_by: "alice",
            },
        )
        .await?;
        let stored = get(&pool, "onprem").await?.expect("the row");
        assert_eq!(stored.endpoint.as_ref(), Some(&endpoint));
        assert_eq!(stored.harness(), Harness::Codex);
        assert_eq!(stored.api_key_env(), Some("OPENAI_API_KEY"));
        assert_eq!(
            stored.config_env(),
            vec![
                (
                    "OPENAI_BASE_URL".to_string(),
                    "http://vllm.internal:8000/v1".to_string()
                ),
                (WIRE_API_ENV.to_string(), "responses".to_string()),
            ]
        );
        let resolved = resolve_dispatch(
            &pool,
            Some(DispatchOverride {
                provider_id: "onprem",
                model: None,
            }),
            None,
            WorkloadClass::Playbook,
        )
        .await?
        .expect("resolves");
        assert_eq!(resolved.harness, Harness::Codex);
        assert_eq!(resolved.model, "gpt-oss-120b");

        let no_default = upsert(
            &pool,
            &NewProvider {
                owner: crate::authz::model::Principal::platform(),
                id: "onprem-2",
                display_name: "no default",
                kind: ProviderKind::Custom,
                models: &[],
                default_model: None,
                secret: None,
                endpoint: Some(&endpoint),
                harness: None,
                enabled: true,
                created_by: "alice",
            },
        )
        .await
        .expect_err("a custom provider has no kind default to fall back on");
        assert!(
            format!("{no_default:#}").contains("default model"),
            "{no_default:#}"
        );
        let misplaced = upsert(
            &pool,
            &NewProvider {
                owner: crate::authz::model::Principal::platform(),
                id: "oa",
                display_name: "OpenAI",
                kind: ProviderKind::OpenAi,
                models: &[],
                default_model: None,
                secret: None,
                endpoint: Some(&endpoint),
                harness: None,
                enabled: true,
                created_by: "alice",
            },
        )
        .await
        .expect_err("an endpoint on a service-addressed kind is refused");
        assert!(
            format!("{misplaced:#}").contains("takes no endpoint"),
            "{misplaced:#}"
        );
        Ok(())
    }

    fn new_provider<'a>(id: &'a str, kind: ProviderKind) -> NewProvider<'a> {
        NewProvider {
            owner: crate::authz::model::Principal::platform(),
            id,
            display_name: "Some Provider",
            kind,
            models: &[],
            default_model: None,
            secret: None,
            endpoint: None,
            harness: None,
            enabled: true,
            created_by: "alice",
        }
    }

    /// The compatibility promise: with nothing registered, resolution answers nothing, which is
    /// what leaves the pack manifest deciding the harness and the model exactly as it did before.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn an_empty_registry_resolves_to_nothing(pool: PgPool) -> anyhow::Result<()> {
        for class in [WorkloadClass::Playbook, WorkloadClass::Autoresearch] {
            assert_eq!(resolve_dispatch(&pool, None, None, class).await?, None);
            assert_eq!(
                resolve_dispatch(&pool, None, Some("org/vllm"), class).await?,
                None
            );
        }
        // A registered provider that nothing points at is still not a default.
        upsert(
            &pool,
            &new_provider("anthropic-plat", ProviderKind::Anthropic),
        )
        .await?;
        assert_eq!(
            resolve_dispatch(&pool, None, Some("org/vllm"), WorkloadClass::Playbook).await?,
            None
        );
        Ok(())
    }

    /// The chain, one step at a time: platform, then domain over platform, then the launch override
    /// over both. Each step only moves the class and domain it names.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn resolution_prefers_the_narrowest_choice(pool: PgPool) -> anyhow::Result<()> {
        upsert(&pool, &new_provider("plat", ProviderKind::Anthropic)).await?;
        upsert(&pool, &new_provider("dom", ProviderKind::Vertex)).await?;
        upsert(&pool, &new_provider("pinned", ProviderKind::OpenAi)).await?;

        set_default(
            &pool,
            &DispatchDefault {
                scope_kind: DefaultScope::Platform,
                scope_ref: String::new(),
                workload_class: WorkloadClass::Autoresearch,
                provider_id: "plat".to_string(),
                model: None,
            },
        )
        .await?;
        let resolved = resolve_dispatch(&pool, None, Some("org/vllm"), WorkloadClass::Autoresearch)
            .await?
            .expect("the platform default answers");
        assert_eq!(resolved.provider.id, "plat");
        assert_eq!(
            resolved.model, "claude-opus-4-6",
            "a default naming no model takes the provider's own"
        );
        assert_eq!(resolved.harness, Harness::Claude);
        assert_eq!(
            resolve_dispatch(&pool, None, Some("org/vllm"), WorkloadClass::Playbook).await?,
            None,
            "a default is per workload class and reaches no other"
        );

        set_default(
            &pool,
            &DispatchDefault {
                scope_kind: DefaultScope::Domain,
                scope_ref: "org/vllm".to_string(),
                workload_class: WorkloadClass::Autoresearch,
                provider_id: "dom".to_string(),
                model: Some("claude-sonnet-5".to_string()),
            },
        )
        .await?;
        let resolved = resolve_dispatch(&pool, None, Some("org/vllm"), WorkloadClass::Autoresearch)
            .await?
            .expect("the domain default answers");
        assert_eq!(resolved.provider.id, "dom");
        assert_eq!(resolved.model, "claude-sonnet-5");
        assert_eq!(
            resolve_dispatch(&pool, None, Some("org/epp"), WorkloadClass::Autoresearch)
                .await?
                .expect("another domain still takes the platform default")
                .provider
                .id,
            "plat"
        );

        let over = DispatchOverride {
            provider_id: "pinned",
            model: Some("gpt-5.6-sol"),
        };
        let resolved = resolve_dispatch(
            &pool,
            Some(over),
            Some("org/vllm"),
            WorkloadClass::Autoresearch,
        )
        .await?
        .expect("the override answers");
        assert_eq!(resolved.provider.id, "pinned");
        assert_eq!(resolved.model, "gpt-5.6-sol");
        assert_eq!(
            resolved.harness,
            Harness::Codex,
            "an OpenAI provider renders the codex harness"
        );

        let bare = DispatchOverride {
            provider_id: "pinned",
            model: None,
        };
        assert_eq!(
            resolve_dispatch(
                &pool,
                Some(bare),
                Some("org/vllm"),
                WorkloadClass::Autoresearch
            )
            .await?
            .expect("provider without model")
            .model,
            "gpt-5.6-luna",
            "an override naming only a provider takes that provider's default model"
        );
        Ok(())
    }

    /// Disabling a provider hides it from the pickers, but work already pinned to it still
    /// resolves: turning one off must not strand what was committed to it.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_disabled_provider_still_resolves_for_a_pinned_launch(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let mut new = new_provider("retiring", ProviderKind::Anthropic);
        new.enabled = false;
        upsert(&pool, &new).await?;

        assert!(
            list(&pool, true).await?.is_empty(),
            "hidden from the pickers"
        );
        assert_eq!(list(&pool, false).await?.len(), 1);
        let over = DispatchOverride {
            provider_id: "retiring",
            model: None,
        };
        assert_eq!(
            resolve_dispatch(&pool, Some(over), None, WorkloadClass::Autoresearch)
                .await?
                .expect("still resolves")
                .provider
                .id,
            "retiring"
        );

        let gone = DispatchOverride {
            provider_id: "never-existed",
            model: None,
        };
        assert!(
            resolve_dispatch(&pool, Some(gone), None, WorkloadClass::Autoresearch)
                .await
                .is_err(),
            "a deregistered provider is an error, not a silent fall-through to the defaults"
        );
        Ok(())
    }

    /// The other half of disabling: a default is not a pin, so the work that inherits one is NEW
    /// work, and new work does not reach a provider an administrator turned off.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_default_naming_a_disabled_provider_takes_no_new_work(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        upsert(&pool, &new_provider("retiring", ProviderKind::Anthropic)).await?;
        set_default(
            &pool,
            &DispatchDefault {
                scope_kind: DefaultScope::Platform,
                scope_ref: String::new(),
                workload_class: WorkloadClass::Autoresearch,
                provider_id: "retiring".to_string(),
                model: None,
            },
        )
        .await?;
        assert!(
            resolve_dispatch(&pool, None, None, WorkloadClass::Autoresearch)
                .await?
                .is_some()
        );

        let mut off = new_provider("retiring", ProviderKind::Anthropic);
        off.enabled = false;
        upsert(&pool, &off).await?;
        let err = resolve_dispatch(&pool, None, None, WorkloadClass::Autoresearch)
            .await
            .expect_err("a disabled default is refused, not silently taken");
        assert!(format!("{err:#}").contains("disabled"), "{err:#}");
        Ok(())
    }

    /// A playbook dispatch resolves exactly like an autoresearch one: its platform default when
    /// the launch pinned nothing, and its pinned pair otherwise. The pair replaces the pack
    /// manifest's `[agent]` table through `crucible plan run --harness/--model`.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_playbook_dispatch_resolves_its_default_and_its_pin(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        upsert(&pool, &new_provider("plat", ProviderKind::OpenAi)).await?;
        upsert(&pool, &new_provider("vertex", ProviderKind::Vertex)).await?;
        set_default(
            &pool,
            &DispatchDefault {
                scope_kind: DefaultScope::Platform,
                scope_ref: String::new(),
                workload_class: WorkloadClass::Playbook,
                provider_id: "plat".to_string(),
                model: None,
            },
        )
        .await?;
        let inherited = resolve_dispatch(&pool, None, None, WorkloadClass::Playbook)
            .await?
            .expect("the platform default applies");
        assert_eq!(inherited.provider.id, "plat");
        assert_eq!(inherited.harness, Harness::Codex);

        let over = DispatchOverride {
            provider_id: "vertex",
            model: Some("claude-haiku-4-5"),
        };
        let pinned = resolve_dispatch(&pool, Some(over), None, WorkloadClass::Playbook)
            .await?
            .expect("the pin applies");
        assert_eq!(pinned.provider.id, "vertex");
        assert_eq!(pinned.model, "claude-haiku-4-5");
        assert_eq!(pinned.harness, Harness::Claude);
        Ok(())
    }

    /// The chain read off a real launch row: a playbook issue that pinned a provider resolves to
    /// it ahead of both the domain and the platform default, and the issue next to it that pinned
    /// nothing inherits the default. The pin is what lets a pack whose manifest says codex run
    /// there when the platform default is a Claude provider.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_playbook_row_pin_outranks_the_platform_default(pool: PgPool) -> anyhow::Result<()> {
        upsert(&pool, &new_provider("vertex", ProviderKind::Vertex)).await?;
        upsert(&pool, &new_provider("openai", ProviderKind::OpenAi)).await?;
        for (scope_kind, scope_ref) in [
            (DefaultScope::Platform, ""),
            (DefaultScope::Domain, "org/vllm"),
        ] {
            set_default(
                &pool,
                &DispatchDefault {
                    scope_kind,
                    scope_ref: scope_ref.to_string(),
                    workload_class: WorkloadClass::Playbook,
                    provider_id: "vertex".to_string(),
                    model: None,
                },
            )
            .await?;
        }
        for key in ["playbook:survey:pinned", "playbook:survey:bare"] {
            sqlx::query(
                "INSERT INTO issues (key, repo, tier, status, priority, input_kind, title, \
                 updated_at) VALUES ($1, 'org/vllm', 'T1', 'new', 0, 'playbook', 'launch', \
                 '2026-09-01T00:00:00Z')",
            )
            .bind(key)
            .execute(&pool)
            .await?;
        }
        crate::issues::store::set_agent_dispatch(
            &pool,
            "playbook:survey:pinned",
            Some("openai"),
            Some("gpt-5.6-luna"),
        )
        .await?;

        let pinned = crate::issues::store::get_issue(&pool, "playbook:survey:pinned")
            .await?
            .expect("issue row");
        let resolved = resolve_for_issue(&pool, &pinned, WorkloadClass::Playbook)
            .await?
            .expect("the pin answers");
        assert_eq!(resolved.provider.id, "openai");
        assert_eq!(resolved.model, "gpt-5.6-luna");
        assert_eq!(resolved.harness, Harness::Codex);
        assert_eq!(
            AgentSelection::from_resolved(Some(&resolved)),
            AgentSelection {
                harness: Some(Harness::Codex),
                model: Some("gpt-5.6-luna".to_string()),
            }
        );

        let bare = crate::issues::store::get_issue(&pool, "playbook:survey:bare")
            .await?
            .expect("issue row");
        let inherited = resolve_for_issue(&pool, &bare, WorkloadClass::Playbook)
            .await?
            .expect("the default answers");
        assert_eq!(inherited.provider.id, "vertex");
        assert_eq!(inherited.harness, Harness::Claude);
        Ok(())
    }

    /// Registering is insert-or-refuse, not upsert: the id check and the write are one statement,
    /// so a second registration under a live id cannot silently replace the first.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn registering_a_taken_id_does_not_replace_it(pool: PgPool) -> anyhow::Result<()> {
        assert!(insert(&pool, &new_provider("plat", ProviderKind::OpenAi)).await?);
        let mut second = new_provider("plat", ProviderKind::Anthropic);
        second.display_name = "Somebody Else";
        assert!(!insert(&pool, &second).await?, "the id was taken");
        let stored = get(&pool, "plat").await?.expect("row");
        assert_eq!(stored.kind, ProviderKind::OpenAi);
        assert_eq!(stored.display_name, "Some Provider");
        Ok(())
    }

    /// Deregistering a provider work still names would strand that work at resolution, so the
    /// registry reports what holds it.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn usage_reports_what_still_pins_a_provider(pool: PgPool) -> anyhow::Result<()> {
        upsert(&pool, &new_provider("plat", ProviderKind::OpenAi)).await?;
        assert!(usage(&pool, "plat").await?.is_empty());

        crate::issues::store::upsert_issue(
            &pool,
            &crate::issues::model::NewIssue {
                key: "owner/repo#1".to_string(),
                repo: "owner/repo".to_string(),
                priority: 0,
                evidence_url: None,
                title: None,
                author: None,
                body: None,
                labels: Vec::new(),
                upstream_updated_at: None,
            },
        )
        .await?;
        crate::issues::store::set_agent_dispatch(&pool, "owner/repo#1", Some("plat"), None).await?;
        let usage = usage(&pool, "plat").await?;
        assert_eq!(usage.issue_count, 1);
        assert_eq!(usage.issues, ["owner/repo#1"]);
        assert_eq!(usage.schedule_count, 0);
        Ok(())
    }

    /// Every model name that reaches a pod is one argv word inside a `/bin/sh -c` wrapper, so the
    /// vocabulary stops at what a model is actually called.
    #[test]
    fn a_model_name_cannot_carry_shell_syntax() {
        for good in [
            "gpt-5.6-luna",
            " claude-opus-4-6 ",
            "us.anthropic.claude-sonnet-5-v2:0",
            "publishers/anthropic/models/claude@20260101",
        ] {
            assert_eq!(check_model_name(good).expect("valid"), good.trim());
        }
        for bad in [
            "",
            "   ",
            "gpt; rm -rf /",
            "gpt$(id)",
            "gpt`id`",
            "gpt luna",
            "gpt&&id",
            "--iterations=99",
            "gpt\n--model=x",
        ] {
            assert!(check_model_name(bad).is_err(), "{bad:?}");
        }
        assert!(check_model_name(&"a".repeat(MODEL_NAME_MAX_LEN + 1)).is_err());
    }

    /// Deregistering a provider takes the defaults pointing at it, so no default can outlive what
    /// it names.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn deleting_a_provider_clears_its_defaults(pool: PgPool) -> anyhow::Result<()> {
        upsert(&pool, &new_provider("plat", ProviderKind::OpenAi)).await?;
        let row = DispatchDefault {
            scope_kind: DefaultScope::Platform,
            scope_ref: String::new(),
            workload_class: WorkloadClass::Playbook,
            provider_id: "plat".to_string(),
            model: None,
        };
        set_default(&pool, &row).await?;
        assert_eq!(list_defaults(&pool).await?, vec![row]);

        assert!(delete(&pool, "plat").await?);
        assert!(list_defaults(&pool).await?.is_empty());
        assert_eq!(
            resolve_dispatch(&pool, None, None, WorkloadClass::Playbook).await?,
            None
        );
        assert!(!delete(&pool, "plat").await?);
        Ok(())
    }

    /// A registration that names no models takes the kind's curated list, and `clear_default`
    /// puts a scope back to inheriting.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_bare_registration_takes_the_curated_list(pool: PgPool) -> anyhow::Result<()> {
        upsert(&pool, &new_provider("openai", ProviderKind::OpenAi)).await?;
        let provider = get(&pool, "openai").await?.expect("row");
        assert_eq!(provider.models, ["gpt-5.6-luna", "gpt-5.6-sol"]);
        assert_eq!(provider.default_model, "gpt-5.6-luna");

        let row = DispatchDefault {
            scope_kind: DefaultScope::Domain,
            scope_ref: "org/vllm".to_string(),
            workload_class: WorkloadClass::Autoresearch,
            provider_id: "openai".to_string(),
            model: None,
        };
        set_default(&pool, &row).await?;
        assert!(
            resolve_dispatch(&pool, None, Some("org/vllm"), WorkloadClass::Autoresearch)
                .await?
                .is_some()
        );
        assert!(
            clear_default(
                &pool,
                DefaultScope::Domain,
                "org/vllm",
                WorkloadClass::Autoresearch
            )
            .await?
        );
        assert_eq!(
            resolve_dispatch(&pool, None, Some("org/vllm"), WorkloadClass::Autoresearch).await?,
            None
        );
        Ok(())
    }

    /// A stored pair is an override only when it names a provider; a stray model is not a choice.
    #[test]
    fn an_override_needs_a_provider() {
        assert_eq!(
            DispatchOverride::from_columns(Some("plat"), Some("m")),
            Some(DispatchOverride {
                provider_id: "plat",
                model: Some("m")
            })
        );
        assert_eq!(
            DispatchOverride::from_columns(Some("plat"), Some("  ")),
            Some(DispatchOverride {
                provider_id: "plat",
                model: None
            })
        );
        assert_eq!(DispatchOverride::from_columns(None, Some("m")), None);
        assert_eq!(DispatchOverride::from_columns(Some("  "), None), None);
    }
}
