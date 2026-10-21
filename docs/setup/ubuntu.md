# Ubuntu 26.04 installation

Use a local administrator session with a recovery path. The supported desktop is
LightDM + lightdm-gtk-greeter + Xfce + light-locker on physical seat0/X11. The
installer rejects other Ubuntu releases and never upgrades the OS. GDM/Wayland
remains unsupported. Neither preparation nor tests restart a display manager.

Install Rust 1.95.0 separately (leave an existing default compiler unchanged),
then install build prerequisites:

```sh
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libssl-dev python3 \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \
  libgstreamer-plugins-bad1.0-dev libx11-dev libxtst-dev libxi-dev \
  libxrandr-dev libxfixes-dev libxkbcommon-dev libwayland-dev \
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-plugins-ugly gstreamer1.0-libav gstreamer1.0-nice
export RUSTUP_TOOLCHAIN=1.95.0
scripts/build-linux.sh
packaging/ubuntu/install.sh --dry-run
sudo packaging/ubuntu/install.sh --prepare --bin-dir target/release
```

`--prepare` blocks package service starts with a temporary `policy-rc.d` returning
101. It restores the exact prior policy, display-manager config/selection, and
Debconf shared/default-x-display-manager value/seen flag on
success, apt failure or ordinary signal interruption. A power failure/SIGKILL
cannot run a shell trap: use the printed private backup to recover the policy
before another apt operation. It installs runtime dependencies, the two binaries
and all three reviewed systemd units, creates the system account and state, and
reloads unit definitions. It does not enable/start/restart services. Review apt's
package changes; dependencies may be installed but are not automatically removed
by rollback. If the Debconf question did not exist before package installation,
the new package schema may remain, but its choice is left empty/unseen until
explicit apply. Build/check scripts do not run the installer.

The software x264 encoder and decoder/plugins are required. The helper probes
usable encoders and falls back to x264 when hardware initialization fails. Its
reviewed `PrivateDevices=yes` service configuration intentionally supports the
software path; installing a hardware plugin alone does not grant DRM access.

The packaged light-locker XDG autostart entry starts it in Xfce. After the explicit
reboot/login, verify `pgrep -x light-locker` and test `light-locker-command --lock`
from the local desktop. Review Xfce Session and Startup for a user override or a
competing saved-session locker; do not run two lockers. Existing per-user desktop
preferences are not overwritten. See [Xfce autostart documentation](https://docs.xfce.org/xfce/xfce4-session/preferences).
The physical lock/unlock check is required before relying on remote control.

Private state is `/var/lib/remotevnc-agent` (0700, actual remotevnc-agent UID).
`/etc/remotevnc/agent.json` is root-owned 0640 with the agent group and fixes
`identity_path`, `helper_socket=/run/remotevnc/helper.sock`, seat0 and HTTPS origin.
Do not run enrollment/doctor/revoke as root: state ownership is checked, and root
is not an identity-ownership bypass. Stop the agent before credential operations:

```sh
sudo systemctl stop remotevnc-agent.service
sudo -u remotevnc-agent /usr/bin/remotevnc-agent enroll \
  --config /etc/remotevnc/agent.json --name Ubuntu
```

Complete Google consent and the explicit displayed device-code confirmation
locally **before reboot**. Secrets stay in identity.json 0600; do not print/copy it.
Only successful explicit enrollment clears a local revoke marker. `--apply`
requires an unrevoked, private enrolled identity; actual backend authorization
is still checked by the agent and backend on every session lifecycle.

```sh
sudo packaging/ubuntu/install.sh --apply
sudo packaging/ubuntu/install.sh --doctor
```

Apply prints a fresh rollback directory before writes. It backs up prior
LightDM config, display-manager selection/alias and unit enablement; selects
LightDM/Xfce for **next boot** and starts the independent input guardian, then
helper, then outbound agent. It never restarts the display manager or reboots.
An existing Wayland session can report unsupported until the administrator
explicitly reboots from a local session. Keep the printed rollback command:

```sh
sudo packaging/ubuntu/rollback.sh /var/lib/remotevnc-installer/backup-XXXXXXXX
```

Replace the directory with the exact printed value. Rollback stops the agent
and helper before requesting guardian stop, restores the previous selection,
config, Debconf selection/seen flag and enablement, and does not restart the
current desktop. It retains
installed packages, binaries and enrollment state. It does not restart previous
RemoteVNC sessions. Guardian stop may wait indefinitely for physical overlap to
clear; release the local keys/buttons and keep the X server responsive. Never
replace this with a forced timeout or kill both recovery owners while X remains
alive. Review selection, then reboot explicitly when safe.

`/run/remotevnc` is a dedicated root-owned 0711 directory; helper.sock is0600
owned by the resolved agent UID. All ancestors must be traversable by that UID.
The installer refuses a stale/untraversable existing directory instead of
chmodding arbitrary paths; inspect and remove the stale dedicated directory
explicitly when services are stopped. Guardian has a different root-only 0700
runtime directory and independent cgroup. The helper uses KillMode=control-group
so capture children die on helper restart. Kernel >=6.5 and usable SO_PEERPIDFD,
pidfd_send_signal and signal permission are mandatory; no numeric-PID fallback.

For disposable Linux checks only:

```sh
sudo apt-get install -y xvfb xauth x11-utils python3-gi gir1.2-gtk-3.0
sudo python3 scripts/test-ubuntu-installer.py
scripts/check.sh
scripts/test-x11.sh
```

Installer tests use private mount namespaces/chroots and actual account/file
ownership. Package operations and live service start/stop/reload are controlled
stand-ins; enable/disable/is-enabled delegate to real offline `systemctl --root`
inside the disposable chroot. No live system manager is contacted. These checks
do not prove physical LightDM/locker acceptance. The existing reviewed units
separately passed actual systemd/sandbox/cgroup tests. Native tests never use a
production virtual-display bypass. Physical greeter/login/lock/logout, encoder
load, CPU/bandwidth and local/remote ownership races still require the final acceptance matrix. X11
query then restoration/release has concurrent physical-change windows; there is
no atomic/race-free or hard real-time guarantee. A responding X server is
required; simultaneous destruction of helper and guardian is unsupported.

The release build defaults to one compiler job and refuses less than 2GiB of
free scratch space in its artifact filesystem (environment capacity guard, not
a runtime requirement; a fresh cache may need more).

The native build manifest records compiler, Cargo.lock checksum and installed
GStreamer package versions; artifact hashes accompany the binaries. Native apt
repositories and host libraries are not byte-reproducible without a separately
frozen OS image/package mirror. Use the pinned backend image for isolated backend
builds; keep build logs/version manifests for native artifact provenance.
