# Validation results — 2026-10-21

These are native build/test results for the Rust implementation. Physical Ubuntu
and live Google/Cloudflare acceptance remain pending. SSH works, but the physical
host runs Ubuntu 25.10/GDM/Wayland without passwordless sudo; the installer requires
Ubuntu 26.04 and the reviewed LightDM/Xfce/X11 activation. No physical OS, package,
display manager, reboot, routing, ingress, privacy or autostart change was made.

## Implemented behavior and regression evidence

The explicit `tests/e2e/` harness uses signed fixture OIDC with real nonce/PKCE,
code exchange, confirmation and device proof. Separate backend, actual agent CLI,
native controller Peer, helper/capture and guardian processes use authenticated
Xvfb and GTK. Ordinary workspace tests keep privileged/display fixtures ignored
and the role-based fixture executable inert. Production has no test login or
discovery bypass.

The receiver compares exact `Tiếng Việt|é|é|中文|😀` code points, including distinct
NFC/NFD text, without clipboard use. Account B can enumerate only its own device
and presence, cannot create A's session, and cannot signal into established A
control. The denied signal must leave A's held Shift/left-button untouched.
Controller/helper/backend process loss and live revoke must release held input
and leave the old session nonrenewable; fresh sessions must work after restart.

Renewal dispatch now stays on a request-start 15s cadence after delayed success.
Its regression first measured 18.064s spacing after a 3s successful response delay;
the fix requires 14–16.5s spacing and independently 55–58s remaining helper authority.
Task1's 59999/60000ms expiry boundary was already fixed in Task3 and remains tested.
Task2's actual-UID ancestor traversal was fixed/reviewed in Task10.

Real capture also exposed an initial-delta transport NotReady teardown. Transport
now requests one fresh config-bearing IDR through its existing atomic/event path,
and the agent waits under unchanged authority/negotiation gates. The focused test
feeds 4 deltas, requires exactly 1 request, then verifies config-IDR resets it.

A subsequent disposable 250ms agent scheduling pause causally exposed encoded
queue saturation closing the helper stream. Recovery now retains queue2/control32,
keeps safe queued references FIFO, drops dependent encoded deltas until SPS/PPS+IDR,
and requests a fresh keyframe. One absolute 20s continuous-recovery deadline cannot
be extended by repeated overflow, requests or profile changes. Only successful
Peer acceptance of the current generation's recovery IDR clears it; unread old
IDRs cannot. Control saturation, malformed IPC, EOF, authority and native errors
still fail closed. The existing pre-encode stale-frame queue remains bounded.

The agent now samples actual Peer counters plus helper encoded-drop counts at 1Hz
through the existing AdaptiveProfile policy and applies lower resolution/FPS/
bitrate with the existing scoped UpdateProfile request. The native service test
requires this update while input is held, then proves release/video/15s renewal;
existing real capture tests cover profile updates preserving generation and PTS.
The real E2E pause requires freshly decoded advancing source PTS, held input and
session authority to survive at default 1920x1080@60fps; fixture quality is not lowered.

## Final native matrix

The final code/test source is commit `92a8187a10fa9861fb0f07a0c24f6ff0800eea48`,
Git tree `b1ba128d9ef652722d4ee5e0e32f29dd372450b9`. Production source was frozen
at `adf6de88d07373951b3da876eba797380e2d8aec`, tree
`bbbf819e41dc74f6c394f1d5ab32608ca6d83d1f`; the sole subsequent code/test delta
corrects the renewal observation origin in `apps/agent/tests/service.rs`.
It records before HTTP/native setup, preserving the separate 23s native-loop
budget, 14s renewal lower bound, independent helper deadline interval and delayed-
success 14–16.5s dispatch checks. Production timing is unchanged.

Every final run selects Rust 1.95.0, Cargo.lock and one compiler job. Mac/Lima
completed against the frozen production tree. AMD64 uses exact Git archives and
covers the test-only correction plus all previously unfinished phases. Passing
Mac/Lima suites were not repeated for that fixture-clock-only delta. The later
results/setup documentation commit changes no tested code.

| Environment/check | Result |
| --- | --- |
| macOS 26.3 ARM64 `scripts/check.sh` | 142 Rust tests passed, 0 failed, 10 explicit ignores; fmt, all-target/all-feature clippy, maintenance 3, shell/plist and host whitespace checks passed. |
| macOS normal `.app` refresh/startup | Release build passed 44.14s; normal copied app survived 5s with fresh registry/no terminal plugin or DYLD variables. Bundled nice signature verified; external scanner warnings retained. |
| Ubuntu 26.04 ARM64 Lima | Workspace 142 passed/0 failed/31 explicit ignores; fmt/all-target/all-feature clippy/maintenance/shell checks passed. Real process E2E 1 pass, 63.33s; X11 suite 13 passes. Agent/helper release passed 105s after disposable-VM tools setup. |
| Ubuntu 26.04 AMD64 disposable Docker | Workspace 142 passed/0 failed/31 explicit ignores, deduplicating successful pre-failure targets and corrected-service/remaining-package completion; fmt/all-target/all-feature clippy passed. Real E2E 1 pass, 109.15s; X11 suite 13 passes; agent/helper release passed 158s. |
| Offline-systemctl installer fixture, Lima | 7 passed/0 failed before the adaptation change; actual agent UID traversal/ownership, Debconf and runtime-only enablement rollback. No live manager activation; installer source unchanged. |
| Actual unit parser, Lima | Exit0 for all 3 project units; 2 external distro xfs warnings remain visible. Unit source unchanged. |

Workspace ignores are not passes. Source archives/read-only guest Git metadata
cannot run the host whitespace check; host `git diff --check` is recorded separately.
Container tests do not establish physical seat0 or a running systemd manager.
Existing Tasks7/8 separately exercised actual sandbox/cgroup cleanup with the
independent guardian; those unit policies are unchanged.

Final production-source fault observations:

| Fault/action | Ubuntu ARM64 Lima | Ubuntu AMD64 Docker |
| --- | --- | --- |
| Established controller SIGKILL, no Signal::End | 28.826s | 27.570s |
| Helper group SIGKILL, independent guardian | 23.27ms | 79.56ms |
| Backend SIGKILL | 10.024s | 10.076s |
| Live agent revoke during a fresh session | 68.95ms | 407.34ms |

Each observation includes native held-input release and nonrenewable closed
access; backend restart rejected the old session, and revocation denied a fresh
session. Backend fixture setup was 37.734s initial/25.008s restart on AMD64, within
the separately bounded 120s setup allowance.

The default 1080p60 scheduling-pause/advancing-decoded-PTS check and unchanged
Unicode/account-denial/session-survival assertions passed in the same run.
Controller loss is observed native detection around 28–29s, separate from the backend
10s outage limit. Neither is healthy input-to-image latency or a hard real-time
promise. Cleanup kills only private process groups before reap and never signals
a numeric group after reaping. The E2E helper group includes its capture child;
Task7's actual single-helper-main loss and map restoration remain separate proof.
Simultaneous helper and guardian loss while X remains alive is unsupported.

## Artifact and warning provenance

Artifacts below use production source adf6de8/tree bbbf819. The later renewal
observation-origin test change does not affect binary source. Build manifests
record toolchain/native versions; ignored run-specific sidecar
`target/release/task11-macos-source-provenance.json` binds the Mac source/tree and
byte hashes. `SOURCE_DATE_EPOCH=1792541164` identifies the production-source commit.

| Native artifact | SHA256 |
| --- | --- |
| macOS client executable | `e2e3f56e90b8638745fe3a189b1868ad2635c2522e953eb7927d846583a66b1c` |
| macOS Info.plist | `7c99c50714b4ed8d3e2fb0d4299c220bb9e49b0862db300ea324389e70332b07` |
| macOS bundled nice plugin | `e8577f9b207dc671dae5f54677f0728d93ad100a66d8ddd45b51fa3ad765d238` |
| Ubuntu ARM64 agent | `42b1a040220c86e4bd3c3698b99215e45de7f2c6658ac9f131448fab3f6f3b6a` |
| Ubuntu ARM64 helper | `0103465073745d055af1130d3b326fb3c6576dfc21942f2674ca05e3f854f548` |
| Ubuntu AMD64 agent | `c8da9e544ca590bb7c5206306d0d7a3bdfa37bedc5af2bb46be7bd5a90c91279` |
| Ubuntu AMD64 helper | `c68344a32daf00597791ed2da99cf30adb98be1dc37c67075d00ffdd68aeca57` |

Lima initially lacked the gst-inspect executable; release preflight stopped before
compilation. Installing only gstreamer1.0-tools inside that disposable VM resolved
it; only release was resumed. No passing suites were repeated. The normal macOS
development app remains Homebrew-dependent and unsigned/ad-hoc, without Developer ID signing/notarization or portable runtime.
Its fresh-registry launch is tested outside the checkout with terminal plugin/DYLD
variables removed; no privacy or LaunchAgent setting is changed.

The existing isolated backend image is from source 69a627e, not labeled a final-source
image. Backend runtime is unchanged; it was not rebuilt or publicly deployed.
A copied build-context Cargo metadata check without root `tests/` resolves the
changed agent manifest, matching the backend Dockerfile's source layout.

T8-M3: `systemd-analyze verify` identifies removed/ignored CPUAccounting in
`/usr/lib/systemd/system/xfs_scrub_all.service:26` and `system-xfs_scrub.slice:15`.
These distro-owned warnings were not suppressed by package/unit/global changes.

T10-M1: fresh-registry Homebrew scans can emit GIRepository libgobject/libglib
basename lookup warnings, missing Python Gst GI namespace and duplicate GTK3/GTK4
Objective-C classes. Required bundled nice/factories and normal app survival are
checked separately. Native stderr is not claimed pristine; no global loader,
plugin blacklist or privacy setting hides these external scanner warnings.

## Historical runs and diagnostic limits

The pre-adaptation source 43aa8b0/tree10dbe6e passed 138 tests on each of macOS ARM64,
Ubuntu ARM64 and Ubuntu AMD64, plus the then-current Lima process/X11 checks.
Its normal app rebuild/committed-source refresh had identical client/plist/nice
hashes. Those matrices and bytes are historical, superseded by the final source
above. Earlier Tasks1–10 partial1.95/1.94 runs are also historical.

The first AMD64 process run passed account/Unicode/controller/helper/backend fault
assertions, then hit the old 30s backend-restart setup wait. Its child logs were lost
with the disposable container: the precise original restart cause is not proven.
An isolated successful bootstrap diagnostic measured 22–26s fresh RSA preparation;
later retained runs directly observed 32.27s and 42.30s successful setup. Discovery
and bind took milliseconds. Backend fixture setup therefore has a 120s allowance,
phase timings and prompt child-exit detection. Controller/helper/revoke/backend-
outage, HTTP, negotiation and authority limits were unchanged.

The fixture-only tree 86a6dd4 then failed `denied B must not disturb A` after A's
controller ended, but logs did not identify its close cause. The unchanged-media
close-diagnostic run passed. The separate bounded scheduling-pause probe proved
the encoded-overload teardown path; attributing the earlier spontaneous failure
to that cause, or to B's denied signal, remains unproven. Assertions were retained.
No broad matrix was repeated for fixture setup alone; the final repeat is justified
by the subsequent production recovery/adaptation changes.

The first post-adaptation AMD64 workspace run stopped at a service assertion that
measured renewal from after native initialization. Its actual elapsed value was
not retained, so premature production dispatch was not established. One targeted
instrumented service run passed 4 tests/1 ignore, 72.85s: grant-dispatch elapsed 15.08458s,
late-origin 15.03461s and helper request-start deadline interval 15005ms. The static
observation-origin defect was corrected to record before create_session while
retaining the independent 23s native-loop budget and all renewal bounds. The
corrected service target and previously unexecuted AMD64 phases completed
successfully. The deduplicated workspace count is 142 passes/0 failures/31 ignores:
11 successful pre-service tests plus 131 corrected-service/remaining-package tests;
the failed original service summary and diagnostic repeats are excluded.
Unchanged passing Mac/Lima suites were not repeated.

## Required external acceptance — pending

Follow [the exact acceptance procedures](acceptance.md) after reviewed activation
on supported 26.04 hardware. SSH is available; physical admin activation and paired
live testing are the gate.

| Required evidence | Status |
| --- | --- |
| Physical boot/greeter/login/desktop/lock/unlock/logout/login/reboot, selected seat0 monitor, no extra desktop, suspend | Pending supported OS/admin activation/on-site observer. Xvfb is separate evidence. |
| Real Google enrollment/consent/account denial/revoke and public proxy lifetime | Pending replacement Google secret rotation and reviewed shared/dedicated routing; private live OAuth JSON was not read/used. |
| Real Cloudflare selected relay, credential renewal and recovery | Pending real TURN key and approved deployment. Fixture TURN/native policy is not real relay proof. |
| Interactive macOS Keychain/IME/LAN permission/login LaunchAgent | Pending interactive acceptance and production signing identity; no privacy/autostart changes made. |
| Actual encoder/resolution/decodedFPS, CPU/GPU, bytes/s/RTT/queue timing, input-to-image p95 | Pending hardware measurement. LAN p95<100ms is an unmeasured target; RTT is not that metric. |
