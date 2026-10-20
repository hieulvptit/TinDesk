mod common;
use common::session_fixture;
use remotevnc_backend::sessions::SessionService;
use remotevnc_backend::turn::TurnClient;
use remotevnc_protocol::*;

#[tokio::test]
async fn ownership_single_controller_lease_close_and_restart() {
    let f = session_fixture();
    let service = SessionService::new(f.db.clone(), TurnClient::unavailable());
    assert!(
        service
            .create_authenticated(&f.tokens[2], f.ids[0])
            .await
            .is_err()
    );
    let (a, b) = tokio::join!(
        service.create_authenticated(&f.tokens[0], f.ids[1]),
        service.create_authenticated(&f.tokens[3], f.ids[1])
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let (grant, token) = match (a, b) {
        (Ok(a), _) => (a, &f.tokens[0]),
        (_, Ok(b)) => (b, &f.tokens[3]),
        _ => unreachable!(),
    };
    assert_eq!(grant.turn_status, TurnStatus::Unavailable);
    assert!(grant.ice_servers.is_empty());
    assert_eq!(grant.agent_id, f.ids[1]);
    let remaining = grant.lease_expires_at - common::unix_ms();
    assert!((59_000..=60_000).contains(&remaining));
    assert!(
        service
            .renew_authenticated(&f.tokens[2], grant.session_id)
            .await
            .is_err()
    );
    assert!(
        service
            .refresh_turn_authenticated(&f.tokens[2], grant.session_id)
            .await
            .is_err()
    );
    assert!(
        service
            .close_authenticated(&f.tokens[2], grant.session_id)
            .await
            .is_err()
    );
    let renewed = service
        .renew_authenticated(&f.tokens[1], grant.session_id)
        .await
        .unwrap();
    assert_eq!(renewed.session_id, grant.session_id);
    service
        .close_authenticated(token, grant.session_id)
        .await
        .unwrap();
    assert!(
        service
            .renew_authenticated(&f.tokens[1], grant.session_id)
            .await
            .is_err()
    );
    let next = service.create_authenticated(token, f.ids[1]).await.unwrap();
    let reopened = remotevnc_backend::db::Database::open(f.path()).unwrap();
    let next_runtime = SessionService::new(reopened, TurnClient::unavailable());
    assert!(
        next_runtime
            .renew_authenticated(&f.tokens[1], next.session_id)
            .await
            .is_ok()
    );
    next_runtime.expire_on_startup().await.unwrap();
    assert!(
        next_runtime
            .renew_authenticated(&f.tokens[1], next.session_id)
            .await
            .is_err()
    );
}
#[tokio::test]
async fn revoked_and_expired_devices_cannot_renew_or_fetch_credentials() {
    let f = session_fixture();
    let service = SessionService::new(f.db.clone(), TurnClient::unavailable());
    let grant = service
        .create_authenticated(&f.tokens[0], f.ids[1])
        .await
        .unwrap();
    f.devices.revoke(&f.tokens[0], f.ids[1]).await.unwrap();
    assert!(
        service
            .renew_authenticated(&f.tokens[1], grant.session_id)
            .await
            .is_err()
    );
    assert!(
        service
            .refresh_turn_authenticated(&f.tokens[1], grant.session_id)
            .await
            .is_err()
    );
    assert!(
        service
            .create_authenticated(&f.tokens[0], f.ids[1])
            .await
            .is_err()
    );
    let next = service
        .create_authenticated(&f.tokens[0], f.ids[3])
        .await
        .unwrap();
    let c = rusqlite::Connection::open(f.path()).unwrap();
    c.execute(
        "UPDATE sessions SET lease_expires_at=0 WHERE id=?",
        [next.session_id.0.to_string()],
    )
    .unwrap();
    assert!(
        service
            .renew_authenticated(&f.tokens[3], next.session_id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn devices_cannot_participate_in_two_sessions_in_either_role() {
    let f = session_fixture();
    let service = SessionService::new(f.db.clone(), TurnClient::unavailable());
    service
        .create_authenticated(&f.tokens[0], f.ids[1])
        .await
        .unwrap();
    assert!(matches!(
        service.create_authenticated(&f.tokens[0], f.ids[3]).await,
        Err(remotevnc_backend::Error::CallerBusy)
    ));
    assert!(matches!(
        service.create_authenticated(&f.tokens[3], f.ids[0]).await,
        Err(remotevnc_backend::Error::AgentBusy)
    ));
}

#[tokio::test]
async fn revocation_commits_while_provider_is_pending_and_prevents_grant() {
    use axum::{Json, Router, http::StatusCode, routing::post};
    use std::sync::Arc;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let e = entered.clone();
    let r = release.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/turn", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/turn",post(move ||async move {
        e.notify_one();r.notified().await;
        (StatusCode::CREATED,Json(serde_json::json!({"iceServers":[{"urls":["turn:turn.cloudflare.com:3478?transport=udp"],"username":"fixture","credential":"fixture"}]})))
    }))).await.unwrap()
    });
    let f = session_fixture();
    let service = SessionService::new(
        f.db.clone(),
        TurnClient::new(&endpoint, "fixture-token".into()).unwrap(),
    );
    let creating = service.clone();
    let token = f.tokens[0].clone();
    let agent = f.ids[1];
    let creation = tokio::spawn(async move { creating.create_authenticated(&token, agent).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        f.devices.revoke(&f.tokens[0], agent),
    )
    .await
    .unwrap()
    .unwrap();
    release.notify_one();
    assert!(creation.await.unwrap().is_err());
    task.abort();
}

#[tokio::test]
async fn controller_cannot_extend_agent_authorization_lease() {
    let f = session_fixture();
    let service = SessionService::new(f.db.clone(), TurnClient::unavailable());
    let grant = service
        .create_authenticated(&f.tokens[0], f.ids[1])
        .await
        .unwrap();
    assert!(
        service
            .renew_authenticated(&f.tokens[0], grant.session_id)
            .await
            .is_err()
    );
    let expiry: i64 = rusqlite::Connection::open(f.path())
        .unwrap()
        .query_row("SELECT lease_expires_at FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(expiry as u64, grant.lease_expires_at);
    assert!(
        service
            .renew_authenticated(&f.tokens[1], grant.session_id)
            .await
            .is_ok()
    );
    f.devices.revoke(&f.tokens[1], f.ids[0]).await.unwrap();
    assert!(
        service
            .renew_authenticated(&f.tokens[1], grant.session_id)
            .await
            .is_err()
    );
}
