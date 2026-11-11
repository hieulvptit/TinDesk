#!/bin/bash
# Repair the executable identity probe on an existing BinaChicken installation.
set -Eeuo pipefail
[[ $EUID -eq 0 ]] || { echo 'Run this script with sudo in your Terminal.' >&2; exit 1; }
for cmd in python3 setpriv systemctl install mktemp; do command -v "$cmd" >/dev/null; done
unit=binachicken-seat-helper.service
systemctl cat "$unit" >/dev/null

# Probe the actual physical Xorg process. No cookies, keys or credentials are read.
python3 - <<'PY'
import os,pathlib,subprocess,sys
xorg=[]
for path in pathlib.Path('/proc').iterdir():
    if not path.name.isdigit(): continue
    try:
        if (path/'comm').read_text().strip() != 'Xorg': continue
        executable=pathlib.Path(os.readlink(path/'exe'))
        if executable.name=='Xorg' and executable.stat().st_uid==0:
            xorg.append(str(path/'exe'))
    except (OSError,ValueError): continue
if len(xorg)!=1:
    sys.exit('Expected exactly one physical Xorg process; no changes made.')
caps=['dac_read_search','chown','fowner','kill','setuid','setgid']
query='import os,sys; os.readlink(sys.argv[1])'
def check(extra):
    return subprocess.run(['setpriv','--bounding-set=-all'+''.join(',+'+c for c in caps+extra),
                           sys.executable,'-c',query,xorg[0]],capture_output=True,timeout=5)
before=check([]);after=check(['sys_ptrace'])
print('Xorg executable lookup with original sandbox:', 'allowed' if before.returncode==0 else 'denied')
print('Xorg executable lookup with CAP_SYS_PTRACE:', 'allowed' if after.returncode==0 else 'denied')
if before.returncode==0 or after.returncode!=0:
    sys.exit('The expected permission failure was not reproduced; no changes made.')
PY

# Confirm that the installed binary can use this X11 display with the patched budget.
setpriv --bounding-set=-all,+dac_read_search,+chown,+fowner,+kill,+setuid,+setgid,+sys_ptrace \
    /usr/bin/binachicken-seat-helper doctor

install -d -m0700 /var/lib/binachicken-installer
backup=$(mktemp -d /var/lib/binachicken-installer/xorg-probe-backup-XXXXXXXX)
dropdir=/etc/systemd/system/binachicken-seat-helper.service.d
dropfile=$dropdir/90-xorg-probe.conf
[[ ! -L "$dropdir" && ! -L "$dropfile" ]] || { echo 'Refusing a symlinked service override.' >&2; exit 1; }
if [[ -e "$dropfile" ]]; then
    [[ -f "$dropfile" ]] || exit 1
    cp -p "$dropfile" "$backup/previous.conf"
else
    touch "$backup/was-absent"
fi
cat > "$backup/new.conf" <<'UNIT'
[Service]
# Xorg executable identity lookup is gated by the kernel's ptrace permission check.
CapabilityBoundingSet=
CapabilityBoundingSet=CAP_DAC_READ_SEARCH CAP_CHOWN CAP_FOWNER CAP_KILL CAP_SETUID CAP_SETGID CAP_SYS_PTRACE
UNIT
chmod 0600 "$backup/new.conf"
echo "Rollback files: $backup"
changed=0
rollback() {
    result=$?
    trap - ERR
    if [[ $changed -eq 1 ]]; then
        echo 'Applying rollback after a failed service check.' >&2
        if [[ -f "$backup/was-absent" ]]; then
            python3 - "$dropfile" <<'PY'
import pathlib,sys
pathlib.Path(sys.argv[1]).unlink(missing_ok=True)
PY
        else
            install -m0644 "$backup/previous.conf" "$dropfile"
        fi
        systemctl daemon-reload
        systemctl restart binachicken-seat-helper.service || true
        systemctl start binachicken-agent.service || true
    fi
    exit "$result"
}
trap rollback ERR
install -d -m0755 "$dropdir"
changed=1
install -m0644 "$backup/new.conf" "$dropfile"
systemctl stop binachicken-agent.service
systemctl daemon-reload
# The independent input guardian remains running throughout this operation.
systemctl restart binachicken-seat-helper.service
systemctl start binachicken-agent.service
systemctl is-active binachicken-seat-helper.service
systemctl is-active binachicken-input-guardian.service
systemctl is-active binachicken-agent.service
trap - ERR
echo 'Xorg probe permission fixed. Connect again from the Mac client.'
