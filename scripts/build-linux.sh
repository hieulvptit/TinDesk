#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
[[ $(uname -s) == Linux ]] || { echo 'Native Linux build required; this script does not configure a desktop.' >&2; exit 1; }
for plugin in webrtcbin nicesrc nicesink dtlsenc sctpenc ximagesrc x264enc h264parse avdec_h264; do gst-inspect-1.0 "$plugin" >/dev/null; done
# Software x264 is mandatory; runtime probes hardware and falls back within the helper sandbox.
# A native release build needs scratch space in addition to cached debug artifacts.
# Refuse the known tight-VM case before filling its filesystem.
build_fs=${CARGO_TARGET_DIR:-.}
[[ -d "$build_fs" ]] || build_fs=$(dirname "$build_fs")
build_free_kib=$(df -Pk "$build_fs" | awk 'NR==2 {print $4}')
(( build_free_kib >= 2097152 )) || { echo 'Native release build needs at least 2GiB free scratch space; use a separate builder.' >&2; exit 1; }
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}
if [[ -z ${SOURCE_DATE_EPOCH:-} ]]; then
  SOURCE_DATE_EPOCH=$(git log -1 --format=%ct) || { echo 'Source archive: set SOURCE_DATE_EPOCH from the reviewed commit before building.' >&2; exit 1; }
fi
export SOURCE_DATE_EPOCH
cargo build --locked --release -p binachicken-agent -p binachicken-seat-helper -p binachicken-sync-helper --no-default-features
artifact_dir="${CARGO_TARGET_DIR:-target}/release"
{ rustc --version; cargo --version; sha256sum Cargo.lock; dpkg-query -W 'gstreamer1.0-*' 'libgstreamer*'; } > "$artifact_dir/native-build-manifest.txt"
sha256sum "$artifact_dir/binachicken-agent" "$artifact_dir/binachicken-seat-helper" "$artifact_dir/binachicken-sync-helper" > "$artifact_dir/SHA256SUMS"
echo "Native artifacts: $artifact_dir (no installation/service/desktop changes)"
