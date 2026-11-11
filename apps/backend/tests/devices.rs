mod common;
use common::*;
use remotevnc_backend::{db::Database, devices::DeviceService};

#[tokio::test]
async fn google_subject_not_shared_email_owns_devices_and_revocation_is_immediate() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let db = Database::open(dir.path().join("identity.db")).unwrap();
    let auth = issuer.auth(db.clone());
    let service = DeviceService::new(config(), db);
    let mut tokens = Vec::new();
    let mut ids = Vec::new();
    for (index, sub) in ["subject-a", "subject-b", "subject-a"].iter().enumerate() {
        let key = device_key(index as u8 + 1);
        let e = begin(&service, &key);
        confirm(&issuer, &auth, &e, sub).await;
        service.poll_enrollment(&e.id, poll(&e, &key)).unwrap();
        tokens.push(access(&service, e.challenge.device_id, &key));
        ids.push(e.challenge.device_id);
    }
    assert_eq!(service.list(&tokens[0].token).unwrap().len(), 2);
    assert_eq!(service.list(&tokens[1].token).unwrap().len(), 1);
    assert!(service.revoke(&tokens[1].token, ids[0]).await.is_err());
    service.revoke(&tokens[0].token, ids[2]).await.unwrap();
    assert!(service.list(&tokens[2].token).is_err());
    assert!(service.challenge(ids[2]).is_err());
    service.revoke(&tokens[0].token, ids[0]).await.unwrap();
    assert!(service.list(&tokens[0].token).is_err());
}

#[tokio::test]
async fn enrolled_public_key_cannot_be_assigned_to_another_subject() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let db = Database::open(dir.path().join("identity.db")).unwrap();
    let auth = issuer.auth(db.clone());
    let service = DeviceService::new(config(), db);
    let key = device_key(9);
    let a = begin(&service, &key);
    let b = begin(&service, &key);
    confirm(&issuer, &auth, &a, "owner-a").await;
    service.poll_enrollment(&a.id, poll(&a, &key)).unwrap();
    let (code, state) = issuer.issue(&auth.start_login(&b.id).unwrap(), "owner-b", Fault::None);
    let login = auth.finish_login(&code, &state).await.unwrap();
    assert!(auth.confirm(&login.confirmation, &b.display_code).is_err());
    assert!(
        service
            .begin_enrollment(binachicken_protocol::EnrollmentRequest {
                name: "duplicate".into(),
                public_key: {
                    use base64::Engine;
                    base64::engine::general_purpose::URL_SAFE_NO_PAD
                        .encode(key.verifying_key().to_bytes())
                }
            })
            .is_err()
    );
}

#[tokio::test]
async fn http_sdk_enrolls_confirms_authenticates_lists_and_revokes() {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use binachicken_device::{DeviceApi, DeviceIdentity};
    use binachicken_protocol::EnrollmentStatus;
    use remotevnc_backend::{BackendState, router};
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let db = Database::open(dir.path().join("identity.db")).unwrap();
    let auth = issuer.auth(db.clone());
    let state = BackendState {
        config: config(),
        auth,
        sessions: remotevnc_backend::sessions::SessionService::new(
            db.clone(),
            remotevnc_backend::turn::TurnClient::unavailable(),
        ),
        devices: DeviceService::new(config(), db),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
    let key = device_key(7);
    let api = DeviceApi::new(&base, key.clone()).unwrap();
    let public = URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes());
    let enrollment = api
        .begin_enrollment("Office <agent>", &public)
        .await
        .unwrap();
    assert!(matches!(
        api.poll_enrollment(&enrollment).await.unwrap(),
        EnrollmentStatus::Pending
    ));
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let start = http
        .get(format!("{base}/auth/google/start"))
        .query(&[("enrollment", &enrollment.id)])
        .send()
        .await
        .unwrap();
    assert_eq!(start.status(), 303);
    let location = start.headers()["location"].to_str().unwrap();
    let (code, state) = issuer.issue(location, "http-owner", Fault::None);
    let callback = http
        .get(format!("{base}/login/callback"))
        .query(&[("code", code), ("state", state)])
        .send()
        .await
        .unwrap();
    assert_eq!(callback.status(), 200);
    assert_eq!(callback.headers()["referrer-policy"], "no-referrer");
    assert_eq!(callback.headers()["cache-control"], "no-store");
    let html = callback.text().await.unwrap();
    assert!(html.contains("Google sign-in complete"));
    assert!(html.contains("registered"));
    assert!(!html.contains("http-owner"));
    assert!(!html.contains(&enrollment.display_code));
    assert!(!html.contains(&enrollment.polling_verifier));
    let EnrollmentStatus::Enrolled { device_id } = api.poll_enrollment(&enrollment).await.unwrap()
    else {
        panic!("not enrolled")
    };
    let token = api
        .authenticate(&DeviceIdentity {
            device_id,
            signing_key: key,
        })
        .await
        .unwrap();
    assert_eq!(api.list_devices(&token).await.unwrap().len(), 1);
    assert_eq!(
        http.get(format!("{base}/v1/devices"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    api.revoke_device(&token, device_id).await.unwrap();
    assert!(api.list_devices(&token).await.is_err());
    let too_large = http
        .post(format!("{base}/v1/enrollments"))
        .header("content-type", "application/json")
        .body("x".repeat(20_000))
        .send()
        .await
        .unwrap();
    assert_eq!(too_large.status(), 413);
    server.abort();
}

#[tokio::test]
async fn request_errors_never_echo_submitted_values() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let db = Database::open(dir.path().join("identity.db")).unwrap();
    let state = remotevnc_backend::BackendState {
        config: config(),
        auth: issuer.auth(db.clone()),
        sessions: remotevnc_backend::sessions::SessionService::new(
            db.clone(),
            remotevnc_backend::turn::TurnClient::unavailable(),
        ),
        devices: DeviceService::new(config(), db),
    };
    let response = remotevnc_backend::router(state)
        .oneshot(
            Request::post("/v1/devices/authenticate")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"challenge_id":"x","signature":["never-echo-secret"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&body).contains("never-echo-secret"));
}

#[tokio::test]
async fn active_device_quota_bounds_listing_and_revocation_frees_a_slot() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let path = dir.path().join("identity.db");
    let db = Database::open(&path).unwrap();
    let service = DeviceService::new(config(), db.clone());
    let auth = issuer.auth(db);
    let key = device_key(5);
    let enrolled = begin(&service, &key);
    confirm(&issuer, &auth, &enrolled, "quota-owner").await;
    service
        .poll_enrollment(&enrolled.id, poll(&enrolled, &key))
        .unwrap();
    let token = access(&service, enrolled.challenge.device_id, &key);
    let mut sql = rusqlite::Connection::open(&path).unwrap();
    let tx = sql.transaction().unwrap();
    let removable = uuid::Uuid::new_v4();
    for i in 0..999 {
        let id = if i == 0 {
            removable
        } else {
            uuid::Uuid::new_v4()
        };
        tx.execute("INSERT INTO devices(id,owner,name,public_key) VALUES(?,'quota-owner','quota fixture',?)",rusqlite::params![id.to_string(),format!("fixture-key-{i}")]).unwrap();
    }
    tx.commit().unwrap();
    let next_key = device_key(6);
    let next = begin(&service, &next_key);
    confirm(&issuer, &auth, &next, "quota-owner").await;
    assert!(
        service
            .poll_enrollment(&next.id, poll(&next, &next_key))
            .is_err()
    );
    assert_eq!(service.list(&token.token).unwrap().len(), 1000);
    service
        .revoke(&token.token, binachicken_protocol::DeviceId(removable))
        .await
        .unwrap();
    assert!(
        service
            .poll_enrollment(&next.id, poll(&next, &next_key))
            .is_ok()
    );
    assert_eq!(service.list(&token.token).unwrap().len(), 1000);
}
