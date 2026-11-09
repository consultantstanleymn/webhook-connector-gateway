//! A small downstream endpoint for demos, compose and load tests.
//!
//! `POST /events` records the call and answers 200. Set `FAIL_FIRST=n` (or
//! `FAIL_FIRST_N`) to answer 503 for the first n calls, `FAIL_RATE=0.0..1.0` to
//! fail a random share of calls, or `DELAY_MS` to slow every answer.
//! The mock applies each `Idempotency-Key` once: `unique` counts keys that got a
//! 200, and `duplicates` counts repeat deliveries of a key that was applied.
//! `POST /fail-rate/<0.0..1.0>` changes the failure share while the mock runs.
//! `GET /stats` shows the counts. With `STATS_FILE=<path>`, the mock also writes
//! the counts to that file after each request, so a restart keeps them for a lab.
//! If `WEBHOOK_SECRET` is set, the gateway signature is checked and a bad one gets 401.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use webhook_connector_gateway::{now_ms, signature};

/// Counts of deliveries by Idempotency-Key.
#[derive(Default)]
struct Stats {
    received: u64,
    /// Keys that were applied (answered 200), with the number of 200 answers.
    applied: HashMap<String, u64>,
}

impl Stats {
    fn record(&mut self, key: &str, applied: bool) {
        self.received += 1;
        if applied {
            *self.applied.entry(key.to_string()).or_insert(0) += 1;
        }
    }

    fn unique(&self) -> u64 {
        self.applied.len() as u64
    }

    /// Extra 200 answers for a key that was already applied.
    fn duplicates(&self) -> u64 {
        self.applied.values().map(|n| n - 1).sum()
    }

    fn json(&self, ok: u64) -> Value {
        json!({"calls": self.received, "ok": ok, "received": self.received,
               "unique": self.unique(), "duplicates": self.duplicates()})
    }
}

struct Mock {
    calls: AtomicU64,
    ok: AtomicU64,
    stats: Mutex<Stats>,
    stats_file: Option<String>,
    /// The failure share, as f64 bits, so a lab can change it while the mock runs.
    fail_rate: AtomicU64,
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
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let key = key.as_str();
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
    let rate = f64::from_bits(m.fail_rate.load(Ordering::SeqCst));
    let random_fail = rate > 0.0 && rand::random::<f64>() < rate;
    if n <= m.fail_first || random_fail {
        persist(&m, key, false);
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "try later"})),
        );
    }
    m.ok.fetch_add(1, Ordering::SeqCst);
    persist(&m, key, true);
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

/// Record one answer and write the stats file when one is set.
fn persist(m: &Mock, key: &str, applied: bool) {
    let mut st = m.stats.lock().unwrap_or_else(|e| e.into_inner());
    st.record(key, applied);
    if let Some(path) = &m.stats_file {
        let tmp = format!("{path}.tmp");
        let body = st.json(m.ok.load(Ordering::SeqCst)).to_string();
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Change the failure share while the mock runs. `POST /fail-rate/0.0`.
async fn set_fail_rate(State(m): State<Arc<Mock>>, Path(value): Path<f64>) -> StatusCode {
    if !(0.0..=1.0).contains(&value) {
        return StatusCode::BAD_REQUEST;
    }
    m.fail_rate.store(value.to_bits(), Ordering::SeqCst);
    StatusCode::NO_CONTENT
}

async fn stats(State(m): State<Arc<Mock>>) -> Json<Value> {
    let st = m.stats.lock().unwrap_or_else(|e| e.into_inner());
    Json(st.json(m.ok.load(Ordering::SeqCst)))
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
        stats: Mutex::new(Stats::default()),
        stats_file: std::env::var("STATS_FILE").ok().filter(|s| !s.is_empty()),
        fail_rate: AtomicU64::new(
            std::env::var("FAIL_RATE")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0)
                .to_bits(),
        ),
        fail_first: env_u64("FAIL_FIRST").max(env_u64("FAIL_FIRST_N")),
        delay_ms: env_u64("DELAY_MS"),
        secret: std::env::var("WEBHOOK_SECRET")
            .ok()
            .filter(|s| !s.is_empty()),
    });
    let app = Router::new()
        .route("/events", post(events))
        .route("/stats", get(stats))
        .route("/fail-rate/{value}", post(set_fail_rate))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("mock downstream listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_count_unique_keys_and_duplicates() {
        let mut s = Stats::default();
        s.record("a", true);
        s.record("a", true);
        s.record("b", false);
        s.record("b", true);
        s.record("c", true);
        assert_eq!(s.received, 5);
        assert_eq!(s.unique(), 3);
        assert_eq!(s.duplicates(), 1);
    }

    #[test]
    fn failed_calls_are_not_unique() {
        let mut s = Stats::default();
        s.record("a", false);
        assert_eq!((s.unique(), s.duplicates()), (0, 0));
    }
}
