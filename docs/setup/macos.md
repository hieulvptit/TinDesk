# macOS native development app

Install Apple's command-line tools and Homebrew, then Rust 1.95.0 in an isolated
prefix or via `rustup toolchain install 1.95.0 --profile minimal`. Select it in
this shell with `export RUSTUP_TOOLCHAIN=1.95.0`; no default-compiler change is
needed. From this checkout:

```sh
scripts/setup-macos-media.sh
source .deps/media/env.sh
scripts/check.sh
python3 packaging/macos/sign-app.py --setup
packaging/macos/build-app.sh
target/release/BinaChicken.app/Contents/MacOS/binachicken-client \
  --backend https://amio.vnoffice.io.vn
```

This is a locally certificate-signed **development bundle**, not a portable, notarized or
production-signed installer. The executable still needs the installed Homebrew
GStreamer/libnice runtime. The build copies the reviewed project-built gstnice
plugin, its licenses and a nonsecret build manifest into Contents/Resources.
It normalizes the plugin install ID and signs the nested plugin and app with
the same persistent local identity; this is not a Developer ID distribution package.
Before starting the UI, the normal executable loads that one fixed bundle-relative
plugin using [GStreamer's plugin API](https://gstreamer.freedesktop.org/documentation/gstreamer/gstplugin.html#gst_plugin_load_file)
and verifies the required factories. Other libraries/plugins use the native
Homebrew runtime's compiled paths. There is no startup environment mutation,
arbitrary plugin configuration or dependency on controller /tmp/checkouts.

Finder, `open target/release/BinaChicken.app`, direct executable and the opt-in
LaunchAgent all use this same initialization, without sourcing media env.sh at
runtime. The integration check removes terminal GST/DYLD variables and uses a
fresh registry, checks all factories and the actual bundled nice plugin path,
and rejects a missing bundled plugin. Do not move the bundle to another machine
and assume its external Homebrew libraries/plugins will be present.

The setup script installs only missing formulae with auto-update/upgrade off,
checks upstream libnice 0.1.24's archive SHA256 and builds its missing GStreamer
plugin against the already installed matching Homebrew libnice. It rejects
another libnice version until its source/checksum is reviewed. This avoids a
second ICE library. It retains sources/licenses under ignored `.deps/media`.
See [native dependencies](native-dependencies.md) for plugin and codec probes.
Existing brew versions are preserved, so the native environment is not a fully
locked dependency graph. Keep native-build-manifest.txt, Cargo.lock and artifact
hashes with any shared test artifact. Build on each target architecture; this
script does not produce a universal binary.

The bundle identifier is `io.binachicken.client`, with a local-network usage
description for direct Ubuntu connections. Apple recommends an Apple-issued
signing identity for reliable local-network permission identity; an unsigned or
ad-hoc executable cannot establish production-signed/LAN permission acceptance.
See [Apple TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy).
A distributable release still
needs a frozen/vendor-contained native runtime, corrected library/plugin lookup
paths, retained third-party notices, nested-code signing, Developer ID signing,
notarization and stapling, followed by clean-machine Gatekeeper/LAN checks.
Apple's [distribution guidance](https://developer.apple.com/documentation/security/distributing-your-app-for-beta-testing-and-releases)
describes that separate release process. Do not label a successful local build
as completion of those steps or disable Gatekeeper/TCC to simulate acceptance.

Local signing setup creates a private, non-exportable signing identity in
`~/.local/share/binachicken/signing/signing.keychain-db`. Its generated build-keychain
password and identity metadata stay outside the project in that private directory.
The login keychain remains the default; setup appends only the dedicated signing
keychain to the search list and configures user-domain code-signing trust only.
It does not change TLS/system trust or the app credential item's access controls.
Builds pin the `io.binachicken.client` identifier and certificate fingerprint in
the designated requirement. However, login Keychain also checks a partition ID:
local self-signed certificates have no Apple Team ID, so that partition follows
the executable's changing `cdhash` even with a stable designated requirement.
There is no automatic fallback to ad-hoc signing. Keep this signing identity for
future builds instead of regenerating it.

The app therefore uses `Contents/Helpers/binachicken-keychain`, a small signed
executable whose bytes are cached outside the repo and reused across client
rebuilds. It accepts only a direct parent with the client identifier and pinned
certificate, handles only the origin-scoped BinaChicken item, and transfers bytes
over private child-process pipes. It never prints credentials in diagnostics.
An absent/rejected bundled helper fails closed without a direct-access fallback.

On first migration, select **Always Allow** for **binachicken-keychain** in the
Keychain prompt. Subsequent client builds reuse its identity. Changing the helper
itself or the certificate may require new approval; a locked login keychain still
needs unlocking. Existing enrollment is retained.
`python3 scripts/test-macos-keychain-helper.py` uses uniquely named fixture items
in the real login Keychain with UI disabled: it reproduces direct access denial
across two different builds, proves access through the unchanged helper, and
rejects an untrusted parent/ad-hoc replacement. It deletes only its own fixtures.
`python3 scripts/test-macos-signing.py` separately verifies legacy ACL behavior in
an isolated temporary keychain; that test alone does not cover login partitions.
Neither test reads the production BinaChicken credential item. See Apple's
[designated-requirement guidance](https://developer.apple.com/library/archive/documentation/Security/Conceptual/CodeSigningGuide/Procedures/Procedures.html).

The normal package build uses `--no-default-features`; `native-smoke` and the
`ui_smoke` rendering fixture stay disabled. The app uses Google enrollment and
user Keychain service `io.binachicken.client`, account keyed by canonical backend
origin, without a file-secret fallback. No global input, screen recording or
Accessibility permission is needed to forward events from its own window. Test
interactive Google/Keychain, Vietnamese IME, local-network privacy and physical
remote input separately; a loopback/render fixture does not exercise them.

The client has three separate screens: Google sign-in, the computer list, and
the remote desktop. Continue with Google opens the browser automatically;
successful sign-in returns to the computer list without a device code. Connect
opens the desktop screen; Back to computers releases input and disconnects.
Connection statistics are in Connection details, while Open at login and Sign
out are in the computer list's Settings menu. The development rendering fixture
accepts `auth`, `devices`, or `remote` to preview each screen without credentials.

The remote desktop fills the available content area. Hover the small handle at
the top center to reveal a floating icon toolbar: disconnect/back, video settings
and fullscreen. Each icon has a descriptive tooltip.
The toolbar hides after leaving it and remains visible while a menu is open.
Toolbar/menu input is kept local, including the click that closes a popup.
Connection information and media receive/transmit speeds stay in a fixed
top-right overlay, without reserving layout height. Click it for ICE path,
RTT/loss where available, received FPS/resolution and local frame skips. Red
means the native connection is disconnected; green means direct P2P, blue means
TURN relay. A yellow/green split marks slow P2P, an orange/blue split slow relay.
Gray means connecting or waiting for the selected ICE pair. Slow/recovered state
requires three fresh consecutive reports, using RTT >250ms or interval RTP loss
>5%; a reporting gap resets that streak. A selected
host-to-host pair is direct P2P. Cloudflare signaling alone does not imply relay.
Video settings offers 15/24/30/60 FPS ceilings and 360p/480p/720p/1080p/source
(up to 4K) stream bounds, applied without reconnecting. The default is 1080p at
up to 30 FPS. Actual dimensions preserve the physical desktop aspect ratio and
never upscale it; mouse coordinates still refer to the physical display. The
host's adaptive profile may reduce FPS/resolution further under sustained load.
These preferences last while the app is running. Upgrade the Ubuntu agent
before using these settings: older agents do not implement the new request.
Connection details distinguishes local decoded-frame skips from RTP packet loss;
skips keep the newest frame instead of accumulating delayed images.
Speeds are observed RTP video byte-counter deltas, not an Internet speed test
or whole-interface throughput. They exclude SCTP keyboard/mouse traffic and
wire overhead. Missing counters, startup, resets or stale reports show a dash;
the app does not invent zero upload traffic when no outbound RTP exists.

The current workspace launcher is `target/release/BinaChicken.app`. Open that
bundle, not the bare `target/macos-auto-google/release/binachicken-client`
compiler output: a bare executable skips bundle-relative ICE plugin loading
unless its documented development environment is configured. Multiple older
copies share the bundle identifier; quit older running copies before opening
the current bundle (`open -n target/release/BinaChicken.app` explicitly starts it).
The media-runtime integration check now instantiates WebRTC and enables H264,
in addition to finding factory names, with shell media variables removed.

Autostart is explicit user opt-in in the app; ordinary startup only reads existing
state. Opt-in writes `~/Library/LaunchAgents/io.binachicken.client.plist` with
RunAtLoad and no KeepAlive, preserving executable/arguments/backend environment.
The app does not bootstrap launchctl automatically. Keep the app at a stable
path before opting in: LaunchAgent current_exe points directly to the normal
executable, whose bundle-relative media initialization also works at login.
Homebrew runtime dependencies must remain installed. Build/install scripts do
not alter autostart or privacy.
