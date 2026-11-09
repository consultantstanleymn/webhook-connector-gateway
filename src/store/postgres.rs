//! Postgres adapter of the store port. Same behavior as the SQLite adapter.
//! `claim` uses `FOR UPDATE SKIP LOCKED`, so two workers never take the same row.

use super::{
    Claimed, DeadLetter, Enqueued, EventDetail, EventSummary, NewEvent, TenantRow, hash_hex,
};
use crate::now_ms;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

/// Versioned migrations. The files live in `migrations/postgres`.
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations/postgres");

#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new().max_connections(8).connect(url).await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self { pool })
    }

    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
    }

    pub async fn create_tenant_if_absent(
        &self,
        id: &str,
        api_key_hash: &str,
        secret: &str,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "INSERT INTO tenants (id, api_key_hash, secret, created_at) VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
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
            "SELECT id, secret, prev_secret, prev_expires_at FROM tenants WHERE api_key_hash = $1",
        )
        .bind(hash)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn tenant_by_id(&self, id: &str) -> Result<Option<TenantRow>, sqlx::Error> {
        sqlx::query_as("SELECT id, secret, prev_secret, prev_expires_at FROM tenants WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
    }

    pub async fn rotate_secret(
        &self,
        id: &str,
        new_secret: &str,
        prev_expires_at: i64,
    ) -> Result<bool, sqlx::Error> {
        let r = sqlx::query(
            "UPDATE tenants SET prev_secret = secret, prev_expires_at = $1, secret = $2 WHERE id = $3",
        )
        .bind(prev_expires_at)
        .bind(new_secret)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(r.rows_affected() == 1)
    }

    pub async fn enqueue(&self, e: &NewEvent<'_>) -> Result<Enqueued, sqlx::Error> {
        let now = now_ms();
        let hash = hash_hex(e.payload.as_bytes());
        let r = sqlx::query(
            "INSERT INTO events (id, tenant_id, idempotency_key, payload_hash, payload, trace_id,
                                 status, attempts, next_attempt_at, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, 'pending', 0, $7, $7, $7)
             ON CONFLICT (tenant_id, idempotency_key) DO NOTHING",
        )
        .bind(e.id)
        .bind(e.tenant_id)
        .bind(e.idempotency_key)
        .bind(&hash)
        .bind(e.payload)
        .bind(e.trace_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        if r.rows_affected() == 1 {
            return Ok(Enqueued::New);
        }
        let (id, old_hash): (String, String) = sqlx::query_as(
            "SELECT id, payload_hash FROM events WHERE tenant_id = $1 AND idempotency_key = $2",
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

    /// Take the next due event, or an event whose lease ran out. Each query takes
    /// one row with `FOR UPDATE SKIP LOCKED`, so concurrent workers get different rows.
    pub async fn claim(&self, now: i64, lease_ms: i64) -> Result<Option<Claimed>, sqlx::Error> {
        let due: Option<Claimed> = sqlx::query_as(
            "UPDATE events SET status = 'processing', attempts = attempts + 1,
                    locked_until = $1, updated_at = $2
             WHERE id = (SELECT id FROM events
                         WHERE status = 'pending' AND next_attempt_at <= $2
                         ORDER BY next_attempt_at LIMIT 1 FOR UPDATE SKIP LOCKED)
             RETURNING id, tenant_id, payload, trace_id, attempts, created_at",
        )
        .bind(now + lease_ms)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        if due.is_some() {
            return Ok(due);
        }
        sqlx::query_as(
            "UPDATE events SET attempts = attempts + 1, locked_until = $1, updated_at = $2
             WHERE id = (SELECT id FROM events
                         WHERE status = 'processing' AND locked_until <= $2
                         ORDER BY locked_until LIMIT 1 FOR UPDATE SKIP LOCKED)
             RETURNING id, tenant_id, payload, trace_id, attempts, created_at",
        )
        .bind(now + lease_ms)
        .bind(now)
        .fetch_optional(&self.pool)
        .await
    }

    async fn finish(&self, id: &str, status: &str, err: Option<&str>) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE events SET status = $1, last_error = $2, updated_at = $3 WHERE id = $4")
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
            "UPDATE events SET status = 'pending', next_attempt_at = $1, last_error = $2, updated_at = $3
             WHERE id = $4",
        )
        .bind(next_at)
        .bind(err)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    pub async fn release(&self, id: &str, next_at: i64) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE events SET status = 'pending', attempts = GREATEST(attempts - 1, 0),
                    next_attempt_at = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(next_at)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    pub async fn dead_letter(&self, id: &str, err: &str) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let now = now_ms();
        sqlx::query(
            "UPDATE events SET status = 'dead', last_error = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(err)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO dead_letters (event_id, tenant_id, payload, attempts, last_error, died_at)
             SELECT id, tenant_id, payload, attempts, $1, $2 FROM events WHERE id = $3
             ON CONFLICT (event_id) DO UPDATE SET attempts = excluded.attempts,
                 last_error = excluded.last_error, died_at = excluded.died_at, replayed_at = NULL",
        )
        .bind(err)
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }

    pub async fn replay(&self, id: &str) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let now = now_ms();
        let r = sqlx::query(
            "UPDATE events SET status = 'pending', attempts = 0, next_attempt_at = $1, updated_at = $1
             WHERE id = $2 AND status = 'dead'",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() == 1 {
            sqlx::query("UPDATE dead_letters SET replayed_at = $1 WHERE event_id = $2")
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
             WHERE ($1::text IS NULL OR status = $1) AND ($2::text IS NULL OR tenant_id = $2)
             ORDER BY created_at DESC, id LIMIT $3"
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
            ", idempotency_key, payload FROM events WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
    }

    pub async fn list_dead_letters(&self, limit: i64) -> Result<Vec<DeadLetter>, sqlx::Error> {
        sqlx::query_as(
            "SELECT event_id, tenant_id, payload, attempts, last_error, died_at, replayed_at
             FROM dead_letters ORDER BY died_at DESC LIMIT $1",
        )
        .bind(limit.clamp(1, 1000))
        .fetch_all(&self.pool)
        .await
    }

    pub async fn depth(&self) -> Result<Vec<(String, i64)>, sqlx::Error> {
        sqlx::query_as("SELECT status, COUNT(*)::bigint FROM events GROUP BY status")
            .fetch_all(&self.pool)
            .await
    }
}
