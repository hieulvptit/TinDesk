use binachicken_device::DeviceApi;
use ed25519_dalek::SigningKey;

#[test]
fn constructor_accepts_https_and_only_loopback_http_including_ipv6() {
    for base in [
        "https://backend.example",
        "http://localhost:8080",
        "http://127.0.0.1:8080",
        "http://[::1]:8080",
    ] {
        assert!(
            DeviceApi::new(base, SigningKey::from_bytes(&[1; 32])).is_ok(),
            "expected local or HTTPS origin to be accepted: {base}"
        );
    }
    for base in [
        "http://backend.example",
        "http://192.0.2.1:8080",
        "http://[2001:db8::1]:8080",
        "http://localhost.example:8080",
        "http://user@localhost:8080",
        "http://[::1]:8080/path",
        "http://[::1]:8080?query=value",
        "http://[::1]:8080#fragment",
    ] {
        assert!(
            DeviceApi::new(base, SigningKey::from_bytes(&[1; 32])).is_err(),
            "expected invalid or nonlocal HTTP origin to be rejected: {base}"
        );
    }
}
