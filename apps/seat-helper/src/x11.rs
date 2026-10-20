//! Explicit local socket and server-owned authority; never modifies process environment.
use crate::{Result, error, seat::DisplayTarget};
use std::{
    fs::File,
    io::{Cursor, Read},
    os::unix::net::UnixStream,
};
use x11rb::rust_connection::{DefaultStream, RustConnection};

fn field(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut size = [0; 2];
    reader.read_exact(&mut size)?;
    let mut value = vec![0; u16::from_be_bytes(size) as usize];
    reader.read_exact(&mut value)?;
    Ok(value)
}
pub fn connect(target: &DisplayTarget) -> Result<RustConnection> {
    let number = crate::seat::display_number(&target.display)
        .ok_or_else(|| error("invalid display"))?
        .to_string();
    let mut data = Vec::new();
    File::open(&target.authority)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut data)?;
    if data.len() > 1024 * 1024 {
        return Err(error("authority file too large"));
    }
    let mut reader = Cursor::new(data.as_slice());
    let mut cookie = None;
    while (reader.position() as usize) < data.len() {
        let mut family = [0; 2];
        reader.read_exact(&mut family)?;
        let _address = field(&mut reader)?;
        let display = field(&mut reader)?;
        let name = field(&mut reader)?;
        let secret = field(&mut reader)?;
        if matches!(u16::from_be_bytes(family), 256 | 65535)
            && display == number.as_bytes()
            && name == b"MIT-MAGIC-COOKIE-1"
            && secret.len() == 16
        {
            cookie = Some((name, secret));
            break;
        }
    }
    let (name, secret) = cookie.ok_or_else(|| error("no local X11 authority cookie"))?;
    let stream = UnixStream::connect(target.socket_path()?)?;
    let (stream, _) = DefaultStream::from_unix_stream(stream)?;
    Ok(RustConnection::connect_to_stream_with_auth_info(
        stream, 0, name, secret,
    )?)
}

/// A live connection detects X server reset even when socket, PID and auth stay put.
#[derive(Default)]
pub struct ServerProbe {
    connection: Option<RustConnection>,
    target: Option<DisplayTarget>,
    epoch: u64,
}
impl ServerProbe {
    pub fn invalidate(&mut self) {
        self.connection = None;
        self.target = None;
    }
    pub fn refresh(&mut self, target: &DisplayTarget) -> Result<(u64, u32, u32)> {
        use x11rb::{connection::Connection, protocol::xproto::ConnectionExt};
        let same = self.target.as_ref() == Some(target);
        let live = same
            && self.connection.as_ref().is_some_and(|c| {
                c.get_input_focus()
                    .ok()
                    .and_then(|r| r.reply().ok())
                    .is_some()
            });
        if !live {
            self.connection = Some(connect(target)?);
            self.epoch = self
                .epoch
                .checked_add(1)
                .ok_or_else(|| error("display identity overflow"))?;
            self.target = Some(target.clone());
        }
        let connection = self
            .connection
            .as_ref()
            .ok_or_else(|| error("no X11 probe connection"))?;
        let geometry = connection
            .get_geometry(connection.setup().roots[0].root)?
            .reply()?;
        Ok((self.epoch, geometry.width.into(), geometry.height.into()))
    }
}

/// Query active connected RandR outputs, never silently capture the entire root.
pub fn selected_output(
    target: &DisplayTarget,
    selector: Option<&str>,
) -> Result<crate::seat::OutputGeometry> {
    use x11rb::{
        connection::Connection,
        protocol::randr::{Connection as OutputConnection, ConnectionExt},
    };
    let c = connect(target)?;
    let root = c.setup().roots[0].root;
    let resources = c.randr_get_screen_resources_current(root)?.reply()?;
    let primary = c.randr_get_output_primary(root)?.reply()?.output;
    let mut outputs = Vec::new();
    for id in resources.outputs {
        let output = c
            .randr_get_output_info(id, resources.config_timestamp)?
            .reply()?;
        if output.connection != OutputConnection::CONNECTED || output.crtc == 0 {
            continue;
        }
        let crtc = c
            .randr_get_crtc_info(output.crtc, resources.config_timestamp)?
            .reply()?;
        if crtc.mode == 0 || crtc.width == 0 || crtc.height == 0 || crtc.x < 0 || crtc.y < 0 {
            continue;
        }
        outputs.push(crate::seat::OutputGeometry {
            name: String::from_utf8(output.name)?,
            x: crtc.x as u32,
            y: crtc.y as u32,
            width: crtc.width.into(),
            height: crtc.height.into(),
            primary: id == primary,
        });
    }
    crate::seat::select_output(&outputs, selector)
}
