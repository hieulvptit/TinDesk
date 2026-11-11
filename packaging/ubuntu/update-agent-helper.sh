#!/bin/bash
# Update the agent and the seat helper/input guardian binary from a reviewed,
# checksum-verified deployment kit. The guardian runs the helper binary, so all
# three services restart; the running desktop session is not touched.
set -Eeuo pipefail
[[ $EUID -eq 0 ]] || { echo 'Run with sudo in your Terminal.' >&2; exit 1; }
kit=$(cd -- "$(dirname -- "$0")" && pwd)
for cmd in sha256sum systemctl install mktemp runuser awk sleep; do command -v "$cmd" >/dev/null; done
binaries=(binachicken-agent binachicken-seat-helper)
for name in "${binaries[@]}"; do
    [[ -f "$kit/$name" ]] || { echo "Incomplete update kit: $name missing." >&2; exit 1; }
    expected=$(awk -v n="$name" '$2 == n && length($1) == 64 {print $1}' "$kit/SHA256SUMS")
    [[ "$expected" =~ ^[0-9a-f]{64}$ ]] || { echo "Expected exactly one $name checksum." >&2; exit 1; }
    [[ ! -L "/usr/bin/$name" && -f "/usr/bin/$name" ]] || { echo "Expected an existing regular /usr/bin/$name." >&2; exit 1; }
done
(cd "$kit" && sha256sum --check --strict SHA256SUMS)
families=$(systemctl show binachicken-agent.service --property=RestrictAddressFamilies --value)
[[ " $families " == *' AF_NETLINK '* ]] || { echo 'Apply fix-agent-ice.sh first: AF_NETLINK is required.' >&2; exit 1; }
services=(binachicken-input-guardian binachicken-seat-helper binachicken-agent)
for service in "${services[@]}"; do systemctl is-active "$service.service"; done
install -d -m0700 /var/lib/binachicken-installer
backup=$(mktemp -d /var/lib/binachicken-installer/agent-helper-update-backup-XXXXXXXX)
for name in "${binaries[@]}"; do install -m0755 "/usr/bin/$name" "$backup/$name"; done
install -m0644 "$kit/SHA256SUMS" "$backup/new-SHA256SUMS"
echo "Rollback files: $backup"
declare -A staged
for name in "${binaries[@]}"; do staged[$name]=$(mktemp "/usr/bin/.$name-update-XXXXXXXX"); done
changed=0
start_all() {
    systemctl start binachicken-input-guardian.service
    systemctl start binachicken-seat-helper.service
    systemctl start binachicken-agent.service
}
rollback() {
    result=$?
    trap - ERR
    if [[ $changed -eq 1 ]]; then
        echo 'Restoring the previous binaries after a failed service check.' >&2
        systemctl stop binachicken-agent.service binachicken-seat-helper.service binachicken-input-guardian.service || true
        for name in "${binaries[@]}"; do
            install -m0755 "$backup/$name" "${staged[$name]}"
            mv -f "${staged[$name]}" "/usr/bin/$name"
        done
        start_all || true
    else
        for name in "${binaries[@]}"; do rm -f "${staged[$name]}"; done
    fi
    exit "$result"
}
trap rollback ERR
for name in "${binaries[@]}"; do
    install -m0755 -o root -g root "$kit/$name" "${staged[$name]}"
    expected=$(awk -v n="$name" '$2 == n {print $1}' "$kit/SHA256SUMS")
    actual=$(sha256sum "${staged[$name]}"); actual=${actual%% *}
    [[ "$actual" == "$expected" ]]
done
changed=1
# Agent first so no session is active while helper input/guardian restart.
systemctl stop binachicken-agent.service
systemctl stop binachicken-seat-helper.service
systemctl stop binachicken-input-guardian.service
for name in "${binaries[@]}"; do mv -f "${staged[$name]}" "/usr/bin/$name"; done
systemctl start binachicken-input-guardian.service
systemctl start binachicken-seat-helper.service
# doctor checks helper IPC readiness, so it runs against the new running helper.
# systemd reports "started" before the helper has bound its socket.
for _ in $(seq 100); do [[ -S /run/binachicken/helper.sock ]] && break; sleep 0.1; done
runuser -u binachicken-agent -- /usr/bin/binachicken-agent doctor --config /etc/binachicken/agent.json
systemctl start binachicken-agent.service
sleep 3
for service in "${services[@]}"; do systemctl is-active "$service.service"; done
trap - ERR
echo 'Agent and seat helper updated. Reconnect from the Mac client; no reboot or enrollment is needed.'
