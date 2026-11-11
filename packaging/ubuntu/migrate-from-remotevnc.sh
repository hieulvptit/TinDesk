#!/bin/bash
# One-time rename of an installed RemoteVNC agent host to BinaChicken. Keeps the
# enrolled device identity (same UID, same state files), so no re-enrollment is
# needed. Any failure restores the previous names, files and running services.
set -Eeuo pipefail
[[ $EUID -eq 0 ]] || { echo 'Run with sudo in your Terminal.' >&2; exit 1; }
kit=$(cd -- "$(dirname -- "$0")" && pwd)
for cmd in sha256sum systemctl install mktemp runuser python3 usermod groupmod getent; do command -v "$cmd" >/dev/null; done
old=remotevnc new=binachicken
binaries=(agent seat-helper)
units=(input-guardian seat-helper agent)
for name in "${binaries[@]}"; do [[ -f "$kit/$new-$name" ]] || { echo "Kit missing $new-$name." >&2; exit 1; }; done
for unit in "${units[@]}"; do [[ -f "$kit/$new-$unit.service" ]] || { echo "Kit missing $new-$unit.service." >&2; exit 1; }; done
(cd "$kit" && sha256sum --check --strict SHA256SUMS)
# Only migrate the exact managed layout; refuse a partial or already-migrated host.
getent passwd "$old-agent" >/dev/null || { echo "No $old-agent account; nothing to migrate." >&2; exit 1; }
! getent passwd "$new-agent" >/dev/null || { echo "$new-agent account already exists." >&2; exit 1; }
! getent group "$new-agent" >/dev/null || { echo "$new-agent group already exists." >&2; exit 1; }
for path in /etc/$old /var/lib/$old-agent /usr/bin/$old-agent /usr/bin/$old-seat-helper; do
    [[ -e "$path" && ! -L "$path" ]] || { echo "Expected managed path $path." >&2; exit 1; }
done
for path in /etc/$new /var/lib/$new-agent /usr/bin/$new-agent /usr/bin/$new-seat-helper; do
    [[ ! -e "$path" && ! -L "$path" ]] || { echo "$path already exists; inspect it before migrating." >&2; exit 1; }
done
for unit in "${units[@]}"; do
    [[ -f /etc/systemd/system/$old-$unit.service ]] || { echo "Missing $old-$unit.service." >&2; exit 1; }
    [[ ! -e /etc/systemd/system/$new-$unit.service ]] || { echo "$new-$unit.service already installed." >&2; exit 1; }
done
uid=$(id -u "$old-agent")
install -d -m0700 /var/lib/$new-installer
backup=$(mktemp -d /var/lib/$new-installer/migrate-from-$old-XXXXXXXX)
echo "Backup: $backup"
cp -a /etc/$old "$backup/etc-$old"
for name in "${binaries[@]}"; do cp -a "/usr/bin/$old-$name" "$backup/"; done
for unit in "${units[@]}"; do
    cp -a "/etc/systemd/system/$old-$unit.service" "$backup/"
    [[ -d /etc/systemd/system/$old-$unit.service.d ]] && cp -a "/etc/systemd/system/$old-$unit.service.d" "$backup/"
    printf '%s %s\n' "$unit" "$(systemctl is-enabled "$old-$unit.service" 2>/dev/null || true)" >> "$backup/enablement"
done
lightdm_old=/etc/lightdm/lightdm.conf.d/50-$old.conf
lightdm_new=/etc/lightdm/lightdm.conf.d/50-$new.conf
[[ -f $lightdm_old ]] && cp -a "$lightdm_old" "$backup/"
step=0
rollback() {
    result=$?
    trap - ERR
    echo "Migration failed at step $step; restoring $old." >&2
    systemctl stop "$new-agent.service" "$new-seat-helper.service" "$new-input-guardian.service" 2>/dev/null || true
    for unit in "${units[@]}"; do
        systemctl disable "$new-$unit.service" 2>/dev/null || true
        rm -f "/etc/systemd/system/$new-$unit.service"
    done
    for name in "${binaries[@]}"; do rm -f "/usr/bin/$new-$name"; done
    if [[ -f $lightdm_new ]]; then mv -f "$lightdm_new" "$lightdm_old"; fi
    if ((step >= 3)) && [[ -d /var/lib/$new-agent ]]; then mv /var/lib/$new-agent /var/lib/$old-agent; fi
    if ((step >= 2)); then
        getent passwd "$new-agent" >/dev/null && usermod -l "$old-agent" -d "/var/lib/$old-agent" "$new-agent"
        getent group "$new-agent" >/dev/null && groupmod -n "$old-agent" "$new-agent"
    fi
    if [[ -d /etc/${new:?} ]]; then rm -rf "/etc/${new:?}"; fi
    rm -rf "/etc/${old:?}" && cp -a "$backup/etc-$old" "/etc/$old"
    for name in "${binaries[@]}"; do cp -a "$backup/$old-$name" /usr/bin/; done
    for unit in "${units[@]}"; do
        cp -a "$backup/$old-$unit.service" /etc/systemd/system/
        [[ -d $backup/$old-$unit.service.d ]] && rm -rf "/etc/systemd/system/$old-$unit.service.d" && cp -a "$backup/$old-$unit.service.d" /etc/systemd/system/
    done
    systemctl daemon-reload
    while read -r unit state; do [[ $state == enabled ]] && systemctl enable "$old-$unit.service" || true; done < "$backup/enablement"
    systemctl start "$old-input-guardian.service" "$old-seat-helper.service" "$old-agent.service" || true
    echo "Restored $old. Backup kept at $backup." >&2
    exit "$result"
}
trap rollback ERR
step=1
systemctl stop "$old-agent.service"
systemctl stop "$old-seat-helper.service"
systemctl stop "$old-input-guardian.service"
for unit in "${units[@]}"; do systemctl disable "$old-$unit.service" 2>/dev/null || true; done
step=2
# Rename in place: the UID/GID and therefore all state ownership stay identical.
groupmod -n "$new-agent" "$old-agent"
usermod -l "$new-agent" -d "/var/lib/$new-agent" "$old-agent"
step=3
mv /var/lib/$old-agent /var/lib/$new-agent
[[ $(stat -c '%u:%a' /var/lib/$new-agent) == "$uid:700" ]]
step=4
mv /etc/$old /etc/$new
python3 - "/etc/$new/agent.json" <<'PY'
import json, os, sys
path = sys.argv[1]
st = os.lstat(path)
data = json.loads(open(path, 'rb').read())
assert data['identity_path'] == '/var/lib/remotevnc-agent' and data['helper_socket'] == '/run/remotevnc/helper.sock'
data['identity_path'] = '/var/lib/binachicken-agent'
data['helper_socket'] = '/run/binachicken/helper.sock'
tmp = path + '.new'
with open(tmp, 'w') as f:
    f.write(json.dumps(data, indent=2) + '\n')
os.chown(tmp, st.st_uid, st.st_gid)
os.chmod(tmp, st.st_mode & 0o7777)
os.replace(tmp, path)
PY
step=5
for name in "${binaries[@]}"; do install -m0755 -o root -g root "$kit/$new-$name" "/usr/bin/$new-$name"; done
for unit in "${units[@]}"; do install -m0644 "$kit/$new-$unit.service" "/etc/systemd/system/$new-$unit.service"; done
if [[ -f $lightdm_old ]]; then mv -f "$lightdm_old" "$lightdm_new"; fi
systemctl daemon-reload
step=6
systemctl start "$new-input-guardian.service"
systemctl start "$new-seat-helper.service"
# systemd reports "started" before the helper has bound its socket.
for _ in $(seq 100); do [[ -S /run/$new/helper.sock ]] && break; sleep 0.1; done
runuser -u "$new-agent" -- "/usr/bin/$new-agent" doctor --config "/etc/$new/agent.json"
systemctl start "$new-agent.service"
sleep 3
for unit in "${units[@]}"; do systemctl is-active "$new-$unit.service"; done
for unit in "${units[@]}"; do systemctl enable "$new-$unit.service"; done
trap - ERR
# Success: retire the old names. Their copies remain in the backup.
for unit in "${units[@]}"; do
    rm -f "/etc/systemd/system/$old-$unit.service"
    rm -rf "/etc/systemd/system/$old-$unit.service.d"
done
for name in "${binaries[@]}"; do rm -f "/usr/bin/$old-$name"; done
if [[ -d /var/lib/$old-installer ]]; then mv /var/lib/$old-installer "/var/lib/$new-installer/legacy-$old-installer"; fi
systemctl daemon-reload
echo "Migrated to BinaChicken. Device identity kept; no re-enrollment needed. Backup: $backup"
