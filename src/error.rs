//! API error type. Each variant maps to one HTTP status and a stable code.

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    Unauthorized(&'static str),
    #[error("{0}")]
    BadRequest(String),
    #[error("rate limit exceeded")]
    RateLimited { retry_after_secs: u64 },
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("admin API is disabled")]
    AdminDisabled,
    #[error("internal error")]
    Internal(#[source] anyhow::Error),
}

impl ApiError {
    /// Short stable code. Used in JSON bodies and as a metrics label.
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::Unauthorized(c) => c,
            ApiError::BadRequest(_) => "bad_request",
            ApiError::RateLimited { .. } => "rate_limited",
            ApiError::NotFound(_) => "not_found",
            ApiError::Conflict(_) => "idempotency_conflict",
            ApiError::AdminDisabled => "admin_disabled",
            ApiError::Internal(_) => "internal",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            ApiError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::AdminDisabled => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Internal(e.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        ApiError::Internal(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = match &self {
            ApiError::Internal(e) => {
                tracing::error!(error = %e, "internal error");
                "internal error".to_string() // never leak details to callers
            }
            other => other.to_string(),
        };
        let mut res = (
            self.status(),
            Json(json!({ "error": { "code": self.code(), "message": message } })),
        )
            .into_response();
        if let ApiError::RateLimited { retry_after_secs } = self {
            if let Ok(v) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                res.headers_mut().insert(RETRY_AFTER, v);
            }
        }
        res
    }
}
