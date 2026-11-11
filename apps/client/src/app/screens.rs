use super::*;

const BACKGROUND: egui::Color32 = egui::Color32::from_rgb(13, 19, 30);
const SURFACE: egui::Color32 = egui::Color32::from_rgb(23, 32, 46);
const BORDER: egui::Color32 = egui::Color32::from_rgb(43, 57, 76);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(67, 211, 225);
const MUTED: egui::Color32 = egui::Color32::from_rgb(154, 171, 193);
const ONLINE: egui::Color32 = egui::Color32::from_rgb(87, 214, 158);

pub(super) fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BACKGROUND;
    visuals.window_fill = SURFACE;
    visuals.override_text_color = Some(egui::Color32::from_rgb(235, 241, 249));
    visuals.faint_bg_color = SURFACE;
    visuals.selection.bg_fill = ACCENT.gamma_multiply(0.3);
    visuals.widgets.inactive.bg_fill = SURFACE;
    visuals.widgets.inactive.weak_bg_fill = SURFACE;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1., BORDER);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(39, 57, 76);
    visuals.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(39, 57, 76);
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals(visuals);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.spacing.item_spacing = egui::vec2(12., 12.);
        style.spacing.button_padding = egui::vec2(18., 11.);
        style
            .text_styles
            .insert(egui::TextStyle::Body, egui::FontId::proportional(15.));
        style
            .text_styles
            .insert(egui::TextStyle::Button, egui::FontId::proportional(15.));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, egui::FontId::proportional(27.));
    });
}

fn card() -> egui::Frame {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1., BORDER))
        .corner_radius(18)
        .inner_margin(24)
}

fn primary(label: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(label).strong().color(BACKGROUND))
        .fill(ACCENT)
        .corner_radius(9)
}

fn monitor(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, 14., ACCENT.gamma_multiply(0.12));
    let screen = rect.shrink2(egui::vec2(size * 0.22, size * 0.28));
    let stroke = egui::Stroke::new(2., ACCENT);
    ui.painter()
        .rect_stroke(screen, 3., stroke, egui::StrokeKind::Inside);
    let center = screen.center_bottom();
    ui.painter()
        .line_segment([center, center + egui::vec2(0., size * 0.1)], stroke);
    ui.painter().line_segment(
        [
            center + egui::vec2(-size * 0.12, size * 0.1),
            center + egui::vec2(size * 0.12, size * 0.1),
        ],
        stroke,
    );
}

impl ClientApp {
    fn open_sign_in_url(&mut self, url: &str) {
        if !url::Url::parse(url)
            .is_ok_and(|u| u.origin().ascii_serialization() == self.config.backend)
        {
            self.state.lock().unwrap().status = "Invalid sign-in URL".into();
            return;
        }
        if let Err(error) = std::process::Command::new("/usr/bin/open").arg(url).spawn() {
            self.state.lock().unwrap().status = error.to_string();
        }
    }

    pub(super) fn auth_screen(&mut self, ui: &mut egui::Ui, status: &str, url: Option<&str>) {
        if self.open_browser_when_ready
            && let Some(url) = url
        {
            self.open_browser_when_ready = false;
            self.open_sign_in_url(url);
        }
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.15).max(24.));
            monitor(ui, 76.);
            ui.add_space(15.);
            ui.label(egui::RichText::new("BinaChicken").size(34.).strong());
            ui.label(egui::RichText::new("Your computers. Wherever you are.").color(MUTED));
            ui.add_space(28.);
            card().show(ui, |ui| {
                ui.set_width(360.);
                ui.vertical_centered(|ui| {
                    ui.heading("Welcome back");
                    ui.label(
                        egui::RichText::new("Sign in to access computers on your Google account.")
                            .color(MUTED),
                    );
                    ui.add_space(12.);
                    if self.worker.is_none() {
                        if ui
                            .add_sized([330., 48.], primary("G   Continue with Google"))
                            .clicked()
                        {
                            self.sign_in();
                        }
                    } else {
                        ui.spinner();
                        ui.label("Complete sign-in in your browser");
                        if let Some(url) = url
                            && ui.button("Open Google sign-in again").clicked()
                        {
                            self.open_sign_in_url(url);
                        }
                        if ui.small_button("Cancel").clicked() {
                            self.open_browser_when_ready = false;
                            self.worker.take();
                            self.state.lock().unwrap().status = "Sign-in cancelled".into();
                        }
                    }
                    if !status.is_empty() && !status.starts_with("Sign in to see") {
                        ui.add_space(8.);
                        ui.label(egui::RichText::new(status).small().color(MUTED));
                    }
                });
            });
            ui.add_space(22.);
            ui.label(
                egui::RichText::new("Only devices on the same Google account can connect.")
                    .small()
                    .color(MUTED),
            );
        });
    }

    pub(super) fn devices_screen(
        &mut self,
        ui: &mut egui::Ui,
        devices: &[DeviceSummary],
        presence: &[DevicePresence],
        status: &str,
    ) {
        egui::Frame::new().inner_margin(28).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("BinaChicken").strong().size(20.));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.menu_button("Settings", |ui| {
                        if ui.checkbox(&mut self.autostart, "Open at login").changed()
                            && let Err(e) = crate::autostart::set_autostart(self.autostart)
                        {
                            self.autostart = crate::autostart::enabled().unwrap_or(false);
                            self.state.lock().unwrap().status = e.to_string();
                        }
                        ui.separator();
                        if ui.button("Sign out").clicked() {
                            self.sign_out();
                            ui.close();
                        }
                    });
                    if ui.button("Refresh").clicked() {
                        self.send(Command::Refresh);
                    }
                });
            });
            ui.add_space(32.);
            ui.heading("Your computers");
            ui.label(
                egui::RichText::new("Choose a computer to start a remote session.").color(MUTED),
            );
            ui.add_space(20.);
            egui::ScrollArea::vertical().show(ui, |ui| {
                if devices.is_empty() {
                    card().show(ui, |ui| {
                        ui.set_width((ui.available_width() - 48.).max(240.));
                        ui.heading("No computers yet");
                        ui.label("Sign in to BinaChicken on Ubuntu with the same Google account.");
                    });
                }
                for device in devices {
                    let online = presence.iter().any(|p| {
                        p.device_id == device.id && p.role == DeviceRole::Agent && p.online
                    });
                    card().show(ui, |ui| {
                        ui.set_min_width((ui.available_width() - 1.).max(240.));
                        ui.horizontal(|ui| {
                            monitor(ui, 56.);
                            ui.vertical(|ui| {
                                ui.label(egui::RichText::new(&device.name).size(19.).strong());
                                ui.label(
                                    egui::RichText::new(if online { "Online" } else { "Offline" })
                                        .color(if online { ONLINE } else { MUTED }),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.add_enabled(online, primary("Connect")).clicked() {
                                        self.release();
                                        self.selected = Some(device.id);
                                        self.send(Command::Connect(device.id));
                                    }
                                },
                            );
                        });
                    });
                    ui.add_space(4.);
                }
            });
            if !status.is_empty() && status != "Choose an online Ubuntu agent" {
                ui.add_space(16.);
                ui.label(egui::RichText::new(status).small().color(MUTED));
            }
        });
    }

    /// Persistent corner status; it overlays the desktop and consumes no layout height.
    pub(super) fn connection_overlay(
        &mut self,
        ctx: &egui::Context,
        devices: &[DeviceSummary],
        status: &str,
        stats: &PeerStats,
        connection: Option<binachicken_transport::ConnectionState>,
    ) -> bool {
        use crate::connection::{Badge, speed};
        let popup_was_open = egui::Popup::is_any_open(ctx);
        let mut measurements = self.telemetry.current(ctx.input(|i| i.time));
        let badge = Badge::classify(connection, stats.native, measurements.slow);
        if badge == Badge::Offline {
            measurements = crate::connection::Measurements::default();
        }
        let area = egui::Area::new(egui::Id::unique("connection-overlay"))
            .order(egui::Order::Foreground).fade_in(false)
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-8., 8.)).movable(false)
            .show(ctx, |ui| {
                egui::Frame::new().fill(SURFACE).stroke(egui::Stroke::new(1., BORDER))
                    .corner_radius(10).inner_margin(6).show(ui, |ui| {
                    let width = (ctx.content_rect().width() - 28.).clamp(120., 220.);
                    let (_, rect) = ui.allocate_space(egui::vec2(width, 36.));
                    let button = ui.interact(rect, egui::Id::unique("remote-connection-info"), egui::Sense::click());
                    button.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true,
                        format!("{} — connection details", badge.label())));
                    if button.hovered() { ui.painter().rect_filled(rect, 6., BORDER); }
                    connection_icon(ui.painter(), egui::pos2(rect.left()+14., rect.center().y), badge);
                    let caption = match badge {
                        Badge::Direct => "P2P", Badge::DirectSlow => "P2P · Slow",
                        Badge::Relay => "Via server · TURN", Badge::RelaySlow => "TURN · Slow",
                        Badge::Offline => "Disconnected", Badge::Pending => "Connecting / measuring",
                    };
                    ui.painter().text(egui::pos2(rect.left()+36., rect.top()+8.),
                        egui::Align2::LEFT_CENTER, caption, egui::FontId::proportional(12.), egui::Color32::WHITE);
                    for (x, value, direction) in [(39., measurements.rx_mbps, 1.), (123., measurements.tx_mbps, -1.)] {
                        let center = egui::pos2(rect.left()+x, rect.bottom()-8.);
                        let stroke = egui::Stroke::new(1.2, MUTED);
                        ui.painter().line_segment([center+egui::vec2(0.,-3.*direction),center+egui::vec2(0.,3.*direction)],stroke);
                        ui.painter().add(egui::Shape::line(vec![center+egui::vec2(-3.,0.),center+egui::vec2(0.,3.*direction),center+egui::vec2(3.,0.)],stroke));
                        ui.painter().text(center+egui::vec2(9.,0.), egui::Align2::LEFT_CENTER,
                            speed(value), egui::FontId::proportional(11.), MUTED);
                    }
                    let button = button.on_hover_text(format!("{}\nClick for connection details\nReceive video / Send video", badge.label()));
                    egui::Popup::menu(&button).show(|ui| {
                        ui.set_min_width(290.);
                        ui.strong("Connection details");
                        let name = devices.iter().find(|d| Some(d.id) == self.selected)
                            .map_or("Remote desktop", |d| d.name.as_str());
                        ui.label(name);
                        ui.label(badge.label());
                        ui.label(status);
                        ui.separator();
                        ui.label(format!("ICE: {} -> {}", candidate_name(stats.native.local_candidate), candidate_name(stats.native.remote_candidate)));
                        ui.label(format!("Video receive: {}", speed(measurements.rx_mbps)));
                        ui.label(format!("Video transmit: {}", speed(measurements.tx_mbps)));
                        ui.label(measurements.rtt_ms.map_or_else(|| "RTT: not yet available".into(), |n| format!("RTT: {n} ms")));
                        ui.label(measurements.loss_ppm.map_or_else(|| "Packet loss: measuring…".into(), |n| format!("Packet loss: {:.2}%", n as f64 / 10_000.)));
                        let (dimensions, fps) = self.frames.metrics();
                        if let Some((w,h)) = dimensions { ui.label(format!("Video: {w} × {h}")); }
                        if let Some(fps) = fps { ui.label(format!("Received video: {fps:.1} FPS")); }
                        ui.label(format!("Requested limit: {} FPS", self.video_profile.fps));
                        ui.label(format!("Decoded frames skipped locally: {}", stats.dropped_frames));
                        ui.separator();
                        ui.label(egui::RichText::new("Slow: RTT >250 ms or packet loss >5% over 3 reports.").small().color(MUTED));
                        ui.label(egui::RichText::new("Speeds measure video traffic; input and protocol overhead are excluded.").small().color(MUTED));
                    });
                });
            });
        popup_was_open
            || egui::Popup::is_any_open(ctx)
            || ctx
                .pointer_hover_pos()
                .is_some_and(|p| area.response.rect.contains(p))
    }

    /// A foreground overlay: it never reserves any desktop layout space.
    pub(super) fn remote_toolbar(
        &mut self,
        ctx: &egui::Context,
        _devices: &[DeviceSummary],
        ready: bool,
        _status: &str,
        _stats: &PeerStats,
    ) -> bool {
        let bounds = ctx.content_rect();
        let pointer = ctx.pointer_hover_pos();
        let now = ctx.input(|i| i.time);
        let popup_was_open = egui::Popup::is_any_open(ctx);
        let hot_zone = egui::Rect::from_center_size(
            egui::pos2(bounds.center().x, bounds.top() + 8.),
            egui::vec2(240., 16.),
        );
        if pointer.is_some_and(|p| {
            hot_zone.contains(p) || self.toolbar_rect.is_some_and(|r| r.expand(12.).contains(p))
        }) || popup_was_open
            || !ready
        {
            self.toolbar_until = self.toolbar_until.max(now + 0.7);
        }
        if ready && now >= self.toolbar_until {
            self.toolbar_rect = None;
            // A small handle makes the hidden toolbar discoverable.
            ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("toolbar-hint"),
            ))
            .rect_filled(
                egui::Rect::from_center_size(
                    egui::pos2(bounds.center().x, bounds.top() + 3.),
                    egui::vec2(42., 3.),
                ),
                2.,
                MUTED.gamma_multiply(0.55),
            );
            return false;
        }
        let area = egui::Area::new(egui::Id::unique("remote-toolbar"))
            .order(egui::Order::Foreground)
            .fade_in(false)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0., 8.))
            .movable(false)
            .show(ctx, |ui| {
                egui::Frame::new().fill(SURFACE).stroke(egui::Stroke::new(1., BORDER))
                    .corner_radius(12).inner_margin(6).show(ui, |ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(6., 0.);
                    ui.horizontal(|ui| {
                        if icon_button(ui, "remote-back", Icon::Back, "Back to computers — disconnect").clicked() {
                            self.release();
                            self.send(Command::Disconnect);
                        }
                        let before = self.video_profile;
                        let settings = icon_button(ui, "remote-video-settings", Icon::Video, "Video settings — FPS limit and resolution");
                        egui::Popup::menu(&settings)
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                            .show(|ui| {
                        ui.set_min_width(270.);
                        ui.strong("Video settings");
                        ui.separator();
                        egui::ComboBox::from_id_salt("video-fps")
                            .selected_text(format!("Up to {} FPS", self.video_profile.fps))
                            .show_ui(ui, |ui| {
                                for fps in [15, 24, 30, 60] {
                                    ui.selectable_value(&mut self.video_profile.fps, fps, format!("{fps} FPS"));
                                }
                            });
                        let mut size = (self.video_profile.width, self.video_profile.height);
                        egui::ComboBox::from_id_salt("video-size")
                            .selected_text(video_size_name(size))
                            .show_ui(ui, |ui| {
                                for dimensions in [(640, 360), (854, 480), (1280, 720), (1920, 1080), (3840, 2160)] {
                                    ui.selectable_value(&mut size, dimensions, video_size_name(dimensions));
                                }
                            });
                        self.video_profile.width = size.0;
                        self.video_profile.height = size.1;
                        self.video_profile.bitrate_kbps = match size.1 {
                            0..=480 => 3000,
                            481..=720 => 6000,
                            721..=1080 => 12000,
                            _ => 20000,
                        };
                        ui.add_space(6.);
                        ui.label(egui::RichText::new("Applies during this session. Keeps desktop aspect ratio.").small().color(MUTED));
                        ui.label(egui::RichText::new("Lower resolution or FPS reduces processing and bandwidth.").small().color(MUTED));

                        });
                        if self.video_profile != before {
                            self.send(Command::VideoProfile(self.video_profile));
                        }
                        let sync = icon_button(ui, "remote-sync", Icon::Clipboard, "Clipboard, files and SSH — share with the remote computer");
                        egui::Popup::menu(&sync)
                            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                            .show(|ui| {
                                ui.set_min_width(290.);
                                ui.strong("Clipboard and files");
                                ui.separator();
                                let mut on = crate::clipboard::enabled();
                                if ui.checkbox(&mut on, "Share clipboard and files").changed() {
                                    crate::clipboard::set_enabled(on);
                                }
                                ui.add_space(6.);
                                ui.label(egui::RichText::new("Copy text or files here (Cmd+C), paste on Ubuntu (Ctrl+V) \u{2014} and the other way round.").small().color(MUTED));
                                ui.label(egui::RichText::new("Folders and files over 512 MiB are not sent. Items marked secret by password managers are never shared.").small().color(MUTED));
                                if let Some(status) = crate::clipboard::status() {
                                    ui.add_space(6.);
                                    ui.label(egui::RichText::new(status).small());
                                }
                                ui.add_space(8.);
                                ui.separator();
                                ui.strong("SSH tunnel");
                                let mut ssh = crate::clipboard::ssh_wanted();
                                if ui.checkbox(&mut ssh, "Allow SSH to the remote user").changed() {
                                    crate::clipboard::set_ssh_wanted(ssh);
                                }
                                ui.label(egui::RichText::new("Off by default and only while this session is open. Opens a port on this Mac (127.0.0.1) that leads to the remote computer's SSH server; your SSH password or key still protects the account.").small().color(MUTED));
                                if ssh {
                                    let state = crate::clipboard::ssh_state();
                                    if let Some(command) = state.command {
                                        ui.add_space(4.);
                                        ui.add(egui::TextEdit::singleline(&mut command.clone()).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
                                        if ui.button("Copy command").clicked() {
                                            ui.ctx().copy_text(command);
                                        }
                                        if state.connections > 0 {
                                            ui.label(egui::RichText::new(format!("{} connection(s) open", state.connections)).small());
                                        }
                                    } else if state.remote_enabled == Some(false) {
                                        ui.label(egui::RichText::new("Not available: SSH is not enabled on the remote computer, nobody is signed in, or the screen is locked.").small());
                                    } else {
                                        ui.label(egui::RichText::new("Waiting for the remote computer\u{2026}").small().color(MUTED));
                                    }
                                    if let Some(message) = state.message {
                                        ui.label(egui::RichText::new(message).small());
                                    }
                                }
                            });
                        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                        if icon_button(ui, "remote-fullscreen", Icon::Fullscreen,
                            if fullscreen { "Exit fullscreen" } else { "Enter fullscreen" }).clicked() {
                            self.release();
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fullscreen));
                        }
                    });
                });
            });
        self.toolbar_rect = Some(area.response.rect);
        // Suppress the entire close-click frame too, so it cannot reach Ubuntu.
        popup_was_open
            || egui::Popup::is_any_open(ctx)
            || pointer.is_some_and(|p| area.response.rect.contains(p))
    }
}

#[derive(Clone, Copy)]
enum Icon {
    Back,
    Video,
    Clipboard,
    Fullscreen,
}

fn icon_button(
    ui: &mut egui::Ui,
    id: &'static str,
    icon: Icon,
    description: &str,
) -> egui::Response {
    let (_, rect) = ui.allocate_space(egui::vec2(34., 34.));
    let response = ui.interact(rect, egui::Id::unique(id), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, description));
    let visuals = ui.style().interact(&response);
    if response.hovered() || response.is_pointer_button_down_on() || response.has_focus() {
        ui.painter().rect_filled(rect, 7., visuals.bg_fill);
    }
    let painter = ui.painter();
    let stroke = egui::Stroke::new(1.7, visuals.fg_stroke.color);
    let p = |x: f32, y: f32| rect.center() + egui::vec2(x - 8., y - 8.);
    let line = |points: &[(f32, f32)]| {
        painter.add(egui::Shape::line(
            points.iter().map(|&(x, y)| p(x, y)).collect(),
            stroke,
        ));
    };
    match icon {
        Icon::Back => {
            line(&[(9., 2.), (3., 8.), (9., 14.)]);
            line(&[(3., 8.), (15., 8.)]);
        }
        Icon::Video => {
            for (y, x) in [(3., 5.), (8., 11.), (13., 7.)] {
                line(&[(0., y), (16., y)]);
                painter.circle_filled(p(x, y), 2.5, SURFACE);
                painter.circle_stroke(p(x, y), 2.5, stroke);
            }
        }
        Icon::Clipboard => {
            line(&[
                (3., 3.),
                (1., 3.),
                (1., 15.),
                (15., 15.),
                (15., 3.),
                (13., 3.),
            ]);
            line(&[(5., 1.), (11., 1.), (11., 5.), (5., 5.), (5., 1.)]);
            line(&[(4., 9.), (12., 9.)]);
            line(&[(4., 12.), (10., 12.)]);
        }
        Icon::Fullscreen => {
            line(&[(5., 0.), (0., 0.), (0., 5.)]);
            line(&[(11., 0.), (16., 0.), (16., 5.)]);
            line(&[(0., 11.), (0., 16.), (5., 16.)]);
            line(&[(11., 16.), (16., 16.), (16., 11.)]);
        }
    }
    response.on_hover_text(description)
}

fn video_size_name(size: (u32, u32)) -> &'static str {
    match size {
        (640, 360) => "360p",
        (854, 480) => "480p",
        (1280, 720) => "720p",
        (1920, 1080) => "1080p",
        _ => "Source (up to 4K)",
    }
}

fn connection_icon(painter: &egui::Painter, center: egui::Pos2, badge: crate::connection::Badge) {
    use crate::connection::Badge;
    let green = ONLINE;
    let blue = egui::Color32::from_rgb(68, 151, 255);
    let yellow = egui::Color32::from_rgb(246, 205, 77);
    let orange = egui::Color32::from_rgb(244, 154, 63);
    let red = egui::Color32::from_rgb(238, 93, 99);
    let (base, half) = match badge {
        Badge::Offline => (red, None),
        Badge::Pending => (MUTED, None),
        Badge::Direct => (green, None),
        Badge::DirectSlow => (green, Some(yellow)),
        Badge::Relay => (blue, None),
        Badge::RelaySlow => (blue, Some(orange)),
    };
    painter.circle_filled(center, 9., base);
    if let Some(color) = half {
        let mut points = vec![center];
        for n in 0..=16 {
            let angle = std::f32::consts::FRAC_PI_2 + n as f32 * std::f32::consts::PI / 16.;
            points.push(center + egui::vec2(angle.cos(), angle.sin()) * 9.);
        }
        painter.add(egui::Shape::convex_polygon(
            points,
            color,
            egui::Stroke::NONE,
        ));
    }
    let stroke = egui::Stroke::new(1.6, egui::Color32::WHITE);
    if badge == Badge::Offline {
        for sign in [-1., 1.] {
            painter.line_segment(
                [
                    center + egui::vec2(-3., -3. * sign),
                    center + egui::vec2(3., 3. * sign),
                ],
                stroke,
            );
        }
    } else {
        painter.circle_filled(center + egui::vec2(0., -4.), 1., egui::Color32::WHITE);
        painter.line_segment(
            [center + egui::vec2(0., -1.), center + egui::vec2(0., 4.)],
            stroke,
        );
    }
}
