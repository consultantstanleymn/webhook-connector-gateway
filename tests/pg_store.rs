//! Postgres adapter tests. They need `GATEWAY_TEST_PG_URL`. Without it, each test prints a note and returns.

use sqlx::postgres::PgPoolOptions;
use webhook_connector_gateway::store::{Enqueued, NewEvent, Store};

/// A store on a fresh schema, so tests do not see each other's rows.
async fn fresh_store() -> Option<(Store, String)> {
    let Ok(base) = std::env::var("GATEWAY_TEST_PG_URL") else {
        eprintln!("GATEWAY_TEST_PG_URL is not set: skipping a Postgres test");
        return None;
    };
    let schema = format!("gw_{}", uuid::Uuid::new_v4().simple());
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .unwrap();
    // The schema name is generated here from a UUID, never from input.
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let sep = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{sep}options=-c%20search_path%3D{schema}");
    Some((Store::connect(&url).await.unwrap(), url))
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

fn now() -> i64 {
    webhook_connector_gateway::now_ms()
}

#[tokio::test]
async fn pg_enqueue_and_claim() {
    let Some((s, _)) = fresh_store().await else {
        return;
    };
    assert_eq!(s.enqueue(&ev("1", "k", "{}")).await.unwrap(), Enqueued::New);
    let c = s.claim(now(), 10_000).await.unwrap().expect("a due event");
    assert_eq!((c.id.as_str(), c.attempts), ("1", 1));
    assert!(s.claim(now(), 10_000).await.unwrap().is_none());
}

#[tokio::test]
async fn pg_claim_skips_locked_rows() {
    let Some((s, url)) = fresh_store().await else {
        return;
    };
    s.enqueue(&ev("a", "ka", "{}")).await.unwrap();
    s.enqueue(&ev("b", "kb", "{}")).await.unwrap();
    // Another session holds a row lock on the first row. The claim must skip it.
    let other = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let mut tx = other.begin().await.unwrap();
    sqlx::query("SELECT id FROM events WHERE id = 'a' FOR UPDATE")
        .execute(&mut *tx)
        .await
        .unwrap();
    let c = s
        .claim(now(), 10_000)
        .await
        .unwrap()
        .expect("the unlocked row");
    assert_eq!(c.id, "b");
    tx.rollback().await.unwrap();
    // Two concurrent claims on two free rows get two different rows.
    let (x, y) = tokio::join!(s.claim(now(), 10_000), s.claim(now(), 10_000));
    let ids: Vec<String> = [x.unwrap(), y.unwrap()]
        .into_iter()
        .flatten()
        .map(|c| c.id)
        .collect();
    assert_eq!(ids, vec!["a".to_string()]);
}

#[tokio::test]
async fn pg_lease_expiry_reclaims() {
    let Some((s, _)) = fresh_store().await else {
        return;
    };
    s.enqueue(&ev("1", "k", "{}")).await.unwrap();
    let t = now();
    s.claim(t, 10_000).await.unwrap().unwrap();
    assert!(s.claim(t, 10_000).await.unwrap().is_none());
    let again = s.claim(t + 10_001, 10_000).await.unwrap().unwrap();
    assert_eq!(again.attempts, 2);
}

#[tokio::test]
async fn pg_dead_letter_and_replay() {
    let Some((s, _)) = fresh_store().await else {
        return;
    };
    s.enqueue(&ev("1", "k", "{}")).await.unwrap();
    s.claim(now(), 1000).await.unwrap();
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
async fn pg_idempotency_conflict_detected() {
    let Some((s, _)) = fresh_store().await else {
        return;
    };
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
