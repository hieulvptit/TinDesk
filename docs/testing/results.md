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
be extended by repeated overflow, requests or profile changes. After review I1,
transport-only QueueFull/NotReady also starts this same earliest-gap clock after
DisplayAck; healthy IPC, ACKs and renewals cannot mask a stalled downstream. Only successful
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

The controlled post-review capture probe also proved a separate live-profile
renegotiation failure: DOWN succeeded, then UP ended ximagesrc with
`not-negotiated (-4)`, worker exit1 and reader EOF. Capture now selects the
supported delayed caps-change mode so in-flight previous-format data remains
acceptable while the new caps negotiate. The same four transitions then passed.
This is consistent with [GStreamer's documented capsfilter behavior](https://gstreamer.freedesktop.org/documentation/coreelements/capsfilter.html).
The original AMD64 DOWN-adjacent held loss is not attributed to this cause.

## Current source and covering checks

The post-review I2 helper/test source is commit
`836669191498645ae4e448557b16c0ec31d01cd2`, tree
`cf3b9090e22ae7e8c52188643b278684b2a49d5e`. The agent runtime remains
`748b230a6d9fe98294af1a7920ddac792605d448`, tree
`ab1262fceac114bc5fc4cde84b0aab479d977fe8`; its I1 fix shares the existing
absolute 20s recovery clock between helper and transport rejection. Accepted
current-generation config-IDR plus the helper loss-revision check alone clears
it. No production timing, authority, input or profile invariant was relaxed.

| Post-I2 check (Rust 1.95.0, locked, one compiler job) | Result |
| --- | --- |
| macOS ARM64 affected helper/agent/transport | All-target/all-feature clippy with `-D warnings` and helper unit 4 passed. |
| Ubuntu ARM64 Lima affected paths | Scoped clippy, helper unit 4 and agent unit/helper/service 12 passed; service 43.15s. Existing physical capture 2.01s/cropped 60fps RTP 2.42s and new hot-profile regression 10.05s passed. |
| Ubuntu AMD64 affected paths | Scoped clippy/helper unit 4 and capture 3 passed (physical 2.15s/cropped 2.44s/new regression 10.09s). Normal native invocation: recovery passed, persistent isolation failed (helper drops 1). Separate instrumented persistent case passed in 21.91s, terminal 20.001512402s/helper0. Original signed process E2E passed in 102.11s; outcomes remain separate. |

The new ignored capture regression applies four literal DOWN/UP/DOWN/UP updates
between 320x180@10/512kbps and 240x134@7/384kbps. Each stage requires actual decoded
new dimensions, fresh config-IDR, advancing original source PTS, generation 7 and
actual held Shift/left-button; explicit release is verified at the end. The old
60fps cropped-RTP test and default 1080p60 process E2E are unchanged. The regression
failed old-code staged tree 41e2e6b in 5.85s when the worker closed its command pipe.

Earlier I1 coverage below used helper adf6de8 and unchanged agent748; current helper836
coverage is the post-I2 table above:

| Post-I1 check | Result |
| --- | --- |
| macOS ARM64 affected agent/transport | All-target/all-feature clippy with `-D warnings`, fmt, 8 ordinary agent unit/helper tests passed. Linux-only native cases are target-gated. |
| Ubuntu 26.04 ARM64 Lima affected agent/transport | All-target/all-feature clippy with `-D warnings`; 12 ordinary unit/helper/service tests passed (39.23s service target); 3 explicit ignores remain outside ordinary coverage. |
| Explicit private after-ACK native Live cases | 2 passed, 47.56s. Native encoded drops increased while helper drops stayed 0. Recovery restored fresh decoded PTS at applied profile dimensions and held input survived the original deadline. Persistent transport stall closed/released at 20.006695042s despite successful renewal, repeated ACK/keyframe and actual lower-profile updates. |
| Original signed-process E2E, default 1080p60 | 1 passed, 56.11s: 250ms scheduling pause, exact Unicode/account B denial, all process-loss/restart and live revoke checks. |
| Normal agent-only release feature audit | ARM64/AMD64 release manifests require empty agent and transport feature sets, excluding dev-only `native-test`; hashes/provenance below. |

The explicit script first runs the private downstream-block tests at a controlled
320x180@10fps to isolate transport pressure, then the unchanged signed-process
1080p60 acceptance. The test-only RAII appsrc pad guard always removes its probe
before Peer teardown, including panic. It is available through a dev-dependency
feature only, with no installed flag or auth/discovery change. Media observation
starts at the first actual native rejection: bounded 5s onset setup, then 21s
persistent/24s recovery observation. Recovery unblocks at 2s and proves survival
more than 20s after unblock; the production budget remains 20s from the first gap.
Trusted direct Live renewal exercises the private state machine; authenticated
HTTP renewal ingress remains covered by the ordinary service tests.

## Baseline whole-workspace matrix

The previous broad matrix covered runtime commit
`adf6de88d07373951b3da876eba797380e2d8aec`, tree
`bbbf819e41dc74f6c394f1d5ab32608ca6d83d1f`. AMD64 completion also covered the
renewal-observation-only test correction `92a8187a10fa9861fb0f07a0c24f6ff0800eea48`,
tree `b1ba128d9ef652722d4ee5e0e32f29dd372450b9`. That correction records before
HTTP/native setup and retains the separate 23s native-loop budget, 14s lower
bound, independent helper deadline interval and delayed-success 14–16.5s check.

These 142-test matrices remain baseline coverage for unchanged paths, not a new
whole-workspace run of 8366691. The scoped post-review checks above cover the changed
agent/transport/capture paths. Unchanged workspace/installer suites and client/
agent/backend builds were not repeated for the capture-only change. Runs select
Rust 1.95.0, Cargo.lock
and one compiler job. AMD64 uses exact Git archives. The later completion commit
changes documentation only.

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

Current component-source fault observations:

| Fault/action | Agent748/helper adf6de8 ARM64 Lima | Agent748/helper836 AMD64 |
| --- | --- | --- |
| Established controller SIGKILL, no Signal::End | 28.840815025s | 28.145969127s |
| Helper group SIGKILL, independent guardian | 24.432495ms | 78.957633ms |
| Backend SIGKILL | 10.012455743s | 10.066235202s |
| Live agent revoke during a fresh session | 46.468861ms | 448.355503ms |

Each includes actual held-input release and nonrenewable closed access. Backend
restart rejects the old session, and revocation denies a fresh session. Fixture
setup was 7.219477489s initial/4.150397906s restart on ARM64 and 30.039345455s/24.663595111s on AMD64, within the separate 120s bound.
The baseline broad-run observations were ARM64 controller 28.826s/helper 23.27ms/
backend 10.024s/revoke 68.95ms and AMD64 controller 27.570s/helper 79.56ms/backend 10.076s/
revoke 407.34ms; AMD64 setup 37.734s initial/25.008s restart.

Controller loss remains observed native detection around 28–29s, separate from
the backend 10s outage limit. Neither is healthy input-to-image latency or a hard
real-time promise. Cleanup kills only private process groups before reap and
never signals a numeric group after reaping. The E2E helper group includes its
capture child; Task7's actual single-helper-main loss and map restoration remain
separate proof. Simultaneous helper and guardian loss while X remains alive is
unsupported.

## Artifact and warning provenance

Agents below are refreshed from post-I1 commit 748b230/tree ab1262f with
`SOURCE_DATE_EPOCH=1792544243`. Run-specific release sidecars
`task11-fix1-agent-provenance.json` and Cargo JSON artifact records contain exact
source, SHA256 and normal agent/transport feature arrays (both empty). ARM64
release passed in 42.44s; AMD64 release passed in 2m59s.

Helpers are refreshed from 8366691/tree cf3b909 with
`SOURCE_DATE_EPOCH=1792545632`; release/task11-fix2-helper-provenance.json records
normal helper features and unchanged agent748 bytes. ARM64 release passed in 21.12s;
AMD64 passed in 1m37s. Unchanged client artifacts retain runtime source
adf6de8/tree bbbf819 and `SOURCE_DATE_EPOCH=1792541164`.
Build manifests record toolchain/native versions; ignored
`target/release/task11-macos-source-provenance.json` binds the actual Mac source
and bytes. The later docs-only commit is not an artifact source.

| Native artifact | SHA256 |
| --- | --- |
| macOS client executable | `e2e3f56e90b8638745fe3a189b1868ad2635c2522e953eb7927d846583a66b1c` |
| macOS Info.plist | `7c99c50714b4ed8d3e2fb0d4299c220bb9e49b0862db300ea324389e70332b07` |
| macOS bundled nice plugin | `e8577f9b207dc671dae5f54677f0728d93ad100a66d8ddd45b51fa3ad765d238` |
| Ubuntu ARM64 agent | `8c2ed689493cff37a46f8ce841731a255ca3a9637e987dc2c60a7ebbd07c5f3d` |
| Ubuntu ARM64 helper | `1c8702a29834f7913452561e6fbcf201f5ffc744bda1e0c108f5d54ba56fad3f` |
| Ubuntu AMD64 agent | `f2ac9aa7499459d85489cab5625a730b2ff33861f1219203f4ce747f2cb57fda` |
| Ubuntu AMD64 helper | `0679776f1ede25190b2da7299f18977d81e1025c8b83ca3e11f9d19fdc8c6c7f` |

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
hashes. Those matrices and bytes are historical; the later baseline and scoped current
source checks above supersede their coverage. Earlier Tasks1–10 partial1.95/1.94 runs are also historical.

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

Review-I1 diagnostic limits: the initial 50.21s native run timed out because its
fixture clock included variable native drop onset; it is not behavioral RED.
A retained pre-fix run then proved persistent transport-only failure: 41 native
drops, queue2, helper drops0 and held input remained after 22.663s with successful
renewal/ACK/profile activity. Its recovery case had one unattributed held-input
loss without poll error; an unchanged focused diagnostic passed 24.22s. No cause
was established and no input/profile behavior or assertion was weakened. A later
instrumented clean GREEN passed both cases in 48.52s (terminal 20.000968783s); final
uninstrumented covering results are the 47.56s pair above. Temporary diagnostic
prints are absent from committed source. The earlier spontaneous B-survival
failure, lost-log bootstrap attribution and original premature-renewal allegation
retain their separate limitations above.

Post-I2 AMD64 diagnostic limits: the normal combined native invocation passed
recovery, then failed persistent-case transport isolation because one local
helper-reader drop was counted. A single unchanged-assertion diagnostic recorded
helper0 at setup/first actual rejection and throughout, closing/releasing at
20.001512402s despite successful renewal/ACK/profile activity. No slow-X11-query or
reader-full diagnostic occurred. The earlier drop's cause is not established;
no fixture profile, zero-helperdrop assertion or product deadline was changed.
The normal 1 pass/1 fail and instrumented persistent 1 pass are separate results, not
a native pair 2/2. Independent original process acceptance then passed in 102.11s.
The controlled UP-caps defect is addressed; original DOWN-adjacent termination
and the later isolated helper drop remain explicit attribution limits for review.

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
