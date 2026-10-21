# RemoteVNC

Rust remote control of an enrolled Ubuntu **physical X11** desktop from a native
macOS client. A single HTTPS backend brokers Google-owned device enrollment,
short-lived authorization, signaling and Cloudflare TURN credentials. Video and
input use authenticated WebRTC; the privileged helper has no network access.

- [Ubuntu installation, local enrollment and rollback](docs/setup/ubuntu.md)
- [macOS development app and signing limits](docs/setup/macos.md)
- [Private backend deployment, ingress and SQLite operations](docs/setup/backend.md)
- [Cloudflare TURN tokens and multiple backend keys](docs/setup/cloudflare-turn.md)
- [Native dependency setup](docs/setup/native-dependencies.md)

Use Rust **1.95.0**, the committed `Cargo.lock`, and `--locked`. After native
media setup, `scripts/check.sh` checks formatting, clippy and workspace tests on
the current OS. CI runs Linux and macOS separately; explicit Xvfb tests are
Linux fixtures. `scripts/build-linux.sh` and `packaging/macos/build-app.sh` build
normal release executables with no test screenshot/fixture feature.

Ubuntu deployment targets 26.04 with LightDM/Xfce/X11 and light-locker. Prepare
and build never switch/restart the active display manager. `--apply` selects
LightDM for the **next boot**; reboot is a separate local administrator action.
GDM/Wayland capture is unsupported. The existing Ubuntu 25.10 host must not be
upgraded or switched as a side effect of builds/tests.

Local tests and unsigned macOS bundles do not establish physical greeter/locker,
interactive Google/Keychain/IME, LAN permission or real Cloudflare relay acceptance.
The owner confirmed `amio.vnoffice.io.vn` is dedicated to this backend; deployment
examples add only its routing and preserve other applications.
No secrets, databases, recordings or generated app bundles belong in git.
