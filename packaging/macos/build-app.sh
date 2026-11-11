#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/../.."
# Use the caller's Rust >=1.95 and configured GStreamer build environment.
# This development bundle uses the external Homebrew runtime and is not notarized.
[[ -f "$HOME/.local/share/binachicken/signing/identity.json" ]] || { echo 'Run python3 packaging/macos/sign-app.py --setup once (persistent local signing identity).' >&2; exit 1; }
[[ -f .deps/media/gst-plugins/libgstnice.dylib ]] || { echo 'Run scripts/setup-macos-media.sh first (project gstnice plugin required).' >&2; exit 1; }
export SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}
cargo build --locked --release -p binachicken-client --no-default-features
bundle="${CARGO_TARGET_DIR:-target}/release/BinaChicken.app"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "${CARGO_TARGET_DIR:-target}/release/binachicken-client" "$bundle/Contents/MacOS/binachicken-client"
cp packaging/macos/Info.plist "$bundle/Contents/Info.plist"
cp assets/branding/client.icns "$bundle/Contents/Resources/BinaChicken.icns"
mkdir -p "$bundle/Contents/Resources/gst-plugins" "$bundle/Contents/Resources/licenses/libnice"
cp .deps/media/gst-plugins/libgstnice.dylib "$bundle/Contents/Resources/gst-plugins/"
# Remove the build-checkout install ID; runtime opens this fixed bundle path.
/usr/bin/install_name_tool -id @rpath/libgstnice.dylib "$bundle/Contents/Resources/gst-plugins/libgstnice.dylib"
cp .deps/media/libnice-0.1.24/COPYING* "$bundle/Contents/Resources/licenses/libnice/"
/usr/bin/plutil -lint "$bundle/Contents/Info.plist"
{ rustc --version; cargo --version; shasum -a 256 Cargo.lock; brew list --versions gstreamer libnice pkgconf; } > "${CARGO_TARGET_DIR:-target}/release/native-build-manifest.txt"
cp "${CARGO_TARGET_DIR:-target}/release/native-build-manifest.txt" "$bundle/Contents/Resources/native-build-manifest.txt"
python3 packaging/macos/sign-app.py "$bundle"
shasum -a 256 "$bundle/Contents/MacOS/binachicken-client" "$bundle/Contents/Helpers/binachicken-keychain" "$bundle/Contents/Info.plist" "$bundle/Contents/Resources/BinaChicken.icns" "$bundle/Contents/Resources/gst-plugins/libgstnice.dylib" > "${CARGO_TARGET_DIR:-target}/release/SHA256SUMS"
echo "Development bundle: $bundle (requires the configured native media runtime)"
