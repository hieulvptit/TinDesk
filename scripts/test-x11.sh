#!/usr/bin/env bash
# Test-only virtual display; the production helper has no virtual-display bypass.
set -euo pipefail
cd "$(dirname "$0")/.."
fixture=$(mktemp -d /tmp/binachicken-x11.XXXXXX)
export XAUTHORITY="$fixture/authority"
export BINACHICKEN_GTK_RESULT="$fixture/received"
xvfb_pid=
gtk_pid=
cleanup() {
  if [[ -n "$gtk_pid" ]]; then kill "$gtk_pid" 2>/dev/null || true; fi
  if [[ -n "$xvfb_pid" ]]; then kill "$xvfb_pid" 2>/dev/null || true; fi
  rm -rf "$fixture"
}
trap cleanup EXIT
# Xvfb selects an unused display; never assumes :0.
Xvfb -displayfd 3 -screen 0 1920x1080x24 -nolisten tcp -auth "$XAUTHORITY" 3>"$fixture/display" >"$fixture/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in {1..100}; do [[ -s "$fixture/display" ]] && break; sleep .05; done
export DISPLAY=":$(cat "$fixture/display")"
touch "$XAUTHORITY"
xauth -f "$XAUTHORITY" add "$DISPLAY" MIT-MAGIC-COOKIE-1 "$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')"
# Xvfb reads auth at initialization: restart on the chosen display with real auth.
kill "$xvfb_pid"
wait "$xvfb_pid" 2>/dev/null || true
Xvfb "$DISPLAY" -screen 0 1920x1080x24 -nolisten tcp -auth "$XAUTHORITY" >"$fixture/xvfb.log" 2>&1 &
xvfb_pid=$!
for _ in {1..100}; do xdpyinfo >/dev/null 2>&1 && break; sleep .05; done
export BINACHICKEN_XVFB_PID="$xvfb_pid"
cargo test -p binachicken-seat-helper --test seat live_probe -- --ignored --test-threads=1
cargo test -p binachicken-seat-helper --test input cancelled_long_text -- --ignored --test-threads=1
cargo test -p binachicken-seat-helper --test input remote_cleanup -- --ignored --test-threads=1
cargo test -p binachicken-seat-helper --test ipc stalled_discovery -- --ignored --test-threads=1
cargo test -p binachicken-seat-helper --test input x11_releases -- --ignored --test-threads=1
python3 - <<'PY' &
import gi, os
from pathlib import Path
gi.require_version('Gtk', '3.0')
from gi.repository import Gtk, GLib
window = Gtk.Window()
entry = Gtk.Entry()
window.add(entry)
window.connect('destroy', Gtk.main_quit)
window.show_all()
entry.grab_focus()
window.present()
def ready():
    Path(os.environ['BINACHICKEN_GTK_RESULT'] + '.ready').touch()
    return False
GLib.timeout_add(200, ready)
def save(widget):
    Path(os.environ['BINACHICKEN_GTK_RESULT']).write_text(widget.get_text())
entry.connect('changed', save)
Gtk.main()
PY
gtk_pid=$!
for _ in {1..100}; do [[ -e "$BINACHICKEN_GTK_RESULT.ready" ]] && break; sleep .05; done
cargo test -p binachicken-seat-helper --test input gtk_receives -- --ignored --test-threads=1
sleep .2
python3 - <<'PY'
import os
from pathlib import Path
expected = 'Tiếng Việt|é|e\u0301|中文|😀'
actual = Path(os.environ['BINACHICKEN_GTK_RESULT']).read_text()
assert actual == expected, f'exact GTK Unicode mismatch: {actual!r} != {expected!r}'
print('GTK exact Unicode PASS: Vietnamese, NFC, NFD, CJK, non-BMP; no duplicate commits')
PY

# Native capture/media and independent guardian recovery on this disposable display.
cargo test -p binachicken-seat-helper --test capture physical_capture -- --ignored --test-threads=1
cargo test -p binachicken-seat-helper --test capture cropped_capture -- --ignored --test-threads=1
cargo test -p binachicken-seat-helper --test capture hot_profile_renegotiation -- --ignored --test-threads=1
cargo test -p binachicken-seat-helper --test recovery guardian_releases_real -- --ignored --test-threads=1
cargo test -p binachicken-seat-helper --test recovery guardian_restores_unicode -- --ignored --test-threads=1
