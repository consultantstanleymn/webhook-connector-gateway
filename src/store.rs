//! SQL storage. All SQL lives here, so a Postgres backend means one new
//! implementation of this module (see docs/DECISIONS.md).

use crate::now_ms;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{FromRow, SqlitePool};
use std::str::FromStr;
use std::time::Duration;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tenants (
    id              TEXT PRIMARY KEY,
    api_key_hash    TEXT NOT NULL UNIQUE,
    secret          TEXT NOT NULL,
    prev_secret     TEXT,
    prev_expires_at INTEGER,
    created_at      INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id              TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload_hash    TEXT NOT NULL,
    payload         TEXT NOT NULL,
    trace_id        TEXT NOT NULL,
    status          TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL,
    locked_until    INTEGER NOT NULL DEFAULT 0,
    last_error      TEXT,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    UNIQUE (tenant_id, idempotency_key)
);
CREATE INDEX IF NOT EXISTS events_queue_idx ON events (status, next_attempt_at);
CREATE INDEX IF NOT EXISTS events_tenant_idx ON events (tenant_id, created_at);
CREATE TABLE IF NOT EXISTS dead_letters (
    event_id    TEXT PRIMARY KEY,
    tenant_id   TEXT NOT NULL,
    payload     TEXT NOT NULL,
    attempts    INTEGER NOT NULL,
    last_error  TEXT NOT NULL,
    died_at     INTEGER NOT NULL,
    replayed_at INTEGER
);
"#;

#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
}

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

macro_rules! summary_cols {
    () => {
        "id, tenant_id, status, attempts, next_attempt_at, last_error, trace_id, created_at, updated_at"
    };
}

pub fn hash_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

impl Store {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let opts = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::raw_sql(SCHEMA).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    // ---- tenants ----

    /// Insert a tenant if it does not exist. Returns true when created.
    pub async fn create_tenant_if_absent(
        &self,
        id: &str,
        api_key_hash: &str,
        secret: &str,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "INSERT INTO tenants (id, api_key_hash, secret, created_at) VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(id)
        .bind(api_key_hash)
        .bind(secret)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn tenant_by_key_hash(&self, hash: &str) -> Result<Option<TenantRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT id, secret, prev_secret, prev_expires_at FROM tenants WHERE api_key_hash = ?",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn tenant_by_id(&self, id: &str) -> Result<Option<TenantRow>, sqlx::Error> {
        sqlx::query_as("SELECT id, secret, prev_secret, prev_expires_at FROM tenants WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    /// Set a new secret. The old one stays valid until `prev_expires_at`.
    pub async fn rotate_secret(
        &self,
        id: &str,
        new_secret: &str,
        prev_expires_at: i64,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE tenants SET prev_secret = secret, prev_expires_at = ?, secret = ? WHERE id = ?",
        )
        .bind(prev_expires_at)
        .bind(new_secret)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    // ---- queue ----

    /// Persist an event. One statement, so the 202 is only sent after commit.
    pub async fn enqueue(&self, e: &NewEvent<'_>) -> Result<Enqueued, sqlx::Error> {
        let now = now_ms();
        let hash = hash_hex(e.payload.as_bytes());
        let r = sqlx::query(
            "INSERT INTO events (id, tenant_id, idempotency_key, payload_hash, payload, trace_id,
                                 status, attempts, next_attempt_at, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 'pending', 0, ?, ?, ?)
             ON CONFLICT(tenant_id, idempotency_key) DO NOTHING",
        )
        .bind(e.id)
        .bind(e.tenant_id)
        .bind(e.idempotency_key)
        .bind(&hash)
        .bind(e.payload)
        .bind(e.trace_id)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 1 {
            return Ok(Enqueued::New);
        }
        let (id, old_hash): (String, String) = sqlx::query_as(
            "SELECT id, payload_hash FROM events WHERE tenant_id = ? AND idempotency_key = ?",
        )
        .bind(e.tenant_id)
        .bind(e.idempotency_key)
        .fetch_one(&self.pool)
        .await?;
        Ok(if old_hash == hash {
            Enqueued::Duplicate { id }
        } else {
            Enqueued::Conflict
        })
    }

    /// Take the next due event. Also takes events whose lease ran out, which
    /// happens when a worker crashed.
    pub async fn claim(&self, now: i64, lease_ms: i64) -> Result<Option<Claimed>, sqlx::Error> {
        sqlx::query_as(
            "UPDATE events SET status = 'processing', attempts = attempts + 1,
                    locked_until = ?, updated_at = ?
             WHERE id = (SELECT id FROM events
                         WHERE (status = 'pending' AND next_attempt_at <= ?)
                            OR (status = 'processing' AND locked_until <= ?)
                         ORDER BY next_attempt_at LIMIT 1)
             RETURNING id, tenant_id, payload, trace_id, attempts",
        )
        .bind(now + lease_ms)
        .bind(now)
        .bind(now)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
    }

    async fn finish(&self, id: &str, status: &str, err: Option<&str>) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE events SET status = ?, last_error = ?, updated_at = ? WHERE id = ?")
            .bind(status)
            .bind(err)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    pub async fn mark_delivered(&self, id: &str) -> Result<(), sqlx::Error> {
        self.finish(id, "delivered", None).await
    }

    pub async fn mark_filtered(&self, id: &str) -> Result<(), sqlx::Error> {
        self.finish(id, "filtered", None).await
    }

    pub async fn schedule_retry(
        &self,
        id: &str,
        next_at: i64,
        err: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE events SET status = 'pending', next_attempt_at = ?, last_error = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(next_at)
        .bind(err)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Give an event back without counting the attempt (circuit open).
    pub async fn release(&self, id: &str, next_at: i64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE events SET status = 'pending', attempts = MAX(attempts - 1, 0),
                    next_attempt_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(next_at)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Move an event to the dead letter table.
    pub async fn dead_letter(&self, id: &str, err: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let now = now_ms();
        sqlx::query(
            "UPDATE events SET status = 'dead', last_error = ?, updated_at = ? WHERE id = ?",
        )
        .bind(err)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO dead_letters (event_id, tenant_id, payload, attempts, last_error, died_at)
             SELECT id, tenant_id, payload, attempts, ?, ? FROM events WHERE id = ?
             ON CONFLICT(event_id) DO UPDATE SET attempts = excluded.attempts,
                 last_error = excluded.last_error, died_at = excluded.died_at, replayed_at = NULL",
        )
        .bind(err)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    /// Put a dead event back in the queue. False when it is not dead.
    pub async fn replay(&self, id: &str) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let now = now_ms();
        let r = sqlx::query(
            "UPDATE events SET status = 'pending', attempts = 0, next_attempt_at = ?, updated_at = ?
             WHERE id = ? AND status = 'dead'",
        )
        .bind(now)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 1 {
            sqlx::query("UPDATE dead_letters SET replayed_at = ? WHERE event_id = ?")
                .bind(now)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn replay_all_dead(&self) -> Result<u64, sqlx::Error> {
        let ids: Vec<(String,)> = sqlx::query_as("SELECT id FROM events WHERE status = 'dead'")
            .fetch_all(&self.pool)
            .await?;
        let mut n = 0;
        for (id,) in ids {
            if self.replay(&id).await? {
                n += 1;
            }
        }
        Ok(n)
    }

    // ---- reading ----

    pub async fn list_events(
        &self,
        status: Option<&str>,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<EventSummary>, sqlx::Error> {
        sqlx::query_as(concat!(
            "SELECT ",
            summary_cols!(),
            " FROM events
             WHERE (?1 IS NULL OR status = ?1) AND (?2 IS NULL OR tenant_id = ?2)
             ORDER BY created_at DESC, id LIMIT ?3"
        ))
        .bind(status)
        .bind(tenant)
        .bind(limit.clamp(1, 1000))
        .fetch_all(&self.pool)
        .await
    }

    pub async fn get_event(&self, id: &str) -> Result<Option<EventDetail>, sqlx::Error> {
        sqlx::query_as(concat!(
            "SELECT ",
            summary_cols!(),
            ", idempotency_key, payload FROM events WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn list_dead_letters(&self, limit: i64) -> Result<Vec<DeadLetter>, sqlx::Error> {
        sqlx::query_as(
            "SELECT event_id, tenant_id, payload, attempts, last_error, died_at, replayed_at
             FROM dead_letters ORDER BY died_at DESC LIMIT ?",
        )
        .bind(limit.clamp(1, 1000))
        .fetch_all(&self.pool)
        .await
    }

    /// Event counts by status, for the queue depth gauge.
    pub async fn depth(&self) -> Result<Vec<(String, i64)>, sqlx::Error> {
        sqlx::query_as("SELECT status, COUNT(*) FROM events GROUP BY status")
            .fetch_all(&self.pool)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem_store() -> Store {
        let path = std::env::temp_dir().join(format!("wcg-{}.db", uuid::Uuid::new_v4()));
        Store::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap()
    }

    fn ev<'a>(id: &'a str, key: &'a str, payload: &'a str) -> NewEvent<'a> {
        NewEvent {
            id,
            tenant_id: "t",
            idempotency_key: key,
            payload,
            trace_id: "tr",
        }
    }

    #[tokio::test]
    async fn enqueue_dedupes_and_detects_conflict() {
        let s = mem_store().await;
        assert_eq!(s.enqueue(&ev("1", "k", "{}")).await.unwrap(), Enqueued::New);
        assert_eq!(
            s.enqueue(&ev("2", "k", "{}")).await.unwrap(),
            Enqueued::Duplicate { id: "1".into() }
        );
        assert_eq!(
            s.enqueue(&ev("3", "k", "{\"a\":1}")).await.unwrap(),
            Enqueued::Conflict
        );
    }

    #[tokio::test]
    async fn claim_is_exclusive_and_respects_schedule() {
        let s = mem_store().await;
        s.enqueue(&ev("1", "k", "{}")).await.unwrap();
        let now = now_ms();
        let c = s.claim(now, 10_000).await.unwrap().unwrap();
        assert_eq!((c.id.as_str(), c.attempts), ("1", 1));
        assert!(s.claim(now, 10_000).await.unwrap().is_none());
        // lease expires: another worker may take it, attempts count up
        let c2 = s.claim(now + 10_001, 10_000).await.unwrap().unwrap();
        assert_eq!(c2.attempts, 2);
        s.schedule_retry("1", now + 60_000, "boom").await.unwrap();
        assert!(s.claim(now + 1000, 10_000).await.unwrap().is_none());
        assert!(s.claim(now + 60_001, 10_000).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn dead_letter_and_replay() {
        let s = mem_store().await;
        s.enqueue(&ev("1", "k", "{}")).await.unwrap();
        s.claim(now_ms(), 1000).await.unwrap();
        s.dead_letter("1", "gave up").await.unwrap();
        assert_eq!(s.list_dead_letters(10).await.unwrap().len(), 1);
        assert!(s.replay("1").await.unwrap());
        assert!(!s.replay("1").await.unwrap());
        let e = s.get_event("1").await.unwrap().unwrap();
        assert_eq!(
            (e.summary.status.as_str(), e.summary.attempts),
            ("pending", 0)
        );
        assert!(
            s.list_dead_letters(10).await.unwrap()[0]
                .replayed_at
                .is_some()
        );
    }

    #[tokio::test]
    async fn release_does_not_count_attempt() {
        let s = mem_store().await;
        s.enqueue(&ev("1", "k", "{}")).await.unwrap();
        s.claim(now_ms(), 1000).await.unwrap();
        s.release("1", now_ms()).await.unwrap();
        assert_eq!(s.get_event("1").await.unwrap().unwrap().summary.attempts, 0);
    }

    #[tokio::test]
    async fn rotation_keeps_old_secret_for_grace() {
        let s = mem_store().await;
        s.create_tenant_if_absent("t", "h", "old").await.unwrap();
        assert!(s.rotate_secret("t", "new", now_ms() + 1000).await.unwrap());
        let t = s.tenant_by_id("t").await.unwrap().unwrap();
        assert_eq!(t.valid_secrets(now_ms()), vec!["new", "old"]);
        assert_eq!(t.valid_secrets(now_ms() + 2000), vec!["new"]);
    }
}
