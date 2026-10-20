#!/usr/bin/env python3
"""Installer integration checks in disposable chroots, never on the active desktop.

Run as root on Linux; creates its own private mount namespace. Only apt/systemctl
are stand-ins: filesystem writes, account creation, ownership and rollback are real.
"""
import os
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parent.parent
if sys.platform != "linux" or os.geteuid() != 0:
    sys.exit("requires disposable Linux root (for example sudo inside the test Lima VM)")
if "--inside" not in sys.argv:
    os.execvp("unshare", ["unshare", "--mount", "--propagation", "private", sys.executable, __file__, "--inside"])
sys.argv.remove("--inside")


class Installer(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="remotevnc-install-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.root.chmod(0o755)
        for directory in ("bin", "lib", "lib64", "dev"):
            source = Path("/") / directory
            if source.exists():
                dest = self.root / directory
                dest.mkdir()
                subprocess.run(["mount", "--bind", str(source), str(dest)], check=True)
                self.addCleanup(subprocess.run, ["umount", str(dest)], check=True)
                subprocess.run(["mount", "-o", "remount,bind,ro", str(dest)], check=True)
        (self.root / "usr").mkdir()
        (self.root / "overlay-upper").mkdir()
        (self.root / "overlay-work").mkdir()
        subprocess.run(["mount", "-t", "overlay", "overlay", "-o", f"lowerdir=/usr,upperdir={self.root}/overlay-upper,workdir={self.root}/overlay-work", str(self.root / "usr")], check=True)
        self.addCleanup(subprocess.run, ["umount", str(self.root / "usr")], check=True)
        for directory in ("etc/lightdm/lightdm.conf.d", "etc/systemd/system", "var/lib", "run", "tmp", "test-bin", "work", "artifacts"):
            (self.root / directory).mkdir(parents=True, exist_ok=True)
        shutil.copytree(REPO / "packaging", self.root / "work/packaging")
        (self.root / "etc/os-release").write_text('ID=ubuntu\nVERSION_ID="26.04"\n')
        for name in ("passwd", "group", "shadow", "gshadow"):
            (self.root / "etc" / name).write_text({"passwd": "root:x:0:0:root:/root:/bin/bash\n", "group": "root:x:0:\n", "shadow": "root:!:1:0:99999:7:::\n", "gshadow": "root:!::\n"}[name])
        for name in ("login.defs", "nsswitch.conf"):
            shutil.copyfile(Path("/etc") / name, self.root / "etc" / name)
        shutil.copytree("/etc/pam.d", self.root / "etc/pam.d")
        (self.root / "etc/X11").mkdir()
        (self.root / "etc/X11/default-display-manager").write_text("/usr/sbin/gdm3\n")
        (self.root / "etc/lightdm/lightdm.conf.d/50-remotevnc.conf").write_text("previous settings\n")
        (self.root / "etc/systemd/system/display-manager.service").symlink_to("/usr/lib/systemd/system/gdm3.service")
        for name in ("remotevnc-agent", "remotevnc-seat-helper"):
            path = self.root / "artifacts" / name
            path.write_text("#!/bin/bash\nexit 0\n")
            path.chmod(0o755)
        (self.root / "usr/share/xsessions").mkdir(parents=True, exist_ok=True)
        (self.root / "usr/share/xsessions/xfce.desktop").write_text('[Desktop Entry]\n')
        (self.root / "etc/debconf-value").write_text('gdm3')
        (self.root / "etc/debconf-seen").write_text('false')
        for name, body in {
            "apt-get": 'echo "apt $*" >> /commands; /usr/sbin/policy-rc.d; [[ $? == 101 ]] || exit 9; echo /usr/sbin/lightdm > /etc/X11/default-display-manager; exit ${APT_STATUS:-0}',
            "lightdm": 'exit 0',
            "light-locker": 'exit 0',
            "systemctl": 'echo "systemctl $*" >> /commands; if [[ "$1" == is-enabled ]]; then exit 1; fi; if [[ "$*" == "enable --force lightdm.service" ]]; then ln -sf /usr/lib/systemd/system/lightdm.service /etc/systemd/system/display-manager.service; fi',
            "gst-inspect-1.0": 'exit 0',
            "debconf-set-selections": 'while read -r owner question kind value; do if [[ "$kind" == select ]]; then printf "%s" "$value" > /etc/debconf-value; echo true > /etc/debconf-seen; fi; done',
            "debconf-communicate": 'while read -r verb question field value; do case "$verb" in GET) if [[ -f /etc/debconf-value ]]; then echo "0 $(cat /etc/debconf-value)"; else echo "10 missing"; fi ;; FGET) echo "0 $(cat /etc/debconf-seen)" ;; SET) printf "%s" "$field" > /etc/debconf-value; echo 0 ;; FSET) printf "%s" "$value" > /etc/debconf-seen; echo 0 ;; esac; done',
            "uname": 'echo 6.17.0',
        }.items():
            path = self.root / "test-bin" / name
            path.write_text("#!/bin/bash\n" + body + "\n")
            path.chmod(0o755)

    def run_install(self, *args, success=True, extra=()):
        result = subprocess.run(["chroot", str(self.root), "/usr/bin/env", "PATH=/test-bin:/usr/sbin:/usr/bin:/sbin:/bin", *extra,
                                 "/bin/bash", "/work/packaging/ubuntu/install.sh", *args, "--bin-dir", "/artifacts"], text=True, capture_output=True)
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        return result

    def snapshot(self):
        return [(str(p.relative_to(self.root)), p.lstat().st_mode, p.read_bytes() if p.is_file() and not p.is_symlink() else b"")
                for folder in ("etc", "var", "run", "tmp") for p in sorted((self.root / folder).rglob("*"))]

    def test_dry_run_changes_nothing(self):
        before = self.snapshot()
        self.run_install("--dry-run")
        after = self.snapshot()
        self.assertEqual(before, after)

    def test_invalid_prerequisites_fail_before_writes(self):
        (self.root / "artifacts/remotevnc-agent").unlink()
        self.run_install("--prepare", success=False)
        self.assertFalse((self.root / "var/lib/remotevnc-installer").exists())
        self.assertFalse((self.root / "commands").exists())

    def test_prepare_private_state_no_service_start_and_apt_failure_cleanup(self):
        policy = self.root / "usr/sbin/policy-rc.d"  # private overlay, never host /usr
        policy.write_text('#!/bin/sh\nexit 7\n')
        policy.chmod(0o755)
        self.run_install("--prepare")
        self.assertEqual((self.root / "etc/debconf-value").read_text().strip(), 'gdm3')
        self.assertEqual((self.root / "etc/debconf-seen").read_text().strip(), 'false')
        self.assertEqual(policy.read_text(), '#!/bin/sh\nexit 7\n')
        commands = (self.root / "commands").read_text()
        self.assertNotRegex(commands, r"systemctl .*\b(start|restart|enable|stop)\b")
        self.assertEqual((self.root / "etc/X11/default-display-manager").read_text(), "/usr/sbin/gdm3\n")
        self.assertEqual((self.root / "etc/remotevnc/agent.json").stat().st_mode & 0o777, 0o640)
        self.assertEqual((self.root / "var/lib/remotevnc-agent").stat().st_mode & 0o777, 0o700)
        self.assertEqual((self.root / "run/remotevnc").stat().st_mode & 0o777, 0o711)
        self.run_install("--prepare", success=False, extra=("APT_STATUS=42",))
        self.assertEqual((self.root / "etc/debconf-value").read_text().strip(), 'gdm3')
        self.assertEqual((self.root / "etc/debconf-seen").read_text().strip(), 'false')
        self.assertEqual(policy.read_text(), '#!/bin/sh\nexit 7\n')
        self.assertEqual((self.root / "etc/X11/default-display-manager").read_text(), "/usr/sbin/gdm3\n")

    def test_prepare_without_previous_debconf_choice_leaves_new_question_unselected(self):
        (self.root / "etc/debconf-value").unlink()
        (self.root / "etc/debconf-seen").unlink()
        self.run_install("--prepare")
        self.assertEqual((self.root / "etc/debconf-value").read_text().strip(), '')
        self.assertEqual((self.root / "etc/debconf-seen").read_text().strip(), 'false')

    def test_untraversable_existing_socket_parent_fails_without_chmod(self):
        (self.root / "run/remotevnc").mkdir(mode=0o700)
        self.run_install("--prepare", success=False)
        self.assertEqual((self.root / "run/remotevnc").stat().st_mode & 0o777, 0o700)
        self.assertFalse((self.root / "commands").exists())

    def test_apply_requires_enrollment_then_rollback_restores_dm(self):
        self.run_install("--prepare")
        self.run_install("--apply", success=False)
        self.assertEqual((self.root / "etc/X11/default-display-manager").read_text(), "/usr/sbin/gdm3\n")
        # Executable fixture above represents successful owner-only `doctor`/credential check.
        identity = self.root / "var/lib/remotevnc-agent/identity.json"
        identity.write_text(json.dumps(dict(version=1,device_id="00000000-0000-4000-8000-000000000001",secret=[1]*32)))
        identity.chmod(0o600)
        os.chown(identity, (self.root / "var/lib/remotevnc-agent").stat().st_uid, (self.root / "var/lib/remotevnc-agent").stat().st_gid)
        self.run_install("--apply")
        self.assertEqual((self.root / "etc/X11/default-display-manager").read_text(), "/usr/sbin/lightdm\n")
        self.assertEqual((self.root / "etc/debconf-value").read_text().strip(), 'lightdm')
        self.assertEqual((self.root / "etc/debconf-seen").read_text().strip(), 'true')
        self.assertEqual(os.readlink(self.root / "etc/systemd/system/display-manager.service"), "/usr/lib/systemd/system/lightdm.service")
        commands = (self.root / "commands").read_text()
        starts = [line for line in commands.splitlines() if line.startswith('systemctl start ')]
        self.assertEqual(starts, ['systemctl start remotevnc-input-guardian.service','systemctl start remotevnc-seat-helper.service','systemctl start remotevnc-agent.service'])
        backups = sorted((self.root / "var/lib/remotevnc-installer").glob("backup-*"))
        self.assertTrue(backups)
        result = subprocess.run(["chroot", str(self.root), "/usr/bin/env", "PATH=/test-bin:/usr/sbin:/usr/bin:/sbin:/bin", "/bin/bash", "/work/packaging/ubuntu/rollback.sh", "/" + str(backups[-1].relative_to(self.root))], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.root / "etc/lightdm/lightdm.conf.d/50-remotevnc.conf").read_text(), "previous settings\n")
        self.assertEqual((self.root / "etc/X11/default-display-manager").read_text(), "/usr/sbin/gdm3\n")
        self.assertEqual(os.readlink(self.root / "etc/systemd/system/display-manager.service"), "/usr/lib/systemd/system/gdm3.service")
        self.assertEqual((self.root / "etc/debconf-value").read_text().strip(), 'gdm3')
        self.assertEqual((self.root / "etc/debconf-seen").read_text().strip(), 'false')


unittest.main()
