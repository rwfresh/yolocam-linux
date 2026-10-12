// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    net::Ipv4Addr,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
use yolocam_linux::{
    camera::{Camera, Crop, param_specs, unsupported_control_reason},
    discovery, firmware, gui,
    preview::{Preview, PreviewFrame},
    settings,
};

#[derive(Parser)]
#[command(
    version,
    about = "YoloCam S3 controls, preview and firmware updates for Linux"
)]
struct Args {
    /// Override the camera's automatically discovered USB network address.
    #[arg(long, env = "YOLOCAM_IP", global = true)]
    ip: Option<Ipv4Addr>,
    #[arg(long, default_value_t = 12345, global = true)]
    port: u16,
    /// Start the desktop app with its preview enabled.
    #[arg(long)]
    preview: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the desktop app (also the default with no command).
    Gui,
    /// Show USB network, video path and Linux driver information, without connecting.
    Diagnose,
    /// Read the camera's version and supported controls.
    Status,
    /// Read a control in native units (exposure-ev: 0..16, neutral 8).
    Get { parameter: String },
    /// Set a control and verify readback where the camera supports it.
    Set {
        parameter: String,
        #[arg(allow_negative_numbers = true)]
        value: i32,
    },
    /// List controls and their supported ranges; no camera access.
    Controls,
    /// Read or change electronic pan/tilt/zoom.
    Crop {
        #[arg(long)]
        zoom: Option<f32>,
        #[arg(long, requires = "zoom")]
        left: Option<f32>,
        #[arg(long, requires = "zoom")]
        top: Option<f32>,
        #[arg(long, conflicts_with = "zoom")]
        recall: Option<u8>,
        #[arg(long)]
        save: Option<u8>,
    },
    /// Read or adjust native HSL (colour 0..7, component hue/saturation/lightness).
    Hsl {
        #[arg(long)]
        colour: Option<usize>,
        #[arg(long, requires = "colour")]
        component: Option<String>,
        #[arg(long, requires_all = ["colour", "component"], allow_negative_numbers = true)]
        value: Option<i32>,
        #[arg(long)]
        enable: Option<bool>,
    },
    /// Read or select an already installed colour profile in the active SDR/HDR bank.
    Profile {
        #[arg(long)]
        id: Option<u32>,
        #[arg(long, default_value_t = 0)]
        scene: u32,
    },
    /// Official S3 firmware metadata, package verification and native installation.
    Firmware {
        #[command(subcommand)]
        action: FirmwareAction,
    },
    /// Capture real frames; optional reversible control/image checks, never flashes firmware.
    VerifyCamera {
        #[arg(long)]
        exercise_controls: bool,
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Display the interface with sample values and no camera connection.
    Demo,
}

#[derive(Subcommand)]
enum FirmwareAction {
    /// Compare running firmware with the latest official S3 metadata.
    Check,
    /// Download and verify a package without writing camera firmware.
    Prepare,
    /// Validate a local official package against fresh metadata, without camera writes.
    Verify { package: PathBuf },
    /// Install a newer verified release and verify it after reboot.
    Install {
        /// Explicit confirmation to write firmware. USB must remain connected.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        package: Option<PathBuf>,
    },
}

fn print_json(value: impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn session_lock() -> Result<File> {
    let directory = dirs::config_dir()
        .context("Cannot locate user config directory")?
        .join("yolocam-linux");
    fs::create_dir_all(&directory)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("session.lock"))?;
    file.try_lock_exclusive().context("YoloCam Linux is already using the camera. Close that app before starting another control session.")?;
    Ok(file)
}

fn run() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    if let Some(Command::Get { parameter } | Command::Set { parameter, .. }) = &args.command
        && let Some(reason) = unsupported_control_reason(parameter)
    {
        bail!("{reason}");
    }
    match args.command {
        Some(Command::Diagnose) => return print_json(discovery::diagnostics()),
        Some(Command::Controls) => return print_json(param_specs().iter().map(|p| {
            let unsupported_reason = unsupported_control_reason(p.key);
            json!({"key":p.key,"label":p.label,"section":p.section,"minimum":p.min,"maximum":p.max,"options":p.options,"unit":p.unit,
                "readback":p.readback && unsupported_reason.is_none(),"supported":unsupported_reason.is_none(),"unsupported_reason":unsupported_reason,
                "display_offset":if p.key == "exposure-ev" { -8 } else { 0 }})
        }).collect::<Vec<_>>()),
        Some(Command::Demo) => return gui::run_demo(),
        _ => {}
    }
    // One app/CLI process at a time prevents competing clients from replacing
    // the camera's native control socket. This does not lock OBS's video stream.
    let _session = session_lock()?;
    let mut camera = Camera::new(args.ip, args.port, Duration::from_secs(3));
    if !matches!(args.command, None | Some(Command::Gui)) {
        camera.client.connect()?;
    }
    match args.command {
        None | Some(Command::Gui) => gui::run(args.ip, args.port, args.preview),
        Some(Command::Status) => {
            let state = camera.connect()?;
            print_json(
                json!({"camera":state,"address":camera.client.address(),"linux":discovery::diagnostics()}),
            )
        }
        Some(Command::Get { parameter }) => {
            camera.version()?;
            print_json(json!({"parameter":parameter,"value":camera.get_param(&parameter)?}))
        }
        Some(Command::Set { parameter, value }) => {
            camera.version()?;
            let actual = camera.set_param(&parameter, value)?;
            let readback = param_specs()
                .iter()
                .find(|spec| spec.key == parameter)
                .context("Unknown camera parameter")?
                .readback;
            if readback {
                print_json(
                    json!({"parameter":parameter,"requested":value,"actual":actual,"readback_verified":true}),
                )
            } else {
                print_json(
                    json!({"parameter":parameter,"requested":value,"acknowledged":actual,"readback_verified":false}),
                )
            }
        }
        Some(Command::Crop {
            zoom,
            left,
            top,
            recall,
            save,
        }) => {
            camera.connect()?;
            if let Some(slot) = recall {
                camera.recall_crop(slot)?;
            }
            if let Some(zoom) = zoom {
                let centred = Crop::centred(zoom)?;
                camera.set_crop(Crop::new(
                    zoom,
                    left.unwrap_or(centred.left),
                    top.unwrap_or(centred.top),
                )?)?;
            }
            if let Some(slot) = save {
                camera.save_crop(slot)?;
            }
            print_json(json!({"position":camera.state.eptz,"presets":camera.state.presets}))
        }
        Some(Command::Hsl {
            colour,
            component,
            value,
            enable,
        }) => {
            camera.connect()?;
            if let Some(value) = value {
                let colour = colour.context("Specify --colour 0..7")?;
                ensure!(colour < 8, "Colour must be 0 through 7");
                let component = match component.as_deref() {
                    Some("hue") => 0,
                    Some("saturation") => 1,
                    Some("lightness" | "luminance") => 2,
                    _ => bail!("Component must be hue, saturation or lightness"),
                };
                let mut hsl = camera
                    .state
                    .hsl
                    .clone()
                    .context("Native HSL is unavailable on this firmware")?;
                hsl.values[component][colour] = value;
                camera.set_hsl(hsl)?;
            }
            if let Some(enable) = enable {
                camera.set_param("hsl-enable", i32::from(enable))?;
            }
            print_json(
                json!({"hsl":camera.state.hsl,"enabled":camera.state.values.get("hsl-enable")}),
            )
        }
        Some(Command::Profile { id, scene }) => {
            camera.connect()?;
            if let Some(id) = id {
                camera.select_profile(id, scene)?;
            }
            print_json(
                json!({"selection":camera.state.profile,"information":camera.state.profile_info}),
            )
        }
        Some(Command::Firmware { action }) => {
            let current = camera.version()?;
            let release = firmware::query_release(&current)?;
            let newer = release.newer_than(&current)?;
            let mut progress = |phase: &str, percent: u8| eprintln!("{phase}: {percent}%");
            match action {
                FirmwareAction::Check => {
                    print_json(json!({"current":current,"latest":release,"update_available":newer}))
                }
                FirmwareAction::Prepare => {
                    print_json(firmware::prepare_firmware(&release, None, &mut progress)?.report())
                }
                FirmwareAction::Verify { package } => {
                    print_json(firmware::verify_package(&package, &release)?.report())
                }
                FirmwareAction::Install { yes, package } => {
                    ensure!(
                        yes,
                        "Firmware installation requires --yes. Keep USB connected through upload, flash and reboot."
                    );
                    ensure!(
                        newer,
                        "The camera already runs this firmware or a newer version; no firmware was written."
                    );
                    let verified = match package {
                        Some(package) => firmware::verify_package(&package, &release)?,
                        None => firmware::prepare_firmware(&release, None, &mut progress)?,
                    };
                    let state = camera.refresh()?;
                    let backup = settings::save_update_backup(&state)?;
                    eprintln!("Camera settings backup: {}", backup.display());
                    let outcome =
                        firmware::upgrade(&mut camera.client, &verified, &current, &mut progress);
                    match outcome {
                        Ok(()) => {}
                        Err(error)
                            if error.downcast_ref::<firmware::UpdateUncertain>().is_some() =>
                        {
                            eprintln!(
                                "{error:#}; checking running firmware after reboot without repeating the transfer."
                            );
                        }
                        Err(error) => return Err(error),
                    }
                    camera.disconnect();
                    let running = firmware::verify_after_reboot(
                        &release,
                        &current.serial,
                        args.ip,
                        args.port,
                        Duration::from_secs(180),
                    )?;
                    print_json(json!({"verified_running":running,"backup":backup}))
                }
            }
        }
        Some(Command::VerifyCamera {
            exercise_controls,
            seconds,
            output,
        }) => {
            let report = verify_camera(&mut camera, exercise_controls, seconds)?;
            if let Some(path) = output {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, serde_json::to_vec_pretty(&report)?)?;
            }
            print_json(report)
        }
        Some(Command::Diagnose | Command::Controls | Command::Demo) => unreachable!(),
    }
}

fn frame_metrics(frame: &PreviewFrame) -> serde_json::Value {
    let count = (frame.width * frame.height) as f64;
    let (mut sum, mut squares, mut chroma) = (0.0, 0.0, 0.0);
    for p in frame.rgb.as_chunks::<3>().0 {
        let (r, g, b) = (f64::from(p[0]), f64::from(p[1]), f64::from(p[2]));
        let y = 0.299 * r + 0.587 * g + 0.114 * b;
        sum += y;
        squares += y * y;
        let maximum = r.max(g).max(b);
        if maximum > 0.0 {
            chroma += 255.0 * (maximum - r.min(g).min(b)) / maximum;
        }
    }
    let mean = sum / count;
    json!({"luma_mean":mean,"luma_std":(squares/count-mean*mean).max(0.0).sqrt(),
           "mean_chroma":chroma/count,"width":frame.width,"height":frame.height,"fps":frame.fps})
}

fn newest(preview: &Preview) -> Result<PreviewFrame> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(error) = preview.error() {
            bail!(error);
        }
        if let Some(frame) = preview.latest() {
            return Ok(frame);
        }
        thread::sleep(Duration::from_millis(20));
    }
    bail!("No camera frames arrived")
}

fn verify_camera(camera: &mut Camera, exercise: bool, seconds: u64) -> Result<serde_json::Value> {
    let baseline = camera.connect()?;
    let mut preview = Preview::new();
    preview.start(None)?;
    let start = Instant::now();
    let mut report =
        json!({"camera":baseline.version,"baseline":baseline,"exercised_controls":exercise});
    let checks = (|| -> Result<()> {
        let initial = newest(&preview)?;
        let initial_sequence = initial.sequence;
        report["initial_frame"] = frame_metrics(&initial);
        if exercise {
            let contrast = *baseline
                .values
                .get("contrast")
                .context("Contrast is unavailable")?;
            let saturation = *baseline
                .values
                .get("saturation")
                .context("Saturation is unavailable")?;
            camera.set_param("contrast", 0)?;
            thread::sleep(Duration::from_millis(750));
            let low = frame_metrics(&newest(&preview)?);
            camera.set_param("contrast", 100)?;
            thread::sleep(Duration::from_millis(750));
            let high = frame_metrics(&newest(&preview)?);
            camera.set_param("contrast", contrast)?;
            ensure!(
                (high["luma_std"].as_f64().unwrap_or(0.0)
                    - low["luma_std"].as_f64().unwrap_or(0.0))
                .abs()
                    > 5.0,
                "Contrast acknowledgement did not produce a measurable video change"
            );
            report["contrast"] = json!({"zero":low,"hundred":high,"actual_image_change":true});
            camera.set_param("saturation", 0)?;
            thread::sleep(Duration::from_millis(750));
            let mono = frame_metrics(&newest(&preview)?);
            camera.set_param("saturation", 100)?;
            thread::sleep(Duration::from_millis(750));
            let colour = frame_metrics(&newest(&preview)?);
            camera.set_param("saturation", saturation)?;
            ensure!(
                colour["mean_chroma"].as_f64().unwrap_or(0.0)
                    - mono["mean_chroma"].as_f64().unwrap_or(0.0)
                    > 10.0,
                "Saturation acknowledgement did not produce a measurable video change"
            );
            report["saturation"] = json!({"zero":mono,"hundred":colour,"actual_image_change":true});
            if baseline.eptz.is_some() {
                report["zoom_readback"] =
                    serde_json::to_value(camera.set_crop(Crop::centred(2.0)?)?)?;
                thread::sleep(Duration::from_millis(750));
                report["zoom_frame"] = frame_metrics(&newest(&preview)?);
                camera.set_crop(baseline.eptz.clone().context("Missing original crop")?)?;
            }
            camera.client.abort_transport();
            let actual = camera.set_param("contrast", 33)?;
            ensure!(
                actual == 33,
                "Control did not recover after closing its socket"
            );
            camera.set_param("contrast", contrast)?;
            report["connection_reestablished"] = json!(true);
        }
        let mut heartbeat = Instant::now();
        while start.elapsed() < Duration::from_secs(seconds) {
            if let Some(error) = preview.error() {
                bail!(error);
            }
            ensure!(
                preview.is_running(),
                "{}",
                preview.error().unwrap_or_else(|| "Video stopped".into())
            );
            if heartbeat.elapsed() > Duration::from_secs(25) {
                camera.heartbeat()?;
                heartbeat = Instant::now();
            }
            thread::sleep(Duration::from_millis(100));
        }
        let final_frame = newest(&preview)?;
        ensure!(
            final_frame.sequence > initial_sequence,
            "Preview did not deliver new frames"
        );
        report["final_frame"] = frame_metrics(&final_frame);
        report["frames_captured"] = json!(final_frame.sequence);
        report["duration_seconds"] = json!(start.elapsed().as_secs_f32());
        Ok(())
    })();
    let mut restoration = Vec::new();
    if exercise {
        for name in ["contrast", "saturation"] {
            if let Some(value) = baseline.values.get(name)
                && let Err(error) = camera.set_param(name, *value)
            {
                restoration.push(format!("{name}: {error:#}"));
            }
        }
        if let Some(crop) = baseline.eptz
            && let Err(error) = camera.set_crop(crop)
        {
            restoration.push(format!("crop: {error:#}"));
        }
    }
    preview.stop();
    camera.disconnect();
    ensure!(
        restoration.is_empty(),
        "Failed to restore test settings: {}",
        restoration.join("; ")
    );
    checks?;
    report["settings_restored"] = json!(true);
    report["success"] = json!(true);
    Ok(report)
}

fn main() {
    if let Err(error) = run() {
        eprintln!("YoloCam Linux: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negative_control_values_parse_without_a_separator() {
        let args = Args::try_parse_from(["yolocam-linux", "set", "sharpness", "-7"]).unwrap();
        assert!(matches!(args.command, Some(Command::Set { value: -7, .. })));
        let args = Args::try_parse_from([
            "yolocam-linux",
            "hsl",
            "--colour",
            "5",
            "--component",
            "hue",
            "--value",
            "-10",
        ])
        .unwrap();
        assert!(matches!(
            args.command,
            Some(Command::Hsl {
                value: Some(-10),
                ..
            })
        ));
    }
}
