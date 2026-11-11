#[path = "../../backend/tests/common/mod.rs"]
mod common;
use axum::{
    extract::{
        FromRequestParts, Request,
        ws::{CloseFrame, Message, WebSocketUpgrade, close_code},
    },
    http::{Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use binachicken_client::{
    app::{ClientCredentialStore, ClientRuntime, Config, ViewState},
    video::LatestFrame,
};
use binachicken_device::{CredentialStore, StoredCredentials};
use binachicken_protocol::*;
use common::*;
use remotevnc_backend::{
    BackendState, auth::AuthService, db::Database, devices::DeviceService,
    sessions::SessionService, turn::TurnClient,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

struct MemoryStore {
    record: Mutex<Option<StoredCredentials>>,
    saves: AtomicUsize,
    fail_save: AtomicUsize,
    fail_delete: AtomicBool,
    deleted: AtomicBool,
    view: Arc<Mutex<ViewState>>,
}
fn clone_credentials(c: &StoredCredentials) -> StoredCredentials {
    StoredCredentials {
        device_id: c.device_id,
        signing_key: c.signing_key.clone(),
    }
}
impl CredentialStore for MemoryStore {
    fn load(&self) -> binachicken_device::Result<Option<StoredCredentials>> {
        Ok(self.record.lock().unwrap().as_ref().map(clone_credentials))
    }
    fn save(&self, c: &StoredCredentials) -> binachicken_device::Result<()> {
        let n = self.saves.fetch_add(1, Ordering::SeqCst) + 1;
        if n == self.fail_save.load(Ordering::SeqCst) {
            return Err(binachicken_device::Error::Storage);
        }
        *self.record.lock().unwrap() = Some(clone_credentials(c));
        Ok(())
    }
}
impl ClientCredentialStore for MemoryStore {
    fn delete(&self) -> binachicken_device::Result<()> {
        let view = self.view.lock().unwrap();
        assert!(
            !view.connected && !view.ready && !view.signed_in,
            "worker/Peer stop precedes persistence deletion"
        );
        if self.fail_delete.load(Ordering::SeqCst) {
            return Err(binachicken_device::Error::Storage);
        }
        self.record.lock().unwrap().take();
        self.deleted.store(true, Ordering::SeqCst);
        Ok(())
    }
}
fn store(record: Option<StoredCredentials>) -> Arc<MemoryStore> {
    Arc::new(MemoryStore {
        record: Mutex::new(record),
        saves: AtomicUsize::new(0),
        fail_save: AtomicUsize::new(0),
        fail_delete: AtomicBool::new(false),
        deleted: AtomicBool::new(false),
        view: Arc::new(Mutex::new(ViewState::default())),
    })
}
#[derive(Default)]
struct Control {
    begins: AtomicUsize,
    polls: AtomicUsize,
    auths: AtomicUsize,
    revokes: AtomicUsize,
    deny_challenge: AtomicUsize,
    revoke_unavailable: AtomicBool,
    rotate: AtomicBool,
    offline_events: AtomicBool,
    policy_close_once: AtomicBool,
}
struct Server {
    base: String,
    auth: AuthService,
    devices: DeviceService,
    task: tokio::task::JoinHandle<()>,
    control: Arc<Control>,
    _issuer: Issuer,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(db: Database, store: Arc<MemoryStore>) -> Server {
    let issuer = Issuer::start().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut config = config();
    config.public_origin = url::Url::parse(&base).unwrap();
    let devices = DeviceService::new(config.clone(), db.clone());
    let auth = issuer.auth(db.clone());
    let control = Arc::new(Control::default());
    let state = BackendState {
        config,
        auth: auth.clone(),
        devices: devices.clone(),
        sessions: SessionService::new(db, TurnClient::unavailable()),
    };
    let checks = control.clone();
    let app = remotevnc_backend::router(state).layer(middleware::from_fn(
        move |request: Request, next: Next| {
            let store = store.clone();
            let checks = checks.clone();
            async move {
                let path = request.uri().path().to_string();
                if path == "/v1/events" && checks.policy_close_once.swap(false, Ordering::SeqCst) {
                    let (mut parts, _) = request.into_parts();
                    let ws = WebSocketUpgrade::from_request_parts(&mut parts, &())
                        .await
                        .unwrap();
                    return ws
                        .on_upgrade(|mut socket| async move {
                            let _ = socket
                                .send(Message::Close(Some(CloseFrame {
                                    code: close_code::POLICY,
                                    reason: "fixture session policy failure".into(),
                                })))
                                .await;
                        })
                        .into_response();
                }
                if path == "/v1/events" && checks.offline_events.load(Ordering::SeqCst) {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                if path == "/v1/enrollments" {
                    checks.begins.fetch_add(1, Ordering::SeqCst);
                    assert!(
                        store.record.lock().unwrap().is_some(),
                        "provisional key must be persisted before begin"
                    );
                }
                if path.ends_with("/poll") {
                    checks.polls.fetch_add(1, Ordering::SeqCst);
                    assert!(
                        store
                            .record
                            .lock()
                            .unwrap()
                            .as_ref()
                            .unwrap()
                            .device_id
                            .is_some(),
                        "candidate must be persisted before first poll"
                    );
                }
                if path.ends_with("/challenge") {
                    match checks.deny_challenge.load(Ordering::SeqCst) {
                        401 => return StatusCode::UNAUTHORIZED.into_response(),
                        503 => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
                        _ => {}
                    }
                }
                if request.method() == Method::DELETE
                    && path.starts_with("/v1/devices/")
                    && store
                        .record
                        .lock()
                        .unwrap()
                        .as_ref()
                        .and_then(|c| c.device_id)
                        .is_some_and(|id| path.ends_with(&id.0.to_string()))
                {
                    checks.revokes.fetch_add(1, Ordering::SeqCst);
                    assert!(
                        !store.view.lock().unwrap().connected,
                        "native delivery stops before HTTP revoke"
                    );
                    assert!(
                        !store.deleted.load(Ordering::SeqCst),
                        "persistent key is deleted after revoke attempt"
                    );
                    if checks.revoke_unavailable.load(Ordering::SeqCst) {
                        return StatusCode::SERVICE_UNAVAILABLE.into_response();
                    }
                }
                let response = next.run(request).await;
                if path == "/v1/devices/authenticate" && response.status().is_success() {
                    checks.auths.fetch_add(1, Ordering::SeqCst);
                    if checks.rotate.load(Ordering::SeqCst) {
                        let (mut parts, body) = response.into_parts();
                        let bytes = axum::body::to_bytes(body, 65536).await.unwrap();
                        let mut token: AccessToken = serde_json::from_slice(&bytes).unwrap();
                        token.expires_at_unix_ms = unix_ms() + 31_000;
                        parts.headers.remove("content-length");
                        return Response::from_parts(
                            parts,
                            axum::body::Body::from(serde_json::to_vec(&token).unwrap()),
                        );
                    }
                }
                response
            }
        },
    ));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Server {
        base,
        auth,
        devices,
        task,
        control,
        _issuer: issuer,
    }
}
async fn wait_for(mut f: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(8);
    while !f() {
        assert!(Instant::now() < until, "fixture state timed out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
fn run(server: &Server, store: &Arc<MemoryStore>) -> ClientRuntime {
    ClientRuntime::start_with_store(
        Config::new(&server.base).unwrap(),
        store.view.clone(),
        LatestFrame::default(),
        store.clone(),
    )
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saturated_ui_commands_close_remote_input_without_signing_out() {
    let f = session_fixture();
    let store = store(Some(StoredCredentials {
        device_id: Some(f.ids[0]),
        signing_key: device_key(1),
    }));
    let server = server(f.db.clone(), store.clone()).await;
    let runtime = run(&server, &store);
    wait_for(|| store.view.lock().unwrap().status == "Choose an online Ubuntu agent").await;
    // A busy native/media worker must not turn bounded UI backpressure into logout.
    let mut rejected = false;
    for _ in 0..10_000 {
        if !runtime.connect(f.ids[1]) {
            rejected = true;
            break;
        }
    }
    assert!(rejected, "fixture must actually saturate the command queue");
    wait_for(|| {
        let view = store.view.lock().unwrap();
        runtime.is_finished()
            || (view.status.contains("queue overloaded")
                && !view.connected
                && !view.ready
                && !view.connection_requested)
    })
    .await;
    assert!(
        !runtime.is_finished(),
        "UI backpressure stopped authentication"
    );
    let view = store.view.lock().unwrap();
    assert!(view.signed_in);
    assert!(!view.connected && !view.ready && !view.connection_requested);
    assert!(!store.deleted.load(Ordering::SeqCst));
    assert_eq!(store.saves.load(Ordering::SeqCst), 0);
    drop(view);
    drop(runtime);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signaling_policy_close_rechecks_identity_without_signing_out_valid_device() {
    let f = session_fixture();
    let store = store(Some(StoredCredentials {
        device_id: Some(f.ids[0]),
        signing_key: device_key(1),
    }));
    let server = server(f.db.clone(), store.clone()).await;
    server
        .control
        .policy_close_once
        .store(true, Ordering::SeqCst);
    let runtime = run(&server, &store);
    wait_for(|| runtime.is_finished() || server.control.auths.load(Ordering::SeqCst) >= 3).await;
    assert!(
        !runtime.is_finished(),
        "session policy closure was treated as account revocation"
    );
    wait_for(|| store.view.lock().unwrap().status == "Choose an online Ubuntu agent").await;
    assert!(store.view.lock().unwrap().signed_in);
    assert_eq!(store.saves.load(Ordering::SeqCst), 0);
    assert!(!store.deleted.load(Ordering::SeqCst));
    drop(runtime);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persists_candidate_before_poll_and_recovers_interrupted_final_save_with_signed_authentication()
 {
    let dir = private_dir();
    let db = Database::open(dir.path().join("db")).unwrap();
    let store = store(None);
    store.fail_save.store(3, Ordering::SeqCst);
    let server = server(db, store.clone()).await;
    let runtime = run(&server, &store);
    wait_for(|| store.view.lock().unwrap().enrollment.is_some()).await;
    let (display_code, url) = store.view.lock().unwrap().enrollment.clone().unwrap();
    let id = url::Url::parse(&url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "enrollment")
        .unwrap()
        .1
        .into_owned();
    let (code, state) = server._issuer.issue(
        &server.auth.start_login(&id).unwrap(),
        "fixture-owner",
        Fault::None,
    );
    let login = server.auth.finish_login(&code, &state).await.unwrap();
    server
        .auth
        .confirm(&login.confirmation, &display_code)
        .unwrap();
    wait_for(|| runtime.is_finished()).await;
    drop(runtime);
    assert!(store.view.lock().unwrap().status.contains("storage"));
    assert!(server.control.polls.load(Ordering::SeqCst) > 0);
    let candidate = store
        .record
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .device_id
        .unwrap();
    assert!(
        server.devices.challenge(candidate).is_ok(),
        "backend enrolled before final local save failed"
    );
    let runtime = run(&server, &store);
    wait_for(|| store.view.lock().unwrap().status == "Choose an online Ubuntu agent").await;
    assert_eq!(
        server.control.begins.load(Ordering::SeqCst),
        1,
        "candidate signed authentication recovers without new enrollment"
    );
    let status = tokio::task::spawn_blocking(move || runtime.sign_out())
        .await
        .unwrap()
        .unwrap();
    assert!(status.contains("registration revoked"));
    assert_eq!(server.control.revokes.load(Ordering::SeqCst), 1);
    assert!(store.record.lock().unwrap().is_none());
    assert!(server.devices.challenge(candidate).is_err());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_signout_reports_unconfirmed_revocation_and_deletion_failure_without_fallback() {
    for fail_delete in [false, true] {
        let f = session_fixture();
        let store = store(Some(StoredCredentials {
            device_id: Some(f.ids[0]),
            signing_key: device_key(1),
        }));
        let server = server(f.db.clone(), store.clone()).await;
        server
            .control
            .revoke_unavailable
            .store(true, Ordering::SeqCst);
        store.fail_delete.store(fail_delete, Ordering::SeqCst);
        let runtime = run(&server, &store);
        wait_for(|| store.view.lock().unwrap().status == "Choose an online Ubuntu agent").await;
        let result = tokio::task::spawn_blocking(move || runtime.sign_out())
            .await
            .unwrap();
        if fail_delete {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Keychain removal failed")
            );
            assert!(store.record.lock().unwrap().is_some());
        } else {
            assert!(result.unwrap().contains("not confirmed"));
            assert!(store.record.lock().unwrap().is_none());
        }
        assert_eq!(server.control.revokes.load(Ordering::SeqCst), 1);
        assert!(!store.view.lock().unwrap().signed_in);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authorization_denial_and_network_failure_preserve_candidate_and_are_distinct() {
    for code in [401, 503] {
        let f = session_fixture();
        let store = store(Some(StoredCredentials {
            device_id: Some(f.ids[0]),
            signing_key: device_key(1),
        }));
        let server = server(f.db.clone(), store.clone()).await;
        server.control.deny_challenge.store(code, Ordering::SeqCst);
        let runtime = run(&server, &store);
        wait_for(|| runtime.is_finished()).await;
        drop(runtime);
        assert_eq!(store.saves.load(Ordering::SeqCst), 0);
        assert!(!store.deleted.load(Ordering::SeqCst));
        let status = store.view.lock().unwrap().status.clone();
        assert!(status.contains(if code == 401 {
            "authorization"
        } else {
            "request failed"
        }));
    }
}

fn encoded() -> EncodedFrame {
    use gst::prelude::*;
    gst::init().unwrap();
    let pipeline=gst::parse::launch("videotestsrc num-buffers=1 pattern=blue ! video/x-raw,width=320,height=240,framerate=30/1 ! videoconvert ! x264enc tune=zerolatency speed-preset=ultrafast bframes=0 key-int-max=30 ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=sink").unwrap().downcast::<gst::Pipeline>().unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let sample: Option<gst::Sample> = pipeline
        .by_name("sink")
        .unwrap()
        .emit_by_name("try-pull-sample", &[&5_000_000_000u64]);
    let sample = sample.unwrap();
    let buffer = sample.buffer().unwrap();
    let frame = EncodedFrame {
        generation: 1,
        pts_ns: 0,
        keyframe: true,
        data: buffer.map_readable().unwrap().as_slice().to_vec(),
    };
    pipeline.set_state(gst::State::Null).unwrap();
    frame
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rotating_signed_sockets_keep_native_video_then_lost_restart_has_bounded_fallback() {
    use binachicken_device::DeviceApi;
    use binachicken_transport::{Peer, PeerConfig, PeerEvent, PeerRole};
    let f = session_fixture();
    let store = store(Some(StoredCredentials {
        device_id: Some(f.ids[0]),
        signing_key: device_key(1),
    }));
    let server = server(f.db.clone(), store.clone()).await;
    server.control.rotate.store(true, Ordering::SeqCst);
    let agent_api = DeviceApi::new(&server.base, device_key(2)).unwrap();
    let agent_token = AccessToken {
        token: f.tokens[1].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let (mut sender, mut incoming) = agent_api
        .events(&agent_token, DeviceRole::Agent)
        .await
        .unwrap()
        .split();
    let frames = LatestFrame::default();
    let runtime = ClientRuntime::start_with_store(
        Config::new(&server.base).unwrap(),
        store.view.clone(),
        frames.clone(),
        store.clone(),
    )
    .unwrap();
    wait_for(|| store.view.lock().unwrap().status == "Choose an online Ubuntu agent").await;
    assert!(runtime.connect(f.ids[1]));
    let mut peer: Option<Peer> = None;
    let mut grant: Option<SessionGrant> = None;
    let mut dropped_at = None;
    let mut video = encoded();
    let mut last_video = Instant::now();
    let mut last_renew = Instant::now();
    let mut decoded = 0;
    let mut renewals = 0;
    let until = Instant::now() + Duration::from_secs(35);
    loop {
        tokio::select! {
            event=incoming.next()=>match event.unwrap().unwrap() {
                DeviceEvent::Session{grant:g} if grant.is_none()=>{
                    let p=Peer::new(PeerConfig{session_id:g.session_id,local_peer_id:g.agent_id,remote_peer_id:g.client_id,role:PeerRole::Agent,display:None,ice_servers:g.ice_servers.clone(),relay_only:false}).unwrap();
                    p.enable_h264().unwrap();p.update_display(DisplayInfo{generation:1,width:640,height:480}).unwrap();peer=Some(p);grant=Some(g);
                },
                DeviceEvent::Signal{message,..}=>{if let Some(peer)=&peer{peer.handle_signal(message).unwrap();}},
                DeviceEvent::SessionClosed{..}=>{assert!(dropped_at.is_some(),"healthy rotation must keep the session");break},
                _=>{},
            },
            _=tokio::time::sleep(Duration::from_millis(10))=>{},
        }
        if let Some(peer) = peer.as_mut() {
            for event in peer.events().try_iter().collect::<Vec<_>>() {
                if let PeerEvent::Signal(message) = event {
                    // Model an accepted control-plane message lost across socket rotation.
                    if dropped_at.is_none() {
                        sender
                            .send(&DeviceCommand::Signal {
                                receiver: f.ids[0],
                                message,
                            })
                            .await
                            .unwrap();
                    }
                }
            }
            if dropped_at.is_none() && last_video.elapsed() > Duration::from_millis(100) {
                video.pts_ns += 100_000_000;
                let _ = peer.push_video(video.clone());
                last_video = Instant::now();
            }
            if let Some(frame) = frames.take() {
                assert_eq!((frame.width, frame.height), (320, 240));
                decoded += 1;
            }
            if dropped_at.is_none()
                && decoded >= 5
                && server.control.auths.load(Ordering::SeqCst) >= 5
            {
                assert!(store.view.lock().unwrap().ready);
                assert_eq!(store.view.lock().unwrap().epoch, 1);
                peer.update_display(DisplayInfo {
                    generation: 2,
                    width: 800,
                    height: 600,
                })
                .unwrap();
                dropped_at = Some(Instant::now());
            }
        }
        if dropped_at.is_some() && last_renew.elapsed() > Duration::from_secs(3) {
            if let Some(grant) = &grant
                && agent_api
                    .renew_session(&agent_token, grant.session_id)
                    .await
                    .is_ok()
            {
                renewals += 1;
            }
            last_renew = Instant::now();
        }
        if let Some(dropped) = dropped_at
            && !store.view.lock().unwrap().connected
        {
            assert!(dropped.elapsed() < Duration::from_secs(23));
            break;
        }
        assert!(
            Instant::now() < until,
            "rotation/reconciliation did not settle: {}",
            store.view.lock().unwrap().status
        );
    }
    assert!(decoded >= 5);
    assert!(server.control.auths.load(Ordering::SeqCst) >= 5);
    assert!(
        renewals > 0,
        "fresh grants must not extend incoherent negotiation"
    );
    tokio::task::spawn_blocking(move || drop(runtime))
        .await
        .unwrap();
    assert!(
        store.record.lock().unwrap().is_some(),
        "ordinary window close preserves enrollment"
    );
    assert_eq!(server.control.revokes.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_connect_recovers_fresh_video_and_ack_then_disconnect_cancels_intent() {
    recover_then_cancel(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revocation_cancels_recovery_intent() {
    recover_then_cancel(true).await;
}
async fn recover_then_cancel(revoke: bool) {
    use binachicken_device::DeviceApi;
    use binachicken_transport::{Peer, PeerConfig, PeerEvent, PeerRole};
    let f = session_fixture();
    let store = store(Some(StoredCredentials {
        device_id: Some(f.ids[0]),
        signing_key: device_key(1),
    }));
    let server = server(f.db.clone(), store.clone()).await;
    server.control.rotate.store(true, Ordering::SeqCst);
    let api = DeviceApi::new(&server.base, device_key(2)).unwrap();
    let token = AccessToken {
        token: f.tokens[1].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let (mut sender, mut incoming) = api.events(&token, DeviceRole::Agent).await.unwrap().split();
    let frames = LatestFrame::default();
    let runtime = ClientRuntime::start_with_store(
        Config::new(&server.base).unwrap(),
        store.view.clone(),
        frames.clone(),
        store.clone(),
    )
    .unwrap();
    wait_for(|| store.view.lock().unwrap().status == "Choose an online Ubuntu agent").await;
    let requested_profile = VideoProfile {
        width: 1280,
        height: 720,
        fps: 24,
        ..VideoProfile::default()
    };
    assert!(runtime.set_video_profile(requested_profile));
    runtime.connect(f.ids[1]); // The only Connect command in this recovery exercise.
    let mut peer: Option<Peer> = None;
    let mut sessions = Vec::new();
    let mut ack = false;
    let mut profile_received = false;
    let mut profile_count = 0;
    let mut changed_profile = false;
    let updated_profile = VideoProfile {
        fps: 15,
        ..requested_profile
    };
    let mut decoded = 0;
    let mut video = encoded();
    let mut last_video = Instant::now();
    let until = Instant::now() + Duration::from_secs(25);
    loop {
        tokio::select! {
            event=incoming.next()=>match event.unwrap().unwrap() {
                DeviceEvent::Session { grant } if sessions.last()!=Some(&grant.session_id)=>{
                    assert!(!sessions.contains(&grant.session_id),"old authority revived");
                    let p=Peer::new(PeerConfig { session_id:grant.session_id,local_peer_id:grant.agent_id,remote_peer_id:grant.client_id,role:PeerRole::Agent,display:None,ice_servers:grant.ice_servers,relay_only:false }).unwrap();
                    p.enable_h264().unwrap(); p.update_display(DisplayInfo{generation:1,width:640,height:480}).unwrap();
                    sessions.push(grant.session_id); peer=Some(p); ack=false; decoded=0; profile_received=false; profile_count=0;
                },
                DeviceEvent::Signal { message,.. }=>{if let Some(p)=&peer {let _=p.handle_signal(message);}},
                _=>{}
            },
            _=tokio::time::sleep(Duration::from_millis(5))=>{}
        }
        if let Some(p) = &mut peer {
            for event in p.events().try_iter() {
                match event {
                    PeerEvent::Signal(message) => sender
                        .send(&DeviceCommand::Signal {
                            receiver: f.ids[0],
                            message,
                        })
                        .await
                        .unwrap(),
                    PeerEvent::Input(input)
                        if matches!(input.event, InputEvent::DisplayAck) && !ack =>
                    {
                        assert_eq!(
                            input.session_id,
                            *sessions.last().unwrap(),
                            "fresh input scope"
                        );
                        ack = true;
                    }
                    PeerEvent::Input(InputEnvelope {
                        event: InputEvent::VideoProfile { profile },
                        ..
                    }) => {
                        assert!(
                            ack,
                            "video settings must follow physical display acknowledgement"
                        );
                        assert_eq!(
                            profile,
                            if changed_profile {
                                updated_profile
                            } else {
                                requested_profile
                            }
                        );
                        profile_received = true;
                        profile_count += 1;
                    }
                    _ => {}
                }
            }
            if last_video.elapsed() > Duration::from_millis(100) {
                video.pts_ns += 100_000_000;
                let _ = p.push_video(video.clone());
                last_video = Instant::now();
            }
        }
        if frames.take().is_some() {
            decoded += 1;
        }
        if ack && profile_received && decoded >= 3 {
            if sessions.len() == 1 {
                assert_eq!(
                    profile_count, 1,
                    "readiness must not repeatedly reset adaptation"
                );
                // Remove the old session while the runtime cannot receive its
                // close event. Reconnection must reconcile the missing grant
                // then create fresh authority without another Connect command.
                server.control.offline_events.store(true, Ordering::SeqCst);
                wait_for(|| store.view.lock().unwrap().presence.is_empty()).await;
                api.close_session(&token, sessions[0]).await.unwrap();
                server.control.offline_events.store(false, Ordering::SeqCst);
                peer.take();
                decoded = 0;
                ack = false;
            } else if !changed_profile {
                assert_eq!(profile_count, 1);
                assert!(runtime.set_video_profile(updated_profile));
                changed_profile = true;
                profile_received = false;
                decoded = 0;
            } else {
                assert_eq!(
                    profile_count, 2,
                    "one manual change must produce one update"
                );
                break;
            }
        }
        assert!(
            Instant::now() < until,
            "fresh controller session/video/ACK missing after loss: {}",
            store.view.lock().unwrap().status
        );
    }
    assert_eq!(sessions.len(), 2);
    if revoke {
        api.revoke_device(&token, f.ids[1]).await.unwrap();
    } else {
        runtime.disconnect();
    }
    wait_for(|| {
        let s = store.view.lock().unwrap();
        !s.connected && !s.connection_requested
    })
    .await;
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        tokio::select! {event=incoming.next()=>{if let Ok(Some(DeviceEvent::Session{grant}))=event {assert!(sessions.contains(&grant.session_id),"Disconnect recreated authority");}},_=tokio::time::sleep(Duration::from_millis(10))=>{}}
    }
    tokio::task::spawn_blocking(move || drop(runtime))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_outage_invalidates_missed_presence_and_role_change() {
    use binachicken_device::DeviceApi;
    let f = session_fixture();
    let store = store(Some(StoredCredentials {
        device_id: Some(f.ids[0]),
        signing_key: device_key(1),
    }));
    let server = server(f.db.clone(), store.clone()).await;
    server.control.rotate.store(true, Ordering::SeqCst);
    let api = DeviceApi::new(&server.base, device_key(2)).unwrap();
    let token = AccessToken {
        token: f.tokens[1].clone(),
        expires_at_unix_ms: unix_ms() + 300_000,
    };
    let agent = api.events(&token, DeviceRole::Agent).await.unwrap();
    let runtime = run(&server, &store);
    wait_for(|| {
        store
            .view
            .lock()
            .unwrap()
            .presence
            .iter()
            .any(|p| p.device_id == f.ids[1] && p.online)
    })
    .await;
    server.control.offline_events.store(true, Ordering::SeqCst);
    wait_for(|| store.view.lock().unwrap().presence.is_empty()).await;
    drop(agent); // Offline announcement is missed during this socket outage.
    tokio::time::sleep(Duration::from_millis(100)).await;
    server.control.offline_events.store(false, Ordering::SeqCst);
    wait_for(|| store.view.lock().unwrap().status == "Choose an online Ubuntu agent").await;
    assert!(
        !store
            .view
            .lock()
            .unwrap()
            .presence
            .iter()
            .any(|p| p.device_id == f.ids[1] && p.online)
    );
    let agent = api.events(&token, DeviceRole::Agent).await.unwrap();
    wait_for(|| {
        store
            .view
            .lock()
            .unwrap()
            .presence
            .iter()
            .any(|p| p.device_id == f.ids[1] && p.role == DeviceRole::Agent && p.online)
    })
    .await;
    drop(agent);
    let _client = api.events(&token, DeviceRole::Client).await.unwrap();
    wait_for(|| {
        store
            .view
            .lock()
            .unwrap()
            .presence
            .iter()
            .any(|p| p.device_id == f.ids[1] && p.role == DeviceRole::Client && p.online)
    })
    .await;
    assert!(
        !store
            .view
            .lock()
            .unwrap()
            .presence
            .iter()
            .any(|p| p.device_id == f.ids[1] && p.role == DeviceRole::Agent && p.online)
    );
    tokio::task::spawn_blocking(move || drop(runtime))
        .await
        .unwrap();
}
