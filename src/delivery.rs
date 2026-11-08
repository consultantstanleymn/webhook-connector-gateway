//! Outbound delivery to the downstream REST endpoint.

use crate::{signature, trace};
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum DeliveryResult {
    Success,
    /// Worth trying again: timeout, network error, 5xx, 408, 429.
    Retryable(String),
    /// Retrying will not help: other 4xx.
    Permanent(String),
}

#[derive(Clone)]
pub struct Deliverer {
    client: reqwest::Client,
}

impl Deliverer {
    pub fn new(timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .user_agent("webhook-connector-gateway/0.1")
            .build()?;
        Ok(Self { client })
    }

    /// POST the body. The call is signed with the tenant secret and carries
    /// the event id as `Idempotency-Key` so the downstream can dedupe, and a
    /// `traceparent` with the original trace id.
    pub async fn deliver(
        &self,
        url: &str,
        secret: &str,
        event_id: &str,
        trace_id: &str,
        body: Vec<u8>,
    ) -> DeliveryResult {
        let ts = crate::now_ms() / 1000;
        let sig = signature::header_value(secret.as_bytes(), ts, &body);
        let res = self
            .client
            .post(url)
            .header("content-type", "application/json")
            .header("x-gateway-signature", sig)
            .header("x-gateway-event-id", event_id)
            .header("idempotency-key", event_id)
            .header("traceparent", trace::traceparent(trace_id))
            .body(body)
            .send()
            .await;
        match res {
            Ok(r) => classify(r.status().as_u16()),
            Err(e) if e.is_timeout() => DeliveryResult::Retryable("timeout".into()),
            Err(e) if e.is_connect() => DeliveryResult::Retryable("connection failed".into()),
            Err(e) => DeliveryResult::Retryable(format!("request error: {}", e.without_url())),
        }
    }
}

pub fn classify(status: u16) -> DeliveryResult {
    match status {
        200..=299 => DeliveryResult::Success,
        408 | 429 | 500..=599 => DeliveryResult::Retryable(format!("downstream status {status}")),
        _ => DeliveryResult::Permanent(format!("downstream status {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_statuses() {
        assert_eq!(classify(204), DeliveryResult::Success);
        assert!(matches!(classify(503), DeliveryResult::Retryable(_)));
        assert!(matches!(classify(429), DeliveryResult::Retryable(_)));
        assert!(matches!(classify(400), DeliveryResult::Permanent(_)));
        assert!(matches!(classify(404), DeliveryResult::Permanent(_)));
        assert!(matches!(classify(301), DeliveryResult::Permanent(_)));
    }
}
