//! Inbound endpoint: `POST /v1/webhooks`.
//!
//! Order of checks, cheapest first: body size (axum layer), API key, rate
//! limit, signature, JSON, idempotency, then the SQL insert. The 202 is
//! only sent after the insert commits.

use crate::{
    app::AppState,
    error::ApiError,
    now_ms, signature,
    store::{Enqueued, NewEvent, hash_hex},
    trace,
};
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
};
use serde_json::{Value, json};
use std::time::Instant;
use tracing::Instrument;

pub const API_KEY_HEADER: &str = "x-api-key";
pub const SIGNATURE_HEADER: &str = "x-signature";
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";

pub async fn ingest(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    match ingest_inner(&st, &headers, &body).await {
        Ok(ok) => Ok(ok),
        Err(e) => {
            st.metrics
                .requests_rejected
                .with_label_values(&[e.code()])
                .inc();
            Err(e)
        }
    }
}

fn header_str<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

async fn ingest_inner(
    st: &AppState,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let api_key =
        header_str(headers, API_KEY_HEADER).ok_or(ApiError::Unauthorized("missing_api_key"))?;
    let tenant = st
        .store
        .tenant_by_key_hash(&hash_hex(api_key.as_bytes()))
        .await?
        .ok_or(ApiError::Unauthorized("invalid_api_key"))?;
    let tenant_cfg = st
        .cfg
        .tenant(&tenant.id)
        .ok_or(ApiError::Unauthorized("invalid_api_key"))?;

    if let Err(wait) = st
        .limiter
        .check(&tenant.id, tenant_cfg.rate_limit.into(), Instant::now())
    {
        return Err(ApiError::RateLimited {
            retry_after_secs: wait.as_secs() + 1,
        });
    }

    let sig =
        header_str(headers, SIGNATURE_HEADER).ok_or(ApiError::Unauthorized("missing_signature"))?;
    let secret_strings = tenant.valid_secrets(now_ms(), &st.keyring);
    let secrets: Vec<&[u8]> = secret_strings.iter().map(|s| s.as_bytes()).collect();
    signature::verify(
        &secrets,
        sig,
        body,
        now_ms() / 1000,
        st.cfg.server.signature_tolerance_secs,
    )
    .map_err(|e| match e {
        signature::SignatureError::Malformed => ApiError::Unauthorized("malformed_signature"),
        signature::SignatureError::TimestampOutOfWindow => {
            ApiError::Unauthorized("stale_timestamp")
        }
        signature::SignatureError::Mismatch => ApiError::Unauthorized("bad_signature"),
    })?;

    let text =
        std::str::from_utf8(body).map_err(|_| ApiError::BadRequest("body must be UTF-8".into()))?;
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(_)) => {}
        Ok(_) => return Err(ApiError::BadRequest("body must be a JSON object".into())),
        Err(e) => return Err(ApiError::BadRequest(format!("body is not valid JSON: {e}"))),
    }

    let key = match header_str(headers, IDEMPOTENCY_HEADER) {
        Some(k)
            if !k.is_empty() && k.len() <= 255 && k.bytes().all(|b| (0x21..0x7f).contains(&b)) =>
        {
            k.to_string()
        }
        Some(_) => {
            return Err(ApiError::BadRequest(
                "Idempotency-Key must be 1-255 printable ASCII characters".into(),
            ));
        }
        None => format!("auto-{}", uuid::Uuid::new_v4()),
    };
    let trace_id = header_str(headers, "traceparent")
        .and_then(trace::parse_traceparent)
        .unwrap_or_else(trace::new_trace_id);

    check_backpressure(st).await?;

    let id = uuid::Uuid::new_v4().to_string();
    let span = tracing::info_span!("ingest", tenant.id = %tenant.id, event.id = %id);
    crate::observability::set_trace_parent(&span, &trace_id);
    let outcome = st
        .store
        .enqueue(&NewEvent {
            id: &id,
            tenant_id: &tenant.id,
            idempotency_key: &key,
            payload: text,
            trace_id: &trace_id,
        })
        .instrument(span)
        .await?;
    match outcome {
        Enqueued::New => {
            bump_pending(st);
            st.metrics
                .events_received
                .with_label_values(&[&tenant.id])
                .inc();
            st.notify.notify_one();
            tracing::info!(event_id = %id, tenant = %tenant.id, trace_id = %trace_id, "event accepted");
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({ "id": id, "status": "queued" })),
            ))
        }
        Enqueued::Duplicate { id } => Ok((
            StatusCode::OK,
            Json(json!({ "id": id, "status": "duplicate" })),
        )),
        Enqueued::Conflict => Err(ApiError::Conflict(
            "this Idempotency-Key was used with a different body".into(),
        )),
    }
}

/// How long a read of the pending count stays valid.
const PENDING_CACHE_MS: u128 = 250;

/// Refuse new events when pending plus processing events reach `max_pending`.
/// The count is cached for 250 ms, and each accepted event adds one to the
/// cache, so a burst cannot pass the limit between two reads.
async fn check_backpressure(st: &AppState) -> Result<(), ApiError> {
    let stale = {
        let c = st.pending_cache.lock().unwrap_or_else(|e| e.into_inner());
        c.0.elapsed().as_millis() >= PENDING_CACHE_MS
    };
    if stale {
        let rows = st.store.depth().await?;
        let n: i64 = rows
            .iter()
            .filter(|(s, _)| s == "pending" || s == "processing")
            .map(|(_, n)| *n)
            .sum();
        *st.pending_cache.lock().unwrap_or_else(|e| e.into_inner()) = (Instant::now(), n);
    }
    let n = st.pending_cache.lock().unwrap_or_else(|e| e.into_inner()).1;
    if n >= st.cfg.queue.max_pending {
        st.metrics
            .ingest_shed
            .with_label_values(&["queue_full"])
            .inc();
        return Err(ApiError::QueueFull {
            retry_after_secs: st.cfg.queue.retry_after_secs,
        });
    }
    Ok(())
}

fn bump_pending(st: &AppState) {
    st.pending_cache.lock().unwrap_or_else(|e| e.into_inner()).1 += 1;
}
