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
- “Chưa có địa chỉ SSH hoặc thông tin GPU của Ubuntu, nên kiểm thử phần cứng là bước nghiệm thu riêng.”
- The initial host is macOS ARM64 with Rust 1.94.0; GStreamer, rustup and Docker are not installed. Establish native build dependencies before media verification. Ubuntu amd64 needs its own build and hardware acceptance.
- No audio, file transfer, general clipboard sync, concurrent viewers, virtual desktops or Wayland in this version. Preserve Ubuntu authentication at the physical greeter and locker.
- Keep protocol, backend and policy tests independent of native media dependencies so they run on the host and in CI. Use existing crypto/OIDC/X11/media libraries rather than implementing protocols.

## Review Focus

1. Late mouse movement arriving after a click or display switch must not move the pointer into the wrong session; pin with Tasks 1 and 3.
2. Caps Lock, active modifiers and repeated IME commits must not corrupt password input or insert text twice; pin with Tasks 2 and 8.
3. X server resets can retain the same display number but replace the authority/socket; detect that and recover without stale input, Task 2.
4. Replayed OAuth callbacks and enrollment polling from another device must not bind an attacker to an account; pin with Task 5.
5. Revocation concurrent with signaling, expired leases and helper failures must close access and release held input, Tasks 6 and 7.

## File Map

| Path | Responsibility |
| --- | --- |
| `Cargo.toml`, `Cargo.lock`, `.gitignore` | Workspace, reproducible dependencies, generated file exclusions |
| `crates/protocol/src/{lib,wire,input,ipc,identity}.rs` | Shared bounded messages, device proof, input/session policy |
| `apps/seat-helper/src/{main,seat,x11,input,capture,ipc}.rs` | Physical seat selection, XTest/Unicode, capture and local IPC |
| `crates/transport/src/{lib,peer,video,channels,stats}.rs` | GStreamer/WebRTC, encoded video, channels and peer statistics |
| `crates/device/src/{lib,api,enrollment,credentials}.rs` | Shared device login, signed authentication and authenticated API calls |
| `apps/backend/src/{main,config,db,auth,devices,sessions,signaling,turn}.rs` | Google OIDC, persistence, account isolation, leases, signaling and Cloudflare |
| `apps/agent/src/{main,config,service,helper}.rs` | Enrollment CLI, reconnecting service, helper and transport integration |
| `apps/client/src/{main,app,input,video,autostart}.rs` | macOS login/device UI, IME/input and frame display |
| `packaging/{ubuntu,macos,backend}/` | Services, installer, app bundle and backend deployment |
| `scripts/`, `.github/workflows/`, `docs/{setup,testing}/` | Real checks, platform builds, deployment and acceptance instructions |

Tests live beside each app/crate in `tests/`. Add implementation files only when their task needs them. No independent abstraction layer for each dependency.

### Task 1: Shared protocol and input lease policy

**Files:** workspace manifests and exclusions; `crates/protocol/src/{lib,wire,input,ipc,identity}.rs`; `crates/protocol/tests/{policy,wire,identity}.rs`.

**Interfaces:** Produce serde `DeviceId`, `SessionId` (UUID), `DisplayInfo { generation, width, height }`, `InputEnvelope { version, session_id, generation, sequence, event }`, `InputEvent`, `SignalMessage`, `HelperRequest`, `HelperEvent` and `EncodedFrame { generation, pts_ns, keyframe, data }`. Produce `InputGuard::accept(&mut self, input: &InputEnvelope, now_ms: u64) -> Result<(), ProtocolError>`, `renew_lease(now_ms: u64)` and `expired(now_ms: u64) -> bool`. Use monotonic milliseconds for local leases. Produce device challenge signing/verification functions using Ed25519; include purpose, nonce, device ID and expiry in the signed message.

- [ ] Create minimal Cargo workspace manifests, protocol test target and `.gitignore`; do not add unimplemented application binaries.
- [ ] Add tests asserting `expired(59_999) == false`, `expired(60_000) == true` after lease at zero; reject other session IDs, stale generation, duplicate pointer sequences, invalid coordinates, oversized messages and replayed device challenges. Wire version is 1, control payload limit 64 KiB and text limit 16 KiB UTF-8; encoded frame IPC limit is 8 MiB with bounded queues.
- [ ] Run `cargo test -p remotevnc-protocol`; confirm red assertions or missing implementation symbols, then implement the named types and policies. Click messages include position; sequence tracking for motion does not reject independent ordered-channel events.
- [ ] Run the protocol tests, including `unicode_payload_round_trips`, `old_generation_is_rejected` and `click_position_is_self_contained`; all pass.
- [ ] Commit `feat: define remote session protocol and input policy`.

### Task 2: Physical X11 helper and Unicode proof

**Files:** `apps/seat-helper/src/{main,seat,x11,input,ipc}.rs`; `apps/seat-helper/tests/{seat,input,ipc}.rs`; `scripts/test-x11.sh`.

**Interfaces:** Consume Task 1 IPC/input types. Produce `SeatMonitor::current(&mut self) -> Result<Option<DisplayTarget>>`, `X11Input::connect(target: &DisplayTarget) -> Result<Self>`, `apply(&mut self, event: &InputEvent) -> Result<()>` and `release_all(&mut self) -> Result<()>`. `DisplayTarget` includes seat, display, authority and an X server identity that changes on server reset. CLI supports `remotevnc-seat-helper doctor` and explicit `--socket`/`--agent-uid` options.

- [ ] Write failing tests for rejecting non-physical/non-seat0 displays, same display number with changed X server identity, stale generation, unauthorized Unix peer UID and bounded IPC decoding. Add X11 integration tests for mouse/key release and keymap restoration on error.
- [ ] Run `cargo test -p remotevnc-seat-helper`; implement seat discovery from logind/LightDM and X11 connection using x11rb, without shell-command interpolation or arbitrary path requests from IPC.
- [ ] Implement XTest key/mouse events, XKB-aware shortcuts and serialized Unicode keysym injection with saved/restored spare keycodes. Keep physical modifiers/Caps Lock state separate from text injection and restore state after errors; report unsupported input rather than silently dropping text.
- [ ] Run `scripts/test-x11.sh` on Linux under Xvfb/Xephyr; assert exact received strings for `Tiếng Việt`, NFC/NFD, CJK and a non-BMP character in a GTK test receiver, plus no duplicate IME commit and no held keys after disconnect. On Ubuntu hardware, run doctor and record login/desktop/lock transitions; if inaccessible, explicitly leave that acceptance pending.
- [ ] Commit `feat: control the active physical X11 seat`.

### Task 3: WebRTC peer and data channels

**Files:** `crates/transport/src/{lib,peer,channels,stats}.rs`; `crates/transport/tests/{loopback,channels}.rs`; `docs/setup/native-dependencies.md`.

**Interfaces:** Consume protocol `SignalMessage`, `InputEnvelope` and IDs. Produce `Peer::new(config: PeerConfig) -> Result<Self>`, `handle_signal(&self, message: SignalMessage) -> Result<()>`, `send_input(&self, input: InputEnvelope) -> Result<()>`, `events(&mut self) -> &mut Receiver<PeerEvent>`, `restart_ice(&self, ice: Vec<IceServer>) -> Result<()>` and `close(&self)`. `PeerConfig` includes session/peer identity, allowed role, ICE servers and relay-only policy; `PeerEvent` covers signaling, decoded input, frame and connection/stats changes. Video frame events carry `DecodedFrame { generation, width, height, rgba, pts_ns }`; `IceServer` carries URLs, username, credential and expiry.

- [ ] Write failing two-peer loopback tests for SDP/ICE exchange and exact Unicode/control messages, rejecting malformed/oversized envelopes and wrong session/generation before input delivery.
- [ ] Install documented native GStreamer development/runtime packages on the build host; verify required plugins with `gst-inspect-1.0 webrtcbin`, `nice`, `h264parse` and `avdec_h264`. Resolve compatible crate versions and retain Cargo.lock.
- [ ] Implement GStreamer `webrtcbin`, asynchronous signaling and two channels: unordered/max-retransmits=0 pointer motion, reliable/ordered control. Use bounded queues and carry click coordinates independently of motion-channel ordering.
- [ ] Run `cargo test -p remotevnc-transport`; loopback peers must connect, exchange input and close cleanly. A reordered motion message after a click may not override a newer pointer sequence. Verify relay-only configuration without claiming Cloudflare was tested locally.
- [ ] Commit `feat: add WebRTC transport and low latency input channels`.

### Task 4: H.264 capture and display recovery

**Files:** `apps/seat-helper/src/capture.rs`; update helper seat/IPC modules; `crates/transport/src/video.rs`; `apps/seat-helper/tests/capture.rs`; `crates/transport/tests/video.rs`.

**Interfaces:** Consume `DisplayTarget`, `EncodedFrame`, IPC and `Peer`. Produce `Capture::start(target: &DisplayTarget, profile: VideoProfile) -> Result<Self>`, `frames(&mut self) -> &mut Receiver<EncodedFrame>`, `request_keyframe(&self) -> Result<()>` and `stop(self)`. Extend `Peer` with `push_video(&self, frame: EncodedFrame) -> Result<()>`. `VideoProfile` has bounded width/height/FPS/bitrate and a supported encoder selection.

- [ ] Add failing tests that encode a known test pattern, deliver it through the peer and verify decoded dimensions/pixels; a capture reset must produce a new generation and begin with decodable keyframe/config data.
- [ ] Implement ximagesrc capture with required XInitThreads initialization before thread creation. Prefer available hardware encoder after probing; use software H.264 fallback, no B frames and bounded leaky queues before encoding. Preserve timestamps/access-unit boundaries through helper IPC and appsrc/RTP.
- [ ] Implement profile updates from transport statistics with bounded bitrate/FPS/resolution, downshift under pressure and keyframe on display changes. Keep old-generation input disabled until current DisplayInfo is acknowledged; clear client-held input when generations change.
- [ ] Run helper capture and transport video tests plus a repeated 1080p stream stress check; verify queue bounds, no unbounded frame backlog and clean restart. Record hardware/software encoder selected; do not infer physical-seat acceptance from a test source.
- [ ] Commit `feat: stream and recover the physical X11 display`.

### Task 5: Google identity and persistent device enrollment

**Files:** `apps/backend/src/{main,config,db,auth,devices}.rs`; `apps/backend/migrations/001_identity.sql`; `crates/device/src/{lib,api,enrollment,credentials}.rs`; backend `tests/{google,enrollment,devices}.rs`; device `tests/credentials.rs`.

**Interfaces:** Consume protocol device identity/proof types. Produce `BackendState`, `router(state: BackendState) -> Router`, `AuthService::finish_login(&self, code: &str, state: &str) -> Result<LoginCompletion>`, `DeviceApi::begin_enrollment(&self, name: &str, public_key: &str) -> Result<Enrollment>`, `poll_enrollment(&self, enrollment: &Enrollment) -> Result<EnrollmentStatus>` and `authenticate(&self, identity: &DeviceIdentity) -> Result<AccessToken>`. `Enrollment` includes display code, browser URL, private polling verifier and expiry; never serialize the verifier into the URL.

- [ ] Write failing tests using a test-only OIDC issuer/token/JWKS fixture for valid login, wrong signature/audience/issuer/nonce, expired token, reused state, stolen polling ID and mismatched device proof. Assert that different Google subjects sharing a supplied email remain different owners.
- [ ] Implement Google discovery and authorization-code exchange with a standard OIDC client, state/nonce/PKCE and confirmation of device display code. Persist users by `sub`, devices, consumed enrollment transactions and revocation status in SQLite. Use test dependency injection rather than production auth-bypass flags.
- [ ] Implement POST `/v1/enrollments`, browser `/auth/google/start` and `/auth/google/callback`, explicit browser confirmation, POST `/v1/enrollments/{id}/poll`, device challenge/authentication, GET `/v1/devices` and DELETE `/v1/devices/{id}`. Require valid device ownership for list/revoke; hash enrollment verifier at rest and consume one-time challenges atomically. Provide Keychain and permission-restricted file storage in the device crate, with platform-gated code.
- [ ] Run `cargo test -p remotevnc-backend -p remotevnc-device`; assert enrollment survives backend restart, credentials survive application restart, consumed OAuth transactions cannot replay and another account cannot list/revoke device IDs. Ensure logs redact all credentials and submitted text.
- [ ] Commit `feat: authenticate Google accounts and enroll persistent devices`.

### Task 6: Session leases, signaling and Cloudflare TURN

**Files:** backend `src/{sessions,signaling,turn}.rs`; `migrations/002_sessions.sql`; backend `tests/{sessions,signaling,turn}.rs`; extend `crates/device/src/api.rs`.

**Interfaces:** Produce `SessionGrant { session_id, client_id, agent_id, lease_expires_at, ice_servers }`, `SessionService::create(&self, caller: DeviceId, agent: DeviceId) -> Result<SessionGrant>`, `renew(&self, caller: DeviceId, session: SessionId) -> Result<SessionGrant>`, `close(&self, caller: DeviceId, session: SessionId) -> Result<()>`, `TurnClient::issue(&self, ttl: Duration) -> Result<Vec<IceServer>>`. Extend DeviceApi with typed session, device event and signaling methods. Persist ownership, not just device IDs in an in-memory connection table.

- [ ] Write failing tests for cross-account session/signaling/credential requests, concurrent clients racing for one agent, revoked credentials on existing WebSockets, and signaling that races with revoke. Tests require one winner and no forwarded payload after revocation commits.
- [ ] Implement POST `/v1/sessions`, lease renewal/close and authenticated `/v1/events` WebSocket signaling, validating sender, receiver, session, expiry, message size and rate on each message. Use a 15-second renewal cadence and 60-second grant lifetime; expire old grants after backend restart. Authenticate sockets without putting bearer secrets in query strings.
- [ ] Implement the documented Cloudflare credential API behind TurnClient with backend-only secrets, bounded timeout, response validation and error handling. Use a 3,600-second TURN credential TTL, request renewal 300 seconds before expiry for authorized sessions, and carry refreshed ICE config to both peers. Do not issue TURN credentials to anonymous or other-account callers.
- [ ] Run backend tests against a local mock TURN HTTP endpoint, asserting URL/body/auth and failure cases without printing tokens. Credential TTL and session lease are independent; no Cloudflare credentials means clear TURN-unavailable status, never a claim of relay support tested live.
- [ ] Commit `feat: authorize remote sessions and provision Cloudflare TURN`.

### Task 7: Ubuntu boot service and end-to-end agent

**Files:** `apps/agent/src/{main,config,service,helper}.rs`; agent `tests/{service,lease,helper}.rs`; `packaging/ubuntu/{remotevnc-agent.service,remotevnc-seat-helper.service}`.

**Interfaces:** Consume DeviceApi, Peer, InputGuard and helper IPC. Produce CLI `remotevnc-agent enroll`, `run`, `doctor` and `revoke-local`; `AgentService::run(self, shutdown: CancellationToken) -> Result<()>`. Config contains backend origin, identity path, helper socket, seat and optional relay-only diagnosis; no arbitrary capture pipeline from the network.

- [ ] Write failing service tests with fake helper/OIDC fixture for boot without a user session, reconnect/backoff, renewal at 15 seconds and release/stream-stop at 60 seconds after last valid lease. A helper crash, focus-release and revoked device must also release held input and stop delivery.
- [ ] Implement outbound event/signaling connection, authenticated session grant handling, capture-to-Peer path, InputGuard checks and bounded helper IPC. Tie helper control to a current lease; reject input before authorization even if a data channel is open. Reconnect through fresh authenticated grants and ICE rather than replaying input.
- [ ] Add systemd services with dedicated network-agent UID, private state directory, restricted helper socket and restart behavior. Helper retains only required local display privileges; configure `RestrictAddressFamilies=AF_UNIX` for helper while preserving GStreamer local IPC requirements, and verify the chosen sandbox permits X11/capture.
- [ ] Run `cargo test -p remotevnc-agent`; use paused Tokio time for lease boundaries. On Linux run `systemd-analyze verify` for both units and a process-level fake-helper disconnect check. No test may switch the developer machine's display manager.
- [ ] Commit `feat: run unattended Ubuntu remote access from boot`.

### Task 8: macOS application and committed IME input

**Files:** `apps/client/src/{main,app,input,video,autostart}.rs`; client `tests/{input,viewport,session}.rs`; `packaging/macos/{Info.plist,build-app.sh}`.

**Interfaces:** Consume DeviceApi, Peer and InputEnvelope. Produce `ClientApp` for eframe, `InputMapper::map(&mut self, event: ClientInput, viewport: Viewport, display: DisplayInfo) -> Vec<InputEvent>`, latest-frame receiver, explicit sign-in/out and `set_autostart(enabled: bool) -> Result<()>`. Keep app model/testable mapping separate from native UI callbacks.

- [ ] Add failing tests for Retina scaling, letterboxed edges, drag release outside viewport, focus loss, Command/Control mapping, IME composition versus commit, duplicate text/key events and generation changes. Assert exactly one `Tiếng Việt` commit, no composition fragments and a release-all on focus loss.
- [ ] Implement sign-in/browser confirmation, device list, selection, connect/disconnect and statuses for busy/offline/reauthorization/display transition. Poll enrollment using verifier and persist only application/device credentials in Keychain.
- [ ] Render newest decoded frame into egui without accumulating old frames; map pointer coordinates to remote pixels and include position on clicks. Integrate macOS IME commit events, shortcuts and held-key cleanup. Show measured stats and selected ICE candidate type; add an opt-in LaunchAgent for macOS login startup without screen recording/accessibility permission requests.
- [ ] Run `cargo test -p remotevnc-client` and `cargo build -p remotevnc-client`; launch the app locally and verify login/device views and window input behavior. Real screen/IME/Keychain/WebRTC interaction with Ubuntu remains a separate manual acceptance, not a screenshot-only test.
- [ ] Commit `feat: add the macOS remote desktop client`.

### Task 9: Installers, backend deployment and reproducible builds

**Files:** `packaging/ubuntu/{install.sh,rollback.sh,lightdm.conf}`, `packaging/backend/{Dockerfile,compose.yaml,env.example}`, `scripts/{build-linux.sh,check.sh}`, `.github/workflows/ci.yml`, `README.md`, `docs/setup/{ubuntu,macos,backend}.md`.

**Interfaces:** Ubuntu installer supports `--prepare`, `--apply` and `--dry-run`, backs up prior display-manager configuration and emits rollback location; it does not change display manager in prepare/build/test modes. Backend deployment expects HTTPS public origin, OAuth client/secret and TURN key from environment/secret files; expose its Rust service only through configured ingress.

- [ ] Add failing installer checks in a disposable Linux root/environment: dry-run makes no changes, prepare cannot restart display manager, invalid prerequisites fail before writes, backup/rollback restore previous settings and secret files are private. Test production backend rejects absent/invalid auth configuration.
- [ ] Implement Ubuntu dependency installation and explicit LightDM/Xfce/greeter/X11-locker setup, service user/state setup, doctor and rollback; do not reboot automatically. Install required GStreamer plugins and choose software encoder fallback when hardware probing fails. Document local enrollment before reboot.
- [ ] Implement macOS app bundle/dependency build instructions, Ubuntu native build script and single-instance SQLite backend image/compose example. Do not label unsigned local builds as notarized packages. Keep generated app bundles, DBs, recordings and secrets outside git.
- [ ] Add CI jobs for Rust fmt/clippy, protocol/backend/device tests, Ubuntu native dependencies plus Xvfb/media tests and macOS client build; test explicit target-gated modules on their native platforms. Run `scripts/check.sh` and `bash -n` on scripts; record Linux checks not runnable on macOS rather than marking them passed.
- [ ] Commit `build: package services and document deployment`.

### Task 10: Integration, acceptance evidence and review

**Files:** `tests/e2e/`, `scripts/test-e2e.sh`, `docs/testing/{acceptance.md,results.md}`; targeted fixes to files from Tasks 1–9.

**Interfaces:** Test harness orchestrates backend, agent/helper and client transport against test-only OIDC issuer and test display. Real acceptance uses actual Google OAuth, Cloudflare TURN and Ubuntu physical screen; no permanent debug login or permissive flags in production binaries.

- [ ] Write failing process-level checks: account A connects through WebRTC and types exact Unicode into a receiver; account B cannot enumerate or signal; revoke during a running session closes access; helper/backend restart cannot leave input pressed. Use existing task fixtures rather than production bypasses.
- [ ] Run local integration, fix failures at their shared cause and rerun only affected checks plus the final suite. Exercise loopback media and channel flow with real GStreamer, not a mock returning connected.
- [ ] On provided Ubuntu hardware, record boot → physical login → desktop → lock/unlock → logout/login → reboot. Verify physical monitor matches client view, input works at greeter/locker and no separate virtual desktop was created. If hardware/SSH is unavailable, list each case as pending with its exact command/procedure.
- [ ] With provided credentials, record real Google enrollment and different-account denial; force Cloudflare relay and verify selected candidate type, TURN renewal and recovery. Record CPU/GPU/encoder/resolution/FPS/RTT, queue timing and observed input-to-image p95; target LAN p95 below 100 ms on capable hardware, without treating RTT as that measurement. Mark missing credentials/results pending, never completed.
- [ ] Run fresh native build/tests/fmt/clippy and git diff checks; obtain the review required by the execution workflow, fix actionable issues and commit. Final delivery separates implemented/build-tested behavior, hardware results and still-required deployment inputs.

## Execution and External Inputs

This plan implements one integrated remote-control product; individual tasks each have a runnable test target. Native execution in the current session is recommended because protocol, helper, transport and agent share evolving interfaces, and it avoids repeating large contexts across implementers. Subagent execution is also available if explicitly selected by the user.

No implementation begins until the user reviews this plan and selects an execution method. During implementation, complete all local authorized code/build work without waiting for remote credentials. Ask for Ubuntu SSH, backend/domain and Google/Cloudflare configuration only when needed for real deployment/acceptance; request secret delivery through environment/secret files, not repository commits or tool output.
