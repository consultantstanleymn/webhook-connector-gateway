//! Webhook and connector gateway.
//!
//! Customers send signed webhooks to the gateway. The gateway checks them,
//! stores them in SQL, acknowledges with 202, and delivers them downstream
//! from a worker pool with retries.

pub mod admin;
pub mod app;
pub mod backoff;
pub mod breaker;
pub mod config;
pub mod delivery;
pub mod error;
pub mod ingest;
pub mod metrics;
pub mod observability;
pub mod ratelimit;
pub mod signature;
pub mod store;
pub mod trace;
pub mod transform;
pub mod worker;

/// Current Unix time in milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
