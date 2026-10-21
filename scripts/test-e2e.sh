#!/usr/bin/env bash
# Disposable process-level signed-auth/native-media acceptance. No installed service changes.
set -euo pipefail
cd "$(dirname "$0")/.."
[[ $(uname -s) == Linux ]] || { echo 'E2E requires Linux Xvfb/GTK; run native macOS checks separately.' >&2; exit 1; }
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-1}
fixture=$(mktemp -d /tmp/remotevnc-e2e.XXXXXX)
export REMOTEVNC_E2E_RESULTS=${REMOTEVNC_E2E_RESULTS:-$fixture/results}
mkdir -m700 -p "$REMOTEVNC_E2E_RESULTS"
export REMOTEVNC_E2E=1 XAUTHORITY="$fixture/authority" REMOTEVNC_GTK_RESULT="$fixture/received"
xvfb_pid= gtk_pid=
cleanup() {
  [[ -z "$gtk_pid" ]] || kill "$gtk_pid" 2>/dev/null || true
  [[ -z "$xvfb_pid" ]] || kill "$xvfb_pid" 2>/dev/null || true
  echo "E2E logs: $REMOTEVNC_E2E_RESULTS"
}
trap cleanup EXIT
cargo build -p remotevnc-agent --locked
export REMOTEVNC_E2E_FIXTURE
REMOTEVNC_E2E_FIXTURE=$(cargo test -p remotevnc-agent --test e2e_fixture --no-run --locked --message-format=json | python3 -c '
import json, sys
executables = []
for line in sys.stdin:
    message = json.loads(line)
    if message.get("reason") == "compiler-message":
        print(message["message"].get("rendered", ""), file=sys.stderr)
    if message.get("reason") == "compiler-artifact" and message.get("target", {}).get("name") == "e2e_fixture" and message.get("executable"):
        executables.append(message["executable"])
if len(executables) != 1:
    sys.exit("expected exactly one compiled E2E fixture executable")
print(executables[0])')
Xvfb -displayfd 3 -screen 0 1920x1080x24 -nolisten tcp -auth "$XAUTHORITY" 3>"$fixture/display" >"$REMOTEVNC_E2E_RESULTS/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in {1..100}; do [[ -s "$fixture/display" ]] && break; sleep .05; done
export DISPLAY=":$(cat "$fixture/display")"
touch "$XAUTHORITY"
xauth -f "$XAUTHORITY" add "$DISPLAY" MIT-MAGIC-COOKIE-1 "$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
kill "$xvfb_pid"
wait "$xvfb_pid" 2>/dev/null || true
Xvfb "$DISPLAY" -screen 0 1920x1080x24 -nolisten tcp -auth "$XAUTHORITY" >"$REMOTEVNC_E2E_RESULTS/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in {1..100}; do xdpyinfo >/dev/null 2>&1 && break; sleep .05; done
python3 - <<'PY' >"$REMOTEVNC_E2E_RESULTS/gtk.log" 2>&1 &
import gi, os
from pathlib import Path
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib
window, entry = Gtk.Window(), Gtk.Entry()
window.add(entry)
window.connect('destroy', Gtk.main_quit)
window.show_all()
entry.grab_focus()
window.present()
entry.connect('changed', lambda widget: Path(os.environ['REMOTEVNC_GTK_RESULT']).write_text(widget.get_text()))
GLib.timeout_add(300, lambda: (Path(os.environ['REMOTEVNC_GTK_RESULT'] + '.ready').touch(), False)[1])
Gtk.main()
PY
gtk_pid=$!
for _ in {1..100}; do [[ -f "$REMOTEVNC_GTK_RESULT.ready" ]] && break; sleep .05; done
[[ -f "$REMOTEVNC_GTK_RESULT.ready" ]]
cargo test -p remotevnc-agent --lib service::native_tests:: --locked -- --ignored --nocapture --test-threads=1
cargo test -p remotevnc-agent --test e2e_session --locked -- --ignored --nocapture --test-threads=1
