//! Controller → engine trace propagation: serialize the controller's current dispatch span into a
//! W3C `traceparent` and inject it as pod env, so the pod's engine re-roots its span under this
//! dispatch and Tempo shows one tree (controller → run/scope/rank → turn → RPCs).
//!
//! Every engine-running dispatch carries this — the loop run and the two agent turns (scope,
//! grounded-rank). Each dispatch's PRODUCER span pairs with the engine's CONSUMER span (`run`,
//! `scope`, `rank_grounded`), the async producer/consumer edge Tempo's service-graph processor
//! draws the controller → crucible link from. Injection is a no-op unless the controller's OTLP layer is
//! actually recording (an unrecorded span serializes to nothing), so a stderr-only controller never
//! injects a bogus all-zeros traceparent.

use k8s_openapi::api::core::v1::{EnvVar, Pod};

/// The W3C env vars the engine reads to adopt this dispatch as its trace parent. Distinct from the
/// `OTEL_*` deploy config the render already sets (endpoint/headers), so injecting them can never
/// collide with the loop pod's exporter configuration.
const TRACEPARENT_ENV: &str = "TRACEPARENT";
const TRACESTATE_ENV: &str = "TRACESTATE";

/// Serialize the current tracing span's OTel context to W3C `(traceparent, tracestate)`, or `None`
/// when tracing isn't recording — no OTLP layer installed, or an invalid/all-zeros span context.
/// A local `TraceContextPropagator` (not the global) mirrors the engine's extraction side.
fn current_trace_env() -> Option<(String, Option<String>)> {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    trace_env_from_context(&tracing::Span::current().context())
}

/// The pure half of [`current_trace_env`]: inject `cx` through the W3C propagator, returning the
/// `traceparent` (and non-empty `tracestate`) only when `cx` carries a valid span context.
fn trace_env_from_context(cx: &opentelemetry::Context) -> Option<(String, Option<String>)> {
    use opentelemetry::propagation::TextMapPropagator as _;
    use opentelemetry::trace::TraceContextExt as _;

    if !cx.span().span_context().is_valid() {
        return None;
    }
    let mut carrier = std::collections::HashMap::new();
    opentelemetry_sdk::propagation::TraceContextPropagator::new().inject_context(cx, &mut carrier);
    let traceparent = carrier.remove("traceparent")?;
    let tracestate = carrier.remove("tracestate").filter(|s| !s.is_empty());
    Some((traceparent, tracestate))
}

/// Inject the current dispatch span's W3C context as `TRACEPARENT`/`TRACESTATE` env onto every main
/// container of `pod`, so the engine can re-parent its `run`/`scope`/`rank_grounded` span under this
/// dispatch. A no-op when tracing isn't recording (see [`current_trace_env`]).
pub(crate) fn inject_dispatch_context(pod: &mut Pod) {
    if let Some((traceparent, tracestate)) = current_trace_env() {
        apply_trace_env(pod, &traceparent, tracestate.as_deref());
    }
}

/// Set the two env vars on every main container, replacing any pre-existing copy so a re-stamp is
/// idempotent. Init containers are left alone: they clone/prepare, they don't run the engine.
fn apply_trace_env(pod: &mut Pod, traceparent: &str, tracestate: Option<&str>) {
    let Some(spec) = pod.spec.as_mut() else {
        return;
    };
    for c in spec.containers.iter_mut() {
        let env = c.env.get_or_insert_with(Default::default);
        env.retain(|v| v.name != TRACEPARENT_ENV && v.name != TRACESTATE_ENV);
        env.push(EnvVar {
            name: TRACEPARENT_ENV.to_string(),
            value: Some(traceparent.to_string()),
            value_from: None,
        });
        if let Some(ts) = tracestate {
            env.push(EnvVar {
                name: TRACESTATE_ENV.to_string(),
                value: Some(ts.to_string()),
                value_from: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{Container, PodSpec};
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt as _, TraceFlags, TraceId, TraceState,
    };

    /// A context carrying a known, valid remote span (the shape a recording dispatch span has).
    fn known_context() -> opentelemetry::Context {
        let sc = SpanContext::new(
            TraceId::from_hex("0af7651916cd43dd8448eb211c80319c").expect("trace id"),
            SpanId::from_hex("b7ad6b7169203331").expect("span id"),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        );
        opentelemetry::Context::new().with_remote_span_context(sc)
    }

    #[test]
    fn valid_context_serializes_to_the_known_traceparent() {
        let (traceparent, tracestate) =
            trace_env_from_context(&known_context()).expect("valid context yields a traceparent");
        // W3C format: version-traceid-spanid-flags, carrying our exact ids.
        assert_eq!(
            traceparent,
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
        );
        assert_eq!(tracestate, None, "empty tracestate is dropped");
    }

    #[test]
    fn invalid_context_serializes_to_nothing() {
        // The default (no active span) context is invalid — never inject an all-zeros traceparent.
        assert!(trace_env_from_context(&opentelemetry::Context::new()).is_none());
    }

    fn pod_with_containers(n: usize) -> Pod {
        Pod {
            spec: Some(PodSpec {
                containers: (0..n)
                    .map(|i| Container {
                        name: format!("c{i}"),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn env_of(pod: &Pod, container: usize, name: &str) -> Option<String> {
        pod.spec
            .as_ref()?
            .containers
            .get(container)?
            .env
            .as_ref()?
            .iter()
            .find(|v| v.name == name)
            .and_then(|v| v.value.clone())
    }

    #[test]
    fn apply_sets_env_on_every_container() {
        let mut pod = pod_with_containers(2);
        apply_trace_env(&mut pod, "00-aaaa-bbbb-01", Some("vendor=1"));
        for c in 0..2 {
            assert_eq!(
                env_of(&pod, c, TRACEPARENT_ENV).as_deref(),
                Some("00-aaaa-bbbb-01")
            );
            assert_eq!(env_of(&pod, c, TRACESTATE_ENV).as_deref(), Some("vendor=1"));
        }
    }

    #[test]
    fn apply_is_idempotent_and_drops_tracestate_when_absent() {
        let mut pod = pod_with_containers(1);
        apply_trace_env(&mut pod, "00-first-1111-01", Some("vendor=1"));
        // Re-stamp with no tracestate: the traceparent is replaced (not duplicated) and the stale
        // tracestate is cleared.
        apply_trace_env(&mut pod, "00-second-2222-01", None);
        let env = pod.spec.unwrap().containers.remove(0).env.unwrap();
        let tp: Vec<_> = env.iter().filter(|v| v.name == TRACEPARENT_ENV).collect();
        assert_eq!(tp.len(), 1, "one traceparent, not two");
        assert_eq!(tp[0].value.as_deref(), Some("00-second-2222-01"));
        assert!(
            !env.iter().any(|v| v.name == TRACESTATE_ENV),
            "stale tracestate cleared on re-stamp"
        );
    }
}
