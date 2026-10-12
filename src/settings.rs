// SPDX-License-Identifier: GPL-3.0-or-later
//! Local preferences and camera presets. Camera images are never saved here.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::camera::{CameraState, Crop, HslState, param_specs, unsupported_control_reason};

pub const APP_DIRECTORY: &str = "yolocam-linux";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub schema_version: u32,
    pub preview_enabled: bool,
    pub active_tab: String,
    pub preview_device: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            preview_enabled: false,
            active_tab: "camera".to_owned(),
            preview_device: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileSelection {
    pub id: u32,
    pub scene_id: u32,
    pub hdr_flag: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Preset {
    pub schema_version: u32,
    pub name: String,
    pub values: BTreeMap<String, i32>,
    pub crop: Option<Crop>,
    pub hsl: Option<HslState>,
    pub profile: Option<ProfileSelection>,
    pub wb_offsets: Option<(u32, u32)>,
    #[serde(default)]
    pub omitted_controls: BTreeMap<String, String>,
}

impl Preset {
    /// Capture values that were actually read from this camera. A manual focus
    /// position is meaningful only when the camera is in manual focus mode.
    pub fn capture(name: &str, state: &CameraState) -> Result<Self> {
        validate_name(name)?;
        let mut values = BTreeMap::new();
        let mut omitted_controls = BTreeMap::new();
        for spec in param_specs() {
            if unsupported_control_reason(spec.key).is_some()
                || !spec.readback
                || state.unsupported.contains_key(spec.key)
                || (spec.key == "focus-manual" && state.values.get("focus-mode") != Some(&2))
                || (["exposure-iso", "exposure-time", "exposure-aperture"].contains(&spec.key)
                    && state.values.get("exposure-mode") != Some(&1))
                || (["exposure-ev", "exposure-lock"].contains(&spec.key)
                    && state.values.get("exposure-mode") != Some(&0))
                || (["wb-temp", "wb-temp-shift"].contains(&spec.key)
                    && state.values.get("wb-mode") != Some(&0))
                || (spec.key == "wb-lock" && state.values.get("wb-mode") != Some(&1))
                || (spec.key == "zoom" && state.eptz.is_some())
                || (spec.key == "zoom-speed" && state.eptz.is_some())
            {
                continue;
            }
            if let Some(value) = state.values.get(spec.key) {
                let maximum = if spec.key == "sharpness"
                    && state
                        .version
                        .as_ref()
                        .is_some_and(|version| !version.uses_eptz())
                {
                    30
                } else {
                    spec.max
                };
                if *value < spec.min
                    || *value > maximum
                    || (!spec.options.is_empty() && !spec.options.iter().any(|(v, _)| v == value))
                {
                    omitted_controls.insert(
                        spec.key.to_owned(),
                        format!(
                            "Camera returned {value}, outside this firmware's supported values"
                        ),
                    );
                    continue;
                }
                values.insert(spec.key.to_owned(), *value);
            }
        }
        Ok(Self {
            schema_version: 1,
            name: name.trim().to_owned(),
            values,
            crop: state.eptz.clone(),
            hsl: state.hsl.clone(),
            profile: state.profile.as_ref().map(|p| ProfileSelection {
                id: p.id,
                scene_id: p.scene_id,
                hdr_flag: p.hdr_flag,
            }),
            wb_offsets: if state.values.get("wb-mode") == Some(&0) {
                state.wb_offsets
            } else {
                None
            },
            omitted_controls,
        })
    }

    /// v0.1.0 presets included controls that the camera acknowledged without
    /// changing its image. Keep the usable settings and a visible migration
    /// note; never reinterpret a legacy brightness value as exposure steps.
    pub fn normalize(&mut self) {
        let omitted = &mut self.omitted_controls;
        self.values.retain(|key, _| {
            if let Some(reason) = unsupported_control_reason(key) {
                omitted.insert(key.clone(), reason.to_owned());
                false
            } else {
                true
            }
        });
    }

    pub fn normalized(&self) -> Self {
        let mut preset = self.clone();
        preset.normalize();
        preset
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let mut preset: Self = serde_json::from_slice(bytes)?;
        preset.normalize();
        preset.validate()?;
        Ok(preset)
    }

    pub fn omission_note(&self) -> Option<String> {
        if self.omitted_controls.is_empty() {
            return None;
        }
        Some(format!(
            "Skipped unavailable controls: {}",
            self.omitted_controls
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }

    pub fn validate(&self) -> Result<()> {
        validate_name(&self.name)?;
        if self.schema_version != 1 {
            bail!(
                "Preset uses an unsupported schema version {}",
                self.schema_version
            );
        }
        for (key, value) in &self.values {
            if unsupported_control_reason(key).is_some() {
                continue;
            }
            let spec = param_specs()
                .iter()
                .find(|spec| spec.key == key)
                .with_context(|| format!("Unknown preset control: {key}"))?;
            if !spec.readback {
                bail!("Write-only control {key} cannot be stored in a readback preset");
            }
            let maximum = if key == "sharpness" { 30 } else { spec.max };
            if *value < spec.min || *value > maximum {
                bail!("Preset control {key} is outside {}…{maximum}", spec.min);
            }
            if !spec.options.is_empty() && !spec.options.iter().any(|(v, _)| v == value) {
                bail!("Preset control {key} has an unsupported value {value}");
            }
        }
        if let Some(crop) = &self.crop {
            let valid = Crop::new(crop.zoom, crop.left, crop.top)?;
            if (valid.right - crop.right).abs() > 0.002
                || (valid.bottom - crop.bottom).abs() > 0.002
            {
                bail!("Preset crop dimensions do not match its zoom");
            }
        }
        if let Some(hsl) = &self.hsl
            && hsl
                .values
                .iter()
                .flatten()
                .any(|v| !(-100..=100).contains(v))
        {
            bail!("Preset HSL adjustments must be between -100 and 100");
        }
        if let Some(profile) = &self.profile
            && profile.hdr_flag > 1
        {
            bail!("Preset colour profile has an invalid HDR bank");
        }
        if let Some((red, blue)) = self.wb_offsets
            && (red > 20 || blue > 20)
        {
            bail!("Preset white-balance offsets must be between 0 and 20");
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    directory: PathBuf,
}

impl Store {
    pub fn new() -> Result<Self> {
        let base = dirs::config_dir().context("Cannot find the user configuration directory")?;
        Self::at(base.join(APP_DIRECTORY))
    }

    pub fn at(directory: PathBuf) -> Result<Self> {
        fs::create_dir_all(directory.join("presets"))
            .with_context(|| format!("Cannot create {}", directory.display()))?;
        Ok(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    pub fn load_settings(&self) -> Result<Settings> {
        let path = self.directory.join("settings.json");
        match fs::read(&path) {
            Ok(bytes) => {
                let settings: Settings = serde_json::from_slice(&bytes)
                    .with_context(|| format!("Cannot read {}", path.display()))?;
                if settings.schema_version != 1 {
                    bail!("Settings use an unsupported schema version");
                }
                Ok(settings)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(error) => Err(error).with_context(|| format!("Cannot read {}", path.display())),
        }
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<()> {
        atomic_json(&self.directory.join("settings.json"), settings)
    }

    pub fn save_preset(&self, preset: &Preset) -> Result<()> {
        let preset = preset.normalized();
        preset.validate()?;
        atomic_json(&self.preset_path(&preset.name)?, &preset)
    }

    pub fn load_preset(&self, name: &str) -> Result<Preset> {
        let path = self.preset_path(name)?;
        let preset = Preset::from_json(
            &fs::read(&path).with_context(|| format!("Cannot read {}", path.display()))?,
        )?;
        Ok(preset)
    }

    pub fn presets(&self) -> Result<Vec<Preset>> {
        let mut presets = Vec::new();
        for entry in fs::read_dir(self.directory.join("presets"))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let preset = Preset::from_json(&fs::read(&path)?)
                .with_context(|| format!("Invalid preset {}", path.display()))?;
            presets.push(preset);
        }
        presets.sort_by_key(|preset| preset.name.to_lowercase());
        Ok(presets)
    }

    pub fn delete_preset(&self, name: &str) -> Result<()> {
        fs::remove_file(self.preset_path(name)?)
            .with_context(|| format!("Cannot delete preset {name}"))
    }

    fn preset_path(&self, name: &str) -> Result<PathBuf> {
        validate_name(name)?;
        // Encode the complete name rather than replacing path characters: two
        // different names can never silently overwrite the same file.
        let encoded: String = name
            .trim()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(self
            .directory
            .join("presets")
            .join(format!("preset-{encoded}.json")))
    }
}

fn validate_name(name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() || name.len() > 100 || name.chars().any(char::is_control) {
        bail!("Choose a preset name of 1–100 bytes without control characters");
    }
    Ok(())
}

pub fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .context("The settings path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let temporary = parent.join(format!(".yolocam-{}-{timestamp}.tmp", std::process::id()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.with_context(|| format!("Cannot save {}", path.display()))
}

pub fn save_update_backup(state: &CameraState) -> Result<PathBuf> {
    let base = dirs::data_dir().context("Cannot find the user data directory")?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let path = base
        .join(APP_DIRECTORY)
        .join("backups")
        .join(format!("before-update-{stamp}.json"));
    atomic_json(&path, state)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_names_cannot_escape_directory_or_collide() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::at(temp.path().to_owned()).unwrap();
        let first = Preset::capture("../../day/light", &CameraState::default()).unwrap();
        let second = Preset::capture(".._.._day_light", &CameraState::default()).unwrap();
        store.save_preset(&first).unwrap();
        store.save_preset(&second).unwrap();
        assert_eq!(store.presets().unwrap().len(), 2);
        assert_eq!(store.load_preset(&first.name).unwrap().name, first.name);
        assert!(
            store
                .preset_path(&first.name)
                .unwrap()
                .starts_with(temp.path().join("presets"))
        );
    }

    #[test]
    fn capture_keeps_only_real_values_and_manual_focus_in_manual_mode() {
        let mut state = CameraState::default();
        state.values.insert("contrast".to_owned(), 42);
        state.values.insert("brightness".to_owned(), 50);
        state.values.insert("focus-mode".to_owned(), 4);
        state.values.insert("focus-manual".to_owned(), 67);
        state.values.insert("dehaze".to_owned(), 10);
        state.values.insert("hue".to_owned(), 50);
        state.values.insert("face-roi".to_owned(), 1);
        state
            .unsupported
            .insert("hue".to_owned(), "Unavailable".to_owned());
        let preset = Preset::capture("Office", &state).unwrap();
        assert_eq!(preset.values.get("contrast"), Some(&42));
        assert!(!preset.values.contains_key("brightness"));
        assert!(!preset.values.contains_key("dehaze"));
        assert!(!preset.values.contains_key("hue"));
        assert!(!preset.values.contains_key("face-roi"));
        assert!(!preset.values.contains_key("focus-manual"));
        state.values.insert("focus-mode".to_owned(), 2);
        assert_eq!(
            Preset::capture("Manual", &state)
                .unwrap()
                .values
                .get("focus-manual"),
            Some(&67)
        );
    }

    #[test]
    fn legacy_presets_load_import_and_save_without_sending_or_reinterpreting_brightness() {
        let bytes = br#"{"schema_version":1,"name":"Legacy look","values":{"brightness":50,"hue":50,"gamma":50,"contrast":42,"exposure-mode":0,"exposure-ev":12,"exposure-lock":1},"crop":null,"hsl":null,"profile":null,"wb_offsets":null}"#;
        let imported = Preset::from_json(bytes).unwrap();
        assert_eq!(imported.schema_version, 1);
        assert_eq!(imported.values.get("contrast"), Some(&42));
        assert_eq!(imported.values.get("exposure-ev"), Some(&12));
        for key in ["brightness", "hue", "gamma"] {
            assert!(!imported.values.contains_key(key));
            assert!(imported.omitted_controls.contains_key(key));
        }
        assert!(imported.omission_note().unwrap().contains("brightness"));
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().to_owned()).unwrap();
        let path = store.preset_path("Legacy look").unwrap();
        fs::write(&path, bytes).unwrap();
        assert!(
            !store
                .load_preset("Legacy look")
                .unwrap()
                .values
                .contains_key("brightness")
        );
        assert!(
            !store.presets().unwrap()[0]
                .values
                .contains_key("brightness")
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            bytes,
            "Loading does not overwrite the original preset"
        );
        let legacy: Preset = serde_json::from_slice(bytes).unwrap();
        store.save_preset(&legacy).unwrap();
        let saved: Preset = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert!(!saved.values.contains_key("brightness"));
        assert_eq!(saved.values.get("exposure-ev"), Some(&12));
        let mut brightness_only = legacy;
        brightness_only.values.clear();
        brightness_only.values.insert("brightness".to_owned(), 100);
        brightness_only.normalize();
        assert!(
            brightness_only.values.is_empty(),
            "Legacy brightness is never rerouted to exposure compensation"
        );
    }

    #[test]
    fn malformed_import_is_rejected_before_camera_io() {
        let mut preset = Preset::capture("Office", &CameraState::default()).unwrap();
        preset.values.insert("contrast".to_owned(), 101);
        assert!(preset.validate().is_err());
        preset.values.clear();
        preset.values.insert("unknown-control".to_owned(), 0);
        assert!(preset.validate().is_err());
        preset.values.clear();
        preset.values.insert("face-roi".to_owned(), 1);
        assert!(preset.validate().is_err());
    }

    #[test]
    fn upgraded_camera_baseline_can_be_saved_without_altering_out_of_range_readbacks() {
        let mut state = CameraState {
            version: Some(crate::camera::CameraVersion {
                serial: "TEST".to_owned(),
                firmware: "YunxiCamera-ch131b-1.0.4".to_owned(),
                build: 1349,
            }),
            eptz: Some(Crop::centred(1.7).unwrap()),
            hsl: Some(HslState::default()),
            wb_offsets: Some((10, 10)),
            ..Default::default()
        };
        for (key, value) in [
            ("sharpness", 24),
            ("contrast", 48),
            ("exposure-mode", 0),
            ("exposure-iso", 400),
            ("exposure-time", 16666),
            ("wb-mode", 1),
            ("wb-temp", 6408),
            ("focus-mode", 4),
            ("focus-manual", 67),
        ] {
            state.values.insert(key.to_owned(), value);
        }
        state.hsl.as_mut().unwrap().values[0][5] = 5;
        let preset = Preset::capture("Before update", &state).unwrap();
        preset.validate().unwrap();
        for key in [
            "sharpness",
            "exposure-iso",
            "exposure-time",
            "wb-temp",
            "focus-manual",
        ] {
            assert!(!preset.values.contains_key(key), "{key}");
        }
        assert_eq!(preset.omitted_controls.len(), 1);
        assert!(preset.omitted_controls.contains_key("sharpness"));
        assert_eq!(state.values.get("sharpness"), Some(&24));
        assert_eq!(preset.values.get("contrast"), Some(&48));
        assert_eq!(preset.crop, state.eptz);
        assert_eq!(preset.hsl, state.hsl);
        assert!(preset.wb_offsets.is_none());
    }

    #[test]
    fn settings_remember_preview_and_presets_are_replaceable() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::at(temp.path().to_owned()).unwrap();
        assert!(!store.load_settings().unwrap().preview_enabled);
        let settings = Settings {
            preview_enabled: true,
            ..Settings::default()
        };
        store.save_settings(&settings).unwrap();
        assert!(store.load_settings().unwrap().preview_enabled);
        let mut preset = Preset::capture("Office", &CameraState::default()).unwrap();
        store.save_preset(&preset).unwrap();
        preset.values.insert("contrast".to_owned(), 55);
        store.save_preset(&preset).unwrap();
        assert_eq!(
            store.load_preset("Office").unwrap().values.get("contrast"),
            Some(&55)
        );
        store.delete_preset("Office").unwrap();
        assert!(store.presets().unwrap().is_empty());
    }
}
