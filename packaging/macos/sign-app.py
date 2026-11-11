#!/usr/bin/env python3
"""Keep one local signing identity across development rebuilds; never read app credentials."""
import json
import hashlib
import os
from pathlib import Path
import secrets
import shlex
import subprocess
import sys
import tempfile
import platform

DIRECTORY = Path.home() / '.local/share/binachicken/signing'
CONFIG = DIRECTORY / 'identity.json'


def keychain_helper(config, bundle):
    source = Path(__file__).with_name('keychain-helper.c')
    digest = hashlib.sha256(source.read_bytes() + config['identity'].encode()
                            + platform.machine().encode()).hexdigest()
    cache = DIRECTORY / 'helpers' / digest
    cache.mkdir(parents=True, exist_ok=True, mode=0o700)
    helper = cache / 'binachicken-keychain'
    requirement = ('=designated => identifier "io.binachicken.keychain" '
                   f'and certificate leaf = H"{config["identity"]}"')
    if not helper.exists():
        temporary = cache / 'building'
        run(['cc', '-O2', '-Wall', '-Wextra', '-Werror', '-Wno-deprecated-declarations',
             f'-DBINACHICKEN_CERT_SHA1="{config["identity"]}"', str(source),
             '-framework', 'Security', '-framework', 'CoreFoundation', '-o', str(temporary)])
        run(['codesign', '--force', '--sign', config['identity'], '--keychain', config['keychain'],
             '--timestamp=none', '--identifier', 'io.binachicken.keychain',
             '--requirements', requirement, str(temporary)])
        temporary.chmod(0o700)
        temporary.replace(helper)
    run(['codesign', '--verify', '--strict', '-R=' + requirement.removeprefix('=designated => '), str(helper)])
    destination = bundle / 'Contents/Helpers/binachicken-keychain'
    destination.parent.mkdir(exist_ok=True)
    # Reuse identical signed bytes across app rebuilds: local certificates lack
    # an Apple Team ID, so login Keychain partitions otherwise follow each cdhash.
    import shutil
    shutil.copy2(helper, destination)


def run(args):
    # Some security arguments contain a generated build-keychain password. Do
    # not print commands, their arguments, or exception representations.
    result = subprocess.run(args, capture_output=True, text=True)
    if result.returncode:
        if Path(args[0]).name == 'codesign':
            print(result.stderr.strip(), file=sys.stderr)
        raise SystemExit(f'{Path(args[0]).name} {args[1]} failed ({result.returncode}); signing stopped.')
    return result.stdout


def setup():
    if CONFIG.exists():
        trust_for_signing()
        print('Reusing the existing BinaChicken signing certificate.')
        return
    DIRECTORY.mkdir(parents=True, mode=0o700, exist_ok=True)
    DIRECTORY.chmod(0o700)
    keychain = DIRECTORY / 'signing.keychain-db'
    if keychain.exists():
        raise SystemExit('An unfinished signing keychain exists; inspect it before retrying setup.')
    password = secrets.token_urlsafe(32)
    password_file = DIRECTORY / 'keychain-password'
    password_file.write_text(password)
    password_file.chmod(0o600)
    with tempfile.TemporaryDirectory(prefix='setup-', dir=DIRECTORY) as temporary:
        temporary = Path(temporary)
        key = temporary / 'key.pem'
        cert = DIRECTORY / 'certificate.pem'
        run(['openssl', 'req', '-x509', '-newkey', 'rsa:3072', '-sha256',
             '-nodes', '-days', '3650', '-subj', '/CN=BinaChicken Local Development/',
             '-addext', 'basicConstraints=critical,CA:TRUE',
             '-addext', 'keyUsage=critical,digitalSignature,keyCertSign',
             '-addext', 'extendedKeyUsage=codeSigning',
             '-keyout', str(key), '-out', str(cert)])
        fingerprint = run(['openssl', 'x509', '-in', str(cert), '-noout',
                           '-fingerprint', '-sha1']).strip().split('=', 1)[1].replace(':', '')
        archive = temporary / 'identity.p12'
        # A password file avoids placing export passwords in OpenSSL arguments.
        run(['openssl', 'pkcs12', '-export', '-inkey', str(key), '-in', str(cert),
             '-name', 'BinaChicken Local Development', '-out', str(archive),
             '-keypbe', 'PBE-SHA1-3DES', '-certpbe', 'PBE-SHA1-3DES', '-macalg', 'sha1',
             '-passout', 'file:' + str(password_file)])
        run(['security', 'create-keychain', '-p', password, str(keychain)])
        run(['security', 'set-keychain-settings', '-l', str(keychain)])
        run(['security', 'unlock-keychain', '-p', password, str(keychain)])
        run(['security', 'import', str(archive), '-k', str(keychain), '-P', password,
             '-x', '-T', '/usr/bin/codesign'])
        run(['security', 'set-key-partition-list', '-S', 'apple-tool:,codesign:',
             '-s', '-k', password, str(keychain)])
    CONFIG.write_text(json.dumps({'identity': fingerprint, 'keychain': str(keychain)}, indent=2) + '\n')
    CONFIG.chmod(0o600)
    trust_for_signing()
    print('Created persistent local certificate:', fingerprint)


def trust_for_signing():
    config = json.loads(CONFIG.read_text())
    ensure_search_list(config['keychain'])
    run(['security', 'unlock-keychain', '-p', (DIRECTORY / 'keychain-password').read_text(),
         config['keychain']])
    valid = run(['security', 'find-identity', '-v', '-p', 'codesigning', config['keychain']])
    if config['identity'] not in valid:
        print('macOS may request approval to trust this local certificate for code signing.', flush=True)
        # User-domain code-signing trust only; no system or TLS trust changes.
        run(['security', 'add-trusted-cert', '-r', 'trustRoot', '-p', 'codeSign',
             '-k', config['keychain'], str(DIRECTORY / 'certificate.pem')])


def ensure_search_list(keychain):
    # codesign needs the keychain listed even with --keychain. Preserve all
    # existing entries and the default login keychain; append only this build key.
    search = shlex.split(run(['security', 'list-keychains', '-d', 'user']))
    if keychain not in search:
        run(['security', 'list-keychains', '-d', 'user', '-s', *search, keychain])


def sign(bundle):
    if not CONFIG.is_file():
        raise SystemExit('Run python3 packaging/macos/sign-app.py --setup once before building.')
    config = json.loads(CONFIG.read_text())
    ensure_search_list(config['keychain'])
    password = (DIRECTORY / 'keychain-password').read_text()
    run(['security', 'unlock-keychain', '-p', password, config['keychain']])
    bundle = Path(bundle).resolve()
    keychain_helper(config, bundle)
    plugin = bundle / 'Contents/Resources/gst-plugins/libgstnice.dylib'
    if not plugin.is_file() or not (bundle / 'Contents/Info.plist').is_file():
        raise SystemExit('Expected a complete BinaChicken.app bundle.')
    requirement = ('=designated => identifier "io.binachicken.client" '
                   f'and certificate leaf = H"{config["identity"]}"')
    run(['codesign', '--force', '--sign', config['identity'], '--keychain', config['keychain'],
         '--timestamp=none', '--identifier', 'io.binachicken.client.gstnice', str(plugin)])
    run(['codesign', '--force', '--sign', config['identity'], '--keychain', config['keychain'],
         '--timestamp=none', '--identifier', 'io.binachicken.client',
         '--requirements', requirement, str(bundle)])
    run(['codesign', '--verify', '--deep', '--strict', str(bundle)])
    print('Signed with persistent local identity:', config['identity'])


if __name__ == '__main__':
    if sys.platform != 'darwin':
        raise SystemExit('macOS is required.')
    os.umask(0o077)
    if sys.argv[1:] == ['--setup']:
        setup()
    elif len(sys.argv) == 2 and sys.argv[1].endswith('.app'):
        sign(sys.argv[1])
    else:
        raise SystemExit('Usage: sign-app.py --setup | /path/to/BinaChicken.app')
