#!/usr/bin/env python3
"""Real, isolated Keychain ACL proof across two differently built signed apps."""
from pathlib import Path
import plistlib
import secrets
import shutil
import subprocess
import sys
import tempfile

if sys.platform != 'darwin':
    raise SystemExit('This check requires macOS and the configured local signing identity.')

root = Path(__file__).resolve().parents[1]
source = r'''
#include <Security/Security.h>
#include <stdio.h>
#include <string.h>
int main(int argc, char **argv) {
    if (argc != 4) return 2;
    SecKeychainSetUserInteractionAllowed(false);
    SecKeychainRef kc = NULL;
    OSStatus status;
    const char *service = "io.binachicken.signing-fixture";
    const char *account = "isolated-test";
    const char *value = "fixture-only-not-a-credential";
    if (!strcmp(argv[1], "create")) {
        status = SecKeychainCreate(argv[2], (UInt32)strlen(argv[3]), argv[3], false, NULL, &kc);
        if (!status) status = SecKeychainAddGenericPassword(kc,
            (UInt32)strlen(service), service, (UInt32)strlen(account), account,
            (UInt32)strlen(value), value, NULL);
    } else {
        status = SecKeychainOpen(argv[2], &kc);
        if (!status) status = SecKeychainUnlock(kc, (UInt32)strlen(argv[3]), argv[3], true);
        UInt32 length = 0; void *data = NULL;
        if (!status) status = SecKeychainFindGenericPassword(kc,
            (UInt32)strlen(service), service, (UInt32)strlen(account), account,
            &length, &data, NULL);
        if (!status && (length != strlen(value) || memcmp(data, value, length))) status = -1;
        if (data) SecKeychainItemFreeContent(NULL, data);
    }
    if (kc) CFRelease(kc);
    printf("build %d: status %d\n", VERSION, (int)status);
    return status ? 1 : 0;
}
'''


def run(args, **kwargs):
    result = subprocess.run(args, capture_output=True, **kwargs)
    if result.returncode:
        # Fixture keychain passwords must never be included in exception arguments.
        raise SystemExit(f'Fixture command {Path(args[0]).name} failed: '
                         + result.stdout.decode() + result.stderr.decode())
    return result


with tempfile.TemporaryDirectory(prefix='binachicken-signing-test-') as directory:
    directory = Path(directory)
    keychain = directory / 'fixture.keychain-db'
    password = secrets.token_urlsafe(32)
    executables = []
    requirements = []
    hashes = []
    try:
        for version in (1, 2):
            app = directory / str(version) / 'BinaChicken.app'
            contents = app / 'Contents'
            (contents / 'MacOS').mkdir(parents=True)
            (contents / 'Resources/gst-plugins').mkdir(parents=True)
            (contents / 'Info.plist').write_bytes(plistlib.dumps({
                'CFBundleIdentifier': 'io.binachicken.client',
                'CFBundleExecutable': 'binachicken-client',
                'CFBundlePackageType': 'APPL',
                'CFBundleVersion': str(version),
            }))
            executable = contents / 'MacOS/binachicken-client'
            run(['cc', '-x', 'c', '-', '-o', str(executable), f'-DVERSION={version}',
                 '-framework', 'Security', '-framework', 'CoreFoundation',
                 '-Wno-deprecated-declarations'], input=source.encode())
            shutil.copy2(root / '.deps/media/gst-plugins/libgstnice.dylib',
                         contents / 'Resources/gst-plugins/libgstnice.dylib')
            run([sys.executable, str(root / 'packaging/macos/sign-app.py'), str(app)])
            inspected = run(['codesign', '-d', '-r-', '--verbose=4', str(app)])
            details = (inspected.stdout + inspected.stderr).decode()
            requirements.append(next(line for line in details.splitlines() if line.startswith('designated =>')))
            hashes.append(next(line for line in details.splitlines() if line.startswith('CDHash=')))
            executables.append(executable)
        assert hashes[0] != hashes[1], 'The two builds must have different executable signatures'
        assert requirements[0] == requirements[1], 'Designated identity must survive a rebuild'
        run([str(executables[0]), 'create', str(keychain), password])
        run([str(executables[1]), 'read', str(keychain), password])
        print('Two distinct signed builds share the same Keychain ACL without UI: PASS')
        run(['codesign', '--force', '--sign', '-', str(executables[1])])
        denied = subprocess.run([str(executables[1]), 'read', str(keychain), password], capture_output=True)
        assert denied.returncode != 0, 'An unrelated ad-hoc signer must not inherit access'
        print('Ad-hoc replacement cannot read the same fixture item without approval: PASS')
    finally:
        if keychain.exists():
            run(['security', 'delete-keychain', str(keychain)])
