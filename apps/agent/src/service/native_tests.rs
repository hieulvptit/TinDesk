//! Explicit disposable-Xvfb tests of private Live policy with real native peers.
use super::*;
use std::{
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, KeyButMask},
};

struct Fixtures {
    dir: tempfile::TempDir,
    children: Vec<Child>,
}
impl Fixtures {
    async fn start() -> Self {
        assert_eq!(std::env::var("REMOTEVNC_E2E").as_deref(), Ok("1"));
        let dir = tempfile::tempdir().unwrap();
        let mut fixture = Self {
            dir,
            children: vec![],
        };
        for role in ["guardian", "helper"] {
            let log =
                std::fs::File::create(fixture.dir.path().join(format!("{role}.log"))).unwrap();
            fixture.children.push(
                Command::new(std::env::var_os("REMOTEVNC_E2E_FIXTURE").unwrap())
                    .env("REMOTEVNC_E2E_ROLE", role)
                    .env("REMOTEVNC_E2E_DIR", fixture.dir.path())
                    .process_group(0)
                    .stdout(Stdio::from(log.try_clone().unwrap()))
                    .stderr(Stdio::from(log))
                    .spawn()
                    .unwrap(),
            );
            let until = Instant::now() + Duration::from_secs(3);
            while !fixture.dir.path().join(format!("{role}.sock")).exists() {
                assert!(Instant::now() < until, "fixture {role} did not listen");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
        fixture
    }
}
impl Drop for Fixtures {
    fn drop(&mut self) {
        // No child is reaped before its private group is signaled; IDs cannot be reused.
        for child in self.children.iter_mut().rev() {
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
        }
        if let Some(results) = std::env::var_os("REMOTEVNC_E2E_RESULTS") {
            for role in ["guardian", "helper"] {
                let _ = std::fs::copy(
                    self.dir.path().join(format!("{role}.log")),
                    Path::new(&results).join(format!(
                        "transport-{}-{role}.log",
                        self.dir.path().file_name().unwrap().to_string_lossy()
                    )),
                );
            }
        }
    }
}
struct Session {
    // Drop Live before its native controller and helper/guardian processes.
    live: Option<Live>,
    controller: Peer,
    tx: mpsc::Sender<DeviceCommand>,
    rx: mpsc::Receiver<DeviceCommand>,
    x: x11rb::rust_connection::RustConnection,
    channels: bool,
    pts: Option<u64>,
    dimensions: (u32, u32),
    sequence: u64,
    _fixtures: Fixtures,
}
impl Session {
    async fn start() -> Self {
        let fixtures = Fixtures::start().await;
        let grant = SessionGrant {
            session_id: SessionId::from_uuid(uuid::Uuid::new_v4()),
            client_id: DeviceId::from_uuid(uuid::Uuid::new_v4()),
            agent_id: DeviceId::from_uuid(uuid::Uuid::new_v4()),
            lease_expires_at: unix_ms() + 60_000,
            ice_servers: vec![],
            turn_status: TurnStatus::Unavailable,
        };
        let controller = Peer::new(PeerConfig {
            session_id: grant.session_id,
            local_peer_id: grant.client_id,
            remote_peer_id: grant.agent_id,
            role: PeerRole::Controller,
            display: None,
            ice_servers: vec![],
            relay_only: false,
        })
        .unwrap();
        controller.enable_h264().unwrap();
        let mut live = Live::new(
            &Config {
                backend_origin: "http://127.0.0.1".into(),
                identity_path: fixtures.dir.path().into(),
                helper_socket: fixtures.dir.path().join("helper.sock"),
                seat: "seat0".into(),
                relay_only: false,
            },
            grant,
            boot_time_ms(),
            CancellationToken::new(),
        )
        .unwrap();
        // Isolate downstream saturation: the separate helper queue must never overflow.
        let profile = VideoProfile {
            width: 320,
            height: 180,
            fps: 10,
            bitrate_kbps: 512,
            encoder: VideoEncoder::X264,
        };
        profile.validate().unwrap();
        live.profile = profile;
        live.adaptation = remotevnc_transport::stats::AdaptiveProfile::new(profile).unwrap();
        let (tx, rx) = mpsc::channel(32);
        let mut session = Self {
            live: Some(live),
            controller,
            tx,
            rx,
            x: x11rb::connect(None).unwrap().0,
            channels: false,
            pts: None,
            dimensions: (0, 0),
            sequence: 0,
            _fixtures: fixtures,
        };
        assert_eq!(session.held(), (false, false));
        let until = Instant::now() + Duration::from_secs(20);
        let mut sent = false;
        loop {
            session.tick().unwrap();
            if session.channels && session.pts.is_some() && !sent {
                session.input(InputEvent::DisplayAck);
                session.input(InputEvent::Key {
                    keysym: 0xffe1,
                    pressed: true,
                });
                session.input(InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                    x: 20,
                    y: 20,
                });
                sent = true;
            }
            if session.live.as_ref().unwrap().acknowledged && session.held() == (true, true) {
                break;
            }
            assert!(
                Instant::now() < until,
                "real native channels/frame/DisplayAck/held input unavailable"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(session.live.as_ref().unwrap().negotiation_until.is_none());
        session
    }
    fn input(&mut self, event: InputEvent) {
        let live = self.live.as_ref().unwrap();
        self.controller
            .send_input(InputEnvelope {
                version: WIRE_VERSION,
                session_id: live.grant.session_id,
                generation: live.display.unwrap().generation,
                sequence: self.sequence,
                event,
            })
            .unwrap();
        self.sequence += 1;
    }
    fn tick(&mut self) -> Result<()> {
        let live = self.live.as_mut().unwrap();
        live.poll(&self.tx)?;
        while let Ok(DeviceCommand::Signal { message, .. }) = self.rx.try_recv() {
            let restart = matches!(message, SignalMessage::Restart { .. });
            self.controller.handle_signal(message)?;
            if restart {
                self.controller.start_negotiation()?;
            }
        }
        for event in self.controller.events().try_iter() {
            match event {
                PeerEvent::Signal(message) => live.signal(live.grant.client_id, message)?,
                PeerEvent::ChannelsReady => self.channels = true,
                PeerEvent::Frame(frame) => {
                    assert_eq!(frame.generation, live.display.unwrap().generation);
                    self.dimensions = (frame.width, frame.height);
                    if let Some(pts) = frame.pts_ns {
                        self.pts = Some(pts);
                    }
                }
                PeerEvent::Error(error) => return Err(error.into()),
                _ => {}
            }
        }
        Ok(())
    }
    fn held(&self) -> (bool, bool) {
        let min = self.x.setup().min_keycode;
        let mapping = self
            .x
            .get_keyboard_mapping(min, self.x.setup().max_keycode - min + 1)
            .unwrap()
            .reply()
            .unwrap();
        let code = min as usize
            + mapping
                .keysyms
                .chunks(mapping.keysyms_per_keycode as usize)
                .position(|s| s.contains(&0xffe1))
                .unwrap();
        let keys = self.x.query_keymap().unwrap().reply().unwrap().keys;
        let pointer = self
            .x
            .query_pointer(self.x.setup().roots[0].root)
            .unwrap()
            .reply()
            .unwrap();
        (
            keys[code / 8] & (1 << (code % 8)) != 0,
            pointer.mask.contains(KeyButMask::BUTTON1),
        )
    }
    async fn closed_and_released(&mut self) {
        self.live.take();
        let until = Instant::now() + Duration::from_secs(3);
        while self.held() != (false, false) {
            assert!(
                Instant::now() < until,
                "teardown must release actual X11 key/button"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

async fn transport_gap(recovers: bool) {
    let mut session = Session::start().await;
    let live = session.live.as_ref().unwrap();
    let mut authority = live.authority.deadline.load(Ordering::Acquire);
    let drops = live.peer.stats().dropped_encoded_frames;
    // This guard is declared after Session, so panic/error always unblocks before Peer teardown.
    let mut blocked = Some(live.peer.hold_video_for_native_test().unwrap());
    let mut first_rejection: Option<Instant> = None;
    let mut renewed = false;
    let mut acknowledged = Instant::now();
    let mut last_pts = session.pts.unwrap();
    let mut unblocked: Option<Instant> = None;
    let block_started = Instant::now();
    // Native GOP/onset is setup; the fixed media deadline starts at actual rejection.
    let observation_budget = Duration::from_secs(if recovers { 24 } else { 21 });
    loop {
        match session.tick() {
            Ok(()) => {}
            Err(error) => {
                assert!(!recovers, "recovered media must preserve session: {error}");
                assert_eq!(error.to_string(), "encoded video recovery timed out");
                let elapsed = first_rejection.expect("real transport rejection").elapsed();
                assert!(
                    elapsed >= Duration::from_secs(19) && elapsed < Duration::from_secs(21),
                    "fixed20s recovery budget: {elapsed:?}"
                );
                assert!(
                    renewed,
                    "healthy successful renewal must not mask media expiry"
                );
                assert!(
                    session.live.as_ref().unwrap().profile.width < 320,
                    "measured native pressure must apply UpdateProfile"
                );
                drop(blocked.take());
                session.closed_and_released().await;
                println!(
                    "PASS transport-only fixed20s close/release after {elapsed:?}; helper drops0, successful renewal/ACK/profile activity"
                );
                return;
            }
        }
        let live = session.live.as_ref().unwrap();
        assert_eq!(
            live.helper
                .video_health(std::time::Instant::now())
                .unwrap()
                .0,
            0,
            "test must isolate transport gap without helper queue overflow"
        );
        if live.peer.stats().dropped_encoded_frames > drops && first_rejection.is_none() {
            first_rejection = Some(Instant::now());
        }
        if let Some(started) = first_rejection {
            assert_eq!(
                session.held(),
                (true, true),
                "held loss: after={:?}, ack={}, display={:?}, helper_transition={:?}, helper_failed={}, stop={}, peer_closed={}, profile={}x{}@{}, pts={:?}",
                started.elapsed(),
                live.acknowledged,
                live.display,
                live.helper_transition_until,
                live.helper.failed.is_cancelled(),
                live.stop.is_cancelled(),
                live.peer.is_closed(),
                live.profile.width,
                live.profile.height,
                live.profile.fps,
                session.pts
            );
            assert_eq!(
                live.authority.deadline.load(Ordering::Acquire),
                authority,
                "media/ACK/profile activity cannot renew authority"
            );
            if recovers && blocked.is_some() && started.elapsed() > Duration::from_secs(2) {
                last_pts = session.pts.unwrap();
                drop(blocked.take());
                unblocked = Some(Instant::now());
            }
            if recovers
                && unblocked.is_some_and(|t| t.elapsed() > Duration::from_secs(20))
                && session.dimensions == (live.profile.width, live.profile.height)
            {
                assert!(
                    session.pts.is_some_and(|pts| pts > last_pts + 500_000_000),
                    "fresh decoded PTS after native unblock"
                );
                assert_eq!(session.held(), (true, true));
                session.closed_and_released().await;
                println!(
                    "PASS transport-only recovery before20s, fresh decoded PTS and held input survive original deadline"
                );
                return;
            }
            if !renewed && started.elapsed() > Duration::from_secs(15) {
                // Trusted post-authentication entry; signed HTTP ingress has separate service coverage.
                let request_start = boot_time_ms();
                session
                    .live
                    .as_mut()
                    .unwrap()
                    .renewed(request_start)
                    .unwrap();
                authority = request_start + 60_000;
                assert_eq!(
                    session
                        .live
                        .as_ref()
                        .unwrap()
                        .authority
                        .deadline
                        .load(Ordering::Acquire),
                    authority
                );
                renewed = true;
            }
            if acknowledged.elapsed() > Duration::from_secs(1) {
                session.input(InputEvent::DisplayAck);
                let live = session.live.as_ref().unwrap();
                live.helper
                    .send(HelperRequest::RequestKeyframe {
                        session_id: live.grant.session_id,
                        generation: live.display.unwrap().generation,
                    })
                    .unwrap();
                acknowledged = Instant::now();
            }
        }
        assert!(
            first_rejection.map_or_else(
                || block_started.elapsed() < Duration::from_secs(5),
                |started| started.elapsed() < observation_budget
            ),
            "transport fixture outer budget: recovers={recovers}, native_drop_delta={}, queued={:?}, first_gap_elapsed={:?}, unblocked_elapsed={:?}, pts={:?}/{last_pts}, profile_width={}",
            session
                .live
                .as_ref()
                .unwrap()
                .peer
                .stats()
                .dropped_encoded_frames
                - drops,
            session
                .live
                .as_ref()
                .unwrap()
                .peer
                .stats()
                .native
                .queued_video_frames,
            first_rejection.map(|t| t.elapsed()),
            unblocked.map(|t| t.elapsed()),
            session.pts,
            session.live.as_ref().unwrap().profile.width,
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit disposable native appsrc/after-ACK recovery fixture: scripts/test-e2e.sh"]
async fn transport_recovers_before_absolute_budget() {
    transport_gap(true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit disposable native appsrc/after-ACK recovery fixture: scripts/test-e2e.sh"]
async fn transport_stall_closes_despite_healthy_renewals() {
    transport_gap(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit disposable GTK/Xvfb long text fixture"]
async fn long_text_keeps_video_renewals_and_cancellation_live() {
    let mut session = Session::start().await;
    session.input(InputEvent::ReleaseAll);
    let path = std::env::var_os("REMOTEVNC_GTK_RESULT").unwrap();
    let received = || std::fs::read_to_string(&path).unwrap_or_default();
    assert!(received().is_empty(), "requires fresh GTK entry");
    let original_deadline = session
        .live
        .as_ref()
        .unwrap()
        .authority
        .deadline
        .load(Ordering::Acquire);
    let text = "ế😀e\u{301}中".repeat(200);
    assert_eq!(text.chars().count(), 1000);
    session.input(InputEvent::Text { text: text.clone() });
    let start = Instant::now();
    let mut renewed = 0;
    let mut next_renew = Instant::now() + Duration::from_secs(15);
    let mut pts = session.pts.unwrap();
    let mut frame_at = Instant::now();
    let mut advancing = 0;
    let mut motion_sent = false;
    let mut motion_observed = false;
    while received() != text {
        session.tick().unwrap();
        if !motion_sent && start.elapsed() >= Duration::from_millis(250) {
            session.input(InputEvent::PointerMove { x: 137, y: 93 });
            motion_sent = true;
        }
        if motion_sent && !motion_observed {
            let pointer = session
                .x
                .query_pointer(session.x.setup().roots[0].root)
                .unwrap()
                .reply()
                .unwrap();
            if (pointer.root_x, pointer.root_y) == (137, 93) {
                assert!(received().chars().count() < 1000);
                motion_observed = true;
                println!(
                    "PASS actual X11 pointer moves during unfinished text at {:?}",
                    start.elapsed()
                );
            }
            assert!(
                start.elapsed() < Duration::from_secs(2),
                "pointer starved behind text"
            );
        }
        if session.pts.unwrap() > pts {
            advancing += 1;
            pts = session.pts.unwrap();
            frame_at = Instant::now();
        }
        assert!(
            frame_at.elapsed() < Duration::from_secs(3),
            "media stalled during accepted text"
        );
        let live = session.live.as_mut().unwrap();
        assert_eq!(
            live.helper
                .video_health(std::time::Instant::now())
                .unwrap()
                .0,
            0
        );
        if Instant::now() >= next_renew {
            let now = boot_time_ms();
            live.renewed(now).unwrap();
            // Raw controller fixture has no SDK grant listener; mirror the same
            // authenticated renewal normally applied by ControllerSession.
            session.controller.renew_input_lease().unwrap();
            assert_eq!(
                live.authority.deadline.load(Ordering::Acquire),
                now + 60_000
            );
            next_renew += Duration::from_secs(15);
            renewed += 1;
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "exact1000-scalar text incomplete: {} scalars",
            received().chars().count()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(motion_observed && renewed >= 2 && advancing > 100);
    println!(
        "PASS exact1000 Unicode scalars; elapsed={:?}, advancing_frames={advancing}, renewals={renewed}",
        start.elapsed()
    );
    // Prove the helper/guardian actually consumed renewals, not only the agent
    // deadline store: keep real capture alive beyond the initial helper lease.
    while boot_time_ms() <= original_deadline + 250 {
        session.tick().unwrap();
        assert!(frame_at.elapsed() < Duration::from_secs(3));
        if session.pts.unwrap() > pts {
            pts = session.pts.unwrap();
            frame_at = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(received(), text);
    println!(
        "PASS real helper/guardian remain alive beyond original absolute lease after renewals"
    );
    // Focus loss cancels queued text and releases; video/session must survive.
    session.input(InputEvent::Text {
        text: "x".repeat(1000),
    });
    let until = Instant::now() + Duration::from_millis(250);
    while Instant::now() < until {
        session.tick().unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    session.input(InputEvent::ReleaseAll);
    let until = Instant::now() + Duration::from_millis(250);
    while Instant::now() < until {
        session.tick().unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let after_focus = received();
    assert!(after_focus.len() > text.len() && after_focus.len() < text.len() + 1000);
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        session.tick().unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        received(),
        after_focus,
        "focus cancellation cannot reinject queued text"
    );
    assert_eq!(session.held(), (false, false));
    session.input(InputEvent::Text {
        text: "y".repeat(1000),
    });
    let until = Instant::now() + Duration::from_millis(250);
    while Instant::now() < until {
        session.tick().unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    session.closed_and_released().await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let after_disconnect = received();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(received(), after_disconnect);
    assert!(after_disconnect.len() < after_focus.len() + 1000);
    println!("PASS focus/disconnect cancel pending Unicode; bounded release; no reinjection");
    // test-e2e.sh shares the disposable GTK receiver with the following signed
    // process test. Clean only its nonsecret contents using a fresh session;
    // never revive the disconnected authority or change the system clipboard.
    let mut cleanup = Session::start().await;
    cleanup.input(InputEvent::ReleaseAll);
    for (keysym, pressed) in [
        (0xffe3, true),
        (0x61, true),
        (0x61, false),
        (0xffe3, false),
        (0xff08, true),
        (0xff08, false),
    ] {
        cleanup.input(InputEvent::Key { keysym, pressed });
    }
    let until = Instant::now() + Duration::from_secs(3);
    while !received().is_empty() {
        cleanup.tick().unwrap();
        assert!(Instant::now() < until, "shared GTK receiver cleanup failed");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    cleanup.closed_and_released().await;
    println!("PASS shared GTK receiver empty and held input released for subsequent E2E case");
}
