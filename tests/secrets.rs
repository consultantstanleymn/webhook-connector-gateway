//! Signing secrets at rest. Runs on the backend that GATEWAY_TEST_BACKEND picks.

mod common;
use common::*;
use webhook_connector_gateway::{app::Runtime, secrets::Keyring};

#[tokio::test]
async fn db_rows_hold_no_plain_secret() {
    let h = Harness::start().await;
    let row =
        h.rt.state
            .store
            .tenant_by_id(TENANT)
            .await
            .unwrap()
            .unwrap();
    assert!(
        row.secret.starts_with("v1:primary:"),
        "stored value was {}",
        row.secret
    );
    assert!(!row.secret.contains(SECRET));
    // The key opens it to the configured secret.
    assert_eq!(row.current_secret(&h.rt.state.keyring).unwrap(), SECRET);
}

#[tokio::test]
async fn signature_still_verifies_with_sealed_secret() {
    let h = Harness::start().await;
    let r = h.post(ORDER, None).await;
    assert_eq!(r.status().as_u16(), 202);
    let id = r.json::<serde_json::Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    h.wait_status(&id, "delivered").await;
    // The mock checks nothing about the secret here, but the gateway signed with the opened one.
    assert_eq!(
        h.mock.call_count.load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

#[tokio::test]
async fn startup_fails_without_active_key() {
    let (url, _mock) = start_mock().await;
    let mut cfg = test_config(&url);
    cfg.secrets.active_kid = "no-such-key-in-the-directory".into();
    let err = Runtime::build(cfg).await.err().expect("startup must fail");
    assert!(
        format!("{err:#}").contains("signing-secret key"),
        "error was: {err:#}"
    );
}

#[test]
fn keyring_loads_from_the_test_directory() {
    let cfg = webhook_connector_gateway::secrets::SecretsCfg {
        provider: "file".into(),
        dir: Some(test_key_dir()),
        ..Default::default()
    };
    assert_eq!(Keyring::load(&cfg).unwrap().active_kid(), "primary");
}
