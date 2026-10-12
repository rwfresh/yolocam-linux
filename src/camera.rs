// SPDX-License-Identifier: GPL-3.0-or-later
// Protocol/control definitions based on yolocam-linux (Seth Hillbrand, 2026).
//! Firmware-aware camera controls, with readback verification and UI units.

use crate::{pb, protocol::Client};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use pb::command_body::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CameraVersion {
    pub serial: String,
    pub firmware: String,
    pub build: u32,
}

impl CameraVersion {
    fn semantic_version(&self) -> Option<(u32, u32, u32)> {
        let version = self.firmware.rsplit('-').next().unwrap_or("");
        let parts: Option<Vec<u32>> = version.split('.').map(|part| part.parse().ok()).collect();
        parts
            .filter(|parts| parts.len() == 3)
            .map(|parts| (parts[0], parts[1], parts[2]))
    }
    pub fn uses_eptz(&self) -> bool {
        self.semantic_version()
            .is_some_and(|version| version >= (0, 0, 8))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Crop {
    pub zoom: f32,
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Crop {
    pub fn centred(zoom: f32) -> Result<Self> {
        ensure!(
            zoom.is_finite() && (1.0..=4.0).contains(&zoom),
            "zoom must be between 1 and 4"
        );
        let edge = (1.0 - 1.0 / zoom) / 2.0;
        Self::new(zoom, edge, edge)
    }
    pub fn new(zoom: f32, left: f32, top: f32) -> Result<Self> {
        ensure!(
            zoom.is_finite() && (1.0..=4.0).contains(&zoom),
            "zoom must be between 1 and 4"
        );
        let extent = 1.0 / zoom;
        ensure!(
            left.is_finite()
                && top.is_finite()
                && left >= 0.0
                && top >= 0.0
                && left <= 1.0 - extent + 1e-6
                && top <= 1.0 - extent + 1e-6,
            "pan/tilt crop lies outside the image"
        );
        Ok(Self {
            zoom,
            left,
            top,
            right: left + extent,
            bottom: top + extent,
        })
    }
    fn validate(&self) -> Result<()> {
        let expected = Self::new(self.zoom, self.left, self.top)?;
        ensure!(
            self.right.is_finite()
                && self.bottom.is_finite()
                && (expected.right - self.right).abs() < 1e-4
                && (expected.bottom - self.bottom).abs() < 1e-4,
            "crop width and height must match its zoom"
        );
        Ok(())
    }
    fn from_pb(value: &pb::Eptz) -> Result<Self> {
        let crop = Self {
            zoom: value.zoom,
            left: value.left,
            top: value.top,
            right: value.right,
            bottom: value.bottom,
        };
        crop.validate()?;
        Ok(crop)
    }
    fn to_pb(&self) -> pb::Eptz {
        pb::Eptz {
            zoom: self.zoom,
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }
    fn approximately_equal(&self, other: &Self) -> bool {
        [
            self.zoom - other.zoom,
            self.left - other.left,
            self.top - other.top,
            self.right - other.right,
            self.bottom - other.bottom,
        ]
        .iter()
        .all(|delta| delta.abs() < 1e-3)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HslState {
    pub values: [[i32; 8]; 3],
}

impl HslState {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.values
                .iter()
                .flatten()
                .all(|value| (-100..=100).contains(value)),
            "HSL adjustments must be between -100 and 100"
        );
        Ok(())
    }
    fn from_pb(value: &pb::HslAdjust) -> Result<Self> {
        let strings = [
            [
                &value.hue_red,
                &value.hue_orange,
                &value.hue_yellow,
                &value.hue_green,
                &value.hue_cyan,
                &value.hue_blue,
                &value.hue_purple,
                &value.hue_magenta,
            ],
            [
                &value.saturation_red,
                &value.saturation_orange,
                &value.saturation_yellow,
                &value.saturation_green,
                &value.saturation_cyan,
                &value.saturation_blue,
                &value.saturation_purple,
                &value.saturation_magenta,
            ],
            [
                &value.luminance_red,
                &value.luminance_orange,
                &value.luminance_yellow,
                &value.luminance_green,
                &value.luminance_cyan,
                &value.luminance_blue,
                &value.luminance_purple,
                &value.luminance_magenta,
            ],
        ];
        let mut state = Self::default();
        for (axis, colors) in strings.iter().enumerate() {
            for (color, text) in colors.iter().enumerate() {
                state.values[axis][color] = if text.is_empty() {
                    0
                } else {
                    text.parse().context("camera returned invalid HSL value")?
                };
            }
        }
        state.validate()?;
        Ok(state)
    }
    fn to_pb(&self) -> pb::HslAdjust {
        macro_rules! fields { ($($field:ident:$axis:literal:$color:literal),* $(,)?) => {
            pb::HslAdjust { $($field: self.values[$axis][$color].to_string(),)* }
        }; }
        fields! {
            hue_red:0:0, hue_orange:0:1, hue_yellow:0:2, hue_green:0:3,
            hue_cyan:0:4, hue_blue:0:5, hue_purple:0:6, hue_magenta:0:7,
            saturation_red:1:0, saturation_orange:1:1, saturation_yellow:1:2, saturation_green:1:3,
            saturation_cyan:1:4, saturation_blue:1:5, saturation_purple:1:6, saturation_magenta:1:7,
            luminance_red:2:0, luminance_orange:2:1, luminance_yellow:2:2, luminance_green:2:3,
            luminance_cyan:2:4, luminance_blue:2:5, luminance_purple:2:6, luminance_magenta:2:7,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CameraState {
    pub version: Option<CameraVersion>,
    /// Actual readbacks, except write-only ParamSpecs where this records only a
    /// successfully acknowledged choice. Unqueried write-only values are absent.
    pub values: BTreeMap<String, i32>,
    pub unsupported: BTreeMap<String, String>,
    pub eptz: Option<Crop>,
    pub hsl: Option<HslState>,
    pub presets: BTreeMap<u8, Crop>,
    pub profile: Option<pb::TuningParam>,
    pub profile_info: Option<pb::TuningFileInfo>,
    pub wb_offsets: Option<(u32, u32)>,
}

#[derive(Clone, Debug)]
pub struct ParamSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub section: &'static str,
    pub cid: u32,
    pub min: i32,
    pub max: i32,
    pub options: &'static [(i32, &'static str)],
    pub unit: &'static str,
    /// False for commands whose current state is not exposed by firmware.
    pub readback: bool,
}

/// Retain legacy specifications for preset migration, but never use commands
/// that acknowledge changes without processing the image on tested S3 firmware.
pub fn unsupported_control_reason(key: &str) -> Option<&'static str> {
    match key {
        "brightness" => Some(
            "Legacy brightness is not functional on S3 firmware 1.0.4; use exposure compensation in Auto mode",
        ),
        "hue" => Some("Global hue is not functional on S3; use HSL"),
        "gamma" => Some("Gamma is not functional on S3"),
        _ => None,
    }
}

pub fn param_specs() -> &'static [ParamSpec] {
    macro_rules! param {
        ($key:literal, $label:literal, $section:literal, $cid:literal, $min:literal, $max:literal) => {
            ParamSpec {
                key: $key,
                label: $label,
                section: $section,
                cid: $cid,
                min: $min,
                max: $max,
                options: &[],
                unit: "",
                readback: $cid != 214 && $cid != 79,
            }
        };
        ($key:literal, $label:literal, $section:literal, $cid:literal, $min:literal, $max:literal, $options:expr, $unit:literal) => {
            ParamSpec {
                key: $key,
                label: $label,
                section: $section,
                cid: $cid,
                min: $min,
                max: $max,
                options: $options,
                unit: $unit,
                readback: $cid != 214 && $cid != 79,
            }
        };
    }
    const SWITCH: &[(i32, &str)] = &[(0, "Off"), (1, "On")];
    static SPECS: &[ParamSpec] = &[
        // Command 7 is a success-returning firmware stub. Its old range is
        // retained only so presets from 0.1.0 can be migrated without data loss.
        param!("brightness", "Legacy brightness", "Unsupported", 7, 0, 100),
        param!("exposure-ev", "Exposure compensation", "Image", 15, 0, 16),
        param!("contrast", "Contrast", "Image", 9, 0, 100),
        param!("saturation", "Saturation", "Image", 10, 0, 100),
        param!("sharpness", "Sharpness", "Image", 8, -10, 10),
        param!("hue", "Global hue", "Image", 11, 0, 100),
        param!("gamma", "Gamma", "Image", 12, 0, 100),
        param!("hdr", "HDR", "Exposure", 98, 0, 1, SWITCH, ""),
        param!(
            "exposure-mode",
            "Exposure mode",
            "Exposure",
            14,
            0,
            1,
            &[(0, "Auto"), (1, "Manual")],
            ""
        ),
        param!(
            "anti-flicker",
            "Anti-flicker",
            "Exposure",
            16,
            0,
            2,
            &[(0, "Off"), (1, "50 Hz"), (2, "60 Hz")],
            ""
        ),
        param!(
            "exposure-iso",
            "ISO",
            "Exposure",
            17,
            100,
            12800,
            &[
                (100, "100"),
                (125, "125"),
                (160, "160"),
                (200, "200"),
                (250, "250"),
                (320, "320"),
                (400, "400"),
                (500, "500"),
                (640, "640"),
                (800, "800"),
                (1000, "1000"),
                (1250, "1250"),
                (1600, "1600"),
                (2000, "2000"),
                (2500, "2500"),
                (3200, "3200"),
                (4000, "4000"),
                (5000, "5000"),
                (6400, "6400"),
                (8000, "8000"),
                (9600, "9600"),
                (12800, "12800")
            ],
            ""
        ),
        param!(
            "exposure-time",
            "Shutter",
            "Exposure",
            18,
            125,
            40000,
            &[
                (40000, "1/25"),
                (33333, "1/30"),
                (20000, "1/50"),
                (16666, "1/60"),
                (12500, "1/80"),
                (10000, "1/100"),
                (8000, "1/125"),
                (6250, "1/160"),
                (5000, "1/200"),
                (4000, "1/250"),
                (3125, "1/320"),
                (2500, "1/400"),
                (2000, "1/500"),
                (1562, "1/640"),
                (1250, "1/800"),
                (1000, "1/1000"),
                (800, "1/1250"),
                (625, "1/1600"),
                (500, "1/2000"),
                (400, "1/2500"),
                (312, "1/3200"),
                (250, "1/4000"),
                (200, "1/5000"),
                (156, "1/6400"),
                (125, "1/8000")
            ],
            "s"
        ),
        param!(
            "exposure-aperture",
            "Aperture",
            "Exposure",
            20,
            0,
            31,
            &[
                (0, "f/1.0"),
                (1, "f/1.1"),
                (2, "f/1.3"),
                (3, "f/1.4"),
                (4, "f/1.6"),
                (5, "f/1.7"),
                (6, "f/1.8"),
                (7, "f/2.0"),
                (8, "f/2.2"),
                (9, "f/2.5"),
                (10, "f/2.8"),
                (11, "f/3.2"),
                (12, "f/3.6"),
                (13, "f/4.0"),
                (14, "f/4.5"),
                (15, "f/5.0"),
                (16, "f/5.7"),
                (17, "f/6.3"),
                (18, "f/7.1"),
                (19, "f/8.0"),
                (20, "f/9.0"),
                (21, "f/10.1"),
                (22, "f/11.3"),
                (23, "f/12.7"),
                (24, "f/14.3"),
                (25, "f/16.0"),
                (26, "f/18.0"),
                (27, "f/20.2"),
                (28, "f/22.6"),
                (29, "f/25.4"),
                (30, "f/28.5"),
                (31, "f/32.0")
            ],
            ""
        ),
        param!(
            "exposure-lock",
            "Exposure lock",
            "Exposure",
            106,
            0,
            1,
            SWITCH,
            ""
        ),
        param!("dynamic-range", "Dynamic range", "Exposure", 102, 0, 100),
        param!(
            "wb-mode",
            "White balance",
            "White Balance",
            21,
            0,
            1,
            &[(0, "Manual"), (1, "Auto")],
            ""
        ),
        param!(
            "wb-temp",
            "Temperature",
            "White Balance",
            22,
            2000,
            10000,
            &[],
            "K"
        ),
        param!(
            "wb-temp-shift",
            "Temperature shift",
            "White Balance",
            105,
            0,
            128
        ),
        param!(
            "wb-lock",
            "White balance lock",
            "White Balance",
            104,
            0,
            1,
            SWITCH,
            ""
        ),
        param!(
            "focus-mode",
            "Focus mode",
            "Focus",
            23,
            1,
            4,
            &[(1, "AF-S"), (2, "Manual"), (3, "AF-C"), (4, "Face")],
            ""
        ),
        param!(
            "focus-level",
            "Focus area",
            "Focus",
            38,
            0,
            2,
            &[(0, "Large"), (1, "Medium"), (2, "Small")],
            ""
        ),
        param!("focus-manual", "Manual focus", "Focus", 111, 0, 100),
        param!(
            "focus-caf",
            "Continuous autofocus",
            "Focus",
            42,
            0,
            1,
            SWITCH,
            ""
        ),
        param!(
            "focus-trigger",
            "Autofocus speed",
            "Focus",
            47,
            0,
            1,
            &[(0, "Basic"), (1, "Quick")],
            ""
        ),
        param!(
            "face-roi",
            "Face exposure region",
            "Focus",
            214,
            0,
            1,
            SWITCH,
            ""
        ),
        param!(
            "ai-tracking",
            "AI tracking",
            "Tracking",
            99,
            0,
            2,
            &[(0, "Portrait"), (1, "Object"), (2, "Off")],
            ""
        ),
        param!(
            "hsl-enable",
            "HSL adjustments",
            "Colour",
            128,
            0,
            1,
            SWITCH,
            ""
        ),
        param!("clut", "Colour LUT", "Colour", 94, 0, 1, SWITCH, ""),
        param!("clut-attr", "LUT intensity", "Colour", 96, 0, 255),
        param!("zoom", "Legacy zoom", "Zoom", 13, 50, 98),
        param!("zoom-speed", "Legacy zoom speed", "Zoom", 112, 1, 5),
        param!(
            "frame-rate",
            "Camera frame rate",
            "Video",
            5,
            25,
            60,
            &[
                (25, "25 fps"),
                (30, "30 fps"),
                (50, "50 fps"),
                (60, "60 fps")
            ],
            ""
        ),
        param!(
            "distortion",
            "Distortion correction",
            "Video",
            26,
            0,
            1,
            SWITCH,
            ""
        ),
        param!(
            "shading",
            "Lens shading correction",
            "Video",
            27,
            0,
            1,
            SWITCH,
            ""
        ),
        param!(
            "hdmi-format",
            "HDMI format",
            "HDMI",
            61,
            0,
            3,
            &[(0, "Auto"), (1, "4K"), (2, "1080p"), (3, "720p")],
            ""
        ),
        param!(
            "hdmi-color",
            "HDMI colour space",
            "HDMI",
            65,
            0,
            2,
            &[(0, "Auto"), (1, "RGB"), (2, "YUV422")],
            ""
        ),
        param!("au-volume", "Audio volume", "Audio", 73, 0, 100),
        param!(
            "au-denoise",
            "Noise reduction",
            "Audio",
            75,
            0,
            3,
            &[(0, "Off"), (1, "Weak"), (2, "Medium"), (3, "Strong")],
            ""
        ),
        param!(
            "au-gain",
            "Automatic audio gain",
            "Audio",
            78,
            0,
            1,
            SWITCH,
            ""
        ),
        param!(
            "au-mic-type",
            "Audio input",
            "Audio",
            79,
            0,
            1,
            &[(0, "Line"), (1, "Microphone")],
            ""
        ),
        param!(
            "au-reverb",
            "Reverb",
            "Audio",
            76,
            0,
            3,
            &[(0, "Off"), (1, "Weak"), (2, "Medium"), (3, "Strong")],
            ""
        ),
        param!("au-delay", "Audio delay", "Audio", 77, 0, 400, &[], "ms"),
    ];
    SPECS
}

pub struct Camera {
    pub client: Client,
    pub state: CameraState,
}

impl Camera {
    pub fn new(ip: Option<Ipv4Addr>, port: u16, timeout: Duration) -> Self {
        Self {
            client: Client::new(ip, port, timeout),
            state: CameraState::default(),
        }
    }
    pub fn connect(&mut self) -> Result<CameraState> {
        self.client.connect()?;
        self.refresh()
    }
    pub fn disconnect(&mut self) {
        self.client.disconnect();
    }
    pub fn heartbeat(&mut self) -> Result<()> {
        accepted(self.client.set_int(215, 1)?)?;
        Ok(())
    }
    pub fn version(&mut self) -> Result<CameraVersion> {
        let message = accepted(self.client.get(1)?)?;
        match body(message)? {
            Value::VersionInfo(info) => {
                let version = CameraVersion {
                    serial: info.serial_num,
                    firmware: info.sys_version,
                    build: info
                        .sys_code
                        .parse()
                        .context("camera returned invalid firmware build number")?,
                };
                self.state.version = Some(version.clone());
                Ok(version)
            }
            _ => bail!("camera returned no version information"),
        }
    }

    pub fn refresh(&mut self) -> Result<CameraState> {
        let version = self.version()?;
        self.state = CameraState {
            version: Some(version),
            ..Default::default()
        };
        for spec in param_specs() {
            if let Some(reason) = unsupported_control_reason(spec.key) {
                self.state
                    .unsupported
                    .insert(spec.key.into(), reason.into());
            }
        }
        for name in ["dehaze", "shadows", "highlights"] {
            self.state.unsupported.insert(
                name.into(),
                "Firmware placeholder; does not process the image".into(),
            );
        }
        let timeout = self.client.timeout();
        self.client
            .set_timeout(timeout.min(Duration::from_millis(900)))?;
        let result = self.refresh_controls();
        // Restore timeout even if a control read fails.
        self.client.set_timeout(timeout)?;
        result?;
        Ok(self.state.clone())
    }

    fn refresh_controls(&mut self) -> Result<()> {
        let uses_eptz = self.uses_eptz();
        if uses_eptz {
            self.state.unsupported.insert(
                "zoom".into(),
                "Use native pan, tilt and zoom controls".into(),
            );
            self.state.unsupported.insert(
                "zoom-speed".into(),
                "Legacy speed does not govern native ePTZ".into(),
            );
        }
        for spec in param_specs() {
            if !spec.readback || self.state.unsupported.contains_key(spec.key) {
                continue;
            }
            if let Err(error) = self.get_param(spec.key) {
                self.state
                    .unsupported
                    .insert(spec.key.into(), format!("{error:#}"));
                // A SET-only/absent module can simply ignore its GET. Verify a
                // fresh heartbeat before deciding that the whole device is gone.
                self.recover_probe().with_context(|| {
                    format!(
                        "camera connection lost while checking {}: {error:#}",
                        spec.key
                    )
                })?;
            }
        }
        let audio_available = param_specs()
            .iter()
            .filter(|spec| spec.section == "Audio" && spec.readback)
            .any(|spec| self.state.values.contains_key(spec.key));
        if !audio_available {
            self.state.unsupported.insert(
                "au-mic-type".into(),
                "No readable audio module controls are available".into(),
            );
        }
        if uses_eptz {
            match self.read_crop() {
                Ok(crop) => self.state.eptz = Some(crop),
                Err(error) => {
                    self.state
                        .unsupported
                        .insert("eptz".into(), format!("{error:#}"));
                }
            }
            self.recover_probe()?;
            match self.read_hsl() {
                Ok(hsl) => self.state.hsl = Some(hsl),
                Err(error) => {
                    self.state
                        .unsupported
                        .insert("hsl".into(), format!("{error:#}"));
                }
            }
            self.recover_probe()?;
            if let Err(error) = self.read_presets() {
                self.state
                    .unsupported
                    .insert("eptz-presets".into(), format!("{error:#}"));
            }
            self.recover_probe()?;
        }
        match self.read_profile() {
            Ok(profile) => self.state.profile = Some(profile),
            Err(error) => {
                self.state
                    .unsupported
                    .insert("profile".into(), format!("{error:#}"));
            }
        }
        if let Ok(Value::TuningFileInfo(info)) =
            self.client.get(50).and_then(accepted).and_then(body)
        {
            self.state.profile_info = Some(info);
        }
        match self.read_wb_offsets() {
            Ok(offsets) => self.state.wb_offsets = Some(offsets),
            Err(error) => {
                self.state
                    .unsupported
                    .insert("wb-offsets".into(), format!("{error:#}"));
            }
        }
        self.recover_probe()?;
        Ok(())
    }

    fn recover_probe(&mut self) -> Result<()> {
        if !self.client.is_connected() {
            self.client
                .connect()
                .context("camera did not respond to the recovery heartbeat")?;
        }
        Ok(())
    }

    fn uses_eptz(&self) -> bool {
        self.state
            .version
            .as_ref()
            .is_some_and(CameraVersion::uses_eptz)
    }

    pub fn get_param(&mut self, key: &str) -> Result<i32> {
        let spec = param_specs()
            .iter()
            .find(|spec| spec.key == key)
            .context("unknown camera parameter")?;
        ensure!(
            spec.readback,
            "{key} is write-only; the camera does not report its current value"
        );
        if let Some(reason) = unsupported_control_reason(key) {
            bail!("{reason}");
        }
        if self.uses_eptz() && matches!(key, "zoom" | "zoom-speed") {
            bail!("use native ePTZ on this firmware");
        }
        let value = body(accepted(self.client.get(spec.cid)?)?)?;
        let raw = match value {
            Value::IntVal(raw) => i32::try_from(raw).context("camera parameter is out of range")?,
            Value::AiTrackingCtrl(control) if key == "ai-tracking" => i32::try_from(control.mode)?,
            _ => bail!("camera returned an unexpected value for {key}"),
        };
        let decoded = if key == "sharpness" && (61..=70).contains(&raw) {
            60 - raw
        } else {
            raw
        };
        self.state.values.insert(key.into(), decoded);
        self.state.unsupported.remove(key);
        Ok(decoded)
    }

    pub fn set_param(&mut self, key: &str, value: i32) -> Result<i32> {
        let spec = param_specs()
            .iter()
            .find(|spec| spec.key == key)
            .context("unknown camera parameter")?;
        if let Some(reason) = unsupported_control_reason(key) {
            bail!("{reason}");
        }
        if let Some(reason) = self.state.unsupported.get(key) {
            bail!("{key} is unavailable: {reason}");
        }
        let maximum = if key == "sharpness" && !self.uses_eptz() {
            30
        } else {
            spec.max
        };
        ensure!(
            (spec.min..=maximum).contains(&value),
            "{key} must be between {} and {maximum}",
            spec.min
        );
        ensure!(
            spec.options.is_empty() || spec.options.iter().any(|(number, _)| *number == value),
            "{value} is not a supported choice for {key}"
        );
        if key == "exposure-ev" {
            // Check the device rather than trusting a cached GUI state. The
            // firmware accepts ineffective EV writes in Manual or locked Auto.
            ensure!(
                self.get_param("exposure-mode")? == 0,
                "Exposure compensation requires Auto exposure; use ISO and shutter in Manual mode"
            );
            ensure!(
                self.get_param("exposure-lock")? == 0,
                "Unlock Auto exposure before changing exposure compensation"
            );
        }
        let wire_value = if key == "sharpness" && value < 0 {
            (60 - value) as u32
        } else {
            value as u32
        };
        let body_value = if key == "ai-tracking" {
            Value::AiTrackingCtrl(pb::AiTrackingCtrl { mode: wire_value })
        } else {
            Value::IntVal(wire_value)
        };
        accepted(self.client.set(spec.cid, body_value)?)?;
        if !spec.readback {
            self.state.values.insert(key.into(), value);
            return Ok(value);
        }
        let actual = self.get_param(key)?;
        ensure!(
            actual == value,
            "camera did not apply {key}: requested {value}, read back {actual}"
        );
        if key == "hdr" {
            self.state.profile = self.read_profile().ok();
            self.state.profile_info = self
                .client
                .get(50)
                .and_then(accepted)
                .and_then(body)
                .ok()
                .and_then(|value| {
                    if let Value::TuningFileInfo(info) = value {
                        Some(info)
                    } else {
                        None
                    }
                });
        }
        // Autofocus/auto-exposure/auto-WB can move values while those controls
        // are hidden. Show fresh readbacks when switching into manual mode.
        let dependent: &[&str] = match (key, value) {
            ("focus-mode", 2) => &["focus-manual"],
            ("exposure-mode", 1) => &["exposure-iso", "exposure-time", "exposure-aperture"],
            ("wb-mode", 0) => &["wb-temp", "wb-temp-shift"],
            _ => &[],
        };
        for &name in dependent {
            if self.state.unsupported.contains_key(name) {
                continue;
            }
            if let Err(error) = self.get_param(name) {
                self.state
                    .unsupported
                    .insert(name.into(), format!("{error:#}"));
            }
        }
        if key == "wb-mode" && value == 0 {
            self.state.wb_offsets = self.read_wb_offsets().ok();
        }
        Ok(actual)
    }

    fn read_crop(&mut self) -> Result<Crop> {
        match body(accepted(self.client.get(117)?)?)? {
            Value::EptzParam(value) => Crop::from_pb(&value),
            _ => bail!("camera returned no ePTZ crop"),
        }
    }
    pub fn set_crop(&mut self, crop: Crop) -> Result<Crop> {
        crop.validate()?;
        ensure!(
            self.uses_eptz(),
            "native ePTZ requires S3 firmware 0.0.8 or later"
        );
        accepted(self.client.set(117, Value::EptzParam(crop.to_pb()))?)?;
        let actual = self.read_crop()?;
        self.state.eptz = Some(actual.clone());
        ensure!(
            crop.approximately_equal(&actual),
            "camera did not apply the requested crop"
        );
        Ok(actual)
    }
    fn read_hsl(&mut self) -> Result<HslState> {
        match body(accepted(self.client.get(118)?)?)? {
            Value::HslParam(value) => HslState::from_pb(&value),
            _ => bail!("camera returned no HSL adjustments"),
        }
    }
    pub fn set_hsl(&mut self, hsl: HslState) -> Result<HslState> {
        hsl.validate()?;
        ensure!(self.uses_eptz(), "HSL requires S3 firmware 0.0.8 or later");
        accepted(self.client.set(118, Value::HslParam(hsl.to_pb()))?)?;
        let actual = self.read_hsl()?;
        self.state.hsl = Some(actual.clone());
        ensure!(
            actual == hsl,
            "camera did not apply the requested HSL adjustments"
        );
        Ok(actual)
    }
    fn read_presets(&mut self) -> Result<()> {
        let value = body(accepted(self.client.get(127)?)?)?;
        let Value::EptzPresets(presets) = value else {
            bail!("camera returned no crop presets");
        };
        let list = match presets.param {
            Some(pb::eptz_presets::Param::Presets(list)) => list,
            None => pb::EptzPresetList::default(),
            _ => bail!("camera returned an unexpected preset list"),
        };
        let mut decoded = BTreeMap::new();
        for entry in list.entries {
            let slot: u8 = entry
                .preset_num
                .parse()
                .context("camera returned invalid preset slot")?;
            ensure!(slot < 4, "camera preset slot must be 0 through 3");
            let Some(native) = entry.eptz else {
                continue;
            };
            // Empty slots in some firmware releases are all-zero messages.
            if native.zoom == 0.0
                && native.left == 0.0
                && native.top == 0.0
                && native.right == 0.0
                && native.bottom == 0.0
            {
                continue;
            }
            let crop = Crop::from_pb(&native)?;
            decoded.insert(slot, crop);
        }
        self.state.presets = decoded;
        Ok(())
    }
    pub fn save_crop(&mut self, slot: u8) -> Result<()> {
        ensure!(slot < 4, "preset slot must be 0 through 3");
        ensure!(
            self.uses_eptz(),
            "crop presets require S3 firmware 0.0.8 or later"
        );
        let crop = self.read_crop()?;
        let value = pb::EptzPresets {
            param: Some(pb::eptz_presets::Param::EptzPreset(pb::EptzPreset {
                preset_num: slot.to_string(),
                eptz: Some(crop.to_pb()),
            })),
        };
        accepted(self.client.set(127, Value::EptzPresets(value))?)?;
        self.read_presets()?;
        ensure!(
            self.state
                .presets
                .get(&slot)
                .is_some_and(|actual| crop.approximately_equal(actual)),
            "camera did not save the requested crop preset"
        );
        Ok(())
    }
    pub fn recall_crop(&mut self, slot: u8) -> Result<Crop> {
        ensure!(slot < 4, "preset slot must be 0 through 3");
        self.read_presets()?;
        let crop = self
            .state
            .presets
            .get(&slot)
            .context("crop preset has not been saved")?
            .clone();
        self.set_crop(crop)
    }

    fn read_profile(&mut self) -> Result<pb::TuningParam> {
        match body(accepted(self.client.get(53)?)?)? {
            Value::TuningParam(profile) => Ok(profile),
            _ => bail!("camera returned no active colour profile"),
        }
    }
    pub fn select_profile(&mut self, id: u32, scene: u32) -> Result<pb::TuningParam> {
        // Read the current bank; selecting a colour profile must never toggle HDR.
        // Pre-HDR firmware has only an SDR bank. New releases must return the
        // actual HDR state; a failed read must never silently change the bank.
        let pre_hdr = self
            .state
            .version
            .as_ref()
            .and_then(CameraVersion::semantic_version)
            .is_some_and(|version| version < (1, 0, 4));
        let hdr = if pre_hdr && self.state.unsupported.contains_key("hdr") {
            0
        } else {
            self.get_param("hdr")?
        };
        ensure!(
            (0..=1).contains(&hdr),
            "camera returned an invalid HDR state"
        );
        let desired = pb::TuningParam {
            id,
            scene_id: scene,
            hdr_flag: hdr as u32,
        };
        accepted(self.client.set(53, Value::TuningParam(desired))?)?;
        let actual = self.read_profile()?;
        self.state.profile = Some(actual);
        ensure!(
            actual == desired,
            "camera did not select the requested profile and style"
        );
        self.state.profile_info = self
            .client
            .get(50)
            .and_then(accepted)
            .and_then(body)
            .ok()
            .and_then(|value| {
                if let Value::TuningFileInfo(info) = value {
                    Some(info)
                } else {
                    None
                }
            });
        Ok(actual)
    }
    fn read_wb_offsets(&mut self) -> Result<(u32, u32)> {
        match body(accepted(self.client.get(33)?)?)? {
            Value::WhiteBalanceOffset(offset) => Ok((offset.offset_r, offset.offset_b)),
            _ => bail!("camera returned no white balance offsets"),
        }
    }
    pub fn set_wb_offsets(&mut self, red: u32, blue: u32) -> Result<(u32, u32)> {
        ensure!(
            red <= 20 && blue <= 20,
            "white balance offsets must be between 0 and 20"
        );
        accepted(self.client.set(
            33,
            Value::WhiteBalanceOffset(pb::WhiteBalanceOffset {
                offset_r: red,
                offset_b: blue,
            }),
        )?)?;
        let actual = self.read_wb_offsets()?;
        self.state.wb_offsets = Some(actual);
        ensure!(
            actual == (red, blue),
            "camera did not apply requested white balance offsets"
        );
        Ok(actual)
    }
    /// Upload packed 10-bit RGB deltas. 5508 zeros reset the camera's LUT.
    pub fn set_clut(&mut self, data: &[u32]) -> Result<()> {
        ensure!(
            data.len() == 5508,
            "camera colour LUT must contain 5508 entries"
        );
        ensure!(
            data.iter().all(|value| *value < (1 << 30)),
            "colour LUT entries must contain three 10-bit channels"
        );
        let file = pb::TuningClutFile {
            clut_size: data.len() as u32,
            clut_data: data.to_vec(),
            hsv_data: "{}".into(),
            extra: String::new(),
            updataflag: "0".into(),
        };
        accepted(self.client.set(95, Value::TuningClutFile(file))?)?;
        Ok(())
    }
}

fn accepted(message: pb::Message) -> Result<pb::Message> {
    ensure!(
        message.resp_code == 200,
        "camera rejected command {} with code {}",
        message.id,
        message.resp_code
    );
    Ok(message)
}
fn body(message: pb::Message) -> Result<Value> {
    message
        .body
        .and_then(|body| body.value)
        .context("camera response contains no value")
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn legacy_brightness_cannot_read_or_write_a_misleading_firmware_value() {
        let mut camera = Camera::new(None, 12345, Duration::from_secs(1));
        for error in [
            camera.get_param("brightness").unwrap_err(),
            camera.set_param("brightness", 39).unwrap_err(),
        ] {
            assert!(error.to_string().contains("exposure compensation"));
        }
        assert!(!camera.client.is_connected());
        assert!(!camera.state.values.contains_key("brightness"));
    }

    #[test]
    fn exposure_compensation_checks_actual_mode_and_lock_before_writing() {
        for (mode, lock) in [(1, 0), (0, 1), (0, 0)] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            let peer = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut requests = Vec::new();
                let mut compensation = 8;
                loop {
                    let mut header = [0; 8];
                    if stream.read_exact(&mut header).is_err() {
                        break;
                    }
                    let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
                    assert!(length <= crate::protocol::MAX_PAYLOAD_SIZE + 8);
                    let mut frame = header.to_vec();
                    frame.resize(8 + length, 0);
                    stream.read_exact(&mut frame[8..]).unwrap();
                    let request = pb::Message::decode(
                        crate::protocol::unpack_frame(&frame).unwrap().as_slice(),
                    )
                    .unwrap();
                    requests.push((request.r#type, request.id));
                    let value = match (request.r#type, request.id) {
                        (2, 215) => 1,
                        (1, 14) => mode,
                        (1, 106) => lock,
                        (2, 15) => {
                            let Value::IntVal(value) = request.body.unwrap().value.unwrap() else {
                                panic!("exposure command must contain its native integer value");
                            };
                            compensation = value;
                            value
                        }
                        (1, 15) => compensation,
                        command => panic!("Unexpected camera request {command:?}"),
                    };
                    let response = pb::Message {
                        r#type: 4,
                        seq: request.seq,
                        id: request.id,
                        resp_code: 200,
                        body: Some(pb::CommandBody {
                            value: Some(Value::IntVal(value)),
                        }),
                        ..Default::default()
                    };
                    stream
                        .write_all(&crate::protocol::pack_frame(&response.encode_to_vec()))
                        .unwrap();
                }
                requests
            });
            let mut camera = Camera::new(Some(Ipv4Addr::LOCALHOST), port, Duration::from_secs(1));
            camera.client.connect().unwrap();
            // Stale UI state must not override the actual mode/lock on camera.
            camera.state.values.insert("exposure-mode".into(), 0);
            camera.state.values.insert("exposure-lock".into(), 0);
            assert!(camera.get_param("brightness").is_err());
            assert!(camera.set_param("brightness", 39).is_err());
            let result = camera.set_param("exposure-ev", 12);
            let expected = match (mode, lock) {
                (1, _) => {
                    assert!(result.unwrap_err().to_string().contains("Auto exposure"));
                    vec![(2, 215), (1, 14)]
                }
                (0, 1) => {
                    assert!(result.unwrap_err().to_string().contains("Unlock"));
                    vec![(2, 215), (1, 14), (1, 106)]
                }
                _ => {
                    assert_eq!(result.unwrap(), 12);
                    vec![(2, 215), (1, 14), (1, 106), (2, 15), (1, 15)]
                }
            };
            camera.disconnect();
            assert_eq!(peer.join().unwrap(), expected);
        }
    }

    #[test]
    fn firmware_version_selects_native_crop_and_sharpness_range() {
        assert!(
            CameraVersion {
                firmware: "YunxiCamera-ch131b-1.0.4".into(),
                ..Default::default()
            }
            .uses_eptz()
        );
        assert!(
            CameraVersion {
                firmware: "YunxiCamera-ch131b-0.0.8".into(),
                ..Default::default()
            }
            .uses_eptz()
        );
        assert!(
            !CameraVersion {
                firmware: "YunxiCamera-ch131b-0.0.7".into(),
                ..Default::default()
            }
            .uses_eptz()
        );
        assert!(
            !CameraVersion {
                firmware: "invalid".into(),
                ..Default::default()
            }
            .uses_eptz()
        );
    }
    #[test]
    fn crop_is_bounded_and_keeps_image_extent() {
        assert_eq!(
            Crop::centred(2.0).unwrap(),
            Crop {
                zoom: 2.0,
                left: 0.25,
                top: 0.25,
                right: 0.75,
                bottom: 0.75
            }
        );
        assert!(Crop::new(2.0, 0.6, 0.0).is_err());
        assert!(Crop::centred(f32::NAN).is_err());
        assert!(Crop::centred(0.9).is_err());
        assert!(Crop::new(4.0, f32::INFINITY, 0.0).is_err());
    }
    #[test]
    fn hsl_roundtrip_preserves_all_twenty_four_signed_strings() {
        let mut state = HslState::default();
        for (index, value) in state.values.iter_mut().flatten().enumerate() {
            *value = index as i32 - 12;
        }
        let native = state.to_pb();
        assert_eq!(native.hue_red, "-12");
        assert_eq!(native.luminance_magenta, "11");
        assert_eq!(HslState::from_pb(&native).unwrap(), state);
        assert_eq!(
            HslState::from_pb(&pb::HslAdjust::default()).unwrap(),
            HslState::default()
        );
    }
    #[test]
    fn validation_rejects_bad_controls_without_connecting() {
        let mut camera = Camera::new(None, 12345, Duration::from_secs(1));
        camera.state.version = Some(CameraVersion {
            firmware: "YunxiCamera-ch131b-1.0.4".into(),
            ..Default::default()
        });
        assert!(camera.set_param("sharpness", 11).is_err());
        assert!(camera.set_param("sharpness", -11).is_err());
        assert!(camera.set_param("exposure-iso", 123).is_err());
        assert!(camera.set_param("gamma", 50).is_err());
        assert!(camera.set_wb_offsets(21, 10).is_err());
        assert!(camera.set_clut(&[0; 10]).is_err());
        assert!(!camera.client.is_connected());
    }
}
