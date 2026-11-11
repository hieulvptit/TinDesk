#!/usr/bin/env bash
# Turn this Mac into a remote-controllable host (user LaunchAgents, no root).
#   packaging/macos/host.sh install [--backend URL] [--name NAME]
#   packaging/macos/host.sh enroll | start | stop | status | doctor | uninstall
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
BASE="$HOME/Library/Application Support/BinaChicken"
BIN="$BASE/bin"
CONFIG="$BASE/agent.json"
IDENTITY="$BASE/identity"
PLUGINS="$BASE/gst-plugins"
LOGS="$HOME/Library/Logs/BinaChicken"
AGENTS="$HOME/Library/LaunchAgents"
HELPER_LABEL=io.binachicken.mac-helper
AGENT_LABEL=io.binachicken.host-agent
DOMAIN="gui/$(id -u)"
BACKEND="${BINACHICKEN_BACKEND:-https://amio.vnoffice.io.vn}"
NAME="$(scutil --get ComputerName 2>/dev/null || hostname -s)"

plist() { # label program-args... ; env GST_PLUGIN_PATH_1_0 + DYLD fallback for Homebrew
  local label="$1"; shift
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    echo '<plist version="1.0"><dict>'
    echo "<key>Label</key><string>$label</string>"
    echo '<key>ProgramArguments</key><array>'
    for a in "$@"; do echo "<string>$a</string>"; done
    echo '</array>'
    echo '<key>EnvironmentVariables</key><dict>'
    echo "<key>GST_PLUGIN_PATH_1_0</key><string>$PLUGINS</string>"
    echo '<key>DYLD_FALLBACK_LIBRARY_PATH</key><string>/opt/homebrew/lib</string>'
    echo '</dict>'
    echo '<key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>ThrottleInterval</key><integer>5</integer>'
    echo '<key>ProcessType</key><string>Interactive</string>'
    echo "<key>StandardErrorPath</key><string>$LOGS/$label.log</string>"
    echo "<key>StandardOutPath</key><string>$LOGS/$label.log</string>"
    echo '</dict></plist>'
  } > "$AGENTS/$label.plist"
}
load()   { launchctl bootout "$DOMAIN/$1" 2>/dev/null || true; launchctl bootstrap "$DOMAIN" "$AGENTS/$1.plist"; }
unload() { launchctl bootout "$DOMAIN/$1" 2>/dev/null || true; }

cmd="${1:-}"; shift || true
while [ $# -gt 0 ]; do
  case "$1" in
    --backend) BACKEND="$2"; shift 2;;
    --name) NAME="$2"; shift 2;;
    *) echo "unknown argument: $1" >&2; exit 2;;
  esac
done

case "$cmd" in
  install)
    for b in binachicken-agent binachicken-mac-helper; do
      [ -x "$ROOT/target/release/$b" ] || { echo "build first: cargo build --release -p binachicken-agent -p binachicken-mac-helper" >&2; exit 1; }
    done
    [ -f "$ROOT/.deps/media/gst-plugins/libgstnice.dylib" ] || { echo "run scripts/setup-macos-media.sh first" >&2; exit 1; }
    mkdir -p "$BIN" "$PLUGINS" "$LOGS" "$AGENTS" "$IDENTITY"
    chmod 700 "$BASE" "$IDENTITY"
    # Stable install path so macOS privacy grants (Screen Recording/Accessibility) persist.
    install -m 755 "$ROOT/target/release/binachicken-agent" "$BIN/"
    install -m 755 "$ROOT/target/release/binachicken-mac-helper" "$BIN/"
    install -m 755 "$ROOT/.deps/media/gst-plugins/libgstnice.dylib" "$PLUGINS/"
    # SIGN_IDENTITY keeps the code identity (and thus the privacy grants) stable across rebuilds.
    codesign --force --sign "${SIGN_IDENTITY:--}" "$BIN/binachicken-mac-helper" "$BIN/binachicken-agent" "$PLUGINS/libgstnice.dylib"
    cat > "$CONFIG" <<JSON
{
  "backend_origin": "$BACKEND",
  "identity_path": "$IDENTITY",
  "helper_socket": "$BASE/helper.sock",
  "seat": "seat0"
}
JSON
    chmod 600 "$CONFIG"
    plist "$HELPER_LABEL" "$BIN/binachicken-mac-helper" --socket "$BASE/helper.sock"
    plist "$AGENT_LABEL" "$BIN/binachicken-agent" run --config "$CONFIG"
    echo "Installed. Next: '$0 enroll', grant the permissions '$0 doctor' lists, then '$0 start'."
    ;;
  enroll)
    GST_PLUGIN_PATH_1_0="$PLUGINS" DYLD_FALLBACK_LIBRARY_PATH=/opt/homebrew/lib \
      "$BIN/binachicken-agent" enroll --config "$CONFIG" --name "$NAME"
    ;;
  doctor)
    "$BIN/binachicken-mac-helper" doctor
    ;;
  start)
    load "$HELPER_LABEL"; load "$AGENT_LABEL"; echo "Host started (logs: $LOGS)";;
  stop)
    unload "$AGENT_LABEL"; unload "$HELPER_LABEL"; echo "Host stopped";;
  status)
    for l in "$HELPER_LABEL" "$AGENT_LABEL"; do
      if launchctl print "$DOMAIN/$l" >/dev/null 2>&1; then echo "$l: loaded"; else echo "$l: not loaded"; fi
    done;;
  uninstall)
    unload "$AGENT_LABEL"; unload "$HELPER_LABEL"
    rm -f "$AGENTS/$HELPER_LABEL.plist" "$AGENTS/$AGENT_LABEL.plist"
    rm -rf "$BIN" "$PLUGINS" "$BASE/helper.sock" "$CONFIG"
    echo "Removed host services. Enrollment identity kept in $IDENTITY; run revoke-local to drop it."
    ;;
  *) echo "usage: $0 install|enroll|doctor|start|stop|status|uninstall [--backend URL] [--name NAME]" >&2; exit 2;;
esac
