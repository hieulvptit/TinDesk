# Agent deployment — clawmachine

Installed 2026-10-21 on `clawmachine@172.22.8.59`, hostname
`clawmachine-HP-ProBook-455-G6`: Ubuntu 25.10, x86_64, kernel 6.17.0-41-generic,
systemd 257.9. Backend: `https://amio.vnoffice.io.vn`.

Ubuntu 25.10 uses the explicit `--allow-ubuntu-2510` installer option. The primary
target remains Ubuntu 26.04. No OS upgrade was performed.

## Installed release

- Installer/units revision: `499f2f42f02431760979f980e73791d16206c70c`.
- Agent source: `b41ba7d03e64fb87612ab645c9d2257b3a5f3341`, SHA256
  `776ee37899ee82d2c1b1c92c1f87391cb0284d58e210c18f7427b96d6fa72f7e`.
- Helper/guardian source: `1bc63df14813ab1067870f3f0d005dc35ec853aa`, SHA256
  `839c6e74ce8b792fb22d08ad0b4ffede34c09ce2ffb156b889da8b0ecab2c524`.

Previously verified native binaries were reused after comparing production
source/dependency inputs and hashes. The agent differs only in a `cfg(test)`
native test file; helper production inputs match exactly. No binary was rebuilt
or restamped. Binaries are installed at `/usr/bin/remotevnc-agent` and
`/usr/bin/remotevnc-seat-helper`.

The reviewed kit is `/home/clawmachine/RemoteVNC-deploy/499f2f4`. A root-owned copy
is at `/opt/remotevnc/releases/499f2f42f02431760979f980e73791d16206c70c`.
Local evidence is under
`/Users/hieulv/Projects/RemoteVNC-builds/ubuntu-deploy-499f2f4`.
No OAuth client secret or long-term TURN key was copied to this host.

## Verification

- Ten installer tests passed in disposable container/chroots before deployment.
- User ran the prepared sudo command in their own terminal and completed Google
  device confirmation. The backend contains the registered device and issued a
  live access token for it; no identity, password or token values were printed.
- All nine required GStreamer plugins are present. A real software H.264
  encode/parse/decode smoke pipeline passed on the host.
- Installed binary hashes match the reviewed artifacts; all shared libraries resolve.
- `remotevnc-agent`, `remotevnc-seat-helper` and `remotevnc-input-guardian` are
  enabled and active/running, with zero restarts at verification.
- Agent runs as `remotevnc-agent`; helper and guardian run as root under their
  reviewed systemd policies. Private state is owner0700; helper socket is
  agent-owned0600 under root0711; guardian directory is root0700.

At initial installation the GDM/Wayland session was preserved. LightDM/Xfce/X11 was selected for
the **next boot**. Save work and reboot manually when ready; the current Wayland
desktop cannot be controlled by this X11 implementation. No automatic reboot
or display-manager restart occurred.

Physical greeter/login/lock/unlock and a real remote session, including selected
Cloudflare relay candidate, remain to be accepted after reboot. Active services
and issued credentials alone do not prove desktop control or TURN media.

For operations and rollback, follow [the Ubuntu setup guide](ubuntu.md). Keep the
exact private backup path printed in the installation terminal. To inspect status:

```sh
systemctl status remotevnc-agent remotevnc-seat-helper remotevnc-input-guardian
```

## Follow-up on 2026-10-21

The user subsequently rebooted into LightDM/X11. The physical `seat0` greeter
was detected at 1920x1080 with XTest/XKB/XI2 ready. The user applied the Xorg
executable-lookup correction (`CAP_SYS_PTRACE`), backed up under
`/var/lib/remotevnc-installer/xorg-probe-backup-BiuJGOf3`, and the agent ICE
interface-discovery correction (`AF_NETLINK`), backed up under
`/var/lib/remotevnc-installer/ice-network-backup-F2mLTBmd`. Live systemd inspection
confirmed AF_NETLINK was loaded, the agent was active and its restart count was
zero. Original-policy denied probes are intentional failure reproductions.

A new agent with live FPS/stream-size requests, 30 FPS default, timestamp-clock
mapping and sanitized P2P/TURN diagnostics was built in the existing Ubuntu
25.10/Rust 1.95 container and staged at
`/home/clawmachine/RemoteVNC-deploy/video-settings-v1`.
Agent SHA256: `2698725fb062b92c968338c0e392a367996322215ce02072898cf16d97d3a9de`.
Checksums, command entry and native library resolution passed on the target.
The user applied this binary update at 16:24 local time. Live verification
confirmed the installed hash above, active state and zero automatic restarts.
The old
helper supports the existing UpdateProfile IPC and does not need replacement.
Current artifacts and immutable source manifest/archive are under
`target/video-controls-build`; the Mac development app is under
`target/release/video-settings`, with the active workspace app also copied to
`target/release/RemoteVNC.app`. Real physical-session performance and route
must be checked after installation; fixtures do not prove those results.
