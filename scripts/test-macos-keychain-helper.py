#!/usr/bin/env python3
"""Prove real login-Keychain partition behavior without reading any existing item."""
from pathlib import Path
import json
import os
import plistlib
import shutil
import subprocess
import sys
import tempfile
import uuid

ROOT = Path(__file__).resolve().parents[1]
SOURCE = r'''
#include <Security/Security.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>
int main(int argc, char **argv) {
    if (argc != 4) return 2;
    const char *fixture = "fixture-only-not-a-credential";
    if (!strncmp(argv[1], "direct-", 7)) {
        SecKeychainSetUserInteractionAllowed(false);
        CFStringRef account = CFStringCreateWithCString(NULL,argv[2],kCFStringEncodingUTF8);
        const void *keys[] = {kSecClass,kSecAttrService,kSecAttrAccount};
        const void *values[] = {kSecClassGenericPassword,CFSTR("io.binachicken.client"),account};
        CFMutableDictionaryRef q = CFDictionaryCreateMutable(NULL,0,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
        for(int i=0;i<3;i++) CFDictionarySetValue(q,keys[i],values[i]);
        OSStatus s;
        if(!strcmp(argv[1],"direct-write")) {
            CFDataRef data=CFDataCreate(NULL,(const UInt8*)fixture,strlen(fixture));
            CFDictionarySetValue(q,kSecValueData,data);s=SecItemAdd(q,NULL);CFRelease(data);
        } else if(!strcmp(argv[1],"direct-read")) {
            CFTypeRef data=NULL;CFDictionarySetValue(q,kSecReturnData,kCFBooleanTrue);
            s=SecItemCopyMatching(q,&data);if(data)CFRelease(data);
        } else { s=SecItemDelete(q); }
        CFRelease(q);CFRelease(account);
        printf("direct build %d: OSStatus %d\n",VERSION,(int)s);return s?1:0;
    }
    int input[2],output[2];if(pipe(input)||pipe(output))return 2;
    pid_t child=fork();
    if(child<0)return 2;
    if(!child){dup2(input[0],0);dup2(output[1],1);close(input[0]);close(input[1]);close(output[0]);close(output[1]);execl(argv[3],argv[3],argv[1],argv[2],NULL);_exit(127);}
    close(input[0]);close(output[1]);
    if(!strcmp(argv[1],"write"))write(input[1],fixture,strlen(fixture));close(input[1]);
    char buffer[4097]={0};size_t total=0;ssize_t n;
    while(total<sizeof(buffer)&&(n=read(output[0],buffer+total,sizeof(buffer)-total))>0)total+=n;
    close(output[0]);int status;if(waitpid(child,&status,0)<0||!WIFEXITED(status))return 2;
    if(WEXITSTATUS(status))return WEXITSTATUS(status);
    if(!strcmp(argv[1],"read")&&(total!=strlen(fixture)||memcmp(buffer,fixture,total)))return 2;
    printf("helper build %d: PASS\n",VERSION);return 0;
}
'''


def run(args, *, check=True):
    result = subprocess.run(args, capture_output=True, env={**os.environ, 'BINACHICKEN_KEYCHAIN_NO_UI': '1'})
    if check and result.returncode:
        raise SystemExit(f'{Path(args[0]).name} failed ({result.returncode}): '
                         + result.stdout.decode() + result.stderr.decode())
    return result


if sys.platform != 'darwin':
    raise SystemExit('Requires macOS and an unlocked login Keychain.')
account = 'https://binachicken-fixture-' + uuid.uuid4().hex + '.invalid'
with tempfile.TemporaryDirectory(prefix='binachicken-keychain-partition-') as directory:
    directory = Path(directory)
    executables, helpers = [], []
    for version in (1, 2):
        app = directory / str(version) / 'BinaChicken.app'
        contents = app / 'Contents'
        (contents / 'MacOS').mkdir(parents=True)
        (contents / 'Resources/gst-plugins').mkdir(parents=True)
        (contents / 'Info.plist').write_bytes(plistlib.dumps({
            'CFBundleIdentifier': 'io.binachicken.client', 'CFBundleExecutable': 'binachicken-client',
            'CFBundlePackageType': 'APPL', 'CFBundleVersion': str(version)}))
        executable = contents / 'MacOS/binachicken-client'
        build = subprocess.run(['cc', '-x', 'c', '-', '-o', str(executable), f'-DVERSION={version}',
             '-framework', 'Security', '-framework', 'CoreFoundation', '-Wno-deprecated-declarations'],
             input=SOURCE.encode(), capture_output=True)
        if build.returncode:
            raise SystemExit(build.stderr.decode())
        shutil.copy2(ROOT / '.deps/media/gst-plugins/libgstnice.dylib', contents / 'Resources/gst-plugins/')
        run([sys.executable, str(ROOT / 'packaging/macos/sign-app.py'), str(app)])
        executables.append(executable)
        helpers.append(contents / 'Helpers/binachicken-keychain')
    assert executables[0].read_bytes() != executables[1].read_bytes()
    assert helpers[0].read_bytes() == helpers[1].read_bytes(), 'Helper signatures must remain identical'
    direct_created = helper_created = False
    try:
        run([str(executables[0]), 'direct-write', account + '-direct', str(helpers[0])])
        direct_created = True
        denied = run([str(executables[1]), 'direct-read', account + '-direct', str(helpers[1])], check=False)
        assert denied.returncode and any(code in denied.stdout for code in
            (b'OSStatus -25308', b'OSStatus -25293')), denied.stdout.decode()
        print('Real login Keychain reproduces per-build cdhash denial despite stable certificate: PASS')
        run([str(executables[0]), 'write', account, str(helpers[0])])
        helper_created = True
        run([str(executables[1]), 'read', account, str(helpers[1])])
        print('Different client builds read the same fixture through the stable helper with UI disabled: PASS')
        denied = run([str(helpers[1]), 'read', account], check=False)
        assert denied.returncode == 65, 'Terminal/untrusted parent must be rejected'
        identity = json.loads((Path.home() / '.local/share/binachicken/signing/identity.json').read_text())
        run(['codesign', '--force', '--sign', identity['identity'], '--keychain', identity['keychain'],
             '--identifier', 'io.binachicken.other', str(executables[1])])
        denied = run([str(executables[1]), 'read', account, str(helpers[1])], check=False)
        assert denied.returncode == 65, 'Another app with the same certificate must be rejected'
        run(['codesign', '--force', '--sign', '-', str(executables[1])])
        denied = run([str(executables[1]), 'read', account, str(helpers[1])], check=False)
        assert denied.returncode == 65, 'Ad-hoc client must be rejected'
        print('Untrusted parent, different app with same certificate, and ad-hoc replacement are rejected: PASS')
    finally:
        if direct_created:
            run([str(executables[0]), 'direct-delete', account + '-direct', str(helpers[0])])
        if helper_created:
            run([str(executables[0]), 'delete', account, str(helpers[0])])
