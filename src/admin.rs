//! Admin operations. The CLI and the HTTP admin API both call these.

use crate::{
    app::AppState,
    config::Config,
    error::ApiError,
    now_ms,
    store::{Store, hash_hex},
};
use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::header::AUTHORIZATION,
    middleware::Next,
    response::Response,
};
use rand::RngExt;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle_eq::constant_time_eq;

/// 32 random bytes as hex, with a prefix.
pub fn random_token(prefix: &str) -> String {
    let bytes: [u8; 32] = rand::rng().random();
    format!("{prefix}{}", hex::encode(bytes))
}

/// Credentials shown once, when a tenant is created.
pub struct NewTenantCreds {
    pub tenant_id: String,
    pub api_key: String,
    pub secret: String,
}

/// Create the tenants from the config that do not exist yet.
/// Existing tenants are not touched, so a rotated secret survives restarts.
pub async fn seed_tenants(
    store: &Store,
    cfg: &Config,
    keyring: &crate::secrets::Keyring,
) -> anyhow::Result<Vec<NewTenantCreds>> {
    let mut created = Vec::new();
    for t in &cfg.tenants {
        let api_key = t.api_key.clone().unwrap_or_else(|| random_token("gwk_"));
        let secret = t.secret.clone().unwrap_or_else(|| random_token("whsec_"));
        if store
            .create_tenant_if_absent(&t.id, &hash_hex(api_key.as_bytes()), &keyring.seal(&secret))
            .await?
        {
            created.push(NewTenantCreds {
                tenant_id: t.id.clone(),
                api_key,
                secret,
            });
        }
    }
    Ok(created)
}

/// Make a new secret. The old one works for `grace_secs` more seconds.
pub async fn rotate_secret(
    store: &Store,
    keyring: &crate::secrets::Keyring,
    tenant: &str,
    grace_secs: i64,
) -> Result<String, ApiError> {
    let new_secret = random_token("whsec_");
    let expires = now_ms() + grace_secs.max(0) * 1000;
    if store
        .rotate_secret(tenant, &keyring.seal(&new_secret), expires)
        .await?
    {
        tracing::info!(tenant, grace_secs, "tenant secret rotated");
        Ok(new_secret)
    } else {
        Err(ApiError::NotFound(format!("tenant {tenant} not found")))
    }
}

/// Counts from a reseal run.
#[derive(Debug, PartialEq, Eq)]
pub struct Resealed {
    pub resealed: u64,
    pub unchanged: u64,
}

/// Seal every stored secret with the key `to_kid`. A value that already uses that key
/// stays as it is, so a second run changes nothing. Each tenant is one atomic update,
/// and it applies only if no rotation changed the row in the meantime.
pub async fn reseal_secrets(
    store: &Store,
    keyring: &crate::secrets::Keyring,
    to_kid: &str,
) -> anyhow::Result<Resealed> {
    let mut out = Resealed {
        resealed: 0,
        unchanged: 0,
    };
    for (id, secret, prev) in store.list_tenant_secrets().await? {
        let new_secret = reseal_one(keyring, to_kid, &secret)?;
        let new_prev = match &prev {
            Some(p) => Some(reseal_one(keyring, to_kid, p)?),
            None => None,
        };
        if new_secret == secret && new_prev == prev {
            out.unchanged += 1;
            continue;
        }
        if store
            .replace_tenant_secrets(
                &id,
                (&secret, prev.as_deref()),
                (&new_secret, new_prev.as_deref()),
            )
            .await?
        {
            out.resealed += 1;
        } else {
            anyhow::bail!("tenant {id} changed during the reseal; run the command again");
        }
    }
    Ok(out)
}

fn reseal_one(
    keyring: &crate::secrets::Keyring,
    to_kid: &str,
    stored: &str,
) -> anyhow::Result<String> {
    if crate::secrets::Keyring::kid_of(stored) == Some(to_kid) {
        return Ok(stored.to_string());
    }
    keyring.seal_to(to_kid, &keyring.open(stored)?)
}

/// Bearer token check for `/admin`.
pub async fn require_admin(
    State(st): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = &st.cfg.server.admin_token;
    if token.is_empty() {
        return Err(ApiError::AdminDisabled);
    }
    let given = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !constant_time_eq(given.as_bytes(), token.as_bytes()) {
        return Err(ApiError::Unauthorized("invalid_admin_token"));
    }
    Ok(next.run(req).await)
}

#[derive(Deserialize)]
pub struct ListQuery {
    status: Option<String>,
    tenant: Option<String>,
    limit: Option<i64>,
}

pub async fn list_events(
    State(st): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let rows = st
        .store
        .list_events(
            q.status.as_deref(),
            q.tenant.as_deref(),
            q.limit.unwrap_or(100),
        )
        .await?;
    Ok(Json(json!({ "events": rows })))
}

pub async fn get_event(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match st.store.get_event(&id).await? {
        Some(e) => Ok(Json(json!(e))),
        None => Err(ApiError::NotFound(format!("event {id} not found"))),
    }
}

pub async fn list_dead_letters(
    State(st): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({ "dead_letters": st.store.list_dead_letters(q.limit.unwrap_or(100)).await? }),
    ))
}

pub async fn replay(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if st.store.replay(&id).await? {
        st.notify.notify_one();
        tracing::info!(event_id = %id, "event replayed");
        Ok(Json(json!({ "id": id, "status": "pending" })))
    } else {
        Err(ApiError::NotFound(format!(
            "event {id} is not a dead letter"
        )))
    }
}

#[derive(Deserialize, Default)]
pub struct RotateQuery {
    grace_secs: Option<i64>,
}

pub async fn rotate(
    State(st): State<AppState>,
    Path(tenant): Path<String>,
    Query(q): Query<RotateQuery>,
) -> Result<Json<Value>, ApiError> {
    let grace = q.grace_secs.unwrap_or(st.cfg.server.rotation_grace_secs);
    let secret = rotate_secret(&st.store, &st.keyring, &tenant, grace).await?;
    Ok(Json(
        json!({ "tenant": tenant, "secret": secret, "old_secret_valid_for_secs": grace }),
    ))
}

/// Tiny constant time compare, so the token check leaks no length of match.
mod subtle_eq {
    pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        if a.len() != b.len() {
            return false;
        }
        a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::subtle_eq::constant_time_eq;

    #[test]
    fn compare() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
