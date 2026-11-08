//! Shared state and the HTTP router.

use crate::{
    admin, config::Config, ingest, metrics::Metrics, observability, ratelimit::RateLimiter,
    store::Store,
};
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::{StatusCode, header::CONTENT_TYPE},
    middleware,
    response::IntoResponse,
    routing::{get, post},
};
use std::sync::Arc;
use tokio::sync::Notify;

#[derive(Clone)]
pub struct AppState {
    pub store: Store,
    pub cfg: Arc<Config>,
    pub limiter: Arc<RateLimiter>,
    pub metrics: Metrics,
    /// Wakes idle workers when an event arrives.
    pub notify: Arc<Notify>,
}

impl AppState {
    pub fn new(store: Store, cfg: Arc<Config>, metrics: Metrics, notify: Arc<Notify>) -> Self {
        Self {
            store,
            cfg,
            limiter: Arc::new(RateLimiter::default()),
            metrics,
            notify,
        }
    }
}

pub fn router(st: AppState) -> Router {
    let admin_routes = Router::new()
        .route("/events", get(admin::list_events))
        .route("/events/{id}", get(admin::get_event))
        .route("/events/{id}/replay", post(admin::replay))
        .route("/dead-letters", get(admin::list_dead_letters))
        .route("/tenants/{id}/rotate-secret", post(admin::rotate))
        .route_layer(middleware::from_fn_with_state(
            st.clone(),
            admin::require_admin,
        ));

    Router::new()
        .route("/v1/webhooks", post(ingest::ingest))
        .layer(DefaultBodyLimit::max(st.cfg.server.max_body_bytes))
        .nest("/admin", admin_routes)
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics))
        .layer(middleware::from_fn_with_state(
            st.clone(),
            observability::request_middleware,
        ))
        .with_state(st)
}

async fn readyz(State(st): State<AppState>) -> impl IntoResponse {
    match st.store.ping().await {
        Ok(()) => (StatusCode::OK, "ready"),
        Err(e) => {
            tracing::error!(error = %e, "readiness check failed");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable")
        }
    }
}

async fn metrics(State(st): State<AppState>) -> impl IntoResponse {
    if let Ok(rows) = st.store.depth().await {
        st.metrics.set_depth(&rows);
    }
    (
        [(CONTENT_TYPE, "text/plain; version=0.0.4")],
        st.metrics.render(),
    )
}

/// Everything the server needs, built from a config.
pub struct Runtime {
    pub state: AppState,
    pub engine: crate::worker::Engine,
    /// Credentials of tenants created by this start. Shown once.
    pub created: Vec<admin::NewTenantCreds>,
}

impl Runtime {
    pub async fn build(cfg: Config) -> anyhow::Result<Self> {
        let cfg = Arc::new(cfg);
        let store = Store::connect(&cfg.database_url).await?;
        let created = admin::seed_tenants(&store, &cfg).await?;
        let metrics = Metrics::new();
        let notify = Arc::new(Notify::new());
        let state = AppState::new(store.clone(), cfg.clone(), metrics.clone(), notify.clone());
        let engine = crate::worker::Engine::new(store, cfg, metrics, notify)?;
        Ok(Self {
            state,
            engine,
            created,
        })
    }
}
