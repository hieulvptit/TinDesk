# Additional Ubuntu 25.10 compatibility — 2026-10-21

The requested additional compatibility run **passed** in an ordinary disposable
Ubuntu 25.10 container on the supplied machine's kernel. The primary product and
installer target remains Ubuntu 26.04. This run does not certify the active
GDM/Wayland physical display or enable the installer on 25.10.

## Exact environment and source

- Source: `5c373874f00553c7fae3e19c473a8965944f7df2`, tree
  `89fd524b72dcd32f3e4fd881368e7dd6cf9bb1f8`. Later changes are documentation only.
- Host kernel `6.17.0-41-generic`, AMD64; container Ubuntu 25.10, GStreamer 1.26.6,
  Rust 1.95.0. Native tests use authenticated Xvfb/GTK and software capture.
- Image `remotevnc-build:25.10-rust1.95`, image index
  `sha256:c764d4de739da7d3256b523f14dcba90469b33000ae0e0839cc9af046cf93482`.
- Ordinary Docker, two CPUs, 4 GiB, 512 PID limit, one Cargo build job; separate
  `remotevnc-target-2510` target volume prevents mixing 26.04 native build outputs.
  No privileged mode, host networking or host filesystem mount was used.

The exact immutable source archive was piped to the container, then:

```sh
cargo test -p remotevnc-seat-helper -p remotevnc-agent --locked
bash scripts/test-x11.sh
bash scripts/test-e2e.sh
```

The combined command exited **0**. Raw output is preserved as
`ubuntu2510-compatibility.log` in the delivery audit, alongside the Dockerfile,
source-archive runner, environment-build logs and release-loader probe.

## Results

| Check | Observed result |
| --- | --- |
| Ordinary helper/agent tests | 40 passed, 0 failed, 36 explicitly ignored native fixtures. Required selected fixtures then ran separately below. |
| X11/GTK script | 12 top-level cases passed, including exact Vietnamese/NFC/NFD/CJK/non-BMP text, source reset, cleanup, capture/config-IDR, crop, four live profile transitions, and independent guardian recovery. Two additional recovery-child summaries are nested subprocesses, not extra top-level cases. |
| Native WebRTC long text | Exact 1,000 scalars in 56.755080506s; 568 advancing frames, three renewals; actual pointer moved at 297.261968ms after text started, before text completed. Motion was sent at 250ms; this observation is not input-to-image p95. Original lease crossing, focus/disconnect cancellation/no reinjection and shared GTK cleanup passed. |
| Native transport isolation | Recovery before the existing 20s budget passed. Persistent rejection closed/released at 20.004800785s despite healthy renewal/ACK/profile activity, with zero helper drops. Combined three native cases passed in 106.92s. |
| Signed-process E2E | Passed in 117.88s at original default 1080p60: real H264/WebRTC exact GTK Unicode, account B enumeration/create/signal denial, 250ms agent pause, fresh sessions after restarts and live revoke. |
| Fault release observations | Controller SIGKILL 27.625667777s; helper SIGKILL 74.507278ms; backend SIGKILL 10.069824346s; live revoke 146.262717ms. These are individual fixture observations, not hard scheduling bounds. |

The normal 26.04-built AMD agent (`b41ba7d`, SHA256
`776ee37899ee82d2c1b1c92c1f87391cb0284d58e210c18f7427b96d6fa72f7e`)
and helper (`1bc63df`, SHA256
`839c6e74ce8b792fb22d08ad0b4ffede34c09ce2ffb156b889da8b0ecab2c524`)
also reached their expected CLI usage errors in the 25.10 userspace, without a
loader/GLIBC failure. The no-argument processes intentionally exit 1; the probe
harness exited 0. This is loader/startup evidence, not a physical control test.
No additional release build was needed or restamped for that observation.

## Limits and retained setup failures

Physical boot/greeter/login/desktop/lock/unlock/logout/reboot, GPU capture, live
Google/Cloudflare relay, macOS interaction and hardware performance remain pending.
Exact shipped-unit Ready under the approved capability policy was demonstrated
separately on disposable Ubuntu 26.04/systemd 259.9, not under the host's 25.10
systemd. Native fixture OIDC does not exercise live Google consent. TURN fixtures
and local WebRTC do not certify Cloudflare relay.

The host remains 25.10/GDM/Wayland, without passwordless sudo, and lacks the native
nice package. No host package, display-manager, service, OS, reboot or ingress
change occurred. The 26.04 installer continues rejecting 25.10; there is no bypass.

Environment preparation retained an interrupted first quiet build and a bounded
retry exiting 124. Neither ran project tests or proves a product failure. A verbose
retry completed successfully: 501 MB/708 package dependencies were downloaded and
installed only inside the container. No exact cause is assigned to the earlier
quiet run. The successful complete project command was run once; no failed
compatibility assertion was hidden or relaxed.
