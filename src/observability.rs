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

/// JSON logs on stdout. Level comes from `RUST_LOG`, default `info`.
pub fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(true)
        .try_init();
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
