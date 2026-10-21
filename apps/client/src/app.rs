use crate::{
    Result,
    input::InputMapper,
    video::{LatestFrame, Viewport},
};
use eframe::egui;
use remotevnc_device::DeviceApi;
use remotevnc_protocol::*;
use remotevnc_transport::PeerStats;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
mod runtime;
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
            name: "RemoteVNC Mac".into(),
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
}
enum Command {
    Connect(DeviceId),
    Disconnect,
    Refresh,
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
}
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
        }
    }
    fn sign_in(&mut self) {
        self.worker.take();
        self.state.lock().unwrap().status = "Opening secure device sign-in…".into();
        match ClientRuntime::start(self.config.clone(), self.state.clone(), self.frames.clone()) {
            Ok(worker) => self.worker = Some(worker),
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
        egui::Frame::central_panel(ui.style()).show(ui, |ui| self.content(ui));
    }
    fn content(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(16));
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
        ui.add_space(12.);
        ui.heading("RemoteVNC");
        ui.label(&self.config.backend);
        ui.horizontal(|ui| {
            if signed_in {
                if ui.button("Sign out").clicked() {
                    self.sign_out();
                }
            } else if self.worker.is_none() {
                if ui.button("Sign in with Google").clicked() {
                    self.sign_in();
                }
                if ui.button("Clear local sign-in").clicked() {
                    self.sign_out();
                }
            } else if ui.button("Cancel sign-in").clicked() {
                self.worker.take();
                self.state.lock().unwrap().status = "Sign-in cancelled".into();
            }
            if ui.checkbox(&mut self.autostart, "Open at login").changed()
                && let Err(e) = crate::autostart::set_autostart(self.autostart)
            {
                self.autostart = crate::autostart::enabled().unwrap_or(false);
                self.state.lock().unwrap().status = e.to_string();
            }
        });
        ui.label(status);
        if let Some((code, url)) = enrollment {
            ui.label(format!("Confirmation code: {code}"));
            if ui.button("Open browser to confirm").clicked() {
                if url::Url::parse(&url)
                    .is_ok_and(|u| u.origin().ascii_serialization() == self.config.backend)
                {
                    if let Err(e) = std::process::Command::new("/usr/bin/open")
                        .arg(&url)
                        .spawn()
                    {
                        self.state.lock().unwrap().status = e.to_string();
                    }
                } else {
                    self.state.lock().unwrap().status = "Invalid sign-in URL".into();
                }
            }
        }
        if signed_in {
            ui.separator();
            ui.horizontal(|ui| {
                ui.strong("Devices");
                if ui.button("Refresh").clicked() {
                    self.send(Command::Refresh);
                }
            });
            if devices.is_empty() {
                ui.label(
                    "No other devices yet. Enroll your Ubuntu agent using this Google account.",
                );
            }
            for device in devices {
                let online = presence
                    .iter()
                    .any(|p| p.device_id == device.id && p.role == DeviceRole::Agent && p.online);
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            self.selected == Some(device.id),
                            format!(
                                "{} · {}",
                                device.name,
                                if online { "online" } else { "offline" }
                            ),
                        )
                        .clicked()
                    {
                        self.selected = Some(device.id);
                    }
                    if ui
                        .add_enabled(online && !connected, egui::Button::new("Connect"))
                        .clicked()
                    {
                        self.release();
                        self.selected = Some(device.id);
                        self.send(Command::Connect(device.id));
                    }
                });
            }
            if connected || connection_requested {
                let (decoded, fps) = self.frames.metrics();
                ui.label(format!(
                    "Video: {} · observed decode FPS: {} · physical display: {}",
                    decoded.map_or_else(|| "unavailable".into(), |(w, h)| format!("{w}×{h}")),
                    fps.map_or_else(|| "unavailable".into(), |fps| format!("{fps:.1}")),
                    display.map_or_else(
                        || "unavailable".into(),
                        |d| format!("{}×{}", d.width, d.height)
                    )
                ));
                ui.horizontal(|ui| {
                    if ui.button("Disconnect").clicked() {
                        self.release();
                        self.send(Command::Disconnect);
                    }
                    ui.label(format!(
                        "ICE: {} to {} · RTT: {} · dropped frames: {}",
                        candidate_name(stats.native.local_candidate),
                        candidate_name(stats.native.remote_candidate),
                        stats
                            .native
                            .rtt_ms
                            .map_or_else(|| "unavailable".into(), |v| format!("{v} ms")),
                        stats.dropped_frames
                    ));
                });
            }
        }
        ui.separator();
        if let Some(frame) = self
            .frames
            .take()
            .filter(|f| display.is_some_and(|d| d.generation == f.generation))
        {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [frame.width as usize, frame.height as usize],
                &frame.rgba,
            );
            if let Some(texture) = self.texture.as_mut() {
                texture.set(image, egui::TextureOptions::LINEAR);
            } else {
                self.texture =
                    Some(ctx.load_texture("remote-desktop", image, egui::TextureOptions::LINEAR));
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
            if self.remote_focus && (!focused || events.iter().any(|e|matches!(e,egui::Event::PointerButton{pos,pressed:true,..} if !rect.contains(*pos)))) {self.release();}
            if response.is_pointer_button_down_on() || response.clicked() {
                self.remote_focus = true;
                response.request_focus();
            }
            if ready && self.remote_focus && focused {
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
            ui.label("Remote desktop appears here after connection.");
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
    /// Explicit fixture renderer for the opt-in development example. It has no
    /// authenticated worker, credentials, or means to send remote input.
    pub fn preview(
        config: Config,
        state: ViewState,
        frame: remotevnc_transport::DecodedFrame,
    ) -> Self {
        let mut app = Self::new(config);
        app.scope = state.display.map(|d| (d.generation, state.epoch));
        *app.state.lock().unwrap() = state;
        app.frames.replace(frame);
        app.remote_focus = true;
        app
    }
}
fn candidate_name(candidate: Option<remotevnc_transport::stats::CandidateType>) -> &'static str {
    use remotevnc_transport::stats::CandidateType::*;
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
        app.frames.replace(remotevnc_transport::DecodedFrame {
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
}
