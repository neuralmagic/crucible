//! LLM-assisted triage ranking: the *only* source of an issue's tier — one bounded ranking call per
//! issue, cached by content hash. There is no heuristic prior to confirm or override (a keyword
//! classifier was built and then deleted; see [`crate::issues::triage`]'s module doc). This module is the
//! pure/async half — build the prompt, run the bounded call, parse the verdict strictly;
//! [`crate::issues::reconcile`] is the half that decides *when* to call it (cache-by-content-hash + cap
//! checks) and applies the result.
//!
//! **No subprocess, by design.** The controller runs in the loop image, which does not carry
//! the `claude` CLI — that binary lives only in the sandbox image `openshell` launches for an
//! agent turn. A `claude -p` shell-out (or a command-override standing in for one) works on a
//! laptop and breaks in-pod. Ranking is therefore an in-process call through the [`genai`]
//! multi-provider client: one `exec_chat` interface covering both arms, dispatched by a
//! [`genai::resolver::ServiceTargetResolver`] from the same env contract as before.
//!
//! - **Vertex (default):** `genai`'s `vertex` adapter speaks Anthropic's native Messages shape
//!   against `.../publishers/anthropic/models/{model}:rawPredict`, for the model in
//!   `CONTROLLER_RANKER_MODEL` (a genai model string, default `vertex::claude-sonnet-5`; the
//!   `vertex::` namespace picks the adapter, the bare `claude-*` remainder is the Vertex model
//!   ID). Project/region come from `ANTHROPIC_VERTEX_PROJECT_ID` / `CLOUD_ML_REGION` — the same
//!   env the agent uses; the resolver builds the endpoint from them rather than genai's own
//!   `VERTEX_PROJECT_ID`/`VERTEX_LOCATION` so our contract stays stable. Auth is a
//!   `gcp_auth`-minted `cloud-platform` access token (genai's vertex adapter expects a
//!   pre-minted bearer, it does no ADC itself) — the same credential resolution
//!   `crucible::openshell::provider::mint_vertex_token` uses for headless agent turns (ADC: a
//!   service-account key, a user refresh token, or the in-cluster metadata server) — Vertex per
//!   house rule, no baked credential.
//! - **Custom endpoint:** `CONTROLLER_RANKER_API_URL` (base override) + `CONTROLLER_RANKER_TOKEN`
//!   (static bearer, skips gcp_auth) point the same code at any OpenAI-compatible
//!   chat-completions server (`POST {base}/chat/completions`) — a vLLM/llm-d endpoint later,
//!   `wiremock` in tests — via genai's OpenAI adapter.
//!
//! This module is the **API tier** — the in-process genai call. The **grounded/openshell tier** (the
//! escalation for `low`-[`Confidence`] or backend-pinned verdicts) is realized outside this module:
//! [`crate::issues::reconcile`] shells the engine binary's `rank-grounded` subcommand (via
//! [`crate::issues::engine::rank_grounded`]) against a maintained checkout, so the ranker turn can grep the
//! code before tiering instead of judging from the issue text alone. That path lives at the
//! engine-subprocess boundary, not through this genai client — the two tiers differ only in what
//! context the model sees. The verdict's [`Confidence`] field is what routes between them.
//!
//! Test seam: `CONTROLLER_RANKER_API_URL` pointed at a `wiremock` server serving a canned
//! chat-completions response (the [`crate::issues::triage`] `GITHUB_API_URL` pattern). GCP token minting
//! only happens on the default Vertex base, so tests need no ADC/GCP credentials at all.
//!
//! ## Ranking-call knobs (env)
//!
//! All plain `std::env::var` reads with a default, matching `CONTROLLER_RANKER_MODEL`:
//!
//! - `CONTROLLER_RANKER_MODEL` — the genai model string (default `vertex::claude-sonnet-5`).
//! - `CONTROLLER_RANKER_API_URL` / `CONTROLLER_RANKER_TOKEN` — the OpenAI-compatible override arm.
//! - `CONTROLLER_RANKER_MAX_TOKENS` — the response cap (default [`DEFAULT_MAX_TOKENS`]). This is a
//!   **thinking-plus-text** budget on Claude, not a text-only one: Sonnet 5 runs adaptive thinking
//!   by default and there is no way to turn it off through genai 0.6.5's *Vertex* Anthropic path
//!   (its request builder never emits a `thinking` object, so `ChatOptions::reasoning_effort` is a
//!   no-op on Vertex — only genai's direct Anthropic-API adapter honors it). With the old 300-token
//!   cap the model spent the whole budget thinking (`stop_reason=max_tokens`, ~299 thinking tokens,
//!   zero text blocks), so `first_text()` failed and the issue stayed tier-NULL. The default is
//!   raised so adaptive thinking *and* the one-line JSON verdict both fit; a malformed value falls
//!   back to the default with a warning.
//! - `CONTROLLER_RANKER_TEMPERATURE` — optional, no default. Left off the request when unset,
//!   because Sonnet 5 rejects any non-default `temperature`/`top_p`/`top_k` with a 400.

#![allow(clippy::disallowed_macros)]

use crate::wire_enum::wire_enum;
use anyhow::{Context, Result, bail};
use crucible_contract::Tier;
use genai::adapter::AdapterKind;
use genai::chat::{ChatOptions, ChatRequest};
use genai::resolver::{AuthData, Endpoint, ServiceTargetResolver};
use genai::{Client, ModelIden, ServiceTarget};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const RANK_PROMPT: &str = include_str!("prompts/rank-prompt.md");

/// Total attempts for one ranking call: the initial try plus one bounded retry on a malformed
/// verdict. Never more — a wedged ranker must not stall reconcile.
const MAX_ATTEMPTS: u32 = 2;

/// Vertex needs a `cloud-platform`-scoped OAuth2 access token (matches
/// `crucible::openshell::provider::mint_vertex_token`'s scope).
const VERTEX_SCOPES: &[&str] = &["https://www.googleapis.com/auth/cloud-platform"];

/// The default response cap (`CONTROLLER_RANKER_MAX_TOKENS`). A thinking-plus-text budget on
/// Claude: room for adaptive thinking *and* the one-line JSON verdict. Raised from an earlier 300
/// that adaptive thinking exhausted on its own, leaving zero text blocks (see the module doc).
const DEFAULT_MAX_TOKENS: u32 = 2048;

/// How sure the model was of its own verdict. `low` is what escalates to the code-grounded ranking
/// tier ([`crate::issues::reconcile`] shelling [`crate::issues::engine::rank_grounded`]); the field is on the verdict
/// JSON so that routing needs no schema change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
pub enum Confidence {
    High,
    Low,
}

wire_enum!(Confidence, "confidence", both, {
    Confidence::High => "high",
    Confidence::Low => "low",
});

/// Whether the issue is the kind of work the performance-optimization loop wants at all —
/// decided by the same ranking call as the tier, on the issue text alone (a topical judgment,
/// unlike the tier it never needs code grounding). `Unrelated` parks the issue before any
/// grounded escalation or scope turn spends on it; the tier axis alone can't do that, because
/// nearly every well-formed bug in a tested repo is honestly T0.
#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::EnumIter)]
#[non_exhaustive]
pub enum Affinity {
    Perf,
    PerfAdjacent,
    Unrelated,
}

wire_enum!(Affinity, "affinity", both, {
    Affinity::Perf => "perf",
    Affinity::PerfAdjacent => "perf-adjacent",
    Affinity::Unrelated => "unrelated",
});

/// The model's ranking verdict.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub(crate) tier: Tier,
    pub(crate) affinity: Affinity,
    pub(crate) rationale: String,
    /// The call's self-reported cost: the verdict JSON's own `cost_usd` if present, else a USD
    /// figure off the response's `usage` object. `None` (Vertex and vLLM report token counts,
    /// not USD) means the caller falls back to a configured estimate.
    pub(crate) cost_usd: Option<f64>,
    /// Defaults to `high` when the verdict JSON omits the field (older/simpler test doubles).
    pub(crate) confidence: Confidence,
}

/// The wire shape of the verdict JSON (stage 2's simplified schema — the fuller
/// scoring axes in `analyze/rank-prompt.md` are the `analyze/` pipeline's own concern, not this
/// stage's).
#[derive(Debug, Deserialize)]
struct RawVerdict {
    tier: String,
    affinity: String,
    rationale: String,
    #[serde(default)]
    cost_usd: Option<f64>,
    #[serde(default)]
    confidence: Option<String>,
}

/// The outcome of a bounded ranking call: either a parsed verdict, or a ranking *failure*
/// (malformed output after every attempt) — never an [`anyhow::Error`], because a ranker
/// malfunction must never bubble up and block or park the issue behind it.
#[derive(Debug, Clone, PartialEq)]
pub enum RankOutcome {
    Verdict(Verdict),
    Failed(String),
}

/// The cache key for stage 2 (a hash of an issue's title, body, and labels): unchanged content
/// means an unchanged verdict, so a re-poll with the same hash never re-spends. Labels are sorted
/// first so a re-fetch that merely reorders them still hits the cache.
pub(crate) fn content_hash(title: &str, body: &str, labels: &[String]) -> String {
    let mut sorted_labels = labels.to_vec();
    sorted_labels.sort();
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update(b"\n");
    hasher.update(body.as_bytes());
    hasher.update(b"\n");
    hasher.update(sorted_labels.join(",").as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Fill the embedded prompt with one issue's identity (the `scope.rs` `{{PLACEHOLDER}}` pattern).
fn render_prompt(title: &str, body: &str, labels: &[String]) -> String {
    RANK_PROMPT
        .replace("{{TITLE}}", title)
        .replace("{{LABELS}}", &labels.join(", "))
        .replace("{{BODY}}", body)
}

fn vertex_project() -> String {
    std::env::var("ANTHROPIC_VERTEX_PROJECT_ID").unwrap_or_default()
}

fn vertex_region() -> String {
    std::env::var("CLOUD_ML_REGION").unwrap_or_else(|_| "global".to_string())
}

/// The ranker's genai model string. The `vertex::` namespace selects genai's Vertex adapter;
/// the bare remainder is the Vertex model ID (current-generation Claude models use the bare
/// first-party ID on Vertex). Sonnet, not Opus, by design: the ranking call is a bounded
/// classification (a tier verdict plus one paragraph of rationale), not agentic coding —
/// cost/latency fit beats flagship capability, the knob is configurable, and a low-confidence
/// verdict already has a designed escalation path (the code-grounded tier) rather than
/// needing a smarter first-pass model.
fn ranker_model() -> String {
    std::env::var("CONTROLLER_RANKER_MODEL")
        .unwrap_or_else(|_| "vertex::claude-sonnet-5".to_string())
}

/// The response cap for one ranking call (`CONTROLLER_RANKER_MAX_TOKENS`, default
/// [`DEFAULT_MAX_TOKENS`]). A malformed value falls back to the default with a warning rather than
/// failing the call — a mistyped env var must not wedge the ranker.
fn ranker_max_tokens() -> u32 {
    match std::env::var("CONTROLLER_RANKER_MAX_TOKENS") {
        Ok(raw) => match raw.trim().parse::<u32>() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    value = %raw,
                    error = %e,
                    "CONTROLLER_RANKER_MAX_TOKENS is not a u32; using the default {DEFAULT_MAX_TOKENS}"
                );
                DEFAULT_MAX_TOKENS
            }
        },
        Err(_) => DEFAULT_MAX_TOKENS,
    }
}

/// The optional sampling temperature (`CONTROLLER_RANKER_TEMPERATURE`). `None` when unset or
/// malformed, in which case it is omitted from the request entirely — Sonnet 5 rejects any
/// non-default `temperature`/`top_p`/`top_k` with a 400, so an absent knob must mean absent, not
/// zero.
fn ranker_temperature() -> Option<f64> {
    let raw = std::env::var("CONTROLLER_RANKER_TEMPERATURE").ok()?;
    match raw.trim().parse::<f64>() {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(
                value = %raw,
                error = %e,
                "CONTROLLER_RANKER_TEMPERATURE is not an f64; leaving temperature unset"
            );
            None
        }
    }
}

/// The Vertex AI base URL for a project/region. `global` uses the un-prefixed host (Google's
/// convention, and genai's); any other region uses the region-prefixed one. Trailing slash
/// because genai's adapters `Url::join` their path suffix onto the base.
fn vertex_base(project: &str, region: &str) -> String {
    if region == "global" {
        format!("https://aiplatform.googleapis.com/v1/projects/{project}/locations/global/")
    } else {
        format!(
            "https://{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/"
        )
    }
}

/// Normalize a `CONTROLLER_RANKER_API_URL` override into a genai endpoint base. genai's OpenAI
/// adapter appends `chat/completions` with `Url::join`, whose RFC 3986 semantics *replace* the
/// last path segment unless the base ends in `/` — so `http://vllm:8000/v1` would silently
/// become `http://vllm:8000/chat/completions` without this.
fn custom_base(url: &str) -> String {
    format!("{}/", url.trim_end_matches('/'))
}

/// Rewrite genai's default [`ServiceTarget`] to our env contract — the one place both arms
/// diverge from genai's own defaults:
///
/// - `CONTROLLER_RANKER_API_URL` set: any OpenAI-compatible chat-completions server (vLLM/llm-d,
///   `wiremock` in tests). Adapter forced to OpenAI; bearer is `CONTROLLER_RANKER_TOKEN` (empty
///   when unset — an in-cluster endpoint typically has no auth and ignores the header).
/// - Otherwise: Vertex. Endpoint built from `ANTHROPIC_VERTEX_PROJECT_ID` + `CLOUD_ML_REGION`
///   (not genai's `VERTEX_PROJECT_ID`/`VERTEX_LOCATION`), bearer minted from ADC via `gcp_auth`.
///   GCP token minting only happens on this arm, so tests need no ADC/GCP credentials at all.
async fn resolve_target(target: ServiceTarget) -> genai::resolver::Result<ServiceTarget> {
    if let Ok(url) = std::env::var("CONTROLLER_RANKER_API_URL")
        && !url.is_empty()
    {
        let token = std::env::var("CONTROLLER_RANKER_TOKEN").unwrap_or_default();
        return Ok(ServiceTarget {
            endpoint: Endpoint::from_owned(custom_base(&url)),
            auth: AuthData::from_single(token),
            model: ModelIden::new(AdapterKind::OpenAI, target.model.model_name),
        });
    }
    let token = mint_vertex_token()
        .await
        .map_err(|e| genai::resolver::Error::Custom(format!("{e:#}")))?;
    Ok(ServiceTarget {
        endpoint: Endpoint::from_owned(vertex_base(&vertex_project(), &vertex_region())),
        auth: AuthData::from_single(token),
        model: ModelIden::new(AdapterKind::Vertex, target.model.model_name),
    })
}

/// Mint a `cloud-platform`-scoped Vertex access token from ADC (genai's vertex adapter expects
/// a pre-minted bearer; it does not resolve ADC itself).
async fn mint_vertex_token() -> Result<String> {
    let provider = gcp_auth::provider().await.context(
        "resolve GCP ADC for the ranker (run `gcloud auth application-default login`, or set \
         GOOGLE_APPLICATION_CREDENTIALS to a service-account key)",
    )?;
    let token = provider
        .token(VERTEX_SCOPES)
        .await
        .context("mint a cloud-platform-scoped Vertex access token for the ranker")?;
    Ok(token.as_str().to_string())
}

/// Run one bounded ranking call for an issue: up to [`MAX_ATTEMPTS`] tries.
pub(crate) async fn rank(title: &str, body: &str, labels: &[String]) -> RankOutcome {
    let prompt = render_prompt(title, body, labels);
    let mut last_err = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        match run_once(&prompt).await {
            Ok(verdict) => return RankOutcome::Verdict(verdict),
            Err(e) => last_err = format!("attempt {attempt}/{MAX_ATTEMPTS}: {e:#}"),
        }
    }
    RankOutcome::Failed(last_err)
}

/// One attempt: call the API tier, then parse its output strictly.
async fn run_once(prompt: &str) -> Result<Verdict> {
    let (text, envelope_cost) = call_ranker(prompt).await?;
    parse_verdict(&text, envelope_cost)
}

/// Build the ranking call's [`ChatOptions`] from the env knobs: the (raised, thinking-inclusive)
/// max-tokens cap, raw-body capture for the self-reported cost, and an *optional* temperature that
/// is set only when `CONTROLLER_RANKER_TEMPERATURE` is present and parseable (Sonnet 5 400s on a
/// non-default one, so unset must stay off the request).
fn build_chat_options() -> ChatOptions {
    let options = ChatOptions::default()
        .with_max_tokens(ranker_max_tokens())
        .with_capture_raw_body(true);
    match ranker_temperature() {
        Some(t) => options.with_temperature(t),
        None => options,
    }
}

/// Run one ranking prompt through genai's `exec_chat` and return the model's raw text response
/// (the verdict JSON, not yet parsed) plus the response's self-reported cost, if its raw `usage`
/// object carried one. The raw body is captured because genai's normalized `Usage` is token
/// counts only — the `usage.cost`/`cost_usd` some OpenAI-compatible servers report never
/// survives normalization.
async fn call_ranker(prompt: &str) -> Result<(String, Option<f64>)> {
    let model = ranker_model();
    type TargetFuture = std::pin::Pin<
        Box<dyn std::future::Future<Output = genai::resolver::Result<ServiceTarget>> + Send>,
    >;
    let client = Client::builder()
        .with_service_target_resolver(ServiceTargetResolver::from_resolver_async_fn(
            |target: ServiceTarget| -> TargetFuture { Box::pin(resolve_target(target)) },
        ))
        .build();
    let options = build_chat_options();
    let res = client
        .exec_chat(&*model, ChatRequest::from_user(prompt), Some(&options))
        .await
        .with_context(|| format!("ranking call via genai (model `{model}`)"))?;
    let envelope_cost = res.captured_raw_body.as_ref().and_then(extract_usage_cost);
    let text = res
        .first_text()
        .with_context(|| format!("no text content in the ranking response: {res:?}"))?
        .to_string();
    Ok((text, envelope_cost))
}

/// A USD cost from the response's `usage` object, if the server reports one (`usage.cost` or
/// `usage.cost_usd` — some OpenAI-compatible servers do; Vertex and vLLM report token counts
/// only, in which case the caller falls back to the configured estimate).
fn extract_usage_cost(payload: &serde_json::Value) -> Option<f64> {
    let usage = payload.get("usage")?;
    usage
        .get("cost_usd")
        .and_then(|c| c.as_f64())
        .or_else(|| usage.get("cost").and_then(|c| c.as_f64()))
}

/// Strip a markdown code fence, if the model wrapped its JSON in one (`analyze/rank.nu`'s
/// `parse-verdict` tolerates the same slop).
fn strip_fence(s: &str) -> &str {
    let s = s.trim();
    let s = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

/// Parse a verdict strictly: an unknown tier/affinity/confidence value, or a missing
/// `tier`/`affinity`/`rationale` field, is a parse error (a ranking failure per the caller), never
/// a silent default. `affinity` is required rather than defaulted because it is the noise gate —
/// a model that omits it must fail loudly, not pass as `perf`. An absent `confidence` defaults to
/// `high` (older/simpler test doubles never set it). The verdict's own `cost_usd` wins over
/// `envelope_cost` (the response `usage` object's figure).
fn parse_verdict(raw: &str, envelope_cost: Option<f64>) -> Result<Verdict> {
    let cleaned = strip_fence(raw);
    let v: RawVerdict = serde_json::from_str(cleaned)
        .with_context(|| format!("verdict is not the expected JSON shape: {cleaned:?}"))?;
    if v.rationale.trim().is_empty() {
        bail!("verdict rationale is empty");
    }
    let tier = Tier::parse(&v.tier)?;
    let affinity = Affinity::parse(&v.affinity)?;
    let confidence = match v.confidence {
        Some(c) => Confidence::parse(&c)?,
        None => Confidence::High,
    };
    Ok(Verdict {
        tier,
        affinity,
        rationale: v.rationale,
        cost_usd: v.cost_usd.or(envelope_cost),
        confidence,
    })
}

#[cfg(test)]
// The crate-wide `ENV_LOCK` (an async mutex) is held across the tests below that mutate
// `CONTROLLER_RANKER_API_URL` — see the note on `crate::ENV_LOCK`.
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_stable_and_order_independent_over_labels() {
        let a = content_hash("t", "b", &["x".to_string(), "y".to_string()]);
        let b = content_hash("t", "b", &["y".to_string(), "x".to_string()]);
        assert_eq!(a, b, "label order must not change the cache key");

        let c = content_hash("t", "different body", &["x".to_string(), "y".to_string()]);
        assert_ne!(a, c, "a changed body must change the cache key");
    }

    #[test]
    fn render_prompt_fills_all_placeholders() {
        let p = render_prompt(
            "My Title",
            "My Body",
            &["bug".to_string(), "perf".to_string()],
        );
        assert!(p.contains("My Title"));
        assert!(p.contains("My Body"));
        assert!(p.contains("bug, perf"));
        assert!(!p.contains("{{"));
    }

    #[test]
    fn parse_verdict_accepts_a_clean_object() {
        let v = parse_verdict(
            r#"{"tier":"T1","affinity":"perf","rationale":"needs a new benchmark"}"#,
            None,
        )
        .expect("parses");
        assert_eq!(v.tier, Tier::T1);
        assert_eq!(v.rationale, "needs a new benchmark");
        assert_eq!(v.cost_usd, None);
        assert_eq!(v.confidence, Confidence::High);
    }

    #[test]
    fn parse_verdict_strips_a_markdown_fence() {
        let v = parse_verdict(
            "```json\n{\"tier\":\"T0\",\"affinity\":\"perf-adjacent\",\"rationale\":\"has a failing test\"}\n```",
            None,
        )
        .expect("parses through the fence");
        assert_eq!(v.tier, Tier::T0);
    }

    #[test]
    fn parse_verdict_prefers_its_own_cost_over_the_usage_figure() {
        let v = parse_verdict(
            r#"{"tier":"N","affinity":"unrelated","rationale":"design discussion","cost_usd":0.01}"#,
            Some(0.5),
        )
        .expect("parses");
        assert_eq!(v.cost_usd, Some(0.01));

        let v2 = parse_verdict(
            r#"{"tier":"N","affinity":"unrelated","rationale":"design discussion"}"#,
            Some(0.5),
        )
        .expect("parses");
        assert_eq!(v2.cost_usd, Some(0.5), "falls back to the usage cost");
    }

    #[test]
    fn parse_verdict_reads_an_explicit_low_confidence() {
        let v = parse_verdict(
            r#"{"tier":"T1","affinity":"perf","rationale":"x","confidence":"low"}"#,
            None,
        )
        .expect("parses");
        assert_eq!(v.confidence, Confidence::Low);
    }

    #[test]
    fn parse_verdict_rejects_an_unknown_confidence() {
        let err = parse_verdict(
            r#"{"tier":"T1","affinity":"perf","rationale":"x","confidence":"medium"}"#,
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("medium") || err.to_string().contains("unknown confidence")
        );
    }

    #[test]
    fn parse_verdict_accepts_t3() {
        let v = parse_verdict(
            r#"{"tier":"T3","affinity":"perf","rationale":"needs a composite live rig"}"#,
            None,
        )
        .expect("parses");
        assert_eq!(v.tier, Tier::T3);
    }

    #[test]
    fn parse_verdict_reads_each_affinity_value() {
        for (raw, want) in [
            ("perf", Affinity::Perf),
            ("perf-adjacent", Affinity::PerfAdjacent),
            ("unrelated", Affinity::Unrelated),
        ] {
            let v = parse_verdict(
                &format!(r#"{{"tier":"T0","affinity":"{raw}","rationale":"x"}}"#),
                None,
            )
            .expect("parses");
            assert_eq!(v.affinity, want);
        }
    }

    #[test]
    fn parse_verdict_rejects_a_missing_affinity() {
        // Affinity is the noise gate: a verdict without it must be a ranking failure, never a
        // silent pass-through as `perf`.
        assert!(parse_verdict(r#"{"tier":"T0","rationale":"x"}"#, None).is_err());
    }

    #[test]
    fn parse_verdict_rejects_an_unknown_affinity() {
        let err = parse_verdict(
            r#"{"tier":"T0","affinity":"tangential","rationale":"x"}"#,
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("tangential") || err.to_string().contains("unknown affinity")
        );
    }

    #[test]
    fn parse_verdict_rejects_an_unknown_tier() {
        let err =
            parse_verdict(r#"{"tier":"T4","affinity":"perf","rationale":"x"}"#, None).unwrap_err();
        assert!(err.to_string().contains("T4") || err.to_string().contains("unknown tier"));
    }

    #[test]
    fn parse_verdict_rejects_a_missing_field() {
        assert!(
            parse_verdict(r#"{"tier":"T0"}"#, None).is_err(),
            "missing rationale"
        );
        assert!(
            parse_verdict(r#"{"rationale":"x"}"#, None).is_err(),
            "missing tier"
        );
    }

    #[test]
    fn parse_verdict_rejects_an_empty_rationale() {
        assert!(parse_verdict(r#"{"tier":"T0","affinity":"perf","rationale":""}"#, None).is_err());
    }

    #[test]
    fn parse_verdict_rejects_non_json() {
        assert!(parse_verdict("not json at all", None).is_err());
    }

    #[test]
    fn extract_usage_cost_reads_cost_usd_or_cost() {
        let with_cost = serde_json::json!({"usage": {"prompt_tokens": 10, "cost": 0.02}});
        assert_eq!(extract_usage_cost(&with_cost), Some(0.02));
        let tokens_only =
            serde_json::json!({"usage": {"prompt_tokens": 10, "completion_tokens": 5}});
        assert_eq!(extract_usage_cost(&tokens_only), None);
        assert_eq!(extract_usage_cost(&serde_json::json!({})), None);
    }

    #[test]
    fn vertex_base_prefixes_the_region_except_global() {
        assert_eq!(
            vertex_base("proj-x", "us-east5"),
            "https://us-east5-aiplatform.googleapis.com/v1/projects/proj-x/locations/us-east5/"
        );
        assert_eq!(
            vertex_base("proj-x", "global"),
            "https://aiplatform.googleapis.com/v1/projects/proj-x/locations/global/"
        );
    }

    #[test]
    fn custom_base_always_ends_with_one_slash() {
        // genai `Url::join`s `chat/completions` onto the base; without the trailing slash the
        // `/v1` segment would be silently replaced.
        assert_eq!(
            custom_base("http://vllm.local:8000/v1"),
            "http://vllm.local:8000/v1/"
        );
        assert_eq!(
            custom_base("http://vllm.local:8000/v1/"),
            "http://vllm.local:8000/v1/"
        );
        assert_eq!(
            custom_base("http://127.0.0.1:9999"),
            "http://127.0.0.1:9999/"
        );
    }

    #[test]
    fn ranker_model_defaults_to_sonnet_on_vertex() {
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_MODEL");
        }
        assert_eq!(ranker_model(), "vertex::claude-sonnet-5");
        unsafe {
            std::env::set_var("CONTROLLER_RANKER_MODEL", "openai::my-vllm-model");
        }
        assert_eq!(ranker_model(), "openai::my-vllm-model");
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_MODEL");
        }
    }

    #[test]
    fn max_tokens_defaults_when_unset() {
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_MAX_TOKENS");
        }
        assert_eq!(ranker_max_tokens(), DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn max_tokens_reads_an_override() {
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::set_var("CONTROLLER_RANKER_MAX_TOKENS", "4096");
        }
        assert_eq!(ranker_max_tokens(), 4096);
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_MAX_TOKENS");
        }
    }

    #[test]
    fn max_tokens_falls_back_on_a_malformed_value() {
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::set_var("CONTROLLER_RANKER_MAX_TOKENS", "not-a-number");
        }
        assert_eq!(ranker_max_tokens(), DEFAULT_MAX_TOKENS);
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_MAX_TOKENS");
        }
    }

    #[test]
    fn temperature_is_none_when_unset() {
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_TEMPERATURE");
        }
        assert_eq!(ranker_temperature(), None);
    }

    #[test]
    fn temperature_reads_a_set_value() {
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::set_var("CONTROLLER_RANKER_TEMPERATURE", "0.2");
        }
        assert_eq!(ranker_temperature(), Some(0.2));
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_TEMPERATURE");
        }
    }

    #[test]
    fn temperature_falls_back_to_none_on_a_malformed_value() {
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::set_var("CONTROLLER_RANKER_TEMPERATURE", "warm");
        }
        assert_eq!(ranker_temperature(), None);
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_TEMPERATURE");
        }
    }

    #[test]
    fn chat_options_omits_temperature_when_unset() {
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_TEMPERATURE");
        }
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_MAX_TOKENS");
        }
        let opts = build_chat_options();
        assert_eq!(opts.max_tokens, Some(DEFAULT_MAX_TOKENS));
        assert_eq!(
            opts.temperature, None,
            "temperature must be absent from the request when unset (Sonnet 5 400s otherwise)"
        );
    }

    #[test]
    fn chat_options_carries_temperature_when_set() {
        let _g = crate::ENV_LOCK.blocking_lock();
        unsafe {
            std::env::set_var("CONTROLLER_RANKER_TEMPERATURE", "0.7");
        }
        let opts = build_chat_options();
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_TEMPERATURE");
        }
        assert_eq!(opts.temperature, Some(0.7));
    }

    /// A real HTTP double (`wiremock`, no in-process mock) exercised end to end through
    /// [`rank`] — the actual seam the reconcile tests drive via `CONTROLLER_RANKER_API_URL`.
    #[tokio::test]
    async fn rank_via_api_override_confirms_a_tier() {
        let _g = crate::ENV_LOCK.lock().await;
        let server = wiremock::MockServer::start().await;
        // The path matcher proves the custom-endpoint arm speaks the OpenAI chat-completions
        // shape at `{base}/chat/completions` (the URL-join contract `custom_base` protects).
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": "{\"tier\":\"T1\",\"affinity\":\"perf\",\"rationale\":\"confirmed\"}"}}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 20}
            })))
            .expect(1)
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("CONTROLLER_RANKER_API_URL", server.uri());
        }
        let outcome = rank("title", "body", &["performance".to_string()]).await;
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_API_URL");
        }

        match outcome {
            RankOutcome::Verdict(v) => {
                assert_eq!(v.tier, Tier::T1);
                assert_eq!(v.rationale, "confirmed");
            }
            RankOutcome::Failed(reason) => panic!("expected a verdict, got failure: {reason}"),
        }
    }

    #[tokio::test]
    async fn rank_retries_once_then_reports_a_failure_on_malformed_output() {
        let _g = crate::ENV_LOCK.lock().await;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "choices": [{"message": {"role": "assistant", "content": "not json"}}]
                })),
            )
            .expect(2)
            .mount(&server)
            .await;
        unsafe {
            std::env::set_var("CONTROLLER_RANKER_API_URL", server.uri());
        }
        let outcome = rank("title", "body", &[]).await;
        unsafe {
            std::env::remove_var("CONTROLLER_RANKER_API_URL");
        }

        assert!(matches!(outcome, RankOutcome::Failed(_)));
        // `.expect(2)` above is checked on `server` drop; reaching here means exactly the
        // bounded retry ran, no more.
    }
}
