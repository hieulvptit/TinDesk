#!/bin/bash
# Restore libnice interface discovery inside the installed agent's systemd sandbox.
set -Eeuo pipefail
[[ $EUID -eq 0 ]] || { echo 'Run this script with sudo in your Terminal.' >&2; exit 1; }
for cmd in python3 systemd-run systemctl install mktemp; do command -v "$cmd" >/dev/null; done
systemctl cat binachicken-agent.service >/dev/null
probe='import ctypes,sys; c=ctypes.CDLL(None,use_errno=True); p=ctypes.c_void_p(); r=c.getifaddrs(ctypes.byref(p)); e=ctypes.get_errno(); print("interface discovery:", "allowed" if r==0 else "denied", "errno", e); c.freeifaddrs(p) if r==0 else None; sys.exit(0 if ((r==0)==(sys.argv[1]=="allowed")) else 1)'
# Both probes use the actual unprivileged agent account and systemd socket filter.
systemd-run --quiet --wait --pipe --collect --unit="binachicken-ice-before-$$" \
    -p User=binachicken-agent -p 'RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6' \
    /usr/bin/python3 -c "$probe" denied
systemd-run --quiet --wait --pipe --collect --unit="binachicken-ice-after-$$" \
    -p User=binachicken-agent -p 'RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK' \
    /usr/bin/python3 -c "$probe" allowed
install -d -m0700 /var/lib/binachicken-installer
backup=$(mktemp -d /var/lib/binachicken-installer/ice-network-backup-XXXXXXXX)
dropdir=/etc/systemd/system/binachicken-agent.service.d
dropfile=$dropdir/90-ice-interfaces.conf
[[ ! -L "$dropdir" && ! -L "$dropfile" ]] || { echo 'Refusing a symlinked service override.' >&2; exit 1; }
if [[ -e "$dropfile" ]]; then
    [[ -f "$dropfile" ]] || exit 1
    cp -p "$dropfile" "$backup/previous.conf"
else
    touch "$backup/was-absent"
fi
cat > "$backup/new.conf" <<'UNIT'
[Service]
# NETLINK_ROUTE is required by getifaddrs/libnice for ICE candidate gathering.
RestrictAddressFamilies=
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK
UNIT
chmod 0600 "$backup/new.conf"
echo "Rollback files: $backup"
changed=0
rollback() {
    result=$?
    trap - ERR
    if [[ $changed -eq 1 ]]; then
        if [[ -f "$backup/was-absent" ]]; then
            python3 - "$dropfile" <<'PY'
import pathlib,sys
pathlib.Path(sys.argv[1]).unlink(missing_ok=True)
PY
        else
            install -m0644 "$backup/previous.conf" "$dropfile"
        fi
        systemctl daemon-reload
        systemctl restart binachicken-agent.service || true
    fi
    exit "$result"
}
trap rollback ERR
install -d -m0755 "$dropdir"
changed=1
install -m0644 "$backup/new.conf" "$dropfile"
systemctl stop binachicken-agent.service
systemctl daemon-reload
systemctl start binachicken-agent.service
systemctl is-active binachicken-agent.service
systemctl is-active binachicken-seat-helper.service
systemctl is-active binachicken-input-guardian.service
trap - ERR
echo 'ICE interface discovery enabled. Reconnect from the Mac client.'
