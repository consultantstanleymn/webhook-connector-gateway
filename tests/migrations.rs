//! Versioned migrations: fresh database, old database file, repeated start.

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use webhook_connector_gateway::store::{Enqueued, NewEvent, Store};

/// The schema text from before migrations, copied here on purpose.
const LEGACY_SCHEMA: &str = include_str!("fixtures/legacy_schema.sql");

fn temp_url() -> (String, String) {
    let path = std::env::temp_dir().join(format!("wcg-mig-{}.db", uuid::Uuid::new_v4()));
    (
        path.display().to_string(),
        format!("sqlite://{}", path.display()),
    )
}

async fn raw_pool(path: &str) -> sqlx::SqlitePool {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))
        .unwrap()
        .create_if_missing(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap()
}

#[tokio::test]
async fn fresh_db_gets_migration_table() {
    let (path, url) = temp_url();
    Store::connect(&url).await.unwrap();
    let pool = raw_pool(&path).await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations WHERE success")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(n >= 1);
}

#[tokio::test]
async fn legacy_db_is_adopted_without_data_loss() {
    let (path, url) = temp_url();
    {
        let pool = raw_pool(&path).await;
        sqlx::raw_sql(LEGACY_SCHEMA).execute(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO events (id, tenant_id, idempotency_key, payload_hash, payload, trace_id,
                                 status, attempts, next_attempt_at, created_at, updated_at)
             VALUES ('old-1', 't', 'k', 'h', '{}', 'tr', 'pending', 0, 0, 0, 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;
    }
    let store = Store::connect(&url).await.unwrap();
    let e = store
        .get_event("old-1")
        .await
        .unwrap()
        .expect("old event survives");
    assert_eq!(e.summary.status, "pending");
    // The adopted database accepts new work.
    let ev = NewEvent {
        id: "new-1",
        tenant_id: "t",
        idempotency_key: "k2",
        payload: "{}",
        trace_id: "tr",
    };
    assert_eq!(store.enqueue(&ev).await.unwrap(), Enqueued::New);
}

#[tokio::test]
async fn migrate_twice_is_noop() {
    let (path, url) = temp_url();
    Store::connect(&url).await.unwrap();
    Store::connect(&url).await.unwrap();
    let pool = raw_pool(&path).await;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
}
