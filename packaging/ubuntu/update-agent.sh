#!/bin/bash
# Update only the outbound agent from a reviewed, checksum-verified deployment kit.
set -Eeuo pipefail
[[ $EUID -eq 0 ]] || { echo 'Run with sudo in your Terminal.' >&2; exit 1; }
kit=$(cd -- "$(dirname -- "$0")" && pwd)
for cmd in sha256sum systemctl install mktemp runuser awk sleep; do command -v "$cmd" >/dev/null; done
[[ -f "$kit/binachicken-agent" && -f "$kit/SHA256SUMS" ]] || { echo 'Incomplete agent update kit.' >&2; exit 1; }
expected=$(awk '$2 == "binachicken-agent" && length($1) == 64 {print $1}' "$kit/SHA256SUMS")
[[ "$expected" =~ ^[0-9a-f]{64}$ ]] || { echo 'Expected exactly one agent checksum.' >&2; exit 1; }
(cd "$kit" && sha256sum --check --strict SHA256SUMS)
[[ ! -L /usr/bin/binachicken-agent && -f /usr/bin/binachicken-agent ]] || { echo 'Expected an existing regular agent binary.' >&2; exit 1; }
families=$(systemctl show binachicken-agent.service --property=RestrictAddressFamilies --value)
[[ " $families " == *' AF_NETLINK '* ]] || { echo 'Apply fix-agent-ice.sh first: AF_NETLINK is required.' >&2; exit 1; }
systemctl is-active binachicken-seat-helper.service
systemctl is-active binachicken-input-guardian.service
install -d -m0700 /var/lib/binachicken-installer
backup=$(mktemp -d /var/lib/binachicken-installer/agent-update-backup-XXXXXXXX)
install -m0755 /usr/bin/binachicken-agent "$backup/binachicken-agent"
install -m0644 "$kit/SHA256SUMS" "$backup/new-SHA256SUMS"
echo "Rollback files: $backup"
staged=$(mktemp /usr/bin/.binachicken-agent-update-XXXXXXXX)
changed=0
rollback() {
    result=$?
    trap - ERR
    if [[ $changed -eq 1 ]]; then
        echo 'Restoring the previous agent after a failed service check.' >&2
        install -m0755 "$backup/binachicken-agent" "$staged"
        mv -f "$staged" /usr/bin/binachicken-agent
        systemctl restart binachicken-agent.service || true
    else
        rm -f "$staged"
    fi
    exit "$result"
}
trap rollback ERR
install -m0755 -o root -g root "$kit/binachicken-agent" "$staged"
actual=$(sha256sum "$staged"); actual=${actual%% *}
[[ "$actual" == "$expected" ]]
changed=1
systemctl stop binachicken-agent.service
mv -f "$staged" /usr/bin/binachicken-agent
runuser -u binachicken-agent -- /usr/bin/binachicken-agent doctor --config /etc/binachicken/agent.json
systemctl start binachicken-agent.service
sleep 3
systemctl is-active binachicken-agent.service
systemctl is-active binachicken-seat-helper.service
systemctl is-active binachicken-input-guardian.service
trap - ERR
echo 'Agent updated. Open the new Mac client and reconnect; no reboot or enrollment is needed.'
