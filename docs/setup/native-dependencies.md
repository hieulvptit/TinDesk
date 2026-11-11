# Native media dependencies

The protocol, backend, and policy crates have no GStreamer dependency. The
transport uses GStreamer Rust bindings 0.24 with a minimum native API of 1.24.
Use Rust 1.95 or newer and the committed Cargo.lock (`--locked`).

## macOS (Apple Silicon or Intel)

Install Apple's command-line developer tools (`xcode-select --install`) and
[Homebrew](https://brew.sh), then run from the repository root:

```sh
scripts/setup-macos-media.sh
source .deps/media/env.sh
cargo test -p binachicken-transport --locked
```

The helper installs only missing `gstreamer`, `libnice`, and `pkgconf` formulae.
It disables automatic Homebrew updates and installed-formula upgrades. It does
not install Rust. Existing formulae and compiler versions remain unchanged.
`scripts/test-macos-media-setup.sh` verifies this with a controlled Homebrew stub,
while still checking the actual installed plugins.

Homebrew libnice 0.1.24 disables its GStreamer plugin. If `nicesrc` is missing,
the helper downloads upstream libnice 0.1.24, checks its pinned SHA256, and builds
only `gstnice.c`, `gstnicesrc.c`, and `gstnicesink.c` against Homebrew's existing
libnice. This avoids loading two copies of the ICE library. Sources, license
files, plugin, and shell environment stay in ignored `.deps/media/`. A different
installed libnice version needs a reviewed source/checksum update. No temporary
validation directory is needed at build or runtime.

The helper probes `webrtcbin`, `nice`, `nicesrc`, `nicesink`, `dtlsenc`, `sctpenc`,
`h264parse`, `avdec_h264`, and `x264enc`. A standalone codec smoke check is:

```sh
gst-launch-1.0 -q videotestsrc num-buffers=5 ! videoconvert ! x264enc tune=zerolatency ! h264parse ! avdec_h264 ! fakesink
```

GStreamer registry discovery can print warnings from unrelated Homebrew Python
or GTK plugins on its first scan. BinaChicken transport does not use those plugins.
Do not enable verbose GStreamer debugging with live sessions: upstream native
logs may contain SDP, addresses, and TURN credentials. Application errors and
Debug implementations deliberately exclude these secrets and input contents.

## Ubuntu 26.04

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libgstreamer1.0-dev \
  libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev \
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav \
  gstreamer1.0-nice
for plugin in webrtcbin nice h264parse avdec_h264; do gst-inspect-1.0 "$plugin"; done
cargo test -p binachicken-transport --locked
```

Loopback tests use actual ICE/DTLS/SCTP on local interfaces and fail if native
dependencies are absent. No test silently skips GStreamer. Relay-only tests
verify the native ICE policy; local tests do not establish Cloudflare TURN
connectivity or Ubuntu hardware capture/encoding acceptance.

## Negotiation and reconnection

`Peer::new` starts a real pipeline but emits no automatic offer. Configure media
transceivers before the controller calls `start_negotiation()`. The controller
creates offers; the agent answers. `NegotiationNeeded` tells the orchestrator
that it may need to ask the controller to offer again. Task4 attaches H264 media
inside the transport crate; raw native pipeline handles are private.

GStreamer 1.24–1.28 `webrtcbin` does not implement in-place ICE restart
([upstream implementation](https://github.com/GStreamer/gstreamer/blob/1.28/subprojects/gst-plugins-bad/ext/webrtc/gstwebrtcbin.c)).
`restart_ice` validates replacement credentials, builds a fresh PeerConnection,
and sends `Restart { session_id, epoch }` before its new SDP/ICE. The other peer
rebuilds with its own current credentials. After processing Restarting and
NegotiationNeeded, the caller explicitly invokes the controller
`start_negotiation()` again; restart does not auto-offer. Restart
preserves authorized peer/session identity and the original lease deadline,
discards pending old input/media/signals, and invalidates old native callbacks.
All signaling carries a mandatory epoch; old epochs are rejected. Signaling
must preserve the Restart marker before the new epoch's offer and candidates.

The orchestrator must release held input on `Restarting`, `Closed`, errors, and
lease expiry. Task4 must persist its structured H264 configuration inside transport and
reattach the media graph during rebuild, before applying a remote offer or
creating an answer. External asynchronous event handling alone can race an
incoming offer. Task3 recreates data channels and requires a fresh display
acknowledgement.
Only a successful backend authorization renewal may call `renew_input_lease()`.
Input arrival never renews the 60-second lease. `display: None` rejects all input
until `update_display`; each initial/new display generation requires a matching
`DisplayAck`. `ReleaseAll` is allowed before acknowledgement when the display
is known. Higher generations invalidate prior coordinates and pointer sequence.
