#!/usr/bin/env python3
"""Linux/root regressions for physical Xorg discovery and ICE interface discovery.

Starts an owned sleeping process and transient systemd probes; changes no
installed service configuration. Run with both companion service files in
packaging/ubuntu available.
"""
import os
import pathlib
import shutil
import subprocess
import sys
import unittest
import uuid


@unittest.skipUnless(sys.platform == 'linux' and os.geteuid() == 0,
                     'requires Linux root with CAP_SYS_PTRACE')
class HelperExecutableLookup(unittest.TestCase):
    def test_bounded_helper_can_verify_a_root_xorg_executable(self):
        self.assertIsNotNone(shutil.which('setpriv'))
        unit = pathlib.Path(__file__).resolve().parents[1] / 'packaging/ubuntu/binachicken-seat-helper.service'
        capabilities = next(line.split('=', 1)[1].split() for line in unit.read_text().splitlines()
                            if line.startswith('CapabilityBoundingSet='))
        with subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)']) as target:
            try:
                probe = ('import os,sys; '
                         'sys.exit(0 if os.readlink(sys.argv[1]) else 2)')
                def query(caps):
                    bounding = '-all' + ''.join(',+' + cap.removeprefix('CAP_').lower() for cap in caps)
                    return subprocess.run(['setpriv', '--bounding-set=' + bounding,
                                           sys.executable, '-c', probe, f'/proc/{target.pid}/exe'],
                                          capture_output=True, timeout=5)
                before = query([cap for cap in capabilities if cap != 'CAP_SYS_PTRACE'])
                self.assertNotEqual(before.returncode, 0, 'old sandbox must reproduce the denied lookup')
                self.assertIn(b'PermissionError', before.stderr)
                after = query(capabilities)
                self.assertEqual(after.returncode, 0, after.stderr.decode())
            finally:
                target.terminate()
                target.wait(timeout=5)


@unittest.skipUnless(sys.platform == 'linux' and os.geteuid() == 0,
                     'requires Linux root and systemd')
class AgentInterfaceLookup(unittest.TestCase):
    def test_ice_agent_can_enumerate_interfaces_in_its_systemd_sandbox(self):
        unit = pathlib.Path(__file__).resolve().parents[1] / 'packaging/ubuntu/binachicken-agent.service'
        families = next(line.split('=', 1)[1].split() for line in unit.read_text().splitlines()
                        if line.startswith('RestrictAddressFamilies='))
        probe = ('import ctypes; c=ctypes.CDLL(None,use_errno=True); p=ctypes.c_void_p(); '
                 'r=c.getifaddrs(ctypes.byref(p)); print(r,ctypes.get_errno()); '
                 'c.freeifaddrs(p) if r==0 else None')
        def query(allowed):
            return subprocess.run(['systemd-run', '--quiet', '--wait', '--pipe', '--collect',
                                   '--unit=binachicken-netlink-test-' + uuid.uuid4().hex,
                                   '-p', 'RestrictAddressFamilies=' + ' '.join(allowed),
                                   sys.executable, '-c', probe],capture_output=True,timeout=10)
        before = query([family for family in families if family != 'AF_NETLINK'])
        self.assertEqual(before.returncode, 0, before.stderr.decode())
        self.assertEqual(before.stdout.strip(), b'-1 97')
        after = query(families)
        self.assertEqual(after.returncode, 0, after.stderr.decode())
        self.assertEqual(after.stdout.strip(), b'0 0')


if __name__ == '__main__':
    unittest.main(verbosity=2)
