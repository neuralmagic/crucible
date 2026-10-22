//! Engine-side cost/pricing helpers over the agent event.
//!
//! The [`AgentEvent`] NDJSON serde types themselves live in `crucible-contract` (the session-log
//! format both the engine and the controller's SSE relay read); they are re-exported here so the
//! engine keeps referring to them as `crate::agent::event::*`. What stays here is the engine-only cost
//! logic: the authoritative-vs-estimated cost extraction and the pricing-table fallback estimate.

pub use crucible_contract::event::{AgentEvent, RawStream, Tokens};
use crucible_contract::inference::TokenPrice;

/// The cumulative run cost this event reports, if any. Prefers authoritative
/// numbers (OTEL summary, result, OTEL-on-tokens) and falls back to the pretty
/// `Cost: $x` detail line the scraper produces in non-json mode.
pub fn cost_of(ev: &AgentEvent) -> Option<f64> {
    match ev {
        AgentEvent::OtelSummary { cost_usd, .. } if *cost_usd > 0.0 => Some(*cost_usd),
        AgentEvent::Result { cost_usd, .. } if *cost_usd > 0.0 => Some(*cost_usd),
        AgentEvent::Tokens(t) => t.cost_usd,
        AgentEvent::Log {
            level,
            label,
            value,
        } if level == "detail" && label == "Cost" => value
            .as_deref()
            .and_then(|v| v.trim_start_matches('$').parse().ok()),
        _ => None,
    }
}

/// USD per token for each billed bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Rates {
    const FREE: Rates = Rates {
        input: 0.0,
        output: 0.0,
        cache_read: 0.0,
        cache_write: 0.0,
    };

    fn per_mtok(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Rates {
        let per = 1e-6;
        Rates {
            input: input * per,
            output: output * per,
            cache_read: cache_read * per,
            cache_write: cache_write * per,
        }
    }

    pub fn cost(&self, t: &Tokens) -> f64 {
        t.input as f64 * self.input
            + t.output as f64 * self.output
            + t.cache_read as f64 * self.cache_read
            + t.cache_write as f64 * self.cache_write
    }
}

impl From<&TokenPrice> for Rates {
    fn from(price: &TokenPrice) -> Rates {
        Rates::per_mtok(
            price.input.get(),
            price.output.get(),
            price.cache_read.get(),
            price.cache_write.get(),
        )
    }
}

/// How a turn's tokens become dollars (RFC-0001 C-INFERENCE).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pricing {
    /// A vendor-billed model: the cost the harness reports is authoritative, and these rates
    /// estimate a turn that reported none.
    Vendor(Rates),
    /// Tokens times these rates whatever the harness reports: the agent binding's price, or
    /// nothing for a model no vendor bills.
    Fixed(Rates),
}

impl Pricing {
    pub fn resolve(model: &str, price: Option<&TokenPrice>) -> Pricing {
        match price {
            Some(price) => Pricing::Fixed(price.into()),
            None => vendor_rates(model).map_or(Pricing::Fixed(Rates::FREE), Pricing::Vendor),
        }
    }

    fn rates(&self) -> &Rates {
        match self {
            Pricing::Vendor(rates) | Pricing::Fixed(rates) => rates,
        }
    }

    /// The turn's cumulative cost one mid-turn token sample implies.
    #[cfg(feature = "autoresearch")]
    pub fn provisional(&self, t: &Tokens) -> f64 {
        match self {
            Pricing::Vendor(rates) => t.cost_usd.unwrap_or_else(|| rates.cost(t)),
            Pricing::Fixed(rates) => rates.cost(t),
        }
    }

    /// The turn's cumulative cost `ev` implies, if it implies one.
    #[cfg(feature = "autoresearch")]
    pub fn sample(&self, ev: &AgentEvent) -> Option<f64> {
        let reported = match self {
            Pricing::Vendor(_) => cost_of(ev),
            Pricing::Fixed(_) => None,
        };
        reported.or(match ev {
            AgentEvent::Tokens(t) => Some(self.provisional(t)),
            _ => None,
        })
    }

    /// What a finished turn cost, from the highest cost its harness reported and its largest
    /// token sample.
    pub fn settle(&self, reported: f64, tokens: Option<&Tokens>) -> f64 {
        match self {
            Pricing::Vendor(_) if reported > 0.0 => reported,
            Pricing::Vendor(_) | Pricing::Fixed(_) => tokens.map_or(0.0, |t| self.rates().cost(t)),
        }
    }
}

/// Per-token USD prices for a vendor-billed model, `None` for any other.
/// Anthropic: cache_read ≈ 0.1× input, cache_write ≈ 1.25× input (default 5-min ephemeral).
/// OpenAI: cached input is 0.1× input and there is no cache-write premium (1.0× input).
/// Sources: claude-api pricing (Opus 4.x $5/$25, Sonnet 4.6 $3/$15, Haiku 4.5 $1/$5 per MTok) and
/// OpenAI's published per-MTok rates (see [`openai_prices`]). A name from neither family is a
/// self-hosted model reached through a custom endpoint (qwen on an in-house MaaS, say): its
/// tokens are counted but priced at nothing, since no vendor bills for them.
fn vendor_rates(model: &str) -> Option<Rates> {
    let m = model.to_ascii_lowercase();
    if let Some((input, output)) = openai_prices(&m) {
        return Some(Rates::per_mtok(input, output, input * 0.1, input));
    }
    let (input, output) = if m.contains("haiku") {
        (1.0, 5.0)
    } else if m.contains("sonnet") {
        (3.0, 15.0)
    } else if m.contains("opus") || m.contains("claude") {
        // Opus, and any other claude-* name: the family's most expensive tier, so an
        // unrecognized Anthropic name never under-charges the budget line.
        (5.0, 25.0)
    } else {
        return None;
    };
    Some(Rates::per_mtok(input, output, input * 0.1, input * 1.25))
}

/// The (input, output) per-MTok rate for an OpenAI model the codex harness can run, or `None` when
/// the name is not OpenAI's and the Anthropic table answers. Short-context standard rates as of
/// August 2026: codex $1.75/$14, GPT-5.6 Luna $0.20/$1.20, Terra $2/$12, Sol $5/$30. Any other
/// `gpt-*` prices as Sol, the family's most expensive tier, so an unrecognized name never
/// under-charges the budget line.
fn openai_prices(m: &str) -> Option<(f64, f64)> {
    if m.contains("codex") {
        Some((1.75, 14.0))
    } else if m.contains("luna") {
        Some((0.20, 1.20))
    } else if m.contains("terra") {
        Some((2.0, 12.0))
    } else if m.contains("sol") || m.starts_with("gpt") {
        Some((5.0, 30.0))
    } else {
        None
    }
}

/// Estimate the run cost so far from the live token counts × the vendor table.
pub fn estimate_cost(model: &str, t: &Tokens) -> f64 {
    vendor_rates(model).map_or(0.0, |rates| rates.cost(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tokens {
        Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
            total: 4_000_000,
            rate: None,
            cost_usd: None,
        }
    }

    #[test]
    fn estimate_matches_published_per_mtok_rates() {
        // 1 MTok of each bucket: input + output + 0.1x input (read) + 1.25x input (write).
        for (model, input, output) in [
            ("claude-opus-4-6", 5.0, 25.0),
            ("claude-sonnet-4-6", 3.0, 15.0),
            ("claude-haiku-4-5", 1.0, 5.0),
            // An unrecognized claude spelling prices as Opus, the family's ceiling.
            ("claude-mystery-9", 5.0, 25.0),
        ] {
            let want = input + output + input * 0.1 + input * 1.25;
            let got = estimate_cost(model, &sample());
            assert!((got - want).abs() < 1e-9, "{model}: got {got}, want {want}");
        }
    }

    /// OpenAI bills cached input at 0.1x and charges no cache-write premium, so a codex turn's
    /// buckets price differently from claude's even at the same headline rates.
    #[test]
    fn openai_rows_price_at_the_published_per_mtok_rates() {
        for (model, input, output) in [
            ("gpt-5.6-sol", 5.0, 30.0),
            ("gpt-5.6-terra", 2.0, 12.0),
            ("gpt-5.6-luna", 0.20, 1.20),
            ("gpt-5.3-codex", 1.75, 14.0),
            // An unrecognized OpenAI name prices as Sol, the priciest tier of the family.
            ("gpt-6-whatever", 5.0, 30.0),
        ] {
            let want = input + output + input * 0.1 + input;
            let got = estimate_cost(model, &sample());
            assert!((got - want).abs() < 1e-9, "{model}: got {got}, want {want}");
        }
    }

    /// The OpenAI rows must not shadow the Anthropic table, and vice versa.
    /// A model from neither vendor family is self-hosted: the estimate counts its tokens at $0
    /// rather than billing a qwen turn on an in-house endpoint as Opus.
    #[test]
    fn a_self_hosted_model_prices_at_zero() {
        for model in [
            "qwen-3-8-27b",
            "Qwen/Qwen3-235B",
            "llama-4-maverick",
            "deepseek-v4",
        ] {
            assert_eq!(estimate_cost(model, &sample()), 0.0, "{model}");
        }
        // An unrecognized claude spelling still prices as Opus.
        let want = 5.0 + 25.0 + 5.0 * 0.1 + 5.0 * 1.25;
        assert!((estimate_cost("claude-fable-5", &sample()) - want).abs() < 1e-9);
    }

    #[test]
    fn the_pricing_tables_stay_on_their_own_side() {
        assert_eq!(openai_prices("claude-opus-4-6"), None);
        assert_eq!(openai_prices("claude-sonnet-4-6"), None);
        assert_eq!(openai_prices("mystery-model"), None);
        assert_eq!(openai_prices("gpt-5.6-sol"), Some((5.0, 30.0)));
        // Codex wins over the family default however the name is spelled.
        assert_eq!(openai_prices("gpt-5.2-codex"), Some((1.75, 14.0)));
    }

    fn price(input: f64, output: f64, cache_read: f64, cache_write: f64) -> TokenPrice {
        let usd = |v| crucible_contract::inference::UsdPerMtok::new(v).unwrap();
        TokenPrice {
            input: usd(input),
            output: usd(output),
            cache_read: usd(cache_read),
            cache_write: usd(cache_write),
        }
    }

    fn turn_tokens() -> Tokens {
        Tokens {
            input: 2_000_000,
            output: 500_000,
            cache_read: 10_000_000,
            cache_write: 1_000_000,
            total: 13_500_000,
            rate: None,
            cost_usd: Some(25.0),
        }
    }

    #[cfg(feature = "autoresearch")]
    #[test]
    fn provisional_prefers_the_authoritative_sample_cost_for_a_vendor_model() {
        let pricing = Pricing::resolve("claude-opus-4-6", None);
        let mut t = sample();
        t.cost_usd = Some(0.42);
        assert!((pricing.provisional(&t) - 0.42).abs() < 1e-9);
        t.cost_usd = None;
        let est = estimate_cost("claude-opus-4-6", &t);
        assert!((pricing.provisional(&t) - est).abs() < 1e-9);
    }

    #[test]
    fn a_self_hosted_model_settles_at_zero_whatever_the_harness_reports() {
        for model in ["zai-org/glm-5", "deepseek-ai/DeepSeek-V4"] {
            let pricing = Pricing::resolve(model, None);
            assert_eq!(pricing, Pricing::Fixed(Rates::FREE), "{model}");
            assert_eq!(pricing.settle(25.0, Some(&turn_tokens())), 0.0, "{model}");
        }
    }

    #[cfg(feature = "autoresearch")]
    #[test]
    fn a_live_sample_follows_the_turns_pricing() {
        let result = AgentEvent::Result {
            subtype: "success".into(),
            is_error: false,
            turns: 3,
            cost_usd: 25.0,
            error: None,
        };
        let glm = Pricing::resolve("zai-org/glm-5", None);
        assert_eq!(glm.provisional(&turn_tokens()), 0.0);
        assert_eq!(glm.sample(&result), None);
        assert_eq!(glm.sample(&AgentEvent::Tokens(turn_tokens())), Some(0.0));
        let sonnet = Pricing::resolve("claude-sonnet-4-6", None);
        assert_eq!(sonnet.sample(&result), Some(25.0));
        assert_eq!(sonnet.provisional(&turn_tokens()), 25.0);
        let priced = Pricing::resolve("zai-org/glm-5", Some(&price(1.0, 1.0, 1.0, 1.0)));
        assert_eq!(priced.sample(&result), None);
        assert!((priced.provisional(&turn_tokens()) - 13.5).abs() < 1e-9);
    }

    #[test]
    fn a_vendor_model_keeps_the_harness_cost_and_estimates_only_without_one() {
        let pricing = Pricing::resolve("claude-sonnet-4-6", None);
        assert!(matches!(pricing, Pricing::Vendor(_)));
        assert_eq!(pricing.settle(25.0, Some(&turn_tokens())), 25.0);
        let est = estimate_cost("claude-sonnet-4-6", &turn_tokens());
        assert!(est > 0.0);
        assert!((pricing.settle(0.0, Some(&turn_tokens())) - est).abs() < 1e-9);
        assert_eq!(pricing.settle(0.0, None), 0.0);
    }

    #[test]
    fn a_binding_price_settles_at_tokens_times_price_for_any_model() {
        let glm = price(0.6, 2.2, 0.11, 0.75);
        let want = 2.0 * 0.6 + 0.5 * 2.2 + 10.0 * 0.11 + 1.0 * 0.75;
        for model in ["zai-org/glm-5", "claude-opus-4-6", "gpt-5.6-sol"] {
            let pricing = Pricing::resolve(model, Some(&glm));
            assert!(matches!(pricing, Pricing::Fixed(_)), "{model}");
            let got = pricing.settle(25.0, Some(&turn_tokens()));
            assert!((got - want).abs() < 1e-9, "{model}: got {got}, want {want}");
            assert_eq!(pricing.settle(25.0, None), 0.0, "{model}");
        }
    }

    #[test]
    fn a_binding_price_bills_each_cache_bucket_at_its_own_rate() {
        let only = |t: Tokens| {
            Pricing::resolve("glm", Some(&price(1.0, 2.0, 3.0, 4.0))).settle(0.0, Some(&t))
        };
        let zero = Tokens {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
            total: 0,
            rate: None,
            cost_usd: None,
        };
        let mtok = 1_000_000;
        for (tokens, want) in [
            (
                Tokens {
                    input: mtok,
                    ..zero.clone()
                },
                1.0,
            ),
            (
                Tokens {
                    output: mtok,
                    ..zero.clone()
                },
                2.0,
            ),
            (
                Tokens {
                    cache_read: mtok,
                    ..zero.clone()
                },
                3.0,
            ),
            (
                Tokens {
                    cache_write: mtok,
                    ..zero.clone()
                },
                4.0,
            ),
        ] {
            let got = only(tokens.clone());
            assert!(
                (got - want).abs() < 1e-9,
                "{tokens:?}: got {got}, want {want}"
            );
        }
    }
}
