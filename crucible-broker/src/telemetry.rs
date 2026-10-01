//! Broker OTLP span export, following the ENGINE convention: OTLP over gRPC/tonic aimed at
//! `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` / `OTEL_EXPORTER_OTLP_ENDPOINT` (a host-level gRPC
//! endpoint, no `/v1/traces` suffix). **No endpoint set = no exporter installed**; the binary
//! keeps its plain stderr `fmt` subscriber and `tracing` spans cost nothing downstream.
//!
//! The loop pod already carries `OTEL_EXPORTER_OTLP_ENDPOINT` for the engine's own spans; the
//! broker rides the same env so its MCP tool spans land in the same Tempo.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use opentelemetry_sdk::trace::SdkTracerProvider;
use std::time::Duration;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// The exporter guard: dropping it flushes + shuts the provider down (bounded, so a dead collector
/// can't wedge exit). `None` provider = no exporter was installed (the default).
pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
}

/// Set once by [`init`] when the OTLP layer is installed; read per tool call. A set-once flag for
/// a real process singleton (the same shape as the engine's published provider) rather than shared
/// mutable state; it keeps the telemetry-off path zero-cost (no span build, no reply parse, no
/// turn-file read, and no `set_parent` that would warn `LayerNotFound` on every call).
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the OTLP span layer is installed in this process.
pub(crate) fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The span service name: the configured broker name when there is one, else the binary's own.
/// Leaked because a tracer name must outlive the provider, and there is exactly one per process.
fn resolve_service_name(configured: Option<String>, fallback: &'static str) -> &'static str {
    match configured {
        Some(name) if !name.trim().is_empty() => Box::leak(name.trim().to_owned().into_boxed_str()),
        _ => fallback,
    }
}

/// Install the global tracing subscriber: always the stderr `fmt` layer (the broker's existing
/// pod-log narration), plus the OTLP span layer when an endpoint is configured.
/// Must run inside a tokio runtime (the OTLP batch processor spawns on it).
///
/// The service name is `BROKER_NAME` (the engine passes `[agent.broker].name`) falling back to
/// `fallback`, the binary's own name. Domains share one generic broker binary, so naming spans
/// after the binary makes every domain look alike — and worse, a domain reusing another's binary
/// reports that domain's name.
pub fn init(fallback: &'static str) -> Telemetry {
    let service_name = resolve_service_name(std::env::var("BROKER_NAME").ok(), fallback);
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,rmcp=debug"));
    let fmt_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
    let registry = tracing_subscriber::registry().with(filter).with(fmt_layer);

    let Some(endpoint) = resolve_endpoint(
        std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT").ok(),
        std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
    ) else {
        let _ = registry.try_init();
        return Telemetry { provider: None };
    };

    match build_provider(&endpoint, service_name) {
        Ok(provider) => {
            let tracer = provider.tracer(service_name);
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
            if registry.with(otel_layer).try_init().is_err() {
                return Telemetry { provider: None };
            }
            // W3C propagation, for parenting tool spans on a client-sent traceparent.
            opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
            ENABLED.store(true, std::sync::atomic::Ordering::Relaxed);
            eprintln!("[{service_name}] OTLP span export enabled -> {endpoint}");
            Telemetry {
                provider: Some(provider),
            }
        }
        Err(e) => {
            let _ = registry.try_init();
            eprintln!(
                "[{service_name}] OTEL endpoint set but the OTLP exporter failed to build; spans \
                 off: {e:#}"
            );
            Telemetry { provider: None }
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // Bounded shutdown on a scratch thread so a hung collector can't wedge exit.
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = provider.shutdown();
                let _ = tx.send(());
            });
            let _ = rx.recv_timeout(Duration::from_secs(3));
        }
    }
}

/// `spawn_blocking` that carries the CURRENT span onto the blocking thread, so `tracing` events
/// and `Span::current().record(..)` inside the tool bodies land on the MCP tool span instead of
/// rooting themselves (tracing's current-span is a thread-local; it doesn't cross by itself).
pub(crate) async fn spawn_blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let span = tracing::Span::current();
    tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        f()
    })
    .await
}

/// Resolves on SIGTERM (k8s termination) or ctrl-c. Feed it to axum's `with_graceful_shutdown`
/// so `main` RETURNS instead of being killed; that lets the [`Telemetry`] guard drop and
/// flush the final span batch.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// The traces-specific env wins over the base one; empty/whitespace = unset. Pure, so the
/// set/unset table is unit-testable. (Mirrors the engine's resolution.)
fn resolve_endpoint(traces: Option<String>, base: Option<String>) -> Option<String> {
    [traces, base]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
}

/// Batch-exporting tracer provider aimed at `endpoint` (OTLP over gRPC/tonic).
fn build_provider(endpoint: &str, service_name: &'static str) -> anyhow::Result<SdkTracerProvider> {
    use anyhow::Context as _;
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()
        .context("building the OTLP gRPC span exporter")?;
    let resource = Resource::builder().with_service_name(service_name).build();
    Ok(SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

#[cfg(test)]
mod tests {
    /// A domain that reuses another domain's broker binary must not report that domain's name;
    /// the configured `[agent.broker].name` wins, and blank config falls back rather than
    /// producing an empty service.
    #[test]
    fn service_name_prefers_the_configured_broker_name() {
        assert_eq!(
            resolve_service_name(Some("vllm-broker".to_string()), "crucible-broker"),
            "vllm-broker"
        );
        assert_eq!(
            resolve_service_name(Some("  spaced  ".to_string()), "crucible-broker"),
            "spaced"
        );
        assert_eq!(
            resolve_service_name(None, "crucible-broker"),
            "crucible-broker"
        );
        assert_eq!(
            resolve_service_name(Some("   ".to_string()), "crucible-broker"),
            "crucible-broker"
        );
    }

    use super::*;

    #[test]
    fn endpoint_resolution_prefers_traces_and_ignores_blank() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(
            resolve_endpoint(s("http://tempo:4317"), s("http://other:4317")),
            s("http://tempo:4317")
        );
        assert_eq!(
            resolve_endpoint(None, s("http://base:4317")),
            s("http://base:4317")
        );
        // Blank values are treated as unset.
        assert_eq!(
            resolve_endpoint(s("  "), s("http://base:4317")),
            s("http://base:4317")
        );
        assert_eq!(resolve_endpoint(None, None), None);
        assert_eq!(resolve_endpoint(s(""), s("  ")), None);
    }
}
