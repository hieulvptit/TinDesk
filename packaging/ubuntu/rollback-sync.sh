#!/usr/bin/env bash
# Undo install-sync.sh using the backup directory it printed.
set -Eeuo pipefail
[[ $EUID -eq 0 ]] || { echo 'run with sudo' >&2; exit 1; }
backup=${1:?usage: rollback-sync.sh /var/lib/binachicken-installer/sync-backup-XXXXXXXX}
[[ -f $backup/binachicken-agent && -f $backup/agent.json ]] || { echo 'not a sync backup directory' >&2; exit 1; }
install -m0755 "$backup/binachicken-agent" /usr/bin/.binachicken-agent-restore && mv -f /usr/bin/.binachicken-agent-restore /usr/bin/binachicken-agent
cp -p "$backup/agent.json" /etc/binachicken/agent.json
rm -f /etc/systemd/system/binachicken-agent.service.d/10-sync.conf /etc/xdg/autostart/binachicken-sync-helper.desktop /usr/bin/binachicken-sync-helper
rmdir /etc/systemd/system/binachicken-agent.service.d 2>/dev/null || true
pkill -x binachicken-sync-helper 2>/dev/null || true
systemctl daemon-reload
systemctl restart binachicken-agent.service
systemctl is-active binachicken-agent.service
