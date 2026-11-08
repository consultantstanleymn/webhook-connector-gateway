//! A small downstream endpoint for demos, compose and load tests.
//!
//! `POST /events` records the call and answers 200. Set `FAIL_FIRST=n` to
//! answer 503 for the first n calls, or `DELAY_MS` to slow every answer.
//! `GET /stats` shows the counts. If `WEBHOOK_SECRET` is set, the gateway
//! signature is checked and a bad one gets 401.

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use webhook_connector_gateway::{now_ms, signature};

struct Mock {
    calls: AtomicU64,
    ok: AtomicU64,
    fail_first: u64,
    delay_ms: u64,
    secret: Option<String>,
}

async fn events(
    State(m): State<Arc<Mock>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    let n = m.calls.fetch_add(1, Ordering::SeqCst) + 1;
    if m.delay_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(m.delay_ms)).await;
    }
    if let Some(secret) = &m.secret {
        let sig = headers
            .get("x-gateway-signature")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if signature::verify(&[secret.as_bytes()], sig, &body, now_ms() / 1000, 300).is_err() {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "bad signature"})),
            );
        }
    }
    if n <= m.fail_first {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "try later"})),
        );
    }
    m.ok.fetch_add(1, Ordering::SeqCst);
    let trace = headers
        .get("traceparent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
    println!(
        "received event_id={} traceparent={} body={}",
        headers
            .get("x-gateway-event-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-"),
        trace,
        String::from_utf8_lossy(&body)
    );
    (StatusCode::OK, Json(json!({"ok": true})))
}

async fn stats(State(m): State<Arc<Mock>>) -> Json<Value> {
    Json(json!({"calls": m.calls.load(Ordering::SeqCst), "ok": m.ok.load(Ordering::SeqCst)}))
}

fn env_u64(name: &str) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:9000".into());
    let mock = Arc::new(Mock {
        calls: AtomicU64::new(0),
        ok: AtomicU64::new(0),
        fail_first: env_u64("FAIL_FIRST"),
        delay_ms: env_u64("DELAY_MS"),
        secret: std::env::var("WEBHOOK_SECRET")
            .ok()
            .filter(|s| !s.is_empty()),
    });
    let app = Router::new()
        .route("/events", post(events))
        .route("/stats", get(stats))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("mock downstream listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}
