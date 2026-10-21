# RemoteVNC Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. The execution method must be selected by the user before implementation; do not dispatch agents before that selection.

**Goal:** Build a Rust macOS client and Ubuntu 26.04 service that control the physical X11 login screen, desktop and lock screen, authorized by a shared Google account, over P2P with Cloudflare TURN fallback.

**Architecture:** A Rust backend handles Google identity, device registration, session leases and WebRTC signaling. An unprivileged Ubuntu agent handles networking, while a local privileged helper accesses the active physical X11 display. The macOS app receives H.264 video and sends input through separate WebRTC data channels.

**Tech Stack:** Rust workspace, Tokio, Axum, SQLite, OpenID Connect, Ed25519 device identities, GStreamer Rust bindings with native GStreamer/WebRTC, X11/XTest/XKB, eframe/egui, systemd and LightDM/Xfce.

**Spec:** [Approved specification](../specs/2026-10-20-remotevnc-design.md).

## Global Constraints

- “Hai thiết bị phải được đăng ký bằng cùng tài khoản Google trong ứng dụng.”
- “Một màn hình vật lý được chọn trên `seat0`, một client điều khiển tại một thời điểm.”
- “Agent là dịch vụ hệ thống tự chạy từ lúc boot.”
- “Agent làm mới lease quyền điều khiển mỗi 15 giây; không gia hạn được trong 60 giây thì dừng stream và nhả toàn bộ input.”
- “Không dùng clipboard mặc định để hỗ trợ cả trường mật khẩu và tránh làm thay đổi clipboard người dùng.”
- “Mock chỉ dành cho test, không có chế độ bỏ xác thực trong cấu hình production.”
- Supplied SSH hardware is Ubuntu 25.10 AMD64 with an active GDM/Wayland desktop and no passwordless sudo; the support target remains Ubuntu 26.04. Use isolated VM/container build tests and prepare a reviewable admin deployment/rollback before physical display-manager activation. No implicit OS upgrade or privileged-Docker sudo bypass.
- The initial host is macOS ARM64 with Rust 1.94.0; GStreamer, rustup and Docker are not installed. Establish native build dependencies before media verification. Ubuntu amd64 needs its own build and hardware acceptance.
- Task9 requires Rust 1.95 for pinned eframe/egui 0.37 and its supported native clipboard-shortcut control. Install a parallel isolated toolchain; preserve the host/default Homebrew compiler. Final native checks and CI use 1.95; earlier 1.94 runs remain historical evidence.
- No audio, file transfer, general clipboard sync, concurrent viewers, virtual desktops or Wayland in this version. Preserve Ubuntu authentication at the physical greeter and locker.
- Keep protocol, backend and policy tests independent of native media dependencies so they run on the host and in CI. Use existing crypto/OIDC/X11/media libraries rather than implementing protocols.

## Review Focus

1. Late mouse movement arriving after a click or display switch must not move the pointer into the wrong session; pin with Tasks 1 and 3.
2. Caps Lock, active modifiers and repeated IME commits must not corrupt password input or insert text twice; pin with Tasks 2 and 9.
3. X server resets can retain the same display number but replace the authority/socket; detect that and recover without stale input, Task 2.
4. Replayed OAuth callbacks and enrollment polling from another device must not bind an attacker to an account; pin with Task 5.
5. Revocation concurrent with signaling, expired leases and helper failures must close access and release held input, Tasks 6, 7 and 8.

## File Map

| Path | Responsibility |
| --- | --- |
| `Cargo.toml`, `Cargo.lock`, `.gitignore` | Workspace, reproducible dependencies, generated file exclusions |
| `crates/protocol/src/{lib,wire,input,ipc,identity,enrollment,session}.rs` | Shared bounded messages, device proof, input/session policy |
| `apps/seat-helper/src/{main,seat,x11,input,capture,ipc,recovery}.rs` | Physical seat selection, XTest/Unicode, capture and local IPC |
| `crates/transport/src/{lib,peer,video,channels,stats}.rs`, `crates/transport/src/peer/{signaling,runtime,lifecycle_tests}.rs` | GStreamer/WebRTC, encoded video, channels and peer statistics |
| `crates/device/src/{lib,api,enrollment,credentials}.rs` | Shared device login, signed authentication and authenticated API calls |
| `apps/backend/src/{lib,main,config,db,auth,devices,sessions,signaling,turn}.rs` | Google OIDC, persistence, account isolation, leases, signaling and Cloudflare |
| `apps/agent/src/{main,config,service,helper}.rs` | Enrollment CLI, reconnecting service, helper and transport integration |
| `apps/client/src/{main,app,input,video,autostart,media_runtime}.rs`, `apps/client/src/app/{session,runtime}.rs` | macOS login/device UI, IME/input, native media discovery and frame display |
| `packaging/{ubuntu,macos,backend}/` | Services, installer, app bundle and backend deployment |
| `scripts/`, `.github/workflows/`, `docs/{setup,testing}/` | Real checks, platform builds, deployment and acceptance instructions |

Tests live beside each app/crate in `tests/`. Add implementation files only when their task needs them. No independent abstraction layer for each dependency.

### Task 1: Shared protocol and input lease policy

**Files:** workspace manifests and exclusions; `crates/protocol/src/{lib,wire,input,ipc,identity}.rs`; `crates/protocol/tests/{policy,wire,identity}.rs`.

**Interfaces:** Produce serde `DeviceId`, `SessionId` (UUID), `DisplayInfo { generation, width, height }`, `InputEnvelope { version, session_id, generation, sequence, event }`, `InputEvent`, `SignalMessage`, `HelperRequest`, `HelperEvent` and `EncodedFrame { generation, pts_ns, keyframe, data }`. Produce `InputGuard::accept(&mut self, input: &InputEnvelope, now_ms: u64) -> Result<(), ProtocolError>`, `renew_lease(now_ms: u64)` and `expired(now_ms: u64) -> bool`. Use monotonic milliseconds for local leases. Produce device challenge signing/verification functions using Ed25519; include purpose, nonce, device ID and expiry in the signed message.

- [x] Create minimal Cargo workspace manifests, protocol test target and `.gitignore`; do not add unimplemented application binaries.
- [x] Add tests asserting `expired(59_999) == false`, `expired(60_000) == true` after lease at zero; reject other session IDs, stale generation, duplicate pointer sequences, invalid coordinates, oversized messages and replayed device challenges. Wire version is 1, control payload limit 64 KiB and text limit 16 KiB UTF-8; encoded frame IPC limit is 8 MiB with bounded queues.
- [x] Run `cargo test -p remotevnc-protocol`; confirm red assertions or missing implementation symbols, then implement the named types and policies. Click messages include position; sequence tracking for motion does not reject independent ordered-channel events.
- [x] Run the protocol tests, including `unicode_payload_round_trips`, `old_generation_is_rejected` and `click_position_is_self_contained`; all pass.
- [x] Commit `feat: define remote session protocol and input policy`.

### Task 2: Physical X11 helper and Unicode proof

**Files:** `apps/seat-helper/src/{main,seat,x11,input,ipc}.rs`; `apps/seat-helper/tests/{seat,input,ipc}.rs`; `scripts/test-x11.sh`.

**Interfaces:** Consume Task 1 IPC/input types. Produce `SeatMonitor::current(&mut self) -> Result<Option<DisplayTarget>>`, `X11Input::connect(target: &DisplayTarget) -> Result<Self>`, `apply(&mut self, event: &InputEvent) -> Result<()>` and `release_all(&mut self) -> Result<()>`. `DisplayTarget` includes seat, display, authority and an X server identity that changes on server reset. CLI supports `remotevnc-seat-helper doctor` and explicit `--socket`/`--agent-uid` options.

- [x] Write failing tests for rejecting non-physical/non-seat0 displays, same display number with changed X server identity, stale generation, unauthorized Unix peer UID and bounded IPC decoding. Add X11 integration tests for mouse/key release and keymap restoration on error.
- [x] Run `cargo test -p remotevnc-seat-helper`; implement seat discovery from logind/LightDM and X11 connection using x11rb, without shell-command interpolation or arbitrary path requests from IPC.
- [x] Implement XTest key/mouse events, XKB-aware shortcuts and serialized Unicode keysym injection with saved/restored spare keycodes. Keep physical modifiers/Caps Lock state separate from text injection and restore state after errors; report unsupported input rather than silently dropping text.
- [x] Run `scripts/test-x11.sh` on Linux under Xvfb/Xephyr; assert exact received strings for `Tiếng Việt`, NFC/NFD, CJK and a non-BMP character in a GTK test receiver, plus no duplicate IME commit and no held keys after disconnect. On Ubuntu hardware, run doctor and record login/desktop/lock transitions; if inaccessible, explicitly leave that acceptance pending.
- [x] Commit `feat: control the active physical X11 seat`.

### Task 3: WebRTC peer and data channels

**Files:** `crates/transport/src/{lib,peer,channels,stats}.rs`, private `crates/transport/src/peer/{signaling,runtime,lifecycle_tests}.rs`; `crates/transport/tests/{loopback,channels}.rs`; `docs/setup/native-dependencies.md`.

**Interfaces:** Consume protocol `SignalMessage`, `InputEnvelope` and IDs. Produce `Peer::new(config: PeerConfig) -> Result<Self>`, `handle_signal(&self, message: SignalMessage) -> Result<()>`, `send_input(&self, input: InputEnvelope) -> Result<()>`, `events(&mut self) -> &mut Receiver<PeerEvent>`, `restart_ice(&self, ice: Vec<IceServer>) -> Result<()>` and `close(&self)`. `PeerConfig` includes session/peer identity, allowed role, ICE servers and relay-only policy; `PeerEvent` covers signaling, decoded input, frame and connection/stats changes. Video frame events carry `DecodedFrame { generation, width, height, rgba, pts_ns }`; `IceServer` carries URLs, optional username/credential and optional expiry. Define it in `remotevnc-protocol` and re-export from transport so the backend/device crates do not need native GStreamer dependencies.

- [x] Write failing two-peer loopback tests for SDP/ICE exchange and exact Unicode/control messages, rejecting malformed/oversized envelopes and wrong session/generation before input delivery.
- [x] Install documented native GStreamer development/runtime packages on the build host; verify required plugins with `gst-inspect-1.0 webrtcbin`, `nice`, `h264parse` and `avdec_h264`. Resolve compatible crate versions and retain Cargo.lock.
- [x] Implement GStreamer `webrtcbin`, asynchronous signaling and two channels: unordered/max-retransmits=0 pointer motion, reliable/ordered control. Use bounded queues and carry click coordinates independently of motion-channel ordering.
- [x] Run `cargo test -p remotevnc-transport`; loopback peers must connect, exchange input and close cleanly. A reordered motion message after a click may not override a newer pointer sequence. Verify relay-only configuration without claiming Cloudflare was tested locally.
- [x] Commit `feat: add WebRTC transport and low latency input channels`.

### Task 4: H.264 capture and display recovery

**Files:** `apps/seat-helper/src/capture.rs`; update helper seat/IPC modules; `crates/transport/src/video.rs`; `apps/seat-helper/tests/capture.rs`; `crates/transport/tests/video.rs`.

**Interfaces:** Consume `DisplayTarget`, `EncodedFrame`, IPC and `Peer`. Produce `Capture::start(target: &DisplayTarget, profile: VideoProfile, generation: u64) -> Result<Self>`, `frames(&mut self) -> &mut Receiver<EncodedFrame>`, `request_keyframe(&self) -> Result<()>` and `stop(self)`. Extend `Peer` with `push_video(&self, frame: EncodedFrame) -> Result<()>`. `VideoProfile` has bounded width/height/FPS/bitrate and a supported encoder selection. Typed display metadata signaling is emitted by Agent before its generation Restart under the same lifecycle transaction; Controller applies it without initiating a second rebuild. Include initial metadata, role/epoch/generation validation and application display-change events. Explicit unavailable-display signaling/API/events must clear current input/frame delivery while retaining the logical authorized session and lease; preserve generation high-water while waiting, then restore with newer metadata, rebuild and acknowledgement.

- [x] Add failing tests that encode a known test pattern, deliver it through the peer and verify decoded dimensions/pixels; a capture reset must produce a new generation and begin with decodable keyframe/config data.
- [x] Implement ximagesrc capture with required XInitThreads initialization before thread creation. Prefer available hardware encoder after probing; use software H.264 fallback, no B frames and bounded leaky queues before encoding. Preserve timestamps/access-unit boundaries through helper IPC and appsrc/RTP.
- [x] Implement profile updates from transport statistics with bounded bitrate/FPS/resolution, downshift under pressure and keyframe on display changes. Keep old-generation input disabled until current DisplayInfo is acknowledged; clear client-held input when generations change.
- [x] Run helper capture and transport video tests plus a repeated 1080p stream stress check; verify queue bounds, no unbounded frame backlog and clean restart. Record hardware/software encoder selected; do not infer physical-seat acceptance from a test source.
- [x] Commit `feat: stream and recover the physical X11 display`.

### Task 5: Google identity and persistent device enrollment

**Files:** `crates/protocol/src/enrollment.rs`; `apps/backend/src/{lib,main,config,db,auth,devices}.rs`; `apps/backend/migrations/001_identity.sql`; `crates/device/src/{lib,api,enrollment,credentials}.rs`; backend `tests/{google,enrollment,devices}.rs`; device `tests/credentials.rs`.

**Interfaces:** Consume protocol device identity/proof types. Produce `BackendState`, `router(state: BackendState) -> Router`, `AuthService::finish_login(&self, code: &str, state: &str) -> Result<LoginCompletion>`, `DeviceApi::begin_enrollment(&self, name: &str, public_key: &str) -> Result<Enrollment>`, `poll_enrollment(&self, enrollment: &Enrollment) -> Result<EnrollmentStatus>` and `authenticate(&self, identity: &DeviceIdentity) -> Result<AccessToken>`. `Enrollment` includes display code, browser URL, private polling verifier and expiry; never serialize the verifier into the URL.

- [x] Write failing tests using a test-only OIDC issuer/token/JWKS fixture for valid login, wrong signature/audience/issuer/nonce, expired token, reused state, stolen polling ID and mismatched device proof. Assert that different Google subjects sharing a supplied email remain different owners.
- [x] Implement Google discovery and authorization-code exchange with a standard OIDC client, state/nonce/PKCE and confirmation of device display code. Persist users by `sub`, devices, consumed enrollment transactions and revocation status in SQLite. Use test dependency injection rather than production auth-bypass flags.
- [x] Implement POST `/v1/enrollments`, browser `/auth/google/start` and the validated configured callback path (default `/auth/google/callback`, supplied deployment `/login/callback`), explicit browser confirmation, POST `/v1/enrollments/{id}/poll`, device challenge/authentication, GET `/v1/devices` and DELETE `/v1/devices/{id}`. Require valid device ownership for list/revoke; hash enrollment verifier at rest and consume one-time challenges atomically. Provide Keychain and permission-restricted file storage in the device crate, with platform-gated code.
- [x] Run `cargo test -p remotevnc-backend -p remotevnc-device`; assert enrollment survives backend restart, credentials survive application restart, consumed OAuth transactions cannot replay and another account cannot list/revoke device IDs. Ensure logs redact all credentials and submitted text.
- [x] Commit `feat: authenticate Google accounts and enroll persistent devices`.

### Task 6: Session leases, signaling and Cloudflare TURN

**Files:** shared pure `crates/protocol/src/session.rs`; backend `src/{sessions,signaling,turn}.rs`; `migrations/002_sessions.sql`; backend `tests/{sessions,signaling,turn}.rs`; extend `crates/device/src/api.rs`.

**Interfaces:** Produce `SessionGrant { session_id, client_id, agent_id, lease_expires_at, ice_servers }`, `SessionService::create(&self, caller: DeviceId, agent: DeviceId) -> Result<SessionGrant>`, `renew(&self, caller: DeviceId, session: SessionId) -> Result<SessionGrant>`, `close(&self, caller: DeviceId, session: SessionId) -> Result<()>`, `TurnClient::issue(&self, ttl: Duration) -> Result<Vec<IceServer>>`. Extend DeviceApi with typed session, device event and signaling methods. Persist ownership, not just device IDs in an in-memory connection table.

- [x] Write failing tests for cross-account session/signaling/credential requests, concurrent clients racing for one agent, revoked credentials on existing WebSockets, and signaling that races with revoke. Tests require one winner and no forwarded payload after revocation commits.
- [x] Implement POST `/v1/sessions`, lease renewal/close and authenticated `/v1/events` WebSocket signaling, validating sender, receiver, session, expiry, message size and rate on each message. Use a 15-second renewal cadence and 60-second grant lifetime; expire old grants after backend restart. Authenticate sockets without putting bearer secrets in query strings.
- [x] Implement the documented Cloudflare credential API behind TurnClient with backend-only secrets, bounded timeout, response validation and error handling. Use a 3,600-second TURN credential TTL, request renewal 300 seconds before expiry for authorized sessions, and carry refreshed ICE config to both peers. Do not issue TURN credentials to anonymous or other-account callers.
- [x] Run backend tests against a local mock TURN HTTP endpoint, asserting URL/body/auth and failure cases without printing tokens. Credential TTL and session lease are independent; no Cloudflare credentials means clear TURN-unavailable status, never a claim of relay support tested live.
- [x] Commit `feat: authorize remote sessions and provision Cloudflare TURN`.

### Task 7: Guarded input deadlines and native crash recovery

**Files:** `crates/transport/src/peer.rs` and scoped native transport tests; protocol input/IPC; helper `src/{input,ipc,main,recovery}.rs`; helper `tests/{ipc,recovery}.rs`; existing X11 fixtures/scripts where needed.

**Interfaces:** Produce config-only `Peer::install_ice(&self, ice: Vec<IceServer>) -> Result<()>` under lifecycle synchronization without rebuild or lease extension. Carry an absolute Linux CLOCK_BOOTTIME deadline in helper Start/RenewLease and enforce it in helper/guardian, bounded to at most 60 seconds. Produce private bounded root-only guardian IPC and binary mode; existing helper input requires a healthy guardian before mutation. Guardian retains an independent X11 connection with a guardian-owned private window/nonce binding to the exact server, and never reconnects recovery onto a replacement server.

- [x] Establish actual native preflight behavior before fixes: helper SIGKILL leaves owned keys/buttons or temporary Unicode keymap. Add RED/GREEN tests for crash cleanup, including exact full-map restoration, and config-only ICE installation preserving engine/epoch/lease while the next Restart uses stored configuration.
- [x] Implement write-before-mutation in-memory recovery state with acknowledgement before X11 effects. Recover owned keys/buttons and conditionally restore only proven temporary Unicode/mapping/modifier state. No disk input journal, typed-value logs, arbitrary remote paths or network-facing recovery API. Validate physical-source ownership before admitting control. Retain bounded pending recovery while a physical source holds an overlapping key/button; refuse conflicting new control until cleanup completes, never use a timeout to release local input.
- [x] On helper death, independent guardian recovers through its retained connection. On guardian death, surviving helper stops capture/new input first and retains its recovery connection until pending physical ownership drains. Test guardian-owned anchor survival after helper death, rejection after actual same-process Xserver reset, pre-existing local state, physical overlap and eventual source release, guardian deadline expiry and graceful cleanup. Simultaneous destruction of both recovery owners with a surviving Xserver is a distinct unsupported fault and must be documented. Document the residual non-atomic query-to-XTest-release window: a new physical press arriving between those operations can be affected, and concurrent local keymap/lock/group changes between conditional query and restoration can be overwritten; do not claim race-free simultaneous local/remote input or a hard timing bound.
- [x] Reject expired/backward/out-of-range local deadlines and prove display/input/reconnect activity cannot extend them. Run focused host/Linux tests and native Xvfb/GTK fixtures with explicit test-only factories; no production bypass flags. Capture worker and local input recovery remain independent of backend/network operations. All three systemd units and actual sandbox wiring are Task8 integration work.
- [x] Commit `fix: guard physical input and lease deadlines across helper crashes` and provide the reviewed API/native-evidence handoff for Task8.

### Task 8: Ubuntu boot service and end-to-end agent

**Files:** `apps/agent/src/{main,config,service,helper}.rs`; scoped typed-401 error handling in `crates/device/src/{lib,api}.rs` and SDK tests; agent `tests/{service,lease,helper}.rs` and test-only `tests/fixtures/seat_service.rs` executable; `packaging/ubuntu/{remotevnc-agent.service,remotevnc-seat-helper.service,remotevnc-input-guardian.service}`.

**Interfaces:** Consume DeviceApi, Peer, InputGuard and helper IPC. Produce CLI `remotevnc-agent enroll`, `run`, `doctor` and `revoke-local`; `AgentService::run(self, shutdown: CancellationToken) -> Result<()>`. Config contains backend origin, identity path, helper socket, seat and optional relay-only diagnosis; no arbitrary capture pipeline from the network.

- [x] Write failing service tests with fake helper/OIDC fixture for boot without a user session, reconnect/backoff, renewal at 15 seconds and release/stream-stop at 60 seconds after last valid lease. Helper failure and device revocation must release held input and stop delivery. Ordinary focus loss/ReleaseAll releases held input while preserving video and the current authorized session; it never renews the lease.
- [x] Implement outbound event/signaling connection, authenticated session grant handling, capture-to-Peer path, InputGuard checks and bounded helper IPC. Consume Task7 guarded helper IPC and shared absolute CLOCK_BOOTTIME deadlines; never reset authority from display/reconnect activity. Integrate independent guardian readiness/recovery-pending status and preserve survivor cleanup. Tie helper control to a current lease; reject input before authorization even if a data channel is open. Reconnect through fresh authenticated grants and ICE rather than replaying input.
- [x] Add systemd services with dedicated network-agent UID, private state directory, restricted helper socket and restart behavior. Helper retains only required local display privileges; configure `RestrictAddressFamilies=AF_UNIX` for helper while preserving GStreamer local IPC requirements, and verify the chosen sandbox permits X11/capture.
- [x] Run `cargo test -p remotevnc-agent`; use paused Tokio time for lease boundaries. On Linux run `systemd-analyze verify` for all three units and a process-level fake-helper disconnect check. No test may switch the developer machine's display manager.
- [x] Commit `feat: run unattended Ubuntu remote access from boot`.

### Task 9: macOS application and committed IME input

**Files:** `apps/client/src/{main,app,input,video,autostart}.rs`, private `apps/client/src/app/{session,runtime}.rs`; focused Rust1.95 lint compatibility in shared protocol; test-only `apps/client/examples/ui_smoke.rs`; client `tests/{input,viewport,session,runtime}.rs`; `packaging/macos/{Info.plist,build-app.sh}`.

**Interfaces:** Consume DeviceApi, Peer and InputEnvelope. Produce `ClientApp` for eframe, `InputMapper::map(&mut self, event: ClientInput, viewport: Viewport, display: DisplayInfo) -> Vec<InputEvent>`, latest-frame receiver, explicit sign-in/out and `set_autostart(enabled: bool) -> Result<()>`. Keep app model/testable mapping separate from native UI callbacks.

- [x] Add failing tests for Retina scaling, letterboxed edges, drag release outside viewport, focus loss, Command/Control mapping, IME composition versus commit, duplicate text/key events and generation changes. Assert exactly one `Tiếng Việt` commit, no composition fragments and a release-all on focus loss.
- [x] Implement sign-in/browser confirmation, device list, selection, connect/disconnect and statuses for busy/offline/reauthorization/display transition. Poll enrollment using verifier and persist only application/device credentials in Keychain.
- [x] Render newest decoded frame into egui without accumulating old frames; map pointer coordinates to remote pixels and include position on clicks. Integrate macOS IME commit events, shortcuts and held-key cleanup. Show measured stats and selected ICE candidate type; add an opt-in LaunchAgent for macOS login startup without screen recording/accessibility permission requests.
- [x] Run `cargo test -p remotevnc-client` and `cargo build -p remotevnc-client`; launch the app locally and verify login/device views and window input behavior. Real screen/IME/Keychain/WebRTC interaction with Ubuntu remains a separate manual acceptance, not a screenshot-only test.
- [x] Commit `feat: add the macOS remote desktop client`.

### Task 10: Installers, backend deployment and reproducible builds

**Files:** `packaging/ubuntu/{install.sh,rollback.sh,lightdm.conf}`, `packaging/backend/{Dockerfile,compose.yaml,env.example}` plus scoped ingress/maintenance/backup support examples, `scripts/{build-linux.sh,check.sh}`, `.github/workflows/ci.yml`, `README.md`, `docs/setup/{ubuntu,macos,backend}.md`; narrow macOS normal-executable media bootstrap and bundle resources for Finder/LaunchAgent plugin discovery.

**Interfaces:** Ubuntu installer supports `--prepare`, `--apply` and `--dry-run`, backs up prior display-manager configuration and emits rollback location; it does not change display manager in prepare/build/test modes. Backend deployment expects HTTPS public origin, OAuth client/secret and TURN key from environment/secret files; expose its Rust service only through configured ingress.

- [x] Add failing installer checks in a disposable Linux root/environment: dry-run makes no changes, prepare cannot restart display manager, invalid prerequisites fail before writes, backup/rollback restore previous settings and secret files are private. Test production backend rejects absent/invalid auth configuration.
- [x] Implement Ubuntu dependency installation and explicit LightDM/Xfce/greeter/X11-locker setup, service user/state setup, doctor and rollback; do not reboot automatically. Install required GStreamer plugins and choose software encoder fallback when hardware probing fails. Document local enrollment before reboot.
- [x] Implement macOS app bundle/dependency build instructions, Ubuntu native build script and single-instance SQLite backend image/compose example. Do not label unsigned local builds as notarized packages. Keep generated app bundles, DBs, recordings and secrets outside git.
- [x] Make the normal macOS app and its opt-in LaunchAgent discover the project-built gstnice plugin without inherited terminal environment. Bundle the plugin/nonsecret runtime configuration as needed, keep external Homebrew runtime requirements explicit, and verify factory discovery and native launch with GST_PLUGIN_PATH removed.
- [x] Carry Task5 operational requirements into deployment: bound public enrollment/challenge ingress and expired ephemeral-record retention, preserve revoked device-key ownership tombstones, provide a private consistent SQLite backup, and prevent OAuth callback queries/auth headers from proxy logs. Ingress examples must be additive; the user confirmed amio.vnoffice.io.vn is dedicated to RemoteVNC. Live routing still requires the documented credential/ingress steps.
- [x] Add CI jobs for Rust fmt/clippy, protocol/backend/device tests, Ubuntu native dependencies plus Xvfb/media tests and macOS client build; test explicit target-gated modules on their native platforms. Run `scripts/check.sh` and `bash -n` on scripts; record Linux checks not runnable on macOS rather than marking them passed.
- [x] Commit `build: package services and document deployment`.

### Task 11: Integration, acceptance evidence and review

**Files:** `tests/e2e/`, `scripts/test-e2e.sh`, `docs/testing/{acceptance.md,results.md}`; targeted fixes to files from Tasks 1–10.

**Interfaces:** Test harness orchestrates backend, agent/helper and client transport against test-only OIDC issuer and test display. Real acceptance uses actual Google OAuth, Cloudflare TURN and Ubuntu physical screen; no permanent debug login or permissive flags in production binaries.

- [x] Write failing process-level checks: account A connects through WebRTC and types exact Unicode into a receiver; account B cannot enumerate or signal; revoke during a running session closes access; helper/backend restart cannot leave input pressed. Use existing task fixtures rather than production bypasses.
- [x] Run local integration, fix failures at their shared cause and rerun only affected checks plus the final suite. Exercise loopback media and channel flow with real GStreamer, not a mock returning connected.
- [ ] On provided Ubuntu hardware, record boot → physical login → desktop → lock/unlock → logout/login → reboot. Verify physical monitor matches client view, input works at greeter/locker and no separate virtual desktop was created. If hardware/SSH is unavailable, list each case as pending with its exact command/procedure.
- [ ] With provided credentials, record real Google enrollment and different-account denial; force Cloudflare relay and verify selected candidate type, TURN renewal and recovery. Record CPU/GPU/encoder/resolution/FPS/RTT, queue timing and observed input-to-image p95; target LAN p95 below 100 ms on capable hardware, without treating RTT as that measurement. Mark missing credentials/results pending, never completed.
- [x] Run fresh native build/tests/fmt/clippy and git diff checks, review Task11 and commit scoped fixes. Exact-source coverage and failures are recorded in `docs/testing/results.md`; preserve the historical AMD 1 pass / 1 fail and instrumented result separately from the later unchanged normal pair passing 2/2 after the proven worker-output flush correction.
- [x] Complete final whole-branch review and required corrections. Original final review at 1d1cebd, negative first scoped rereview at a1bfbf7, and the explicitly exceptional narrow correction through bedcc58 are preserved. Final scoped verification closes FR-I3/FR-M3/FR-R1 with no new Critical/Important breakage; the other six findings were already closed. Final delivery separates implemented/build-tested behavior, hardware results and still-required deployment inputs.

Implementation record: Tasks1–11 passed their task-scoped reviews, followed by whole-branch review and the corrections recorded above. Current normal component provenance is client2990896, agentb41ba7d, helper1bc63df, unit5c37387, backend unchanged Task10/69a627e; documentation-only commits do not restamp binary sources. Historical 142-test matrices at adf6de8 remain unchanged-path baseline. The normal AMD pair subsequently passed 2/2 after a focused buffering RED/GREEN; the exact cause of the original DOWN-adjacent termination remains unattributed. Physical Ubuntu, live Google/TURN, manual macOS and input-to-image p95 acceptance remain pending with procedures in `docs/testing/acceptance.md`. Additional Ubuntu25.10 ordinary/X11/GTK/WebRTC E2E compatibility passed separately; see `docs/testing/compatibility-ubuntu2510.md`. The 26.04 installer target and pending physical service activation are unchanged. No physical/public deployment has occurred.

## Execution and External Inputs

This plan implements one integrated remote-control product; individual tasks each have a runnable test target. Native execution in the current session is recommended because protocol, helper, transport and agent share evolving interfaces, and it avoids repeating large contexts across implementers. Subagent execution is also available if explicitly selected by the user.

No implementation begins until the user reviews this plan and selects an execution method. During implementation, complete all local authorized code/build work without waiting for remote credentials. Ask for Ubuntu SSH, backend/domain and Google/Cloudflare configuration only when needed for real deployment/acceptance; request secret delivery through environment/secret files, not repository commits or tool output.
