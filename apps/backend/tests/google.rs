use remotevnc_backend::config::Config;

#[test]
fn callback_configuration_is_same_origin_https_and_supports_registered_path() {
    let config = Config::new(
        "https://amio.vnoffice.io.vn",
        Some("https://amio.vnoffice.io.vn/login/callback"),
        "client".into(),
        "secret".into(),
    )
    .unwrap();
    assert_eq!(config.redirect_uri.path(), "/login/callback");
    let default = Config::new(
        "https://example.test",
        None,
        "client".into(),
        "secret".into(),
    )
    .unwrap();
    assert_eq!(default.redirect_uri.path(), "/auth/google/callback");
    for (origin, redirect) in [
        ("http://example.test", None),
        ("https://example.test/path", None),
        ("https://user:pass@example.test", None),
        (
            "https://example.test",
            Some("https://evil.test/login/callback"),
        ),
        (
            "https://example.test",
            Some("https://example.test/login/callback?secret=x"),
        ),
        (
            "https://example.test",
            Some("https://example.test/v1/devices"),
        ),
    ] {
        assert!(Config::new(origin, redirect, "client".into(), "secret".into()).is_err());
    }
    assert!(Config::new("https://example.test", None, "".into(), "".into()).is_err());
}

mod common;
use common::*;
use remotevnc_backend::{db::Database, devices::DeviceService};

#[tokio::test]
async fn real_oidc_exchange_rejects_invalid_tokens_and_consumes_state() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let db = Database::open(dir.path().join("identity.db")).unwrap();
    let auth = issuer.auth(db.clone());
    let devices = DeviceService::new(config(), db);
    for fault in [
        Fault::None,
        Fault::Signature,
        Fault::Audience,
        Fault::Issuer,
        Fault::Nonce,
        Fault::Expired,
    ] {
        let e = begin(&devices, &device_key(7));
        let (code, state) =
            issuer.issue(&auth.start_login(&e.id).unwrap(), "google-subject", fault);
        assert!(auth.finish_login(&code, "unissued-state").await.is_err());
        let result = auth.finish_login(&code, &state).await;
        assert_eq!(result.is_ok(), matches!(fault, Fault::None));
        assert!(auth.finish_login(&code, &state).await.is_err());
    }
}

#[tokio::test]
async fn confirmation_requires_matching_code_and_cannot_replay_or_swap_enrollments() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let db = Database::open(dir.path().join("identity.db")).unwrap();
    let auth = issuer.auth(db.clone());
    let devices = DeviceService::new(config(), db);
    let a = begin(&devices, &device_key(1));
    let b = begin(&devices, &device_key(2));
    let (code, state) = issuer.issue(&auth.start_login(&a.id).unwrap(), "sub-a", Fault::None);
    let login = auth.finish_login(&code, &state).await.unwrap();
    assert!(auth.confirm(&login.confirmation, &b.display_code).is_err());
    assert!(matches!(
        devices
            .poll_enrollment(&a.id, poll(&a, &device_key(1)))
            .unwrap(),
        binachicken_protocol::EnrollmentStatus::Pending
    ));
    // A failed code attempt burns the confirmation capability.
    assert!(auth.confirm(&login.confirmation, &a.display_code).is_err());
    let (code, state) = issuer.issue(&auth.start_login(&a.id).unwrap(), "sub-a", Fault::None);
    let login = auth.finish_login(&code, &state).await.unwrap();
    auth.confirm(&login.confirmation, &a.display_code).unwrap();
    assert!(auth.confirm(&login.confirmation, &a.display_code).is_err());
    assert!(matches!(
        devices
            .poll_enrollment(&b.id, poll(&b, &device_key(2)))
            .unwrap(),
        binachicken_protocol::EnrollmentStatus::Pending
    ));
}

#[tokio::test]
async fn ongoing_service_accepts_rotated_jwks_but_not_wrong_signature() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let db = Database::open(dir.path().join("identity.db")).unwrap();
    let auth = issuer.auth(db.clone());
    let devices = DeviceService::new(config(), db);
    issuer.rotate_keys();
    for fault in [Fault::None, Fault::Signature] {
        let e = begin(&devices, &device_key(8));
        let (code, state) =
            issuer.issue(&auth.start_login(&e.id).unwrap(), "rotated-subject", fault);
        assert_eq!(
            auth.finish_login(&code, &state).await.is_ok(),
            matches!(fault, Fault::None)
        );
    }
}

#[tokio::test]
async fn concurrent_callbacks_consume_oauth_state_exactly_once() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let path = dir.path().join("identity.db");
    let db = Database::open(&path).unwrap();
    let a = issuer.auth(db.clone());
    let b = issuer.auth(Database::open(&path).unwrap());
    let e = begin(&DeviceService::new(config(), db), &device_key(3));
    let (code, state) = issuer.issue(&a.start_login(&e.id).unwrap(), "subject", Fault::None);
    let (left, right) = tokio::join!(a.finish_login(&code, &state), b.finish_login(&code, &state));
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
}
