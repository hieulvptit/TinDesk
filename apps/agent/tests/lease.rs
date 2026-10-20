use remotevnc_agent::service::Authority;
use std::sync::atomic::Ordering;
use tokio::time::{Duration, Instant, advance};
use tokio_util::sync::CancellationToken;

#[tokio::test(start_paused = true)]
async fn delayed_renewal_uses_request_start_and_watchdog_does_not_wait_for_http() {
    let lease = Authority::new();
    lease.renewed(1_000, 16_000).unwrap();
    assert_eq!(lease.deadline.load(Ordering::Acquire), 61_000);
    let clock = Instant::now();
    let stop = CancellationToken::new();
    let stopped = stop.clone();
    let watch = tokio::spawn(async move {
        lease
            .watch(|| 16_000 + clock.elapsed().as_millis() as u64, stopped)
            .await
    });
    tokio::task::yield_now().await;
    advance(Duration::from_millis(44_999)).await;
    assert!(!stop.is_cancelled());
    // An indefinitely held network response cannot occupy the independent watchdog.
    advance(Duration::from_millis(11)).await;
    tokio::task::yield_now().await;
    assert!(stop.is_cancelled());
    watch.await.unwrap();
}
#[test]
fn expired_and_backwards_renewals_cannot_reauthorize() {
    let lease = Authority::new();
    assert!(lease.renewed(1_000, 61_000).is_err());
    lease.renewed(10_000, 10_001).unwrap();
    assert!(lease.renewed(9_000, 10_002).is_err());
    assert_eq!(lease.deadline.load(Ordering::Acquire), 70_000);
}

#[tokio::test]
async fn sdk_distinguishes_denial_from_transient_http_failure() {
    use remotevnc_protocol::{AccessToken, SessionId};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route(
            "/v1/sessions/{id}/renew",
            axum::routing::post(|| async { axum::http::StatusCode::UNAUTHORIZED }),
        )
        .route(
            "/v1/sessions/{id}/turn",
            axum::routing::post(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        )
        .route(
            "/v1/events",
            axum::routing::get(|| async { axum::http::StatusCode::UNAUTHORIZED }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let api =
        remotevnc_device::DeviceApi::new(&base, ed25519_dalek::SigningKey::from_bytes(&[1; 32]))
            .unwrap();
    let token = AccessToken {
        token: "fixture".into(),
        expires_at_unix_ms: u64::MAX,
    };
    let session = SessionId::from_uuid(uuid::Uuid::new_v4());
    assert!(matches!(
        api.renew_session(&token, session).await,
        Err(remotevnc_device::Error::Unauthorized)
    ));
    assert!(matches!(
        api.refresh_turn(&token, session).await,
        Err(remotevnc_device::Error::Request)
    ));
    assert!(matches!(
        api.events(&token, remotevnc_protocol::DeviceRole::Agent)
            .await,
        Err(remotevnc_device::Error::Unauthorized)
    ));
    server.abort();
}

#[tokio::test(start_paused = true)]
async fn watchdog_interrupts_partial_video_and_held_renewal_at_sixty_seconds() {
    use remotevnc_agent::helper::Helper;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixListener,
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("helper.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (header_tx, header_rx) = std::sync::mpsc::channel();
    let helper_process = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .write_all(
                &(0x80000000u32 | (remotevnc_protocol::ENCODED_FRAME_LIMIT as u32 + 25))
                    .to_be_bytes(),
            )
            .unwrap();
        header_tx.send(()).unwrap();
        let mut byte = [0];
        assert_eq!(socket.read(&mut byte).unwrap(), 0);
    });
    let helper = Helper::connect(&path).unwrap();
    header_rx.recv().unwrap();
    let lease = Authority::new();
    lease.renewed(1_000, 1_000).unwrap();
    let stop = CancellationToken::new();
    let ended = stop.clone();
    let clock = Instant::now();
    let watcher = tokio::spawn(async move {
        lease
            .watch(|| 1_000 + clock.elapsed().as_millis() as u64, ended)
            .await;
        helper.disconnect();
    });
    let held_renewal = tokio::spawn(std::future::pending::<()>());
    tokio::task::yield_now().await;
    advance(Duration::from_millis(59_999)).await;
    assert!(!stop.is_cancelled());
    advance(Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(stop.is_cancelled());
    assert!(!held_renewal.is_finished());
    held_renewal.abort();
    watcher.await.unwrap();
    helper_process.join().unwrap();
}
