#!/usr/bin/env bash
# Shared required checks. Run on each native platform; never mark another OS passed.
set -euo pipefail
cd "$(dirname "$0")/.."
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}
for script in scripts/*.sh packaging/ubuntu/*.sh packaging/macos/*.sh packaging/backend/*.sh; do bash -n "$script"; done
python3 scripts/test-backend-maintenance.py
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
case "$(uname -s)" in
  Linux) echo 'Linux native modules checked; explicit Xvfb and root installer fixtures: scripts/test-x11.sh and sudo python3 scripts/test-ubuntu-installer.py' ;;
  Darwin) /usr/bin/plutil -lint packaging/macos/Info.plist; echo 'macOS native modules checked. Linux/Xvfb/systemd/installer checks require Linux and were not run here.' ;;
  *) echo 'Unsupported native test platform' >&2; exit 1 ;;
esac
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  git diff --check
else
  echo 'No usable Git metadata in this source tree; run git diff --check in its owning checkout (not checked here).'
fi
