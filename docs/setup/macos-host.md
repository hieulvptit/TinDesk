# macOS host (this Mac as a remote computer)

A Mac can be both the controller (`BinaChicken.app`) and a controlled host. The host reuses the
unmodified agent; `binachicken-mac-helper` replaces the Linux seat helper behind the same local IPC
(`apps/mac-helper`): AVFoundation screen capture, VideoToolbox (or x264) H.264, and Quartz
CGEvent input. It serves the **main display** of the logged-in GUI session.

```sh
source .deps/media/env.sh
cargo build --release -p binachicken-agent -p binachicken-mac-helper
packaging/macos/host.sh install [--backend URL] [--name "My Mac"]
packaging/macos/host.sh enroll     # open the printed URL, sign in with the same Google account
packaging/macos/host.sh doctor     # shows missing permissions
packaging/macos/host.sh start      # user LaunchAgents: helper + agent, restarted on failure
```

The helper asks for **Screen Recording** and **Accessibility** itself (at startup and in `doctor`):
click Allow / Open System Settings in the system prompts for
`~/Library/Application Support/BinaChicken/bin/binachicken-mac-helper`, then restart with
`host.sh stop && host.sh start`. Other commands: `status`, `stop`, `uninstall`.
Logs: `~/Library/Logs/BinaChicken/`.

Limits:
- Runs as a user LaunchAgent, so the user must be logged in. A sleeping display is woken when a
  session starts and kept awake until it ends, and the lock screen is captured and typed into
  through the same path (unverified: test with Ctrl+Cmd+Q, then connect). The login window before
  any login and FileVault pre-boot need a root LaunchDaemon and are not supported.
- Only the main display is shared. Retina input is mapped from capture pixels to points.
- Keys use the US layout; non-ASCII text is injected as Unicode. Remote Ctrl/Alt/Super map to
  Control/Option/Command, so use the Super key for Mac shortcuts such as copy.
- Privacy grants follow the code identity. Rebuilds are ad-hoc signed unless `SIGN_IDENTITY` is set,
  so grants must be repeated after each reinstall without it.
- Clipboard/file sync and SSH tunnel are not configured for the macOS host.
- Not yet verified end to end against a client: capture/input need the permissions above and an
  awake display.
