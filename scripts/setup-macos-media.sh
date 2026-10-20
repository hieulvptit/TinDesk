#!/usr/bin/env bash
# Install native dependencies and the missing Homebrew libnice GStreamer plugin.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin ]] || { echo 'This helper requires macOS.' >&2; exit 1; }
command -v brew >/dev/null || { echo 'Install Homebrew from https://brew.sh first.' >&2; exit 1; }
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_UPGRADE=1
for media_formula in gstreamer libnice pkgconf; do
  if ! brew list --versions "$media_formula" >/dev/null 2>&1; then brew install "$media_formula"; fi
done
media_root="$PWD/.deps/media"
mkdir -p "$media_root/gst-plugins"
media_prefix=$(brew --prefix)
export DYLD_FALLBACK_LIBRARY_PATH="$media_prefix/lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
export GST_PLUGIN_PATH_1_0="$media_root/gst-plugins${GST_PLUGIN_PATH_1_0:+:$GST_PLUGIN_PATH_1_0}"
if ! gst-inspect-1.0 nicesrc >/dev/null 2>&1; then
  nice_version=0.1.24
  installed_nice=$(pkg-config --modversion nice)
  [[ "$installed_nice" == "$nice_version" ]] || { echo "libnice $installed_nice requires reviewing the source version/checksum in this helper." >&2; exit 1; }
  nice_archive="$media_root/libnice-$nice_version.tar.gz"
  curl --fail --location --retry 3 "https://libnice.freedesktop.org/releases/libnice-$nice_version.tar.gz" -o "$nice_archive"
  echo "cfb5e8e778534f2f5b3c6f4958a1eb057c6b95c537c0f100817a537cf5d64fcc  $nice_archive" | shasum -a 256 -c -
  tar -xzf "$nice_archive" -C "$media_root"
  nice_source="$media_root/libnice-$nice_version"
  # Compile just the plugin, linking the existing Homebrew libnice once.
  # Word splitting of pkg-config output is intentional; Homebrew prefixes have no spaces.
  read -ra nice_flags <<< "$(pkg-config --cflags --libs gstreamer-1.0 gstreamer-base-1.0 gstreamer-net-1.0 nice)"
  cc -dynamiclib -O2 -DGST_USE_UNSTABLE_API \
    '-DVERSION="0.1.24"' '-DPACKAGE="libnice"' '-DPACKAGE_NAME="libnice"' \
    -I"$nice_source" -I"$nice_source/agent" \
    "$nice_source/gst/gstnice.c" "$nice_source/gst/gstnicesrc.c" "$nice_source/gst/gstnicesink.c" \
    "${nice_flags[@]}" -o "$media_root/gst-plugins/libgstnice.dylib"
fi
for plugin in webrtcbin nice nicesrc nicesink dtlsenc sctpenc h264parse avdec_h264 x264enc; do
  gst-inspect-1.0 "$plugin" >/dev/null
  echo "$plugin: available"
done
# Quote paths for shell sourcing; no temporary controller paths are used.
printf 'export GST_PLUGIN_PATH_1_0=%q\nexport DYLD_FALLBACK_LIBRARY_PATH=%q\n' "$GST_PLUGIN_PATH_1_0" "$DYLD_FALLBACK_LIBRARY_PATH" > "$media_root/env.sh"
echo "Native media is ready. Run: source .deps/media/env.sh"
