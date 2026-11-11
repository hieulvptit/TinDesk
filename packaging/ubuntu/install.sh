#!/usr/bin/env bash
# Administrative install only. Build/test/prepare never switch the active display manager.
set -euo pipefail
umask 077
here=$(cd "$(dirname "$0")" && pwd)
mode='' bin_dir=target/release origin=https://amio.vnoffice.io.vn allow_ubuntu_2510=false
fail() { echo "BinaChicken: $*" >&2; exit 1; }
while (($#)); do
  case "$1" in
    --prepare|--apply|--dry-run|--doctor) [[ -z "$mode" ]] || fail 'choose one mode'; mode=$1; shift ;;
    --bin-dir) bin_dir=${2:?missing binary directory}; shift 2 ;;
    --backend) origin=${2:?missing HTTPS backend origin}; shift 2 ;;
    --allow-ubuntu-2510) allow_ubuntu_2510=true; shift ;;
    *) fail "unknown argument: $1" ;;
  esac
done
[[ -n "$mode" ]] || fail 'usage: install.sh --dry-run|--prepare|--apply|--doctor [--bin-dir DIR] [--backend HTTPS_ORIGIN] [--allow-ubuntu-2510]'
if [[ "$mode" == --dry-run ]]; then
  echo 'Prepare: install native/Xfce/LightDM/greeter/locker dependencies with service starts blocked; preserve display-manager selection; install private agent state and reviewed units.'
  echo 'Enroll locally as binachicken-agent. Apply: back up display selection/config/unit enablement, select LightDM/Xfce for NEXT BOOT; start guardian, helper, agent.'
  echo 'No display-manager restart or automatic reboot. Current Wayland desktop remains unsupported until an explicit administrator reboot. Rollback path is printed before changes.'
  exit 0
fi
[[ $EUID == 0 ]] || fail 'requires root; dry-run needs no privilege'
[[ -f /etc/os-release ]] || fail 'missing Ubuntu release information'
# shellcheck source=/dev/null
source /etc/os-release
[[ "$ID" == ubuntu && ( "$VERSION_ID" == 26.04 || ( "$VERSION_ID" == 25.10 && "$allow_ubuntu_2510" == true ) ) ]] || fail 'requires Ubuntu 26.04 or Ubuntu 25.10 with explicit --allow-ubuntu-2510; no OS upgrade is performed'
for command in python3 systemctl runuser useradd groupadd getent install stat; do command -v "$command" >/dev/null || fail "missing prerequisite: $command"; done
python3 - "$origin" <<'PY'
import sys, urllib.parse
u=urllib.parse.urlsplit(sys.argv[1])
assert u.scheme == 'https' and u.hostname and not u.username and not u.password and u.path in ('','/') and not u.query and not u.fragment, 'requires HTTPS origin only'
PY
kernel=$(uname -r); IFS=. read -r major minor _ <<< "$kernel"
((major > 6 || (major == 6 && minor >= 5))) || fail 'guardian requires Linux >=6.5 and usable SO_PEERPIDFD/pidfd_send_signal (checked at runtime)'
for managed in /etc/binachicken /var/lib/binachicken-agent /var/lib/binachicken-installer /run/binachicken /run/binachicken-guardian; do
  [[ ! -L "$managed" ]] || fail "managed directory must not be a symlink: $managed"
done
if [[ -e /run/binachicken ]]; then
  [[ -d /run/binachicken && $(stat -c '%u:%a' /run/binachicken) == 0:711 ]] || fail '/run/binachicken must be dedicated root-owned 0711; inspect/remove stale directory explicitly (not chmod arbitrary parents)'
fi
for managed in /var/lib/binachicken-installer /run/binachicken-guardian; do
  if [[ -e "$managed" ]]; then
    [[ -d "$managed" && $(stat -c '%u:%a' "$managed") == 0:700 ]] || fail "existing managed directory must be root-owned0700: $managed"
  fi
done
if [[ -e /etc/binachicken ]]; then
  [[ -d /etc/binachicken && $(stat -c %u /etc/binachicken) == 0 ]] || fail 'config directory must be root-owned'
  (( (8#$(stat -c %a /etc/binachicken) & 0022) == 0 )) || fail 'config directory must not be group/other writable'
fi
units=(binachicken-input-guardian binachicken-seat-helper binachicken-agent)
if [[ -e /var/lib/binachicken-agent ]]; then
  [[ -d /var/lib/binachicken-agent ]] || fail 'state must be a dedicated directory'
  existing_uid=$(id -u binachicken-agent) || fail 'existing private state requires its original agent account'
  [[ "$existing_uid" != 0 && $(stat -c '%u:%a' /var/lib/binachicken-agent) == "$existing_uid:700" ]] || fail 'existing state ownership/mode invalid'
fi
if [[ -e /etc/binachicken/agent.json || -L /etc/binachicken/agent.json ]]; then
  [[ -f /etc/binachicken/agent.json && ! -L /etc/binachicken/agent.json ]] || fail 'agent config must be a regular managed file'
fi
if [[ "$mode" == --prepare ]]; then
  for command in apt-get debconf-set-selections; do command -v "$command" >/dev/null || fail "missing prerequisite: $command"; done
  for binary in binachicken-agent binachicken-seat-helper; do [[ -x "$bin_dir/$binary" ]] || fail "missing native executable: $bin_dir/$binary (run scripts/build-linux.sh)"; done
else
  for binary in binachicken-agent binachicken-seat-helper; do [[ -x /usr/bin/$binary ]] || fail "not prepared: /usr/bin/$binary"; done
  [[ -f /etc/binachicken/agent.json ]] || fail 'run --prepare first'
fi
# Snapshot Debconf selection before package configuration can change it.
if [[ "$mode" != --doctor ]]; then
  for command in debconf-communicate debconf-set-selections; do command -v "$command" >/dev/null || fail "missing prerequisite: $command"; done
  debconf_reply=$(printf 'GET shared/default-x-display-manager\n' | DEBIAN_FRONTEND=noninteractive debconf-communicate binachicken-installer 2>/dev/null || true)
  debconf_absent=false
  case "$debconf_reply" in
    '0 '*) debconf_value=${debconf_reply#0 } ;;
    '10 '*) debconf_value=''; debconf_absent=true ;;
    *) fail 'could not read display-manager Debconf state before writes' ;;
  esac
  debconf_seen=false
  if ! "$debconf_absent"; then
    debconf_reply=$(printf 'FGET shared/default-x-display-manager seen\n' | DEBIAN_FRONTEND=noninteractive debconf-communicate binachicken-installer 2>/dev/null)
    case "$debconf_reply" in '0 true') debconf_seen=true ;; '0 false') ;; *) fail 'invalid display-manager Debconf seen flag' ;; esac
  fi
fi
# Check managed ancestors before writes. Never change unrelated ancestor permissions.
for ancestor in / /run /var /var/lib /etc; do
  [[ -d "$ancestor" && ! -L "$ancestor" ]] || fail "invalid managed ancestor: $ancestor"
  [[ $(stat -c %u "$ancestor") == 0 ]] || fail "managed ancestor must be root-owned: $ancestor"
  (( (8#$(stat -c %a "$ancestor") & 0022) == 0 )) || fail "managed ancestor must not be group/other writable: $ancestor"
  (( (8#$(stat -c %a "$ancestor") & 0001) != 0 )) || fail "agent cannot traverse ancestor: $ancestor"
done
if [[ "$mode" != --prepare ]]; then
  for plugin in webrtcbin nicesrc nicesink dtlsenc sctpenc ximagesrc x264enc h264parse avdec_h264; do gst-inspect-1.0 "$plugin" >/dev/null; done
  for binary in lightdm light-locker; do command -v "$binary" >/dev/null || fail "missing desktop prerequisite: $binary"; done
  [[ -f /usr/share/xsessions/xfce.desktop ]] || fail 'missing Xfce X11 session'
  runuser -u binachicken-agent -- test -r /etc/binachicken/agent.json || fail 'actual agent UID cannot read config'
  uid=$(id -u binachicken-agent)
  (( uid != 0 )) || fail 'agent account must be unprivileged'
  [[ $(stat -c '%u:%a' /var/lib/binachicken-agent) == "$uid:700" ]] || fail 'state must be owned by actual agent UID with mode0700'
  runuser -u binachicken-agent -- test -x /run/binachicken || fail 'actual agent UID cannot traverse helper socket ancestors'
  if [[ "$mode" == --doctor ]]; then
    for plugin in webrtcbin nice nicesrc nicesink dtlsenc sctpenc ximagesrc x264enc h264parse avdec_h264; do gst-inspect-1.0 "$plugin" >/dev/null; done
    runuser -u binachicken-agent -- /usr/bin/binachicken-agent doctor --config /etc/binachicken/agent.json
    echo 'Doctor checks installed dependencies/identity/IPC, not physical greeter/locker or public TURN acceptance.'
    exit 0
  fi
  for unit in "${units[@]}" lightdm; do
    [[ $(systemctl is-enabled "$unit.service" 2>/dev/null || true) != masked* ]] || fail "unmask $unit explicitly before apply"
  done
  python3 - <<'PYCONFIG'
import json, stat, urllib.parse
from pathlib import Path
p=Path('/etc/binachicken/agent.json')
s=p.lstat()
assert stat.S_ISREG(s.st_mode) and s.st_uid == 0 and not s.st_mode & 0o027, 'root-owned, group-readable, nonwritable agent config required'
d=json.loads(p.read_bytes())
u=urllib.parse.urlsplit(d['backend_origin'])
assert u.scheme=='https' and u.hostname and not u.username and not u.password and u.path in ('','/') and not u.query and not u.fragment, 'HTTPS backend origin required'
assert d['identity_path']=='/var/lib/binachicken-agent' and d['helper_socket']=='/run/binachicken/helper.sock' and d['seat']=='seat0', 'installed managed paths required'
PYCONFIG
  # Local enrollment is only a prerequisite, never a backend authorization bypass.
  runuser -u binachicken-agent -- python3 - <<'PY'
import json, os, stat, uuid
from pathlib import Path
p=Path('/var/lib/binachicken-agent/identity.json')
s=p.lstat()
assert stat.S_ISREG(s.st_mode) and s.st_uid == os.getuid() and stat.S_IMODE(s.st_mode) == 0o600, 'private owner-only identity required'
d=json.loads(p.read_bytes())
assert d['version'] == 1 and uuid.UUID(d['device_id']) and len(d['secret']) == 32 and all(type(x) is int and 0<=x<=255 for x in d['secret']), 'complete enrollment as state owner before apply'
assert not p.with_name('revoked').exists(), 'locally revoked: explicit successful re-enrollment required'
PY
fi
install -d -m 0700 /var/lib/binachicken-installer
backup=$(mktemp -d /var/lib/binachicken-installer/backup-XXXXXXXX)
paths=(etc/X11/default-display-manager etc/lightdm/lightdm.conf.d/50-binachicken.conf etc/systemd/system/display-manager.service etc/systemd/system/graphical.target.wants/lightdm.service etc/systemd/system/multi-user.target.wants/lightdm.service)
for path in "${paths[@]}"; do
  if [[ -e /$path || -L /$path ]]; then mkdir -p "$backup/$(dirname "$path")"; cp -a "/$path" "$backup/$path"; else echo "$path" >> "$backup/absent"; fi
done
for unit in "${units[@]}" lightdm; do
  status=$(systemctl is-enabled "$unit.service" 2>/dev/null || true)
  printf '%s %s\n' "$unit" "${status:-disabled}" >> "$backup/enablement"
done
printf '%s\n' "$debconf_value" > "$backup/debconf-value"
printf '%s\n' "$debconf_seen" > "$backup/debconf-seen"
if "$debconf_absent"; then touch "$backup/debconf-absent"; fi
printf '%s\n' "Rollback: sudo $here/rollback.sh $backup"
restore_selection() {
  for path in "${paths[@]}"; do
    rm -f "/$path"
    if [[ -e "$backup/$path" || -L "$backup/$path" ]]; then mkdir -p "/$(dirname "$path")"; cp -a "$backup/$path" "/$path"; fi
  done
  # For a newly installed question with no previous value, restore unselected/unseen.
  # Package schema may now exist; no manager is selected by prepare.
  local current
  current=$(printf 'GET shared/default-x-display-manager\n' | DEBIAN_FRONTEND=noninteractive debconf-communicate binachicken-installer 2>/dev/null || true)
  if [[ -f "$backup/debconf-absent" && "$current" == '10 '* ]]; then return; fi
  local replies code _detail
  replies=$(printf 'SET shared/default-x-display-manager %s\nFSET shared/default-x-display-manager seen %s\n' "$(cat "$backup/debconf-value")" "$(cat "$backup/debconf-seen")" | DEBIAN_FRONTEND=noninteractive debconf-communicate binachicken-installer)
  while read -r code _detail; do [[ "$code" == 0 ]] || fail "cannot restore Debconf selection; inspect $backup"; done <<< "$replies"
}
if [[ "$mode" == --prepare ]]; then
  # policy-rc.d is honored by Debian package service starts, including LightDM.
  # Restore the administrator's exact policy and display selection on every exit.
  policy=/usr/sbin/policy-rc.d
  policy_existed=false
  if [[ -e "$policy" || -L "$policy" ]]; then cp -a "$policy" "$backup/policy-rc.d"; policy_existed=true; fi
  cleanup_prepare() {
    status=$?
    rm -f "$policy"
    if "$policy_existed"; then cp -a "$backup/policy-rc.d" "$policy"; fi
    restore_selection
    exit "$status"
  }
  trap cleanup_prepare EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM
  rm -f "$policy"
  printf '#!/bin/sh\nexit 101\n' > "$policy"; chmod 0755 "$policy"
  previous=lightdm
  if [[ -s /etc/X11/default-display-manager ]]; then previous=$(basename "$(cat /etc/X11/default-display-manager)"); fi
  printf 'lightdm shared/default-x-display-manager select %s\n' "$previous" | debconf-set-selections
  export DEBIAN_FRONTEND=noninteractive
  apt-get update
  apt-get install -y --no-install-recommends lightdm lightdm-gtk-greeter xfce4 light-locker xserver-xorg x11-xserver-utils xauth dbus-x11 \
    gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-nice
  if ! getent group binachicken-agent >/dev/null; then groupadd --system binachicken-agent; fi
  if ! getent passwd binachicken-agent >/dev/null; then useradd --system --gid binachicken-agent --home-dir /var/lib/binachicken-agent --no-create-home --shell /usr/sbin/nologin binachicken-agent; fi
  uid=$(id -u binachicken-agent)
  if [[ -e /var/lib/binachicken-agent ]]; then
    [[ $(stat -c '%u:%a' /var/lib/binachicken-agent) == "$uid:700" ]] || fail 'existing state ownership/mode invalid; will not adopt arbitrary data'
  else install -d -m0700 -o binachicken-agent -g binachicken-agent /var/lib/binachicken-agent; fi
  install -d -m0750 -o root -g binachicken-agent /etc/binachicken
  install -d -m0711 /run/binachicken
  runuser -u binachicken-agent -- test -x /run/binachicken || fail 'actual agent UID cannot traverse socket ancestors'
  if [[ ! -e /etc/binachicken/agent.json ]]; then
    python3 - "$origin" <<'PY' > /etc/binachicken/agent.json
import json, sys
print(json.dumps(dict(backend_origin=sys.argv[1],identity_path='/var/lib/binachicken-agent',helper_socket='/run/binachicken/helper.sock',seat='seat0',relay_only=False),indent=2))
PY
    chown root:binachicken-agent /etc/binachicken/agent.json; chmod 0640 /etc/binachicken/agent.json
  fi
  for binary in binachicken-agent binachicken-seat-helper; do install -m0755 "$bin_dir/$binary" "/usr/bin/$binary"; done
  for unit in "${units[@]}"; do install -m0644 "$here/$unit.service" "/etc/systemd/system/$unit.service"; done
  systemctl daemon-reload
  echo 'Prepared. Stop agent before enrollment: sudo systemctl stop binachicken-agent.service'
  echo 'Enroll: sudo -u binachicken-agent /usr/bin/binachicken-agent enroll --config /etc/binachicken/agent.json --name Ubuntu'
  echo 'Then review --apply. No display manager was restarted; previous selection restored.'
else
  install -d -m0755 /etc/lightdm/lightdm.conf.d /etc/X11
  install -m0644 "$here/lightdm.conf" /etc/lightdm/lightdm.conf.d/50-binachicken.conf
  printf 'lightdm shared/default-x-display-manager select lightdm\n' | DEBIAN_FRONTEND=noninteractive debconf-set-selections
  printf '/usr/sbin/lightdm\n' > /etc/X11/default-display-manager
  systemctl enable --force lightdm.service
  systemctl enable "${units[@]/%/.service}"
  systemctl daemon-reload
  for unit in "${units[@]}"; do systemctl start "$unit.service"; done
  echo 'Applied for NEXT BOOT. No display-manager restart or reboot performed. Current Wayland seat may report unsupported.'
  echo 'Run --doctor; review rollback path above; then reboot explicitly from your local session when ready.'
fi
