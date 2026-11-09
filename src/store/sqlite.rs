//! SQLite adapter of the store port.

use super::{
    Claimed, DeadLetter, Enqueued, EventDetail, EventSummary, NewEvent, TenantRow, hash_hex,
};
use crate::now_ms;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::str::FromStr;
use std::time::Duration;

/// Versioned migrations. The files live in `migrations/sqlite`.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/sqlite");

/// A database file made by the code before migrations has the tables but no
/// `_sqlx_migrations` table. Record the baseline migration as applied, so the
/// DDL does not run again on top of existing data (DECISIONS entry 16).
async fn adopt_legacy_database(pool: &SqlitePool) -> anyhow::Result<()> {
    let has = |name: &'static str| {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(name)
    };
    let legacy = has("events").fetch_one(pool).await? > 0;
    let versioned = has("_sqlx_migrations").fetch_one(pool).await? > 0;
    if !legacy || versioned {
        return Ok(());
    }
    let baseline = MIGRATOR
        .iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no baseline migration"))?;
    sqlx::query(
        "CREATE TABLE _sqlx_migrations (
            version BIGINT PRIMARY KEY,
            description TEXT NOT NULL,
            installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            success BOOLEAN NOT NULL,
            checksum BLOB NOT NULL,
            execution_time BIGINT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
         VALUES (?, ?, TRUE, ?, -1)",
    )
    .bind(baseline.version)
    .bind(baseline.description.to_string())
    .bind(baseline.checksum.to_vec())
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
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
        adopt_legacy_database(&self.pool).await?;
        MIGRATOR.run(&self.pool).await?;
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

    /// Take the next due event. If none is due, take an event whose lease
    /// ran out, which happens when a worker crashed. Two small queries, each
    /// served by a partial index in order, so the cost does not grow with
    /// the backlog.
    pub async fn claim(&self, now: i64, lease_ms: i64) -> Result<Option<Claimed>, sqlx::Error> {
        let due: Option<Claimed> = sqlx::query_as(
            "UPDATE events SET status = 'processing', attempts = attempts + 1,
                    locked_until = ?, updated_at = ?
             WHERE id = (SELECT id FROM events
                         WHERE status = 'pending' AND next_attempt_at <= ?
                         ORDER BY next_attempt_at LIMIT 1)
             RETURNING id, tenant_id, payload, trace_id, attempts, created_at",
        )
        .bind(now + lease_ms)
        .bind(now)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        if due.is_some() {
            return Ok(due);
        }
        sqlx::query_as(
            "UPDATE events SET attempts = attempts + 1, locked_until = ?, updated_at = ?
             WHERE id = (SELECT id FROM events
                         WHERE status = 'processing' AND locked_until <= ?
                         ORDER BY locked_until LIMIT 1)
             RETURNING id, tenant_id, payload, trace_id, attempts, created_at",
        )
        .bind(now + lease_ms)
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

    async fn mem_store() -> SqliteStore {
        let path = std::env::temp_dir().join(format!("wcg-{}.db", uuid::Uuid::new_v4()));
        SqliteStore::connect(&format!("sqlite://{}", path.display()))
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
        // The values here are legacy plain text, which the keyring passes through.
        let ring = crate::secrets::Keyring::new(("k", &[7u8; 32]), &[]).unwrap();
        assert_eq!(t.valid_secrets(now_ms(), &ring), vec!["new", "old"]);
        assert_eq!(t.valid_secrets(now_ms() + 2000, &ring), vec!["new"]);
    }
}
