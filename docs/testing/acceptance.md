# Acceptance procedure

This matrix separates local signed-fixture/native evidence from physical and live
provider acceptance. Use Rust 1.95.0 and `--locked`. Never use production auth or
physical-display bypasses. Keep logs private; do not collect identity.json, OAuth
JSON, bearer tokens, SDP, ICE addresses or TURN passwords in shared evidence.

## Repeatable local checks

On macOS select the isolated Rust 1.95 prefix and source `.deps/media/env.sh`;
on Ubuntu select `RUSTUP_TOOLCHAIN=1.95.0`. Keep `CARGO_BUILD_JOBS=1` on the
4GiB validation machines. Run:

```sh
scripts/check.sh
# Linux disposable display only:
scripts/test-x11.sh
scripts/test-e2e.sh
# Root in the disposable VM/container, never against the physical desktop:
sudo python3 scripts/test-ubuntu-installer.py
```

`test-x11.sh` includes the explicit real-capture four-transition hot-profile
regression. Valid 320x180@10/512kbps and 240x134@7/384kbps profiles must produce fresh
config-IDR and actual new decoded dimensions while source PTS advances, display
generation stays fixed and real held Shift/left-button survives until release.
The existing 60fps cropped native RTP test remains separate.

`test-e2e.sh` creates authenticated Xvfb and a GTK entry, then runs separate
backend, agent CLI, controller native Peer, helper/capture and input guardian
processes. Test-only discovery selects Xvfb through the existing library API;
production display discovery and OAuth behavior are unchanged. The backend uses
the existing signed OIDC issuer with nonce/PKCE/exchange/confirmation and device
proof. Account A must decode real H264 and send reliable-channel Unicode to GTK:
`Tiếng Việt|é|é|中文|😀` (the second accented e is NFD). The receiver compares
exact code points and rejects duplicate commits. Account B sees only its own
device/presence and cannot create or signal into A's session. A bounded 250ms agent
scheduling pause must preserve held input/session authority and restore freshly
decoded source timestamps at the default 1920x1080@60fps capture profile. Focused
service coverage separately uses measured IPC drops to require adaptive quality
updates. Before the process test, two explicitly ignored private Live cases
block actual native appsrc downstream after DisplayAck at controlled 320x180@10fps:
native drops must increase with helper drops 0; fresh decoded PTS/held input must
recover before the bound, and a persistent gap must close/release at 20s despite
successful renewal, ACK/keyframe and applied profile updates. A dev-dependency-only
RAII pad guard removes its probe on every exit and is absent from normal release
feature sets. This is separate from authenticated HTTP renewal ingress coverage.
Helper and transport config-bearing-IDR recovery share one absolute 20s episode;
only successful current-generation/revision-valid config-IDR acceptance clears it.
Repeated rejection, profile changes, ACKs, keyframes or leases cannot extend it.
The bounded 5s native-drop onset setup precedes the 21/24s observation windows;
none changes the production 20s budget.
Backend fixture setup has a 120s allowance for fresh RSA preparation, phase logs,
and prompt child-exit detection; native failure/authority bounds are separate.
The harness kills the established controller without Signal::End, kills/restarts helper and
backend separately while Shift/left-button are held, verifies native X11 state
release and nonrenewable closed sessions, reconnects with fresh sessions, and
revokes the agent during live control. Ordinary workspace tests leave these
fixtures inert/ignored. No installed units, display manager or privacy settings
are changed. The script prints a private `/tmp` results directory; retain only
its nonsecret logs, then remove that exact owned directory when finished.

## Physical Ubuntu activation gate — pending

SSH works to `clawmachine@172.22.8.59`; read-only preflight found Ubuntu **25.10**,
GDM49/Wayland, AMD Raven/Vega, Xfce/LightDM packages and no passwordless sudo.
This is not an accepted Ubuntu 26.04/X11 deployment. No packages, display manager,
OS, reboot, ingress or routing change is authorized by these test instructions.
The installer deliberately rejects 25.10. An administrator must first provide a
reviewed supported 26.04 installation/host and approve activation using
[the concrete install/rollback procedure](../setup/ubuntu.md). Do not bypass the
version check, use privileged Docker as sudo, or remotely replace the active DM.

Before activation collect read-only baseline:

```sh
ssh clawmachine@172.22.8.59 'cat /etc/os-release; uname -r; loginctl list-sessions; readlink /etc/systemd/system/display-manager.service; systemctl is-active display-manager; sudo -n true'
```

On the approved26.04 host, prepare binaries/units, enroll as the actual
`remotevnc-agent` state owner, save the printed rollback path, review `--doctor`
and apply, using the linked procedure. An on-site administrator must preserve
local console access and explicitly perform the reboot. Never log a password or
film real credentials. After that gate, record each case separately:

| Case | Procedure and required observation |
| --- | --- |
| Boot → physical greeter | Administrator reboots; `systemctl is-active remotevnc-input-guardian remotevnc-seat-helper remotevnc-agent`; paired client connects before user login. On-site observer confirms monitor/client show the same physical LightDM screen on seat0. |
| Login → desktop | Select Xfce/X11 explicitly for the existing user. Enter credentials remotely without clipboard; local Ubuntu authentication remains authoritative. `loginctl show-session SESSION -p Seat -p Type -p Remote -p Display` must report local seat0/X11. `xrandr --query` identifies the chosen physical output. Compare a visible local marker against the client; no extra desktop/session is created. |
| Lock → unlock | In the local Xfce desktop run `pgrep -x light-locker`, then `light-locker-command --lock`. Compare actual monitor/client and enter a disposable account password remotely. Verify active modifiers/Caps Lock and no clipboard mutation. Unlock must use Ubuntu's own locker. |
| Logout → login | Use Xfce's normal Log Out action; compare physical greeter, authenticate again, verify display generation changes and stale pointer/input cannot apply to the old session. |
| Reboot/reconnect | Explicit administrator reboot; repeat pre-login control and same-output comparison, then fresh login. Automatic agent startup must require no interactive application launch. |
| Input/recovery | Hold a remote modifier and mouse button, kill the controller process, revoke the agent from the paired app, and separately restart the approved helper/backend. Record release and closed-access times; do not kill helper and guardian together. Repeat with a local physical source overlapping the remote press, and verify it remains down until locally released. |
| Suspend | Explicit administrator suspend/resume; verify CLOCK_BOOTTIME lease expiration prevents stale input/stream after60s. Paused clocks and VM tests do not prove physical suspend. |

X11 query/release and conditional map/lock restoration are not atomic against
new simultaneous local input/settings changes. A responding X server is needed;
simultaneous loss of helper and guardian while X remains alive is unsupported.
Never claim these residual windows were eliminated by stable-overlap tests.

## Live Google, Keychain and IME — pending

The owner must rotate the previously exposed Google secret and deliver replacement
credentials through private files/secret management. Do not read or use the
existing private OAuth JSON for fixture tests. Resolve dedicated/shared ownership
of `https://amio.vnoffice.io.vn` before any deployment; the callback remains
`https://amio.vnoffice.io.vn/login/callback`. Follow the additive
[backend deployment](../setup/backend.md); fixture OIDC does not validate Google
consent, real browser behavior or public proxy lifetime.

Launch the normal macOS app using `--backend https://amio.vnoffice.io.vn`; enroll
both devices with the same Google account and verify device-code confirmation.
Restart the app and confirm it recovers the existing Keychain identity without
re-enrollment; deny Keychain access and verify fail-closed behavior without a
file fallback. In a separate macOS user/account, enroll a different Google
account and verify it cannot enumerate/connect/signal the first account's
agent. Revoke during control; verify video/input stop and reconnection is denied.
Try consent cancellation, stale/replayed callback and owner-selected client
device recovery. Never reset the user's Keychain or remove existing credentials.

With Vietnamese IME active, type repeated identical commits, NFC/NFD, CJK and
non-BMP text into an ordinary entry and a disposable password entry on Ubuntu;
compare exact receiver text where safe. Exercise Shift/Caps Lock/Command and
Cmd+C/X/V without changing either clipboard. Native mapping fixtures prove event
handling, not the OS IME/Keychain dialogs. For LAN permission use the normal app
from Finder on the same LAN and record the actual permission prompt/result.
A reliable production permission identity requires Apple-issued signing; no
certificate/notarization has been supplied. Do not reset TCC, disable Gatekeeper
or grant unrelated Accessibility/Screen Recording permissions. Login LaunchAgent
acceptance is explicit user opt-in only, from a stable installed app path.

## LAN/real Cloudflare relay and latency — pending

Provide the real Cloudflare TURN key privately and reviewed public routing before
this run. First establish direct LAN control and record the UI's selected
local/remote candidate types and RTT. Then start the client with
`--backend https://amio.vnoffice.io.vn --relay-only`; configure the enrolled agent's
`relay_only: true` via the approved administrator procedure and restart only its
agent service. Confirm **selected** local/remote candidate stats show relay;
a configured TURN URI or successful fixture response is insufficient. Keep the
session beyond the returned credential renewal point, verify a new generation
and fresh media after credential rotation, then interrupt/recover the network
and verify a fresh display acknowledgement/no stale input. Test idle public
proxy EOF/reconnect separately; loopback is not that evidence. Restore reviewed
nonsecret relay policy when the acceptance run finishes.

Record host/OS/kernel/GPU (`lspci -nnk`), actual selected encoder, physical/captured
resolution, observed decoded FPS, CPU (`pidstat -u -p AGENT,HELPER,CAPTURE 1 60`
when already available), GPU load using an already approved platform tool,
bytes/s and candidate RTT. A configured30FPS profile is not observed30FPS.
Transport counters include input/frame drops and native queue depth. Queue-age
and capture/encode/decode timing require separate instrumentation; they are
distinct from end-to-end latency and may require a reviewed nonsecret acceptance build if
not exposed by the normal UI. Do not enable verbose native logs containing SDP
or credentials to obtain them. Record unavailable metrics explicitly.

For input-to-image p95, use a high-speed camera that captures both physical
controller key actuation and the changed region in the displayed client image,
or a calibrated equivalent that accounts for clocks and display scanout. Use a
nonsecret test receiver toggling a high-contrast patch on each of at least100
separated inputs. For each input compute time from actuation to the first changed
client frame; retain sample count, camera FPS, raw timing samples and nearest-rank
p95 (`sorted(samples)[ceil(0.95*n)-1]`). Report LAN and relay independently with
measurement resolution. Target LAN p95<100ms on capable hardware. RTT, Xvfb
queue time and software logs alone are not input-to-image p95. No performance
claim is made until this measurement and resource observations exist.
