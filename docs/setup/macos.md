# macOS native development app

Install Apple's command-line tools and Homebrew, then Rust 1.95.0 in an isolated
prefix or via `rustup toolchain install 1.95.0 --profile minimal`. Select it in
this shell with `export RUSTUP_TOOLCHAIN=1.95.0`; no default-compiler change is
needed. From this checkout:

```sh
scripts/setup-macos-media.sh
source .deps/media/env.sh
scripts/check.sh
packaging/macos/build-app.sh
target/release/RemoteVNC.app/Contents/MacOS/remotevnc-client \
  --backend https://amio.vnoffice.io.vn
```

This is an unsigned/ad-hoc **development bundle**, not a portable, notarized or
production-signed installer. The executable still needs the installed Homebrew
GStreamer/libnice runtime. The build copies the reviewed project-built gstnice
plugin, its licenses and a nonsecret build manifest into Contents/Resources.
It normalizes the plugin install ID and applies only a local ad-hoc plugin
signature; the app is not a Developer ID signed/sealed distribution package.
Before starting the UI, the normal executable loads that one fixed bundle-relative
plugin using [GStreamer's plugin API](https://gstreamer.freedesktop.org/documentation/gstreamer/gstplugin.html#gst_plugin_load_file)
and verifies the required factories. Other libraries/plugins use the native
Homebrew runtime's compiled paths. There is no startup environment mutation,
arbitrary plugin configuration or dependency on controller /tmp/checkouts.

Finder, `open target/release/RemoteVNC.app`, direct executable and the opt-in
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

The bundle identifier is `io.remotevnc.client`, with a local-network usage
description for direct Ubuntu connections. Apple recommends an Apple-issued
signing identity for reliable local-network permission identity; an unsigned or
ad-hoc executable cannot establish production-signed/LAN permission acceptance.
See [Apple TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy).
No signing identity/certificate has been supplied. A distributable release still
needs a frozen/vendor-contained native runtime, corrected library/plugin lookup
paths, retained third-party notices, nested-code signing, Developer ID signing,
notarization and stapling, followed by clean-machine Gatekeeper/LAN checks.
Apple's [distribution guidance](https://developer.apple.com/documentation/security/distributing-your-app-for-beta-testing-and-releases)
describes that separate release process. Do not label a successful local build
as completion of those steps or disable Gatekeeper/TCC to simulate acceptance.

The normal package build uses `--no-default-features`; `native-smoke` and the
`ui_smoke` rendering fixture stay disabled. The app uses Google enrollment and
user Keychain service `io.remotevnc.client`, account keyed by canonical backend
origin, without a file-secret fallback. No global input, screen recording or
Accessibility permission is needed to forward events from its own window. Test
interactive Google/Keychain, Vietnamese IME, local-network privacy and physical
remote input separately; a loopback/render fixture does not exercise them.

Autostart is explicit user opt-in in the app; ordinary startup only reads existing
state. Opt-in writes `~/Library/LaunchAgents/io.remotevnc.client.plist` with
RunAtLoad and no KeepAlive, preserving executable/arguments/backend environment.
The app does not bootstrap launchctl automatically. Keep the app at a stable
path before opting in: LaunchAgent current_exe points directly to the normal
executable, whose bundle-relative media initialization also works at login.
Homebrew runtime dependencies must remain installed. Build/install scripts do
not alter autostart or privacy.
