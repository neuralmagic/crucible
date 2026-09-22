//! Tracing setup for the controller daemon: an [`EnvFilter`]-driven `tracing_subscriber` (RUST_LOG,
//! default `info`) writing to stderr, plus an OTLP trace layer that is installed **only** when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set. Unset (the default) means no OpenTelemetry is initialized
//! at all — no exporter, no connection attempt, no startup cost — because the cluster has no shared
//! trace backend yet; the layer exists so the day a collector appears it's one env var away.
//!
//! [`init`] returns a [`TelemetryGuard`] the entrypoint holds for the process's life; dropping it
//! flushes and shuts the tracer provider down so buffered spans aren't lost on exit.

use opentelemetry::KeyValue;
use opentelemetry::trace::{Span as _, TracerProvider as _};
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// The `service.name` every exported span carries.
const SERVICE_NAME: &str = "crucible-controller";

/// Holds the OTLP tracer provider (when one was installed) so it's shut down on drop — flushing any
/// batched spans. A stderr-only run holds `None` and does nothing on drop.
#[must_use = "dropping the guard shuts tracing down; hold it for the process's lifetime"]
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.provider.take() {
            // Best-effort flush; nothing to do but log if the collector is already gone.
            if let Err(e) = provider.shutdown() {
                tracing::debug!(error = %e, "otlp tracer provider shutdown failed");
            }
        }
    }
}

/// Install the global tracing subscriber. Idempotent-ish: a second call is a no-op (the global
/// default is already set) and returns an inert guard. Call once at startup, inside a tokio runtime
/// when OTLP might be enabled.
pub fn init(db_url: &str) -> TelemetryGuard {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty());

    let (otel_layer, provider, failed) = match endpoint.as_deref() {
        // The default path: no trace backend, so no OpenTelemetry at all — stderr only.
        None => (None, None, None),
        Some(endpoint) => match build_provider(endpoint, pool_tags(db_url)) {
            Ok(provider) => {
                let tracer = provider.tracer(SERVICE_NAME);
                // `err`-instrumented functions emit an error-level event on failure; without this
                // mapping the span still exports with status Unset and never renders as an error.
                let layer = tracing_opentelemetry::layer()
                    .with_tracer(tracer)
                    .with_error_events_to_status(true)
                    .with_error_records_to_exceptions(true);
                (Some(layer), Some(provider), None)
            }
            // A misconfigured endpoint must never wedge the daemon: fall back to stderr-only.
            Err(e) => (None, None, Some(e)),
        },
    };
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .try_init();
    if let Some(endpoint) = &endpoint {
        if provider.is_some() {
            tracing::info!(%endpoint, "OTLP trace export enabled");
        }
        if let Some(e) = failed {
            tracing::warn!(
                %endpoint,
                error = format!("{e:#}"),
                "OTEL_EXPORTER_OTLP_ENDPOINT is set but the OTLP exporter failed to build; tracing to stderr only"
            );
        }
    }
    TelemetryGuard { provider }
}

/// Build a batch-exporting tracer provider aimed at `endpoint` (OTLP over HTTP/protobuf).
fn build_provider(endpoint: &str, pool_tags: Vec<KeyValue>) -> anyhow::Result<SdkTracerProvider> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(traces_url(endpoint))
        .build()?;
    let resource = Resource::builder().with_service_name(SERVICE_NAME).build();
    Ok(SdkTracerProvider::builder()
        .with_span_processor(DbPoolTags { tags: pool_tags })
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

/// `with_endpoint` uses its argument verbatim, but an OTLP/HTTP collector only serves traces at
/// `/v1/traces`, and a base URL silently 404s every batch. Accept either form.
fn traces_url(endpoint: &str) -> String {
    let base = endpoint.trim_end_matches('/');
    if base.ends_with("/v1/traces") {
        base.to_string()
    } else {
        format!("{base}/v1/traces")
    }
}

/// The ledger connection tags every `db.*` span carries (Datadog's peer/out/db.name set). One
/// shared pool serves the whole daemon, so they are computed once from the URL — never the URL
/// itself, which carries credentials.
fn pool_tags(db_url: &str) -> Vec<KeyValue> {
    let Ok(url) = url::Url::parse(db_url) else {
        return Vec::new();
    };
    let mut tags = Vec::new();
    if let Some(host) = url.host_str() {
        tags.push(KeyValue::new("peer.hostname", host.to_string()));
        tags.push(KeyValue::new("out.host", host.to_string()));
    }
    tags.push(KeyValue::new(
        "out.port",
        i64::from(url.port().unwrap_or(5432)),
    ));
    let db = url.path().trim_start_matches('/');
    if !db.is_empty() {
        tags.push(KeyValue::new("db.instance", db.to_string()));
        tags.push(KeyValue::new("db.name", db.to_string()));
    }
    tags
}

/// Stamps the pool tags onto every span named `db.*` as it starts, so the 125 operation
/// `#[instrument]` sites don't each need connection details they can't see.
#[derive(Debug)]
struct DbPoolTags {
    tags: Vec<KeyValue>,
}

impl opentelemetry_sdk::trace::SpanProcessor for DbPoolTags {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, _cx: &opentelemetry::Context) {
        let is_db = span
            .exported_data()
            .is_some_and(|d| d.name.starts_with("db."));
        if is_db {
            for tag in &self.tags {
                span.set_attribute(tag.clone());
            }
        }
    }

    fn on_end(&self, _span: opentelemetry_sdk::trace::SpanData) {}

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(
        &self,
        _timeout: std::time::Duration,
    ) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::telemetry::{pool_tags, traces_url};

    #[test]
    fn appends_the_traces_path_to_a_base_url() {
        assert_eq!(
            traces_url("http://10.0.0.1:4318"),
            "http://10.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            traces_url("http://10.0.0.1:4318/"),
            "http://10.0.0.1:4318/v1/traces"
        );
    }

    #[test]
    fn keeps_an_explicit_traces_path() {
        assert_eq!(
            traces_url("http://10.0.0.1:4318/v1/traces"),
            "http://10.0.0.1:4318/v1/traces"
        );
        assert_eq!(
            traces_url("http://10.0.0.1:4318/v1/traces/"),
            "http://10.0.0.1:4318/v1/traces"
        );
    }

    fn get<'a>(tags: &'a [opentelemetry::KeyValue], key: &str) -> Option<&'a opentelemetry::Value> {
        tags.iter()
            .find(|kv| kv.key.as_str() == key)
            .map(|kv| &kv.value)
    }

    #[test]
    fn tags_carry_host_port_and_database_but_never_credentials() {
        let tags = pool_tags("postgres://crucible:s3cret@pg.example.internal:5433/ledger");
        assert_eq!(
            get(&tags, "peer.hostname").unwrap().to_string(),
            "pg.example.internal"
        );
        assert_eq!(
            get(&tags, "out.host").unwrap().to_string(),
            "pg.example.internal"
        );
        assert_eq!(get(&tags, "out.port").unwrap().to_string(), "5433");
        assert_eq!(get(&tags, "db.instance").unwrap().to_string(), "ledger");
        assert_eq!(get(&tags, "db.name").unwrap().to_string(), "ledger");
        for kv in &tags {
            assert!(
                !kv.value.to_string().contains("s3cret"),
                "credential leaked into {}",
                kv.key
            );
            assert!(
                !kv.value.to_string().contains("crucible:"),
                "userinfo leaked into {}",
                kv.key
            );
        }
    }

    #[test]
    fn port_defaults_to_5432_and_database_is_optional() {
        let tags = pool_tags("postgres://localhost");
        assert_eq!(get(&tags, "out.port").unwrap().to_string(), "5432");
        assert!(get(&tags, "db.name").is_none());
    }

    #[test]
    fn an_unparseable_url_yields_no_tags() {
        assert!(pool_tags("not a url").is_empty());
    }
}
