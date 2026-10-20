#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/../.."
# Use the caller's Rust >=1.95 and configured GStreamer build environment.
# This development bundle is not a signed/notarized, dependency-contained release.
cargo build --locked --release -p remotevnc-client --no-default-features
bundle="${CARGO_TARGET_DIR:-target}/release/RemoteVNC.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "${CARGO_TARGET_DIR:-target}/release/remotevnc-client" "$bundle/Contents/MacOS/remotevnc-client"
cp packaging/macos/Info.plist "$bundle/Contents/Info.plist"
/usr/bin/plutil -lint "$bundle/Contents/Info.plist"
echo "Development bundle: $bundle (requires the configured native media runtime)"
