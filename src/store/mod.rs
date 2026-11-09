//! The store port. One enum with one variant for each backend. All SQL lives in
//! the adapter files (see docs/DECISIONS.md).

use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::FromRow;

macro_rules! summary_cols {
    () => {
        "id, tenant_id, status, attempts, next_attempt_at, last_error, trace_id, created_at, updated_at"
    };
}

mod sqlite;

#[derive(Debug, Clone, FromRow)]
pub struct TenantRow {
    pub id: String,
    pub secret: String,
    pub prev_secret: Option<String>,
    pub prev_expires_at: Option<i64>,
}

impl TenantRow {
    /// Secrets that are valid at `now_ms`: the current one, and the previous
    /// one while its grace window is open.
    pub fn valid_secrets(&self, now_ms: i64) -> Vec<&str> {
        let mut v = vec![self.secret.as_str()];
        if let (Some(p), Some(exp)) = (&self.prev_secret, self.prev_expires_at) {
            if exp > now_ms {
                v.push(p.as_str());
            }
        }
        v
    }
}

pub struct NewEvent<'a> {
    pub id: &'a str,
    pub tenant_id: &'a str,
    pub idempotency_key: &'a str,
    pub payload: &'a str,
    pub trace_id: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Enqueued {
    New,
    Duplicate {
        id: String,
    },
    /// Same key, different body.
    Conflict,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct EventSummary {
    pub id: String,
    pub tenant_id: String,
    pub status: String,
    pub attempts: i64,
    pub next_attempt_at: i64,
    pub last_error: Option<String>,
    pub trace_id: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct EventDetail {
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub summary: EventSummary,
    pub idempotency_key: String,
    pub payload: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct Claimed {
    pub id: String,
    pub tenant_id: String,
    pub payload: String,
    pub trace_id: String,
    pub attempts: i64,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct DeadLetter {
    pub event_id: String,
    pub tenant_id: String,
    pub payload: String,
    pub attempts: i64,
    pub last_error: String,
    pub died_at: i64,
    pub replayed_at: Option<i64>,
}

pub fn hash_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The store. Methods forward to the backend that `connect` chose.
#[derive(Clone)]
pub enum Store {
    Sqlite(sqlite::SqliteStore),
}

impl Store {
    /// Pick the backend by URL scheme.
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        if url.starts_with("sqlite:") {
            return Ok(Store::Sqlite(sqlite::SqliteStore::connect(url).await?));
        }
        if url.starts_with("postgres:") || url.starts_with("postgresql:") {
            anyhow::bail!("postgres backend not built");
        }
        anyhow::bail!("unknown database url scheme (use sqlite: or postgres:)")
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.ping().await,
        }
    }

    pub async fn create_tenant_if_absent(
        &self,
        id: &str,
        api_key_hash: &str,
        secret: &str,
    ) -> Result<bool, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.create_tenant_if_absent(id, api_key_hash, secret).await,
        }
    }

    pub async fn tenant_by_key_hash(&self, hash: &str) -> Result<Option<TenantRow>, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.tenant_by_key_hash(hash).await,
        }
    }

    pub async fn tenant_by_id(&self, id: &str) -> Result<Option<TenantRow>, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.tenant_by_id(id).await,
        }
    }

    pub async fn rotate_secret(
        &self,
        id: &str,
        new_secret: &str,
        prev_expires_at: i64,
    ) -> Result<bool, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.rotate_secret(id, new_secret, prev_expires_at).await,
        }
    }

    pub async fn enqueue(&self, e: &NewEvent<'_>) -> Result<Enqueued, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.enqueue(e).await,
        }
    }

    pub async fn claim(&self, now: i64, lease_ms: i64) -> Result<Option<Claimed>, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.claim(now, lease_ms).await,
        }
    }

    pub async fn mark_delivered(&self, id: &str) -> Result<(), sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.mark_delivered(id).await,
        }
    }

    pub async fn mark_filtered(&self, id: &str) -> Result<(), sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.mark_filtered(id).await,
        }
    }

    pub async fn schedule_retry(
        &self,
        id: &str,
        next_at: i64,
        err: &str,
    ) -> Result<(), sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.schedule_retry(id, next_at, err).await,
        }
    }

    pub async fn release(&self, id: &str, next_at: i64) -> Result<(), sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.release(id, next_at).await,
        }
    }

    pub async fn dead_letter(&self, id: &str, err: &str) -> Result<(), sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.dead_letter(id, err).await,
        }
    }

    pub async fn replay(&self, id: &str) -> Result<bool, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.replay(id).await,
        }
    }

    pub async fn replay_all_dead(&self) -> Result<u64, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.replay_all_dead().await,
        }
    }

    pub async fn list_events(
        &self,
        status: Option<&str>,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<EventSummary>, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.list_events(status, tenant, limit).await,
        }
    }

    pub async fn get_event(&self, id: &str) -> Result<Option<EventDetail>, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.get_event(id).await,
        }
    }

    pub async fn list_dead_letters(&self, limit: i64) -> Result<Vec<DeadLetter>, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.list_dead_letters(limit).await,
        }
    }

    pub async fn depth(&self) -> Result<Vec<(String, i64)>, sqlx::Error> {
        match self {
            Store::Sqlite(s) => s.depth().await,
        }
    }
}
