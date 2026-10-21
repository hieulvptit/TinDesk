#[path = "../../apps/backend/tests/common/mod.rs"]
mod common;
use common::*;
use remotevnc_backend::{
    BackendState, db::Database, devices::DeviceService, sessions::SessionService, turn::TurnClient,
};
use remotevnc_device::{DeviceApi, DeviceIdentity};
use remotevnc_protocol::*;
use remotevnc_seat_helper::{
    ipc, recovery,
    seat::{DisplayTarget, ServerIdentity},
};
use remotevnc_transport::{Peer, PeerConfig, PeerEvent, PeerRole};
use std::{
    os::unix::net::UnixListener,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

pub fn run() -> remotevnc_agent::Result<()> {
    // Capture::start self-execs its helper; this is exclusively a test executable.
    if std::env::args().nth(1).as_deref() == Some("--capture-worker") {
        return remotevnc_seat_helper::capture::worker_main();
    }
    let Ok(role) = std::env::var("REMOTEVNC_E2E_ROLE") else {
        return Ok(());
    };
    let dir =
        PathBuf::from(std::env::var_os("REMOTEVNC_E2E_DIR").ok_or("fixture directory required")?);
    match role.as_str() {
        "helper" => helper(&dir),
        "guardian" => {
            let listener = UnixListener::bind(dir.join("guardian.sock"))?;
            for stream in listener.incoming() {
                if let Err(error) = recovery::serve(&mut stream?) {
                    eprintln!("guardian fixture: {error}");
                }
            }
            Ok(())
        }
        "backend" | "controller" | "unicode-controller" => {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?
                .block_on(async {
                    if role == "backend" {
                        backend(&dir).await
                    } else {
                        controller(&dir, role == "unicode-controller").await
                    }
                })
        }
        _ => Err("unknown explicit fixture role".into()),
    }
}
fn target() -> DisplayTarget {
    DisplayTarget {
        seat: "test-only".into(),
        display: std::env::var("DISPLAY").unwrap(),
        authority: std::env::var_os("XAUTHORITY").unwrap().into(),
        identity: ServerIdentity {
            process_start: "e2e-test-only".into(),
            socket_inode: 0,
            authority_inode: 0,
            authority_modified_ns: 0,
            connection_epoch: 0,
        },
        output: "screen".into(),
        origin_x: 0,
        origin_y: 0,
        width: 1920,
        height: 1080,
    }
}
fn helper(dir: &std::path::Path) -> remotevnc_agent::Result<()> {
    let listener = UnixListener::bind(dir.join("helper.sock"))?;
    let mut generation = 0;
    for stream in listener.incoming() {
        let mut peer = stream?;
        ipc::authorize_uid(ipc::peer_uid(&peer)?, unsafe { libc::geteuid() })?;
        peer.set_read_timeout(Some(Duration::from_millis(100)))?;
        peer.set_write_timeout(Some(Duration::from_millis(100)))?;
        let result = ipc::serve_with_guardian(
            &mut peer,
            &mut |_| Ok(Some(target())),
            &mut generation,
            || recovery::boot_time_ms().unwrap_or(u64::MAX),
            &AtomicBool::new(false),
            &dir.join("guardian.sock"),
        );
        if let Err(error) = result {
            eprintln!("native helper fixture: {error}");
        }
    }
    Ok(())
}
async fn backend(dir: &std::path::Path) -> remotevnc_agent::Result<()> {
    let started = Instant::now();
    eprintln!(
        "backend fixture bootstrap started; restart={}",
        dir.join("devices.json").exists()
    );
    let db = Database::open(dir.join("backend.sqlite"))?;
    eprintln!("backend fixture database ready: {:?}", started.elapsed());
    let issuer = Issuer::start().await;
    eprintln!(
        "backend fixture signed issuer ready (RSA preparation and discovery): {:?}",
        started.elapsed()
    );
    let auth = issuer.auth(db.clone());
    let devices = DeviceService::new(config(), db.clone());
    let manifest = dir.join("devices.json");
    if !manifest.exists() {
        let mut ids = Vec::new();
        // Real signed issuer, nonce, PKCE code exchange, confirmation and device proof.
        for (byte, subject) in [(1, "account-a"), (2, "account-a"), (3, "account-b")] {
            let key = device_key(byte);
            let e = begin(&devices, &key);
            confirm(&issuer, &auth, &e, subject).await;
            let EnrollmentStatus::Enrolled { device_id } =
                devices.poll_enrollment(&e.id, poll(&e, &key))?
            else {
                panic!("enrollment incomplete")
            };
            ids.push(device_id);
        }
        std::fs::write(manifest, serde_json::to_vec(&ids)?)?;
    }
    let sessions = SessionService::new(db, TurnClient::unavailable());
    sessions.expire_on_startup().await?;
    let address = std::fs::read_to_string(dir.join("address"))?;
    eprintln!(
        "backend fixture enrollment/session startup complete: {:?}",
        started.elapsed()
    );
    let listener = tokio::net::TcpListener::bind(address.trim()).await?;
    std::fs::write(dir.join("backend.ready"), b"ready")?;
    eprintln!("backend fixture listening: {:?}", started.elapsed());
    // Keep issuer alive for the lifetime of the actual HTTP/WebSocket backend process.
    axum::serve(
        listener,
        remotevnc_backend::router(BackendState {
            config: config(),
            auth,
            devices,
            sessions,
        }),
    )
    .await?;
    drop(issuer);
    Ok(())
}
async fn controller(dir: &std::path::Path, unicode: bool) -> remotevnc_agent::Result<()> {
    let ids: Vec<DeviceId> = serde_json::from_slice(&std::fs::read(dir.join("devices.json"))?)?;
    let base = format!(
        "http://{}",
        std::fs::read_to_string(dir.join("address"))?.trim()
    );
    let api = DeviceApi::new(&base, device_key(1))?;
    let token = api
        .authenticate(&DeviceIdentity {
            device_id: ids[0],
            signing_key: device_key(1),
        })
        .await?;
    let (mut sender, mut incoming) = api.events(&token, DeviceRole::Client).await?.split();
    let grant = api.create_session(&token, ids[1]).await?;
    std::fs::write(dir.join("session.json"), serde_json::to_vec(&grant)?)?;
    let mut peer = Peer::new(PeerConfig {
        session_id: grant.session_id,
        local_peer_id: ids[0],
        remote_peer_id: ids[1],
        role: PeerRole::Controller,
        display: None,
        ice_servers: grant.ice_servers,
        relay_only: false,
    })?;
    peer.enable_h264()?;
    let mut generation = None;
    let mut channels = false;
    let mut decoded = false;
    let mut sent = false;
    let timeout = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        tokio::select! {
            event = incoming.next() => match event? {
                Some(DeviceEvent::Signal { message, .. }) => {
                    if let SignalMessage::Display { display, .. } = &message { generation = Some(display.generation); }
                    let restart = matches!(message, SignalMessage::Restart { .. });
                    peer.handle_signal(message)?;
                    if restart { peer.start_negotiation()?; }
                }
                Some(DeviceEvent::SessionClosed { .. }) | None => { eprintln!("controller ended: channels={channels}, decoded={decoded}, sent={sent}"); return Ok(()); },
                _ => {}
            },
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        for event in peer.events().try_iter() {
            match event {
                PeerEvent::Signal(message) => {
                    assert!(
                        !matches!(message, SignalMessage::End { .. }),
                        "fixture never sends graceful End"
                    );
                    sender
                        .send(&DeviceCommand::Signal {
                            receiver: ids[1],
                            message,
                        })
                        .await?;
                }
                PeerEvent::ChannelsReady => channels = true,
                PeerEvent::Frame(frame) => {
                    decoded = true;
                    if let Some(pts) = frame.pts_ns {
                        std::fs::write(dir.join("controller.pts.next"), pts.to_string())?;
                        std::fs::rename(
                            dir.join("controller.pts.next"),
                            dir.join("controller.pts"),
                        )?;
                    }
                }
                PeerEvent::Error(e) => return Err(e.into()),
                _ => {}
            }
        }
        if channels && decoded && !sent {
            let mut events = vec![InputEvent::DisplayAck];
            if unicode {
                events.push(InputEvent::Text {
                    text: "Tiếng Việt|é|e\u{301}|中文|😀".into(),
                });
            }
            events.extend([
                InputEvent::Key {
                    keysym: 0xffe1,
                    pressed: true,
                },
                InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                    x: 20,
                    y: 20,
                },
            ]);
            for (sequence, event) in events.into_iter().enumerate() {
                peer.send_input(InputEnvelope {
                    version: WIRE_VERSION,
                    session_id: grant.session_id,
                    generation: generation.ok_or("no display")?,
                    sequence: sequence as u64,
                    event,
                })?;
            }
            std::fs::write(
                dir.join("controller.ready"),
                b"native decoded H264 and sent held input",
            )?;
            sent = true;
        }
        if !sent && tokio::time::Instant::now() >= timeout {
            return Err("native negotiation/decode timeout".into());
        }
    }
}
