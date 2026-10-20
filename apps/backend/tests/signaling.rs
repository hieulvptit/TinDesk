mod common;
use common::*;
use futures_util::{SinkExt, StreamExt};
use remotevnc_backend::{
    BackendState, auth::AuthService, sessions::SessionService, turn::TurnClient,
};
use remotevnc_protocol::*;
use std::time::Duration;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn server(
    f: &SessionFixture,
    turn: TurnClient,
) -> (String, SessionService, tokio::task::JoinHandle<()>) {
    let sessions = SessionService::new(f.db.clone(), turn);
    let metadata = serde_json::from_value(serde_json::json!({"issuer":"https://accounts.google.com","authorization_endpoint":"https://accounts.google.com/auth","token_endpoint":"https://accounts.google.com/token","jwks_uri":"https://accounts.google.com/jwks","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]})).unwrap();
    let state = BackendState {
        config: config(),
        auth: AuthService::new(config(), f.db.clone(), metadata).unwrap(),
        devices: f.devices.clone(),
        sessions: sessions.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    (
        base,
        sessions,
        tokio::spawn(async move {
            axum::serve(listener, remotevnc_backend::router(state))
                .await
                .unwrap()
        }),
    )
}
async fn socket(
    base: &str,
    token: &str,
    role: &str,
) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
    let mut req = format!("{}/v1/events?role={role}", base.replacen("http", "ws", 1))
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    connect_async(req).await.map(|(s, _)| s)
}
async fn event(s: &mut Socket) -> DeviceEvent {
    let m = tokio::time::timeout(Duration::from_secs(2), s.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    decode_control(&m.into_data()).unwrap()
}
async fn signal(s: &mut Socket, receiver: DeviceId, message: SignalMessage) {
    s.send(Message::Text(
        String::from_utf8(encode_control(&DeviceCommand::Signal { receiver, message }).unwrap())
            .unwrap()
            .into(),
    ))
    .await
    .unwrap();
}
async fn next_signal(s: &mut Socket) -> SignalMessage {
    loop {
        if let DeviceEvent::Signal { message, .. } = event(s).await {
            return message;
        }
    }
}
#[tokio::test]
async fn authenticated_ordered_signals_and_live_revocation() {
    let f = session_fixture();
    let (base, service, task) = server(&f, TurnClient::unavailable()).await;
    assert!(socket(&base, "invalid", "client").await.is_err());
    let no_bearer = format!(
        "{}/v1/events?role=client&token={}",
        base.replacen("http", "ws", 1),
        f.tokens[0]
    );
    assert!(connect_async(no_bearer).await.is_err());

    let mut client = socket(&base, &f.tokens[0], "client").await.unwrap();
    assert!(matches!(
        event(&mut client).await,
        DeviceEvent::Presence { .. }
    ));
    assert!(socket(&base, &f.tokens[0], "client").await.is_err());
    let mut agent = socket(&base, &f.tokens[1], "agent").await.unwrap();
    let grant = service
        .create_authenticated(&f.tokens[0], f.ids[1])
        .await
        .unwrap();
    let messages = [
        SignalMessage::Display {
            session_id: grant.session_id,
            epoch: 1,
            display: DisplayInfo {
                generation: 7,
                width: 1280,
                height: 720,
            },
        },
        SignalMessage::Restart {
            session_id: grant.session_id,
            epoch: 2,
        },
        SignalMessage::Offer {
            session_id: grant.session_id,
            epoch: 2,
            sdp: "ordered".into(),
        },
    ];
    for m in &messages {
        signal(&mut agent, f.ids[0], m.clone()).await;
    }
    for expected in messages {
        assert!(next_signal(&mut client).await == expected);
    }
    f.devices.revoke(&f.tokens[0], f.ids[1]).await.unwrap();
    let _ = agent
        .send(Message::Text(
            serde_json::to_string(&DeviceCommand::Signal {
                receiver: f.ids[0],
                message: SignalMessage::Offer {
                    session_id: grant.session_id,
                    epoch: 2,
                    sdp: "after-revoke".into(),
                },
            })
            .unwrap()
            .into(),
        ))
        .await;
    let received = tokio::time::timeout(Duration::from_millis(1200), async {
        while let Some(Ok(m)) = client.next().await {
            if let Ok(DeviceEvent::Signal { .. }) = decode_control::<DeviceEvent>(&m.into_data()) {
                return true;
            }
        }
        false
    })
    .await;
    assert!(!matches!(received, Ok(true)));
    assert!(
        service
            .renew_authenticated(&f.tokens[1], grant.session_id)
            .await
            .is_err()
    );
    task.abort();
}
#[tokio::test]
async fn cross_account_and_client_metadata_are_never_forwarded() {
    for wrong_owner in [true, false] {
        let f = session_fixture();
        let (base, service, task) = server(&f, TurnClient::unavailable()).await;
        let grant = service
            .create_authenticated(&f.tokens[0], f.ids[1])
            .await
            .unwrap();
        let mut agent = socket(&base, &f.tokens[1], "agent").await.unwrap();
        let mut sender = socket(&base, &f.tokens[if wrong_owner { 2 } else { 0 }], "client")
            .await
            .unwrap();
        signal(
            &mut sender,
            f.ids[1],
            SignalMessage::DisplayUnavailable {
                session_id: grant.session_id,
                epoch: 1,
                generation: 1,
            },
        )
        .await;
        let received = tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                if let DeviceEvent::Signal { .. } = event(&mut agent).await {
                    return true;
                }
            }
        })
        .await;
        assert!(received.is_err());
        task.abort();
    }
}
#[tokio::test]
async fn http_sessions_require_bearer_and_return_explicit_turn_status() {
    let f = session_fixture();
    let (base, _, task) = server(&f, TurnClient::unavailable()).await;
    let http = reqwest::Client::new();
    assert_eq!(
        http.post(format!("{base}/v1/sessions"))
            .json(&CreateSession { agent_id: f.ids[1] })
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        http.post(format!("{base}/v1/sessions"))
            .bearer_auth(&f.tokens[2])
            .json(&CreateSession { agent_id: f.ids[1] })
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let response = http
        .post(format!("{base}/v1/sessions"))
        .bearer_auth(&f.tokens[0])
        .json(&CreateSession { agent_id: f.ids[1] })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let grant: SessionGrant = response.json().await.unwrap();
    assert_eq!(grant.turn_status, TurnStatus::Unavailable);
    task.abort();
}

#[tokio::test]
async fn device_sdk_session_presence_and_event_contract() {
    let f = session_fixture();
    let (base, _, task) = server(&f, TurnClient::unavailable()).await;
    let api = remotevnc_device::DeviceApi::new(&base, device_key(1)).unwrap();
    let token = AccessToken {
        token: f.tokens[0].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let grant = api.create_session(&token, f.ids[1]).await.unwrap();
    assert_eq!(grant.turn_status, TurnStatus::Unavailable);
    let connection = api.events(&token, DeviceRole::Client).await.unwrap();
    let (mut sender, mut events) = connection.split();
    assert!(matches!(
        events.next().await.unwrap(),
        Some(DeviceEvent::Presence { .. })
    ));
    assert_eq!(api.presence(&token).await.unwrap().len(), 1);
    assert_eq!(
        api.renew_session(
            &AccessToken {
                token: f.tokens[1].clone(),
                expires_at_unix_ms: unix_ms() + 300_000
            },
            grant.session_id
        )
        .await
        .unwrap()
        .session_id,
        grant.session_id
    );
    api.close_session(&token, grant.session_id).await.unwrap();
    assert!(api.renew_session(&token, grant.session_id).await.is_err());
    sender.close().await.unwrap();
    task.abort();
}

async fn next_configuration(s: &mut Socket) -> (u64, bool, Vec<IceServer>) {
    loop {
        if let DeviceEvent::IceConfiguration {
            generation,
            requires_ack,
            ice_servers,
            ..
        } = event(s).await
        {
            return (generation, requires_ack, ice_servers);
        }
    }
}
async fn installed(s: &mut Socket, session_id: SessionId, generation: u64) {
    s.send(Message::Text(
        serde_json::to_string(&DeviceCommand::IceInstalled {
            session_id,
            generation,
        })
        .unwrap()
        .into(),
    ))
    .await
    .unwrap();
}
#[tokio::test]
async fn turn_refresh_requires_both_installs_and_replays_readiness_on_reconnect() {
    use axum::{Json, Router, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/turn", provider.local_addr().unwrap());
    let provider_task = tokio::spawn(async move {
        axum::serve(provider,Router::new().route("/turn",post(move || async move {
        let n=count.fetch_add(1,Ordering::SeqCst);
        (StatusCode::CREATED,Json(serde_json::json!({"iceServers":[{"urls":["turn:turn.cloudflare.com:3478?transport=udp"],"username":format!("generation-{n}"),"credential":"fixture"}]})))
    }))).await.unwrap()
    });
    let f = session_fixture();
    let (base, service, task) = server(
        &f,
        TurnClient::new(&endpoint, "fixture-token".into()).unwrap(),
    )
    .await;
    assert!(
        service
            .create_authenticated(&f.tokens[2], f.ids[1])
            .await
            .is_err()
    );
    assert_eq!(requests.load(Ordering::SeqCst), 0);
    let grant = service
        .create_authenticated(&f.tokens[0], f.ids[1])
        .await
        .unwrap();
    assert_eq!(grant.turn_status, TurnStatus::Available);
    let mut client = socket(&base, &f.tokens[0], "client").await.unwrap();
    let mut agent = socket(&base, &f.tokens[1], "agent").await.unwrap();
    let sql = rusqlite::Connection::open(f.path()).unwrap();
    sql.execute(
        "UPDATE sessions SET turn_expires_at=?",
        [unix_ms() as i64 / 1000 + 300],
    )
    .unwrap();
    assert!(
        service
            .refresh_turn_authenticated(&f.tokens[0], grant.session_id)
            .await
            .is_err()
    );
    assert!(
        service
            .refresh_turn_authenticated(&f.tokens[2], grant.session_id)
            .await
            .is_err()
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    service
        .refresh_turn_authenticated(&f.tokens[1], grant.session_id)
        .await
        .unwrap();
    let (generation, ack, ice) = next_configuration(&mut agent).await;
    assert!(ack);
    let (other_generation, other_ack, other_ice) = next_configuration(&mut client).await;
    assert_eq!(generation, other_generation);
    assert!(other_ack && ice == other_ice);
    installed(&mut agent, grant.session_id, generation).await;
    let premature = tokio::time::timeout(Duration::from_millis(100), async {
        loop {
            if matches!(event(&mut agent).await, DeviceEvent::IceRestartReady { .. }) {
                return;
            }
        }
    })
    .await;
    assert!(premature.is_err());
    installed(&mut client, grant.session_id, generation).await;
    loop {
        if let DeviceEvent::IceRestartReady { generation: g, .. } = event(&mut agent).await {
            assert_eq!(g, generation);
            break;
        }
    }
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    agent.close(None).await.unwrap();
    drop(agent);
    let mut reconnected = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match socket(&base, &f.tokens[1], "agent").await {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap();
    let mut snapshot = false;
    loop {
        match event(&mut reconnected).await {
            DeviceEvent::SessionSnapshot { session_ids } => {
                assert_eq!(session_ids, vec![grant.session_id]);
                snapshot = true;
            }
            DeviceEvent::IceConfiguration {
                generation: g,
                requires_ack,
                ..
            } => {
                assert_eq!(g, generation);
                assert!(!requires_ack);
                assert!(snapshot);
            }
            DeviceEvent::IceRestartReady { generation: g, .. } => {
                assert_eq!(g, generation);
                break;
            }
            _ => {}
        }
    }
    service
        .close_authenticated(&f.tokens[0], grant.session_id)
        .await
        .unwrap();
    reconnected.close(None).await.unwrap();
    drop(reconnected);
    let mut reconnected = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match socket(&base, &f.tokens[1], "agent").await {
                Ok(s) => break s,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .unwrap();
    loop {
        if let DeviceEvent::SessionSnapshot { session_ids } = event(&mut reconnected).await {
            assert!(session_ids.is_empty());
            break;
        }
    }
    task.abort();
    provider_task.abort();
}

#[tokio::test]
async fn failed_refresh_does_not_invent_completed_generation() {
    use axum::{Json, Router, http::StatusCode, routing::post};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let count = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/turn", listener.local_addr().unwrap());
    let provider = tokio::spawn(async move {
        axum::serve(listener,Router::new().route("/turn",post(move ||async move {
        let status=if count.fetch_add(1,Ordering::SeqCst)==0 {StatusCode::CREATED}else{StatusCode::SERVICE_UNAVAILABLE};
        (status,Json(serde_json::json!({"iceServers":[{"urls":["turn:turn.cloudflare.com:3478?transport=udp"],"username":"fixture","credential":"fixture"}]})))
    }))).await.unwrap()
    });
    let f = session_fixture();
    let (base, service, task) = server(
        &f,
        TurnClient::new(&endpoint, "fixture-token".into()).unwrap(),
    )
    .await;
    let grant = service
        .create_authenticated(&f.tokens[0], f.ids[1])
        .await
        .unwrap();
    let _client = socket(&base, &f.tokens[0], "client").await.unwrap();
    let _agent = socket(&base, &f.tokens[1], "agent").await.unwrap();
    let sql = rusqlite::Connection::open(f.path()).unwrap();
    sql.execute("UPDATE sessions SET turn_expires_at=0", [])
        .unwrap();
    assert!(
        service
            .refresh_turn_authenticated(&f.tokens[1], grant.session_id)
            .await
            .is_err()
    );
    let generation: i64 = sql
        .query_row("SELECT refresh_generation FROM sessions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(generation, 0);
    task.abort();
    provider.abort();
}

#[tokio::test]
async fn message_size_rate_and_expired_socket_tokens_are_enforced() {
    for fault in ["size", "rate", "expired", "wrong_session", "wrong_receiver"] {
        let f = session_fixture();
        let (base, service, task) = server(&f, TurnClient::unavailable()).await;
        let grant = service
            .create_authenticated(&f.tokens[0], f.ids[1])
            .await
            .unwrap();
        let mut socket = socket(&base, &f.tokens[0], "client").await.unwrap();
        match fault {
            "size" => {
                let _ = socket
                    .send(Message::Text("x".repeat(CONTROL_PAYLOAD_LIMIT + 1).into()))
                    .await;
            }
            "rate" => {
                for _ in 0..121 {
                    if socket.send(Message::Ping(vec![1].into())).await.is_err() {
                        break;
                    }
                }
            }
            "expired" => {
                rusqlite::Connection::open(f.path())
                    .unwrap()
                    .execute("UPDATE access_tokens SET expires_at=0", [])
                    .unwrap();
                let _ = socket.send(Message::Ping(vec![1].into())).await;
            }
            _ => {
                signal(
                    &mut socket,
                    if fault == "wrong_receiver" {
                        f.ids[2]
                    } else {
                        f.ids[1]
                    },
                    SignalMessage::Offer {
                        session_id: if fault == "wrong_session" {
                            SessionId(uuid::Uuid::new_v4())
                        } else {
                            grant.session_id
                        },
                        epoch: 1,
                        sdp: "rejected".into(),
                    },
                )
                .await;
            }
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(Ok(message)) = socket.next().await {
                if matches!(message, Message::Close(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        task.abort();
    }
}

#[tokio::test]
async fn close_revoke_and_expiry_notify_surviving_peer_and_revoked_socket_has_policy_close() {
    for action in ["close", "revoke", "expire"] {
        let f = session_fixture();
        let (base, service, task) = server(&f, TurnClient::unavailable()).await;
        let mut client = socket(&base, &f.tokens[0], "client").await.unwrap();
        let mut agent = socket(&base, &f.tokens[1], "agent").await.unwrap();
        let grant = service
            .create_authenticated(&f.tokens[0], f.ids[1])
            .await
            .unwrap();
        loop {
            if matches!(event(&mut client).await, DeviceEvent::Session { .. }) {
                break;
            }
        }
        loop {
            if matches!(event(&mut agent).await, DeviceEvent::Session { .. }) {
                break;
            }
        }
        match action {
            "close" => service
                .close_authenticated(&f.tokens[0], grant.session_id)
                .await
                .unwrap(),
            "revoke" => f.devices.revoke(&f.tokens[0], f.ids[1]).await.unwrap(),
            _ => {
                rusqlite::Connection::open(f.path())
                    .unwrap()
                    .execute("UPDATE sessions SET lease_expires_at=0", [])
                    .unwrap();
            }
        }
        loop {
            if let DeviceEvent::SessionClosed { session_id } = event(&mut client).await {
                assert_eq!(session_id, grant.session_id);
                break;
            }
        }
        if action == "revoke" {
            tokio::time::timeout(Duration::from_secs(2),async {
                loop {if let Some(Ok(Message::Close(Some(frame))))=agent.next().await {assert_eq!(frame.code,tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy);break;}}
            }).await.unwrap();
        }
        task.abort();
    }
}

#[tokio::test]
async fn sdk_sends_typed_signals_and_distinguishes_policy_close() {
    let f = session_fixture();
    let (base, service, task) = server(&f, TurnClient::unavailable()).await;
    let api = remotevnc_device::DeviceApi::new(&base, device_key(1)).unwrap();
    let token = AccessToken {
        token: f.tokens[0].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let (mut sender, mut events) = api
        .events(&token, DeviceRole::Client)
        .await
        .unwrap()
        .split();
    let mut agent = socket(&base, &f.tokens[1], "agent").await.unwrap();
    let grant = service
        .create_authenticated(&f.tokens[0], f.ids[1])
        .await
        .unwrap();
    let message = SignalMessage::Offer {
        session_id: grant.session_id,
        epoch: 1,
        sdp: "sdk-offer".into(),
    };
    sender
        .send(&DeviceCommand::Signal {
            receiver: f.ids[1],
            message: message.clone(),
        })
        .await
        .unwrap();
    assert!(next_signal(&mut agent).await == message);
    f.devices.revoke(&f.tokens[1], f.ids[0]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match events.next().await {
                Err(remotevnc_device::Error::PolicyClosed) => break,
                Ok(Some(_)) => {}
                _ => panic!("expected typed policy close"),
            }
        }
    })
    .await
    .unwrap();
    task.abort();
}
