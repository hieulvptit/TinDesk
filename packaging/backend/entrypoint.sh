#!/bin/sh
set -eu
umask 077
# No eval, xtrace, credential values, request URLs or auth headers in logs.
load_secret() {
  name=$1 file=$2 value=$3
  if [ -n "$file" ]; then
    [ -z "$value" ] || { echo 'Configure one secret source only' >&2; exit 1; }
    value=$(cat "$file")
  fi
  [ -n "$value" ] || { echo 'Required backend secret is absent' >&2; exit 1; }
  export "$name=$value"
}
load_secret REMOTEVNC_GOOGLE_CLIENT_SECRET "${REMOTEVNC_GOOGLE_CLIENT_SECRET_FILE:-}" "${REMOTEVNC_GOOGLE_CLIENT_SECRET:-}"
[ -n "${REMOTEVNC_PUBLIC_ORIGIN:-}" ] && [ -n "${REMOTEVNC_GOOGLE_CLIENT_ID:-}" ] || { echo 'Required backend configuration is absent' >&2; exit 1; }
if [ "${REMOTEVNC_TURN_KEYS_FILE+x}" = x ]; then
  [ -z "${REMOTEVNC_TURN_KEY_ID+x}${REMOTEVNC_TURN_API_TOKEN+x}${REMOTEVNC_TURN_API_TOKEN_FILE+x}" ] || { echo 'Configure one TURN source only' >&2; exit 1; }
  [ -n "$REMOTEVNC_TURN_KEYS_FILE" ] && [ -f "$REMOTEVNC_TURN_KEYS_FILE" ] && [ -r "$REMOTEVNC_TURN_KEYS_FILE" ] || { echo 'Required TURN file is absent or unreadable' >&2; exit 1; }
  # Rust validates private permissions, strict JSON and key bounds at startup.
else
  [ -n "${REMOTEVNC_TURN_KEY_ID:-}" ] || { echo 'Required backend configuration is absent' >&2; exit 1; }
  load_secret REMOTEVNC_TURN_API_TOKEN "${REMOTEVNC_TURN_API_TOKEN_FILE:-}" "${REMOTEVNC_TURN_API_TOKEN:-}"
fi
# Docker command overrides also let packaging checks execute a harmless process.
if [ "$#" -eq 0 ]; then set -- /usr/local/bin/remotevnc-backend; fi
exec "$@"
