// SPDX-License-Identifier: GPL-3.0-or-later
// Port of the verified yolocam-linux S3 updater and native protocol.
// Protocol research: yolocam-linux, Copyright (C) 2026 Seth Hillbrand.
//! Official S3 metadata, bounded package validation, and native firmware upload.
//!
//! Downloading and validating are read-only camera operations. Only [`upgrade`]
//! writes firmware. Its last packet commits the camera's native update; a lost
//! acknowledgement thereafter is an uncertain outcome, never a reason to replay.

use crate::{camera::CameraVersion, pb, protocol::Client};
use anyhow::{Context, Result, bail, ensure};
use flate2::read::MultiGzDecoder;
use md5::Md5;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const VERSION_ENDPOINT: &str = "https://b-api.yunxi.tv/api/box/version-query";
pub const PACKAGE_HOST: &str = "yunxi-material-library-prod.oss-accelerate.aliyuncs.com";
pub const PACKAGE_PREFIX: &str = "/vendor/assistant/";
pub const MODEL_PREFIX: &str = "YunxiCamera-ch131b-";
pub const CHUNK_SIZE: usize = 2 * 1024 * 1024;
pub const MAX_PACKAGE_SIZE: u64 = 256 * 1024 * 1024;
pub const FLASH_TIMEOUT: Duration = Duration::from_secs(300);
const METADATA_LIMIT: u64 = 1024 * 1024;
const IMAGE_LIMITS: [(&str, u64); 3] = [
    ("uImage-fdt", 64 * 1024 * 1024),
    ("rootfs.ext4.img", 768 * 1024 * 1024),
    ("app.ext4.img", 512 * 1024 * 1024),
];
const UPDATE_ROM: u32 = 2;
const TRANSFORM_FILE: u32 = 3;
const EVENT_UPGRADE: i32 = 28;

/// The camera explicitly rejected its native update. A failure event differs
/// from a disconnected socket: the latter cannot establish whether it succeeded.
#[derive(Debug, thiserror::Error)]
#[error("Camera updater failed (code {code})")]
pub struct CameraUpdateFailed {
    pub code: u32,
}

/// The last packet may have started flashing. Verify the running firmware before
/// taking further action; the application must never retry this transfer itself.
#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub struct UpdateUncertain {
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FirmwareRelease {
    pub firmware: String,
    pub build: u32,
    pub url: String,
    pub description: String,
}

impl FirmwareRelease {
    pub fn version(&self) -> Result<String> {
        Ok(parse_version(&self.firmware)?.0.to_owned())
    }

    pub fn filename(&self) -> String {
        format!("{}.tar.gz", self.firmware)
    }

    pub fn validate(&self) -> Result<()> {
        self.version()?;
        // Compare the complete URL, rather than normalizing paths, credentials,
        // encoded separators or ports into something resembling an official URL.
        let expected = format!("https://{PACKAGE_HOST}{PACKAGE_PREFIX}{}", self.filename());
        ensure!(
            self.url == expected && self.build > 0,
            "Release does not identify an official S3 firmware package"
        );
        Ok(())
    }

    pub fn newer_than(&self, camera: &CameraVersion) -> Result<bool> {
        let latest = parse_version(&self.firmware)?.1;
        let current = parse_version(&camera.firmware)?.1;
        Ok(latest > current || (latest == current && self.build > camera.build))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifiedFirmware {
    pub path: PathBuf,
    pub release: FirmwareRelease,
    pub size: u64,
    pub sha256: String,
    pub md5: String,
    pub image_md5: BTreeMap<String, String>,
}

impl VerifiedFirmware {
    pub fn report(&self) -> serde_json::Value {
        serde_json::json!({
            "path": self.path,
            "size": self.size,
            "sha256": self.sha256,
            "md5": self.md5,
            "images": self.image_md5,
            "release": self.release,
            "metadata_source": VERSION_ENDPOINT,
        })
    }
}

fn parse_version(value: &str) -> Result<(&str, [u32; 3])> {
    let suffix = value
        .strip_prefix(MODEL_PREFIX)
        .with_context(|| format!("Unsupported firmware model/version: {value:?}"))?;
    let version = if let Some((date, version)) = suffix.split_once('-') {
        ensure!(
            date.len() == 8 && date.bytes().all(|c| c.is_ascii_digit()),
            "Unsupported firmware model/version: {value:?}"
        );
        version
    } else {
        suffix
    };
    let components: Vec<&str> = version.split('.').collect();
    ensure!(
        components.len() == 3
            && components
                .iter()
                .all(|part| !part.is_empty() && part.bytes().all(|c| c.is_ascii_digit())),
        "Unsupported firmware model/version: {value:?}"
    );
    let mut numbers = [0; 3];
    for (number, component) in numbers.iter_mut().zip(components) {
        *number = component
            .parse()
            .context("Firmware version component is out of range")?;
    }
    Ok((version, numbers))
}

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(45))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("YoloCam-Linux/0.1")
        .build()
        .context("Cannot initialize official firmware HTTPS client")
}

fn numeric_field(value: &serde_json::Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

fn parse_release_response(result: &serde_json::Value) -> Result<FirmwareRelease> {
    ensure!(
        result.get("code").and_then(serde_json::Value::as_i64) == Some(0),
        "Firmware service rejected query: {}",
        result
            .get("msg")
            .or_else(|| result.get("code"))
            .unwrap_or(&serde_json::Value::Null)
    );
    let data = result
        .get("data")
        .and_then(serde_json::Value::as_object)
        .context("Incomplete firmware metadata")?;
    ensure!(
        data.get("productType").and_then(numeric_field) == Some(51)
            && data.get("installPackageType").and_then(numeric_field) == Some(2),
        "Firmware service returned another product/package type"
    );
    let release = FirmwareRelease {
        firmware: data
            .get("versionNumber")
            .and_then(serde_json::Value::as_str)
            .context("Incomplete firmware version metadata")?
            .to_owned(),
        build: data
            .get("subVersionNumber")
            .and_then(numeric_field)
            .context("Incomplete firmware build metadata")?,
        url: data
            .get("installPackageUrl")
            .and_then(serde_json::Value::as_str)
            .context("Incomplete firmware package URL")?
            .to_owned(),
        description: data
            .get("desc")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned(),
    };
    release.validate()?;
    Ok(release)
}

pub fn query_release(camera: &CameraVersion) -> Result<FirmwareRelease> {
    parse_version(&camera.firmware)?;
    ensure!(!camera.serial.is_empty(), "Camera has no serial number");
    // These are S3 and English Compose metadata identifiers. No proprietary
    // Compose executable is installed, shipped or executed by this application.
    let payload = serde_json::json!({
        "boxId": camera.serial,
        "productType": 51,
        "installPackageType": 2,
        "softwareVersion": "1.1.2",
        "softwareProductType": 39,
        "firmwareVersion": camera.firmware,
        "firmwareSubVersion": camera.build,
    });
    let response = http_client()?
        .post(VERSION_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("source", "20")
        .header("language", "en-US")
        .json(&payload)
        .send()
        .context("Official firmware query failed")?
        .error_for_status()
        .context("Official firmware service returned an HTTP error")?;
    ensure!(
        response.url().as_str() == VERSION_ENDPOINT && !response.status().is_redirection(),
        "Firmware metadata redirected away from the official service"
    );
    let mut data = Vec::new();
    response.take(METADATA_LIMIT + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= METADATA_LIMIT,
        "Firmware metadata exceeded its size limit"
    );
    let result = serde_json::from_slice(&data).context("Invalid firmware service JSON")?;
    parse_release_response(&result)
}

/// Validate the full compressed archive without extracting anything onto disk.
/// The camera extracts it as root, so archive paths, links, duplicate members,
/// extension headers and oversize partition images are rejected here first.
pub fn verify_package(path: &Path, release: &FirmwareRelease) -> Result<VerifiedFirmware> {
    verify_file(path, release, true)
}

fn verify_file(
    path: &Path,
    release: &FirmwareRelease,
    check_name: bool,
) -> Result<VerifiedFirmware> {
    release.validate()?;
    let path = fs::canonicalize(path).context("Cannot open firmware package")?;
    if check_name {
        ensure!(
            path.file_name().and_then(|name| name.to_str()) == Some(release.filename().as_str()),
            "Package filename must be {}",
            release.filename()
        );
    }
    let mut file = File::open(&path)?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Firmware package is not a regular file");
    let size = metadata.len();
    ensure!(
        (1..=MAX_PACKAGE_SIZE).contains(&size),
        "Invalid firmware package size: {size}"
    );
    let mut sha256 = Sha256::new();
    let mut md5 = Md5::new();
    let mut observed = 0u64;
    let mut block = vec![0; 1024 * 1024];
    loop {
        let count = file.read(&mut block)?;
        if count == 0 {
            break;
        }
        observed += count as u64;
        ensure!(
            observed <= MAX_PACKAGE_SIZE,
            "Firmware package exceeded its size limit"
        );
        sha256.update(&block[..count]);
        md5.update(&block[..count]);
    }
    ensure!(
        observed == size,
        "Firmware package changed during validation"
    );
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let images = verify_archive(file, release)?;
    Ok(VerifiedFirmware {
        path,
        release: release.clone(),
        size,
        sha256: format!("{:x}", sha256.finalize()),
        md5: format!("{:x}", md5.finalize()),
        image_md5: images,
    })
}

fn verify_archive(
    reader: impl Read,
    release: &FirmwareRelease,
) -> Result<BTreeMap<String, String>> {
    let mut archive = tar::Archive::new(MultiGzDecoder::new(reader));
    let mut seen = BTreeSet::new();
    let mut images = BTreeMap::new();
    let mut manifest = None;
    for entry in archive
        .entries()
        .context("Invalid firmware archive")?
        .raw(true)
    {
        let mut entry = entry.context("Invalid firmware archive member")?;
        let bytes = entry.path_bytes();
        let name = std::str::from_utf8(&bytes)
            .context("Non-ASCII firmware archive path")?
            .to_owned();
        let image_limit = IMAGE_LIMITS
            .iter()
            .find(|(image, _)| *image == name)
            .map(|(_, size)| *size);
        let allowed = image_limit.is_some()
            || matches!(
                name.as_str(),
                "update_desc"
                    | "config"
                    | "emmc_ab_hi3519.xml"
                    | "boot_image.bin"
                    | "bl31.bin"
                    | "emmc_env.bin"
            );
        ensure!(
            allowed && seen.insert(name.clone()) && entry.header().entry_type().is_file(),
            "Unsafe or unexpected archive member: {name:?}"
        );
        let size = entry.size();
        ensure!(
            size > 0 && size <= image_limit.unwrap_or(METADATA_LIMIT),
            "Archive member exceeds its partition/metadata limit: {name}"
        );
        if image_limit.is_some() {
            let mut digest = Md5::new();
            let mut block = vec![0; 1024 * 1024];
            let mut observed = 0u64;
            loop {
                let count = entry.read(&mut block).context("Truncated firmware image")?;
                if count == 0 {
                    break;
                }
                observed += count as u64;
                digest.update(&block[..count]);
            }
            ensure!(observed == size, "Truncated firmware image: {name}");
            images.insert(name, format!("{:x}", digest.finalize()));
        } else if name == "update_desc" {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .context("Invalid firmware manifest")?;
            ensure!(
                bytes.len() as u64 == size && bytes.is_ascii(),
                "Invalid firmware manifest encoding"
            );
            manifest = Some(String::from_utf8(bytes)?);
        } else {
            // Consume optional metadata as well, so a truncated member cannot be
            // silently skipped by the tar iterator's seek-to-next-entry logic.
            ensure!(
                std::io::copy(&mut entry, &mut std::io::sink())? == size,
                "Truncated firmware metadata: {name}"
            );
        }
    }
    // Drain gzip to verify CRC/truncation and reject a hidden second archive.
    let mut decoder = archive.into_inner();
    let mut tail = [0; 8192];
    let mut padding = 0u64;
    loop {
        let count = decoder
            .read(&mut tail)
            .context("Invalid firmware gzip stream")?;
        if count == 0 {
            break;
        }
        padding += count as u64;
        ensure!(
            padding <= METADATA_LIMIT && tail[..count].iter().all(|byte| *byte == 0),
            "Unexpected data after firmware archive"
        );
    }
    ensure!(
        images.len() == IMAGE_LIMITS.len() && manifest.is_some(),
        "Firmware archive is missing its manifest or system images"
    );
    let manifest = manifest.unwrap();
    let mut lines = manifest.trim().lines();
    let expected_version = format!("version:{}_{}", release.version()?, release.build);
    ensure!(
        lines.next().map(str::trim) == Some(expected_version.as_str()),
        "Firmware manifest version/build does not match official metadata"
    );
    let mut expected = BTreeMap::new();
    for line in lines {
        let parts: Vec<&str> = line.split_ascii_whitespace().collect();
        ensure!(
            parts.len() == 2
                && parts[0].len() == 32
                && parts[0]
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "Invalid or duplicate manifest checksum"
        );
        ensure!(
            expected
                .insert(parts[1].to_owned(), parts[0].to_owned())
                .is_none(),
            "Invalid or duplicate manifest checksum"
        );
    }
    ensure!(expected == images, "Firmware image checksum mismatch");
    Ok(images)
}

pub fn prepare_firmware(
    release: &FirmwareRelease,
    cache: Option<&Path>,
    progress: &mut dyn FnMut(&str, u8),
) -> Result<VerifiedFirmware> {
    release.validate()?;
    let directory = match cache {
        Some(path) => path.to_owned(),
        None => dirs::cache_dir()
            .context("Cannot find a firmware cache directory")?
            .join("yolocam/firmware"),
    };
    fs::create_dir_all(&directory).context("Cannot create firmware cache")?;
    let path = directory.join(release.filename());
    if !path.exists() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let partial = directory.join(format!(
            "{}.part-{}-{stamp}",
            release.filename(),
            std::process::id()
        ));
        let result = (|| -> Result<()> {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)?;
            progress("download", 0);
            let mut response = http_client()?
                .get(&release.url)
                .send()
                .context("Official firmware download failed")?
                .error_for_status()
                .context("Official firmware download returned an HTTP error")?;
            ensure!(
                response.url().as_str() == release.url && !response.status().is_redirection(),
                "Firmware download redirected away from the official URL"
            );
            let length = response.content_length();
            if let Some(length) = length {
                ensure!(
                    (1..=MAX_PACKAGE_SIZE).contains(&length),
                    "Invalid firmware download size: {length}"
                );
            }
            let mut size = 0u64;
            let mut block = vec![0; 1024 * 1024];
            loop {
                let count = response
                    .read(&mut block)
                    .context("Firmware download was interrupted")?;
                if count == 0 {
                    break;
                }
                size += count as u64;
                ensure!(
                    size <= MAX_PACKAGE_SIZE,
                    "Firmware download exceeded the size limit"
                );
                output.write_all(&block[..count])?;
                if let Some(length) = length {
                    progress(
                        "download",
                        (size.saturating_mul(100) / length.max(1)).min(99) as u8,
                    );
                }
            }
            ensure!(
                length.is_none_or(|length| length == size),
                "Incomplete firmware download"
            );
            output.sync_all()?;
            drop(output);
            progress("verify", 0);
            verify_file(&partial, release, false)?;
            fs::rename(&partial, &path).context("Cannot promote verified firmware into cache")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&partial);
        }
        result?;
    }
    progress("verify", 0);
    let verified = verify_package(&path, release)?;
    let report = path.with_file_name(format!("{}.json", release.filename()));
    fs::write(
        report,
        serde_json::to_string_pretty(&verified.report())? + "\n",
    )?;
    progress("prepared", 100);
    Ok(verified)
}

fn read_version(client: &mut Client) -> Result<CameraVersion> {
    let response = client.get(1)?;
    ensure!(
        response.resp_code == 200,
        "Camera rejected firmware identity request (code {})",
        response.resp_code
    );
    let Some(pb::command_body::Value::VersionInfo(info)) =
        response.body.and_then(|body| body.value)
    else {
        bail!("Camera did not return its firmware identity");
    };
    let camera = CameraVersion {
        serial: info.serial_num,
        firmware: info.sys_version,
        build: info
            .sys_code
            .parse()
            .context("Invalid camera firmware build")?,
    };
    ensure!(!camera.serial.is_empty(), "Camera has no serial number");
    parse_version(&camera.firmware)?;
    Ok(camera)
}

#[derive(Default)]
struct UpgradeState {
    committed: bool,
    complete: bool,
}

impl UpgradeState {
    fn observe(&mut self, message: &pb::Message, progress: &mut dyn FnMut(&str, u8)) -> Result<()> {
        if message.id != EVENT_UPGRADE {
            return Ok(());
        }
        let Some(pb::command_body::Value::UpgradeEvent(event)) =
            message.body.as_ref().and_then(|body| body.value.as_ref())
        else {
            bail!("Invalid firmware progress event");
        };
        match event.status {
            1 => Err(CameraUpdateFailed { code: event.value }.into()),
            2 => {
                ensure!(
                    self.committed,
                    "Camera reported firmware completion before the commit packet"
                );
                self.complete = true;
                Ok(())
            }
            3 => {
                progress("flash", event.value.min(100) as u8);
                Ok(())
            }
            status => bail!("Unknown firmware update status {status}"),
        }
    }
}

fn accepted(message: &pb::Message, command: u32) -> Result<()> {
    ensure!(
        message.r#type == 4 && message.id == command as i32,
        "Camera returned an unexpected firmware response"
    );
    ensure!(
        message.resp_code == 200,
        "Camera rejected command {command} (code {})",
        message.resp_code
    );
    Ok(())
}

/// Upload and commit a previously validated release through the camera's own
/// updater. Caller owns the exclusive control connection and explicit user
/// confirmation. This function never retries a write, including partial packets.
pub fn upgrade(
    client: &mut Client,
    verified: &VerifiedFirmware,
    camera: &CameraVersion,
    progress: &mut dyn FnMut(&str, u8),
) -> Result<()> {
    verified.release.validate()?;
    ensure!(
        read_version(client)? == *camera,
        "Camera identity/version changed since the update was prepared"
    );
    ensure!(
        verified.release.newer_than(camera)?,
        "Camera already runs this firmware or a newer version"
    );
    ensure!(
        (1..=MAX_PACKAGE_SIZE).contains(&verified.size),
        "Invalid verified firmware size"
    );
    // Hold the full validated snapshot in memory. Recheck the layout as well as
    // hashes, so deserializing a forged cache report cannot make it trusted.
    let file = File::open(&verified.path).context("Cannot reopen prepared firmware")?;
    let mut data = Vec::with_capacity(verified.size as usize);
    file.take(MAX_PACKAGE_SIZE + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 == verified.size
            && format!("{:x}", Sha256::digest(&data)) == verified.sha256
            && format!("{:x}", Md5::digest(&data)) == verified.md5,
        "Firmware package changed after validation"
    );
    ensure!(
        verify_archive(Cursor::new(&data), &verified.release)? == verified.image_md5,
        "Firmware package validation report does not match its images"
    );
    let mut state = UpgradeState::default();
    let init = pb::FileInfo {
        total_size: verified.size,
        md5_info: verified.md5.clone(),
        version: verified.release.version()?,
        ..Default::default()
    };
    let response = client.set_with_notifications(
        UPDATE_ROM,
        pb::command_body::Value::FileInfo(init),
        &mut |message| state.observe(message, progress),
    )?;
    accepted(&response, UPDATE_ROM)?;
    let total = data.len().div_ceil(CHUNK_SIZE) as u32;
    let transfer = (|| -> Result<()> {
        progress("upload", 0);
        for (index, block) in data.chunks(CHUNK_SIZE).enumerate() {
            let current = index as u32 + 1;
            let packet = pb::TransportFile {
                total_index: total,
                current_index: current,
                buf_size: block.len() as u32,
                buffer: block.to_vec(),
            };
            if current == total {
                // Mark before the write, including a partial send: the camera
                // may commit even if its acknowledgement never reaches us.
                state.committed = true;
                progress("commit", 100);
            }
            let response = client.set_with_notifications(
                TRANSFORM_FILE,
                pb::command_body::Value::TransportFile(packet),
                &mut |message| state.observe(message, progress),
            )?;
            accepted(&response, TRANSFORM_FILE)?;
            let Some(pb::command_body::Value::TransportFile(ack)) =
                response.body.and_then(|body| body.value)
            else {
                bail!("Camera did not acknowledge the firmware packet");
            };
            ensure!(
                (ack.total_index, ack.current_index) == (total, current),
                "Camera acknowledged an unexpected firmware packet index"
            );
            progress("upload", (current * 100 / total) as u8);
        }
        let deadline = Instant::now() + FLASH_TIMEOUT;
        while !state.complete {
            state.observe(&client.receive_until(deadline)?, progress)?;
        }
        progress("flash", 100);
        Ok(())
    })();
    match transfer {
        Err(error) if error.downcast_ref::<CameraUpdateFailed>().is_some() => Err(error),
        Err(error) if state.committed => Err(UpdateUncertain { detail: format!(
            "Update was committed; verify firmware after reboot. Do not retry the transfer automatically. ({error:#})") }.into()),
        other => other,
    }
}

pub fn verify_after_reboot(
    release: &FirmwareRelease,
    serial: &str,
    ip: Option<Ipv4Addr>,
    port: u16,
    timeout: Duration,
) -> Result<CameraVersion> {
    release.validate()?;
    ensure!(
        !serial.is_empty(),
        "Cannot verify firmware without the camera serial number"
    );
    ensure!(
        !timeout.is_zero(),
        "Reboot verification timeout must be greater than zero"
    );
    let deadline = Instant::now() + timeout;
    let mut last_seen = "unreachable".to_owned();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        if remaining.is_zero() {
            break;
        }
        let mut client = Client::new(ip, port, remaining.min(Duration::from_secs(3)));
        let result = client.connect().and_then(|()| read_version(&mut client));
        client.disconnect();
        if let Ok(camera) = result {
            ensure!(
                camera.serial == serial,
                "A different camera appeared after the update"
            );
            last_seen = format!("{} (build {})", camera.firmware, camera.build);
            if parse_version(&camera.firmware)?.1 == parse_version(&release.firmware)?.1
                && camera.build == release.build
            {
                return Ok(camera);
            }
        }
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            std::thread::sleep(remaining.min(Duration::from_secs(2)));
        }
    }
    Err(UpdateUncertain { detail: format!(
        "Could not verify {}/{} after reboot; last seen: {last_seen}. Do not retry a flash until the camera's status has been checked.",
        release.version()?, release.build) }.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release() -> FirmwareRelease {
        let firmware = "YunxiCamera-ch131b-20260831-1.0.4".to_owned();
        FirmwareRelease {
            url: format!("https://{PACKAGE_HOST}{PACKAGE_PREFIX}{firmware}.tar.gz"),
            firmware,
            build: 1349,
            description: "HDR".to_owned(),
        }
    }

    #[test]
    fn version_parsing_restricts_model_and_shape() {
        assert_eq!(release().version().unwrap(), "1.0.4");
        assert_eq!(
            parse_version("YunxiCamera-ch131b-1.0.4").unwrap().1,
            [1, 0, 4]
        );
        for invalid in [
            "YunxiCamera-ch431-1.0.4",
            "YunxiCamera-ch131b-1.0.4/../x",
            "YunxiCamera-ch131b-2026083-1.0.4",
            "YunxiCamera-ch131b-1.0",
            "YunxiCamera-ch131b-+1.0.4",
            "YunxiCamera-ch131b-1.0.4294967296",
        ] {
            assert!(parse_version(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn update_comparison_uses_semver_then_build() {
        let mut camera = CameraVersion {
            serial: "S3-test".into(),
            firmware: "YunxiCamera-ch131b-1.0.4".into(),
            build: 1348,
        };
        assert!(release().newer_than(&camera).unwrap());
        camera.build = 1349;
        assert!(!release().newer_than(&camera).unwrap());
        camera.firmware = "YunxiCamera-ch131b-1.0.5".into();
        camera.build = 1;
        assert!(!release().newer_than(&camera).unwrap());
        camera.firmware = "YunxiCamera-ch131b-0.0.7".into();
        camera.build = 9999;
        assert!(release().newer_than(&camera).unwrap());
    }

    #[test]
    fn metadata_checks_product_and_supports_native_string_build() {
        let release = release();
        let mut response = serde_json::json!({ "code": 0, "data": {
            "productType": "51", "installPackageType": 2,
            "versionNumber": release.firmware, "subVersionNumber": "1349",
            "installPackageUrl": release.url, "desc": "HDR",
        }});
        assert_eq!(parse_release_response(&response).unwrap(), release);
        response["data"]["productType"] = serde_json::json!(50);
        assert!(parse_release_response(&response).is_err());
        response["code"] = serde_json::json!(403);
        assert!(parse_release_response(&response).is_err());
    }

    #[test]
    fn url_validation_rejects_redirects_credentials_and_paths() {
        let valid = release();
        valid.validate().unwrap();
        for url in [
            valid.url.replacen("https://", "http://", 1),
            valid.url.replacen(PACKAGE_HOST, "example.com", 1),
            valid.url.replacen("https://", "https://user@", 1),
            valid
                .url
                .replacen(PACKAGE_HOST, &format!("{PACKAGE_HOST}:443"), 1),
            format!("{}?download=1", valid.url),
            format!("{}#x", valid.url),
            valid.url.replacen("/vendor/", "/other/../vendor/", 1),
        ] {
            assert!(
                FirmwareRelease {
                    url,
                    ..valid.clone()
                }
                .validate()
                .is_err()
            );
        }
    }

    #[test]
    fn native_file_fields_keep_verified_wire_numbers() {
        use prost::Message;
        assert_eq!(
            pb::FileInfo {
                total_size: 1,
                md5_info: "a".into(),
                version: "b".into(),
                file_path: "c".into(),
                ..Default::default()
            }
            .encode_to_vec(),
            b"\x08\x01\x12\x01a\x1a\x01b\x22\x01c"
        );
        assert_eq!(
            pb::TransportFile {
                total_index: 2,
                current_index: 1,
                buf_size: 1,
                buffer: b"z".to_vec()
            }
            .encode_to_vec(),
            b"\x08\x02\x10\x01\x18\x01\x22\x01z"
        );
    }

    #[test]
    fn malformed_events_and_completion_before_commit_are_rejected() {
        let mut state = UpgradeState::default();
        let mut progress = |_: &str, _: u8| {};
        let missing = pb::Message {
            id: EVENT_UPGRADE,
            ..Default::default()
        };
        assert!(state.observe(&missing, &mut progress).is_err());
        let event = |status| pb::Message {
            id: EVENT_UPGRADE,
            body: Some(pb::CommandBody {
                value: Some(pb::command_body::Value::UpgradeEvent(pb::UpgradeEvent {
                    status,
                    value: 0,
                })),
            }),
            ..Default::default()
        };
        assert!(state.observe(&event(9), &mut progress).is_err());
        assert!(state.observe(&event(2), &mut progress).is_err());
        assert!(!state.complete);
        state.committed = true;
        state.observe(&event(2), &mut progress).unwrap();
        assert!(state.complete);
    }
}
