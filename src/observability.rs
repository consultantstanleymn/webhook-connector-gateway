//! Structured JSON logs and the request id / latency middleware.

use crate::app::AppState;
use axum::{
    extract::{MatchedPath, Request, State},
    http::{HeaderName, HeaderValue},
    middleware::Next,
    response::Response,
};
use std::time::Instant;
use tracing::Instrument;

pub const REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

/// True when an OTLP endpoint is configured. Without one, no exporter starts.
pub fn exporter_enabled(endpoint: Option<&str>) -> bool {
    endpoint.is_some_and(|e| !e.trim().is_empty())
}

/// JSON logs on stdout. Level comes from `RUST_LOG`, default `info`.
/// When `OTEL_EXPORTER_OTLP_ENDPOINT` is set, spans also go to that OTLP endpoint
/// (gRPC). The caller must call `shutdown()` on the returned provider before exit,
/// so the last spans are sent.
pub fn init_logging() -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_current_span(true);
    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
    let provider = if exporter_enabled(endpoint.as_deref()) {
        build_provider()
            .map_err(|e| eprintln!("span export is off: {e}"))
            .ok()
    } else {
        None
    };
    let otel_layer = provider
        .as_ref()
        .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer("webhook-connector-gateway")));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .with(otel_layer)
        .try_init();
    provider
}

fn build_provider() -> anyhow::Result<opentelemetry_sdk::trace::SdkTracerProvider> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .build()?;
    Ok(opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name("webhook-connector-gateway")
                .build(),
        )
        .build())
}

/// Make the span a child of a remote parent with this trace id, so spans of one
/// event share one trace. The parent span id is made up, because only the trace id
/// travels with the event (see `trace.rs`).
pub fn set_trace_parent(span: &tracing::Span, trace_id: &str) {
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let Ok(tid) = TraceId::from_hex(trace_id) else {
        return;
    };
    let sid = SpanId::from_hex(&crate::trace::new_span_id()).unwrap_or(SpanId::INVALID);
    let sc = SpanContext::new(tid, sid, TraceFlags::SAMPLED, true, TraceState::default());
    let _ = span.set_parent(opentelemetry::Context::new().with_remote_span_context(sc));
}

fn clean_request_id(v: Option<&HeaderValue>) -> String {
    v.and_then(|v| v.to_str().ok())
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

/// Give each request an id, log it, time it, and echo the id back.
pub async fn request_middleware(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let request_id = clean_request_id(req.headers().get(&REQUEST_ID));
    let method = req.method().to_string();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "unmatched".into());
    let span =
        tracing::info_span!("http", request_id = %request_id, method = %method, route = %route);
    let start = Instant::now();
    let mut res = next.run(req).instrument(span.clone()).await;
    let elapsed = start.elapsed();
    let status = res.status().as_u16();
    st.metrics
        .http_duration
        .with_label_values(&[&route, &method, &status.to_string()])
        .observe(elapsed.as_secs_f64());
    span.in_scope(|| {
        tracing::info!(
            status,
            latency_ms = elapsed.as_secs_f64() * 1000.0,
            "request done"
        );
    });
    if let Ok(v) = HeaderValue::from_str(&request_id) {
        res.headers_mut().insert(REQUEST_ID, v);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_exporter_without_env() {
        assert!(!exporter_enabled(None));
        assert!(!exporter_enabled(Some("")));
        assert!(!exporter_enabled(Some("   ")));
        assert!(exporter_enabled(Some("http://127.0.0.1:25617")));
    }
}
