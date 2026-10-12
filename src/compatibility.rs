// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared, read-only policy for camera controls, capture modes and presets.
//!
//! A camera's shared frame-rate command can be a stub. Missing frame-rate
//! information never becomes an invented value or an automatic HDR change.

use crate::{
    camera::{CameraState, param_specs, unsupported_control_reason},
    settings::Preset,
};
use anyhow::{Result, bail, ensure};

pub const HDR_MAX_FPS: f64 = 30.0;
// UVC's 100 ns interval ticks can describe nominal 30 fps as
// 10_000_000 / 333_333 = 30.000030...; retain that actual capture interval.
const FRAME_INTERVAL_TOLERANCE: f64 = 0.001;

/// Compare an already validated rate with the S3 30 fps limit, allowing only
/// the small representation error of native capture intervals.
pub fn exceeds_hdr_limit(fps: f64) -> bool {
    fps > HDR_MAX_FPS + FRAME_INTERVAL_TOLERANCE
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompatibilityCheck {
    pub reason: Option<String>,
    pub advisory: Option<String>,
}

/// Check an actual camera or V4L2 frame rate, including fractional intervals.
pub fn hdr_fps_check(hdr: Option<i32>, fps: Option<f64>) -> CompatibilityCheck {
    let reason = match (hdr, fps) {
        (Some(value), _) if !matches!(value, 0 | 1) => {
            Some("Camera returned an invalid HDR state".to_owned())
        }
        (_, Some(value)) if !value.is_finite() || value <= 0.0 => {
            Some("Frame rate must be a positive, finite value".to_owned())
        }
        (Some(1), Some(value)) if exceeds_hdr_limit(value) => {
            Some("YoloCam S3 HDR requires 30 fps or lower; disable HDR before selecting this frame rate".to_owned())
        }
        (None, Some(value)) if exceeds_hdr_limit(value) => Some(
            "HDR state is unavailable; confirm HDR is off before selecting a frame rate above 30 fps".to_owned(),
        ),
        _ => None,
    };
    let advisory = if reason.is_none() && hdr == Some(1) && fps.is_none() {
        Some("Camera frame rate is unavailable; use preview and output modes at 30 fps or lower with HDR".to_owned())
    } else {
        None
    };
    CompatibilityCheck { reason, advisory }
}

/// Older firmware with explicitly absent HDR has one SDR profile bank. For
/// newer/unknown firmware, an unavailable HDR readback remains unknown.
pub fn effective_hdr(state: &CameraState) -> Option<i32> {
    if !state.unsupported.contains_key("hdr")
        && let Some(&value) = state.values.get("hdr")
        && matches!(value, 0 | 1)
    {
        return Some(value);
    }
    let pre_hdr = state
        .version
        .as_ref()
        .filter(|version| version.firmware.starts_with("YunxiCamera-ch131b-"))
        .and_then(|version| version.semantic_version())
        .is_some_and(|version| version < (1, 0, 4));
    (pre_hdr && state.unsupported.contains_key("hdr")).then_some(0)
}

pub fn camera_fps(state: &CameraState) -> Option<f64> {
    if state.unsupported.contains_key("frame-rate") {
        return None;
    }
    state
        .values
        .get("frame-rate")
        .filter(|value| matches!(**value, 25 | 30 | 50 | 60))
        .map(|value| f64::from(*value))
}

pub fn preview_reason(state: &CameraState, fps: f64) -> Option<String> {
    hdr_fps_check(effective_hdr(state), Some(fps)).reason
}

/// Validate the requested value/capability without relying on a cached mode.
/// Camera writes call this before fresh dependency reads.
pub fn control_value_reason(state: &CameraState, key: &str, value: i32) -> Option<String> {
    if let Some(reason) = unsupported_control_reason(key) {
        return Some(reason.to_owned());
    }
    if let Some(reason) = state.unsupported.get(key) {
        return Some(format!("{key} is unavailable: {reason}"));
    }
    let Some(spec) = param_specs().iter().find(|spec| spec.key == key) else {
        return Some(format!("Unknown camera parameter: {key}"));
    };
    let legacy_sharpness = state
        .version
        .as_ref()
        .and_then(|version| version.semantic_version())
        .is_some_and(|version| version < (0, 0, 8));
    let maximum = if key == "sharpness" && legacy_sharpness {
        30
    } else {
        spec.max
    };
    if !(spec.min..=maximum).contains(&value) {
        return Some(format!(
            "{key} must be between {} and {maximum} on this firmware",
            spec.min
        ));
    }
    if !spec.options.is_empty() && !spec.options.iter().any(|(number, _)| *number == value) {
        return Some(format!("{value} is not a supported choice for {key}"));
    }
    if matches!(key, "zoom" | "zoom-speed")
        && state
            .version
            .as_ref()
            .is_some_and(|version| version.uses_eptz())
    {
        return Some("Use native ePTZ on this firmware".to_owned());
    }
    None
}

fn mode_reason(state: &CameraState, key: &str, check_lock: bool) -> Option<String> {
    let (mode, expected, reason) = match key {
        "exposure-ev" => (
            "exposure-mode",
            0,
            "Exposure compensation requires Auto exposure; use ISO and shutter in Manual mode",
        ),
        "exposure-iso" | "exposure-time" | "exposure-aperture" => (
            "exposure-mode",
            1,
            "ISO, shutter and aperture require Manual exposure mode",
        ),
        "exposure-lock" => (
            "exposure-mode",
            0,
            "Exposure lock requires Auto exposure mode",
        ),
        "wb-temp" | "wb-temp-shift" | "wb-offsets" => (
            "wb-mode",
            0,
            "White-balance temperature, shift and offsets require Manual white balance",
        ),
        "wb-lock" => (
            "wb-mode",
            1,
            "White-balance lock requires Auto white balance",
        ),
        "focus-manual" => ("focus-mode", 2, "Manual focus requires Manual focus mode"),
        _ => return None,
    };
    if state.unsupported.contains_key(mode) || state.values.get(mode) != Some(&expected) {
        return Some(reason.to_owned());
    }
    if key == "exposure-ev" && check_lock && state.values.get("exposure-lock") != Some(&0) {
        return Some("Unlock Auto exposure before changing exposure compensation".to_owned());
    }
    None
}

pub fn control_reason(state: &CameraState, key: &str, value: i32) -> Option<String> {
    if let Some(reason) =
        control_value_reason(state, key, value).or_else(|| mode_reason(state, key, true))
    {
        return Some(reason);
    }
    match key {
        "hdr" => hdr_fps_check(Some(value), camera_fps(state)).reason,
        "frame-rate" => hdr_fps_check(effective_hdr(state), Some(f64::from(value))).reason,
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PresetPlan {
    /// Every requested scalar key exactly once, with controlling modes first.
    /// The worker still owns the EV unlock/settle/relock transaction.
    pub ordered_keys: Vec<String>,
    pub target_hdr: Option<i32>,
    pub target_fps: Option<f64>,
    pub advisories: Vec<String>,
}

fn advanced_supported(state: &CameraState, key: &str, present: bool) -> Result<()> {
    if let Some(reason) = state.unsupported.get(key) {
        bail!("Preset {key} is unavailable: {reason}");
    }
    ensure!(
        present,
        "Preset {key} has no confirmed camera capability/readback"
    );
    Ok(())
}

/// Preflight the complete preset without any I/O or camera writes. A valid
/// plan changes only the values explicitly present in the normalized preset.
pub fn preset_plan(state: &CameraState, preset: &Preset) -> Result<PresetPlan> {
    let preset = preset.normalized();
    preset.validate()?;
    if let Some(crop) = &preset.crop {
        crop.validate()?;
    }
    if let Some(hsl) = &preset.hsl {
        hsl.validate()?;
    }
    for (key, value) in &preset.values {
        if let Some(reason) = control_value_reason(state, key, *value) {
            bail!("Preset {reason}");
        }
        ensure!(
            state.values.contains_key(key),
            "Preset {key} has no confirmed camera capability/readback"
        );
    }
    if preset.values.contains_key("exposure-ev") || preset.values.contains_key("exposure-lock") {
        if let Some(reason) = state.unsupported.get("exposure-lock") {
            bail!("Preset needs readable Auto exposure lock: {reason}");
        }
        // EV implicitly needs this capability even if the preset did not
        // request a final lock value. The worker snapshots/restores this flag.
        ensure!(
            matches!(state.values.get("exposure-lock"), Some(0 | 1)),
            "Preset needs a readable Auto exposure lock with value 0 or 1"
        );
    }
    let mut effective = state.clone();
    effective.values.extend(preset.values.clone());
    for key in preset.values.keys() {
        // EV presets explicitly unlock temporarily in the worker; their
        // final lock choice must not reject that existing transaction.
        if let Some(reason) = mode_reason(&effective, key, false) {
            bail!("Preset {reason}");
        }
    }
    let target_hdr = effective_hdr(&effective);
    let target_fps = camera_fps(&effective);
    let mut advisories = Vec::new();
    if preset.values.contains_key("hdr")
        || preset.values.contains_key("frame-rate")
        || preset.profile.is_some()
    {
        let check = hdr_fps_check(target_hdr, target_fps);
        if let Some(reason) = check.reason {
            bail!("Preset {reason}");
        }
        if let Some(advisory) = check.advisory {
            advisories.push(advisory);
        }
    }
    if preset.crop.is_some() || preset.hsl.is_some() {
        ensure!(
            state
                .version
                .as_ref()
                .is_some_and(|version| version.uses_eptz()),
            "Preset native crop and HSL require S3 firmware 0.0.8 or later"
        );
    }
    if preset.crop.is_some() {
        advanced_supported(state, "eptz", state.eptz.is_some())?;
    }
    if preset.hsl.is_some() {
        advanced_supported(state, "hsl", state.hsl.is_some())?;
    }
    if preset.wb_offsets.is_some() {
        advanced_supported(state, "wb-offsets", state.wb_offsets.is_some())?;
        if let Some(reason) = mode_reason(&effective, "wb-offsets", false) {
            bail!("Preset {reason}");
        }
    }
    if let Some(profile) = &preset.profile {
        advanced_supported(state, "profile", state.profile.is_some())?;
        let hdr = target_hdr.ok_or_else(|| anyhow::anyhow!(
            "Preset colour profile needs a confirmed HDR bank; the camera HDR state is unavailable"
        ))?;
        ensure!(
            profile.hdr_flag == hdr as u32,
            "Preset colour profile belongs to a different HDR bank"
        );
        // CID50 describes one installed profile, not the entire catalogue.
        // Validate its scene count only when it describes the requested bank
        // and ID. Other installed IDs remain camera-validated at SET time.
        if let Some(info) = &state.profile_info
            && info.id == profile.id
            && state
                .profile
                .as_ref()
                .is_some_and(|current| current.hdr_flag == profile.hdr_flag)
            && info.scene_count > 0
        {
            ensure!(
                profile.scene_id < info.scene_count,
                "Preset colour profile scene {} is unavailable ({} installed scenes)",
                profile.scene_id,
                info.scene_count
            );
        }
    }
    let video_order = if target_hdr == Some(1) {
        ["frame-rate", "hdr"]
    } else {
        ["hdr", "frame-rate"]
    };
    let mut ordered_keys = Vec::new();
    for key in video_order
        .into_iter()
        .chain(["exposure-mode", "wb-mode", "focus-mode"])
    {
        if preset.values.contains_key(key) {
            ordered_keys.push(key.to_owned());
        }
    }
    for key in preset.values.keys() {
        if !ordered_keys.contains(key) {
            ordered_keys.push(key.clone());
        }
    }
    Ok(PresetPlan {
        ordered_keys,
        target_hdr,
        target_fps,
        advisories,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        camera::{CameraVersion, Crop, HslState},
        pb,
        settings::ProfileSelection,
    };

    fn state() -> CameraState {
        CameraState {
            version: Some(CameraVersion {
                firmware: "YunxiCamera-ch131b-1.0.4".into(),
                build: 1349,
                ..Default::default()
            }),
            values: [
                ("hdr", 0),
                ("frame-rate", 60),
                ("contrast", 48),
                ("exposure-mode", 0),
                ("exposure-ev", 8),
                ("exposure-lock", 1),
                ("exposure-iso", 400),
                ("wb-mode", 1),
                ("wb-temp", 5600),
                ("wb-lock", 0),
                ("focus-mode", 3),
                ("focus-manual", 30),
                ("sharpness", 0),
                ("zoom", 50),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
            profile: Some(pb::TuningParam::default()),
            eptz: Some(Crop::centred(1.0).unwrap()),
            hsl: Some(HslState::default()),
            wb_offsets: Some((10, 10)),
            ..Default::default()
        }
    }

    fn preset(values: &[(&str, i32)]) -> Preset {
        Preset {
            schema_version: 1,
            name: "Compatibility fixture".into(),
            values: values
                .iter()
                .map(|(key, value)| ((*key).to_owned(), *value))
                .collect(),
            crop: None,
            hsl: None,
            profile: None,
            wb_offsets: None,
            omitted_controls: Default::default(),
        }
    }

    #[test]
    fn hdr_limits_actual_fractional_rates_and_preserves_unknown_values() {
        for fps in [25.0, 30.0, 30000.0 / 1001.0, 10_000_000.0 / 333_333.0] {
            assert!(hdr_fps_check(Some(1), Some(fps)).reason.is_none());
        }
        for fps in [30.01, 50.0, 60.0, 60000.0 / 1001.0] {
            assert!(hdr_fps_check(Some(1), Some(fps)).reason.is_some());
            assert!(hdr_fps_check(Some(0), Some(fps)).reason.is_none());
            assert!(hdr_fps_check(None, Some(fps)).reason.is_some());
        }
        for fps in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(hdr_fps_check(Some(0), Some(fps)).reason.is_some());
        }
        let unknown_fps = hdr_fps_check(Some(1), None);
        assert!(unknown_fps.reason.is_none());
        assert!(unknown_fps.advisory.unwrap().contains("unavailable"));
        assert!(hdr_fps_check(None, Some(30.0)).reason.is_none());
    }

    #[test]
    fn preview_permits_sole_legacy_sdr_bank_but_never_guesses_modern_hdr() {
        let mut state = state();
        state.values.remove("hdr");
        state
            .unsupported
            .insert("hdr".into(), "absent module".into());
        assert!(preview_reason(&state, 60.0).is_some());
        state.version.as_mut().unwrap().firmware = "YunxiCamera-ch131b-0.0.7".into();
        assert_eq!(effective_hdr(&state), Some(0));
        assert!(preview_reason(&state, 60.0).is_none());
        state.version.as_mut().unwrap().firmware = "OtherCamera-othermodel-0.0.7".into();
        assert_eq!(effective_hdr(&state), None);
        assert!(preview_reason(&state, 60.0).is_some());
        state.version = None;
        assert!(preview_reason(&state, 60.0).is_some());
    }

    #[test]
    fn presets_order_both_hdr_transitions_without_adding_unrequested_values() {
        let mut state = state();
        let hdr_on = preset(&[("hdr", 1), ("frame-rate", 30), ("contrast", 55)]);
        let plan = preset_plan(&state, &hdr_on).unwrap();
        assert_eq!(plan.ordered_keys, ["frame-rate", "hdr", "contrast"]);
        assert_eq!(plan.target_hdr, Some(1));
        assert_eq!(plan.target_fps, Some(30.0));
        state.values.insert("hdr".into(), 1);
        state.values.insert("frame-rate".into(), 30);
        let hdr_off = preset(&[("hdr", 0), ("frame-rate", 60), ("contrast", 55)]);
        assert_eq!(
            preset_plan(&state, &hdr_off).unwrap().ordered_keys,
            ["hdr", "frame-rate", "contrast"]
        );
        assert!(preset_plan(&state, &preset(&[("frame-rate", 60)])).is_err());
        let only_fps = preset_plan(&state, &preset(&[("frame-rate", 25)])).unwrap();
        assert_eq!(only_fps.ordered_keys, ["frame-rate"]);
        assert_eq!(only_fps.target_hdr, Some(1));
        assert!(preset_plan(&state, &preset(&[("hdr", 1), ("frame-rate", 60)])).is_err());
    }

    #[test]
    fn preset_unknown_fps_advisory_does_not_invent_or_write_a_frame_rate() {
        let mut state = state();
        state.values.remove("frame-rate");
        state
            .unsupported
            .insert("frame-rate".into(), "shared command is a stub".into());
        let plan = preset_plan(&state, &preset(&[("hdr", 1)])).unwrap();
        assert_eq!(plan.ordered_keys, ["hdr"]);
        assert_eq!(plan.target_fps, None);
        assert_eq!(plan.advisories.len(), 1);
        assert!(preset_plan(&state, &preset(&[("hdr", 1), ("frame-rate", 30)])).is_err());
    }

    #[test]
    fn preset_validates_all_effective_modes_while_retaining_ev_lock_transaction() {
        let state = state();
        for invalid in [
            preset(&[("contrast", 55), ("exposure-iso", 400)]),
            preset(&[("contrast", 55), ("wb-temp", 6500)]),
            preset(&[("contrast", 55), ("focus-manual", 40)]),
            preset(&[("exposure-mode", 1), ("exposure-ev", 12)]),
            preset(&[("wb-mode", 0), ("wb-lock", 1)]),
        ] {
            assert!(preset_plan(&state, &invalid).is_err());
        }
        let valid = preset(&[
            ("exposure-mode", 1),
            ("exposure-iso", 400),
            ("wb-mode", 0),
            ("wb-temp", 6500),
            ("focus-mode", 2),
            ("focus-manual", 40),
        ]);
        let plan = preset_plan(&state, &valid).unwrap();
        assert_eq!(
            &plan.ordered_keys[..3],
            ["exposure-mode", "wb-mode", "focus-mode"]
        );
        let ev = preset(&[("exposure-ev", 12), ("exposure-lock", 1)]);
        assert!(preset_plan(&state, &ev).is_ok());
        assert!(
            control_reason(&state, "exposure-ev", 12)
                .unwrap()
                .contains("Unlock")
        );
    }

    #[test]
    fn ev_presets_require_valid_lock_readback_even_without_a_requested_lock() {
        let preset = preset(&[("hdr", 1), ("frame-rate", 30), ("exposure-ev", 12)]);
        let mut state = state();
        assert!(preset_plan(&state, &preset).is_ok());
        state.values.remove("exposure-lock");
        assert!(
            preset_plan(&state, &preset)
                .unwrap_err()
                .to_string()
                .contains("readable Auto exposure lock")
        );
        state.values.insert("exposure-lock".into(), 2);
        assert!(preset_plan(&state, &preset).is_err());
        state.values.insert("exposure-lock".into(), 0);
        state
            .unsupported
            .insert("exposure-lock".into(), "unavailable module".into());
        assert!(preset_plan(&state, &preset).is_err());
        state.unsupported.clear();
        assert!(preset_plan(&state, &preset).is_ok());
    }

    #[test]
    fn firmware_and_optional_capabilities_reject_complete_presets_up_front() {
        let mut state = state();
        let mut advanced = preset(&[("contrast", 55)]);
        advanced.crop = Some(Crop::centred(2.0).unwrap());
        advanced.hsl = Some(HslState::default());
        assert!(preset_plan(&state, &advanced).is_ok());
        state.unsupported.insert("hsl".into(), "not present".into());
        assert!(preset_plan(&state, &advanced).is_err());
        state.unsupported.remove("hsl");
        state.version.as_mut().unwrap().firmware = "YunxiCamera-ch131b-0.0.7".into();
        assert!(preset_plan(&state, &advanced).is_err());
        assert!(preset_plan(&state, &preset(&[("sharpness", 24)])).is_ok());
        state.version.as_mut().unwrap().firmware = "YunxiCamera-ch131b-1.0.4".into();
        assert!(preset_plan(&state, &preset(&[("sharpness", 24)])).is_err());
        assert!(preset_plan(&state, &preset(&[("zoom", 50)])).is_err());
        state.values.remove("contrast");
        assert!(preset_plan(&state, &preset(&[("contrast", 55)])).is_err());
    }

    #[test]
    fn profile_bank_and_known_scene_count_are_checked_before_any_setting() {
        let mut state = state();
        state.profile_info = Some(pb::TuningFileInfo {
            id: 0,
            scene_count: 2,
            ..Default::default()
        });
        let mut preset = preset(&[("contrast", 55)]);
        preset.profile = Some(ProfileSelection {
            id: 0,
            scene_id: 0,
            hdr_flag: 1,
        });
        assert!(
            preset_plan(&state, &preset)
                .unwrap_err()
                .to_string()
                .contains("HDR bank")
        );
        preset.profile.as_mut().unwrap().hdr_flag = 0;
        preset.profile.as_mut().unwrap().scene_id = 2;
        assert!(
            preset_plan(&state, &preset)
                .unwrap_err()
                .to_string()
                .contains("installed scenes")
        );
        preset.profile.as_mut().unwrap().scene_id = 1;
        assert!(preset_plan(&state, &preset).is_ok());
        state.unsupported.insert("hdr".into(), "unreadable".into());
        state.values.insert("frame-rate".into(), 30);
        assert!(
            preset_plan(&state, &preset)
                .unwrap_err()
                .to_string()
                .contains("confirmed HDR bank")
        );
    }

    #[test]
    fn offsets_and_nonfinite_crop_are_rejected_before_sibling_settings() {
        let mut preset = preset(&[("contrast", 55)]);
        preset.wb_offsets = Some((10, 10));
        assert!(preset_plan(&state(), &preset).is_err());
        preset.values.insert("wb-mode".into(), 0);
        assert!(preset_plan(&state(), &preset).is_ok());
        preset.crop = Some(Crop {
            right: f32::NAN,
            ..Crop::centred(2.0).unwrap()
        });
        assert!(preset_plan(&state(), &preset).is_err());
    }
}
