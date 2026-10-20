#!/usr/bin/env bash
set -euo pipefail
umask 077
[[ $EUID == 0 && $# == 1 ]] || { echo 'usage: sudo rollback.sh /var/lib/remotevnc-installer/backup-XXXXXXXX' >&2; exit 1; }
backup=$1
[[ "$backup" =~ ^/var/lib/remotevnc-installer/backup-[a-zA-Z0-9]+$ && ! -L "$backup" && -d "$backup" && $(stat -c '%u:%a' "$backup") == 0:700 && -f "$backup/enablement" ]] || { echo 'invalid private installer backup' >&2; exit 1; }
# Stop input producer before recovery owner. Never force-kill or time out guardian.
systemctl stop remotevnc-agent.service remotevnc-seat-helper.service
systemctl stop remotevnc-input-guardian.service
for path in etc/X11/default-display-manager etc/lightdm/lightdm.conf.d/50-remotevnc.conf etc/systemd/system/display-manager.service etc/systemd/system/graphical.target.wants/lightdm.service etc/systemd/system/multi-user.target.wants/lightdm.service; do
  rm -f "/$path"
  if [[ -e "$backup/$path" || -L "$backup/$path" ]]; then mkdir -p "/$(dirname "$path")"; cp -a "$backup/$path" "/$path"; fi
done
while read -r unit status; do
  case "$unit" in remotevnc-agent|remotevnc-seat-helper|remotevnc-input-guardian|lightdm) ;; *) exit 1 ;; esac
  case "$status" in
    enabled) systemctl enable "$unit.service" ;;
    enabled-runtime) systemctl enable --runtime "$unit.service" ;;
    *) systemctl disable "$unit.service" ;;
  esac
done < "$backup/enablement"
# Restore the previous display alias after enable/disable may have changed it.
rm -f /etc/systemd/system/display-manager.service
if [[ -L "$backup/etc/systemd/system/display-manager.service" || -e "$backup/etc/systemd/system/display-manager.service" ]]; then cp -a "$backup/etc/systemd/system/display-manager.service" /etc/systemd/system/display-manager.service; fi
if [[ -f "$backup/debconf-value" ]]; then
  current=$(printf 'GET shared/default-x-display-manager\n' | DEBIAN_FRONTEND=noninteractive debconf-communicate remotevnc-installer 2>/dev/null || true)
  if [[ ! -f "$backup/debconf-absent" || "$current" != '10 '* ]]; then
    replies=$(printf 'SET shared/default-x-display-manager %s\nFSET shared/default-x-display-manager seen %s\n' "$(cat "$backup/debconf-value")" "$(cat "$backup/debconf-seen")" | DEBIAN_FRONTEND=noninteractive debconf-communicate remotevnc-installer)
    while read -r code _detail; do [[ "$code" == 0 ]] || { echo 'Debconf restore failed; inspect private backup' >&2; exit 1; }; done <<< "$replies"
  fi
fi
systemctl daemon-reload
echo 'Previous display-manager configuration/selection and enablement restored. No desktop restart/reboot; reboot explicitly when safe. Installed packages, binaries and private enrollment remain.'
