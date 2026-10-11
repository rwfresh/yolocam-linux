// SPDX-License-Identifier: GPL-3.0-or-later
//! Native desktop camera console. All camera I/O runs in the serialized worker.

use anyhow::{Context, Result};
use eframe::egui::{self, Color32, FontId, RichText, Stroke, TextureHandle, Vec2};
use std::{collections::BTreeMap, net::Ipv4Addr, path::PathBuf, time::Duration};

use crate::{
    camera::{CameraState, CameraVersion, Crop, HslState, ParamSpec, param_specs},
    firmware::FirmwareRelease,
    preview::Preview,
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
    desired: BTreeMap<String, i32>,
    write_only_choices: BTreeMap<String, i32>,
    desired_hsl: BTreeMap<(usize, usize), i32>,
    desired_crop: Option<Crop>,
    desired_wb: Option<(u32, u32)>,
    preview: Preview,
    preview_attempted: bool,
    texture: Option<TextureHandle>,
    frame_sequence: u64,
    frame_dimensions: (usize, usize),
    frame_fps: f32,
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
            desired: BTreeMap::new(),
            write_only_choices: BTreeMap::new(),
            desired_hsl: BTreeMap::new(),
            desired_crop: None,
            desired_wb: None,
            preview: Preview::new(),
            preview_attempted: false,
            texture: None,
            frame_sequence: 0,
            frame_dimensions: (0, 0),
            frame_fps: 0.0,
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
        if self.demo {
            match command {
                Command::SetParam { key, value } => {
                    self.state.values.insert(key, value);
                }
                Command::SetCrop(crop) => self.state.eptz = Some(crop),
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
            match event {
                Event::Connection(connected) => self.connected = connected,
                Event::State(state) => {
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
                    }
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
                        self.status = if preset.omitted_controls.is_empty() {
                            format!("Saved preset {} from camera readbacks", preset.name)
                        } else {
                            format!(
                                "Saved preset {}; omitted out-of-range controls: {}",
                                preset.name,
                                preset
                                    .omitted_controls
                                    .keys()
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        };
                        self.status_error = false;
                        self.reload_presets();
                    }
                    Err(error) => self.error(format!("{error:#}")),
                },
            }
        }
    }

    fn reload_presets(&mut self) {
        match self.store.presets() {
            Ok(presets) => self.presets = presets,
            Err(error) => self.error(format!("{error:#}")),
        }
    }

    fn toggle_preview(&mut self) {
        if self.installing || self.uncertain_update {
            return;
        }
        self.settings.preview_enabled = !self.settings.preview_enabled;
        self.preview_attempted = false;
        if !self.settings.preview_enabled {
            self.preview.stop();
            self.texture = None;
        }
        self.save_preferences();
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        if self.demo || self.installing || self.uncertain_update {
            return;
        }
        if self.settings.preview_enabled && self.connected && !self.preview_attempted {
            self.preview_attempted = true;
            if let Err(error) = self.preview.start(self.settings.preview_device.clone()) {
                self.error(format!("Preview: {error:#}"));
            }
        }
        if let Some(error) = self.preview.error() {
            if self.preview.is_running() {
                self.preview.stop();
            }
            let text = format!("Preview: {error}");
            if self.status != text {
                self.error(text);
            }
        }
        if let Some(frame) = self.preview.latest()
            && frame.sequence != self.frame_sequence
        {
            self.frame_sequence = frame.sequence;
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

    fn footer(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if self.status_error { "!" } else { "·" })
                    .strong()
                    .color(if self.status_error { ERROR } else { AMBER }),
            );
            ui.add(
                egui::Label::new(
                    RichText::new(&self.status)
                        .size(12.0)
                        .color(if self.status_error { ERROR } else { MUTED }),
                )
                .wrap(),
            );
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
                                    changed |=
                                        ui.selectable_value(&mut value, *number, *label).changed();
                                }
                            });
                    });
                }
            });
            if spec.options.is_empty() {
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
        .on_hover_text(reason.unwrap_or_default());
        if actual.is_none() && !self.busy && self.connected {
            ui.label(
                RichText::new("Unavailable on this camera")
                    .size(10.0)
                    .color(MUTED),
            );
        }
        if actual.is_some_and(|value| value < spec.min || value > maximum) {
            ui.label(
                RichText::new(format!(
                    "Camera currently reports {}. New values must be {}…{maximum}.",
                    actual.unwrap(),
                    spec.min
                ))
                .size(10.0)
                .color(AMBER),
            );
        }
        ui.add_space(8.0);
        if changed
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
                        !self.installing && !self.uncertain_update,
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
        ui.add_space(10.0);
        let width = ui.available_width();
        let height = (width * 9.0 / 16.0).min((ui.available_height() - 190.0).max(180.0));
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), egui::Sense::hover());
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 4.0, Color32::from_rgb(11, 13, 14));
        if self.settings.preview_enabled && self.demo {
            draw_demo_pattern(&painter, rect);
        } else if self.settings.preview_enabled
            && let Some(texture) = &self.texture
        {
            let aspect = self.frame_dimensions.0 as f32 / self.frame_dimensions.1.max(1) as f32;
            let size = if rect.width() / rect.height() > aspect {
                Vec2::new(rect.height() * aspect, rect.height())
            } else {
                Vec2::new(rect.width(), rect.width() / aspect)
            };
            let image_rect = egui::Rect::from_center_size(rect.center(), size);
            painter.image(
                texture.id(),
                image_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        } else {
            let message = if self.installing {
                "PREVIEW RELEASED FOR FIRMWARE UPDATE"
            } else if self.uncertain_update {
                "WAITING FOR CAMERA VERIFICATION"
            } else if self.settings.preview_enabled && self.preview_attempted {
                "PREVIEW UNAVAILABLE"
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
            if self.preview.is_running() {
                ui.label(
                    RichText::new(format!(
                        "{} × {} · {:.1} fps",
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
                            self.connected && !self.busy && !self.demo,
                            egui::Button::new("Apply"),
                        )
                        .clicked()
                    {
                        self.send(Command::ApplyPreset(Box::new(preset.clone())));
                    }
                });
            });
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
                    let preset: Preset = serde_json::from_slice(&bytes)?;
                    self.store.save_preset(&preset)?;
                    Ok(preset)
                })();
                match result {
                    Ok(preset) => {
                        self.selected_preset = Some(preset.name);
                        self.reload_presets();
                        self.status =
                            "Preset imported locally. Apply it to change the camera.".to_owned();
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
                    !self.busy && !self.demo && !self.uncertain_update,
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
            Duration::from_millis(33)
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
        "exposure-ev" | "exposure-lock" if state.values.get("exposure-mode") != Some(&0) => {
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
        if !spec.readback {
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
    for (key, reason) in [
        ("hue", "Global hue does not process the image; use HSL"),
        ("gamma", "Gamma is unavailable on S3"),
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
