#!/usr/bin/env python3
"""Run the packaged entrypoint against fake secrets and a harmless process probe."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ENTRYPOINT = Path(__file__).resolve().parents[1] / 'packaging/backend/entrypoint.sh'


class BackendEntrypointTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.google = self.root / 'google'
        self.google.write_text('fixture-google-secret\n')
        self.token = self.root / 'token'
        self.token.write_text('fixture-turn-token\n')
        self.keys = self.root / 'keys.json'
        self.keys.write_text('{"keys":[{"key_id":"fixture-key","api_token":"fixture-file-token"}]}')
        self.keys.chmod(0o600)
        self.probe = self.root / 'probe.py'
        self.probe.write_text('import json, os; print(json.dumps({key: value for key, value in os.environ.items() if key.startswith("REMOTEVNC_")}))')
        self.env = {
            'PATH': os.environ['PATH'],
            'REMOTEVNC_PUBLIC_ORIGIN': 'https://example.test',
            'REMOTEVNC_GOOGLE_CLIENT_ID': 'fixture-id',
            'REMOTEVNC_GOOGLE_CLIENT_SECRET_FILE': str(self.google),
        }

    def run_entrypoint(self, values, success):
        result = subprocess.run(['/bin/sh', str(ENTRYPOINT), sys.executable, str(self.probe)],
                                env=self.env | values, capture_output=True, text=True, timeout=5)
        self.assertEqual(result.returncode == 0, success, result.stderr)
        self.assertNotIn('fixture-google-secret', result.stderr)
        self.assertNotIn('fixture-turn-token', result.stderr)
        self.assertNotIn('fixture-file-token', result.stderr)
        if success:
            return json.loads(result.stdout)
        self.assertEqual(result.stdout, '')
        return None

    def test_file_mode_passes_path_without_exporting_turn_credentials(self):
        env = self.run_entrypoint({'REMOTEVNC_TURN_KEYS_FILE': str(self.keys)}, True)
        self.assertEqual(env['REMOTEVNC_TURN_KEYS_FILE'], str(self.keys))
        self.assertEqual(env['REMOTEVNC_GOOGLE_CLIENT_SECRET'], 'fixture-google-secret')
        self.assertNotIn('REMOTEVNC_TURN_API_TOKEN', env)

    def test_legacy_file_and_environment_modes_still_work(self):
        for source in [{'REMOTEVNC_TURN_API_TOKEN_FILE': str(self.token)},
                       {'REMOTEVNC_TURN_API_TOKEN': 'fixture-turn-token'}]:
            with self.subTest(source=list(source)):
                env = self.run_entrypoint({'REMOTEVNC_TURN_KEY_ID': 'legacy-key'} | source, True)
                self.assertEqual(env['REMOTEVNC_TURN_API_TOKEN'], 'fixture-turn-token')

    def test_missing_or_ambiguous_configuration_fails_before_exec(self):
        file_mode = {'REMOTEVNC_TURN_KEYS_FILE': str(self.keys)}
        for values in [{}, {'REMOTEVNC_TURN_KEY_ID': 'legacy-key'},
                       {'REMOTEVNC_TURN_API_TOKEN_FILE': str(self.token)},
                       {'REMOTEVNC_TURN_KEYS_FILE': ''},
                       file_mode | {'REMOTEVNC_TURN_KEY_ID': ''},
                       file_mode | {'REMOTEVNC_TURN_KEY_ID': 'legacy-key'},
                       file_mode | {'REMOTEVNC_TURN_API_TOKEN_FILE': str(self.token)},
                       file_mode | {'REMOTEVNC_TURN_API_TOKEN': 'fixture-turn-token'},
                       {'REMOTEVNC_TURN_KEY_ID': 'legacy-key', 'REMOTEVNC_TURN_API_TOKEN_FILE': str(self.token), 'REMOTEVNC_TURN_API_TOKEN': 'fixture-turn-token'}]:
            with self.subTest(values=list(values)):
                self.run_entrypoint(values, False)


if __name__ == '__main__':
    unittest.main()
