#!/bin/sh
set -eu
umask 077
# Compose secrets are files; the existing Rust config receives them only in-process.
# No eval, xtrace, credential values, request URLs or auth headers in logs.
for name in REMOTEVNC_GOOGLE_CLIENT_SECRET REMOTEVNC_TURN_API_TOKEN; do
  case "$name" in
    REMOTEVNC_GOOGLE_CLIENT_SECRET) file=${REMOTEVNC_GOOGLE_CLIENT_SECRET_FILE:-}; value=${REMOTEVNC_GOOGLE_CLIENT_SECRET:-} ;;
    REMOTEVNC_TURN_API_TOKEN) file=${REMOTEVNC_TURN_API_TOKEN_FILE:-}; value=${REMOTEVNC_TURN_API_TOKEN:-} ;;
  esac
  if [ -n "$file" ]; then
    [ -z "$value" ] || { echo 'Configure one secret source only' >&2; exit 1; }
    value=$(cat "$file")
  fi
  [ -n "$value" ] || { echo 'Required backend secret is absent' >&2; exit 1; }
  export "$name=$value"
done
[ -n "${REMOTEVNC_PUBLIC_ORIGIN:-}" ] && [ -n "${REMOTEVNC_GOOGLE_CLIENT_ID:-}" ] && [ -n "${REMOTEVNC_TURN_KEY_ID:-}" ] || { echo 'Required backend configuration is absent' >&2; exit 1; }
exec /usr/local/bin/remotevnc-backend
