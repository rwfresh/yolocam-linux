// SPDX-License-Identifier: GPL-3.0-or-later
//! Firmware package and native socket tests. These use loopback simulators and
//! generated archives only; no camera, official server or firmware binary needed.

use anyhow::{Context, Result, bail, ensure};
use flate2::{Compression, write::GzEncoder};
use md5::Md5;
use prost::Message as _;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use yolocam_linux::camera::CameraVersion;
use yolocam_linux::firmware::{
    self, CHUNK_SIZE, CameraUpdateFailed, FirmwareRelease, PACKAGE_HOST, PACKAGE_PREFIX,
    UpdateUncertain, upgrade, verify_package,
};
use yolocam_linux::pb::{self, command_body::Value};
use yolocam_linux::protocol::{Client, pack_frame, unpack_frame};

fn camera() -> CameraVersion {
    CameraVersion {
        serial: "test-camera".into(),
        firmware: "YunxiCamera-ch131b-0.0.7".into(),
        build: 1198,
    }
}

fn release() -> FirmwareRelease {
    let firmware = "YunxiCamera-ch131b-20260831-1.0.4".to_owned();
    FirmwareRelease {
        url: format!("https://{PACKAGE_HOST}{PACKAGE_PREFIX}{firmware}.tar.gz"),
        firmware,
        build: 1349,
        description: "test release".into(),
    }
}

fn random_bytes(size: usize, mut seed: u32) -> Vec<u8> {
    (0..size)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        })
        .collect()
}

fn append(
    builder: &mut tar::Builder<GzEncoder<fs::File>>,
    name: &str,
    data: &[u8],
    kind: tar::EntryType,
) {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_entry_type(kind);
    // Write a literal malicious path into the tar header as well: Builder's
    // normal set_path API appropriately rejects '..' before our code sees it.
    assert!(name.len() <= 100);
    header.as_mut_bytes()[..100].fill(0);
    header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
    if kind.is_symlink() {
        header.set_link_name("/etc/passwd").unwrap();
    }
    header.set_cksum();
    builder.append(&header, data).unwrap();
}

fn archive(
    path: &Path,
    size: usize,
    corrupt: bool,
    build: u32,
    extra: Option<(&str, &[u8], tar::EntryType)>,
) {
    let mut images: Vec<(&str, Vec<u8>)> = ["app.ext4.img", "rootfs.ext4.img", "uImage-fdt"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| (name, random_bytes(size, index as u32 + 701)))
        .collect();
    let manifest = format!("version:1.0.4_{build}\n")
        + &images
            .iter()
            .map(|(name, data)| format!("{:x}  {name}\n", Md5::digest(data)))
            .collect::<String>();
    if corrupt {
        images[0].1 = b"corrupted image".to_vec();
    }
    let compressed = GzEncoder::new(fs::File::create(path).unwrap(), Compression::fast());
    let mut builder = tar::Builder::new(compressed);
    for (name, data) in images {
        append(&mut builder, name, &data, tar::EntryType::Regular);
    }
    append(
        &mut builder,
        "update_desc",
        manifest.as_bytes(),
        tar::EntryType::Regular,
    );
    if let Some((name, data, kind)) = extra {
        append(&mut builder, name, data, kind);
    }
    builder.into_inner().unwrap().finish().unwrap();
}

fn package(size: usize) -> (TempDir, std::path::PathBuf) {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(release().filename());
    archive(&path, size, false, 1349, None);
    (temp, path)
}

#[derive(Clone, Copy)]
enum Mode {
    Ok,
    Reject,
    BadAck,
    DisconnectAfterCommit,
    Fail,
}

#[derive(Default)]
struct Report {
    messages: Vec<pb::Message>,
    data: Vec<u8>,
}

fn read_message(stream: &mut TcpStream) -> Result<pb::Message> {
    let mut header = [0; 8];
    stream.read_exact(&mut header)?;
    let size = u32::from_le_bytes(header[4..8].try_into()?) as usize;
    ensure!(
        size <= CHUNK_SIZE + 65536,
        "Unexpected simulator frame size"
    );
    let mut frame = vec![0; size + 8];
    frame[..8].copy_from_slice(&header);
    stream.read_exact(&mut frame[8..])?;
    Ok(pb::Message::decode(unpack_frame(&frame)?.as_slice())?)
}

fn send(stream: &mut TcpStream, message: &pb::Message) -> Result<()> {
    let frame = pack_frame(&message.encode_to_vec());
    // Fragment the frame in the outer/inner header to exercise exact reads.
    stream.write_all(&frame[..9])?;
    stream.write_all(&frame[9..])?;
    Ok(())
}

fn simulate(mode: Mode, identity: CameraVersion) -> (Client, JoinHandle<Result<Report>>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || -> Result<Report> {
        let (mut stream, _) = listener.accept()?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        stream.set_nodelay(true)?;
        let mut report = Report::default();
        let mut file_info: Option<pb::FileInfo> = None;
        loop {
            let message = match read_message(&mut stream) {
                Ok(message) => message,
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        [
                            std::io::ErrorKind::UnexpectedEof,
                            std::io::ErrorKind::ConnectionReset,
                        ]
                        .contains(&error.kind())
                    }) =>
                {
                    return Ok(report);
                }
                Err(error) => return Err(error),
            };
            report.messages.push(message.clone());
            let mut reply = pb::Message {
                r#type: 4,
                seq: message.seq,
                id: message.id,
                resp_code: 200,
                ..Default::default()
            };
            // Notifications and old response sequences appear between real
            // acknowledgements. Neither may count as the current packet ACK.
            send(
                &mut stream,
                &pb::Message {
                    r#type: 5,
                    id: 59,
                    ..Default::default()
                },
            )?;
            send(
                &mut stream,
                &pb::Message {
                    seq: message.seq.wrapping_sub(1),
                    ..reply.clone()
                },
            )?;
            match message.id {
                215 => ensure!(
                    message.body.and_then(|body| body.value) == Some(Value::IntVal(1)),
                    "Wrong immediate heartbeat"
                ),
                1 => {
                    reply.body = Some(pb::CommandBody {
                        value: Some(Value::VersionInfo(pb::VersionInfo {
                            serial_num: identity.serial.clone(),
                            sys_version: identity.firmware.clone(),
                            sys_code: identity.build.to_string(),
                            ..Default::default()
                        })),
                    });
                }
                2 => {
                    let Some(Value::FileInfo(info)) = message.body.and_then(|body| body.value)
                    else {
                        bail!("Missing file info");
                    };
                    ensure!(
                        info.version == release().version()?,
                        "Wrong uploaded version"
                    );
                    file_info = Some(info);
                    if matches!(mode, Mode::Reject) {
                        reply.resp_code = 100;
                    }
                }
                3 => {
                    let Some(Value::TransportFile(packet)) =
                        message.body.and_then(|body| body.value)
                    else {
                        bail!("Missing transport packet");
                    };
                    ensure!(
                        packet.buf_size as usize == packet.buffer.len(),
                        "Invalid packet byte count"
                    );
                    report.data.extend_from_slice(&packet.buffer);
                    reply.body = Some(pb::CommandBody {
                        value: Some(Value::TransportFile(pb::TransportFile {
                            total_index: packet.total_index,
                            current_index: if matches!(mode, Mode::BadAck) {
                                999
                            } else {
                                packet.current_index
                            },
                            ..Default::default()
                        })),
                    });
                    if packet.current_index == packet.total_index {
                        let info = file_info.as_ref().context("No firmware init command")?;
                        ensure!(
                            info.total_size == report.data.len() as u64,
                            "Wrong firmware upload size"
                        );
                        ensure!(
                            info.md5_info == format!("{:x}", Md5::digest(&report.data)),
                            "Wrong uploaded MD5"
                        );
                        if matches!(mode, Mode::DisconnectAfterCommit) {
                            return Ok(report);
                        }
                        send(&mut stream, &reply)?;
                        send(
                            &mut stream,
                            &pb::Message {
                                r#type: 5,
                                id: 28,
                                body: Some(pb::CommandBody {
                                    value: Some(Value::UpgradeEvent(pb::UpgradeEvent {
                                        status: 3,
                                        value: 50,
                                    })),
                                }),
                                ..Default::default()
                            },
                        )?;
                        send(
                            &mut stream,
                            &pb::Message {
                                r#type: 5,
                                id: 28,
                                body: Some(pb::CommandBody {
                                    value: Some(Value::UpgradeEvent(pb::UpgradeEvent {
                                        status: if matches!(mode, Mode::Fail) { 1 } else { 2 },
                                        value: if matches!(mode, Mode::Fail) { 6 } else { 0 },
                                    })),
                                }),
                                ..Default::default()
                            },
                        )?;
                        return Ok(report);
                    }
                }
                unexpected => bail!("Unexpected command {unexpected}"),
            }
            send(&mut stream, &reply)?;
        }
    });
    let mut client = Client::new(Some(Ipv4Addr::LOCALHOST), port, Duration::from_secs(5));
    client.connect().unwrap();
    (client, server)
}

#[test]
fn valid_archive_hashes_images_without_extraction() {
    let (_temp, path) = package(4096);
    let verified = verify_package(&path, &release()).unwrap();
    assert_eq!(verified.image_md5.len(), 3);
    assert_eq!(
        verified.sha256,
        format!("{:x}", Sha256::digest(fs::read(&path).unwrap()))
    );
    assert_eq!(verified.path, path.canonicalize().unwrap());
    assert!(!path.parent().unwrap().join("app.ext4.img").exists());
}

#[test]
fn corrupt_image_is_rejected() {
    let (_temp, path) = package(4096);
    archive(&path, 4096, true, 1349, None);
    assert!(
        verify_package(&path, &release())
            .unwrap_err()
            .to_string()
            .contains("checksum")
    );
}

#[test]
fn mismatched_manifest_build_is_rejected() {
    let (_temp, path) = package(4096);
    archive(&path, 4096, false, 1295, None);
    assert!(
        verify_package(&path, &release())
            .unwrap_err()
            .to_string()
            .contains("version/build")
    );
}

#[test]
fn paths_links_directories_extensions_and_duplicate_members_are_rejected() {
    let (_temp, path) = package(4096);
    for (name, kind) in [
        ("../update_desc", tar::EntryType::Regular),
        ("bl31.bin", tar::EntryType::Symlink),
        ("config", tar::EntryType::Directory),
        ("update_desc", tar::EntryType::Regular),
        ("config", tar::EntryType::XGlobalHeader),
        ("./config", tar::EntryType::Regular),
        ("/config", tar::EntryType::Regular),
    ] {
        archive(&path, 4096, false, 1349, Some((name, b"unsafe", kind)));
        assert!(
            verify_package(&path, &release()).is_err(),
            "accepted {name} {kind:?}"
        );
    }
}

#[test]
fn metadata_member_size_limit_is_enforced() {
    let (_temp, path) = package(4096);
    let oversized = vec![0; 1024 * 1024 + 1];
    archive(
        &path,
        4096,
        false,
        1349,
        Some(("config", &oversized, tar::EntryType::Regular)),
    );
    assert!(
        verify_package(&path, &release())
            .unwrap_err()
            .to_string()
            .contains("limit")
    );
}

#[test]
fn truncated_gzip_and_wrong_package_filename_are_rejected() {
    let (_temp, path) = package(4096);
    let mut bytes = fs::read(&path).unwrap();
    bytes.truncate(bytes.len() - 6);
    fs::write(&path, bytes).unwrap();
    assert!(verify_package(&path, &release()).is_err());
    archive(&path, 4096, false, 1349, None);
    let wrong = path.with_file_name("firmware.tar.gz");
    fs::rename(&path, &wrong).unwrap();
    assert!(
        verify_package(&wrong, &release())
            .unwrap_err()
            .to_string()
            .contains("filename")
    );
}

#[test]
fn gzip_crc_and_hidden_second_member_are_checked() {
    let (_temp, path) = package(4096);
    let mut bytes = fs::read(&path).unwrap();
    let checksum = bytes.len() - 8;
    bytes[checksum] ^= 0x40;
    fs::write(&path, bytes).unwrap();
    assert!(verify_package(&path, &release()).is_err());

    archive(&path, 4096, false, 1349, None);
    let mut hidden = GzEncoder::new(Vec::new(), Compression::fast());
    hidden.write_all(b"hidden archive content").unwrap();
    let mut output = fs::OpenOptions::new().append(true).open(&path).unwrap();
    output.write_all(&hidden.finish().unwrap()).unwrap();
    drop(output);
    assert!(
        verify_package(&path, &release())
            .unwrap_err()
            .to_string()
            .contains("Unexpected data after firmware archive")
    );
}

#[test]
fn cached_prepare_revalidates_and_produces_report_without_network() {
    let (temp, _path) = package(4096);
    let mut progress = Vec::new();
    let verified =
        firmware::prepare_firmware(&release(), Some(temp.path()), &mut |phase, percent| {
            progress.push((phase.to_owned(), percent))
        })
        .unwrap();
    assert!(progress.contains(&("prepared".to_owned(), 100)));
    assert!(
        progress
            .iter()
            .all(|(phase, _)| phase != "upload" && phase != "commit")
    );
    let report = temp.path().join(format!("{}.json", release().filename()));
    let json: serde_json::Value = serde_json::from_slice(&fs::read(report).unwrap()).unwrap();
    assert_eq!(json["sha256"], verified.sha256);
    fs::write(verified.path, b"invalid cached archive").unwrap();
    assert!(firmware::prepare_firmware(&release(), Some(temp.path()), &mut |_, _| {}).is_err());
}

#[test]
fn transfer_handles_notifications_and_full_two_mib_packets() {
    let (_temp, path) = package(CHUNK_SIZE);
    let verified = verify_package(&path, &release()).unwrap();
    let (mut client, server) = simulate(Mode::Ok, camera());
    let mut progress = Vec::new();
    upgrade(&mut client, &verified, &camera(), &mut |phase, percent| {
        progress.push((phase.to_owned(), percent))
    })
    .unwrap();
    client.disconnect();
    let report = server.join().unwrap().unwrap();
    assert_eq!(report.data, fs::read(&path).unwrap());
    let packets: Vec<_> = report
        .messages
        .iter()
        .filter_map(
            |message| match message.body.as_ref().and_then(|body| body.value.as_ref()) {
                Some(Value::TransportFile(packet)) => Some(packet),
                _ => None,
            },
        )
        .collect();
    assert!(packets.len() >= 2);
    assert_eq!(packets[0].buf_size as usize, CHUNK_SIZE);
    assert!(
        packets[..packets.len() - 1]
            .iter()
            .all(|packet| packet.buf_size as usize == CHUNK_SIZE)
    );
    assert_eq!(
        progress
            .iter()
            .filter(|(phase, _)| phase == "commit")
            .count(),
        1
    );
    assert!(progress.contains(&("flash".to_owned(), 50)));
    assert_eq!(progress.last(), Some(&("flash".to_owned(), 100)));
}

#[test]
fn init_rejection_sends_no_firmware_packets() {
    let (_temp, path) = package(4096);
    let (mut client, server) = simulate(Mode::Reject, camera());
    let error = upgrade(
        &mut client,
        &verify_package(&path, &release()).unwrap(),
        &camera(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("rejected"));
    assert!(error.downcast_ref::<UpdateUncertain>().is_none());
    client.disconnect();
    let report = server.join().unwrap().unwrap();
    assert!(report.data.is_empty());
    assert!(!report.messages.iter().any(|message| message.id == 3));
}

#[test]
fn wrong_ack_index_stops_before_the_commit_packet() {
    let (_temp, path) = package(CHUNK_SIZE / 2);
    let (mut client, server) = simulate(Mode::BadAck, camera());
    let mut committed = false;
    let error = upgrade(
        &mut client,
        &verify_package(&path, &release()).unwrap(),
        &camera(),
        &mut |phase, _| committed |= phase == "commit",
    )
    .unwrap_err();
    assert!(error.to_string().contains("packet index"));
    assert!(!committed);
    assert!(error.downcast_ref::<UpdateUncertain>().is_none());
    client.disconnect();
    let report = server.join().unwrap().unwrap();
    assert_eq!(report.data.len(), CHUNK_SIZE);
    assert_eq!(
        report
            .messages
            .iter()
            .filter(|message| message.id == 3)
            .count(),
        1
    );
}

#[test]
fn disconnect_after_commit_is_uncertain_and_never_replayed() {
    let (_temp, path) = package(4096);
    let (mut client, server) = simulate(Mode::DisconnectAfterCommit, camera());
    let error = upgrade(
        &mut client,
        &verify_package(&path, &release()).unwrap(),
        &camera(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(error.downcast_ref::<UpdateUncertain>().is_some());
    client.disconnect();
    let report = server.join().unwrap().unwrap();
    assert_eq!(report.data, fs::read(&path).unwrap());
    assert_eq!(
        report
            .messages
            .iter()
            .filter(|message| message.id == 3)
            .count(),
        1
    );
}

#[test]
fn explicit_device_failure_is_distinct_from_an_uncertain_disconnect() {
    let (_temp, path) = package(4096);
    let (mut client, server) = simulate(Mode::Fail, camera());
    let error = upgrade(
        &mut client,
        &verify_package(&path, &release()).unwrap(),
        &camera(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert_eq!(error.downcast_ref::<CameraUpdateFailed>().unwrap().code, 6);
    assert!(error.downcast_ref::<UpdateUncertain>().is_none());
    client.disconnect();
    server.join().unwrap().unwrap();
}

#[test]
fn changed_camera_identity_stops_before_the_first_firmware_write() {
    let (_temp, path) = package(4096);
    let actual = CameraVersion {
        serial: "different-camera".into(),
        ..camera()
    };
    let (mut client, server) = simulate(Mode::Ok, actual);
    let error = upgrade(
        &mut client,
        &verify_package(&path, &release()).unwrap(),
        &camera(),
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("identity/version changed"));
    client.disconnect();
    let report = server.join().unwrap().unwrap();
    assert_eq!(
        report
            .messages
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>(),
        [215, 1]
    );
}

#[test]
fn changed_package_bytes_never_reach_the_camera() {
    let (_temp, path) = package(4096);
    let verified = verify_package(&path, &release()).unwrap();
    fs::write(&path, b"modified after validation").unwrap();
    let (mut client, server) = simulate(Mode::Ok, camera());
    let error = upgrade(&mut client, &verified, &camera(), &mut |_, _| {}).unwrap_err();
    assert!(error.to_string().contains("changed after validation"));
    client.disconnect();
    let report = server.join().unwrap().unwrap();
    assert_eq!(
        report
            .messages
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>(),
        [215, 1]
    );
}

#[test]
fn forged_validation_report_does_not_bypass_archive_validation() {
    let (_temp, path) = package(4096);
    archive(
        &path,
        4096,
        false,
        1349,
        Some(("../config", b"unsafe", tar::EntryType::Regular)),
    );
    let bytes = fs::read(&path).unwrap();
    let verified = firmware::VerifiedFirmware {
        path,
        release: release(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        md5: format!("{:x}", Md5::digest(&bytes)),
        image_md5: Default::default(),
    };
    let (mut client, server) = simulate(Mode::Ok, camera());
    let error = upgrade(&mut client, &verified, &camera(), &mut |_, _| {}).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Unsafe or unexpected archive member")
    );
    client.disconnect();
    let report = server.join().unwrap().unwrap();
    assert_eq!(
        report
            .messages
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>(),
        [215, 1]
    );
}

#[test]
fn already_current_firmware_is_not_reflashed() {
    let (_temp, path) = package(4096);
    let current = CameraVersion {
        serial: camera().serial,
        firmware: release().firmware,
        build: release().build,
    };
    let (mut client, server) = simulate(Mode::Ok, current.clone());
    let error = upgrade(
        &mut client,
        &verify_package(&path, &release()).unwrap(),
        &current,
        &mut |_, _| {},
    )
    .unwrap_err();
    assert!(error.to_string().contains("already runs"));
    client.disconnect();
    let report = server.join().unwrap().unwrap();
    assert_eq!(
        report
            .messages
            .iter()
            .map(|message| message.id)
            .collect::<Vec<_>>(),
        [215, 1]
    );
}

fn version_server(expected: CameraVersion) -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        for _ in 0..2 {
            let request = read_message(&mut stream).unwrap();
            let body = if request.id == 1 {
                Some(pb::CommandBody {
                    value: Some(Value::VersionInfo(pb::VersionInfo {
                        sys_version: expected.firmware.clone(),
                        serial_num: expected.serial.clone(),
                        sys_code: expected.build.to_string(),
                        ..Default::default()
                    })),
                })
            } else {
                None
            };
            send(
                &mut stream,
                &pb::Message {
                    r#type: 4,
                    seq: request.seq,
                    id: request.id,
                    resp_code: 200,
                    body,
                    ..Default::default()
                },
            )
            .unwrap();
        }
    });
    (port, server)
}

#[test]
fn reboot_verification_checks_identity_and_target_build() {
    let identity = CameraVersion {
        serial: camera().serial,
        firmware: "YunxiCamera-ch131b-1.0.4".into(),
        build: 1349,
    };
    let (port, server) = version_server(identity.clone());
    let verified = firmware::verify_after_reboot(
        &release(),
        &identity.serial,
        Some(Ipv4Addr::LOCALHOST),
        port,
        Duration::from_secs(2),
    )
    .unwrap();
    assert_eq!(verified, identity);
    server.join().unwrap();
}

#[test]
fn reboot_verification_refuses_a_different_camera() {
    let identity = CameraVersion {
        serial: "different-camera".into(),
        firmware: "YunxiCamera-ch131b-1.0.4".into(),
        build: 1349,
    };
    let (port, server) = version_server(identity);
    let error = firmware::verify_after_reboot(
        &release(),
        "test-camera",
        Some(Ipv4Addr::LOCALHOST),
        port,
        Duration::from_secs(2),
    )
    .unwrap_err();
    assert!(error.to_string().contains("different camera"));
    server.join().unwrap();
}

#[test]
fn reboot_verification_timeout_is_bounded_and_uncertain() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let start = Instant::now();
    let error = firmware::verify_after_reboot(
        &release(),
        "test-camera",
        Some(Ipv4Addr::LOCALHOST),
        port,
        Duration::from_millis(50),
    )
    .unwrap_err();
    assert!(error.downcast_ref::<UpdateUncertain>().is_some());
    assert!(start.elapsed() < Duration::from_secs(1));
}
