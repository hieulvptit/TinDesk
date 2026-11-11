#!/usr/bin/env bash
# Local development backend on loopback only. Data lives outside the repository and is private.
# Google sign-in and TURN are NOT configured: this instance is for testing, not real enrollment.
set -euo pipefail
cd "$(dirname "$0")/.."
bin=${CARGO_TARGET_DIR:-target}/release/remotevnc-backend
[[ -x $bin ]] || { echo "build first: cargo build --release -p remotevnc-backend" >&2; exit 1; }
data="$HOME/Library/Application Support/BinaChicken-backend-dev"
mkdir -p "$data" && chmod 0700 "$data"
export REMOTEVNC_DATABASE="$data/backend.db"
export REMOTEVNC_BIND="${REMOTEVNC_BIND:-127.0.0.1:8080}"
# The backend insists on an https origin; it only builds sign-in URLs from it. The listener below
# stays plain HTTP on loopback, which the client accepts for 127.0.0.1.
export REMOTEVNC_PUBLIC_ORIGIN="https://localhost"
export REMOTEVNC_GOOGLE_CLIENT_ID="local-dev-placeholder.apps.googleusercontent.com"
export REMOTEVNC_GOOGLE_CLIENT_SECRET="local-dev-placeholder"
exec "$bin"
