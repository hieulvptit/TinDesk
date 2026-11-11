//! Explicit own-window rendering fixture. No Keychain, OAuth, network or input injection.
use binachicken_client::app::{ClientApp, Config, ViewState};
use binachicken_protocol::{
    DeviceId, DevicePresence, DeviceRole, DeviceSummary, DisplayInfo, SessionId,
};
use binachicken_transport::DecodedFrame;
use eframe::App;
struct Smoke(ClientApp, bool, Option<&'static str>, usize);
impl App for Smoke {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, frame: &mut eframe::Frame) {
        self.0.ui(ui, frame);
        if let Some(widget) = self.2
            && let Some(button) = ui.ctx().read_response(eframe::egui::Id::unique(widget))
        {
            eframe::egui::Popup::open_id(
                ui.ctx(),
                eframe::egui::Popup::default_response_id(&button),
            );
        }
        if !self.1 && ui.ctx().output(|o| o.ime.is_some()) {
            println!(
                "Native fixture rendered devices and focused image; IME output enabled with candidate area."
            );
            self.1 = true;
        }
        self.3 += 1;
        // Overlay areas need several layout passes; the built-in two-pass
        // screenshot exits before a freshly opened menu becomes visible.
        if self.3 == 8
            && let Ok(path) = std::env::var("BINACHICKEN_PREVIEW_PPM")
        {
            ui.ctx()
                .send_viewport_cmd(eframe::egui::ViewportCommand::Screenshot(
                    eframe::egui::ScreenshotCallback::new(move |image| {
                        use std::io::Write;
                        let mut file = std::fs::File::create(path).unwrap();
                        write!(file, "P6\n{} {}\n255\n", image.width(), image.height()).unwrap();
                        let rgb: Vec<u8> = image
                            .pixels
                            .iter()
                            .flat_map(|p| [p.r(), p.g(), p.b()])
                            .collect();
                        file.write_all(&rgb).unwrap();
                        std::process::exit(0);
                    }),
                ));
        }
    }
}
fn main() -> eframe::Result {
    let id = DeviceId(uuid::Uuid::from_u128(1));
    let mut state = ViewState {
        status: String::new(),
        signed_in: true,
        devices: vec![DeviceSummary {
            id,
            name: "Ubuntu workstation".into(),
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
    // Choose a screen without authenticating, opening a browser or sending input.
    let mode = std::env::args().nth(1);
    eprintln!("Fixture screen: {mode:?}");
    match mode.as_deref() {
        Some("auth") => state = ViewState::default(),
        Some("devices") => {
            state.display = None;
            state.ready = false;
            state.connected = false;
            state.devices.push(DeviceSummary {
                id: DeviceId(uuid::Uuid::from_u128(2)),
                name: "Studio workstation".into(),
                public_key: String::new(),
                revoked: false,
            });
        }
        Some(
            "remote" | "remote-toolbar" | "remote-settings" | "remote-details" | "p2p-slow"
            | "relay" | "relay-slow" | "offline",
        )
        | None => {}
        _ => panic!("expected auth, devices, remote, remote-toolbar or remote-settings"),
    }
    if state.connected {
        state.session_id = Some(SessionId::from_uuid(uuid::Uuid::from_u128(1)));
        state.connection = Some(if mode.as_deref() == Some("offline") {
            binachicken_transport::ConnectionState::Disconnected
        } else {
            binachicken_transport::ConnectionState::Connected
        });
        state.stats.native.local_candidate =
            Some(binachicken_transport::stats::CandidateType::Host);
        state.stats.native.remote_candidate =
            Some(if matches!(mode.as_deref(), Some("relay" | "relay-slow")) {
                binachicken_transport::stats::CandidateType::Relay
            } else {
                binachicken_transport::stats::CandidateType::Host
            });
    }
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
    let mut app = ClientApp::preview(
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
    if !matches!(mode.as_deref(), Some("auth" | "devices")) {
        app.preview_connection_measurements(matches!(
            mode.as_deref(),
            Some("p2p-slow" | "relay-slow")
        ));
    }
    if matches!(mode.as_deref(), Some("remote-toolbar" | "remote-settings")) {
        app.preview_show_toolbar();
    }
    eframe::run_native(
        "BinaChicken rendering fixture",
        eframe::NativeOptions {
            clipboard_shortcuts: false,
            viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1100., 760.]),
            ..Default::default()
        },
        Box::new(move |_| {
            Ok(Box::new(Smoke(
                app,
                false,
                match mode.as_deref() {
                    Some("remote-settings") => Some("remote-video-settings"),
                    Some("remote-details") => Some("remote-connection-info"),
                    _ => None,
                },
                0,
            )))
        }),
    )
}
