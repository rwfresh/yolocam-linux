// SPDX-License-Identifier: GPL-3.0-or-later
//! Direct V4L2 mmap capture and JPEG decoding. No Python, ffmpeg, or video server.
use anyhow::{Context, Result, bail};
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

#[derive(Clone, Debug)]
pub struct PreviewFrame {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    pub sequence: u64,
    pub fps: f32,
}

#[derive(Default)]
struct Shared {
    frame: Option<PreviewFrame>,
    error: Option<String>,
    last_frame_at: Option<Instant>,
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
        if self.is_running() {
            return Ok(());
        }
        self.stop();
        // Each capture session owns its stop flag and frame mailbox. A delayed
        // old V4L2 thread cannot be reactivated by a subsequent preview start.
        self.running = Arc::new(AtomicBool::new(false));
        self.shared = Arc::new(Mutex::new(Shared::default()));
        let device = device
            .or_else(crate::discovery::discover_video)
            .context("YoloCam video device not found. Connect the camera by USB.")?;
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
                    camera
                        .start(&rscam::Config {
                            interval: (1, 30),
                            resolution: (1280, 720),
                            format: b"MJPG",
                            nbuffers: 2,
                            ..Default::default()
                        })
                        .with_context(|| {
                            format!("Cannot capture {path}. Pause capture in other camera apps.")
                        })?;
                    let _ = ready_tx.send(Ok::<(), String>(()));
                    let mut sequence = 0u64;
                    let mut counter = 0u32;
                    let mut measured = Instant::now();
                    let mut fps = 0.0;
                    while running.load(Ordering::Acquire) {
                        let frame = camera
                            .capture()
                            .context("Camera stopped delivering video")?;
                        let image =
                            image::load_from_memory_with_format(&frame, image::ImageFormat::Jpeg)
                                .context("Camera delivered an invalid JPEG frame")?
                                .to_rgb8();
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
                        };
                        if let Ok(mut data) = shared.lock() {
                            data.frame = Some(output);
                            data.last_frame_at = Some(Instant::now());
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
        if let Some(done) = self.stopped.take() {
            // A normal V4L2 stop takes one frame. Never leave the desktop UI
            // indefinitely waiting on a disconnected or unresponsive device.
            if done.recv_timeout(Duration::from_secs(1)).is_ok()
                && let Some(thread) = self.thread.take()
            {
                let _ = thread.join();
            }
        }
        self.thread.take();
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub fn latest(&self) -> Option<PreviewFrame> {
        self.shared.lock().ok()?.frame.clone()
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
