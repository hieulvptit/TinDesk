#!/usr/bin/env bash
# Adds clipboard/file sync to an installed BinaChicken agent: new agent binary, per-user sync
# helper, one autostart entry, `sync_socket` in agent.json and a systemd drop-in granting the
# agent its RuntimeDirectory. Backs everything up and rolls back if the agent does not come up.
# Usage: sudo install-sync.sh [--enable-ssh [PORT]] [BIN_DIR]
#   BIN_DIR holds binachicken-agent, binachicken-sync-helper and optionally SHA256SUMS
#   (default: this script's directory, else target/release).
#   --enable-ssh lets an authorized controller tunnel to this machine's sshd (default port 22),
#   only for the active, unlocked seat user. Without it the tunnel stays off.
set -Eeuo pipefail
here=$(cd -- "$(dirname -- "$0")" && pwd)
ssh_port=
bin_dir=
while [[ $# -gt 0 ]]; do
  case $1 in
    --enable-ssh)
      ssh_port=22
      if [[ ${2:-} =~ ^[0-9]+$ ]]; then ssh_port=$2; shift; fi ;;
    *) bin_dir=$1 ;;
  esac
  shift
done
if [[ -n $ssh_port ]] && (( ssh_port < 1 || ssh_port > 65535 )); then echo 'invalid ssh port' >&2; exit 1; fi
if [[ -z $bin_dir ]]; then
  if [[ -x $here/binachicken-agent ]]; then bin_dir=$here; else bin_dir="$here/../../target/release"; fi
fi
fail() { echo "install-sync: $*" >&2; exit 1; }
[[ $EUID -eq 0 ]] || fail 'run with sudo'
for cmd in install systemctl python3 mktemp sha256sum; do command -v "$cmd" >/dev/null || fail "missing $cmd"; done
for f in binachicken-agent binachicken-sync-helper; do [[ -x $bin_dir/$f ]] || fail "missing $bin_dir/$f"; done
desktop="$here/binachicken-sync-helper.desktop"; [[ -f $desktop ]] || desktop="$bin_dir/binachicken-sync-helper.desktop"
[[ -f $desktop ]] || fail 'missing binachicken-sync-helper.desktop'
if [[ -f $bin_dir/SHA256SUMS ]]; then (cd "$bin_dir" && sha256sum --check --strict SHA256SUMS) || fail 'checksum mismatch'; fi
[[ -f /usr/bin/binachicken-agent && ! -L /usr/bin/binachicken-agent ]] || fail 'expected an installed agent binary'
[[ -f /etc/binachicken/agent.json && ! -L /etc/binachicken/agent.json ]] || fail 'expected /etc/binachicken/agent.json'
systemctl is-active --quiet binachicken-seat-helper.service || fail 'seat helper is not active'

install -d -m0700 /var/lib/binachicken-installer
backup=$(mktemp -d /var/lib/binachicken-installer/sync-backup-XXXXXXXX)
cp -p /usr/bin/binachicken-agent "$backup/binachicken-agent"
cp -p /etc/binachicken/agent.json "$backup/agent.json"
echo "Rollback files: $backup"
dropin_dir=/etc/systemd/system/binachicken-agent.service.d
dropin=$dropin_dir/10-sync.conf
changed=0
rollback() {
  local result=$?
  trap - ERR
  if [[ $changed -eq 1 ]]; then
    echo 'Restoring the previous agent and configuration after a failed check.' >&2
    install -m0755 "$backup/binachicken-agent" /usr/bin/.binachicken-agent-restore && mv -f /usr/bin/.binachicken-agent-restore /usr/bin/binachicken-agent
    cp -p "$backup/agent.json" /etc/binachicken/agent.json
    rm -f "$dropin" /etc/xdg/autostart/binachicken-sync-helper.desktop /usr/bin/binachicken-sync-helper
    rmdir "$dropin_dir" 2>/dev/null || true
    systemctl daemon-reload || true
    systemctl restart binachicken-agent.service || true
  fi
  exit "$result"
}
trap rollback ERR

staged=$(mktemp /usr/bin/.binachicken-agent-update-XXXXXXXX)
install -m0755 -o root -g root "$bin_dir/binachicken-agent" "$staged"
install -m0755 -o root -g root "$bin_dir/binachicken-sync-helper" /usr/bin/binachicken-sync-helper
install -d -m0755 /etc/xdg/autostart
install -m0644 "$desktop" /etc/xdg/autostart/binachicken-sync-helper.desktop
install -d -m0755 "$dropin_dir"
cat > "$dropin" <<'UNIT'
[Service]
# Socket for the logged-in user's clipboard/file sync helper (peer-credential gated by the agent).
RuntimeDirectory=binachicken-agent
RuntimeDirectoryMode=0711
UNIT
chmod 0644 "$dropin"
SSH_PORT=$ssh_port python3 - <<'PY'
import json, os, tempfile
p = '/etc/binachicken/agent.json'
st = os.stat(p)
d = json.load(open(p))
d['sync_socket'] = '/run/binachicken-agent/sync.sock'
if os.environ.get('SSH_PORT'):
    d['ssh_port'] = int(os.environ['SSH_PORT'])
fd, tmp = tempfile.mkstemp(dir='/etc/binachicken')
with os.fdopen(fd, 'w') as f:
    json.dump(d, f, indent=2)
os.chown(tmp, st.st_uid, st.st_gid)
os.chmod(tmp, st.st_mode & 0o7777)
os.replace(tmp, p)
PY
changed=1
mv -f "$staged" /usr/bin/binachicken-agent
systemctl daemon-reload
systemctl restart binachicken-agent.service
for _ in 1 2 3 4 5 6 7 8; do sleep 1; systemctl is-active --quiet binachicken-agent.service || fail 'agent did not stay active'; done
[[ -S /run/binachicken-agent/sync.sock ]] || echo 'Note: the sync socket appears once a remote session starts.'
trap - ERR
[[ -z $ssh_port ]] || { systemctl is-active --quiet ssh.service || systemctl is-active --quiet sshd.service || echo "Warning: no sshd is running; SSH tunnels need one listening on 127.0.0.1:$ssh_port."; }
echo 'Agent updated and active. Start the helper in your desktop session now (or log out and in):'
echo '  nohup /usr/bin/binachicken-sync-helper >/dev/null 2>&1 &'
echo "To roll back: sudo $here/rollback-sync.sh $backup  (or restore files from $backup manually)"
