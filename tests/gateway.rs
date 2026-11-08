mod common;

use common::*;
use serde_json::{Value, json};
use std::{sync::atomic::Ordering, time::Duration};
use webhook_connector_gateway::{now_ms, signature, trace};

async fn id_of(res: reqwest::Response) -> String {
    let v: Value = res.json().await.unwrap();
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn happy_path_transforms_signs_and_propagates_trace() {
    let h = Harness::start().await;
    let tid = "4bf92f3577b34da6a3ce929d0e0e4736";
    let res = h
        .client
        .post(format!("{}/v1/webhooks", h.base))
        .header("x-api-key", API_KEY)
        .header("x-signature", h.sign(ORDER))
        .header("traceparent", format!("00-{tid}-00f067aa0ba902b7-01"))
        .header("x-request-id", "req-123")
        .body(ORDER)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 202);
    assert_eq!(res.headers()["x-request-id"], "req-123");
    let id = id_of(res).await;
    h.wait_status(&id, "delivered").await;

    let calls = h.mock.recorded();
    assert_eq!(calls.len(), 1);
    let sent: Value = serde_json::from_slice(&calls[0].body).unwrap();
    assert_eq!(
        sent,
        json!({"type":"order.created","order_id":7,"customer":{"id":3},"email":"a@b.c","source":"gateway-test","tenant":"acme"})
    );
    // outbound signature verifies with the tenant secret
    let sig = calls[0].headers["x-gateway-signature"].to_str().unwrap();
    assert!(
        signature::verify(
            &[SECRET.as_bytes()],
            sig,
            &calls[0].body,
            now_ms() / 1000,
            300
        )
        .is_ok()
    );
    // the same trace id arrives downstream
    let tp = calls[0].headers["traceparent"].to_str().unwrap();
    assert_eq!(trace::parse_traceparent(tp).as_deref(), Some(tid));
    assert_eq!(calls[0].headers["idempotency-key"].to_str().unwrap(), id);
}

#[tokio::test]
async fn rejects_bad_auth_and_signatures() {
    let h = Harness::start().await;
    let url = format!("{}/v1/webhooks", h.base);

    let no_key = h.client.post(&url).body(ORDER).send().await.unwrap();
    assert_eq!(no_key.status(), 401);

    let bad_key = h
        .client
        .post(&url)
        .header("x-api-key", "nope")
        .header("x-signature", h.sign(ORDER))
        .body(ORDER)
        .send()
        .await
        .unwrap();
    assert_eq!(bad_key.status(), 401);

    let no_sig = h
        .client
        .post(&url)
        .header("x-api-key", API_KEY)
        .body(ORDER)
        .send()
        .await
        .unwrap();
    assert_eq!(no_sig.status(), 401);

    let wrong_secret = signature::header_value(b"other", now_ms() / 1000, ORDER.as_bytes());
    let r = h
        .client
        .post(&url)
        .header("x-api-key", API_KEY)
        .header("x-signature", wrong_secret)
        .body(ORDER)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "bad_signature");

    // replay protection: a correct signature from 10 minutes ago
    let old = signature::header_value(SECRET.as_bytes(), now_ms() / 1000 - 600, ORDER.as_bytes());
    let r = h
        .client
        .post(&url)
        .header("x-api-key", API_KEY)
        .header("x-signature", old)
        .body(ORDER)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "stale_timestamp");

    // body changed after signing
    let sig = h.sign(ORDER);
    let r = h
        .client
        .post(&url)
        .header("x-api-key", API_KEY)
        .header("x-signature", sig)
        .body(ORDER.replace("7", "8"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // nothing was queued
    assert!(
        h.rt.state
            .store
            .list_events(None, None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn validates_body_and_size() {
    let h = Harness::start().await;
    assert_eq!(h.post("not json", None).await.status(), 400);
    assert_eq!(h.post("[1,2]", None).await.status(), 400);
    let big = format!(r#"{{"type":"order.created","pad":"{}"}}"#, "x".repeat(5000));
    assert_eq!(h.post(&big, None).await.status(), 413);
    assert_eq!(
        h.post(ORDER, Some("bad key with spaces")).await.status(),
        400
    );
}

#[tokio::test]
async fn idempotency_key_dedupes_and_detects_conflict() {
    let h = Harness::start().await;
    let first = h.post(ORDER, Some("order-7")).await;
    assert_eq!(first.status(), 202);
    let id1 = id_of(first).await;
    let again = h.post(ORDER, Some("order-7")).await;
    assert_eq!(again.status(), 200);
    assert_eq!(id_of(again).await, id1);
    let other = h.post(&ORDER.replace("7", "9"), Some("order-7")).await;
    assert_eq!(other.status(), 422);
    h.wait_status(&id1, "delivered").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.mock.recorded().len(),
        1,
        "duplicate must not be delivered twice"
    );
}

#[tokio::test]
async fn rate_limit_returns_429_with_retry_after() {
    let h = Harness::start_with(|c| {
        c.tenants[0].rate_limit.capacity = 3.0;
        c.tenants[0].rate_limit.refill_per_sec = 0.5;
    })
    .await;
    let mut codes = vec![];
    let mut last = None;
    for _ in 0..5 {
        let r = h.post(ORDER, None).await;
        codes.push(r.status().as_u16());
        last = Some(r);
    }
    assert_eq!(codes, vec![202, 202, 202, 429, 429]);
    assert!(last.unwrap().headers().contains_key("retry-after"));
}

#[tokio::test]
async fn retries_with_backoff_then_delivers() {
    let h = Harness::start().await;
    h.mock.fail_first.store(2, Ordering::SeqCst);
    let id = id_of(h.post(ORDER, None).await).await;
    h.wait_status(&id, "delivered").await;
    assert_eq!(h.mock.call_count.load(Ordering::SeqCst), 3);
    assert_eq!(
        h.rt.state
            .store
            .get_event(&id)
            .await
            .unwrap()
            .unwrap()
            .summary
            .attempts,
        3
    );
}

#[tokio::test]
async fn dead_letters_after_max_attempts_then_replay_delivers() {
    let h = Harness::start().await;
    h.mock.force_status.store(503, Ordering::SeqCst);
    let id = id_of(h.post(ORDER, None).await).await;
    h.wait_status(&id, "dead").await;
    assert_eq!(h.mock.call_count.load(Ordering::SeqCst), 4); // max_attempts
    let dl = h.rt.state.store.list_dead_letters(10).await.unwrap();
    assert_eq!(dl.len(), 1);
    assert!(dl[0].last_error.contains("max attempts"));

    // fix the downstream, replay through the admin API
    h.mock.force_status.store(0, Ordering::SeqCst);
    let r = h
        .client
        .post(format!("{}/admin/events/{id}/replay", h.base))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    h.wait_status(&id, "delivered").await;
    assert_eq!(h.mock.recorded().len(), 1);
    // replaying a delivered event is refused
    let r = h
        .client
        .post(format!("{}/admin/events/{id}/replay", h.base))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn permanent_4xx_goes_straight_to_dead_letters() {
    let h = Harness::start().await;
    h.mock.force_status.store(400, Ordering::SeqCst);
    let id = id_of(h.post(ORDER, None).await).await;
    h.wait_status(&id, "dead").await;
    assert_eq!(h.mock.call_count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn filtered_events_are_not_delivered() {
    let h = Harness::start().await;
    let id = id_of(h.post(r#"{"type":"ping"}"#, None).await).await;
    h.wait_status(&id, "filtered").await;
    assert_eq!(h.mock.call_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn circuit_breaker_stops_hammering_a_dead_downstream() {
    let h = Harness::start_with(|c| {
        c.delivery.breaker_failures = 2;
        c.delivery.breaker_cooldown_ms = 600;
        c.queue.max_attempts = 50;
    })
    .await;
    h.mock.force_status.store(503, Ordering::SeqCst);
    for i in 0..5 {
        h.post(ORDER, Some(&format!("k{i}"))).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    let during = h.mock.call_count.load(Ordering::SeqCst);
    // 2 failures open the circuit; only a few calls get through the cooldown
    assert!((2..=4).contains(&during), "calls while open: {during}");
    // downstream recovers, circuit closes after cooldown, all 5 arrive
    h.mock.force_status.store(0, Ordering::SeqCst);
    for _ in 0..300 {
        if h.mock.recorded().len() == 5 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "only {} events arrived after recovery",
        h.mock.recorded().len()
    );
}

#[tokio::test]
async fn secret_rotation_has_a_grace_window() {
    let h = Harness::start().await;
    let r = h
        .client
        .post(format!(
            "{}/admin/tenants/acme/rotate-secret?grace_secs=60",
            h.base
        ))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let new_secret = v["secret"].as_str().unwrap().to_string();
    assert_ne!(new_secret, SECRET);

    // old secret still works inside the grace window
    assert_eq!(h.post(ORDER, None).await.status(), 202);
    // new secret works
    let sig = signature::header_value(new_secret.as_bytes(), now_ms() / 1000, ORDER.as_bytes());
    let r = h
        .client
        .post(format!("{}/v1/webhooks", h.base))
        .header("x-api-key", API_KEY)
        .header("x-signature", sig)
        .body(ORDER)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);

    // with zero grace the old secret stops working
    let r = h
        .client
        .post(format!(
            "{}/admin/tenants/acme/rotate-secret?grace_secs=0",
            h.base
        ))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    let newest = r.json::<Value>().await.unwrap()["secret"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(newest, new_secret);
    tokio::time::sleep(Duration::from_millis(5)).await;
    let sig = signature::header_value(new_secret.as_bytes(), now_ms() / 1000, ORDER.as_bytes());
    let r = h
        .client
        .post(format!("{}/v1/webhooks", h.base))
        .header("x-api-key", API_KEY)
        .header("x-signature", sig)
        .body(ORDER)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    let r = h
        .client
        .post(format!("{}/admin/tenants/ghost/rotate-secret", h.base))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

#[tokio::test]
async fn admin_api_needs_token_and_lists_things() {
    let h = Harness::start().await;
    let id = id_of(h.post(ORDER, None).await).await;
    h.wait_status(&id, "delivered").await;

    let r = h
        .client
        .get(format!("{}/admin/events", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = h
        .client
        .get(format!("{}/admin/events", h.base))
        .bearer_auth("wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    let v: Value = h
        .admin_get("/admin/events?status=delivered")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["events"].as_array().unwrap().len(), 1);
    let v: Value = h
        .admin_get("/admin/events?status=dead")
        .await
        .json()
        .await
        .unwrap();
    assert!(v["events"].as_array().unwrap().is_empty());
    let v: Value = h
        .admin_get(&format!("/admin/events/{id}"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(v["status"], "delivered");
    assert!(v["payload"].as_str().unwrap().contains("order.created"));
    assert_eq!(h.admin_get("/admin/events/nope").await.status(), 404);
    let v: Value = h
        .admin_get("/admin/dead-letters")
        .await
        .json()
        .await
        .unwrap();
    assert!(v["dead_letters"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn admin_api_is_off_without_a_token() {
    let h = Harness::start_with(|c| c.server.admin_token = String::new()).await;
    let r = h
        .client
        .get(format!("{}/admin/events", h.base))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503);
}

#[tokio::test]
async fn health_ready_and_metrics() {
    let h = Harness::start().await;
    assert_eq!(
        h.client
            .get(format!("{}/healthz", h.base))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        h.client
            .get(format!("{}/readyz", h.base))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let id = id_of(h.post(ORDER, None).await).await;
    h.wait_status(&id, "delivered").await;
    h.post("garbage", None).await;
    let text = h
        .client
        .get(format!("{}/metrics", h.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for needle in [
        "gateway_events_received_total{tenant=\"acme\"} 1",
        "gateway_deliveries_total{outcome=\"delivered\"} 1",
        "gateway_http_request_duration_seconds_bucket",
        "gateway_delivery_duration_seconds_count",
        "gateway_queue_depth{status=\"delivered\"} 1",
        "gateway_queue_depth{status=\"pending\"} 0",
    ] {
        assert!(text.contains(needle), "metrics missing {needle}\n{text}");
    }
}

#[tokio::test]
async fn shutdown_finishes_in_flight_work() {
    let h = Harness::start().await;
    h.mock.delay_ms.store(400, Ordering::SeqCst);
    let id = id_of(h.post(ORDER, None).await).await;
    // wait until the worker holds the event
    h.wait_status(&id, "processing").await;
    h.shutdown.send(true).unwrap();
    for w in h.workers {
        w.await.unwrap();
    }
    // workers are stopped, and the in-flight event was finished, not dropped
    let status =
        h.rt.state
            .store
            .get_event(&id)
            .await
            .unwrap()
            .unwrap()
            .summary
            .status;
    assert_eq!(status, "delivered");
}

#[tokio::test]
async fn events_survive_a_restart_before_delivery() {
    // Accept with no workers running, then start a second runtime on the same DB.
    let (url, mock) = start_mock().await;
    let cfg = test_config(&url);
    let rt1 = webhook_connector_gateway::app::Runtime::build(cfg.clone())
        .await
        .unwrap();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn({
        let app = webhook_connector_gateway::app::router(rt1.state.clone());
        async move { axum::serve(l, app).await.unwrap() }
    });
    let body = ORDER;
    let sig = signature::header_value(SECRET.as_bytes(), now_ms() / 1000, body.as_bytes());
    let r = reqwest::Client::new()
        .post(format!("{base}/v1/webhooks"))
        .header("x-api-key", API_KEY)
        .header("x-signature", sig)
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 202);
    assert_eq!(mock.call_count.load(Ordering::SeqCst), 0);
    drop(rt1);

    let rt2 = webhook_connector_gateway::app::Runtime::build(cfg)
        .await
        .unwrap();
    assert!(rt2.engine.process_one().await.unwrap());
    assert_eq!(mock.recorded().len(), 1);
}
