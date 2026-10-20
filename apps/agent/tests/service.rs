#[path = "../../backend/tests/common/mod.rs"]
mod common;
use common::*;
use remotevnc_agent::{config::Config, service::AgentService};
use remotevnc_backend::{BackendState, sessions::SessionService, turn::TurnClient};
use remotevnc_device::{CredentialStore, FileStore, StoredCredentials};
use remotevnc_protocol::*;
use std::{os::unix::net::UnixListener, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boot_without_user_session_reconciles_grant_and_revocation() {
    let f = session_fixture();
    let issuer = Issuer::start().await;
    let sessions = SessionService::new(f.db.clone(), TurnClient::unavailable());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = BackendState {
        config: config(),
        auth: issuer.auth(f.db.clone()),
        devices: f.devices.clone(),
        sessions: sessions.clone(),
    };
    let backend = tokio::spawn(async move {
        axum::serve(listener, remotevnc_backend::router(state))
            .await
            .unwrap()
    });
    let state_dir = private_dir();
    FileStore::new(state_dir.path())
        .save(&StoredCredentials {
            device_id: Some(f.ids[1]),
            signing_key: device_key(2),
        })
        .unwrap();
    let socket = state_dir.path().join("helper.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let helper = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        write_helper_output(&mut stream, &HelperOutput::Event(HelperEvent::Ready)).unwrap();
        accepted_tx.send(()).unwrap();
        use std::io::Read;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut byte = [0];
        assert_eq!(
            stream.read(&mut byte).unwrap(),
            0,
            "no physical Start without a display; revoke closes connection"
        );
    });
    let stop = CancellationToken::new();
    let service = AgentService::new(Config {
        backend_origin: base.clone(),
        identity_path: state_dir.path().into(),
        helper_socket: socket,
        seat: "seat0".into(),
        relay_only: false,
    })
    .unwrap();
    let run = tokio::spawn(service.run(stop.clone()));
    let client = remotevnc_device::DeviceApi::new(&base, device_key(1)).unwrap();
    let token = AccessToken {
        token: f.tokens[0].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let _client_socket = client.events(&token, DeviceRole::Client).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if client
            .presence(&token)
            .await
            .unwrap()
            .iter()
            .any(|p| p.device_id == f.ids[1] && p.online)
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let grant = client.create_session(&token, f.ids[1]).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), accepted_rx)
        .await
        .unwrap()
        .unwrap();
    f.devices.revoke(&f.tokens[0], f.ids[1]).await.unwrap();
    tokio::task::spawn_blocking(move || helper.join().unwrap())
        .await
        .unwrap();
    assert!(
        sessions
            .renew_authenticated(&token.token, grant.session_id)
            .await
            .is_err()
    );
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(3), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    backend.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn focus_release_preserves_video_and_renewal_is_at_fifteen_seconds() {
    use remotevnc_transport::*;
    let f = session_fixture();
    let issuer = Issuer::start().await;
    let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/turn", provider.local_addr().unwrap());
    let provider_task = tokio::spawn(async move {
        axum::serve(provider,axum::Router::new().route("/turn",axum::routing::post(|| async {
   (axum::http::StatusCode::CREATED,axum::Json(serde_json::json!({"iceServers":[{"urls":["turn:127.0.0.1:3478?transport=udp"],"username":"fixture-user","credential":"fixture-password"}]})))
 }))).await.unwrap()
    });
    let sessions = SessionService::new(
        f.db.clone(),
        TurnClient::new(&endpoint, "fixture-token".into()).unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = BackendState {
        config: config(),
        auth: issuer.auth(f.db.clone()),
        devices: f.devices.clone(),
        sessions: sessions.clone(),
    };
    let authentications = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = authentications.clone();
    // Real bearer and proof verification; only the advertised TTL is shortened to exercise rotation.
    let app = remotevnc_backend::router(state).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let count = observed.clone();
            async move {
                let authentication = request.uri().path() == "/v1/devices/authenticate";
                let response = next.run(request).await;
                if authentication && response.status().is_success() {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let (parts, body) = response.into_parts();
                    let bytes = axum::body::to_bytes(body, 65536).await.unwrap();
                    let mut token: AccessToken = serde_json::from_slice(&bytes).unwrap();
                    token.expires_at_unix_ms = unix_ms() + 50_000;
                    axum::response::Response::from_parts(
                        parts,
                        axum::body::Body::from(serde_json::to_vec(&token).unwrap()),
                    )
                } else {
                    response
                }
            }
        },
    ));
    let backend = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = private_dir();
    FileStore::new(dir.path())
        .save(&StoredCredentials {
            device_id: Some(f.ids[1]),
            signing_key: device_key(2),
        })
        .unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (requests_tx, mut requests) = tokio::sync::mpsc::channel(128);
    let helper = std::thread::spawn(move || {
        use std::io::Read;
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        write_helper_output(&mut stream, &HelperOutput::Event(HelperEvent::Ready)).unwrap();
        write_helper_output(
            &mut stream,
            &HelperOutput::Event(HelperEvent::DisplayChanged(DisplayInfo {
                generation: 1,
                width: 640,
                height: 480,
            })),
        )
        .unwrap();
        let mut decoder = remotevnc_seat_helper::ipc::FrameDecoder::default();
        let mut capture = false;
        let encoded = encoded_fixture();
        let mut last = std::time::Instant::now();
        let mut pts = 0;
        loop {
            let mut bytes = [0; 4096];
            match stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    for frame in decoder.push(&bytes[..n]).unwrap() {
                        let request: HelperRequest = decode_control(&frame).unwrap();
                        if matches!(request, HelperRequest::StartCapture { .. }) {
                            capture = true;
                        }
                        requests_tx.blocking_send(request).unwrap();
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
            if capture && last.elapsed() > Duration::from_millis(100) {
                let frame = EncodedFrame {
                    generation: 1,
                    pts_ns: pts,
                    keyframe: true,
                    data: encoded.clone(),
                };
                pts += 100_000_000;
                last = std::time::Instant::now();
                if write_helper_output(&mut stream, &HelperOutput::Video(frame)).is_err() {
                    break;
                }
            }
        }
    });
    let config = Config {
        backend_origin: base.clone(),
        identity_path: dir.path().into(),
        helper_socket: socket,
        seat: "seat0".into(),
        relay_only: false,
    };
    let stop = CancellationToken::new();
    let run = tokio::spawn(AgentService::new(config).unwrap().run(stop.clone()));
    let api = remotevnc_device::DeviceApi::new(&base, device_key(1)).unwrap();
    let token = AccessToken {
        token: f.tokens[0].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let connection = api.events(&token, DeviceRole::Client).await.unwrap();
    let (mut sender, mut incoming) = connection.split();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !api
        .presence(&token)
        .await
        .unwrap()
        .iter()
        .any(|p| p.device_id == f.ids[1] && p.online)
    {
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let grant = api.create_session(&token, f.ids[1]).await.unwrap();
    let mut controller = Peer::new(PeerConfig {
        session_id: grant.session_id,
        local_peer_id: f.ids[0],
        remote_peer_id: f.ids[1],
        role: PeerRole::Controller,
        display: None,
        ice_servers: grant.ice_servers.clone(),
        relay_only: false,
    })
    .unwrap();
    controller.enable_h264().unwrap();
    let start = tokio::time::Instant::now();
    let end = start + Duration::from_secs(23);
    let mut started = None;
    let mut channels = false;
    let mut sent = false;
    let mut held = false;
    let mut released = false;
    let mut decoded_after_release = false;
    let mut renewed = false;
    loop {
        tokio::select! {
          event=incoming.next()=>match event.unwrap().unwrap(){
            DeviceEvent::Signal{message,..}=>{
              let restart=matches!(message,SignalMessage::Restart{..});
              let epoch=match &message{SignalMessage::Display{epoch,..}|SignalMessage::DisplayUnavailable{epoch,..}|SignalMessage::Restart{epoch,..}|SignalMessage::Offer{epoch,..}|SignalMessage::Answer{epoch,..}|SignalMessage::IceCandidate{epoch,..}|SignalMessage::End{epoch,..}=>*epoch};
              if let Err(e)=controller.handle_signal(message){assert!(e==TransportError::StaleEpoch && epoch<controller.epoch(),"{e}");}
              if restart {controller.start_negotiation().unwrap();}
            },
            DeviceEvent::SessionClosed{..}=>panic!("focus release unexpectedly closed session"),
            _=>{},
          },
          _=tokio::time::sleep(Duration::from_millis(5))=>{},
        }
        for event in controller.events().try_iter().collect::<Vec<_>>() {
            match event {
                PeerEvent::Signal(message) => sender
                    .send(&DeviceCommand::Signal {
                        receiver: f.ids[1],
                        message,
                    })
                    .await
                    .unwrap(),
                PeerEvent::ChannelsReady => channels = true,
                PeerEvent::Frame(_) if released => decoded_after_release = true,
                PeerEvent::Error(e) => panic!("controller {e}"),
                _ => {}
            }
        }
        if channels && !sent {
            for (sequence, event) in [
                InputEvent::DisplayAck,
                InputEvent::Key {
                    keysym: 0x61,
                    pressed: true,
                },
                InputEvent::ReleaseAll,
            ]
            .into_iter()
            .enumerate()
            {
                controller
                    .send_input(InputEnvelope {
                        version: WIRE_VERSION,
                        session_id: grant.session_id,
                        generation: 1,
                        sequence: sequence as u64,
                        event,
                    })
                    .unwrap();
            }
            sent = true;
        }
        while let Ok(request) = requests.try_recv() {
            match request {
                HelperRequest::Start {
                    deadline_boot_ms, ..
                } => {
                    assert!(started.replace(deadline_boot_ms).is_none());
                }
                HelperRequest::Input(input) => match input.event {
                    InputEvent::Key { pressed: true, .. } => held = true,
                    InputEvent::ReleaseAll => {
                        assert!(held);
                        held = false;
                        released = true;
                    }
                    _ => {}
                },
                HelperRequest::RenewLease {
                    deadline_boot_ms, ..
                } => {
                    assert!(
                        start.elapsed() >= Duration::from_secs(14),
                        "activity must not renew before scheduled interval"
                    );
                    assert!(deadline_boot_ms >= started.unwrap() + 14_000);
                    renewed = true;
                }
                HelperRequest::Stop { .. } => panic!("ordinary focus loss must preserve capture"),
                _ => {}
            }
        }
        if released && decoded_after_release && renewed {
            break;
        }
        assert!(
            tokio::time::Instant::now() < end,
            "focus/video/renewal proof did not finish"
        );
    }
    assert!(!held);
    // Coordinate real TURN refresh. The controller installs but withholds ACK first.
    rusqlite::Connection::open(f.path())
        .unwrap()
        .execute("UPDATE sessions SET turn_expires_at=0", [])
        .unwrap();
    sessions
        .refresh_turn_authenticated(&f.tokens[1], grant.session_id)
        .await
        .unwrap();
    let generation = loop {
        match incoming.next().await.unwrap().unwrap() {
            DeviceEvent::IceConfiguration {
                generation,
                requires_ack,
                ice_servers,
                ..
            } => {
                assert!(requires_ack);
                controller.install_ice(ice_servers).unwrap();
                break generation;
            }
            DeviceEvent::Signal {
                message: SignalMessage::Restart { .. },
                ..
            } => panic!("restart before both installs"),
            _ => {}
        }
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(150), async {
            loop {
                if matches!(
                    incoming.next().await.unwrap().unwrap(),
                    DeviceEvent::Signal {
                        message: SignalMessage::Restart { .. },
                        ..
                    }
                ) {
                    break;
                }
            }
        })
        .await
        .is_err()
    );
    let previous_epoch = controller.epoch();
    sender
        .send(&DeviceCommand::IceInstalled {
            session_id: grant.session_id,
            generation,
        })
        .await
        .unwrap();
    let restarted = tokio::time::Instant::now();
    loop {
        if let DeviceEvent::Signal {
            message: SignalMessage::Restart { epoch, .. },
            ..
        } = incoming.next().await.unwrap().unwrap()
        {
            assert_eq!(epoch, previous_epoch + 1);
            break;
        }
    }
    // The hub accepted Restart, but target socket dies before the app applies it.
    sender.close().await.unwrap();
    drop(sender);
    drop(incoming);
    let rotated = api
        .authenticate(&remotevnc_device::DeviceIdentity {
            device_id: f.ids[0],
            signing_key: device_key(1),
        })
        .await
        .unwrap();
    let connection = loop {
        match api.events(&rotated, DeviceRole::Client).await {
            Ok(s) => break s,
            Err(remotevnc_device::Error::Limit) => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(e) => panic!("{e}"),
        }
    };
    let (_sender, mut incoming) = connection.split();
    let mut renewals_after_restart = 0;
    let mut closed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(23);
    while tokio::time::Instant::now() < deadline {
        tokio::select! {
          event=incoming.next()=>match event.unwrap().unwrap(){
            DeviceEvent::SessionClosed{session_id}=>{assert_eq!(session_id,grant.session_id);closed=true;break;},
            DeviceEvent::IceConfiguration{requires_ack,..}=>assert!(!requires_ack),
            DeviceEvent::Signal{message:SignalMessage::Restart{..},..}=>panic!("replayed readiness caused duplicate restart"),
            _=>{},
          },
          _=tokio::time::sleep(Duration::from_millis(10))=>{},
        }
        while let Ok(request) = requests.try_recv() {
            if matches!(request, HelperRequest::RenewLease { .. }) {
                renewals_after_restart += 1;
            }
        }
    }
    assert!(closed, "lost Restart must fall back to a fresh session");
    assert!(restarted.elapsed() >= Duration::from_secs(19));
    assert!(
        renewals_after_restart >= 1,
        "timeout must survive successful lease renewal"
    );
    assert!(
        authentications.load(std::sync::atomic::Ordering::SeqCst) >= 3,
        "agent and controller both rotated tokens"
    );
    assert_eq!(controller.epoch(), previous_epoch);
    stop.cancel();
    run.await.unwrap().unwrap();
    helper.join().unwrap();
    backend.abort();
    provider_task.abort();
}
fn encoded_fixture() -> Vec<u8> {
    use gst::prelude::*;
    gst::init().unwrap();
    let pipeline=gst::parse::launch("videotestsrc num-buffers=1 pattern=red ! video/x-raw,width=320,height=240,framerate=10/1 ! videoconvert ! x264enc tune=zerolatency speed-preset=ultrafast bframes=0 ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=sink").unwrap().downcast::<gst::Pipeline>().unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let sample: Option<gst::Sample> = pipeline
        .by_name("sink")
        .unwrap()
        .emit_by_name("try-pull-sample", &[&5_000_000_000u64]);
    let bytes = sample
        .unwrap()
        .buffer()
        .unwrap()
        .map_readable()
        .unwrap()
        .as_slice()
        .to_vec();
    pipeline.set_state(gst::State::Null).unwrap();
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cli_oidc_candidate_recovery_and_running_local_revoke() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };
    let f = session_fixture();
    let issuer = Issuer::start().await;
    let auth = issuer.auth(f.db.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = BackendState {
        config: config(),
        auth: auth.clone(),
        devices: f.devices.clone(),
        sessions: SessionService::new(f.db.clone(), TurnClient::unavailable()),
    };
    let backend = tokio::spawn(async move {
        axum::serve(listener, remotevnc_backend::router(state))
            .await
            .unwrap()
    });
    let dir = private_dir();
    let identity = dir.path().join("identity");
    let config = Config {
        backend_origin: base.clone(),
        identity_path: identity.clone(),
        helper_socket: dir.path().join("absent-helper.sock"),
        seat: "seat0".into(),
        relay_only: false,
    };
    let config_path = dir.path().join("config.json");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let binary = env!("CARGO_BIN_EXE_remotevnc-agent");
    let mut enroll = Command::new(binary)
        .args(["enroll", "--config"])
        .arg(&config_path)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(enroll.stdout.take().unwrap()).lines();
    let (line, _remaining_stdout) =
        tokio::task::spawn_blocking(move || (lines.next().unwrap().unwrap(), lines))
            .await
            .unwrap();
    let parts: Vec<_> = line.split_whitespace().collect();
    assert_eq!(parts[0], "Open");
    let enrollment = url::Url::parse(parts[1])
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "enrollment")
        .unwrap()
        .1
        .into_owned();
    let candidate = FileStore::new(&identity).load().unwrap().unwrap();
    assert!(
        candidate.device_id.is_some(),
        "candidate must precede first completion poll"
    );
    let (code, state) = issuer.issue(
        &auth.start_login(&enrollment).unwrap(),
        "owner",
        Fault::None,
    );
    let login = auth.finish_login(&code, &state).await.unwrap();
    auth.confirm(&login.confirmation, parts.last().unwrap())
        .unwrap();
    assert!(
        tokio::task::spawn_blocking(move || enroll.wait().unwrap())
            .await
            .unwrap()
            .success()
    );
    let path = config_path.clone();
    let recovered = tokio::task::spawn_blocking(move || {
        Command::new(binary)
            .args(["enroll", "--config"])
            .arg(path)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(recovered.status.success());
    assert!(
        String::from_utf8(recovered.stdout)
            .unwrap()
            .contains("recovered")
    );
    let mut running = Command::new(binary)
        .args(["run", "--config"])
        .arg(&config_path)
        .spawn()
        .unwrap();
    let api = remotevnc_device::DeviceApi::new(&base, device_key(1)).unwrap();
    let token = AccessToken {
        token: f.tokens[0].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if api
            .presence(&token)
            .await
            .unwrap()
            .iter()
            .any(|p| Some(p.device_id) == candidate.device_id && p.online)
        {
            break;
        }
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let path = config_path.clone();
    let revoked = tokio::task::spawn_blocking(move || {
        Command::new(binary)
            .args(["revoke-local", "--config"])
            .arg(path)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        revoked.status.success(),
        "{}",
        String::from_utf8_lossy(&revoked.stderr)
    );
    assert!(running.wait().unwrap().success());
    assert!(config.revoked_path().exists());
    assert!(!identity.join("identity.json").exists());
    assert!(f.devices.challenge(candidate.device_id.unwrap()).is_err());
    assert!(
        !f.devices
            .list(&f.tokens[0])
            .unwrap()
            .iter()
            .any(|d| Some(d.id) == candidate.device_id)
    );
    let output = Command::new(binary)
        .args(["run", "--config"])
        .arg(&config_path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    backend.abort();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicit root disposable Ubuntu systemd fixture; never run on developer host"]
async fn installed_agent_unit_runs_as_state_owner_and_revoke_stops_ram_identity() {
    assert_eq!(
        std::env::var("REMOTEVNC_SYSTEMD_FIXTURE").as_deref(),
        Ok("disposable-lima")
    );
    assert_eq!(unsafe { libc::geteuid() }, 0);
    use std::process::Command;
    let f = session_fixture();
    let issuer = Issuer::start().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = BackendState {
        config: config(),
        auth: issuer.auth(f.db.clone()),
        devices: f.devices.clone(),
        sessions: SessionService::new(f.db.clone(), TurnClient::unavailable()),
    };
    let backend = tokio::spawn(async move {
        axum::serve(listener, remotevnc_backend::router(state))
            .await
            .unwrap()
    });
    let state_path = std::path::Path::new("/var/lib/remotevnc-agent");
    std::fs::create_dir_all(state_path).unwrap();
    let uid = String::from_utf8(
        Command::new("id")
            .args(["-u", "remotevnc-agent"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .parse::<u32>()
    .unwrap();
    // Seed a fixture identity under root, then transfer to the actual service owner before use.
    Command::new("chown")
        .args(["root:root", "/var/lib/remotevnc-agent"])
        .status()
        .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(state_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _ = std::fs::remove_file(state_path.join("identity.json"));
    let _ = std::fs::remove_file(state_path.join("revoked"));
    let _ = std::fs::remove_file(state_path.join("agent.lock"));
    FileStore::new(state_path)
        .save(&StoredCredentials {
            device_id: Some(f.ids[1]),
            signing_key: device_key(2),
        })
        .unwrap();
    assert!(
        Command::new("chown")
            .args([
                "-R",
                "remotevnc-agent:remotevnc-agent",
                "/var/lib/remotevnc-agent"
            ])
            .status()
            .unwrap()
            .success()
    );
    let config = Config {
        backend_origin: base.clone(),
        identity_path: state_path.into(),
        helper_socket: "/run/remotevnc/helper.sock".into(),
        seat: "seat0".into(),
        relay_only: false,
    };
    std::fs::create_dir_all("/etc/remotevnc").unwrap();
    std::fs::write(
        "/etc/remotevnc/agent.json",
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    assert!(
        Command::new("systemctl")
            .args(["start", "remotevnc-agent.service"])
            .status()
            .unwrap()
            .success()
    );
    let api = remotevnc_device::DeviceApi::new(&base, device_key(1)).unwrap();
    let token = AccessToken {
        token: f.tokens[0].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let until = tokio::time::Instant::now() + Duration::from_secs(8);
    while !api
        .presence(&token)
        .await
        .unwrap()
        .iter()
        .any(|p| p.device_id == f.ids[1] && p.online)
    {
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let pid = String::from_utf8(
        Command::new("systemctl")
            .args([
                "show",
                "remotevnc-agent.service",
                "--property=MainPID",
                "--value",
            ])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let status = std::fs::read_to_string(format!("/proc/{}/status", pid.trim())).unwrap();
    assert!(status.lines().any(
        |l| l.starts_with("Uid:") && l.split_whitespace().skip(1).all(|v| v == uid.to_string())
    ));
    let output = tokio::task::spawn_blocking(|| {
        Command::new("runuser")
            .args([
                "-u",
                "remotevnc-agent",
                "--",
                "/usr/bin/remotevnc-agent",
                "revoke-local",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(config.revoked_path().exists());
    assert!(!state_path.join("identity.json").exists());
    assert!(f.devices.challenge(f.ids[1]).is_err());
    assert!(!std::path::Path::new(&format!("/proc/{}", pid.trim())).exists());
    println!(
        "actual agent unit: UID={uid}, PID={} authenticated; owner CLI stopped running RAM identity and revoked backend",
        pid.trim()
    );
    backend.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renewal_denial_closes_locally_without_any_backend_close_event() {
    let f = session_fixture();
    let issuer = Issuer::start().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = BackendState {
        config: config(),
        auth: issuer.auth(f.db.clone()),
        devices: f.devices.clone(),
        sessions: SessionService::new(f.db.clone(), TurnClient::unavailable()),
    };
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = count.clone();
    let app = remotevnc_backend::router(state).layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let seen = seen.clone();
            async move {
                if request.uri().path().ends_with("/renew")
                    && seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
                {
                    // Return an explicit denial without changing backend session/socket state.
                    axum::response::Response::builder()
                        .status(401)
                        .body(axum::body::Body::empty())
                        .unwrap()
                } else {
                    next.run(request).await
                }
            }
        },
    ));
    let backend = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = private_dir();
    FileStore::new(dir.path())
        .save(&StoredCredentials {
            device_id: Some(f.ids[1]),
            signing_key: device_key(2),
        })
        .unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    let helper = std::thread::spawn(move || {
        use std::io::Read;
        let (mut peer, _) = listener.accept().unwrap();
        write_helper_output(&mut peer, &HelperOutput::Event(HelperEvent::Ready)).unwrap();
        connected_tx.send(()).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
        closed_tx.send(()).unwrap();
    });
    let config = Config {
        backend_origin: base.clone(),
        identity_path: dir.path().into(),
        helper_socket: socket,
        seat: "seat0".into(),
        relay_only: false,
    };
    let stop = CancellationToken::new();
    let run = tokio::spawn(AgentService::new(config).unwrap().run(stop.clone()));
    let api = remotevnc_device::DeviceApi::new(&base, device_key(1)).unwrap();
    let token = AccessToken {
        token: f.tokens[0].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let _connection = api.events(&token, DeviceRole::Client).await.unwrap();
    let until = tokio::time::Instant::now() + Duration::from_secs(5);
    while !api
        .presence(&token)
        .await
        .unwrap()
        .iter()
        .any(|p| p.device_id == f.ids[1] && p.online)
    {
        assert!(tokio::time::Instant::now() < until);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    api.create_session(&token, f.ids[1]).await.unwrap();
    connected_rx.await.unwrap();
    tokio::time::timeout(Duration::from_secs(18), closed_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(count.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    stop.cancel();
    run.await.unwrap().unwrap();
    helper.join().unwrap();
    backend.abort();
}
