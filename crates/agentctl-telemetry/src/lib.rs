// SPDX-License-Identifier: BUSL-1.1
//! Shared tracing/telemetry init for the agentctl control-plane binaries
//! (operator, apiserver, gateway, modelgateway, coordination, scaler, admission).
//!
//! Every binary calls [`init`] once, early in `main` (inside the tokio runtime).
//! By default it installs a standard `tracing_subscriber` fmt layer — honoring
//! `RUST_LOG`, defaulting to `info` — so the behaviour matches a plain
//! `tracing_subscriber::fmt().with_env_filter(..).init()`.
//!
//! When **`OTEL_EXPORTER_OTLP_ENDPOINT` is set** it additionally:
//!   * exports spans over **OTLP/gRPC** (tonic, no TLS) to that collector, and
//!   * installs the **W3C `traceparent`** propagator,
//!
//! so traces flow across process hops. The apiserver injects the context into
//! its mTLS HTTPS request to an agent pod's `/mcp` ([`inject_context`]); the
//! agent continues the trace from the incoming headers ([`set_parent`]). Both helpers
//! are no-ops when OTLP is off (no propagator installed ⇒ nothing is written and
//! no remote parent is attached), keeping the default path unchanged.

use opentelemetry::propagation::{Extractor, Injector};
use opentelemetry::trace::TracerProvider as _;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// When set, turns on OTLP span export to that collector endpoint. Absent ⇒ the
/// OTLP layer is never installed and tracing is the fmt-only default.
const OTLP_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Initialise tracing for a control-plane binary. Call exactly once, early in
/// `main`, from within the tokio runtime (the OTLP exporter binds to it).
///
/// `service_name` is reported as the OTLP `service.name` resource attribute
/// (e.g. `"agentctl-operator"`); it is unused when OTLP is off.
pub fn init(service_name: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer();

    // The OTLP layer is wired only when the endpoint env var is set; otherwise it
    // is `None`, which `Layer` treats as a no-op, leaving the fmt-only path intact.
    let otlp_layer = if std::env::var_os(OTLP_ENV).is_some() {
        match build_tracer(service_name) {
            Ok(tracer) => {
                opentelemetry::global::set_text_map_propagator(
                    opentelemetry_sdk::propagation::TraceContextPropagator::new(),
                );
                Some(tracing_opentelemetry::layer().with_tracer(tracer))
            }
            Err(e) => {
                // Never fail startup over telemetry: log and fall back to stdout.
                eprintln!(
                    "agentctl-telemetry: {OTLP_ENV} is set but the OTLP exporter \
                     failed to initialise ({e}); continuing with stdout tracing only"
                );
                None
            }
        }
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otlp_layer)
        .init();
}

/// Build a batched OTLP/gRPC tracer for `service_name`. The collector endpoint
/// and other knobs are read from the standard `OTEL_*` env vars by the exporter.
fn build_tracer(
    service_name: &str,
) -> Result<opentelemetry_sdk::trace::Tracer, Box<dyn std::error::Error>> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(service_name.to_string())
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();
    let tracer = provider.tracer("agentctl");
    // Make it the process-wide provider so `global::tracer(..)` and any library
    // spans share the same export pipeline.
    opentelemetry::global::set_tracer_provider(provider);
    Ok(tracer)
}

// --- W3C trace-context propagation ----------------------------------------

/// Inject the current span's W3C trace context (`traceparent`/`tracestate`) into
/// `headers`, so a downstream service can continue the trace. No-op unless a
/// propagator is installed (i.e. OTLP is enabled) — then it writes nothing.
pub fn inject_context(headers: &mut http::HeaderMap) {
    let cx = Span::current().context();
    opentelemetry::global::get_text_map_propagator(|prop| {
        prop.inject_context(&cx, &mut HeaderInjector(headers));
    });
}

/// Extract a W3C trace context from `headers` and set it as `span`'s parent, so
/// `span` continues the caller's trace across the process hop. No-op unless OTLP
/// is enabled (the extracted context is empty ⇒ no remote parent is attached).
pub fn set_parent(span: &Span, headers: &http::HeaderMap) {
    let parent = opentelemetry::global::get_text_map_propagator(|prop| {
        prop.extract(&HeaderExtractor(headers))
    });
    // Best-effort: `set_parent` errors only when no OTel layer is installed
    // (OTLP off), in which case there is nothing to parent and the miss is benign.
    let _ = span.set_parent(parent);
}

/// Adapts an `http::HeaderMap` to the OpenTelemetry [`Injector`] interface.
struct HeaderInjector<'a>(&'a mut http::HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(key.as_bytes()),
            http::HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, val);
        }
    }
}

/// Adapts an `http::HeaderMap` to the OpenTelemetry [`Extractor`] interface.
struct HeaderExtractor<'a>(&'a http::HeaderMap);

impl Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(http::HeaderName::as_str).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_is_noop_without_a_propagator() {
        // With no propagator installed (the default, OTLP off), injection must not
        // add any header — the downstream request carries no trace context.
        let mut headers = http::HeaderMap::new();
        inject_context(&mut headers);
        assert!(headers.is_empty());
    }

    #[test]
    fn header_extractor_reads_present_keys_only() {
        let mut headers = http::HeaderMap::new();
        headers.insert("traceparent", http::HeaderValue::from_static("abc"));
        let ex = HeaderExtractor(&headers);
        assert_eq!(ex.get("traceparent"), Some("abc"));
        assert_eq!(ex.get("missing"), None);
        assert_eq!(ex.keys(), vec!["traceparent"]);
    }

    #[test]
    fn header_injector_sets_a_valid_header() {
        let mut headers = http::HeaderMap::new();
        HeaderInjector(&mut headers).set("traceparent", "00-x-y-01".to_string());
        assert_eq!(
            headers.get("traceparent").and_then(|v| v.to_str().ok()),
            Some("00-x-y-01")
        );
    }
}
