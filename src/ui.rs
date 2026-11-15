//! Desktop window: discover TVs, cast with one click, tune quality, watch live stats.

use crate::{
    config::{Codec, Player, Preset, RateControl, Settings, Visibility},
    discovery::{self, Device},
    engine::Engine,
    platform,
    server::Viewer,
};
use eframe::egui::{self, Color32, RichText};
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{Receiver, Sender, channel},
    },
    thread,
    time::{Duration, Instant},
};

const ACCENT: Color32 = Color32::from_rgb(0x4f, 0x8c, 0xff);
const LIVE: Color32 = Color32::from_rgb(0x3d, 0xd6, 0x8c);
const WARN: Color32 = Color32::from_rgb(0xff, 0xb4, 0x4f);

pub fn run() -> anyhow::Result<()> {
    let engine = Engine::start(Settings::load())?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("LanCast")
            .with_inner_size([480.0, 700.0])
            .with_min_inner_size([400.0, 520.0])
            .with_icon(std::sync::Arc::new(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-256.png")).expect("embedded icon is a valid PNG"),
            )),
        ..Default::default()
    };
    eframe::run_native("LanCast", options, Box::new(|cc| Ok(Box::new(App::new(engine, cc.egui_ctx.clone()))))).map_err(|e| anyhow::anyhow!("{e}"))
}

enum Msg {
    Devices(Vec<Device>),
    Prompt(String),
    Cast(String, Result<(), String>),
    Stopped(String),
    /// Codecs the GPU behind a monitor (by index) can encode.
    Encoders(usize, Vec<Codec>),
}

/// What a click on a TV's card asks for (by IP).
enum Action {
    Cast(String),
    Stop(String),
    /// Forget a TV added by IP.
    Remove(String),
}

/// Smoothed throughput derived from the engine's counters.
#[derive(Default)]
struct Rates {
    at: Option<Instant>,
    frames: u64,
    bytes: u64,
    fps: f64,
    mbps: f64,
}

struct App {
    engine: Arc<Engine>,
    draft: Settings,
    devices: Vec<Device>,
    scanning: bool,
    manual_ip: String,
    /// Per-device transient state: (ip, message, is_error).
    note: Option<(String, String, bool)>,
    busy: Option<String>,
    /// TVs we cast to and haven't stopped.
    casting: BTreeSet<String>,
    monitors: Vec<platform::Monitor>,
    /// Monitor index whose GPU encoders are known (or being checked: `None`).
    encoders: (usize, Option<Vec<Codec>>),
    rates: Rates,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
}

impl App {
    fn new(engine: Arc<Engine>, ctx: egui::Context) -> App {
        let (tx, rx) = channel();
        let mut app = App {
            draft: engine.settings(),
            engine,
            devices: Vec::new(),
            scanning: false,
            manual_ip: String::new(),
            note: None,
            busy: None,
            casting: BTreeSet::new(),
            monitors: platform::monitors(),
            encoders: (usize::MAX, None),
            rates: Rates::default(),
            tx,
            rx,
            ctx,
        };
        app.scan();
        app.check_encoders();
        app
    }

    /// Finds out (in the background) which codecs the selected monitor's GPU can encode.
    fn check_encoders(&mut self) {
        let monitor = self.draft.monitor;
        if self.encoders.0 == monitor {
            return;
        }
        self.encoders = (monitor, None);
        self.background(move |tx| drop(tx.send(Msg::Encoders(monitor, platform::encoders(monitor)))));
    }

    /// Runs `f` on a worker thread and wakes the UI when it reports back.
    fn background(&self, f: impl FnOnce(&Sender<Msg>) + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        thread::spawn(move || {
            f(&tx);
            ctx.request_repaint();
        });
    }

    fn scan(&mut self) {
        self.scanning = true;
        self.background(|tx| {
            let found = discovery::search(Duration::from_secs(2)).unwrap_or_default();
            let _ = tx.send(Msg::Devices(found));
        });
    }

    fn cast(&mut self, ip: String) {
        self.busy = Some(ip.clone());
        self.note = Some((ip.clone(), "Connecting…".into(), false));
        let engine = self.engine.clone();
        let ctx = self.ctx.clone();
        self.background(move |tx| {
            let prompt_tx = tx.clone();
            let r = engine.cast_webos(&ip, || {
                let _ = prompt_tx.send(Msg::Prompt(ip.clone()));
                ctx.request_repaint();
            });
            let _ = tx.send(Msg::Cast(ip, r.map_err(|e| format!("{e:#}"))));
        });
    }

    fn stop(&mut self, ip: String) {
        self.busy = Some(ip.clone());
        let engine = self.engine.clone();
        self.background(move |tx| {
            if let Err(e) = engine.stop_webos(&ip) {
                log::warn!("closing TV browser: {e:#}");
            }
            let _ = tx.send(Msg::Stopped(ip));
        });
    }

    fn handle_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Devices(found) => {
                    self.scanning = false;
                    self.devices = found;
                }
                Msg::Prompt(ip) => self.note = Some((ip, "Select “Accept” on your TV".into(), false)),
                Msg::Cast(ip, Ok(())) => {
                    self.busy = None;
                    self.note = None;
                    self.casting.insert(ip);
                }
                Msg::Cast(ip, Err(e)) => {
                    self.busy = None;
                    self.note = Some((ip, e, true));
                }
                Msg::Stopped(ip) => {
                    self.busy = None;
                    self.casting.remove(&ip);
                }
                Msg::Encoders(monitor, codecs) if monitor == self.encoders.0 => self.encoders.1 = Some(codecs),
                Msg::Encoders(..) => {} // for a monitor no longer selected
            }
        }
    }

    /// Discovered devices plus manually added ones not already found.
    fn all_devices(&self) -> Vec<Device> {
        let mut list = self.devices.clone();
        for ip in &self.draft.manual_devices {
            if !list.iter().any(|d| &d.ip == ip) {
                list.push(Device::manual(ip));
            }
        }
        list
    }

    fn update_rates(&mut self) {
        let s = &self.engine.stats;
        let (frames, bytes) = (s.frames.load(Ordering::Relaxed), s.video_bytes.load(Ordering::Relaxed));
        let now = Instant::now();
        let r = &mut self.rates;
        match r.at {
            Some(at) if now - at < Duration::from_millis(900) => return,
            Some(at) => {
                let dt = (now - at).as_secs_f64();
                r.fps = (frames - r.frames) as f64 / dt;
                r.mbps = (bytes - r.bytes) as f64 * 8.0 / 1e6 / dt;
            }
            None => {}
        }
        (r.at, r.frames, r.bytes) = (Some(now), frames, bytes);
    }
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(title).small().strong().color(ui.visuals().weak_text_color()));
    ui.add_space(4.0);
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::group(ui.style())
        .corner_radius(10)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_messages();
        self.update_rates();
        ui.ctx().request_repaint_after(Duration::from_millis(500));
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.header(ui);
                self.error_ui(ui);
                self.devices_ui(ui);
                self.link_ui(ui);
                self.quality_ui(ui);
                self.stats_ui(ui);
            });
        });
        self.check_encoders(); // the monitor may have changed
        // Apply settings once the user stops dragging (avoids restarting mid-drag). The engine's
        // own state (see `apply_user_settings`) is mirrored so it doesn't count as an edit.
        let current = self.engine.settings();
        self.draft.copy_engine_state(&current);
        if self.draft != current && !ui.input(|i| i.pointer.any_down()) {
            self.engine.apply_user_settings(self.draft.clone());
        }
    }
}

impl App {
    fn header(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("LanCast").size(26.0).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let viewers = self.engine.hub.viewers();
                let (dot, text) = match viewers {
                    0 => (ui.visuals().weak_text_color(), "Idle".to_owned()),
                    1 => (LIVE, "Live · 1 viewer".to_owned()),
                    n => (LIVE, format!("Live · {n} viewers")),
                };
                ui.label(RichText::new(text).color(dot));
                let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.5, dot);
            });
        });
        ui.label(RichText::new("Cast your screen to any TV on your network — no app needed on the TV.").weak());
    }

    /// The pipeline keeps failing (e.g. no encoder for the chosen codec): say so up front.
    fn error_ui(&self, ui: &mut egui::Ui) {
        let Some(error) = self.engine.stats.error() else { return };
        ui.add_space(10.0);
        card(ui, |ui| {
            ui.label(RichText::new("Can't stream").strong().color(WARN));
            ui.label(RichText::new(error).small());
        });
    }

    fn devices_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            section(ui, "TVS");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Max), |ui| {
                if self.scanning {
                    ui.spinner();
                } else if ui.button("Scan").clicked() {
                    self.scan();
                }
            });
        });
        let devices = self.all_devices();
        if devices.is_empty() {
            card(ui, |ui| {
                ui.label(if self.scanning { "Searching your network…" } else { "No TVs found." });
                ui.label(RichText::new("Make sure the TV is on and connected to the same network, or add it by IP below.").weak().small());
            });
        }
        let mut action = None;
        for d in &devices {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&d.name).strong());
                        let kinds = [d.webos.then_some("webOS"), d.av_transport.as_ref().map(|_| "DLNA")];
                        let kinds: Vec<_> = kinds.into_iter().flatten().collect();
                        ui.label(RichText::new(format!("{}  ·  {}", d.ip, kinds.join(" · "))).weak().small());
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let busy = self.busy.as_deref() == Some(d.ip.as_str());
                        let live = self.casting.contains(&d.ip);
                        if busy {
                            ui.spinner();
                        } else if live {
                            if ui.button("Stop").clicked() {
                                action = Some(Action::Stop(d.ip.clone()));
                            }
                        } else if d.webos {
                            let cast =
                                egui::Button::new(RichText::new("Cast").strong().color(Color32::WHITE)).fill(ACCENT).min_size(egui::vec2(72.0, 28.0));
                            if ui.add_enabled(self.busy.is_none(), cast).clicked() {
                                action = Some(Action::Cast(d.ip.clone()));
                            }
                        } else {
                            ui.label(RichText::new("Open the link below on the TV").weak().small());
                        }
                        if !busy && !live && self.draft.manual_devices.contains(&d.ip) && ui.small_button("Remove").clicked() {
                            action = Some(Action::Remove(d.ip.clone()));
                        }
                    });
                });
                if let Some((ip, text, err)) = &self.note
                    && ip == &d.ip
                {
                    ui.label(RichText::new(text).small().color(if *err { WARN } else { ui.visuals().text_color() }));
                }
            });
        }
        match action {
            Some(Action::Cast(ip)) => self.cast(ip),
            Some(Action::Stop(ip)) => self.stop(ip),
            Some(Action::Remove(ip)) => self.draft.manual_devices.retain(|d| d != &ip),
            None => {}
        }
        ui.horizontal(|ui| {
            let edit = egui::TextEdit::singleline(&mut self.manual_ip).hint_text("Add TV by IP, e.g. 192.168.1.50").desired_width(240.0);
            let r = ui.add(edit);
            let valid = self.manual_ip.trim().parse::<std::net::Ipv4Addr>().is_ok();
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.add_enabled(valid, egui::Button::new("Add")).clicked() || (enter && valid))
                && !self.draft.manual_devices.contains(&self.manual_ip.trim().to_owned())
            {
                self.draft.manual_devices.push(self.manual_ip.trim().to_owned());
                self.manual_ip.clear();
            }
        });
    }

    fn link_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "LINK");
        // As reachable from a TV being cast to (it picks the network interface), if any.
        let url = self.engine.url_for(self.casting.first().map(String::as_str));
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&url).monospace().color(ACCENT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Open").clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&url));
                    }
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(url.clone());
                    }
                });
            });
            ui.horizontal(|ui| {
                ui.label("Who can watch");
                let s = &mut self.draft;
                egui::ComboBox::from_id_salt("visibility").selected_text(s.visibility.label()).show_ui(ui, |ui| {
                    for v in Visibility::ALL {
                        ui.selectable_value(&mut s.visibility, v, v.label()).on_hover_text(match v {
                            Visibility::Network => "Any device on your network can open the link",
                            Visibility::CastTargets => "TVs you've pressed Cast for (and this PC)",
                            Visibility::Code => "The link carries a secret code; casting adds it automatically",
                            Visibility::ThisPc => "Only browsers on this PC",
                        });
                    }
                });
                match s.visibility {
                    Visibility::CastTargets if s.cast_targets.is_empty() => {
                        ui.label(RichText::new("press Cast once per TV").weak().small());
                    }
                    Visibility::CastTargets => {
                        let tvs = s.cast_targets.iter().map(String::as_str).collect::<Vec<_>>().join(", ");
                        if ui.small_button("Forget TVs").on_hover_text(format!("Allowed now: {tvs}")).clicked() {
                            self.engine.forget_cast_targets();
                        }
                    }
                    Visibility::Code => {
                        if ui.small_button("New code").on_hover_text("Links shared before stop working").clicked() {
                            self.engine.new_access_code();
                        }
                    }
                    Visibility::Network | Visibility::ThisPc => {}
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new("Works in any modern browser: TVs, phones, laptops.").weak().small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("Latency check")
                        .on_hover_text("Opens a big PC clock; turn on \"Stats on TV\" and compare it with the clock on the TV")
                        .clicked()
                    {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(format!("http://127.0.0.1:{}/clock", self.engine.port)));
                    }
                });
            });
        });
    }

    fn quality_ui(&mut self, ui: &mut egui::Ui) {
        section(ui, "QUALITY");
        let s = &mut self.draft;
        ui.horizontal(|ui| {
            for p in Preset::ALL {
                if ui.selectable_label(s.preset == Some(p), p.label()).clicked() {
                    p.apply(s);
                }
            }
            if s.preset.is_none() {
                ui.label(RichText::new("Custom").weak());
            }
        });
        let before = s.clone();
        egui::CollapsingHeader::new("Advanced").show(ui, |ui| {
            egui::Grid::new("advanced").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Monitor");
                let current = self.monitors.get(s.monitor).map_or("Primary".into(), |m| format!("{} ({}×{})", m.name, m.width, m.height));
                egui::ComboBox::from_id_salt("monitor").selected_text(current).show_ui(ui, |ui| {
                    for (i, m) in self.monitors.iter().enumerate() {
                        let star = if m.primary { " ★" } else { "" };
                        ui.selectable_value(&mut s.monitor, i, format!("{} ({}×{}){star}", m.name, m.width, m.height));
                    }
                });
                ui.end_row();

                ui.label("Resolution");
                let label = |h: u32| match h {
                    0 => "Native".to_owned(),
                    2160 => "2160p (4K)".to_owned(),
                    h => format!("{h}p"),
                };
                egui::ComboBox::from_id_salt("res").selected_text(label(s.height)).show_ui(ui, |ui| {
                    for h in [0, 2160, 1440, 1080, 720] {
                        ui.selectable_value(&mut s.height, h, label(h));
                    }
                })
                .response
                .on_hover_text("Output resolution; scales your screen up or down on the GPU.\nUpscaling (e.g. 1440p → 4K) sends a pixel-exact picture for a 4K TV,\nbut can't add detail your desktop doesn't have.");
                ui.end_row();

                ui.label("Frame rate");
                ui.horizontal(|ui| {
                    for fps in [30, 60, 120] {
                        ui.selectable_value(&mut s.fps, fps, format!("{fps}"));
                    }
                });
                ui.end_row();

                ui.label("Codec");
                ui.horizontal(|ui| {
                    for codec in Codec::ALL {
                        let hint = match codec {
                            Codec::H264 => "Plays everywhere",
                            Codec::Hevc => "Sharper than H.264 at the same bitrate.
Needs a TV that decodes HEVC (most 4K TVs do).",
                            Codec::Av1 => "Sharpest at the same bitrate.
Needs an AV1 encoder on the PC (RTX 40+, Radeon RX 7000+, Intel Arc)
and an AV1-capable TV (e.g. LG 2022+).",
                        };
                        // Unknown (still checking, or the check failed): leave it to the pipeline.
                        let encodable = self.encoders.1.as_ref().is_none_or(|list| list.is_empty() || list.contains(&codec));
                        ui.add_enabled_ui(encodable, |ui| ui.selectable_value(&mut s.codec, codec, codec.label()))
                            .inner
                            .on_hover_text(hint)
                            .on_disabled_hover_text(format!("This PC's graphics can't encode {} in hardware.", codec.label()));
                    }
                });
                ui.end_row();

                ui.label("Bitrate");
                ui.horizontal(|ui| {
                    let mut cbr = matches!(s.rate, RateControl::Cbr { .. });
                    if ui.selectable_value(&mut cbr, true, "Constant").changed() || ui.selectable_value(&mut cbr, false, "Quality").changed() {
                        s.rate = if cbr { RateControl::Cbr { kbps: 40_000 } } else { RateControl::Quality { level: 85 } };
                    }
                });
                ui.end_row();

                ui.label("");
                match &mut s.rate {
                    RateControl::Cbr { kbps } => {
                        let mut mbps = *kbps / 1000;
                        ui.add(egui::Slider::new(&mut mbps, 2..=150).suffix(" Mbps"));
                        *kbps = mbps * 1000;
                    }
                    RateControl::Quality { level } => {
                        ui.add(egui::Slider::new(level, 30..=100).text("quality"));
                    }
                }
                ui.end_row();

                ui.label("TV player");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut s.player, Player::Auto, "Auto");
                    ui.selectable_value(&mut s.player, Player::WebCodecs, "WebCodecs").on_hover_text("Lowest latency; needs HTTPS on TVs");
                    ui.selectable_value(&mut s.player, Player::WebRtc, "WebRTC").on_hover_text("Low latency on plain HTTP");
                    ui.selectable_value(&mut s.player, Player::Mse, "MSE").on_hover_text("Most compatible; TVs buffer ~0.5-1 s");
                });
                ui.end_row();

                ui.label("Options");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut s.audio, "Audio");
                    ui.checkbox(&mut s.cursor, "Show cursor");
                    ui.checkbox(&mut s.tv_app, "Use TV app")
                        .on_hover_text("Cast to the LanCast webOS app when it's installed on the TV, instead of the TV's browser");
                    ui.checkbox(&mut s.tv_stats, "Stats on TV")
                        .on_hover_text("Shows stats and the live PC clock on the TV.\nOpen the latency check (link below) on the PC and compare the two clocks.");
                    ui.checkbox(&mut s.https, "HTTPS")
                        .on_hover_text("Enables the low-latency WebCodecs player in browsers that accept a self-signed certificate\n(choose Advanced → Proceed on the warning). Doesn't work on LG webOS 25 TVs.");
                });
                ui.end_row();
            });
        });
        // Any manual tweak of a preset-controlled value turns the preset into "Custom".
        let preset_fields = |s: &Settings| (s.height, s.fps, s.rate);
        if preset_fields(s) != preset_fields(&before) {
            s.preset = None;
        }
    }

    fn stats_ui(&mut self, ui: &mut egui::Ui) {
        let st = &self.engine.stats;
        if self.engine.hub.viewers() == 0 {
            return;
        }
        section(ui, "LIVE");
        card(ui, |ui| {
            let ms = |v: &std::sync::atomic::AtomicU64| v.load(Ordering::Relaxed) as f64 / 1e3;
            egui::Grid::new("stats").num_columns(4).spacing([18.0, 4.0]).show(ui, |ui| {
                for (value, label) in [
                    (format!("{:.0}", self.rates.fps), "fps"),
                    (format!("{:.1}", self.rates.mbps), "Mbps"),
                    (format!("{:.1}", ms(&st.encode_us)), "ms encode"),
                    (format!("{:.0}", st.latency_us() as f64 / 1e3), "ms to screen"),
                ] {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(value).size(20.0).strong());
                        ui.label(RichText::new(label).weak().small());
                    });
                }
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new(st.status.lock().unwrap().as_str()).weak().small());
            ui.add_space(8.0);
            ui.label(RichText::new("Watching now").small().strong());
            egui::Grid::new("viewers").num_columns(3).spacing([18.0, 2.0]).show(ui, |ui| {
                for v in st.viewers() {
                    ui.label(self.viewer_name(&v));
                    ui.label(RichText::new(&v.player).weak());
                    ui.label(if v.latency_us > 0 { format!("{:.0} ms", v.latency_us as f64 / 1e3) } else { "…".to_owned() });
                    ui.end_row();
                }
            });
        });
    }

    /// A viewer's address, with the TV's name when it's one we know.
    fn viewer_name(&self, v: &Viewer) -> String {
        if v.this_pc {
            return "This PC".to_owned();
        }
        let ip = v.ip.to_string();
        match self.devices.iter().find(|d| d.ip == ip) {
            Some(d) => format!("{} ({ip})", d.name),
            None => ip,
        }
    }
}
