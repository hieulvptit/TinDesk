//! Explicit own-window rendering fixture. No Keychain, OAuth, network or input injection.
use eframe::App;
use remotevnc_client::app::{ClientApp, Config, ViewState};
use remotevnc_protocol::{DeviceId, DevicePresence, DeviceRole, DeviceSummary, DisplayInfo};
use remotevnc_transport::DecodedFrame;
struct Smoke(ClientApp, bool);
impl App for Smoke {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        self.0.ui(ui, frame);
        if !self.1 && ui.ctx().output(|o| o.ime.is_some()) {
            println!(
                "Native fixture rendered devices and focused image; IME output enabled with candidate area."
            );
            self.1 = true;
        }
    }
}
fn main() -> eframe::Result {
    let id = DeviceId(uuid::Uuid::from_u128(1));
    let state = ViewState {
        status: "Rendering fixture — no authenticated session".into(),
        signed_in: true,
        devices: vec![DeviceSummary {
            id,
            name: "Ubuntu rendering fixture".into(),
            public_key: String::new(),
            revoked: false,
        }],
        presence: vec![DevicePresence {
            device_id: id,
            role: DeviceRole::Agent,
            online: true,
        }],
        display: Some(DisplayInfo {
            generation: 1,
            width: 1920,
            height: 1080,
        }),
        ready: true,
        connected: true,
        ..Default::default()
    };
    let mut rgba = vec![0; 640 * 360 * 4];
    for y in 0..360 {
        for x in 0..640 {
            let i = (y * 640 + x) * 4;
            rgba[i] = (x * 255 / 640) as u8;
            rgba[i + 1] = (y * 255 / 360) as u8;
            rgba[i + 2] = 100;
            rgba[i + 3] = 255;
        }
    }
    let app = ClientApp::preview(
        Config::new("http://127.0.0.1:1").unwrap(),
        state,
        DecodedFrame {
            generation: 1,
            width: 640,
            height: 360,
            rgba,
            pts_ns: None,
        },
    );
    eframe::run_native(
        "RemoteVNC rendering fixture",
        eframe::NativeOptions {
            clipboard_shortcuts: false,
            viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1100., 760.]),
            ..Default::default()
        },
        Box::new(move |_| Ok(Box::new(Smoke(app, false)))),
    )
}
