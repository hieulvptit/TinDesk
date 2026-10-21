# RemoteVNC app icons

| Client — macOS controller | Server — Ubuntu remote host |
| --- | --- |
| <img src="client-512.png" width="160" alt="RemoteVNC Client icon"> | <img src="server-512.png" width="160" alt="RemoteVNC Server icon"> |
| Cyan display, peer connection arrows and mouse pointer | Orange display, peer connection arrows and server rack |

Both icons share the navy tile and have transparent margins. Original PNGs are
1254×1254 RGBA. `client-512.png` and `server-512.png` are 512×512 exports for app
and desktop integration. Each `.icns` contains standard and Retina sizes from
16 through 1024 pixels.

The macOS build script installs `client.icns` as `RemoteVNC.icns` and Info.plist
references that resource. Existing built apps must be rebuilt to include it.
The Ubuntu server runs as background services; its PNG is ready for a future
desktop launcher or management UI.

Created with the built-in `image_gen` tool. Full prompts and reference information
are recorded in [generation.json](generation.json). The Server icon was generated
as a companion variant of the Client icon; PNG resizing and ICNS conversion use
macOS `sips` and `iconutil`.
