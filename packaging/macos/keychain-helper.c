// Stable Keychain process identity for local, self-signed development builds.
// Only a direct parent signed as this client by our pinned certificate may use it.
#include <Security/Security.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#ifndef BINACHICKEN_CERT_SHA1
#error A pinned signing certificate is required
#endif

static int authorized_parent(void) {
    pid_t parent = getppid();
    if (parent <= 1) return 0;
    CFNumberRef pid = CFNumberCreate(NULL, kCFNumberIntType, &parent);
    const void *keys[] = { kSecGuestAttributePid };
    const void *values[] = { pid };
    CFDictionaryRef attributes = CFDictionaryCreate(NULL, keys, values, 1,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    SecCodeRef code = NULL;
    SecRequirementRef requirement = NULL;
    OSStatus status = SecCodeCopyGuestWithAttributes(NULL, attributes, kSecCSDefaultFlags, &code);
    if (!status) status = SecRequirementCreateWithString(
        CFSTR("identifier \"io.binachicken.client\" and certificate leaf = H\"" BINACHICKEN_CERT_SHA1 "\""),
        kSecCSDefaultFlags, &requirement);
    if (!status) status = SecCodeCheckValidity(code, kSecCSStrictValidate, requirement);
    if (requirement) CFRelease(requirement);
    if (code) CFRelease(code);
    CFRelease(attributes);
    CFRelease(pid);
    return !status && parent == getppid();
}

int main(int argc, char **argv) {
    // Reject terminals, other apps and ad-hoc replacements before querying Keychain.
    if (!authorized_parent()) return 65;
    if (argc != 3 || strlen(argv[2]) > 1024 || !strlen(argv[2])) return 64;
    int reading = !strcmp(argv[1], "read");
    int writing = !strcmp(argv[1], "write");
    int deleting = !strcmp(argv[1], "delete");
    if (!reading && !writing && !deleting) return 64;
    // Fixtures can prove prompt-free access using the same SecItem APIs as production.
    if (getenv("BINACHICKEN_KEYCHAIN_NO_UI")) SecKeychainSetUserInteractionAllowed(false);
    CFStringRef account = CFStringCreateWithCString(NULL, argv[2], kCFStringEncodingUTF8);
    if (!account) return 64;
    const void *keys[] = { kSecClass, kSecAttrService, kSecAttrAccount };
    const void *values[] = { kSecClassGenericPassword, CFSTR("io.binachicken.client"), account };
    CFMutableDictionaryRef query = CFDictionaryCreateMutable(NULL, 0,
        &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    for (int i = 0; i < 3; i++) CFDictionarySetValue(query, keys[i], values[i]);
    OSStatus status;
    CFTypeRef result = NULL;
    unsigned char buffer[4097] = {0};
    if (reading) {
        CFDictionarySetValue(query, kSecReturnData, kCFBooleanTrue);
        status = SecItemCopyMatching(query, &result);
        if (!status) {
            if (!result || CFGetTypeID(result) != CFDataGetTypeID() || CFDataGetLength(result) > 4096) {
                status = errSecParam;
            } else {
                CFIndex length = CFDataGetLength(result);
                if (fwrite(CFDataGetBytePtr(result), 1, length, stdout) != (size_t)length) status = errSecIO;
            }
        }
    } else if (writing) {
        size_t length = fread(buffer, 1, sizeof(buffer), stdin);
        if (!length || length > 4096 || ferror(stdin)) {
            status = errSecParam;
        } else {
            CFDataRef data = CFDataCreate(NULL, buffer, length);
            const void *update_keys[] = { kSecValueData };
            const void *update_values[] = { data };
            CFDictionaryRef update = CFDictionaryCreate(NULL, update_keys, update_values, 1,
                &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
            status = SecItemUpdate(query, update);
            if (status == errSecItemNotFound) {
                CFDictionarySetValue(query, kSecValueData, data);
                status = SecItemAdd(query, NULL);
            }
            CFRelease(update);
            CFRelease(data);
        }
    } else {
        status = SecItemDelete(query);
        if (status == errSecItemNotFound) status = errSecSuccess;
    }
    volatile unsigned char *erase = buffer;
    for (size_t i = 0; i < sizeof(buffer); i++) erase[i] = 0;
    if (result) CFRelease(result);
    CFRelease(query);
    CFRelease(account);
    if (reading && status == errSecItemNotFound) return 3;
    if (status) fprintf(stderr, "binachicken-keychain: OSStatus %d\n", (int)status);
    return status ? 66 : 0;
}
