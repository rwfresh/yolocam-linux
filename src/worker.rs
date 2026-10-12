// SPDX-License-Identifier: GPL-3.0-or-later
//! One serialized owner of the camera control connection.
//!
//! Slider drags are coalesced per control. A failed absolute write is followed
//! by a fresh connection and readback before a deliberate retry; firmware and
//! camera-preset commands are never replayed automatically.

use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, VecDeque},
    net::Ipv4Addr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    camera::{Camera, CameraState, CameraVersion, Crop, HslState},
    firmware::{self, FirmwareRelease, VerifiedFirmware},
    settings::{self, Preset},
};

#[derive(Clone, Debug)]
pub enum Command {
    Connect,
    Refresh,
    SetParam {
        key: String,
        value: i32,
    },
    SetCrop(Crop),
    SetHslField {
        component: usize,
        colour: usize,
        value: i32,
    },
    ResetHsl,
    SetWbOffsets(u32, u32),
    SelectProfile {
        id: u32,
        scene: u32,
    },
    SaveCrop(u8),
    RecallCrop(u8),
    UploadClut(PathBuf),
    ResetClut,
    CapturePreset(String),
    ApplyPreset(Box<Preset>),
    CheckFirmware,
    PrepareFirmware,
    InstallFirmware,
    VerifyUpdate,
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct PreparedSummary {
    pub release: FirmwareRelease,
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
    pub md5: String,
    pub image_md5: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Connection(bool),
    State(Box<CameraState>),
    Status { message: String, error: bool },
    Busy(bool),
    FirmwareRelease(Box<FirmwareRelease>),
    FirmwarePrepared(Box<PreparedSummary>),
    FirmwareProgress { phase: String, percent: u8 },
    FirmwareFinished(CameraVersion),
    FirmwareFailed { message: String, uncertain: bool },
    PresetCaptured(Box<Preset>),
}

pub struct Worker {
    tx: Sender<Command>,
    rx: Receiver<Event>,
    busy: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn new(ip: Option<Ipv4Addr>, port: u16) -> Self {
        let (tx, commands) = mpsc::channel();
        let (events, rx) = mpsc::channel();
        let busy = Arc::new(AtomicBool::new(false));
        let thread_busy = Arc::clone(&busy);
        let join = thread::Builder::new()
            .name("yolocam-control".to_owned())
            .spawn(move || run_worker(commands, events, thread_busy, ip, port))
            .expect("Cannot start camera worker");
        Self {
            tx,
            rx,
            busy,
            join: Some(join),
        }
    }

    pub fn send(&self, command: Command) -> Result<()> {
        if self.busy.load(Ordering::Acquire) && !matches!(command, Command::Shutdown) {
            bail!("The firmware operation is still running; camera controls are paused");
        }
        self.tx
            .send(command)
            .context("The camera worker has stopped")
    }

    pub fn poll(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    pub fn shutdown(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
        // The GUI cannot close while a committed firmware operation is running.
        // Normal shutdown is bounded by the camera's network timeout.
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Default)]
struct Pending {
    commands: VecDeque<Command>,
}

impl Pending {
    fn push(&mut self, command: Command) {
        // Coalescing stops at a command barrier: moving a slider before and
        // after applying a preset must retain that order.
        if let Some(key) = coalescing_key(&command) {
            let barrier = self
                .commands
                .iter()
                .rposition(|c| coalescing_key(c).is_none());
            let start = barrier.map_or(0, |index| index + 1);
            let mut index = start;
            while index < self.commands.len() {
                if coalescing_key(&self.commands[index]).as_ref() == Some(&key) {
                    self.commands.remove(index);
                } else {
                    index += 1;
                }
            }
        }
        self.commands.push_back(command);
    }
}

fn coalescing_key(command: &Command) -> Option<String> {
    match command {
        Command::SetParam { key, .. } => Some(format!("param:{key}")),
        Command::SetCrop(_) => Some("crop".to_owned()),
        Command::SetHslField {
            component, colour, ..
        } => Some(format!("hsl:{component}:{colour}")),
        Command::SetWbOffsets(_, _) => Some("wb-offsets".to_owned()),
        Command::SelectProfile { .. } => Some("profile".to_owned()),
        _ => None,
    }
}

fn status(events: &Sender<Event>, message: impl Into<String>, error: bool) {
    let _ = events.send(Event::Status {
        message: message.into(),
        error,
    });
}

fn transport_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
}

fn target_matches(command: &Command, state: &CameraState) -> bool {
    match command {
        Command::SetParam { key, value } => state.values.get(key) == Some(value),
        Command::SetCrop(crop) => state.eptz.as_ref().is_some_and(|actual| {
            (actual.zoom - crop.zoom).abs() < 0.02
                && (actual.left - crop.left).abs() < 0.002
                && (actual.top - crop.top).abs() < 0.002
        }),
        Command::SetHslField {
            component,
            colour,
            value,
        } => state.hsl.as_ref().is_some_and(|hsl| {
            *component < 3 && *colour < 8 && hsl.values[*component][*colour] == *value
        }),
        Command::ResetHsl => state
            .hsl
            .as_ref()
            .is_some_and(|hsl| hsl.values.iter().flatten().all(|v| *v == 0)),
        Command::SetWbOffsets(red, blue) => state.wb_offsets == Some((*red, *blue)),
        Command::SelectProfile { id, scene } => state
            .profile
            .as_ref()
            .is_some_and(|p| p.id == *id && p.scene_id == *scene),
        _ => false,
    }
}

fn perform_absolute(camera: &mut Camera, command: &Command) -> Result<()> {
    match command {
        Command::SetParam { key, value } => {
            let applied = camera.set_param(key, *value)?;
            if applied != *value {
                bail!("Camera returned {applied} for {key}; requested {value}");
            }
        }
        Command::SetCrop(crop) => {
            camera.set_crop(crop.clone())?;
        }
        Command::SetHslField {
            component,
            colour,
            value,
        } => {
            if *component >= 3 || *colour >= 8 || !(-100..=100).contains(value) {
                bail!("Invalid HSL adjustment");
            }
            let mut hsl = camera
                .state
                .hsl
                .clone()
                .context("This camera does not expose HSL adjustments")?;
            hsl.values[*component][*colour] = *value;
            camera.set_hsl(hsl)?;
        }
        Command::ResetHsl => {
            camera.set_hsl(HslState {
                values: [[0; 8]; 3],
            })?;
        }
        Command::SetWbOffsets(red, blue) => {
            camera.set_wb_offsets(*red, *blue)?;
        }
        Command::SelectProfile { id, scene } => {
            camera.select_profile(*id, *scene)?;
        }
        _ => bail!("This command is not an absolute camera control"),
    }
    if !target_matches(command, &camera.state) {
        bail!("The camera did not confirm the requested value");
    }
    Ok(())
}

fn absolute_with_recovery(
    camera: &mut Camera,
    command: &Command,
    events: &Sender<Event>,
) -> Result<()> {
    match perform_absolute(camera, command) {
        Ok(()) => Ok(()),
        Err(error) if transport_error(&error) && command_has_readback(command) => {
            status(
                events,
                "Control connection interrupted; reconnecting and checking the camera…",
                false,
            );
            camera.disconnect();
            camera
                .connect()
                .context("Cannot restore the camera control connection")?;
            if target_matches(command, &camera.state) {
                return Ok(());
            }
            perform_absolute(camera, command)
        }
        Err(error) => Err(error),
    }
}

fn command_has_readback(command: &Command) -> bool {
    match command {
        Command::SetParam { key, .. } => crate::camera::param_specs()
            .iter()
            .find(|spec| spec.key == key)
            .is_some_and(|spec| spec.readback),
        Command::SetCrop(_)
        | Command::SetHslField { .. }
        | Command::ResetHsl
        | Command::SetWbOffsets(_, _)
        | Command::SelectProfile { .. } => true,
        _ => false,
    }
}

fn run_worker(
    commands: Receiver<Command>,
    events: Sender<Event>,
    busy: Arc<AtomicBool>,
    ip: Option<Ipv4Addr>,
    port: u16,
) {
    let mut camera = Camera::new(ip, port, Duration::from_secs(3));
    let mut connected = false;
    let mut reconnect_at = Instant::now();
    let mut heartbeat_at = Instant::now();
    let mut pending = Pending::default();
    let mut release: Option<FirmwareRelease> = None;
    let mut verified: Option<VerifiedFirmware> = None;
    let mut expected_update: Option<(FirmwareRelease, CameraVersion)> = None;

    loop {
        if pending.commands.is_empty() {
            match commands.recv_timeout(Duration::from_millis(200)) {
                Ok(command) => pending.push(command),
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
        for command in commands.try_iter() {
            pending.push(command);
        }
        if pending
            .commands
            .iter()
            .any(|c| matches!(c, Command::Shutdown))
        {
            break;
        }

        if !connected && Instant::now() >= reconnect_at && expected_update.is_none() {
            status(&events, "Connecting to YoloCam…", false);
            match camera.connect() {
                Ok(state) => {
                    connected = true;
                    heartbeat_at = Instant::now() + Duration::from_secs(30);
                    let _ = events.send(Event::Connection(true));
                    let _ = events.send(Event::State(Box::new(state)));
                    status(
                        &events,
                        "Camera connected. Controls show the values read from your camera.",
                        false,
                    );
                }
                Err(error) => {
                    camera.disconnect();
                    reconnect_at = Instant::now() + Duration::from_secs(5);
                    let _ = events.send(Event::Connection(false));
                    status(
                        &events,
                        format!("Camera unavailable: {error:#}. Retrying…"),
                        true,
                    );
                }
            }
        }

        if connected && Instant::now() >= heartbeat_at {
            match camera.heartbeat() {
                Ok(()) => heartbeat_at = Instant::now() + Duration::from_secs(30),
                Err(error) => {
                    connected = false;
                    camera.disconnect();
                    reconnect_at = Instant::now() + Duration::from_secs(1);
                    let _ = events.send(Event::Connection(false));
                    status(
                        &events,
                        format!("Camera connection lost: {error:#}. Reconnecting…"),
                        true,
                    );
                }
            }
        }

        let Some(command) = pending.commands.pop_front() else {
            continue;
        };

        if matches!(command, Command::Connect) {
            if connected {
                camera.disconnect();
                connected = false;
            }
            reconnect_at = Instant::now();
            continue;
        }

        if matches!(command, Command::VerifyUpdate) {
            let Some((expected, before)) = expected_update.as_ref() else {
                status(&events, "No firmware update is awaiting verification", true);
                continue;
            };
            busy.store(true, Ordering::Release);
            let _ = events.send(Event::Busy(true));
            status(
                &events,
                "Waiting for the camera to reboot and verifying its firmware…",
                false,
            );
            match firmware::verify_after_reboot(
                expected,
                &before.serial,
                ip,
                port,
                Duration::from_secs(120),
            ) {
                Ok(version) => {
                    let _ = events.send(Event::FirmwareFinished(version));
                    expected_update = None;
                    verified = None;
                    connected = false;
                    reconnect_at = Instant::now();
                }
                Err(error) => {
                    let _ = events.send(Event::FirmwareFailed {
                        message: format!("{error:#}"),
                        uncertain: true,
                    });
                }
            }
            busy.store(false, Ordering::Release);
            let _ = events.send(Event::Busy(false));
            continue;
        }

        if !connected {
            status(
                &events,
                "Camera controls are unavailable until the control connection is restored",
                true,
            );
            continue;
        }

        if matches!(
            command,
            Command::CheckFirmware | Command::PrepareFirmware | Command::InstallFirmware
        ) {
            busy.store(true, Ordering::Release);
            let _ = events.send(Event::Busy(true));
            let mut progress = |phase: &str, percent: u8| {
                let _ = events.send(Event::FirmwareProgress {
                    phase: phase.to_owned(),
                    percent,
                });
            };
            let result = (|| -> Result<()> {
                let current = camera.version()?;
                match command {
                    Command::CheckFirmware => {
                        verified = None;
                        let found = firmware::query_release(&current)?;
                        found.newer_than(&current)?;
                        let _ = events.send(Event::FirmwareRelease(Box::new(found.clone())));
                        release = Some(found);
                        status(&events, "Official firmware check completed", false);
                    }
                    Command::PrepareFirmware => {
                        let found = release
                            .as_ref()
                            .context("Check for firmware updates first")?;
                        if !found.newer_than(&current)? {
                            bail!("The camera already has this firmware or a newer version");
                        }
                        // Long downloads do not hold an idle camera TCP session.
                        camera.disconnect();
                        connected = false;
                        let _ = events.send(Event::Connection(false));
                        let cache = dirs::cache_dir()
                            .map(|base| base.join(settings::APP_DIRECTORY).join("firmware"));
                        let prepared =
                            firmware::prepare_firmware(found, cache.as_deref(), &mut progress)?;
                        let summary = PreparedSummary {
                            release: prepared.release.clone(),
                            path: prepared.path.clone(),
                            size: prepared.size,
                            sha256: prepared.sha256.clone(),
                            md5: prepared.md5.clone(),
                            image_md5: prepared.image_md5.clone(),
                        };
                        verified = Some(prepared);
                        let _ = events.send(Event::FirmwarePrepared(Box::new(summary)));
                        status(
                            &events,
                            "Firmware downloaded and verified. Review it before installing.",
                            false,
                        );
                        reconnect_at = Instant::now();
                    }
                    Command::InstallFirmware => {
                        let package = verified
                            .as_ref()
                            .context("Download and verify the firmware first")?;
                        if !package.release.newer_than(&current)? {
                            bail!(
                                "The verified package is no longer newer than the camera firmware"
                            );
                        }
                        let snapshot = camera.refresh()?;
                        let backup = settings::save_update_backup(&snapshot)?;
                        status(
                            &events,
                            format!(
                                "Saved camera settings to {}. Keep the USB cable attached.",
                                backup.display()
                            ),
                            false,
                        );
                        match firmware::upgrade(
                            &mut camera.client,
                            package,
                            &current,
                            &mut progress,
                        ) {
                            Ok(()) => {}
                            Err(error)
                                if error.downcast_ref::<firmware::UpdateUncertain>().is_some() =>
                            {
                                status(
                                    &events,
                                    format!(
                                        "{error:#}. Checking the running firmware after reboot without repeating the transfer…"
                                    ),
                                    false,
                                );
                            }
                            Err(error) => return Err(error),
                        }
                        expected_update = Some((package.release.clone(), current.clone()));
                        camera.disconnect();
                        connected = false;
                        let _ = events.send(Event::Connection(false));
                        progress("reboot", 0);
                        let version = firmware::verify_after_reboot(
                            &package.release,
                            &current.serial,
                            ip,
                            port,
                            Duration::from_secs(120),
                        )?;
                        let _ = events.send(Event::FirmwareFinished(version));
                        expected_update = None;
                        verified = None;
                        reconnect_at = Instant::now();
                    }
                    _ => unreachable!(),
                }
                Ok(())
            })();
            if let Err(error) = result {
                let installing = matches!(command, Command::InstallFirmware);
                let uncertain = installing
                    && (error.downcast_ref::<firmware::UpdateUncertain>().is_some()
                        || expected_update.is_some());
                if installing {
                    camera.disconnect();
                    connected = false;
                    let _ = events.send(Event::Connection(false));
                    if !uncertain {
                        expected_update = None;
                    }
                }
                let message = format!("{error:#}");
                status(&events, message.clone(), true);
                let _ = events.send(Event::FirmwareFailed { message, uncertain });
            }
            busy.store(false, Ordering::Release);
            let _ = events.send(Event::Busy(false));
            continue;
        }

        let result = match &command {
            Command::Refresh => camera.refresh().map(|_| ()),
            Command::SetParam { .. }
            | Command::SetCrop(_)
            | Command::SetHslField { .. }
            | Command::ResetHsl
            | Command::SetWbOffsets(_, _)
            | Command::SelectProfile { .. } => {
                absolute_with_recovery(&mut camera, &command, &events)
            }
            Command::SaveCrop(slot) => camera
                .save_crop(*slot)
                .and_then(|()| camera.refresh().map(|_| ())),
            Command::RecallCrop(slot) => camera.recall_crop(*slot).map(|_| ()),
            Command::UploadClut(path) => {
                read_clut(path).and_then(|values| camera.set_clut(&values))
            }
            Command::ResetClut => camera.set_clut(&identity_clut()),
            Command::CapturePreset(name) => camera.refresh().and_then(|state| {
                let preset = Preset::capture(name, &state)?;
                let _ = events.send(Event::PresetCaptured(Box::new(preset)));
                Ok(())
            }),
            Command::ApplyPreset(preset) => apply_preset(&mut camera, preset, &events),
            _ => Ok(()),
        };
        match result {
            Ok(()) => {
                let _ = events.send(Event::State(Box::new(camera.state.clone())));
                let message = match command {
                    Command::Refresh => "Camera values refreshed",
                    Command::CapturePreset(_) => "Camera settings read for preset",
                    Command::ApplyPreset(_) => "Preset applied and verified by the camera",
                    Command::UploadClut(_) | Command::ResetClut => {
                        "Colour LUT accepted by the camera"
                    }
                    Command::SaveCrop(_) => "Crop preset saved on the camera",
                    Command::RecallCrop(_) => "Camera crop preset recalled",
                    Command::SetParam { ref key, .. }
                        if !crate::camera::param_specs()
                            .iter()
                            .find(|spec| spec.key == key)
                            .is_some_and(|spec| spec.readback) =>
                    {
                        "Camera acknowledged the command; its current value cannot be read back"
                    }
                    _ => "Camera confirmed the requested value",
                };
                let message = if let Command::ApplyPreset(preset) = &command {
                    match preset.normalized().omission_note() {
                        Some(note) => format!("{message}. {note}"),
                        None => message.to_owned(),
                    }
                } else {
                    message.to_owned()
                };
                status(&events, message, false);
            }
            Err(error) => {
                status(&events, format!("{error:#}"), true);
                if transport_error(&error) || !camera.client.is_connected() {
                    connected = false;
                    camera.disconnect();
                    reconnect_at = Instant::now() + Duration::from_secs(1);
                    let _ = events.send(Event::Connection(false));
                }
                let _ = events.send(Event::State(Box::new(camera.state.clone())));
            }
        }
    }
    camera.disconnect();
}

fn apply_preset(camera: &mut Camera, preset: &Preset, events: &Sender<Event>) -> Result<()> {
    let preset = preset.normalized();
    preset.validate()?;
    if preset.values.contains_key("exposure-ev")
        && preset
            .values
            .get("exposure-mode")
            .or_else(|| camera.state.values.get("exposure-mode"))
            != Some(&0)
    {
        bail!("Preset exposure compensation requires Auto exposure mode");
    }
    // Set the controlling modes first, then their dependent values. Applying a
    // preset is deliberate and stops at the first rejected or failed write.
    for key in ["hdr", "exposure-mode", "wb-mode", "focus-mode"] {
        if let Some(value) = preset.values.get(key) {
            status(
                events,
                format!("Applying preset {}: {key}", preset.name),
                false,
            );
            perform_absolute(
                camera,
                &Command::SetParam {
                    key: key.to_owned(),
                    value: *value,
                },
            )?;
        }
    }
    // Lock snapshots the current AE shutter/gain, rather than the compensation
    // target. Keep exposure unlocked through the whole preset, then let AE
    // settle before taking the final lock snapshot. Partial presets restore
    // the original lock when they did not request a persistent lock change.
    let original_lock = if preset.values.contains_key("exposure-ev")
        || preset.values.contains_key("exposure-lock")
    {
        Some(camera.get_param("exposure-lock")?)
    } else {
        None
    };
    let desired_lock = preset
        .values
        .get("exposure-lock")
        .copied()
        .or(original_lock);
    let mut lock_change_requested = false;
    let result: Result<()> = (|| {
        if preset.values.contains_key("exposure-ev")
            && original_lock.is_some_and(|value| value != 0)
        {
            // Record the cleanup obligation before writing: a lost ACK can
            // still mean that the unlock reached the camera.
            lock_change_requested = true;
            write_exposure_lock(camera, 0, events)?;
        }
        apply_preset_values(camera, &preset, events)?;
        if let Some(value) = desired_lock
            && camera.state.values.get("exposure-lock") != Some(&value)
        {
            lock_change_requested = true;
            write_exposure_lock(camera, value, events)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if lock_change_requested && let Some(value) = original_lock {
            status(events, "Restoring the original exposure lock", false);
            if let Err(restore) = write_exposure_lock(camera, value, events) {
                return Err(error.context(format!(
                    "Could not restore the original exposure lock: {restore:#}"
                )));
            }
        }
        return Err(error);
    }
    Ok(())
}

fn write_exposure_lock(camera: &mut Camera, value: i32, events: &Sender<Event>) -> Result<()> {
    if value == 1 {
        status(
            events,
            "Waiting for Auto exposure to settle before locking",
            false,
        );
        thread::sleep(Duration::from_secs(3));
    }
    perform_absolute(
        camera,
        &Command::SetParam {
            key: "exposure-lock".to_owned(),
            value,
        },
    )
}

fn apply_preset_values(camera: &mut Camera, preset: &Preset, events: &Sender<Event>) -> Result<()> {
    if let Some(value) = preset.values.get("exposure-ev") {
        status(
            events,
            format!("Applying preset {}: exposure compensation", preset.name),
            false,
        );
        perform_absolute(
            camera,
            &Command::SetParam {
                key: "exposure-ev".to_owned(),
                value: *value,
            },
        )?;
    }
    for (key, value) in &preset.values {
        if [
            "hdr",
            "exposure-mode",
            "wb-mode",
            "focus-mode",
            "exposure-ev",
            "exposure-lock",
        ]
        .contains(&key.as_str())
        {
            continue;
        }
        if key == "focus-manual" && camera.state.values.get("focus-mode") != Some(&2) {
            bail!("Preset manual focus requires manual focus mode");
        }
        status(
            events,
            format!("Applying preset {}: {key}", preset.name),
            false,
        );
        perform_absolute(
            camera,
            &Command::SetParam {
                key: key.clone(),
                value: *value,
            },
        )?;
    }
    if let Some(crop) = &preset.crop {
        perform_absolute(camera, &Command::SetCrop(crop.clone()))?;
    }
    if let Some(hsl) = &preset.hsl {
        camera.set_hsl(hsl.clone())?;
        if camera
            .state
            .hsl
            .as_ref()
            .is_none_or(|actual| actual.values != hsl.values)
        {
            bail!("Camera did not confirm the preset HSL adjustments");
        }
    }
    if let Some((red, blue)) = preset.wb_offsets {
        perform_absolute(camera, &Command::SetWbOffsets(red, blue))?;
    }
    if let Some(profile) = &preset.profile {
        let current_hdr = u32::from(camera.state.values.get("hdr").is_some_and(|v| *v != 0));
        if profile.hdr_flag != current_hdr {
            bail!("Preset colour profile belongs to a different HDR bank");
        }
        perform_absolute(
            camera,
            &Command::SelectProfile {
                id: profile.id,
                scene: profile.scene_id,
            },
        )?;
    }
    Ok(())
}

pub fn read_clut(path: &std::path::Path) -> Result<Vec<u32>> {
    let file =
        std::fs::read(path).with_context(|| format!("Cannot open LUT {}", path.display()))?;
    if file.len() > 1024 * 1024 {
        bail!("A LUT JSON file must be smaller than 1 MiB");
    }
    let value: serde_json::Value = serde_json::from_slice(&file)
        .context("LUT must be a JSON array or an object with a values array")?;
    let entries = value
        .as_array()
        .or_else(|| value.get("values").and_then(serde_json::Value::as_array))
        .context("LUT must contain a values array")?;
    if entries.len() != 5508 {
        bail!(
            "YoloCam LUTs require exactly 5,508 entries; this file contains {}",
            entries.len()
        );
    }
    entries
        .iter()
        .map(|entry| {
            let value = entry
                .as_u64()
                .context("LUT entries must be unsigned integers")?;
            if value > 0x3fff_ffff {
                bail!("LUT entries must contain packed 30-bit RGB deltas (0…1,073,741,823)");
            }
            Ok(value as u32)
        })
        .collect()
}

pub fn identity_clut() -> Vec<u32> {
    // HiSilicon 17×17×17 LUT, in an interleaved 5,508-entry octant layout.
    // Each entry packs three signed 10-bit RGB deltas; zero is neutral.
    vec![0; 5508]
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    type ExposureRequests = Vec<(i32, i32, Option<u32>, Instant)>;

    struct ExposureFixture {
        port: u16,
        stop: Arc<AtomicBool>,
        values: Arc<Mutex<BTreeMap<i32, u32>>>,
        requests: Arc<Mutex<ExposureRequests>>,
        thread: Option<JoinHandle<()>>,
    }

    impl ExposureFixture {
        fn new(rejected_control: Option<i32>, reject_relock: bool) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let values = Arc::new(Mutex::new(BTreeMap::from([
                (14, 0),
                (15, 16),
                (106, 1),
                (9, 48),
            ])));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let thread_stop = Arc::clone(&stop);
            let thread_values = Arc::clone(&values);
            let thread_requests = Arc::clone(&requests);
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_read_timeout(Some(Duration::from_secs(8)))
                                .unwrap();
                            let _ = serve_exposure(
                                &mut stream,
                                &thread_stop,
                                &thread_values,
                                &thread_requests,
                                rejected_control,
                                reject_relock,
                            );
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5))
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                port,
                stop,
                values,
                requests,
                thread: Some(thread),
            }
        }

        fn camera(&self) -> Camera {
            let mut camera =
                Camera::new(Some(Ipv4Addr::LOCALHOST), self.port, Duration::from_secs(2));
            camera.client.connect().unwrap();
            camera.version().unwrap();
            for key in ["exposure-mode", "exposure-ev", "exposure-lock", "contrast"] {
                camera.get_param(key).unwrap();
            }
            camera
        }

        fn writes(&self) -> Vec<(i32, u32)> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(kind, cid, value, _)| {
                    if *kind == 2 && *cid != 215 {
                        value.map(|value| (*cid, value))
                    } else {
                        None
                    }
                })
                .collect()
        }

        fn assert_no_legacy_brightness_requests(&self) {
            assert!(
                !self
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, cid, _, _)| *cid == 7)
            );
        }

        fn assert_settled_final_lock(&self) {
            let requests = self.requests.lock().unwrap();
            let writes: Vec<_> = requests
                .iter()
                .filter(|(kind, cid, _, _)| *kind == 2 && *cid != 215)
                .collect();
            let (_, cid, value, locked_at) = writes.last().unwrap();
            assert_eq!((*cid, *value), (106, Some(1)));
            let (_, _, _, last_control_at) = writes
                .iter()
                .rev()
                .find(|(_, cid, _, _)| *cid != 106)
                .unwrap();
            assert!(locked_at.duration_since(*last_control_at) >= Duration::from_secs(3));
        }
    }

    impl Drop for ExposureFixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    fn serve_exposure(
        stream: &mut TcpStream,
        stop: &AtomicBool,
        values: &Mutex<BTreeMap<i32, u32>>,
        requests: &Mutex<ExposureRequests>,
        rejected_control: Option<i32>,
        reject_relock: bool,
    ) -> Result<()> {
        use crate::pb::{self, command_body::Value};
        while !stop.load(Ordering::Acquire) {
            let mut header = [0; 8];
            stream.read_exact(&mut header)?;
            let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            if length > crate::protocol::MAX_PAYLOAD_SIZE + 8 {
                bail!("invalid request");
            }
            let mut frame = header.to_vec();
            frame.resize(8 + length, 0);
            stream.read_exact(&mut frame[8..])?;
            let request = pb::Message::decode(crate::protocol::unpack_frame(&frame)?.as_slice())?;
            let raw = request.body.and_then(|body| body.value).and_then(|value| {
                if let Value::IntVal(value) = value {
                    Some(value)
                } else {
                    None
                }
            });
            requests
                .lock()
                .unwrap()
                .push((request.r#type, request.id, raw, Instant::now()));
            let mut response = pb::Message {
                r#type: 4,
                seq: request.seq,
                id: request.id,
                resp_code: 100,
                ..Default::default()
            };
            let body = match (request.r#type, request.id, raw) {
                (2, 215, _) => Some(Value::IntVal(1)),
                (1, 1, _) => Some(Value::VersionInfo(pb::VersionInfo {
                    sys_version: "YunxiCamera-ch131b-1.0.4".to_owned(),
                    sys_code: "1349".to_owned(),
                    serial_num: "PRESET-TEST".to_owned(),
                    ..Default::default()
                })),
                (1, cid, _) => values.lock().unwrap().get(&cid).copied().map(Value::IntVal),
                (2, cid, Some(_)) if Some(cid) == rejected_control => {
                    response.resp_code = 410;
                    None
                }
                (2, 106, Some(1)) if reject_relock => {
                    response.resp_code = 411;
                    None
                }
                (2, cid, Some(value)) => {
                    let mut values = values.lock().unwrap();
                    if cid == 15 && (values.get(&14) != Some(&0) || values.get(&106) != Some(&0)) {
                        None
                    } else {
                        values.insert(cid, value);
                        Some(Value::IntVal(value))
                    }
                }
                _ => None,
            };
            if let Some(value) = body {
                response.resp_code = 200;
                response.body = Some(pb::CommandBody { value: Some(value) });
            }
            stream.write_all(&crate::protocol::pack_frame(&response.encode_to_vec()))?;
        }
        Ok(())
    }

    #[test]
    fn legacy_locked_and_partial_presets_restore_compensation_then_lock_without_cid7() {
        for include_lock in [true, false] {
            let fixture = ExposureFixture::new(None, false);
            let mut camera = fixture.camera();
            let mut preset = Preset::capture("Legacy", &camera.state).unwrap();
            preset.values.insert("brightness".to_owned(), 50);
            preset.values.insert("exposure-ev".to_owned(), 12);
            preset.values.insert("contrast".to_owned(), 55);
            if !include_lock {
                preset.values.remove("exposure-lock");
                preset.values.remove("exposure-mode");
            }
            let (events, received) = mpsc::channel();
            apply_preset(&mut camera, &preset, &events).unwrap();
            let writes = fixture.writes();
            let exposure_writes: Vec<_> = writes
                .iter()
                .copied()
                .filter(|(cid, _)| [15, 106].contains(cid))
                .collect();
            assert_eq!(exposure_writes, vec![(106, 0), (15, 12), (106, 1)]);
            assert!(
                writes.contains(&(9, 55)),
                "Other preset settings remain applicable"
            );
            fixture.assert_settled_final_lock();
            assert!(received.try_iter().any(|event| matches!(event,
                Event::Status { message, error: false }
                if message == "Waiting for Auto exposure to settle before locking"
            )));
            assert_eq!(fixture.values.lock().unwrap().get(&15), Some(&12));
            assert_eq!(fixture.values.lock().unwrap().get(&106), Some(&1));
            fixture.assert_no_legacy_brightness_requests();
            assert!(
                preset
                    .normalized()
                    .omission_note()
                    .unwrap()
                    .contains("brightness")
            );
            camera.disconnect();
        }
    }

    #[test]
    fn rejected_compensation_restores_prior_lock_without_retry_and_reports_cleanup_failures() {
        for reject_relock in [false, true] {
            let fixture = ExposureFixture::new(Some(15), reject_relock);
            let mut camera = fixture.camera();
            let mut preset = Preset::capture("Partial", &camera.state).unwrap();
            preset.values.clear();
            preset.values.insert("exposure-ev".to_owned(), 12);
            let (events, _) = mpsc::channel();
            let error = apply_preset(&mut camera, &preset, &events).unwrap_err();
            assert_eq!(fixture.writes(), vec![(106, 0), (15, 12), (106, 1)]);
            assert_eq!(fixture.values.lock().unwrap().get(&15), Some(&16));
            if reject_relock {
                let message = format!("{error:#}");
                assert!(
                    message.contains("410") && message.contains("411"),
                    "{message}"
                );
            } else {
                assert_eq!(fixture.values.lock().unwrap().get(&106), Some(&1));
            }
            fixture.assert_no_legacy_brightness_requests();
            camera.disconnect();
        }
    }

    #[test]
    fn later_preset_failure_restores_the_original_lock_after_settling_without_replaying_ev() {
        for reject_relock in [false, true] {
            let fixture = ExposureFixture::new(Some(9), reject_relock);
            let mut camera = fixture.camera();
            let mut preset = Preset::capture("Partial", &camera.state).unwrap();
            preset.values.clear();
            preset.values.insert("exposure-ev".to_owned(), 12);
            preset.values.insert("contrast".to_owned(), 55);
            let (events, _) = mpsc::channel();
            let error = apply_preset(&mut camera, &preset, &events).unwrap_err();
            assert_eq!(
                fixture.writes(),
                vec![(106, 0), (15, 12), (9, 55), (106, 1)]
            );
            fixture.assert_settled_final_lock();
            assert_eq!(fixture.values.lock().unwrap().get(&15), Some(&12));
            assert_eq!(fixture.values.lock().unwrap().get(&9), Some(&48));
            if reject_relock {
                let message = format!("{error:#}");
                assert!(
                    message.contains("410") && message.contains("411"),
                    "{message}"
                );
            } else {
                assert_eq!(fixture.values.lock().unwrap().get(&106), Some(&1));
            }
            fixture.assert_no_legacy_brightness_requests();
            camera.disconnect();
        }
    }

    #[test]
    fn preset_without_compensation_preserves_an_existing_lock_without_unlocking() {
        let fixture = ExposureFixture::new(None, false);
        let mut camera = fixture.camera();
        let mut preset = Preset::capture("Locked colour", &camera.state).unwrap();
        preset.values.clear();
        preset.values.insert("brightness".to_owned(), 39);
        preset.values.insert("contrast".to_owned(), 55);
        preset.values.insert("exposure-lock".to_owned(), 1);
        let (events, _) = mpsc::channel();
        apply_preset(&mut camera, &preset, &events).unwrap();
        assert_eq!(fixture.writes(), vec![(9, 55)]);
        assert_eq!(fixture.values.lock().unwrap().get(&106), Some(&1));
        assert_eq!(fixture.values.lock().unwrap().get(&15), Some(&16));
        fixture.assert_no_legacy_brightness_requests();
        camera.disconnect();
    }

    #[test]
    fn unlocked_preset_does_not_wait_or_add_an_unrequested_exposure_lock() {
        let fixture = ExposureFixture::new(None, false);
        let mut camera = fixture.camera();
        let mut preset = Preset::capture("Unlocked", &camera.state).unwrap();
        preset.values.clear();
        preset.values.insert("exposure-ev".to_owned(), 12);
        preset.values.insert("contrast".to_owned(), 55);
        preset.values.insert("exposure-lock".to_owned(), 0);
        let (events, received) = mpsc::channel();
        apply_preset(&mut camera, &preset, &events).unwrap();
        assert_eq!(fixture.writes(), vec![(106, 0), (15, 12), (9, 55)]);
        assert!(!received.try_iter().any(|event| matches!(event,
            Event::Status { message, .. }
            if message == "Waiting for Auto exposure to settle before locking"
        )));
        assert_eq!(fixture.values.lock().unwrap().get(&106), Some(&0));
        camera.disconnect();
    }

    struct FakeCamera {
        port: u16,
        stop: Arc<AtomicBool>,
        writes: Arc<AtomicU32>,
        value: Arc<AtomicU32>,
        thread: Option<JoinHandle<()>>,
    }

    impl FakeCamera {
        fn lost_first_ack(applied_before_disconnect: bool) -> Self {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let writes = Arc::new(AtomicU32::new(0));
            let value = Arc::new(AtomicU32::new(42));
            let thread_stop = Arc::clone(&stop);
            let thread_writes = Arc::clone(&writes);
            let thread_value = Arc::clone(&value);
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            stream
                                .set_read_timeout(Some(Duration::from_millis(500)))
                                .unwrap();
                            let _ = fake_connection(
                                &mut stream,
                                &thread_stop,
                                &thread_writes,
                                &thread_value,
                                applied_before_disconnect,
                            );
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                port,
                stop,
                writes,
                value,
                thread: Some(thread),
            }
        }
    }

    impl Drop for FakeCamera {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }

    fn fake_connection(
        stream: &mut TcpStream,
        stop: &AtomicBool,
        writes: &AtomicU32,
        value: &AtomicU32,
        applied_before_disconnect: bool,
    ) -> Result<()> {
        use crate::pb::{self, command_body::Value};
        while !stop.load(Ordering::Acquire) {
            let mut header = [0; 8];
            stream.read_exact(&mut header)?;
            let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
            if length > crate::protocol::MAX_PAYLOAD_SIZE + 8 {
                bail!("invalid request");
            }
            let mut frame = header.to_vec();
            frame.resize(8 + length, 0);
            stream.read_exact(&mut frame[8..])?;
            let request = pb::Message::decode(crate::protocol::unpack_frame(&frame)?.as_slice())?;
            let mut response = pb::Message {
                r#type: 4,
                seq: request.seq,
                id: request.id,
                resp_code: 100,
                ..Default::default()
            };
            let body = match (request.r#type, request.id) {
                (2, 215) => Some(Value::IntVal(1)),
                (1, 1) => Some(Value::VersionInfo(pb::VersionInfo {
                    sys_version: "YunxiCamera-ch131b-1.0.4".to_owned(),
                    sys_code: "1349".to_owned(),
                    serial_num: "LOCALHOST-TEST".to_owned(),
                    ..Default::default()
                })),
                (1, 9) => Some(Value::IntVal(value.load(Ordering::Acquire))),
                (2, 9 | 214) => {
                    let Some(Value::IntVal(requested)) = request.body.and_then(|body| body.value)
                    else {
                        bail!("invalid write");
                    };
                    let count = writes.fetch_add(1, Ordering::AcqRel);
                    if count == 0 {
                        if applied_before_disconnect {
                            value.store(requested, Ordering::Release);
                        }
                        // Simulate the exact ambiguous write boundary: the
                        // client cannot infer whether the camera applied it.
                        return Ok(());
                    }
                    value.store(requested, Ordering::Release);
                    Some(Value::IntVal(requested))
                }
                _ => None,
            };
            if let Some(value) = body {
                response.resp_code = 200;
                response.body = Some(pb::CommandBody { value: Some(value) });
            }
            stream.write_all(&crate::protocol::pack_frame(&response.encode_to_vec()))?;
        }
        Ok(())
    }

    fn wait_for_state(worker: &Worker, value: i32) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            for event in worker.poll() {
                if let Event::State(state) = event
                    && state.values.get("contrast") == Some(&value)
                {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("Worker did not confirm the requested value {value}");
    }

    #[test]
    fn lost_ack_reads_the_camera_before_deciding_whether_to_send_again() {
        for applied_before_disconnect in [true, false] {
            let fake = FakeCamera::lost_first_ack(applied_before_disconnect);
            let mut worker = Worker::new(Some(Ipv4Addr::LOCALHOST), fake.port);
            wait_for_state(&worker, 42);
            worker
                .send(Command::SetParam {
                    key: "contrast".to_owned(),
                    value: 73,
                })
                .unwrap();
            wait_for_state(&worker, 73);
            assert_eq!(fake.value.load(Ordering::Acquire), 73);
            assert_eq!(
                fake.writes.load(Ordering::Acquire),
                if applied_before_disconnect { 1 } else { 2 }
            );
            worker.shutdown();
        }
    }

    #[test]
    fn slider_bursts_keep_the_latest_value_without_losing_other_controls() {
        let mut queue = Pending::default();
        for value in 0..100 {
            queue.push(Command::SetParam {
                key: "contrast".to_owned(),
                value,
            });
        }
        queue.push(Command::SetParam {
            key: "saturation".to_owned(),
            value: 70,
        });
        queue.push(Command::SetParam {
            key: "contrast".to_owned(),
            value: 42,
        });
        assert_eq!(queue.commands.len(), 2);
        assert!(
            matches!(queue.commands.pop_front(), Some(Command::SetParam { key, value: 70 }) if key == "saturation")
        );
        assert!(
            matches!(queue.commands.pop_front(), Some(Command::SetParam { key, value: 42 }) if key == "contrast")
        );
    }

    #[test]
    fn different_hsl_fields_survive_coalescing_and_presets_are_barriers() {
        let mut queue = Pending::default();
        queue.push(Command::SetHslField {
            component: 0,
            colour: 0,
            value: 10,
        });
        queue.push(Command::SetHslField {
            component: 1,
            colour: 0,
            value: 20,
        });
        queue.push(Command::SetHslField {
            component: 0,
            colour: 0,
            value: 30,
        });
        assert_eq!(queue.commands.len(), 2);
        queue.push(Command::ResetHsl);
        queue.push(Command::SetHslField {
            component: 0,
            colour: 0,
            value: 40,
        });
        assert_eq!(queue.commands.len(), 4);
    }

    #[test]
    fn write_only_command_lost_ack_is_reported_without_auto_replay() {
        let fake = FakeCamera::lost_first_ack(true);
        let mut worker = Worker::new(Some(Ipv4Addr::LOCALHOST), fake.port);
        wait_for_state(&worker, 42);
        worker
            .send(Command::SetParam {
                key: "face-roi".to_owned(),
                value: 1,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut reported = false;
        while Instant::now() < deadline && !reported {
            reported = worker
                .poll()
                .iter()
                .any(|event| matches!(event, Event::Status { error: true, .. }));
            thread::sleep(Duration::from_millis(10));
        }
        worker.shutdown();
        assert!(reported, "The uncertain write must be visible to the user");
        assert_eq!(fake.writes.load(Ordering::Acquire), 1);
    }

    #[test]
    fn malformed_lut_is_rejected_before_device_writes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("lut.json");
        std::fs::write(&path, "[0,1,2]").unwrap();
        assert!(read_clut(&path).is_err());
        let mut values = vec![0; 5508];
        values[0] = 0x4000_0000;
        std::fs::write(&path, serde_json::to_vec(&values).unwrap()).unwrap();
        assert!(read_clut(&path).is_err());
        values[0] = 0x3fff_ffff;
        std::fs::write(&path, serde_json::to_vec(&values).unwrap()).unwrap();
        assert_eq!(read_clut(&path).unwrap(), values);
    }
}
