// SPDX-License-Identifier: GPL-3.0-or-later
//! Direct V4L2 mmap capture and JPEG decoding. No Python, ffmpeg, or video server.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// The exact V4L2 frame interval is retained, including fractional frame rates.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PreviewMode {
    pub width: u32,
    pub height: u32,
    pub interval: (u32, u32),
}

impl Default for PreviewMode {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            interval: (1, 30),
        }
    }
}

impl PreviewMode {
    pub fn fps(self) -> f64 {
        if self.interval.0 == 0 {
            0.0
        } else {
            self.interval.1 as f64 / self.interval.0 as f64
        }
    }

    pub fn label(self) -> String {
        let fps = self.fps();
        let rate = if (fps - fps.round()).abs() < 0.0001 {
            format!("{fps:.0}")
        } else {
            format!("{fps:.2}")
        };
        format!("{} × {} · {rate} fps", self.width, self.height)
    }

    pub fn restriction_reason(self) -> Option<String> {
        if self.width == 0 || self.height == 0 || self.interval.0 == 0 || self.interval.1 == 0 {
            Some("The saved preview mode has invalid dimensions or frame interval".into())
        } else if (self.width >= 3840 || self.height >= 2160)
            && crate::compatibility::exceeds_hdr_limit(self.fps())
        {
            Some(
                "4K preview supports at most 30 fps; select a lower frame rate or resolution"
                    .into(),
            )
        } else {
            None
        }
    }

    pub fn validate(self) -> Result<()> {
        if let Some(reason) = self.restriction_reason() {
            bail!("{reason}");
        }
        Ok(())
    }

    fn matches(self, other: Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.interval.0 != 0
            && other.interval.0 != 0
            && self.interval.0 as u64 * other.interval.1 as u64
                == other.interval.0 as u64 * self.interval.1 as u64
    }
}

#[derive(Clone, Debug)]
pub struct PreviewCapabilities {
    pub device: PathBuf,
    pub modes: Vec<PreviewMode>,
    pub warnings: Vec<String>,
}

impl PreviewCapabilities {
    pub fn supports(&self, mode: PreviewMode) -> bool {
        self.modes.iter().any(|available| available.matches(mode))
    }
}

/// Read-only format enumeration. This does not start capture or change CID5.
pub fn enumerate(device: Option<PathBuf>) -> Result<PreviewCapabilities> {
    let device = resolve_device(device)?;
    let path = device.to_str().context("Invalid video device path")?;
    let camera = rscam::new(path)
        .with_context(|| format!("Cannot inspect video modes on {}", device.display()))?;
    enumerate_camera(&camera, device)
}

fn resolve_device(device: Option<PathBuf>) -> Result<PathBuf> {
    device
        .or_else(crate::discovery::discover_video)
        .context("YoloCam video device not found. Connect the camera by USB.")
}

fn enumerate_camera(camera: &rscam::Camera, device: PathBuf) -> Result<PreviewCapabilities> {
    let mut warnings = Vec::new();
    let sizes = resolution_choices(
        camera
            .resolutions(b"MJPG")
            .context("The video device did not enumerate MJPEG resolutions")?,
        &mut warnings,
    );
    let mut modes = Vec::new();
    for (width, height) in sizes {
        match camera.intervals(b"MJPG", (width, height)) {
            Ok(intervals) => {
                modes.extend(interval_choices(intervals, &mut warnings).into_iter().map(
                    |interval| PreviewMode {
                        width,
                        height,
                        interval,
                    },
                ));
            }
            Err(error) => warnings.push(format!(
                "Cannot enumerate MJPEG frame intervals for {width} × {height}: {error}"
            )),
        }
    }
    modes.sort_by(|a, b| {
        (a.width, a.height)
            .cmp(&(b.width, b.height))
            .then_with(|| a.fps().total_cmp(&b.fps()))
    });
    modes.dedup_by(|a, b| a.matches(*b));
    ensure!(
        !modes.is_empty(),
        "The video device did not advertise any MJPEG capture modes"
    );
    Ok(PreviewCapabilities {
        device,
        modes,
        warnings,
    })
}

fn resolution_choices(info: rscam::ResolutionInfo, warnings: &mut Vec<String>) -> Vec<(u32, u32)> {
    let mut sizes = match info {
        rscam::ResolutionInfo::Discretes(sizes) => sizes,
        rscam::ResolutionInfo::Stepwise { min, max, step } => {
            warnings.push("The device advertises a size range; showing matching common sizes and range boundaries".into());
            [min, (1280, 720), (1920, 1080), (3840, 2160), max]
                .into_iter()
                .filter(|&(w, h)| {
                    let matches = |v: u32, lo: u32, hi: u32, increment: u32| {
                        v >= lo && v <= hi && (increment == 0 || (v - lo).is_multiple_of(increment))
                    };
                    matches(w, min.0, max.0, step.0) && matches(h, min.1, max.1, step.1)
                })
                .collect()
        }
    };
    sizes.retain(|(w, h)| *w != 0 && *h != 0);
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

fn fraction_cmp(a: (u32, u32), b: (u32, u32)) -> std::cmp::Ordering {
    (a.0 as u64 * b.1 as u64).cmp(&(b.0 as u64 * a.1 as u64))
}

fn interval_choices(info: rscam::IntervalInfo, warnings: &mut Vec<String>) -> Vec<(u32, u32)> {
    let intervals = match info {
        rscam::IntervalInfo::Discretes(intervals) => intervals,
        rscam::IntervalInfo::Stepwise { min, max, step } => {
            warnings.push("The device advertises an interval range; showing matching common frame rates and range boundaries".into());
            [min, (1, 25), (1, 30), (1, 50), (1, 60), max]
                .into_iter()
                .filter(|&candidate| {
                    if candidate.0 == 0 || candidate.1 == 0 || min.1 == 0 || max.1 == 0 {
                        return false;
                    }
                    if fraction_cmp(candidate, min).is_lt() || fraction_cmp(candidate, max).is_gt()
                    {
                        return false;
                    }
                    if step.0 == 0 {
                        return true;
                    }
                    if step.1 == 0 {
                        return false;
                    }
                    let delta =
                        candidate.0 as u128 * min.1 as u128 - min.0 as u128 * candidate.1 as u128;
                    let numerator = delta * step.1 as u128;
                    let denominator = candidate.1 as u128 * min.1 as u128 * step.0 as u128;
                    numerator.is_multiple_of(denominator)
                })
                .collect()
        }
    };
    let mut intervals: Vec<_> = intervals
        .into_iter()
        .filter(|(n, d)| *n != 0 && *d != 0)
        .map(|(n, d)| {
            let (mut a, mut b) = (n, d);
            while b != 0 {
                (a, b) = (b, a % b);
            }
            (n / a, d / a)
        })
        .collect();
    intervals.sort_by(|a, b| fraction_cmp(*a, *b));
    intervals.dedup();
    intervals
}

#[derive(Clone, Debug)]
pub struct PreviewFrame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    pub sequence: u64,
    pub fps: f32,
    /// Monotonic producer time, before JPEG decoding or any UI mailbox delay.
    pub captured_at: Instant,
}

#[derive(Default)]
struct Shared {
    frame: Option<PreviewFrame>,
    error: Option<String>,
    last_frame_at: Option<Instant>,
    mode: Option<PreviewMode>,
    device: Option<PathBuf>,
}

pub struct Preview {
    running: Arc<AtomicBool>,
    shared: Arc<Mutex<Shared>>,
    thread: Option<JoinHandle<()>>,
    stopped: Option<mpsc::Receiver<()>>,
}

impl Default for Preview {
    fn default() -> Self {
        Self::new()
    }
}

impl Preview {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            shared: Arc::new(Mutex::new(Shared::default())),
            thread: None,
            stopped: None,
        }
    }

    pub fn start(&mut self, device: Option<PathBuf>) -> Result<()> {
        self.start_mode(device, PreviewMode::default())
    }

    pub fn start_mode(&mut self, device: Option<PathBuf>, mode: PreviewMode) -> Result<()> {
        mode.validate()?;
        let device = resolve_device(device)?;
        if self.is_running()
            && self
                .active_mode()
                .is_some_and(|active| active.matches(mode))
            && self
                .shared
                .lock()
                .ok()
                .is_some_and(|data| data.device.as_ref() == Some(&device))
        {
            return Ok(());
        }
        self.stop();
        ensure!(
            !self.is_releasing(),
            "The previous preview session is still releasing video capture. Wait, then retry preview."
        );
        // Each capture session owns its stop flag and frame mailbox. A delayed
        // old V4L2 thread cannot be reactivated by a subsequent preview start.
        self.running = Arc::new(AtomicBool::new(false));
        self.shared = Arc::new(Mutex::new(Shared::default()));
        let path = device
            .to_str()
            .context("Invalid video device path")?
            .to_owned();
        let running = Arc::clone(&self.running);
        let shared = Arc::clone(&self.shared);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::channel();
        if let Ok(mut data) = self.shared.lock() {
            *data = Shared {
                last_frame_at: Some(Instant::now()),
                ..Shared::default()
            };
        }
        running.store(true, Ordering::Release);
        self.thread = Some(thread::Builder::new().name("video-preview".into()).spawn(
            move || {
                let capture = || -> Result<()> {
                    let mut camera = rscam::new(&path).with_context(|| {
                        format!("Cannot open {path}. Check camera access permissions.")
                    })?;
                    let capabilities = enumerate_camera(&camera, PathBuf::from(&path))?;
                    ensure!(capabilities.supports(mode), "{} is not advertised by this video device. Refresh video modes and choose an available mode.", mode.label());
                    camera
                        .start(&rscam::Config {
                            interval: mode.interval,
                            resolution: (mode.width, mode.height),
                            format: b"MJPG",
                            nbuffers: 2,
                            ..Default::default()
                        })
                        .with_context(|| {
                            format!("Cannot capture {path}. Pause capture in other camera apps.")
                        })?;
                    // rscam rejects a silently substituted format or interval;
                    // these values therefore describe the negotiated stream.
                    if let Ok(mut data) = shared.lock() {
                        data.mode = Some(mode);
                        data.device = Some(PathBuf::from(&path));
                    }
                    let _ = ready_tx.send(Ok::<(), String>(()));
                    let mut sequence = 0u64;
                    let mut counter = 0u32;
                    let mut measured = Instant::now();
                    let mut fps = 0.0;
                    while running.load(Ordering::Acquire) {
                        let frame = camera
                            .capture()
                            .context("Camera stopped delivering video")?;
                        let captured_at = Instant::now();
                        let image =
                            image::load_from_memory_with_format(&frame, image::ImageFormat::Jpeg)
                                .context("Camera delivered an invalid JPEG frame")?
                                .to_rgb8();
                        ensure!(image.width() == mode.width && image.height() == mode.height,
                            "Camera delivered {} × {} frames for negotiated {} × {} capture",
                            image.width(), image.height(), mode.width, mode.height);
                        sequence += 1;
                        counter += 1;
                        let elapsed = measured.elapsed().as_secs_f32();
                        if elapsed >= 0.5 {
                            fps = counter as f32 / elapsed;
                            measured = Instant::now();
                            counter = 0;
                        }
                        let output = PreviewFrame {
                            width: image.width() as usize,
                            height: image.height() as usize,
                            rgb: image.into_raw(),
                            sequence,
                            fps,
                            captured_at,
                        };
                        if let Ok(mut data) = shared.lock() {
                            data.frame = Some(output);
                            data.last_frame_at = Some(captured_at);
                        }
                    }
                    camera.stop().context("Releasing camera capture")?;
                    Ok(())
                };
                if let Err(error) = capture() {
                    let text = format!("{error:#}");
                    let _ = ready_tx.send(Err(text.clone()));
                    if let Ok(mut data) = shared.lock() {
                        data.error = Some(text);
                    }
                }
                running.store(false, Ordering::Release);
                let _ = done_tx.send(());
            },
        )?);
        self.stopped = Some(done_rx);
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                self.stop();
                bail!(error)
            }
            Err(error) => {
                self.stop();
                Err(error).context("Starting camera preview")
            }
        }
    }

    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(done) = &self.stopped {
            // A normal V4L2 stop takes one frame. Never leave the desktop UI
            // indefinitely waiting on a disconnected or unresponsive device.
            if !matches!(
                done.recv_timeout(Duration::from_secs(1)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                self.stopped = None;
                if let Some(thread) = self.thread.take() {
                    let _ = thread.join();
                }
            }
        }
        // Keep ownership if capture is blocked: a subsequent start must not
        // race a still-open old V4L2 session or revive its frame mailbox.
    }

    pub fn is_releasing(&mut self) -> bool {
        if !self.is_running()
            && let Some(done) = &self.stopped
            && !matches!(done.try_recv(), Err(mpsc::TryRecvError::Empty))
        {
            self.stopped = None;
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
        self.stopped.is_some() && !self.is_running()
    }

    pub fn active_mode(&self) -> Option<PreviewMode> {
        if !self.is_running() {
            return None;
        }
        self.shared.lock().ok()?.mode
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn latest(&self) -> Option<PreviewFrame> {
        self.shared.lock().ok()?.frame.clone()
    }

    /// Avoid copying a large RGB frame when the desktop already displays it.
    pub fn latest_since(&self, sequence: u64) -> Option<PreviewFrame> {
        let data = self.shared.lock().ok()?;
        data.frame
            .as_ref()
            .filter(|frame| frame.sequence != sequence)
            .cloned()
    }

    pub fn error(&self) -> Option<String> {
        let data = self.shared.lock().ok()?;
        if let Some(error) = &data.error {
            return Some(error.clone());
        }
        if self.is_running()
            && data
                .last_frame_at
                .is_some_and(|time| time.elapsed() > Duration::from_secs(5))
        {
            return Some("Camera has not delivered a frame for five seconds. Restart preview or check the USB connection.".into());
        }
        None
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fractional_intervals_remain_exact_and_match_equivalent_advertised_modes() {
        let mode = PreviewMode {
            width: 1920,
            height: 1080,
            interval: (1001, 30000),
        };
        assert!((mode.fps() - 29.97002997).abs() < 0.000001);
        assert!(mode.label().contains("29.97 fps"));
        let equivalent = PreviewMode {
            interval: (2002, 60000),
            ..mode
        };
        let capabilities = PreviewCapabilities {
            device: PathBuf::from("/not-a-device"),
            modes: vec![mode],
            warnings: Vec::new(),
        };
        assert!(capabilities.supports(equivalent));
        assert!(!capabilities.supports(PreviewMode {
            interval: (1, 30),
            ..mode
        }));
    }

    #[test]
    fn enumeration_keeps_only_advertised_discrete_and_valid_stepwise_choices() {
        let mut warnings = Vec::new();
        assert_eq!(
            resolution_choices(
                rscam::ResolutionInfo::Discretes(vec![(640, 480), (0, 0), (640, 480)]),
                &mut warnings
            ),
            vec![(640, 480)]
        );
        assert!(warnings.is_empty());
        assert_eq!(
            interval_choices(
                rscam::IntervalInfo::Discretes(vec![(2, 60), (1, 25), (0, 1), (1, 30)]),
                &mut warnings
            ),
            vec![(1, 30), (1, 25)]
        );
        assert!(warnings.is_empty());
        let sizes = resolution_choices(
            rscam::ResolutionInfo::Stepwise {
                min: (640, 480),
                max: (3840, 2160),
                step: (160, 240),
            },
            &mut warnings,
        );
        assert!(sizes.contains(&(1280, 720)));
        assert!(
            !sizes.contains(&(1920, 1080)),
            "Height 1080 does not lie on the actual advertised grid"
        );
        let rates = interval_choices(
            rscam::IntervalInfo::Stepwise {
                min: (1, 60),
                max: (1, 25),
                step: (1, 60),
            },
            &mut warnings,
        );
        assert_eq!(rates, vec![(1, 60), (1, 30)]);
        assert!(!warnings.is_empty());
    }

    #[test]
    fn four_k_above_thirty_and_invalid_saved_modes_fail_before_device_access() {
        let mut preview = Preview::new();
        for mode in [
            PreviewMode {
                width: 3840,
                height: 2160,
                interval: (1, 60),
            },
            PreviewMode {
                interval: (0, 30),
                ..PreviewMode::default()
            },
        ] {
            let error = preview
                .start_mode(Some(PathBuf::from("/not-a-device")), mode)
                .unwrap_err();
            assert!(!error.to_string().contains("Cannot open"));
            assert!(!preview.is_running());
        }
        assert!(
            PreviewMode {
                width: 3840,
                height: 2160,
                interval: (1001, 30000)
            }
            .validate()
            .is_ok()
        );
        assert!(
            PreviewMode {
                width: 3840,
                height: 2160,
                interval: (333_333, 10_000_000)
            }
            .validate()
            .is_ok()
        );
        assert!(
            PreviewMode {
                width: 3840,
                height: 2160,
                interval: (100, 3001)
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn delayed_capture_release_prevents_a_competing_session_and_can_be_reaped() {
        let mut preview = Preview::new();
        let (release, release_requested) = mpsc::channel();
        let (done, stopped) = mpsc::channel();
        preview.stopped = Some(stopped);
        preview.thread = Some(thread::spawn(move || {
            release_requested.recv().unwrap();
            done.send(()).unwrap();
        }));
        let error = preview
            .start_mode(Some(PathBuf::from("/not-a-device")), PreviewMode::default())
            .unwrap_err();
        assert!(error.to_string().contains("still releasing"));
        assert!(preview.is_releasing());
        release.send(()).unwrap();
        preview.stop();
        assert!(!preview.is_releasing());
        assert!(preview.thread.is_none());
        assert!(!preview.is_running());
    }

    #[test]
    fn mailbox_reads_keep_producer_age_during_the_two_to_five_second_stall_window() {
        let preview = Preview::new();
        let captured_at = Instant::now() - Duration::from_secs(3);
        preview.running.store(true, Ordering::Release);
        {
            let mut data = preview.shared.lock().unwrap();
            data.frame = Some(PreviewFrame {
                width: 1,
                height: 1,
                rgb: vec![0, 0, 0],
                sequence: 7,
                fps: 30.0,
                captured_at,
            });
            data.last_frame_at = Some(captured_at);
        }
        let frame = preview.latest_since(6).unwrap();
        assert_eq!(frame.captured_at, captured_at);
        assert!(frame.captured_at.elapsed() > Duration::from_secs(2));
        assert!(preview.latest_since(7).is_none());
        assert!(
            preview.error().is_none(),
            "Focus must stop before the separate five-second capture error"
        );
        preview.running.store(false, Ordering::Release);
    }

    #[test]
    fn stalled_capture_is_reported_instead_of_showing_an_old_frame_as_live() {
        let preview = Preview::new();
        preview.running.store(true, Ordering::Release);
        preview.shared.lock().unwrap().last_frame_at =
            Some(Instant::now() - Duration::from_secs(6));
        assert!(preview.error().unwrap().contains("not delivered a frame"));
        preview.running.store(false, Ordering::Release);
        assert!(preview.error().is_none());
    }
}
