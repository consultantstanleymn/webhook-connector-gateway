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
    let secrets: Vec<&[u8]> = tenant
        .valid_secrets(now_ms())
        .into_iter()
        .map(str::as_bytes)
        .collect();
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

    let id = uuid::Uuid::new_v4().to_string();
    let outcome = st
        .store
        .enqueue(&NewEvent {
            id: &id,
            tenant_id: &tenant.id,
            idempotency_key: &key,
            payload: text,
            trace_id: &trace_id,
        })
        .await?;
    match outcome {
        Enqueued::New => {
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
