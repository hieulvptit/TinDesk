use crate::{
    Result,
    input::InputMapper,
    video::{LatestFrame, Viewport},
};
use binachicken_device::DeviceApi;
use binachicken_protocol::*;
use binachicken_transport::PeerStats;
use eframe::egui;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
mod runtime;
mod screens;
mod session;
pub use runtime::{ClientCredentialStore, ClientRuntime};
pub use session::{Authority, ControllerSession};
#[derive(Clone)]
pub struct Config {
    pub backend: String,
    pub name: String,
    pub relay_only: bool,
}
impl Config {
    pub fn new(backend: &str) -> Result<Self> {
        // Let the SDK enforce HTTPS/loopback and reject paths, credentials and queries.
        DeviceApi::new(backend, ed25519_dalek::SigningKey::from_bytes(&[0; 32]))?;
        let origin = url::Url::parse(backend)?.origin().ascii_serialization();
        Ok(Self {
            backend: origin,
            name: "BinaChicken Mac".into(),
            relay_only: false,
        })
    }
}
#[derive(Default)]
pub struct ViewState {
    pub status: String,
    pub signed_in: bool,
    pub enrollment: Option<(String, String)>,
    pub devices: Vec<DeviceSummary>,
    pub presence: Vec<DevicePresence>,
    pub display: Option<DisplayInfo>,
    pub epoch: u64,
    pub ready: bool,
    pub connected: bool,
    pub connection_requested: bool,
    pub stats: PeerStats,
    pub session_id: Option<SessionId>,
    pub connection: Option<binachicken_transport::ConnectionState>,
}
enum Command {
    Connect(DeviceId),
    Disconnect,
    Refresh,
    VideoProfile(VideoProfile),
    Input {
        generation: u64,
        epoch: u64,
        events: Vec<InputEvent>,
    },
}
enum NetworkEvent {
    Event(DeviceEvent),
    Offline,
    Denied,
    SessionRejected,
}
/// Mipmaps keep text legible when a large desktop is shown in a smaller window.
const VIDEO_TEXTURE: egui::TextureOptions = egui::TextureOptions {
    mipmap_mode: Some(egui::TextureFilter::Linear),
    ..egui::TextureOptions::LINEAR
};
/// One native own-window application. No desktop capture or global input hooks.
pub struct ClientApp {
    config: Config,
    state: Arc<Mutex<ViewState>>,
    worker: Option<ClientRuntime>,
    frames: LatestFrame,
    mapper: InputMapper,
    texture: Option<egui::TextureHandle>,
    frame_size: Option<(u32, u32, u64)>,
    scope: Option<(u64, u64)>,
    remote_focus: bool,
    autostart: bool,
    selected: Option<DeviceId>,
    theme_ready: bool,
    open_browser_when_ready: bool,
    video_profile: VideoProfile,
    toolbar_rect: Option<egui::Rect>,
    toolbar_until: f64,
    telemetry: crate::connection::Telemetry,
}
impl ClientApp {
    pub fn new(config: Config) -> Self {
        let (autostart, status) = match crate::autostart::enabled() {
            Ok(value) => (
                value,
                "Sign in to see devices on your Google account.".into(),
            ),
            Err(e) => (false, e.to_string()),
        };
        Self {
            config,
            state: Arc::new(Mutex::new(ViewState {
                status,
                ..Default::default()
            })),
            worker: None,
            frames: LatestFrame::default(),
            mapper: InputMapper::default(),
            texture: None,
            frame_size: None,
            scope: None,
            remote_focus: false,
            autostart,
            selected: None,
            theme_ready: false,
            open_browser_when_ready: false,
            video_profile: VideoProfile::default(),
            toolbar_rect: None,
            toolbar_until: 0.,
            telemetry: Default::default(),
        }
    }
    fn sign_in(&mut self) {
        self.open_browser_when_ready = true;
        self.worker.take();
        self.state.lock().unwrap().status = "Opening secure device sign-in…".into();
        match ClientRuntime::start(self.config.clone(), self.state.clone(), self.frames.clone()) {
            Ok(worker) => {
                worker.set_video_profile(self.video_profile);
                self.worker = Some(worker);
            }
            Err(e) => self.state.lock().unwrap().status = e.to_string(),
        }
    }
    fn sign_out(&mut self) {
        let result = if let Some(worker) = self.worker.take() {
            worker.sign_out()
        } else {
            runtime::native_store(&self.config)
                .and_then(|s| s.delete().map_err(Into::into))
                .map(|_| "Signed out locally; backend revocation was not confirmed".into())
        };
        self.frames.clear();
        self.texture = None;
        self.frame_size = None;
        self.mapper.reset();
        self.remote_focus = false;
        let status = result.unwrap_or_else(|e| e.to_string());
        *self.state.lock().unwrap() = ViewState {
            status,
            ..Default::default()
        };
    }
    fn send(&self, command: Command) {
        if let Some(worker) = &self.worker {
            worker.send(command);
        }
    }
    fn release(&mut self) {
        if let Some((generation, epoch)) = self.scope {
            self.send(Command::Input {
                generation,
                epoch,
                events: vec![InputEvent::ReleaseAll],
            });
        }
        self.mapper.reset();
        self.remote_focus = false;
    }
}
impl ClientApp {
    fn render(&mut self, ui: &mut egui::Ui) {
        if !self.theme_ready {
            screens::apply_theme(ui.ctx());
            self.theme_ready = true;
        }
        ui.set_style(ui.ctx().global_style());
        let remote = {
            let s = self.state.lock().unwrap();
            s.signed_in && (s.connected || s.connection_requested)
        };
        let margin = if remote { 0. } else { 8. };
        let size = ui.available_size() - egui::vec2(margin * 2., margin * 2.);
        egui::Frame::central_panel(ui.style())
            .inner_margin(margin)
            .show(ui, |ui| {
                ui.set_min_size(size);
                self.content(ui);
            });
    }
    fn content(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(16));
        self.frames.set_repaint(&ctx);
        if self.worker.as_ref().is_some_and(ClientRuntime::is_finished) {
            self.worker.take();
        }
        let (
            signed_in,
            status,
            enrollment,
            devices,
            presence,
            display,
            epoch,
            ready,
            connected,
            connection_requested,
            stats,
            session_id,
            connection,
        ) = {
            let s = self.state.lock().unwrap();
            (
                s.signed_in,
                s.status.clone(),
                s.enrollment.clone(),
                s.devices.clone(),
                s.presence.clone(),
                s.display,
                s.epoch,
                s.ready,
                s.connected,
                s.connection_requested,
                s.stats,
                s.session_id,
                s.connection,
            )
        };
        let scope = display.map(|d| (d.generation, epoch));
        if self.scope != scope || !ready {
            self.mapper = InputMapper::default();
            self.texture = None;
            self.frame_size = None;
            if self.scope != scope {
                self.remote_focus = false;
            }
            self.scope = scope;
        }
        if !signed_in {
            self.toolbar_rect = None;
            self.auth_screen(
                ui,
                &status,
                enrollment.as_ref().map(|(_, url)| url.as_str()),
            );
            return;
        }
        if !connected && !connection_requested {
            self.toolbar_rect = None;
            self.devices_screen(ui, &devices, &presence, &status);
            return;
        }
        self.telemetry.observe(
            session_id.map(|id| (id, epoch)),
            stats.native,
            ctx.input(|i| i.time),
        );
        let status_blocks_input =
            self.connection_overlay(&ctx, &devices, &status, &stats, connection);
        let toolbar_blocks_input =
            self.remote_toolbar(ui.ctx(), &devices, ready, &status, &stats) || status_blocks_input;
        if let Some(frame) = self
            .frames
            .take()
            .filter(|f| display.is_some_and(|d| d.generation == f.generation))
        {
            // Decoded video is opaque (alpha 255), so this is an exact bulk copy
            // instead of a per-pixel unmultiply.
            let image = egui::ColorImage::from_rgba_premultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            );
            if let Some(texture) = self.texture.as_mut() {
                texture.set(image, VIDEO_TEXTURE);
            } else {
                self.texture = Some(ctx.load_texture("remote-desktop", image, VIDEO_TEXTURE));
            }
            self.frame_size = Some((frame.width, frame.height, frame.generation));
        }
        if let (Some(d), Some(texture), Some((width, height, _))) =
            (display, self.texture.as_ref(), self.frame_size)
        {
            let bounds = ui.available_rect_before_wrap();
            let viewport = Viewport::fit(
                Viewport {
                    left: bounds.left(),
                    top: bounds.top(),
                    width: bounds.width(),
                    height: bounds.height(),
                },
                width,
                height,
            );
            let viewport = viewport.snap(width, height, ctx.pixels_per_point());
            let rect = egui::Rect::from_min_size(
                egui::pos2(viewport.left, viewport.top),
                egui::vec2(viewport.width, viewport.height),
            );
            let response = ui.interact(
                rect,
                ui.scope_id().with("remote-desktop"),
                egui::Sense::click_and_drag(),
            );
            ui.painter().image(
                texture.id(),
                rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1., 1.)),
                egui::Color32::WHITE,
            );
            let (events, modifiers, pointer, focused) = ctx.input(|i| {
                (
                    i.events.clone(),
                    i.modifiers,
                    i.pointer.latest_pos(),
                    i.focused,
                )
            });
            if self.remote_focus && (toolbar_blocks_input || !focused || events.iter().any(|e|matches!(e,egui::Event::PointerButton{pos,pressed:true,..} if !rect.contains(*pos)))) {self.release();}
            if !toolbar_blocks_input && (response.is_pointer_button_down_on() || response.clicked())
            {
                self.remote_focus = true;
                response.request_focus();
            }
            if ready && self.remote_focus && focused && !toolbar_blocks_input {
                ctx.memory_mut(|m| {
                    m.set_focus_lock_filter(
                        response.id,
                        egui::EventFilter {
                            tab: true,
                            horizontal_arrows: true,
                            vertical_arrows: true,
                            escape: true,
                        },
                    )
                });
                ctx.output_mut(|o| {
                    o.ime = Some(egui::output::IMEOutput {
                        purpose: egui::IMEPurpose::Normal,
                        rect,
                        cursor_rect: egui::Rect::from_min_size(
                            pointer.unwrap_or(rect.center()),
                            egui::vec2(1., 18.),
                        ),
                        should_interrupt_composition: false,
                    });
                });
                let events = self
                    .mapper
                    .native_batch(&events, modifiers, pointer, viewport, d);
                if !events.is_empty() {
                    self.send(Command::Input {
                        generation: d.generation,
                        epoch,
                        events,
                    });
                }
            }
        } else {
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.38).max(24.));
                ui.spinner();
                ui.add_space(16.);
                ui.heading("Connecting to your desktop");
                ui.add_space(8.);
                ui.label(&status);
            });
        }
    }
}

impl eframe::App for ClientApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render(ui);
    }
}

#[cfg(feature = "native-smoke")]
impl ClientApp {
    /// Fixture-only: keep controls visible without moving the system pointer.
    pub fn preview_show_toolbar(&mut self) {
        self.toolbar_until = f64::INFINITY;
    }
    /// Explicit fixture data only; never compiled into the normal client.
    pub fn preview_connection_measurements(&mut self, slow: bool) {
        let mut state = self.state.lock().unwrap();
        let scope = state.session_id.map(|id| (id, state.epoch));
        for sample in 1..=4 {
            state.stats.native.sampled_at_ms = Some(sample * 1000);
            state.stats.native.bytes_received = Some(sample * 625000);
            state.stats.native.packets_received = Some(sample * 1000);
            state.stats.native.packets_lost = Some(0);
            state.stats.native.rtt_ms = Some(if slow { 300 } else { 25 });
            self.telemetry.observe(scope, state.stats.native, 0.);
        }
    }
    /// Explicit fixture renderer for the opt-in development example. It has no
    /// authenticated worker, credentials, or means to send remote input.
    pub fn preview(
        config: Config,
        state: ViewState,
        frame: binachicken_transport::DecodedFrame,
    ) -> Self {
        let mut app = Self::new(config);
        app.scope = state.display.map(|d| (d.generation, state.epoch));
        *app.state.lock().unwrap() = state;
        app.frames.replace(frame);
        app.remote_focus = true;
        app
    }
}
fn candidate_name(candidate: Option<binachicken_transport::stats::CandidateType>) -> &'static str {
    use binachicken_transport::stats::CandidateType::*;
    match candidate {
        Some(Host) => "host",
        Some(ServerReflexive) => "server reflexive",
        Some(PeerReflexive) => "peer reflexive",
        Some(Relay) => "relay",
        None => "unavailable",
    }
}

#[cfg(test)]
mod ui_tests {
    use super::*;
    fn rendered_text(app: &mut ClientApp) -> String {
        fn append(shape: &egui::epaint::Shape, out: &mut String) {
            match shape {
                egui::epaint::Shape::Text(text) => {
                    out.push_str(text.galley.text());
                    out.push('\n');
                }
                egui::epaint::Shape::Vec(shapes) => {
                    for shape in shapes {
                        append(shape, out);
                    }
                }
                _ => {}
            }
        }
        let ctx = egui::Context::default();
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1100., 760.),
                )),
                ..Default::default()
            },
            |ui| app.render(ui),
        );
        let mut text = String::new();
        for shape in &output.shapes {
            append(&shape.shape, &mut text);
        }
        output.drop_without_applying_deltas();
        text
    }
    #[test]
    fn auth_devices_and_remote_render_separate_screens() {
        let mut app = ClientApp::new(Config::new("http://127.0.0.1:1").unwrap());
        let auth = rendered_text(&mut app);
        assert!(auth.contains("Continue with Google"), "{auth}");
        assert!(!auth.contains("\nYour computers\n"));
        assert!(!auth.contains("Connection details"));
        app.state.lock().unwrap().signed_in = true;
        let devices = rendered_text(&mut app);
        assert!(devices.contains("Your computers"), "{devices}");
        assert!(!devices.contains("Continue with Google"));
        assert!(!devices.contains("Connection details"));
        app.state.lock().unwrap().connection_requested = true;
        let remote = rendered_text(&mut app);
        assert!(
            !remote.contains("Connection details"),
            "remote actions use icons"
        );
        assert!(
            app.toolbar_rect.is_some(),
            "connecting still exposes disconnect"
        );
        assert!(remote.contains("Connecting to your desktop"));
        assert!(!remote.contains("Your computers"));
        assert!(!remote.contains("Continue with Google"));
        assert!(!remote.contains("unavailable"));
    }
    #[test]
    fn focused_remote_image_requests_native_ime_area_and_focus_loss_removes_it() {
        let mut app = ClientApp::new(Config::new("http://127.0.0.1:1").unwrap());
        let display = DisplayInfo {
            generation: 1,
            width: 1920,
            height: 1080,
        };
        {
            let mut s = app.state.lock().unwrap();
            s.signed_in = true;
            s.display = Some(display);
            s.ready = true;
            s.connected = true;
        }
        app.scope = Some((1, 0));
        app.remote_focus = true;
        app.frames.replace(binachicken_transport::DecodedFrame {
            generation: 1,
            width: 2,
            height: 2,
            rgba: vec![255; 16],
            pts_ns: None,
        });
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000., 800.),
            )),
            focused: true,
            ..Default::default()
        };
        let out = ctx.run_ui(input.clone(), |ui| app.render(ui));
        let ime = out
            .platform_output
            .ime
            .expect("image control enables IME without a TextEdit");
        assert!(ime.rect.width() > 100. && ime.rect.height() > 100.);
        assert_eq!(
            ime.rect.top(),
            0.,
            "toolbar must not reserve vertical space"
        );
        assert_eq!(ime.rect.height(), 800.);
        assert!(ime.cursor_rect.width() > 0.);
        out.drop_without_applying_deltas();
        let out = ctx.run_ui(
            egui::RawInput {
                focused: false,
                events: vec![egui::Event::WindowFocused(false)],
                ..input
            },
            |ui| app.render(ui),
        );
        assert!(out.platform_output.ime.is_none());
        assert!(!app.remote_focus);
        out.drop_without_applying_deltas();
    }

    #[test]
    fn hover_toolbar_and_open_menus_never_forward_input_to_the_desktop() {
        let mut app = ClientApp::new(Config::new("http://127.0.0.1:1").unwrap());
        {
            let mut state = app.state.lock().unwrap();
            state.signed_in = true;
            state.connected = true;
            state.ready = true;
            state.display = Some(DisplayInfo {
                generation: 1,
                width: 1000,
                height: 800,
            });
        }
        app.scope = Some((1, 0));
        app.remote_focus = true;
        app.frames.replace(binachicken_transport::DecodedFrame {
            generation: 1,
            width: 2,
            height: 2,
            rgba: vec![255; 16],
            pts_ns: None,
        });
        let ctx = egui::Context::default();
        let raw = |time, events| egui::RawInput {
            time: Some(time),
            focused: true,
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000., 800.),
            )),
            ..Default::default()
        };
        let render = |time, events: Vec<egui::Event>, app: &mut ClientApp| {
            let output = ctx.run_ui(raw(time, events), |ui| app.render(ui));
            let ime = output.platform_output.ime;
            output.drop_without_applying_deltas();
            ime
        };
        assert!(render(1., vec![], &mut app).is_some());
        assert!(
            app.toolbar_rect.is_none(),
            "hidden toolbar uses no layout space"
        );
        render(
            1.1,
            vec![egui::Event::PointerMoved(egui::pos2(500., 4.))],
            &mut app,
        );
        render(1.2, vec![], &mut app); // Finish the overlay sizing pass.
        render(1.25, vec![], &mut app); // read_response reads the preceding pass.
        assert!(app.toolbar_rect.is_some());
        let settings = ctx
            .read_response(egui::Id::unique("remote-video-settings"))
            .unwrap()
            .rect
            .center();
        assert!(
            render(1.3, vec![egui::Event::PointerMoved(settings)], &mut app).is_none(),
            "settings={settings:?} toolbar={:?} focus={}",
            app.toolbar_rect,
            app.remote_focus
        );
        assert!(
            !app.remote_focus,
            "hovering controls releases held remote input"
        );
        let button = |position, pressed| egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        render(1.4, vec![button(settings, true)], &mut app);
        render(1.5, vec![button(settings, false)], &mut app);
        assert!(
            egui::Popup::is_any_open(&ctx),
            "icon opens the video settings menu"
        );
        assert!(
            render(
                3.,
                vec![
                    egui::Event::PointerMoved(egui::pos2(100., 700.)),
                    egui::Event::Text("local settings input".into())
                ],
                &mut app
            )
            .is_none()
        );
        assert!(
            app.toolbar_rect.is_some(),
            "an open popup keeps the toolbar visible"
        );
        render(3.1, vec![button(egui::pos2(100., 700.), true)], &mut app);
        render(3.2, vec![button(egui::pos2(100., 700.), false)], &mut app);
        assert!(
            !app.remote_focus,
            "outside close-click cannot click Ubuntu underneath"
        );
        render(5., vec![], &mut app);
        assert!(app.toolbar_rect.is_none(), "toolbar hides after leaving it");
        let corner = ctx
            .read_response(egui::Id::unique("remote-connection-info"))
            .unwrap()
            .rect;
        assert!(
            corner.right() > 970. && corner.top() < 30.,
            "status stays in the top-right corner"
        );
        app.remote_focus = true;
        assert!(
            render(
                5.1,
                vec![egui::Event::PointerMoved(corner.center())],
                &mut app
            )
            .is_none()
        );
        assert!(
            !app.remote_focus,
            "corner overlay must also release remote input"
        );
        render(5.2, vec![button(corner.center(), true)], &mut app);
        render(5.3, vec![button(corner.center(), false)], &mut app);
        assert!(
            egui::Popup::is_any_open(&ctx),
            "corner indicator opens connection information"
        );
    }
}
