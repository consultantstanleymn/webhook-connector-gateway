//! Signing secrets at rest. Runs on the backend that GATEWAY_TEST_BACKEND picks.

mod common;
use common::*;
use webhook_connector_gateway::{
    admin::reseal_secrets,
    app::Runtime,
    secrets::{Keyring, SecretsCfg},
};

/// A keyring that seals with `second` and can still open `primary`.
fn ring_second_active() -> Keyring {
    Keyring::load(&SecretsCfg {
        provider: "file".into(),
        dir: Some(test_key_dir()),
        active_kid: "second".into(),
        previous_kids: vec!["primary".into()],
    })
    .unwrap()
}

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

#[tokio::test]
async fn reseal_moves_all_rows_to_new_kid() {
    let h = Harness::start().await;
    let ring = ring_second_active();
    let before =
        h.rt.state
            .store
            .tenant_by_id(TENANT)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(Keyring::kid_of(&before.secret), Some("primary"));
    let r = reseal_secrets(&h.rt.state.store, &ring, "second")
        .await
        .unwrap();
    assert_eq!(r.resealed, 1);
    let after =
        h.rt.state
            .store
            .tenant_by_id(TENANT)
            .await
            .unwrap()
            .unwrap();
    assert_eq!(Keyring::kid_of(&after.secret), Some("second"));
    assert_eq!(after.current_secret(&ring).unwrap(), SECRET);
}

#[tokio::test]
async fn old_kid_still_opens_during_overlap() {
    let h = Harness::start().await;
    let row =
        h.rt.state
            .store
            .tenant_by_id(TENANT)
            .await
            .unwrap()
            .unwrap();
    // The row is still sealed with `primary`. The new keyring opens it as a previous key.
    assert_eq!(Keyring::kid_of(&row.secret), Some("primary"));
    assert_eq!(row.current_secret(&ring_second_active()).unwrap(), SECRET);
}

#[tokio::test]
async fn reseal_twice_is_noop() {
    let h = Harness::start().await;
    let ring = ring_second_active();
    let first = reseal_secrets(&h.rt.state.store, &ring, "second")
        .await
        .unwrap();
    let sealed =
        h.rt.state
            .store
            .tenant_by_id(TENANT)
            .await
            .unwrap()
            .unwrap()
            .secret;
    let again = reseal_secrets(&h.rt.state.store, &ring, "second")
        .await
        .unwrap();
    assert_eq!((first.resealed, again.resealed, again.unchanged), (1, 0, 1));
    let same =
        h.rt.state
            .store
            .tenant_by_id(TENANT)
            .await
            .unwrap()
            .unwrap()
            .secret;
    assert_eq!(
        sealed, same,
        "a second run must not change the stored value"
    );
}
