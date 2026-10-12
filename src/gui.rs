// SPDX-License-Identifier: GPL-3.0-or-later
//! Native desktop camera console. All camera I/O runs in the serialized worker.

use anyhow::{Context, Result};
use eframe::egui::{self, Color32, FontId, RichText, Stroke, TextureHandle, Vec2};
use std::{
    collections::BTreeMap,
    net::Ipv4Addr,
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use crate::{
    camera::{
        CameraState, CameraVersion, Crop, HslState, ParamSpec, param_specs,
        unsupported_control_reason,
    },
    firmware::FirmwareRelease,
    focus::{self, FocusTarget},
    preview::{self, Preview, PreviewCapabilities, PreviewFrame, PreviewMode},
    settings::{Preset, Settings, Store},
    worker::{Command, Event, PreparedSummary, Worker},
};

const BACKGROUND: Color32 = Color32::from_rgb(19, 21, 23);
const PANEL: Color32 = Color32::from_rgb(27, 30, 33);
const FIELD: Color32 = Color32::from_rgb(38, 42, 46);
const LINE: Color32 = Color32::from_rgb(54, 59, 63);
const TEXT: Color32 = Color32::from_rgb(233, 235, 233);
const MUTED: Color32 = Color32::from_rgb(154, 164, 168);
const AMBER: Color32 = Color32::from_rgb(239, 180, 76);
const ERROR: Color32 = Color32::from_rgb(249, 125, 111);
const GREEN: Color32 = Color32::from_rgb(133, 193, 155);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Camera,
    Colour,
    Firmware,
    Presets,
    Device,
}

impl Tab {
    fn key(self) -> &'static str {
        match self {
            Self::Camera => "camera",
            Self::Colour => "colour",
            Self::Firmware => "firmware",
            Self::Presets => "presets",
            Self::Device => "device",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Camera => "Camera",
            Self::Colour => "Colour",
            Self::Firmware => "Firmware",
            Self::Presets => "Presets",
            Self::Device => "Device",
        }
    }
    fn from_key(key: &str) -> Self {
        match key {
            "colour" => Self::Colour,
            "firmware" => Self::Firmware,
            "presets" => Self::Presets,
            "device" => Self::Device,
            _ => Self::Camera,
        }
    }
}

pub fn run(ip: Option<Ipv4Addr>, port: u16, preview: bool) -> Result<()> {
    launch(ip, port, preview, false)
}

pub fn run_demo() -> Result<()> {
    launch(None, 12345, true, true)
}

fn launch(ip: Option<Ipv4Addr>, port: u16, preview: bool, demo: bool) -> Result<()> {
    let store = Store::new()?;
    let settings = if demo {
        Settings::default()
    } else {
        store.load_settings()?
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 800.0])
            .with_min_inner_size([940.0, 620.0])
            .with_app_id("yolocam-linux"),
        ..Default::default()
    };
    eframe::run_native(
        "YoloCam Linux",
        options,
        Box::new(move |cc| {
            configure_theme(&cc.egui_ctx);
            Ok(Box::new(App::new(store, settings, ip, port, preview, demo)))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Cannot open the desktop window: {error}"))
}

struct App {
    store: Store,
    settings: Settings,
    tab: Tab,
    worker: Option<Worker>,
    state: CameraState,
    connected: bool,
    demo: bool,
    busy: bool,
    status: String,
    status_error: bool,
    control_connection_error: Option<String>,
    desired: BTreeMap<String, i32>,
    write_only_choices: BTreeMap<String, i32>,
    desired_hsl: BTreeMap<(usize, usize), i32>,
    desired_crop: Option<Crop>,
    desired_wb: Option<(u32, u32)>,
    preview: Preview,
    preview_attempted: bool,
    preview_capabilities: Option<PreviewCapabilities>,
    preview_probe: Option<mpsc::Receiver<std::result::Result<PreviewCapabilities, String>>>,
    preview_probe_started: Option<Instant>,
    preview_probe_attempted: bool,
    preview_error: Option<String>,
    texture: Option<TextureHandle>,
    frame_sequence: u64,
    frame_dimensions: (usize, usize),
    frame_fps: f32,
    frame_captured_at: Option<Instant>,
    focus_draft: Option<FocusTarget>,
    focus_drag_armed: bool,
    focus_pending: bool,
    crop_slot: u8,
    release: Option<FirmwareRelease>,
    prepared: Option<PreparedSummary>,
    firmware_phase: String,
    firmware_percent: u8,
    installing: bool,
    uncertain_update: bool,
    confirmation_open: bool,
    keep_usb_checked: bool,
    restore_preview_after_update: bool,
    preset_name: String,
    presets: Vec<Preset>,
    selected_preset: Option<String>,
    import_path: String,
    export_path: String,
    clut_path: String,
    profile_id: u32,
    profile_scene: u32,
    diagnostics: Option<String>,
    demo_screenshot: Option<PathBuf>,
    demo_frames: u32,
}

impl App {
    fn new(
        store: Store,
        mut settings: Settings,
        ip: Option<Ipv4Addr>,
        port: u16,
        preview: bool,
        demo: bool,
    ) -> Self {
        if preview {
            settings.preview_enabled = true;
        }
        let presets_result = if demo {
            let daylight = Preset::capture("Daylight", &demo_state());
            let mut warm_state = demo_state();
            warm_state.values.insert("contrast".to_owned(), 48);
            warm_state.values.insert("wb-mode".to_owned(), 0);
            warm_state.values.insert("wb-temp".to_owned(), 4200);
            daylight
                .and_then(|daylight| Ok(vec![daylight, Preset::capture("Warm room", &warm_state)?]))
        } else {
            store.presets()
        };
        let status = match &presets_result {
            Ok(_) if demo => "Demo mode. No camera connection or firmware operations.".to_owned(),
            Ok(_) => "Connecting to your camera…".to_owned(),
            Err(error) => format!("Cannot load local presets: {error:#}"),
        };
        let status_error = presets_result.is_err();
        Self {
            tab: if demo {
                Tab::from_key(
                    &std::env::var("YOLOCAM_DEMO_TAB").unwrap_or_else(|_| "camera".to_owned()),
                )
            } else {
                Tab::from_key(&settings.active_tab)
            },
            store,
            settings,
            worker: (!demo).then(|| Worker::new(ip, port)),
            state: if demo {
                demo_state()
            } else {
                CameraState::default()
            },
            connected: demo,
            demo,
            busy: false,
            status,
            status_error,
            control_connection_error: None,
            desired: BTreeMap::new(),
            write_only_choices: BTreeMap::new(),
            desired_hsl: BTreeMap::new(),
            desired_crop: None,
            desired_wb: None,
            preview: Preview::new(),
            preview_attempted: false,
            preview_capabilities: demo.then(demo_preview_capabilities),
            preview_probe: None,
            preview_probe_started: None,
            preview_probe_attempted: demo,
            preview_error: None,
            texture: None,
            frame_sequence: 0,
            frame_dimensions: (0, 0),
            frame_fps: 0.0,
            frame_captured_at: None,
            focus_draft: None,
            focus_drag_armed: false,
            focus_pending: false,
            crop_slot: 0,
            release: None,
            prepared: None,
            firmware_phase: String::new(),
            firmware_percent: 0,
            installing: false,
            uncertain_update: false,
            confirmation_open: false,
            keep_usb_checked: false,
            restore_preview_after_update: false,
            preset_name: String::new(),
            presets: presets_result.unwrap_or_default(),
            selected_preset: None,
            import_path: String::new(),
            export_path: String::new(),
            clut_path: String::new(),
            profile_id: 0,
            profile_scene: 0,
            diagnostics: None,
            demo_screenshot: if demo {
                std::env::var_os("YOLOCAM_DEMO_SCREENSHOT").map(PathBuf::from)
            } else {
                None
            },
            demo_frames: 0,
        }
    }

    fn error(&mut self, error: impl std::fmt::Display) {
        self.status = error.to_string();
        self.status_error = true;
    }

    fn capture_demo(&mut self, ctx: &egui::Context) {
        if !self.demo || self.demo_screenshot.is_none() {
            return;
        }
        let screenshot = ctx.input(|input| {
            input.events.iter().find_map(|event| {
                if let egui::Event::Screenshot { image, .. } = event {
                    Some(image.clone())
                } else {
                    None
                }
            })
        });
        if let Some(image) = screenshot {
            let path = self.demo_screenshot.take().expect("demo screenshot path");
            let result = (|| -> Result<()> {
                if let Some(parent) = path
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    std::fs::create_dir_all(parent)?;
                }
                let pixels: Vec<u8> = image
                    .pixels
                    .iter()
                    .flat_map(|colour| colour.to_array())
                    .collect();
                let png =
                    image::RgbaImage::from_raw(image.width() as u32, image.height() as u32, pixels)
                        .context("Invalid screenshot dimensions")?;
                png.save(&path)?;
                Ok(())
            })();
            if let Err(error) = result {
                self.error(format!("Cannot save demo screenshot: {error:#}"));
            } else {
                log::info!("Demo screenshot saved to {}", path.display());
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        } else {
            self.demo_frames += 1;
            if self.demo_frames == 3 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            }
        }
    }

    fn save_preferences(&mut self) {
        if self.demo {
            return;
        }
        self.settings.active_tab = self.tab.key().to_owned();
        if let Err(error) = self.store.save_settings(&self.settings) {
            self.error(format!("{error:#}"));
        }
    }

    fn send(&mut self, command: Command) -> bool {
        let reason = match &command {
            Command::SetParam { key, value } if key == "hdr" || key == "frame-rate" => {
                self.requested_control_reason(key, *value)
            }
            Command::ApplyPreset(preset) => self.preset_reason(preset),
            _ => None,
        };
        if let Some(reason) = reason {
            self.error(reason);
            return false;
        }
        if self.demo {
            match command {
                Command::SetParam { key, value } => {
                    self.state.values.insert(key, value);
                }
                Command::SetCrop(crop) => self.state.eptz = Some(crop),
                Command::SetFocusTarget(target) => self.state.focus_target = Some(target),
                Command::SetHslField {
                    component,
                    colour,
                    value,
                } => {
                    if let Some(hsl) = &mut self.state.hsl {
                        hsl.values[component][colour] = value;
                    }
                }
                Command::ResetHsl => self.state.hsl = Some(HslState::default()),
                Command::SetWbOffsets(red, blue) => self.state.wb_offsets = Some((red, blue)),
                Command::SelectProfile { id, scene } => {
                    self.state.profile = Some(crate::pb::TuningParam {
                        id,
                        scene_id: scene,
                        hdr_flag: 0,
                    })
                }
                Command::SaveCrop(slot) => {
                    if let Some(crop) = &self.state.eptz {
                        self.state.presets.insert(slot, crop.clone());
                    }
                }
                Command::RecallCrop(slot) => {
                    self.state.eptz = self.state.presets.get(&slot).cloned()
                }
                _ => {}
            }
            self.status = "Demo control changed. No settings were sent to a camera.".to_owned();
            self.status_error = false;
            return true;
        }
        let clear_all = matches!(
            command,
            Command::ApplyPreset(_) | Command::Refresh | Command::Connect
        );
        let clear_crop = clear_all || matches!(command, Command::RecallCrop(_));
        let clear_hsl = clear_all || matches!(command, Command::ResetHsl);
        let pending_preset = if let Command::ApplyPreset(preset) = &command {
            Some((
                preset
                    .values
                    .iter()
                    .filter(|(key, _)| ["hdr", "focus-mode", "focus-level"].contains(&key.as_str()))
                    .map(|(key, value)| (key.clone(), *value))
                    .collect::<BTreeMap<_, _>>(),
                preset.crop.clone(),
            ))
        } else {
            None
        };
        let result = self
            .worker
            .as_ref()
            .context("Camera worker is unavailable")
            .and_then(|worker| worker.send(command));
        match result {
            Ok(()) => {
                if clear_all {
                    self.desired.clear();
                    self.desired_wb = None;
                }
                if clear_crop {
                    self.desired_crop = None;
                }
                if clear_hsl {
                    self.desired_hsl.clear();
                }
                if let Some((values, crop)) = pending_preset {
                    self.desired.extend(values);
                    self.desired_crop = crop;
                }
                self.status = "Sending control to camera…".to_owned();
                self.status_error = false;
                true
            }
            Err(error) => {
                self.error(format!("{error:#}"));
                false
            }
        }
    }

    fn poll_worker(&mut self) {
        let events = self.worker.as_ref().map(Worker::poll).unwrap_or_default();
        for event in events {
            self.handle_worker_event(event);
        }
    }

    fn handle_worker_event(&mut self, event: Event) {
        match event {
            Event::Connection(connected) => {
                self.connected = connected;
                if connected {
                    self.control_connection_error = None;
                } else {
                    self.state.focus_target = None;
                    self.focus_pending = false;
                    self.focus_draft = None;
                    self.focus_drag_armed = false;
                }
            }
            Event::ConnectionError(message) => {
                self.control_connection_error = Some(message);
            }
            Event::State(mut state) => {
                if !self.connected {
                    state.focus_target = None;
                }
                self.desired
                    .retain(|key, value| state.values.get(key) != Some(value));
                self.desired_hsl.retain(|(axis, colour), value| {
                    state
                        .hsl
                        .as_ref()
                        .is_none_or(|hsl| hsl.values[*axis][*colour] != *value)
                });
                if self
                    .desired_crop
                    .as_ref()
                    .zip(state.eptz.as_ref())
                    .is_some_and(|(want, actual)| crop_matches(want, actual))
                {
                    self.desired_crop = None;
                }
                if self.desired_wb == state.wb_offsets {
                    self.desired_wb = None;
                }
                self.state = *state;
            }
            Event::Status { message, error } => {
                self.status = message;
                self.status_error = error;
                if error {
                    self.desired.clear();
                    self.desired_hsl.clear();
                    self.desired_crop = None;
                    self.desired_wb = None;
                    self.focus_pending = false;
                    self.focus_draft = None;
                    self.focus_drag_armed = false;
                }
            }
            Event::FocusTargetApplied(target) => {
                self.state.focus_target = Some(target);
                self.focus_pending = false;
                self.focus_draft = None;
            }
            Event::Busy(busy) => self.busy = busy,
            Event::FirmwareRelease(release) => {
                self.release = Some(*release);
                self.prepared = None;
                self.firmware_phase.clear();
            }
            Event::FirmwarePrepared(prepared) => {
                self.prepared = Some(*prepared);
                self.firmware_phase = "prepared".to_owned();
                self.firmware_percent = 100;
            }
            Event::FirmwareProgress { phase, percent } => {
                self.firmware_phase = phase;
                self.firmware_percent = percent;
            }
            Event::FirmwareFinished(version) => {
                self.status = format!(
                    "Firmware {} / build {} verified after reboot",
                    version.firmware, version.build
                );
                self.status_error = false;
                self.state.version = Some(version);
                self.installing = false;
                self.uncertain_update = false;
                self.prepared = None;
                self.firmware_phase = "verified".to_owned();
                self.firmware_percent = 100;
                self.preview_capabilities = None;
                self.preview_probe_attempted = false;
                self.preview_error = None;
                if self.restore_preview_after_update {
                    self.preview_attempted = false;
                }
                self.restore_preview_after_update = false;
            }
            Event::FirmwareFailed { message, uncertain } => {
                self.installing = false;
                self.uncertain_update = uncertain;
                self.error(message);
                if !uncertain && self.restore_preview_after_update {
                    self.preview_attempted = false;
                    self.restore_preview_after_update = false;
                }
            }
            Event::PresetCaptured(preset) => match self.store.save_preset(&preset) {
                Ok(()) => {
                    self.selected_preset = Some(preset.name.clone());
                    self.status = preset_status(
                        &format!("Saved preset {} from camera readbacks", preset.name),
                        &preset,
                    );
                    self.status_error = false;
                    self.reload_presets();
                }
                Err(error) => self.error(format!("{error:#}")),
            },
        }
    }

    fn reload_presets(&mut self) {
        match self.store.presets() {
            Ok(presets) => self.presets = presets,
            Err(error) => self.error(format!("{error:#}")),
        }
    }

    fn toggle_preview(&mut self) {
        if self.busy || self.installing || self.uncertain_update {
            return;
        }
        self.settings.preview_enabled = !self.settings.preview_enabled;
        self.preview_attempted = false;
        if !self.settings.preview_enabled {
            self.preview.stop();
            self.texture = None;
            self.frame_captured_at = None;
            self.focus_draft = None;
            self.focus_drag_armed = false;
        }
        self.save_preferences();
    }

    fn requested_control_reason(&self, key: &str, value: i32) -> Option<String> {
        crate::compatibility::control_reason(&self.state, key, value).or_else(|| {
            (key == "hdr"
                && value == 1
                && crate::compatibility::exceeds_hdr_limit(self.settings.preview_mode.fps()))
            .then(|| "Select a preview mode at 30 fps or lower before enabling HDR".to_owned())
        })
    }

    fn preset_reason(&self, preset: &Preset) -> Option<String> {
        match crate::compatibility::preset_plan(&self.state, preset) {
            Err(error) => Some(error.to_string()),
            Ok(plan) => (plan.target_hdr == Some(1)
                && crate::compatibility::exceeds_hdr_limit(self.settings.preview_mode.fps()))
            .then(|| {
                "Select a preview mode at 30 fps or lower before applying an HDR preset".to_owned()
            }),
        }
    }

    fn refresh_preview_modes(&mut self) {
        if self.demo
            || self.busy
            || self.installing
            || self.uncertain_update
            || self.preview_probe.is_some()
        {
            return;
        }
        self.preview_capabilities = None;
        self.preview_probe_attempted = true;
        self.preview_error = None;
        self.preview_probe_started = Some(Instant::now());
        let device = self.settings.preview_device.clone();
        let (sender, receiver) = mpsc::channel();
        match thread::Builder::new()
            .name("video-modes".to_owned())
            .spawn(move || {
                let result = preview::enumerate(device).map_err(|error| format!("{error:#}"));
                let _ = sender.send(result);
            }) {
            Ok(_) => self.preview_probe = Some(receiver),
            Err(error) => self.preview_error = Some(format!("Cannot inspect video modes: {error}")),
        }
    }

    fn select_preview_mode(&mut self, mode: PreviewMode) {
        let reason = self.preview_choice_reason(self.preview_capabilities.as_ref(), mode);
        if let Some(reason) = reason {
            self.preview_error = Some(reason);
            return;
        }
        if self.settings.preview_mode != mode {
            self.preview.stop();
            self.texture = None;
            self.frame_sequence = 0;
            self.frame_captured_at = None;
            self.focus_draft = None;
            self.focus_drag_armed = false;
            self.settings.preview_mode = mode;
            self.preview_attempted = false;
            self.preview_error = None;
            self.save_preferences();
        }
    }

    fn preview_choice_reason(
        &self,
        capabilities: Option<&PreviewCapabilities>,
        mode: PreviewMode,
    ) -> Option<String> {
        preview_mode_reason(&self.state, capabilities, mode).or_else(|| {
            (crate::compatibility::exceeds_hdr_limit(mode.fps())
                && self.desired.get("hdr") == Some(&1))
            .then(|| {
                "Wait for the pending HDR change; preview above 30 fps requires confirmed SDR"
                    .to_owned()
            })
        })
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        if self.demo || self.installing || self.uncertain_update {
            return;
        }
        if !self.preview_probe_attempted && !self.busy {
            self.refresh_preview_modes();
        }
        if let Some(receiver) = &self.preview_probe {
            let result = match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(
                    "Video mode inspection stopped unexpectedly. Refresh video modes to retry."
                        .to_owned(),
                )),
                Err(mpsc::TryRecvError::Empty)
                    if self
                        .preview_probe_started
                        .is_some_and(|started| started.elapsed() > Duration::from_secs(5)) =>
                {
                    self.preview_error = Some("Video mode inspection timed out. Check USB; waiting for the device to release its inspection session.".to_owned());
                    None
                }
                Err(mpsc::TryRecvError::Empty) => None,
            };
            if let Some(result) = result {
                self.preview_probe = None;
                self.preview_probe_started = None;
                match result {
                    Ok(capabilities) => {
                        self.preview_capabilities = Some(capabilities);
                        self.preview_attempted = false;
                    }
                    Err(reason) => self.preview_error = Some(reason),
                }
            }
        }
        let mode_reason = self.preview_choice_reason(
            self.preview_capabilities.as_ref(),
            self.settings.preview_mode,
        );
        if self.preview.is_running() && mode_reason.is_some() {
            self.preview.stop();
            self.texture = None;
            self.frame_captured_at = None;
            self.preview_error = None;
            self.preview_attempted = false;
        }
        if self.settings.preview_enabled
            && self.connected
            && !self.busy
            && self.preview_capabilities.is_some()
            && mode_reason.is_none()
            && !self.preview_attempted
        {
            self.preview_attempted = true;
            let device = self
                .preview_capabilities
                .as_ref()
                .map(|capabilities| capabilities.device.clone());
            let result = self.preview.start_mode(device, self.settings.preview_mode);
            self.handle_preview_start(result);
        }
        if let Some(error) = self.preview.error() {
            if self.preview.is_running() {
                self.preview.stop();
            }
            self.preview_error = Some(format!("Preview: {error}"));
            self.frame_captured_at = None;
            self.focus_draft = None;
            self.focus_drag_armed = false;
        }
        if self.preview.is_running()
            && let Some(frame) = self.preview.latest_since(self.frame_sequence)
            && frame.sequence != self.frame_sequence
        {
            self.display_preview_frame(ctx, frame);
        }
    }

    fn handle_preview_start(&mut self, result: Result<()>) {
        self.preview_attempted = true;
        match result {
            Ok(()) => {
                // Starting UVC capture can change CID5 inside the camera.
                // Read its actual FPS/HDR/focus once after negotiation. Direct
                // enqueueing preserves pending edits that send(Refresh) clears.
                let refresh = self
                    .worker
                    .as_ref()
                    .context("Camera worker is unavailable")
                    .and_then(|worker| worker.send(Command::Refresh));
                self.preview_error = refresh.err().map(|error| {
                    format!(
                        "Preview started, but camera readbacks could not be refreshed: {error:#}"
                    )
                });
            }
            Err(error) => self.preview_error = Some(format!("Preview: {error:#}")),
        }
    }

    fn display_preview_frame(&mut self, ctx: &egui::Context, frame: PreviewFrame) {
        self.frame_sequence = frame.sequence;
        self.frame_captured_at = Some(frame.captured_at);
        self.frame_dimensions = (frame.width, frame.height);
        self.frame_fps = frame.fps;
        let image = egui::ColorImage::from_rgb([frame.width, frame.height], &frame.rgb);
        if let Some(texture) = &mut self.texture {
            texture.set(image, egui::TextureOptions::LINEAR);
        } else {
            self.texture =
                Some(ctx.load_texture("yolocam-live", image, egui::TextureOptions::LINEAR));
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_centered(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(30.0), egui::Sense::hover());
            let painter = ui.painter();
            painter.rect_stroke(
                rect.shrink(3.0),
                5.0,
                Stroke::new(1.4_f32, AMBER),
                egui::StrokeKind::Inside,
            );
            painter.circle_stroke(rect.center(), 6.0, Stroke::new(1.4_f32, AMBER));
            ui.label(RichText::new("YoloCam").size(22.0).strong());
            ui.label(RichText::new("LINUX").monospace().size(11.0).color(AMBER));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if self.demo {
                    ui.label(
                        RichText::new("DEMO · NO DEVICE I/O")
                            .monospace()
                            .color(AMBER)
                            .size(11.0),
                    );
                } else {
                    let dot = if self.connected { GREEN } else { MUTED };
                    ui.colored_label(
                        dot,
                        if self.busy {
                            "Firmware operation"
                        } else if self.connected {
                            "Camera connected"
                        } else {
                            "Connecting"
                        },
                    );
                    if self.busy {
                        ui.spinner();
                    }
                    if let Some(version) = &self.state.version {
                        ui.label(
                            RichText::new(&version.serial)
                                .monospace()
                                .size(12.0)
                                .color(MUTED),
                        );
                    }
                }
            });
        });
    }

    fn navigation(&mut self, ui: &mut egui::Ui) {
        ui.add_space(22.0);
        ui.label(
            RichText::new("WORKSPACE")
                .monospace()
                .size(10.0)
                .color(MUTED),
        );
        ui.add_space(16.0);
        for (number, tab) in [
            Tab::Camera,
            Tab::Colour,
            Tab::Firmware,
            Tab::Presets,
            Tab::Device,
        ]
        .iter()
        .enumerate()
        {
            let selected = self.tab == *tab;
            let label = RichText::new(format!("{:02}   {}", number + 1, tab.label()))
                .size(14.0)
                .color(if selected { AMBER } else { MUTED });
            let button = egui::Button::new(label)
                .fill(if selected { FIELD } else { PANEL })
                .min_size([130.0, 44.0].into());
            if ui.add(button).clicked() {
                self.tab = *tab;
                self.save_preferences();
            }
            ui.add_space(4.0);
        }
        ui.add_space(32.0);
        ui.separator();
        ui.add_space(12.0);
        ui.label(RichText::new("YoloCam S3").size(12.0).strong());
        if let Some(version) = &self.state.version {
            ui.label(
                RichText::new(
                    version
                        .firmware
                        .rsplit('-')
                        .next()
                        .unwrap_or(&version.firmware),
                )
                .monospace()
                .size(11.0)
                .color(MUTED),
            );
            ui.label(
                RichText::new(format!("build {}", version.build))
                    .monospace()
                    .size(11.0)
                    .color(MUTED),
            );
        }
        ui.add_space(10.0);
        ui.label(
            RichText::new("Independent, open source camera tools.")
                .size(11.0)
                .color(MUTED),
        );
    }

    fn can_reconnect(&self) -> bool {
        !self.demo && !self.busy && !self.installing && !self.uncertain_update
    }

    fn displayed_status(&self) -> (&str, bool) {
        if !self.connected
            && !self.busy
            && !self.installing
            && !self.uncertain_update
            && let Some(message) = &self.control_connection_error
        {
            (message, true)
        } else {
            (&self.status, self.status_error)
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let (message, error) = self.displayed_status();
            ui.label(
                RichText::new(if error { "!" } else { "·" })
                    .strong()
                    .color(if error { ERROR } else { AMBER }),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(message)
                        .size(12.0)
                        .color(if error { ERROR } else { MUTED }),
                )
                .wrap(),
            );
            if !self.connected
                && !self.demo
                && matches!(self.tab, Tab::Camera | Tab::Colour)
                && ui
                    .add_enabled(self.can_reconnect(), egui::Button::new("Reconnect"))
                    .on_disabled_hover_text("Reconnect is unavailable while firmware work or reboot verification is pending")
                    .clicked()
            {
                self.send(Command::Connect);
            }
        });
    }

    fn camera_panel(&mut self, ui: &mut egui::Ui) {
        panel_title(
            ui,
            "Camera",
            "Changes apply to the camera and its USB video.",
        );
        for section in ["Image", "Exposure", "White Balance", "Focus", "Tracking"] {
            section_title(ui, section);
            for spec in param_specs().iter().filter(|spec| spec.section == section) {
                self.param_control(ui, spec);
            }
            if section == "White Balance" {
                self.wb_offsets(ui);
            }
        }
        if self.state.eptz.is_none() {
            section_title(ui, "Legacy zoom");
            for spec in param_specs().iter().filter(|spec| spec.section == "Zoom") {
                self.param_control(ui, spec);
            }
        }
        section_title(ui, "Output");
        for spec in param_specs().iter().filter(|spec| spec.section == "Video") {
            self.param_control(ui, spec);
        }
        egui::CollapsingHeader::new("HDMI and audio")
            .id_salt("hdmi-audio")
            .show(ui, |ui| {
                for spec in param_specs()
                    .iter()
                    .filter(|spec| ["HDMI", "Audio"].contains(&spec.section))
                {
                    self.param_control(ui, spec);
                }
            });
    }

    fn param_control(&mut self, ui: &mut egui::Ui, spec: &ParamSpec) {
        if !spec.readback {
            self.write_only_control(ui, spec);
            return;
        }
        let actual = self.state.values.get(spec.key).copied();
        let mut value = self
            .desired
            .get(spec.key)
            .copied()
            .or(actual)
            .unwrap_or(spec.min);
        let reason = control_reason(&self.state, spec.key, self.connected, self.busy);
        let enabled = reason.is_none();
        let maximum = if spec.key == "sharpness"
            && self.state.version.as_ref().is_some_and(|v| !v.uses_eptz())
        {
            30
        } else {
            spec.max
        };
        let mut changed = false;
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(spec.label).size(12.0));
                if !spec.options.is_empty() {
                    let selected = spec
                        .options
                        .iter()
                        .find(|(v, _)| *v == value)
                        .map(|(_, label)| (*label).to_owned())
                        .unwrap_or_else(|| value.to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        egui::ComboBox::from_id_salt(spec.key)
                            .selected_text(selected)
                            .width(112.0)
                            .show_ui(ui, |ui| {
                                for (number, label) in spec.options {
                                    let option_reason =
                                        self.requested_control_reason(spec.key, *number);
                                    let response = ui.add_enabled(
                                        option_reason.is_none(),
                                        egui::Button::selectable(value == *number, *label),
                                    );
                                    if response.clicked() {
                                        changed |= value != *number;
                                        value = *number;
                                    }
                                    if let Some(reason) = option_reason {
                                        response.on_disabled_hover_text(reason);
                                    }
                                }
                            });
                    });
                }
            });
            if spec.key == "exposure-ev" {
                let mut step = exposure_display(value);
                let response = ui.add(
                    egui::Slider::new(&mut step, -8..=8)
                        .integer()
                        .clamping(egui::SliderClamping::Edits)
                        .custom_formatter(|value, _| format_exposure_step(value as i32)),
                );
                if response.changed() {
                    value = exposure_wire(step);
                    changed = true;
                }
            } else if spec.options.is_empty() {
                let mut slider = egui::Slider::new(&mut value, spec.min..=maximum)
                    .integer()
                    .clamping(egui::SliderClamping::Edits)
                    .show_value(true);
                if !spec.unit.is_empty() {
                    slider = slider.suffix(format!(" {}", spec.unit));
                }
                changed |= ui.add(slider).changed();
            }
        })
        .response
        .on_hover_text(reason.clone().unwrap_or_default());
        if spec.key == "exposure-ev" {
            ui.label(
                RichText::new("Camera brightness in Auto exposure. −8…+8 steps; 0 is neutral.")
                    .size(10.0)
                    .color(MUTED),
            );
            if let Some(reason) = reason {
                ui.label(RichText::new(reason).size(11.0).color(AMBER));
            }
            if self.connected && !self.busy {
                let action = match (
                    self.state.values.get("exposure-mode"),
                    self.state.values.get("exposure-lock"),
                ) {
                    (Some(0), Some(1)) => Some(("Unlock exposure", "exposure-lock")),
                    (Some(1), _) => Some(("Use Auto exposure", "exposure-mode")),
                    _ => None,
                };
                if let Some((label, key)) = action
                    && ui.button(label).clicked()
                    && self.send(Command::SetParam {
                        key: key.to_owned(),
                        value: 0,
                    })
                    && !self.demo
                {
                    self.desired.insert(key.to_owned(), 0);
                }
            }
        }
        if actual.is_none() && !self.busy && self.connected {
            ui.label(
                RichText::new("Unavailable on this camera")
                    .size(10.0)
                    .color(MUTED),
            );
        }
        if matches!(spec.key, "hdr" | "frame-rate") {
            let requested = if spec.key == "hdr" { 1 } else { value };
            if let Some(reason) = self.requested_control_reason(spec.key, requested) {
                ui.label(RichText::new(reason).size(10.0).color(AMBER));
            } else if spec.key == "hdr"
                && let Some(advisory) = crate::compatibility::hdr_fps_check(
                    Some(1),
                    crate::compatibility::camera_fps(&self.state),
                )
                .advisory
            {
                ui.label(RichText::new(advisory).size(10.0).color(MUTED));
            }
        }
        if let Some(actual) = actual.filter(|value| *value < spec.min || *value > maximum) {
            let (reported, minimum, maximum) = if spec.key == "exposure-ev" {
                (exposure_display(actual), -8, 8)
            } else {
                (actual, spec.min, maximum)
            };
            ui.label(
                RichText::new(format!(
                    "Camera currently reports {reported}. New values must be {minimum}…{maximum}."
                ))
                .size(10.0)
                .color(AMBER),
            );
        }
        ui.add_space(8.0);
        if changed
            && self.requested_control_reason(spec.key, value).is_none()
            && self.send(Command::SetParam {
                key: spec.key.to_owned(),
                value,
            })
            && !self.demo
        {
            self.desired.insert(spec.key.to_owned(), value);
        }
    }

    fn write_only_control(&mut self, ui: &mut egui::Ui, spec: &ParamSpec) {
        let acknowledged = self.state.values.get(spec.key).copied();
        let mut selection = self
            .write_only_choices
            .get(spec.key)
            .copied()
            .or(acknowledged);
        let reason = control_reason(&self.state, spec.key, self.connected, self.busy);
        let mut apply = false;
        ui.add_enabled_ui(reason.is_none(), |ui| {
            ui.label(RichText::new(spec.label).size(12.0));
            ui.horizontal(|ui| {
                let selected_text = selection
                    .and_then(|value| {
                        spec.options
                            .iter()
                            .find(|(v, _)| *v == value)
                            .map(|(_, label)| *label)
                    })
                    .unwrap_or("Choose…");
                egui::ComboBox::from_id_salt(spec.key)
                    .selected_text(selected_text)
                    .width(130.0)
                    .show_ui(ui, |ui| {
                        for (value, label) in spec.options {
                            ui.selectable_value(&mut selection, Some(*value), *label);
                        }
                    });
                apply = ui
                    .add_enabled(selection.is_some(), egui::Button::new("Apply"))
                    .clicked();
            });
        })
        .response
        .on_hover_text(reason.unwrap_or_default());
        let note = match acknowledged {
            Some(value) => {
                let label = spec
                    .options
                    .iter()
                    .find(|(v, _)| *v == value)
                    .map(|(_, label)| *label)
                    .unwrap_or("Command accepted");
                format!("Last acknowledged: {label} · no current readback")
            }
            None => "Current value unavailable · write-only command".to_owned(),
        };
        ui.label(RichText::new(note).size(10.0).color(MUTED));
        ui.add_space(8.0);
        if let Some(value) = selection {
            self.write_only_choices.insert(spec.key.to_owned(), value);
        }
        if apply && let Some(value) = selection {
            self.send(Command::SetParam {
                key: spec.key.to_owned(),
                value,
            });
        }
    }

    fn wb_offsets(&mut self, ui: &mut egui::Ui) {
        let enabled = self.connected
            && !self.busy
            && self.state.wb_offsets.is_some()
            && self.value("wb-mode") == Some(0);
        let (mut red, mut blue) = self
            .desired_wb
            .or(self.state.wb_offsets)
            .unwrap_or((10, 10));
        let mut changed = false;
        ui.add_enabled_ui(enabled, |ui| {
            ui.label(RichText::new("Red / blue offset").size(12.0));
            changed |= ui
                .add(egui::Slider::new(&mut red, 0..=20).text("Red"))
                .changed();
            changed |= ui
                .add(egui::Slider::new(&mut blue, 0..=20).text("Blue"))
                .changed();
        })
        .response
        .on_hover_text("Available in manual white balance. 10 is neutral.");
        if changed && self.send(Command::SetWbOffsets(red, blue)) && !self.demo {
            self.desired_wb = Some((red, blue));
        }
        ui.add_space(8.0);
    }

    fn value(&self, key: &str) -> Option<i32> {
        self.desired
            .get(key)
            .or(self.state.values.get(key))
            .copied()
    }

    fn colour_panel(&mut self, ui: &mut egui::Ui) {
        panel_title(ui, "Colour", "Camera-side HSL, colour profiles and LUTs.");
        section_title(ui, "HSL adjustments");
        if let Some(spec) = param_specs().iter().find(|spec| spec.key == "hsl-enable") {
            self.param_control(ui, spec);
        }
        let enabled = self.connected
            && !self.busy
            && self.state.hsl.is_some()
            && self.value("hsl-enable") == Some(1);
        let hsl = self.state.hsl.clone().unwrap_or_default();
        ui.add_enabled_ui(enabled, |ui| {
            egui::Grid::new("hsl-grid")
                .spacing([8.0, 10.0])
                .show(ui, |ui| {
                    for label in ["Colour", "Hue", "Sat.", "Light"] {
                        ui.label(RichText::new(label).size(11.0).color(MUTED));
                    }
                    ui.end_row();
                    let colours = [
                        ("Red", Color32::from_rgb(212, 101, 94)),
                        ("Orange", Color32::from_rgb(220, 151, 73)),
                        ("Yellow", Color32::from_rgb(204, 187, 80)),
                        ("Green", Color32::from_rgb(112, 168, 121)),
                        ("Cyan", Color32::from_rgb(103, 172, 184)),
                        ("Blue", Color32::from_rgb(119, 146, 201)),
                        ("Purple", Color32::from_rgb(157, 132, 186)),
                        ("Magenta", Color32::from_rgb(191, 124, 164)),
                    ];
                    for (colour, (label, swatch)) in colours.iter().enumerate() {
                        ui.colored_label(*swatch, *label);
                        for component in 0..3 {
                            let mut value = self
                                .desired_hsl
                                .get(&(component, colour))
                                .copied()
                                .unwrap_or(hsl.values[component][colour]);
                            if ui
                                .add(
                                    egui::DragValue::new(&mut value)
                                        .range(-100..=100)
                                        .speed(1.0),
                                )
                                .changed()
                                && self.send(Command::SetHslField {
                                    component,
                                    colour,
                                    value,
                                })
                                && !self.demo
                            {
                                self.desired_hsl.insert((component, colour), value);
                            }
                        }
                        ui.end_row();
                    }
                });
            ui.add_space(10.0);
            if ui.button("Reset HSL").clicked() {
                self.send(Command::ResetHsl);
            }
        });
        if self.state.hsl.is_some() && self.value("hsl-enable") != Some(1) {
            ui.label(
                RichText::new("Turn on HSL adjustments to edit the eight colour bands.")
                    .size(11.0)
                    .color(MUTED),
            );
        }
        section_title(ui, "Colour profile");
        if let Some(profile) = &self.state.profile {
            let name = if profile.id == 0 {
                "Factory".to_owned()
            } else {
                self.state
                    .profile_info
                    .as_ref()
                    .map(|i| i.description.clone())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| format!("Profile {}", profile.id))
            };
            ui.label(
                RichText::new(format!(
                    "{name} · {}",
                    if profile.hdr_flag == 0 { "SDR" } else { "HDR" }
                ))
                .strong(),
            );
            ui.label(
                RichText::new(format!("ID {} / scene {}", profile.id, profile.scene_id))
                    .monospace()
                    .size(11.0)
                    .color(MUTED),
            );
        }
        ui.add_enabled_ui(self.connected && !self.busy && self.state.profile.is_some(), |ui| {
            if ui.button("Use factory profile").clicked() { self.send(Command::SelectProfile { id: 0, scene: 0 }); }
            egui::CollapsingHeader::new("Select an installed profile").show(ui, |ui| {
                ui.label(RichText::new("Select an ID already installed on the camera. The current HDR bank is used.").size(11.0).color(MUTED));
                ui.horizontal(|ui| {
                    ui.label("Profile"); ui.add(egui::DragValue::new(&mut self.profile_id).range(0..=65535));
                    ui.label("Scene"); ui.add(egui::DragValue::new(&mut self.profile_scene).range(0..=65535));
                });
                if ui.button("Apply installed profile").clicked() { self.send(Command::SelectProfile { id: self.profile_id, scene: self.profile_scene }); }
            });
        });
        section_title(ui, "Colour LUT");
        for spec in param_specs()
            .iter()
            .filter(|spec| ["clut", "clut-attr"].contains(&spec.key))
        {
            self.param_control(ui, spec);
        }
        ui.label(
            RichText::new("Import a native YoloCam LUT JSON file: 5,508 packed RGB delta entries.")
                .size(11.0)
                .color(MUTED),
        );
        ui.add(
            egui::TextEdit::singleline(&mut self.clut_path)
                .hint_text("/home/you/look.json")
                .desired_width(f32::INFINITY),
        );
        ui.add_enabled_ui(
            self.connected && !self.busy && self.state.values.contains_key("clut"),
            |ui| {
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            !self.clut_path.trim().is_empty(),
                            egui::Button::new("Import LUT"),
                        )
                        .clicked()
                    {
                        self.send(Command::UploadClut(PathBuf::from(self.clut_path.trim())));
                    }
                    if ui.button("Reset LUT").clicked() {
                        self.send(Command::ResetClut);
                    }
                });
            },
        );
    }

    fn preview_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("LIVE VIEW")
                    .monospace()
                    .size(11.0)
                    .color(MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if self.settings.preview_enabled {
                    "Pause preview"
                } else {
                    "Start preview"
                };
                if ui
                    .add_enabled(
                        !self.busy && !self.installing && !self.uncertain_update,
                        egui::Button::new(label),
                    )
                    .on_hover_text(
                        "Ctrl+P · Pause to release video capture for OBS or a video call",
                    )
                    .clicked()
                {
                    self.toggle_preview();
                }
            });
        });
        self.preview_mode_controls(ui);
        ui.add_space(10.0);
        let width = ui.available_width();
        let height = (width * 9.0 / 16.0).min((ui.available_height() - 245.0).max(150.0));
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, Color32::from_rgb(11, 13, 14));
        let mut image_rect = None;
        if self.settings.preview_enabled && self.demo {
            let image = letterboxed_image_rect(
                rect,
                (
                    self.settings.preview_mode.width as usize,
                    self.settings.preview_mode.height as usize,
                ),
            );
            draw_demo_pattern(&painter, image);
            image_rect = Some(image);
        } else if self.settings.preview_enabled
            && let Some(texture) = &self.texture
        {
            let image = letterboxed_image_rect(rect, self.frame_dimensions);
            painter.image(
                texture.id(),
                image,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
            image_rect = Some(image);
        } else {
            let message = if self.installing {
                "PREVIEW RELEASED FOR FIRMWARE UPDATE"
            } else if self.uncertain_update {
                "WAITING FOR CAMERA VERIFICATION"
            } else if self.settings.preview_enabled
                && (self.preview_attempted || self.preview_error.is_some())
            {
                "PREVIEW UNAVAILABLE"
            } else if self.settings.preview_enabled && self.preview_probe.is_some() {
                "READING VIDEO MODES"
            } else if self.settings.preview_enabled {
                "WAITING FOR CAMERA"
            } else {
                "PREVIEW PAUSED"
            };
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                message,
                FontId::monospace(14.0),
                MUTED,
            );
            painter.text(
                rect.center() + Vec2::new(0.0, 28.0),
                egui::Align2::CENTER_CENTER,
                if self.settings.preview_enabled {
                    "Camera controls remain available."
                } else {
                    "Video capture is free for OBS and video calls."
                },
                FontId::proportional(12.0),
                MUTED,
            );
        }
        painter.rect_stroke(
            rect,
            4.0,
            Stroke::new(1.0_f32, LINE),
            egui::StrokeKind::Inside,
        );
        self.focus_overlay(ui, &painter, image_rect);
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if self.preview.is_running() {
                    "LIVE"
                } else if self.demo && self.settings.preview_enabled {
                    "TEST PATTERN"
                } else {
                    "USB VIDEO"
                })
                .monospace()
                .size(10.0)
                .color(AMBER),
            );
            if self.preview.is_running() && self.frame_dimensions.0 > 0 {
                ui.label(
                    RichText::new(format!(
                        "{} × {} · {:.1} fps delivered",
                        self.frame_dimensions.0, self.frame_dimensions.1, self.frame_fps
                    ))
                    .monospace()
                    .size(10.0)
                    .color(MUTED),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new("Ctrl+P  preview")
                        .monospace()
                        .size(10.0)
                        .color(MUTED),
                );
            });
        });
        if let Some(reason) = self.preview_focus_reason() {
            ui.label(RichText::new(reason).size(11.0).color(MUTED));
            if self.connected
                && !self.busy
                && self.state.focus_target.is_none()
                && matches!(self.state.values.get("focus-mode"), Some(1 | 3))
                && ui.small_button("Refresh focus region").clicked()
            {
                self.send(Command::Refresh);
            }
        } else {
            ui.label(
                RichText::new("Click or drag the focus region. Release to apply.")
                    .size(11.0)
                    .color(MUTED),
            );
        }
        if self.state.eptz.is_some() {
            self.crop_controls(ui);
        } else {
            ui.add_space(22.0);
            ui.label(
                RichText::new("Camera-side controls also affect OBS and your video-call image.")
                    .size(12.0)
                    .color(MUTED),
            );
        }
    }

    fn preview_mode_controls(&mut self, ui: &mut egui::Ui) {
        ui.add_space(9.0);
        let selected = self.settings.preview_mode;
        let capabilities = self.preview_capabilities.clone();
        let selectable =
            capabilities.is_some() && !self.busy && !self.installing && !self.uncertain_update;
        let mut chosen = selected;
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Preview mode").size(11.0).color(MUTED));
            ui.add_enabled_ui(selectable, |ui| {
                egui::ComboBox::from_id_salt("preview-mode")
                    .selected_text(selected.label())
                    .width(215.0)
                    .show_ui(ui, |ui| {
                        if let Some(capabilities) = &capabilities {
                            for mode in &capabilities.modes {
                                let reason = self.preview_choice_reason(Some(capabilities), *mode);
                                let response = ui.add_enabled(
                                    reason.is_none(),
                                    egui::Button::selectable(*mode == selected, mode.label()),
                                );
                                if response.clicked() {
                                    chosen = *mode;
                                }
                                if let Some(reason) = reason {
                                    response.on_disabled_hover_text(reason);
                                }
                            }
                        }
                    });
            });
            if ui
                .add_enabled(
                    !self.demo
                        && !self.busy
                        && !self.installing
                        && !self.uncertain_update
                        && self.preview_probe.is_none(),
                    egui::Button::new("Refresh modes").small(),
                )
                .clicked()
            {
                self.refresh_preview_modes();
            }
        });
        if chosen != selected {
            self.select_preview_mode(chosen);
        }
        let reason = self.preview_error.clone().or_else(|| {
            self.preview_choice_reason(
                self.preview_capabilities.as_ref(),
                self.settings.preview_mode,
            )
        });
        if let Some(reason) = reason {
            ui.label(RichText::new(reason).size(11.0).color(AMBER));
            if self.settings.preview_enabled
                && self.preview_error.is_some()
                && ui
                    .add_enabled(
                        selectable
                            && self
                                .preview_choice_reason(capabilities.as_ref(), chosen)
                                .is_none(),
                        egui::Button::new("Retry preview").small(),
                    )
                    .clicked()
            {
                self.preview_attempted = false;
                self.preview_error = None;
                self.frame_sequence = 0;
            }
        } else if self.preview_probe.is_some() {
            ui.label(
                RichText::new("Reading capture modes from the USB video device…")
                    .size(10.0)
                    .color(MUTED),
            );
        } else if let Some(mode) = self.preview.active_mode() {
            ui.label(
                RichText::new(format!("Negotiated {} · MJPEG", mode.label()))
                    .size(10.0)
                    .color(MUTED),
            );
        } else {
            ui.label(
                RichText::new("MJPEG USB capture · Starting capture may update camera frame rate.")
                    .size(10.0)
                    .color(MUTED),
            );
        }
        if let Some(capabilities) = capabilities
            && !capabilities.warnings.is_empty()
        {
            ui.label(
                RichText::new("Video mode enumeration notes")
                    .size(10.0)
                    .color(AMBER),
            )
            .on_hover_text(capabilities.warnings.join("\n"));
        }
    }

    fn preview_focus_reason(&self) -> Option<String> {
        if self.busy || self.installing || self.uncertain_update {
            return Some("Focus positioning is paused during the firmware operation".to_owned());
        }
        if !self.connected {
            return Some("Connect the camera to position focus".to_owned());
        }
        if self.focus_pending
            || ["focus-mode", "focus-level"]
                .iter()
                .any(|key| self.desired.contains_key(*key))
            || self.desired_crop.is_some()
        {
            return Some("Waiting for the camera to confirm focus or framing changes".to_owned());
        }
        if let Some(reason) = focus::control_reason(&self.state) {
            return Some(reason);
        }
        if !self.settings.preview_enabled {
            return Some("Start preview to position focus".to_owned());
        }
        let fresh = self.demo
            || (self.preview.is_running()
                && self
                    .frame_captured_at
                    .is_some_and(|received| received.elapsed() <= Duration::from_secs(2)));
        (!fresh).then(|| "Focus positioning requires a fresh live preview".to_owned())
    }

    fn focus_overlay(
        &mut self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        image_rect: Option<egui::Rect>,
    ) {
        let interactive = self.preview_focus_reason().is_none();
        if !interactive {
            self.focus_draft = None;
            self.focus_drag_armed = false;
        }
        let (Some(image_rect), Some(actual)) = (image_rect, self.state.focus_target) else {
            return;
        };
        if focus::control_reason(&self.state).is_some() {
            return;
        }
        if interactive {
            let response = ui
                .interact(
                    image_rect,
                    ui.id().with("focus-overlay"),
                    egui::Sense::click_and_drag(),
                )
                .on_hover_cursor(egui::CursorIcon::Crosshair);
            if response.drag_started() {
                self.focus_drag_armed = response
                    .ctx
                    .input(|input| input.pointer.press_origin())
                    .is_some_and(|origin| image_rect.contains(origin));
            }
            if response.dragged() && self.focus_drag_armed {
                self.focus_draft = response
                    .interact_pointer_pos()
                    .and_then(|position| focus_target_at(actual, image_rect, position));
            }
            let commit = response.clicked() || (response.drag_stopped() && self.focus_drag_armed);
            if commit {
                let target = response
                    .interact_pointer_pos()
                    .and_then(|position| focus_target_at(actual, image_rect, position));
                self.focus_draft = None;
                self.focus_drag_armed = false;
                if let Some(target) = target
                    && self.send(Command::SetFocusTarget(target))
                {
                    self.focus_pending = !self.demo;
                }
            }
        }
        let target = self
            .focus_draft
            .or(self.state.focus_target)
            .unwrap_or(actual);
        let target_rect = egui::Rect::from_min_max(
            egui::pos2(
                image_rect.left() + target.left * image_rect.width(),
                image_rect.top() + target.top * image_rect.height(),
            ),
            egui::pos2(
                image_rect.left() + target.right * image_rect.width(),
                image_rect.top() + target.bottom * image_rect.height(),
            ),
        );
        let colour = if interactive { AMBER } else { MUTED };
        painter.rect_stroke(
            target_rect,
            1.0,
            Stroke::new(1.6_f32, colour),
            egui::StrokeKind::Inside,
        );
        painter.circle_stroke(target_rect.center(), 3.0, Stroke::new(1.2_f32, colour));
    }

    fn crop_controls(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "Framing");
        let Some(mut crop) = self
            .desired_crop
            .clone()
            .or_else(|| self.state.eptz.clone())
        else {
            return;
        };
        let mut changed = false;
        ui.add_enabled_ui(self.connected && !self.busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Zoom").size(12.0));
                let old_zoom = crop.zoom;
                if ui
                    .add(
                        egui::Slider::new(&mut crop.zoom, 1.0..=4.0)
                            .step_by(0.01)
                            .suffix("×"),
                    )
                    .changed()
                {
                    let center_x = crop.left + 0.5 / old_zoom;
                    let center_y = crop.top + 0.5 / old_zoom;
                    let extent = 1.0 / crop.zoom;
                    crop = Crop::new(
                        crop.zoom,
                        (center_x - extent / 2.0).clamp(0.0, 1.0 - extent),
                        (center_y - extent / 2.0).clamp(0.0, 1.0 - extent),
                    )
                    .unwrap_or(crop.clone());
                    changed = true;
                }
                if ui.button("Centre").clicked()
                    && let Ok(centered) = Crop::centred(crop.zoom)
                {
                    crop = centered;
                    changed = true;
                }
                if ui.button("Full frame").clicked() {
                    crop = Crop::centred(1.0).unwrap();
                    changed = true;
                }
            });
            let available = (1.0 - 1.0 / crop.zoom).max(0.0);
            let (mut pan, mut tilt) = (
                if available > 0.0 {
                    crop.left / available * 200.0 - 100.0
                } else {
                    0.0
                },
                if available > 0.0 {
                    crop.top / available * 200.0 - 100.0
                } else {
                    0.0
                },
            );
            ui.add_enabled_ui(available > 0.0, |ui| {
                ui.horizontal(|ui| {
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut pan, -100.0..=100.0)
                                .text("Pan")
                                .show_value(false),
                        )
                        .changed();
                    changed |= ui
                        .add(
                            egui::Slider::new(&mut tilt, -100.0..=100.0)
                                .text("Tilt")
                                .show_value(false),
                        )
                        .changed();
                });
            });
            if changed {
                crop = Crop::new(
                    crop.zoom,
                    (pan + 100.0) / 200.0 * available,
                    (tilt + 100.0) / 200.0 * available,
                )
                .unwrap_or(crop.clone());
            }
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Camera presets").size(11.0).color(MUTED));
                for (slot, label) in ["A", "B", "C", "D"].iter().enumerate() {
                    ui.selectable_value(&mut self.crop_slot, slot as u8, *label);
                }
                if ui
                    .add_enabled(
                        self.state.presets.contains_key(&self.crop_slot),
                        egui::Button::new("Recall"),
                    )
                    .clicked()
                {
                    self.send(Command::RecallCrop(self.crop_slot));
                }
                if ui
                    .button("Save")
                    .on_hover_text("Overwrite this crop preset on the camera")
                    .clicked()
                {
                    self.send(Command::SaveCrop(self.crop_slot));
                }
            });
        });
        if changed && self.send(Command::SetCrop(crop.clone())) && !self.demo {
            self.desired_crop = Some(crop);
        }
    }

    fn firmware_page(&mut self, ui: &mut egui::Ui) {
        page_title(
            ui,
            "Firmware",
            "Official YoloLiv updates, downloaded and verified on Linux.",
        );
        section_title(ui, "Connected camera");
        if let Some(version) = &self.state.version {
            version_details(ui, version);
        } else {
            ui.label("Connect your YoloCam to check its firmware.");
        }
        ui.add_space(16.0);
        let can_check = self.connected && !self.busy && !self.demo && !self.uncertain_update;
        if ui
            .add_enabled(can_check, accent_button("Check for updates"))
            .clicked()
        {
            self.send(Command::CheckFirmware);
        }
        if self.demo {
            ui.label(
                RichText::new("Firmware operations require a real connected camera.").color(MUTED),
            );
        }
        if let Some(release) = self.release.clone() {
            section_title(ui, "Official release");
            ui.label(
                RichText::new(format!("{} · build {}", release.firmware, release.build))
                    .size(20.0)
                    .strong(),
            );
            let newer = self
                .state
                .version
                .as_ref()
                .is_some_and(|current| release.newer_than(current).unwrap_or(false));
            ui.colored_label(
                if newer { AMBER } else { GREEN },
                if newer {
                    "An update is available"
                } else {
                    "Your camera is up to date"
                },
            );
            if !release.description.trim().is_empty() {
                ui.add_space(10.0);
                ui.label(&release.description);
            }
            ui.add_space(10.0);
            ui.label(RichText::new("Package source").size(11.0).color(MUTED));
            ui.add(
                egui::Label::new(
                    RichText::new(&release.url)
                        .monospace()
                        .size(11.0)
                        .color(MUTED),
                )
                .wrap(),
            );
            ui.add_space(16.0);
            if ui
                .add_enabled(
                    newer && self.connected && !self.busy && !self.demo && !self.uncertain_update,
                    egui::Button::new("Download and verify"),
                )
                .clicked()
            {
                self.send(Command::PrepareFirmware);
            }
        }
        if !self.firmware_phase.is_empty() {
            ui.add_space(18.0);
            let label = phase_label(&self.firmware_phase);
            ui.label(RichText::new(label).color(AMBER));
            ui.add(
                egui::ProgressBar::new(self.firmware_percent as f32 / 100.0)
                    .show_percentage()
                    .desired_width(400.0),
            );
        }
        if let Some(prepared) = self.prepared.clone() {
            section_title(ui, "Verified package");
            ui.label(format!(
                "{:.1} MiB · {}",
                prepared.size as f64 / 1_048_576.0,
                prepared.path.display()
            ));
            ui.label(RichText::new("SHA-256").size(11.0).color(MUTED));
            ui.add(egui::Label::new(RichText::new(&prepared.sha256).monospace().size(11.0)).wrap());
            for (image, hash) in &prepared.image_md5 {
                ui.label(
                    RichText::new(format!("{image}: {hash}"))
                        .monospace()
                        .size(11.0)
                        .color(MUTED),
                );
            }
            ui.add_space(12.0);
            let newer = self
                .state
                .version
                .as_ref()
                .is_some_and(|v| prepared.release.newer_than(v).unwrap_or(false));
            if ui
                .add_enabled(
                    newer && self.connected && !self.busy && !self.demo && !self.uncertain_update,
                    accent_button("Review and install…"),
                )
                .clicked()
            {
                self.confirmation_open = true;
                self.keep_usb_checked = false;
            }
        }
        if self.installing {
            ui.add_space(18.0);
            ui.colored_label(AMBER, "Keep your USB cable attached. The camera will reboot, then this app will verify its firmware.");
        }
        if self.uncertain_update {
            ui.add_space(18.0);
            ui.colored_label(ERROR, "The update result could not be verified. Keep USB attached and allow the camera to finish rebooting.");
            ui.label(
                "Checking again reads the camera version; it does not flash the package again.",
            );
            if ui
                .add_enabled(!self.busy, egui::Button::new("Verify camera after reboot"))
                .clicked()
            {
                self.installing = true;
                self.send(Command::VerifyUpdate);
            }
        }
        ui.add_space(24.0);
        ui.label(RichText::new("Camera settings are backed up locally before installation. Proprietary firmware is downloaded from YoloLiv and is not bundled with this app.").size(12.0).color(MUTED));
    }

    fn firmware_confirmation(&mut self, ctx: &egui::Context) {
        if !self.confirmation_open {
            return;
        }
        let mut confirm = false;
        let mut cancel = false;
        egui::Window::new("Install camera firmware").collapsible(false).resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO).default_width(470.0).show(ctx, |ui| {
                ui.colored_label(AMBER, "Keep the camera connected until reboot verification completes.");
                ui.add_space(12.0);
                if let Some(current) = &self.state.version { ui.label(format!("Current: {} / build {}", current.firmware, current.build)); ui.label(format!("Camera: {}", current.serial)); }
                if let Some(prepared) = &self.prepared { ui.label(format!("Install: {} / build {}", prepared.release.firmware, prepared.release.build)); }
                ui.add_space(12.0);
                ui.label("Live preview will pause. Camera controls are disabled during installation. The camera will reboot automatically.");
                ui.add_space(12.0);
                ui.checkbox(&mut self.keep_usb_checked, "I will keep the USB cable attached until verification finishes");
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    cancel = ui.button("Cancel").clicked();
                    confirm = ui.add_enabled(self.keep_usb_checked && !self.busy && self.connected, accent_button("Install verified firmware")).clicked();
                });
            });
        if cancel {
            self.confirmation_open = false;
        }
        if confirm {
            self.confirmation_open = false;
            self.restore_preview_after_update = self.settings.preview_enabled;
            self.preview.stop();
            self.texture = None;
            self.preview_attempted = true;
            if self.send(Command::InstallFirmware) {
                self.installing = true;
            } else {
                self.preview_attempted = false;
            }
        }
    }

    fn presets_page(&mut self, ui: &mut egui::Ui) {
        page_title(
            ui,
            "Presets",
            "Save real camera readbacks, then restore the complete look and framing.",
        );
        section_title(ui, "Capture a preset");
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.preset_name)
                    .hint_text("Preset name")
                    .desired_width(250.0),
            );
            if ui
                .add_enabled(
                    self.connected
                        && !self.busy
                        && !self.demo
                        && !self.preset_name.trim().is_empty(),
                    accent_button("Save current camera settings"),
                )
                .clicked()
            {
                self.send(Command::CapturePreset(self.preset_name.trim().to_owned()));
            }
        });
        ui.label(RichText::new("Includes supported settings active in the current camera modes, HSL, crop and profile. Manual focus and white balance are saved only in their manual modes.").size(12.0).color(MUTED));
        section_title(ui, "Saved looks");
        if self.presets.is_empty() {
            ui.label(
                RichText::new(
                    "No presets yet. Set up your camera, then save its current settings.",
                )
                .color(MUTED),
            );
        }
        for preset in self.presets.clone() {
            let compatibility_reason = self.preset_reason(&preset);
            ui.horizontal(|ui| {
                let selected = self.selected_preset.as_deref() == Some(&preset.name);
                if ui
                    .selectable_label(selected, RichText::new(&preset.name).size(16.0))
                    .clicked()
                {
                    self.selected_preset = Some(preset.name.clone());
                }
                ui.label(
                    RichText::new(format!("{} controls", preset.values.len()))
                        .monospace()
                        .size(11.0)
                        .color(MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled(
                            self.connected
                                && !self.busy
                                && !self.demo
                                && compatibility_reason.is_none(),
                            egui::Button::new("Apply"),
                        )
                        .on_disabled_hover_text(compatibility_reason.clone().unwrap_or_default())
                        .clicked()
                    {
                        self.send(Command::ApplyPreset(Box::new(preset.clone())));
                    }
                });
            });
            if let Some(reason) = compatibility_reason {
                ui.label(RichText::new(reason).size(11.0).color(AMBER));
            } else if let Ok(plan) = crate::compatibility::preset_plan(&self.state, &preset) {
                for advisory in plan.advisories {
                    ui.label(RichText::new(advisory).size(11.0).color(MUTED));
                }
            }
            if let Some(note) = preset.omission_note() {
                let details = preset
                    .omitted_controls
                    .iter()
                    .map(|(key, reason)| format!("{key}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.label(RichText::new(note).size(11.0).color(AMBER))
                    .on_hover_text(details);
            }
            ui.separator();
        }
        section_title(ui, "Import / export");
        ui.label(RichText::new("Import an app preset JSON file").size(12.0));
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.import_path)
                    .hint_text("/home/you/preset.json")
                    .desired_width(350.0),
            );
            if ui
                .add_enabled(
                    !self.demo && !self.import_path.trim().is_empty(),
                    egui::Button::new("Import"),
                )
                .clicked()
            {
                let result = (|| -> Result<Preset> {
                    let bytes = std::fs::read(self.import_path.trim())?;
                    if bytes.len() > 1024 * 1024 {
                        anyhow::bail!("Preset JSON is larger than 1 MiB");
                    }
                    let preset = Preset::from_json(&bytes)?;
                    self.store.save_preset(&preset)?;
                    Ok(preset)
                })();
                match result {
                    Ok(preset) => {
                        self.selected_preset = Some(preset.name.clone());
                        self.reload_presets();
                        self.status = preset_status(
                            "Preset imported locally. Apply it to change the camera.",
                            &preset,
                        );
                        self.status_error = false;
                    }
                    Err(error) => self.error(format!("{error:#}")),
                }
            }
        });
        ui.add_space(12.0);
        ui.label(RichText::new("Export the selected preset").size(12.0));
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.export_path)
                    .hint_text("/home/you/preset.json")
                    .desired_width(350.0),
            );
            if ui
                .add_enabled(
                    !self.demo
                        && self.selected_preset.is_some()
                        && !self.export_path.trim().is_empty(),
                    egui::Button::new("Export"),
                )
                .clicked()
            {
                let result = self
                    .selected_preset
                    .as_ref()
                    .context("Select a preset")
                    .and_then(|name| self.store.load_preset(name))
                    .and_then(|preset| {
                        crate::settings::atomic_json(
                            &PathBuf::from(self.export_path.trim()),
                            &preset,
                        )
                    });
                match result {
                    Ok(()) => {
                        self.status = "Preset exported".to_owned();
                        self.status_error = false;
                    }
                    Err(error) => self.error(format!("{error:#}")),
                }
            }
        });
        if let Some(name) = self.selected_preset.clone() {
            ui.add_space(20.0);
            if ui
                .add_enabled(
                    !self.demo,
                    egui::Button::new(format!("Delete local preset {name}")),
                )
                .clicked()
            {
                match self.store.delete_preset(&name) {
                    Ok(()) => {
                        self.selected_preset = None;
                        self.reload_presets();
                    }
                    Err(error) => self.error(format!("{error:#}")),
                }
            }
        }
        ui.add_space(24.0);
        ui.label(
            RichText::new(format!(
                "Stored locally in {}",
                self.store.directory().join("presets").display()
            ))
            .monospace()
            .size(11.0)
            .color(MUTED),
        );
    }

    fn device_page(&mut self, ui: &mut egui::Ui) {
        page_title(
            ui,
            "Device",
            "Connection details and the capabilities reported by this firmware.",
        );
        if let Some(version) = &self.state.version {
            version_details(ui, version);
        }
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.connected && !self.busy && !self.demo,
                    egui::Button::new("Refresh camera values"),
                )
                .clicked()
            {
                self.send(Command::Refresh);
            }
            if ui
                .add_enabled(
                    self.can_reconnect(),
                    egui::Button::new("Reconnect"),
                )
                .clicked()
            {
                self.send(Command::Connect);
            }
            if ui.button("Read device diagnostics").clicked() {
                let diagnostics = if self.demo {
                    serde_json::json!({"mode": "demo", "camera": "YoloCam S3", "video_driver": "uvcvideo", "control_connection": "USB network", "video_device": "/dev/v4l/by-id/usb-YoloCam_S3_DEMO-video-index0"})
                } else { crate::discovery::diagnostics() };
                self.diagnostics = Some(serde_json::to_string_pretty(&diagnostics).unwrap_or_default());
            }
        });
        section_title(ui, "Linux video capture");
        ui.label("Video uses the Linux UVC driver. Camera controls use the camera’s USB-network connection.");
        ui.label("Pause this app’s preview to release video capture for OBS, GUVCView or video calls. Camera-side changes affect their image too.");
        ui.add_space(8.0);
        let device = if self.demo {
            Some(PathBuf::from(
                "/dev/v4l/by-id/usb-YoloCam_S3_DEMO-video-index0",
            ))
        } else {
            self.settings
                .preview_device
                .clone()
                .or_else(crate::discovery::discover_video)
        };
        if let Some(device) = device {
            ui.label(
                RichText::new(device.display().to_string())
                    .monospace()
                    .color(MUTED),
            );
        }
        if !self.state.unsupported.is_empty() {
            section_title(ui, "Unavailable controls");
            for (key, reason) in &self.state.unsupported {
                ui.label(RichText::new(key).monospace().size(12.0));
                ui.label(RichText::new(reason).size(11.0).color(MUTED));
                ui.add_space(6.0);
            }
        }
        if let Some(diagnostics) = &self.diagnostics {
            section_title(ui, "Diagnostics");
            ui.add(egui::Label::new(RichText::new(diagnostics).monospace().size(11.0)).wrap());
        }
        section_title(ui, "Open source");
        ui.label(format!(
            "YoloCam Linux {} · GPL-3.0-or-later",
            env!("CARGO_PKG_VERSION")
        ));
        ui.hyperlink_to(
            "Source code and issues",
            "https://github.com/rwfresh/yolocam-linux",
        );
        ui.label(
            RichText::new("Independent community software; not affiliated with YoloLiv.")
                .size(12.0)
                .color(MUTED),
        );
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.capture_demo(ctx);
        self.poll_worker();
        if ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::P)) {
            self.toggle_preview();
        }
        if ctx.input(|input| input.viewport().close_requested())
            && (self.installing || (self.busy && self.firmware_phase == "commit"))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.status = "Firmware installation is running. Keep USB attached until the camera reboots and verification finishes.".to_owned();
            self.status_error = false;
        }
        self.update_preview(ctx);
        egui::TopBottomPanel::top("header")
            .exact_height(62.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(18, 10)),
            )
            .show(ctx, |ui| self.header(ui));
        egui::TopBottomPanel::bottom("status")
            .min_height(39.0)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(16, 9)),
            )
            .show(ctx, |ui| self.footer(ui));
        egui::SidePanel::left("navigation")
            .exact_width(154.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ctx, |ui| self.navigation(ui));
        if matches!(self.tab, Tab::Camera | Tab::Colour) {
            egui::SidePanel::right("controls")
                .default_width(if self.tab == Tab::Colour {
                    370.0
                } else {
                    350.0
                })
                .width_range(320.0..=470.0)
                .frame(
                    egui::Frame::new()
                        .fill(PANEL)
                        .inner_margin(egui::Margin::symmetric(20, 18)),
                )
                .show(ctx, |ui| {
                    ui.style_mut().spacing.slider_width = (ui.available_width() - 60.0).max(170.0);
                    egui::ScrollArea::vertical()
                        .id_salt(self.tab.key())
                        .show(ui, |ui| {
                            if self.tab == Tab::Camera {
                                self.camera_panel(ui);
                            } else {
                                self.colour_panel(ui);
                            }
                        });
                });
        }
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BACKGROUND)
                    .inner_margin(egui::Margin::same(24)),
            )
            .show(ctx, |ui| {
                if matches!(self.tab, Tab::Camera | Tab::Colour) {
                    self.preview_panel(ui);
                } else {
                    egui::ScrollArea::vertical()
                        .id_salt(self.tab.key())
                        .show(ui, |ui| match self.tab {
                            Tab::Firmware => self.firmware_page(ui),
                            Tab::Presets => self.presets_page(ui),
                            Tab::Device => self.device_page(ui),
                            _ => {}
                        });
                }
            });
        self.firmware_confirmation(ctx);
        ctx.request_repaint_after(if self.preview.is_running() || self.demo {
            let fps = self
                .preview
                .active_mode()
                .unwrap_or(self.settings.preview_mode)
                .fps()
                .clamp(1.0, 60.0);
            Duration::from_secs_f64(1.0 / fps)
        } else {
            Duration::from_millis(200)
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.preview.stop();
        self.save_preferences();
        if let Some(worker) = &mut self.worker {
            worker.shutdown();
        }
    }
}

fn configure_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = BACKGROUND;
    visuals.faint_bg_color = FIELD;
    visuals.override_text_color = Some(TEXT);
    visuals.selection.bg_fill = Color32::from_rgb(101, 77, 38);
    visuals.selection.stroke = Stroke::new(1.0_f32, AMBER);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    visuals.widgets.inactive.bg_fill = FIELD;
    visuals.widgets.inactive.weak_bg_fill = FIELD;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, LINE);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(57, 61, 63);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(57, 61, 63);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, AMBER);
    visuals.widgets.active.bg_fill = Color32::from_rgb(79, 66, 43);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(79, 66, 43);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, AMBER);
    visuals.hyperlink_color = AMBER;
    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(10.0, 7.0);
        style.spacing.button_padding = Vec2::new(12.0, 7.0);
        style.spacing.interact_size = Vec2::new(38.0, 28.0);
        style
            .text_styles
            .insert(egui::TextStyle::Body, FontId::proportional(13.0));
        style
            .text_styles
            .insert(egui::TextStyle::Button, FontId::proportional(12.0));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, FontId::proportional(25.0));
        style
            .text_styles
            .insert(egui::TextStyle::Monospace, FontId::monospace(12.0));
    });
}

fn control_reason(state: &CameraState, key: &str, connected: bool, busy: bool) -> Option<String> {
    if busy {
        return Some("Camera controls are paused during the firmware operation".to_owned());
    }
    if !connected {
        return Some("Waiting for the camera control connection".to_owned());
    }
    if let Some(reason) = unsupported_control_reason(key) {
        return Some(reason.to_owned());
    }
    if let Some(reason) = state.unsupported.get(key) {
        return Some(reason.clone());
    }
    if !state.values.contains_key(key)
        && !param_specs()
            .iter()
            .find(|spec| spec.key == key)
            .is_some_and(|spec| !spec.readback)
    {
        return Some("This camera did not return a value for this control".to_owned());
    }
    match key {
        "exposure-iso" | "exposure-time" | "exposure-aperture"
            if state.values.get("exposure-mode") != Some(&1) =>
        {
            Some("Select manual exposure to adjust this control".to_owned())
        }
        "exposure-ev" if state.values.get("exposure-mode") != Some(&0) => {
            Some("Select Auto exposure to adjust camera brightness".to_owned())
        }
        "exposure-ev" if state.values.get("exposure-lock") == Some(&1) => {
            Some("Unlock exposure to adjust camera brightness".to_owned())
        }
        "exposure-lock" if state.values.get("exposure-mode") != Some(&0) => {
            Some("Available in automatic exposure mode".to_owned())
        }
        "focus-manual" if state.values.get("focus-mode") != Some(&2) => {
            Some("Select manual focus to adjust lens position".to_owned())
        }
        "wb-temp" | "wb-temp-shift" if state.values.get("wb-mode") != Some(&0) => {
            Some("Select manual white balance to adjust this control".to_owned())
        }
        "wb-lock" if state.values.get("wb-mode") != Some(&1) => {
            Some("Available in automatic white balance mode".to_owned())
        }
        "clut-attr" if state.values.get("clut") != Some(&1) => {
            Some("Enable the colour LUT to adjust its intensity".to_owned())
        }
        _ => None,
    }
}

fn preview_mode_reason(
    state: &CameraState,
    capabilities: Option<&PreviewCapabilities>,
    mode: PreviewMode,
) -> Option<String> {
    if let Some(reason) = mode.restriction_reason() {
        return Some(reason);
    }
    if capabilities.is_some_and(|capabilities| !capabilities.supports(mode)) {
        return Some(format!(
            "Saved preview mode {} is unavailable on this video device. Choose an advertised mode; your saved choice has been kept.",
            mode.label()
        ));
    }
    crate::compatibility::preview_reason(state, mode.fps())
}

fn letterboxed_image_rect(bounds: egui::Rect, dimensions: (usize, usize)) -> egui::Rect {
    let aspect = dimensions.0.max(1) as f32 / dimensions.1.max(1) as f32;
    let size = if bounds.width() / bounds.height() > aspect {
        Vec2::new(bounds.height() * aspect, bounds.height())
    } else {
        Vec2::new(bounds.width(), bounds.width() / aspect)
    };
    egui::Rect::from_center_size(bounds.center(), size)
}

fn focus_target_at(
    target: FocusTarget,
    image: egui::Rect,
    point: egui::Pos2,
) -> Option<FocusTarget> {
    if !image.contains(point) || image.width() <= 0.0 || image.height() <= 0.0 {
        return None;
    }
    target
        .moved_to(
            (point.x - image.left()) / image.width(),
            (point.y - image.top()) / image.height(),
        )
        .ok()
}

fn demo_preview_capabilities() -> PreviewCapabilities {
    PreviewCapabilities {
        device: PathBuf::from("/dev/v4l/by-id/usb-YoloCam_S3_DEMO-video-index0"),
        modes: [
            (1280, 720, 30),
            (1280, 720, 60),
            (1920, 1080, 30),
            (1920, 1080, 60),
            (3840, 2160, 30),
        ]
        .map(|(width, height, fps)| PreviewMode {
            width,
            height,
            interval: (1, fps),
        })
        .to_vec(),
        warnings: Vec::new(),
    }
}

fn exposure_display(raw: i32) -> i32 {
    raw - 8
}
fn exposure_wire(step: i32) -> i32 {
    step + 8
}
fn format_exposure_step(step: i32) -> String {
    if step > 0 {
        format!("+{step}")
    } else {
        step.to_string()
    }
}
fn preset_status(message: &str, preset: &Preset) -> String {
    match preset.omission_note() {
        Some(note) => format!("{message}. {note}"),
        None => message.to_owned(),
    }
}

fn panel_title(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.label(RichText::new(title).size(20.0).strong());
    ui.label(RichText::new(description).size(11.0).color(MUTED));
    ui.add_space(6.0);
}

fn page_title(ui: &mut egui::Ui, title: &str, description: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).size(30.0).strong());
    ui.add_space(6.0);
    ui.label(RichText::new(description).size(13.0).color(MUTED));
    ui.add_space(18.0);
}

fn section_title(ui: &mut egui::Ui, title: &str) {
    ui.add_space(15.0);
    ui.label(
        RichText::new(title.to_uppercase())
            .monospace()
            .size(10.0)
            .color(AMBER),
    );
    ui.add_space(3.0);
    ui.separator();
    ui.add_space(7.0);
}

fn accent_button(label: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(label).strong().color(BACKGROUND)).fill(AMBER)
}

fn version_details(ui: &mut egui::Ui, version: &CameraVersion) {
    ui.label(RichText::new(&version.firmware).size(18.0).strong());
    ui.label(
        RichText::new(format!(
            "Build {} · serial {}",
            version.build, version.serial
        ))
        .monospace()
        .size(12.0)
        .color(MUTED),
    );
}

fn phase_label(phase: &str) -> &str {
    match phase {
        "download" => "Downloading official firmware…",
        "verify" => "Verifying package and native image…",
        "prepared" => "Package verified and ready to review",
        "upload" => "Uploading firmware to the camera…",
        "commit" => "Committing the verified image…",
        "flash" => "Camera is installing firmware. Keep USB attached.",
        "reboot" => "Waiting for reboot and verifying the new version…",
        "verified" => "Running firmware verified after reboot",
        _ => phase,
    }
}

fn crop_matches(wanted: &Crop, actual: &Crop) -> bool {
    (wanted.zoom - actual.zoom).abs() < 0.02
        && (wanted.left - actual.left).abs() < 0.002
        && (wanted.top - actual.top).abs() < 0.002
}

fn demo_state() -> CameraState {
    let mut state = CameraState {
        version: Some(CameraVersion {
            serial: "DEMO S3".to_owned(),
            firmware: "YunxiCamera-ch131b-1.0.4".to_owned(),
            build: 1349,
        }),
        ..Default::default()
    };
    for spec in param_specs() {
        if !spec.readback || unsupported_control_reason(spec.key).is_some() {
            continue;
        }
        let value = if !spec.options.is_empty() {
            spec.options[0].0
        } else {
            (spec.min + spec.max) / 2
        };
        state.values.insert(spec.key.to_owned(), value);
    }
    for (key, value) in [
        ("hdr", 0),
        ("exposure-mode", 0),
        ("exposure-ev", 8),
        ("wb-mode", 1),
        ("wb-temp", 5500),
        ("focus-mode", 4),
        ("focus-manual", 67),
        ("sharpness", 5),
        ("hsl-enable", 1),
        ("clut", 0),
    ] {
        state.values.insert(key.to_owned(), value);
    }
    for key in ["brightness", "hue", "gamma"] {
        if let Some(reason) = unsupported_control_reason(key) {
            state.unsupported.insert(key.to_owned(), reason.to_owned());
        }
    }
    for (key, reason) in [
        ("zoom", "Use native framing controls"),
        ("zoom-speed", "Native framing is a direct crop"),
        ("dehaze", "Firmware placeholder"),
        ("shadows", "Firmware placeholder"),
        ("highlights", "Firmware placeholder"),
    ] {
        state.unsupported.insert(key.to_owned(), reason.to_owned());
    }
    state.eptz = Some(Crop::centred(1.0).unwrap());
    state.hsl = Some(HslState::default());
    state.profile = Some(crate::pb::TuningParam::default());
    state.wb_offsets = Some((10, 10));
    state.focus_target = Some(FocusTarget::new(0.3, 0.23, 0.7, 0.77).unwrap());
    state
}

fn draw_demo_pattern(painter: &egui::Painter, rect: egui::Rect) {
    for column in 0..12 {
        let x = rect.left() + rect.width() * column as f32 / 12.0;
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            Stroke::new(1.0_f32, Color32::from_rgb(29, 34, 36)),
        );
    }
    for row in 0..8 {
        let y = rect.top() + rect.height() * row as f32 / 8.0;
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            Stroke::new(1.0_f32, Color32::from_rgb(29, 34, 36)),
        );
    }
    painter.circle_stroke(
        rect.center(),
        rect.height() * 0.24,
        Stroke::new(1.0_f32, LINE),
    );
    painter.line_segment(
        [
            rect.center() - Vec2::new(30.0, 0.0),
            rect.center() + Vec2::new(30.0, 0.0),
        ],
        Stroke::new(1.0_f32, AMBER),
    );
    painter.line_segment(
        [
            rect.center() - Vec2::new(0.0, 30.0),
            rect.center() + Vec2::new(0.0, 30.0),
        ],
        Stroke::new(1.0_f32, AMBER),
    );
    painter.text(
        rect.center() + Vec2::new(0.0, 66.0),
        egui::Align2::CENTER_CENTER,
        "YOLOCAM / LINUX",
        FontId::monospace(12.0),
        MUTED,
    );
    let colours = [
        Color32::from_rgb(199, 196, 182),
        Color32::from_rgb(190, 177, 82),
        Color32::from_rgb(93, 158, 169),
        Color32::from_rgb(111, 153, 104),
        Color32::from_rgb(163, 105, 153),
        Color32::from_rgb(173, 97, 83),
        Color32::from_rgb(101, 119, 171),
    ];
    for (index, colour) in colours.iter().enumerate() {
        let from = egui::pos2(
            rect.left() + rect.width() * index as f32 / colours.len() as f32,
            rect.bottom() - 18.0,
        );
        let to = egui::pos2(
            rect.left() + rect.width() * (index + 1) as f32 / colours.len() as f32,
            rect.bottom(),
        );
        painter.rect_filled(egui::Rect::from_min_max(from, to), 0.0, *colour);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_app() -> App {
        let temp = tempfile::tempdir().unwrap();
        App::new(
            Store::at(temp.path().to_owned()).unwrap(),
            Settings::default(),
            None,
            12345,
            true,
            true,
        )
    }

    #[test]
    fn successful_preview_negotiation_refreshes_once_without_guessing_fps_or_losing_edits() {
        let mut app = sample_app();
        let (worker, commands, events) = Worker::test_channel();
        app.worker = Some(worker);
        app.demo = false;
        app.desired.insert("contrast".to_owned(), 71);
        app.desired_hsl.insert((0, 3), 20);
        app.desired_crop = Some(Crop::centred(1.5).unwrap());
        app.desired_wb = Some((11, 8));
        let desired = app.desired.clone();
        let desired_hsl = app.desired_hsl.clone();
        let desired_crop = app.desired_crop.clone();
        let desired_wb = app.desired_wb;
        let original_fps = app.state.values.get("frame-rate").copied();

        // The same completion handler follows a real successful start_mode.
        app.handle_preview_start(Ok(()));
        assert!(app.preview_attempted);
        assert!(app.preview_error.is_none());
        let queued = commands.try_iter().collect::<Vec<_>>();
        assert_eq!(queued.len(), 1);
        assert!(matches!(queued[0], Command::Refresh));
        assert_eq!(
            app.state.values.get("frame-rate").copied(),
            original_fps,
            "The negotiated preview rate must not become a guessed camera readback"
        );
        assert_eq!(app.desired, desired);
        assert_eq!(app.desired_hsl, desired_hsl);
        assert_eq!(app.desired_crop, desired_crop);
        assert_eq!(app.desired_wb, desired_wb);

        // Repaints after this attempt perform no capture/refresh or SET5.
        for _ in 0..3 {
            app.update_preview(&egui::Context::default());
        }
        assert!(commands.try_recv().is_err());

        // Only a serialized camera State event updates controlling readbacks.
        let mut readback = app.state.clone();
        readback.values.insert("frame-rate".to_owned(), 30);
        let focus = FocusTarget::new(0.2, 0.2, 0.6, 0.74).unwrap();
        readback.focus_target = Some(focus);
        events.send(Event::State(Box::new(readback))).unwrap();
        app.poll_worker();
        assert_eq!(app.state.values.get("frame-rate"), Some(&30));
        assert_eq!(app.state.focus_target, Some(focus));
        assert_eq!(app.desired, desired);
        assert_eq!(app.desired_hsl, desired_hsl);
        assert_eq!(app.desired_crop, desired_crop);
        assert_eq!(app.desired_wb, desired_wb);

        app.handle_preview_start(Err(anyhow::anyhow!("Capture negotiation failed")));
        assert!(
            commands.try_recv().is_err(),
            "Failed starts must not refresh"
        );
        assert!(
            app.preview_error
                .as_ref()
                .unwrap()
                .contains("Capture negotiation failed")
        );
        app.update_preview(&egui::Context::default());
        assert!(commands.try_recv().is_err());
        app.handle_preview_start(Ok(()));
        let restarted = commands.try_iter().collect::<Vec<_>>();
        assert_eq!(
            restarted.len(),
            1,
            "A new successful start requires one new refresh"
        );
        assert!(matches!(restarted[0], Command::Refresh));
    }

    #[test]
    fn post_preview_refresh_queue_failure_is_visible_and_preserves_pending_edits() {
        let mut app = sample_app();
        let (worker, commands, _events) = Worker::test_channel();
        app.worker = Some(worker);
        drop(commands);
        app.desired.insert("contrast".to_owned(), 71);
        app.handle_preview_start(Ok(()));
        assert!(
            app.preview_error
                .as_ref()
                .unwrap()
                .contains("camera readbacks could not be refreshed")
        );
        assert_eq!(app.desired.get("contrast"), Some(&71));
        assert!(
            app.preview_attempted,
            "No automatic refresh/write retry on every repaint"
        );
    }

    #[test]
    fn preview_modes_require_advertised_support_and_confirmed_sdr_above_thirty() {
        let mut state = demo_state();
        let capabilities = demo_preview_capabilities();
        let high = PreviewMode {
            interval: (1, 60),
            ..PreviewMode::default()
        };
        assert!(preview_mode_reason(&state, Some(&capabilities), high).is_none());
        state.values.insert("hdr".to_owned(), 1);
        assert!(
            preview_mode_reason(&state, Some(&capabilities), high)
                .unwrap()
                .contains("HDR")
        );
        assert!(preview_mode_reason(&state, Some(&capabilities), PreviewMode::default()).is_none());
        state.values.remove("hdr");
        assert!(
            preview_mode_reason(&state, Some(&capabilities), high)
                .unwrap()
                .contains("unavailable")
        );
        let unavailable = PreviewMode {
            interval: (1, 50),
            ..PreviewMode::default()
        };
        assert!(
            preview_mode_reason(&state, Some(&capabilities), unavailable)
                .unwrap()
                .contains("saved choice has been kept")
        );
        let four_k = PreviewMode {
            width: 3840,
            height: 2160,
            interval: (1, 60),
        };
        assert!(
            preview_mode_reason(&state, Some(&capabilities), four_k)
                .unwrap()
                .contains("4K")
        );
    }

    #[test]
    fn selected_preview_rate_guards_hdr_controls_and_presets_without_camera_rate_writes() {
        let mut app = sample_app();
        let before = app.state.values.clone();
        let high = PreviewMode {
            interval: (1, 60),
            ..PreviewMode::default()
        };
        app.select_preview_mode(high);
        assert_eq!(app.settings.preview_mode, high);
        assert_eq!(
            app.state.values, before,
            "Preview selection must not change camera-side controls"
        );
        assert!(
            app.requested_control_reason("hdr", 1)
                .unwrap()
                .contains("preview mode at 30 fps")
        );
        assert!(
            app.requested_control_reason("hdr", 0).is_none(),
            "Turning HDR off must remain available"
        );
        assert!(!app.send(Command::SetParam {
            key: "hdr".to_owned(),
            value: 1
        }));
        assert_eq!(app.state.values.get("hdr"), Some(&0));
        let mut preset = Preset::capture("HDR look", &app.state).unwrap();
        preset.values = BTreeMap::from([("hdr".to_owned(), 1)]);
        preset.profile = None;
        assert!(app.preset_reason(&preset).unwrap().contains("HDR preset"));
        assert!(!app.send(Command::ApplyPreset(Box::new(preset))));
        assert_eq!(app.state.values, before);
        app.select_preview_mode(PreviewMode::default());
        assert!(app.requested_control_reason("hdr", 1).is_none());
        app.settings.preview_mode = PreviewMode {
            interval: (333_333, 10_000_000),
            ..PreviewMode::default()
        };
        assert!(
            app.requested_control_reason("hdr", 1).is_none(),
            "Nominal 30 fps interval quantization must not block HDR"
        );
        app.desired.insert("hdr".to_owned(), 1);
        assert!(
            app.preview_choice_reason(app.preview_capabilities.as_ref(), high)
                .unwrap()
                .contains("pending HDR")
        );
        let mut confirmed = app.state.clone();
        confirmed.values.insert("hdr".to_owned(), 1);
        app.handle_worker_event(Event::State(Box::new(confirmed)));
        assert!(!app.desired.contains_key("hdr"));
        assert!(
            app.preview_choice_reason(app.preview_capabilities.as_ref(), high)
                .unwrap()
                .contains("HDR requires")
        );
    }

    #[test]
    fn unavailable_saved_mode_stays_visible_without_fallback_or_control_changes() {
        let mut app = sample_app();
        let saved = PreviewMode {
            width: 2560,
            height: 1440,
            interval: (1, 30),
        };
        app.settings.preview_mode = saved;
        let before = app.state.values.clone();
        let context = egui::Context::default();
        configure_theme(&context);
        let output = context.run(egui::RawInput::default(), |context| {
            egui::CentralPanel::default().show(context, |ui| app.preview_mode_controls(ui));
        });
        let text: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect();
        assert!(
            text.iter().any(|text| text.contains("2560 × 1440")),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|text| text.contains("unavailable on this video device")),
            "{text:?}"
        );
        assert_eq!(app.settings.preview_mode, saved);
        assert_eq!(app.state.values, before);
        assert!(app.desired.is_empty());
    }

    #[test]
    fn focus_coordinates_ignore_letterboxing_and_preserve_confirmed_target_size() {
        let bounds = egui::Rect::from_min_size(egui::pos2(100.0, 80.0), Vec2::new(600.0, 600.0));
        let image = letterboxed_image_rect(bounds, (1920, 1080));
        let target = FocusTarget::new(0.3, 0.23, 0.7, 0.77).unwrap();
        assert!(focus_target_at(target, image, bounds.left_top()).is_none());
        let centered = focus_target_at(target, image, image.center()).unwrap();
        assert!(centered.approximately_equal(&target));
        let edge = focus_target_at(target, image, image.right_bottom()).unwrap();
        assert!((edge.width() - target.width()).abs() < 0.00001);
        assert!((edge.height() - target.height()).abs() < 0.00001);
        assert_eq!((edge.right, edge.bottom), (1.0, 1.0));
    }

    #[test]
    fn copying_a_stalled_mailbox_frame_does_not_make_its_focus_coordinates_fresh() {
        let mut app = sample_app();
        app.state.values.insert("focus-mode".to_owned(), 1);
        app.demo = false;
        let captured_at = Instant::now() - Duration::from_secs(3);
        app.display_preview_frame(
            &egui::Context::default(),
            PreviewFrame {
                width: 1,
                height: 1,
                rgb: vec![255, 255, 255],
                sequence: 1,
                fps: 30.0,
                captured_at,
            },
        );
        assert_eq!(app.frame_captured_at, Some(captured_at));
        assert!(app.frame_captured_at.unwrap().elapsed() > Duration::from_secs(2));
        assert!(app.preview_focus_reason().unwrap().contains("fresh live"));
    }

    #[test]
    fn disconnected_focus_geometry_is_cleared_and_requires_new_connected_readback() {
        let mut app = sample_app();
        let original = app.state.focus_target;
        assert!(original.is_some());
        app.focus_pending = true;
        app.focus_draft = original;
        app.handle_worker_event(Event::Connection(false));
        assert!(app.state.focus_target.is_none());
        assert!(app.focus_draft.is_none());
        assert!(!app.focus_pending);
        app.handle_worker_event(Event::State(Box::new(demo_state())));
        assert!(
            app.state.focus_target.is_none(),
            "Disconnected cached State events must not restore old geometry"
        );
        app.handle_worker_event(Event::Connection(true));
        assert!(
            app.state.focus_target.is_none(),
            "A recovered socket alone is not a new focus readback"
        );
        app.handle_worker_event(Event::State(Box::new(demo_state())));
        assert_eq!(app.state.focus_target, original);
    }

    #[test]
    fn focus_gesture_waits_until_release_and_disabled_states_cancel_drafts() {
        let mut app = sample_app();
        app.state.values.insert("focus-mode".to_owned(), 1);
        let original = app.state.focus_target.unwrap();
        let context = egui::Context::default();
        let image = egui::Rect::from_min_size(egui::pos2(100.0, 100.0), Vec2::new(640.0, 360.0));
        let render = |app: &mut App, events| {
            let _ = context.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(1000.0, 700.0),
                    )),
                    events,
                    ..Default::default()
                },
                |context| {
                    egui::CentralPanel::default().show(context, |ui| {
                        let painter = ui.painter().clone();
                        app.focus_overlay(ui, &painter, Some(image));
                    });
                },
            );
        };
        render(&mut app, vec![]);
        let pressed = egui::pos2(320.0, 250.0);
        render(
            &mut app,
            vec![
                egui::Event::PointerMoved(pressed),
                egui::Event::PointerButton {
                    pos: pressed,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        let moved = egui::pos2(550.0, 280.0);
        render(&mut app, vec![egui::Event::PointerMoved(moved)]);
        assert_eq!(
            app.state.focus_target,
            Some(original),
            "Dragging must not write the camera"
        );
        assert!(app.focus_draft.is_some());
        render(
            &mut app,
            vec![egui::Event::PointerButton {
                pos: moved,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert_eq!(
            app.state.focus_target,
            focus_target_at(original, image, moved)
        );
        assert!(app.focus_draft.is_none());
        let confirmed = app.state.focus_target;
        app.focus_draft = Some(original);
        app.settings.preview_enabled = false;
        render(&mut app, vec![]);
        assert!(app.focus_draft.is_none());
        assert_eq!(app.state.focus_target, confirmed);
        assert!(
            app.preview_focus_reason()
                .unwrap()
                .contains("Start preview")
        );
        app.settings.preview_enabled = true;
        app.desired.insert("focus-level".to_owned(), 2);
        assert!(app.preview_focus_reason().unwrap().contains("Waiting"));
        app.desired.clear();
        app.busy = true;
        assert!(app.preview_focus_reason().unwrap().contains("firmware"));
        app.busy = false;
        app.demo = false;
        app.frame_captured_at = Some(Instant::now() - Duration::from_secs(3));
        assert!(app.preview_focus_reason().unwrap().contains("fresh live"));
        app.handle_worker_event(Event::Status {
            message: "Focus acknowledgement was lost; command was not replayed".to_owned(),
            error: true,
        });
        assert_eq!(
            app.state.focus_target, confirmed,
            "A rejected action must not invent a new confirmed target"
        );
        assert!(!app.focus_pending);
    }

    #[test]
    fn control_connection_error_survives_reconnect_progress_and_clears_after_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        let mut app = App::new(store, Settings::default(), None, 12345, true, true);
        // Use sample state without creating a real worker or preview device.
        app.demo = false;
        let error = "The camera closed the control connection; command was not replayed";
        app.handle_worker_event(Event::ConnectionError(error.to_owned()));
        app.handle_worker_event(Event::Status {
            message: error.to_owned(),
            error: true,
        });
        app.handle_worker_event(Event::Connection(false));
        app.handle_worker_event(Event::Status {
            message: "Connecting to YoloCam…".to_owned(),
            error: false,
        });
        assert_eq!(app.displayed_status(), (error, true));
        assert!(app.can_reconnect());
        assert!(control_reason(&app.state, "contrast", app.connected, app.busy).is_some());
        let context = egui::Context::default();
        configure_theme(&context);
        for tab in [Tab::Camera, Tab::Colour] {
            app.tab = tab;
            let output = context.run(egui::RawInput::default(), |context| {
                egui::CentralPanel::default().show(context, |ui| app.footer(ui));
            });
            let labels: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                    _ => None,
                })
                .collect();
            assert!(labels.iter().any(|label| label == error), "{labels:?}");
            assert!(
                labels.iter().any(|label| label == "Reconnect"),
                "{labels:?}"
            );
        }
        app.handle_worker_event(Event::Connection(true));
        app.handle_worker_event(Event::Status {
            message: "Camera connected".to_owned(),
            error: false,
        });
        assert!(app.control_connection_error.is_none());
        assert_eq!(app.displayed_status(), ("Camera connected", false));
        assert!(control_reason(&app.state, "contrast", app.connected, app.busy).is_none());
        assert!(
            app.settings.preview_enabled,
            "Control recovery preserves preview choice"
        );
        assert!(app.worker.is_none());
        assert!(!app.preview.is_running());
    }

    #[test]
    fn reconnect_action_cannot_interrupt_firmware_or_bypass_reboot_verification() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        let mut app = App::new(store, Settings::default(), None, 12345, false, true);
        app.demo = false;
        app.connected = false;
        app.control_connection_error = Some("Previous control error".to_owned());
        app.status = "Downloading verified firmware".to_owned();
        app.handle_worker_event(Event::Busy(true));
        assert!(!app.can_reconnect());
        assert_eq!(
            app.displayed_status(),
            ("Downloading verified firmware", false)
        );
        app.handle_worker_event(Event::Busy(false));
        app.installing = true;
        assert!(!app.can_reconnect());
        app.handle_worker_event(Event::FirmwareFailed {
            message: "Firmware verification is pending".to_owned(),
            uncertain: true,
        });
        assert!(!app.can_reconnect());
        assert_eq!(
            app.displayed_status(),
            ("Firmware verification is pending", true)
        );
        app.uncertain_update = false;
        assert!(app.can_reconnect());
    }

    #[test]
    fn exposure_steps_map_native_endpoints_and_neutral_without_stop_conversion() {
        assert_eq!(
            (
                exposure_display(0),
                exposure_display(8),
                exposure_display(16)
            ),
            (-8, 0, 8)
        );
        for raw in 0..=16 {
            assert_eq!(exposure_wire(exposure_display(raw)), raw);
        }
        assert_eq!(format_exposure_step(-8), "-8");
        assert_eq!(format_exposure_step(0), "0");
        assert_eq!(format_exposure_step(8), "+8");
    }

    #[test]
    fn exposure_compensation_requires_auto_and_unlocked_exposure() {
        let mut state = demo_state();
        assert!(!state.values.contains_key("brightness"));
        assert!(state.unsupported.contains_key("brightness"));
        assert!(control_reason(&state, "exposure-ev", true, false).is_none());
        state.values.insert("exposure-lock".to_owned(), 1);
        assert!(
            control_reason(&state, "exposure-ev", true, false)
                .unwrap()
                .contains("Unlock exposure")
        );
        state.values.insert("exposure-mode".to_owned(), 1);
        assert!(
            control_reason(&state, "exposure-ev", true, false)
                .unwrap()
                .contains("Select Auto")
        );
    }

    #[test]
    fn locked_compensation_reason_and_signed_value_are_visible_without_startup_writes() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        let mut app = App::new(store, Settings::default(), None, 12345, true, true);
        app.state.values.insert("exposure-ev".to_owned(), 16);
        app.state.values.insert("exposure-lock".to_owned(), 1);
        let context = egui::Context::default();
        configure_theme(&context);
        let output = context.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(1240.0, 800.0),
                )),
                ..Default::default()
            },
            |context| {
                egui::CentralPanel::default().show(context, |ui| app.camera_panel(ui));
            },
        );
        let labels: Vec<String> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_owned()),
                _ => None,
            })
            .collect();
        assert!(
            labels
                .iter()
                .any(|label| label.contains("Unlock exposure to adjust camera brightness")),
            "{labels:?}"
        );
        assert!(labels.iter().any(|label| label == "+8"), "{labels:?}");
        assert_eq!(app.state.values.get("exposure-ev"), Some(&16));
        assert_eq!(app.state.values.get("exposure-lock"), Some(&1));
        assert!(app.desired.is_empty());
    }

    #[test]
    fn automatic_modes_disable_manual_controls_and_unknown_controls_stay_disabled() {
        let state = demo_state();
        for key in [
            "exposure-iso",
            "exposure-time",
            "exposure-aperture",
            "focus-manual",
            "wb-temp",
            "wb-temp-shift",
            "hue",
            "gamma",
            "dehaze",
        ] {
            assert!(control_reason(&state, key, true, false).is_some(), "{key}");
        }
        for key in [
            "contrast",
            "saturation",
            "hdr",
            "exposure-mode",
            "wb-mode",
            "focus-mode",
        ] {
            assert!(control_reason(&state, key, true, false).is_none(), "{key}");
        }
    }

    #[test]
    fn manual_modes_expose_their_supported_controls_and_updater_disables_everything() {
        let mut state = demo_state();
        state.values.insert("exposure-mode".to_owned(), 1);
        state.values.insert("focus-mode".to_owned(), 2);
        state.values.insert("wb-mode".to_owned(), 0);
        for key in ["exposure-iso", "exposure-time", "focus-manual", "wb-temp"] {
            assert!(control_reason(&state, key, true, false).is_none(), "{key}");
        }
        for spec in param_specs() {
            assert!(control_reason(&state, spec.key, true, true).is_some());
        }
    }

    #[test]
    fn all_navigation_views_render_without_camera_io() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::at(temp.path().to_owned()).unwrap();
        store
            .save_preset(&Preset::capture("Private local preset", &demo_state()).unwrap())
            .unwrap();
        let mut app = App::new(store, Settings::default(), None, 12345, true, true);
        assert_eq!(
            app.presets
                .iter()
                .map(|preset| preset.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Daylight", "Warm room"]
        );
        app.state.values.insert("sharpness".to_owned(), 24);
        let ctx = egui::Context::default();
        configure_theme(&ctx);
        for tab in [
            Tab::Camera,
            Tab::Colour,
            Tab::Firmware,
            Tab::Presets,
            Tab::Device,
        ] {
            app.tab = tab;
            let output = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        Vec2::new(1240.0, 800.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| match tab {
                        Tab::Camera => app.camera_panel(ui),
                        Tab::Colour => app.colour_panel(ui),
                        Tab::Firmware => app.firmware_page(ui),
                        Tab::Presets => app.presets_page(ui),
                        Tab::Device => app.device_page(ui),
                    });
                },
            );
            assert!(!output.shapes.is_empty());
            assert_eq!(
                app.state.values.get("sharpness"),
                Some(&24),
                "Rendering must never alter an out-of-range camera readback"
            );
        }
        assert!(app.worker.is_none());
        assert!(!app.preview.is_running());
    }
}
