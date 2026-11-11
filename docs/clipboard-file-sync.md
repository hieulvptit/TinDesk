# Clipboard and file sync

Supersedes the "no clipboard sync" non-goal in the original design. Text and files are
shared in both directions between the macOS client and the Ubuntu desktop.

## Behaviour
- **Text**: copy on one side, paste on the other. Cmd on macOS acts as Control on Ubuntu,
  so Cmd+C / Cmd+V map to Ctrl+C / Ctrl+V there.
- **Files**: copy files in Finder (Cmd+C), paste in the Ubuntu file manager (Ctrl+V), and the
  reverse. Folders and files over 512 MiB are skipped (the UI says so). At most 64 files per copy.
- **Switch**: toolbar clipboard button → "Share clipboard and files". On by default; turning it
  off persists (`~/Library/Application Support/BinaChicken/clipboard-sync-off`) and also makes
  the Mac refuse incoming transfers.
- Content already on a clipboard when a session starts is not sent.
- Items marked secret by password managers (`org.nspasteboard.ConcealedType` and friends) are
  never read.

## Architecture
```
Mac client ──WebRTC "sync" channel──▶ agent (sandboxed) ──unix socket──▶ sync-helper (as the user)
 NSPasteboard                          relays opaque frames                 X11 CLIPBOARD owner/watcher
```
- `crates/protocol/src/sync.rs`: wire format, limits, `SyncReceiver`/`SyncSender` state machines
  (chunked, sha256-verified, windowed flow control: 1 MiB in flight).
- `crates/sync`: `Endpoint` shared by both ends (staging, queueing, backpressure, notices).
- `crates/transport`: third data channel `sync` (reliable, ordered); its receive queue is
  separate from input so a transfer cannot starve or fail input handling.
- `apps/agent/src/sync_bridge.rs`: the agent never interprets content. It admits only the
  **active seat0 user** (SO_PEERCRED + `loginctl`), only while the session is **not locked**,
  rechecks every 5 s, and answers Offers with a refusal when nobody can receive.
- `apps/sync-helper`: runs inside the user's graphical session (XDG autostart), so staged files and
  clipboard access belong to that user, never root. Received files are staged under
  `~/.cache/binachicken/clipboard-files/<random>/<name>` (purged after 24 h) and offered to the
  file manager as `text/uri-list` + `x-special/gnome-copied-files`.

Nothing is applied while the greeter or lock screen is showing: there is no user session at the
greeter, and the agent drops the helper when the session reports `LockedHint=yes`.

## Install on Ubuntu
`scripts/build-linux.sh` now also builds `binachicken-sync-helper`; then
`sudo packaging/ubuntu/install-sync.sh`. It adds the binary, one autostart entry, `sync_socket` in
`agent.json` and the agent's `RuntimeDirectory`, then restarts the agent. Client and agent must be
updated together (the peer now opens three data channels).

## Known limits
- X11 INCR transfers (selections larger than the server's property limit, e.g. very large text
  from some apps) are ignored rather than guessed at.
- Folders are not supported.
- Verified: protocol/state-machine tests, real WebRTC loopback (5 MiB both ways), real X11
  (Xvfb + xclip, text and files both ways) and the real macOS pasteboard. Not yet exercised: a full
  Mac ↔ Ubuntu session over the network, and the installer on a real machine.

## SSH tunnel

Lets the Mac reach the remote computer's `sshd` through the already-authorized session, without
exposing SSH on the network. Frames travel on the same `sync` channel (tag 2), so no extra data
channel is needed and channel compatibility with a sync-capable agent is unchanged.

- **Off by default, twice.** The agent offers it only if `ssh_port` is set in `agent.json`
  (`sudo install-sync.sh --enable-ssh [PORT]`); the Mac opens a port only while its switch is on
  (toolbar clipboard button → "Allow SSH to the remote user"). The switch never persists.
- **Only for the active seat user, only while unlocked.** The agent re-checks `loginctl` every 5 s
  and closes live tunnels when the screen locks or the user changes. The offered account name is
  validated to POSIX-portable characters because it is shown inside a command you copy.
- **The destination is fixed.** A tunnel `Open` carries only a connection id; the agent connects to
  its own `127.0.0.1:<ssh_port>`. A controller cannot steer it elsewhere.
- **Authentication stays with sshd** (key or password). The Mac listens on `127.0.0.1` only; any
  local process on the Mac can reach that port, exactly as with `ssh -L`.
- **Bounded.** 8 connections, 256 KiB credit window per connection and direction, 16 KiB frames.
  Half-close works, so `ssh host cmd < file` and `scp` behave normally.

The UI shows the ready command, e.g.
`ssh -p 2222 -o HostKeyAlias=binachicken-1a2b3c4d hieulv@127.0.0.1` (the alias keeps this
computer's host key apart from anything else that ever used the same local port).

Verified with a real `sshd` in an Ubuntu VM: remote command, 30 MiB upload over stdin, 30 MiB `scp`
download (hashes equal), four parallel sessions and a PTY session, all through the tunnel. Not yet
exercised: the full Mac ↔ Ubuntu path over the network with the new agent installed.
