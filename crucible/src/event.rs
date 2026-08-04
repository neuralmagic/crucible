//! Engine-side cost/pricing helpers over the agent event.
//!
//! The [`AgentEvent`] NDJSON serde types themselves live in `crucible-contract` (the session-log
//! format both the engine and the controller's SSE relay read); they are re-exported here so the
//! engine keeps referring to them as `crate::event::*`. What stays here is the engine-only cost
//! logic: the authoritative-vs-estimated cost extraction and the pricing-table fallback estimate.

pub use crucible_contract::event::{AgentEvent, RawStream, Tokens};

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

/// Per-token USD prices: (input, output, cache_read, cache_write).
/// cache_read ≈ 0.1× input, cache_write ≈ 1.25× input (default 5-min ephemeral).
/// Source: claude-api pricing (Opus 4.x $5/$25, Sonnet 4.6 $3/$15, Haiku 4.5 $1/$5 per MTok).
fn model_prices(model: &str) -> (f64, f64, f64, f64) {
    let m = model.to_ascii_lowercase();
    let (input, output) = if m.contains("haiku") {
        (1.0, 5.0)
    } else if m.contains("sonnet") {
        (3.0, 15.0)
    } else {
        // Opus (and unknown, Opus is the loop default).
        (5.0, 25.0)
    };
    let per = 1e-6;
    (
        input * per,
        output * per,
        input * 0.1 * per,
        input * 1.25 * per,
    )
}

/// The turn's provisional cost from one mid-turn token sample: the OTEL number when
/// telemetry stamped one, otherwise the pricing-table estimate. Reconciled by the
/// authoritative turn-end cost, so streaming this keeps the budget line moving
/// inside a turn without changing what the run ultimately charges.
pub fn provisional_cost(model: &str, t: &Tokens) -> f64 {
    t.cost_usd.unwrap_or_else(|| estimate_cost(model, t))
}

/// Estimate the run cost so far from the live token counts × model pricing.
/// Replaced by the agent's authoritative number once `result`/OTEL reports it.
pub fn estimate_cost(model: &str, t: &Tokens) -> f64 {
    let (pin, pout, pcr, pcw) = model_prices(model);
    t.input as f64 * pin
        + t.output as f64 * pout
        + t.cache_read as f64 * pcr
        + t.cache_write as f64 * pcw
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
            // Unknown models price as Opus, the loop default.
            ("mystery-model", 5.0, 25.0),
        ] {
            let want = input + output + input * 0.1 + input * 1.25;
            let got = estimate_cost(model, &sample());
            assert!((got - want).abs() < 1e-9, "{model}: got {got}, want {want}");
        }
    }

    #[test]
    fn provisional_prefers_the_authoritative_sample_cost() {
        let mut t = sample();
        t.cost_usd = Some(0.42);
        assert!((provisional_cost("claude-opus-4-6", &t) - 0.42).abs() < 1e-9);
        t.cost_usd = None;
        let est = estimate_cost("claude-opus-4-6", &t);
        assert!((provisional_cost("claude-opus-4-6", &t) - est).abs() < 1e-9);
    }
}
