mod common;
use common::*;
use remotevnc_backend::{db::Database, devices::DeviceService};
use remotevnc_protocol::*;

#[tokio::test]
async fn enrollment_restart_requires_private_verifier_and_bound_key_proof() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let path = dir.path().join("identity.db");
    let key = device_key(1);
    let e;
    let oauth;
    {
        let db = Database::open(&path).unwrap();
        let devices = DeviceService::new(config(), db.clone());
        e = begin(&devices, &key);
        assert!(!e.browser_url.contains(&e.polling_verifier));
        let auth = issuer.auth(db);
        oauth = issuer.issue(&auth.start_login(&e.id).unwrap(), "subject", Fault::None);
    }
    let db = Database::open(&path).unwrap();
    let devices = DeviceService::new(config(), db.clone());
    let auth = issuer.auth(db);
    let login = auth.finish_login(&oauth.0, &oauth.1).await.unwrap();
    auth.confirm(&login.confirmation, &e.display_code).unwrap();
    let mut stolen = poll(&e, &key);
    stolen.polling_verifier = "stolen-id-only".into();
    assert!(devices.poll_enrollment(&e.id, stolen).is_err());
    assert!(
        devices
            .poll_enrollment(&e.id, poll(&e, &device_key(2)))
            .is_err()
    );
    assert!(matches!(
        devices.poll_enrollment(&e.id, poll(&e, &key)).unwrap(),
        EnrollmentStatus::Enrolled { .. }
    ));
    assert!(devices.poll_enrollment(&e.id, poll(&e, &key)).is_err());
    let restarted = issuer.auth(Database::open(&path).unwrap());
    assert!(restarted.finish_login(&oauth.0, &oauth.1).await.is_err());
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !bytes
            .windows(e.polling_verifier.len())
            .any(|w| w == e.polling_verifier.as_bytes())
    );
}

#[tokio::test]
async fn authentication_challenges_survive_restart_and_are_single_use() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let path = dir.path().join("identity.db");
    let db = Database::open(&path).unwrap();
    let service = DeviceService::new(config(), db.clone());
    let key = device_key(5);
    let e = begin(&service, &key);
    confirm(&issuer, &issuer.auth(db), &e, "sub").await;
    service.poll_enrollment(&e.id, poll(&e, &key)).unwrap();
    let challenge = service.challenge(e.challenge.device_id).unwrap();
    drop(service);
    let service = DeviceService::new(config(), Database::open(&path).unwrap());
    let proof = DeviceProof {
        challenge_id: challenge.id,
        signature: sign_challenge(&key, &challenge.challenge).unwrap(),
    };
    assert!(
        service
            .authenticate(DeviceProof {
                signature: sign_challenge(&device_key(4), &challenge.challenge).unwrap(),
                ..proof.clone()
            })
            .is_err()
    );
    let token = service.authenticate(proof.clone()).unwrap();
    assert_eq!(service.list(&token.token).unwrap().len(), 1);
    assert!(service.authenticate(proof).is_err());
}

#[tokio::test]
async fn independent_database_connections_consume_device_proofs_once() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let path = dir.path().join("identity.db");
    let db = Database::open(&path).unwrap();
    let service = DeviceService::new(config(), db.clone());
    let key = device_key(3);
    let e = begin(&service, &key);
    confirm(&issuer, &issuer.auth(db), &e, "subject").await;
    let other = DeviceService::new(config(), Database::open(&path).unwrap());
    let e2 = e.clone();
    let key2 = key.clone();
    let first = std::thread::spawn(move || other.poll_enrollment(&e2.id, poll(&e2, &key2)));
    let second = service.poll_enrollment(&e.id, poll(&e, &key));
    assert_eq!(
        usize::from(first.join().unwrap().is_ok()) + usize::from(second.is_ok()),
        1
    );
    let c = service.challenge(e.challenge.device_id).unwrap();
    let proof = DeviceProof {
        challenge_id: c.id,
        signature: sign_challenge(&key, &c.challenge).unwrap(),
    };
    let other = DeviceService::new(config(), Database::open(&path).unwrap());
    let p2 = proof.clone();
    let first = std::thread::spawn(move || other.authenticate(p2));
    let second = service.authenticate(proof);
    assert_eq!(
        usize::from(first.join().unwrap().is_ok()) + usize::from(second.is_ok()),
        1
    );
}

#[tokio::test]
async fn expired_transactions_challenges_and_access_tokens_fail_closed() {
    let issuer = Issuer::start().await;
    let dir = private_dir();
    let path = dir.path().join("identity.db");
    let db = Database::open(&path).unwrap();
    let auth = issuer.auth(db.clone());
    let service = DeviceService::new(config(), db);
    let sql = rusqlite::Connection::open(&path).unwrap();
    let key = device_key(4);
    let expired = begin(&service, &key);
    sql.execute("UPDATE enrollments SET expires_at=0", [])
        .unwrap();
    assert!(auth.start_login(&expired.id).is_err());
    assert!(
        service
            .poll_enrollment(&expired.id, poll(&expired, &key))
            .is_err()
    );
    let e = begin(&service, &key);
    let (code, state) = issuer.issue(&auth.start_login(&e.id).unwrap(), "subject", Fault::None);
    sql.execute("UPDATE oauth_transactions SET expires_at=0", [])
        .unwrap();
    assert!(auth.finish_login(&code, &state).await.is_err());
    let (code, state) = issuer.issue(&auth.start_login(&e.id).unwrap(), "subject", Fault::None);
    let login = auth.finish_login(&code, &state).await.unwrap();
    sql.execute("UPDATE confirmations SET expires_at=0", [])
        .unwrap();
    assert!(auth.confirm(&login.confirmation, &e.display_code).is_err());
    confirm(&issuer, &auth, &e, "subject").await;
    service.poll_enrollment(&e.id, poll(&e, &key)).unwrap();
    let c = service.challenge(e.challenge.device_id).unwrap();
    sql.execute("UPDATE device_challenges SET expires_at=0", [])
        .unwrap();
    assert!(
        service
            .authenticate(DeviceProof {
                challenge_id: c.id,
                signature: sign_challenge(&key, &c.challenge).unwrap()
            })
            .is_err()
    );
    let token = access(&service, e.challenge.device_id, &key);
    sql.execute("UPDATE access_tokens SET expires_at=0", [])
        .unwrap();
    assert!(service.list(&token.token).is_err());
    assert!(service.revoke(&token.token, e.challenge.device_id).is_err());
}
