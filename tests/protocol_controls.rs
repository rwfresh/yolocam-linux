// SPDX-License-Identifier: GPL-3.0-or-later
//! These fixtures run against local TCP listeners, never against a real camera.
use anyhow::Result;
use prost::Message as _;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use yolocam_linux::{
    camera::{Camera, CameraVersion},
    pb,
    protocol::{Client, MAX_PAYLOAD_SIZE, pack_frame, unpack_frame},
};

fn read_request(stream: &mut TcpStream) -> pb::Message {
    let mut header = [0; 8];
    stream.read_exact(&mut header).unwrap();
    let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    assert!(size <= MAX_PAYLOAD_SIZE + 8);
    let mut frame = header.to_vec();
    frame.resize(8 + size, 0);
    stream.read_exact(&mut frame[8..]).unwrap();
    pb::Message::decode(unpack_frame(&frame).unwrap().as_slice()).unwrap()
}

fn response(request: &pb::Message, value: Option<pb::command_body::Value>) -> pb::Message {
    pb::Message {
        r#type: 4,
        seq: request.seq,
        id: request.id,
        resp_code: 200,
        body: value.map(|value| pb::CommandBody { value: Some(value) }),
        ..Default::default()
    }
}

fn send(stream: &mut TcpStream, message: &pb::Message) {
    let mut frame = pack_frame(&message.encode_to_vec());
    frame[1] = 0; // Actual camera-to-client flag.
    stream.write_all(&frame).unwrap();
}

fn accept(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let heartbeat = read_request(&mut stream);
    assert_eq!((heartbeat.r#type, heartbeat.id), (2, 215));
    assert_eq!(
        heartbeat.body.as_ref().unwrap().value,
        Some(pb::command_body::Value::IntVal(1))
    );
    send(&mut stream, &response(&heartbeat, None));
    stream
}

fn server(task: impl FnOnce(TcpListener) + Send + 'static) -> (Client, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let worker = thread::spawn(move || task(listener));
    (
        Client::new(Some(Ipv4Addr::LOCALHOST), port, Duration::from_millis(400)),
        worker,
    )
}

#[test]
fn dual_envelope_validates_every_boundary_and_supports_firmware_chunks() {
    let payload = vec![0xf1; 2 * 1024 * 1024 + 64];
    let frame = pack_frame(&payload);
    assert_eq!(unpack_frame(&frame).unwrap(), payload);
    for index in [0, 1, 2, 4, 8, 9, 13, 14, frame.len() - 2, frame.len() - 1] {
        let mut bad = frame.clone();
        bad[index] ^= 0x80;
        assert!(unpack_frame(&bad).is_err(), "accepted damaged byte {index}");
    }
    assert!(unpack_frame(&frame[..frame.len() - 1]).is_err());
    let mut extra = frame;
    extra.push(0);
    assert!(unpack_frame(&extra).is_err());
}

#[test]
fn reads_reconnect_once_with_a_new_sequence_after_eof() {
    let (mut client, worker) = server(|listener| {
        let mut first = accept(&listener);
        let request = read_request(&mut first);
        assert_eq!((request.r#type, request.id), (1, 9));
        drop(first);
        let mut second = accept(&listener);
        let retried = read_request(&mut second);
        assert_ne!(retried.seq, request.seq);
        send(
            &mut second,
            &response(&retried, Some(pb::command_body::Value::IntVal(48))),
        );
    });
    client.connect().unwrap();
    let reply = client.get(9).unwrap();
    assert_eq!(
        reply.body.unwrap().value,
        Some(pb::command_body::Value::IntVal(48))
    );
    worker.join().unwrap();
}

#[test]
fn writes_with_lost_acknowledgements_are_never_replayed() {
    let writes = Arc::new(AtomicUsize::new(0));
    let counter = writes.clone();
    let (mut client, worker) = server(move |listener| {
        let mut first = accept(&listener);
        let request = read_request(&mut first);
        assert_eq!((request.r#type, request.id), (2, 9));
        counter.fetch_add(1, Ordering::Relaxed);
        drop(first); // The camera may already have committed this write.
        let mut second = accept(&listener);
        let readback = read_request(&mut second);
        assert_eq!((readback.r#type, readback.id), (1, 9));
        send(
            &mut second,
            &response(&readback, Some(pb::command_body::Value::IntVal(55))),
        );
    });
    client.connect().unwrap();
    let error = client.set_int(9, 55).unwrap_err();
    assert!(error.to_string().contains("was not replayed"));
    assert!(!client.is_connected());
    assert!(client.get(9).is_ok());
    assert_eq!(writes.load(Ordering::Relaxed), 1);
    worker.join().unwrap();
}

#[test]
fn explicit_disconnect_prevents_background_reconnections() {
    let (mut client, worker) = server(|listener| {
        let _stream = accept(&listener);
    });
    client.connect().unwrap();
    client.disconnect();
    assert!(client.get(9).is_err());
    assert!(client.set_int(9, 50).is_err());
    assert!(!client.is_connected());
    worker.join().unwrap();
}

#[test]
fn stale_responses_and_notifications_cannot_satisfy_a_request() {
    let (mut client, worker) = server(|listener| {
        let mut stream = accept(&listener);
        let request = read_request(&mut stream);
        send(
            &mut stream,
            &pb::Message {
                r#type: 5,
                id: 59,
                ..Default::default()
            },
        );
        let mut old = response(&request, Some(pb::command_body::Value::IntVal(7)));
        old.seq -= 1;
        send(&mut stream, &old);
        let mut wrong_id = response(&request, Some(pb::command_body::Value::IntVal(8)));
        wrong_id.id += 1;
        send(&mut stream, &wrong_id);
        let frame = pack_frame(
            &response(&request, Some(pb::command_body::Value::IntVal(42))).encode_to_vec(),
        );
        for chunk in frame.chunks(3) {
            stream.write_all(chunk).unwrap();
        }
    });
    client.connect().unwrap();
    let reply = client.get(9).unwrap();
    assert_eq!(
        reply.body.unwrap().value,
        Some(pb::command_body::Value::IntVal(42))
    );
    worker.join().unwrap();
}

#[test]
fn a_partial_read_timeout_discards_the_stream_before_retry() {
    let (mut client, worker) = server(|listener| {
        let mut first = accept(&listener);
        let request = read_request(&mut first);
        let frame = pack_frame(&response(&request, None).encode_to_vec());
        first.write_all(&frame[..12]).unwrap();
        thread::sleep(Duration::from_millis(200));
        drop(first);
        let mut second = accept(&listener);
        let readback = read_request(&mut second);
        send(
            &mut second,
            &response(&readback, Some(pb::command_body::Value::IntVal(33))),
        );
    });
    client.set_timeout(Duration::from_millis(150)).unwrap();
    client.connect().unwrap();
    let reply = client.get(9).unwrap();
    assert_eq!(
        reply.body.unwrap().value,
        Some(pb::command_body::Value::IntVal(33))
    );
    worker.join().unwrap();
}

#[test]
fn notification_callbacks_can_abort_without_replaying_a_firmware_action() {
    let (mut client, worker) = server(|listener| {
        let mut stream = accept(&listener);
        let request = read_request(&mut stream);
        assert_eq!(request.id, 2);
        send(
            &mut stream,
            &pb::Message {
                r#type: 5,
                id: 28,
                body: Some(pb::CommandBody {
                    value: Some(pb::command_body::Value::UpgradeEvent(pb::UpgradeEvent {
                        status: 4,
                        value: 0,
                    })),
                }),
                ..Default::default()
            },
        );
    });
    client.connect().unwrap();
    let mut called = false;
    let error = client
        .set_with_notifications(2, pb::command_body::Value::IntVal(1), &mut |_| {
            called = true;
            anyhow::bail!("upgrade rejected by camera");
        })
        .unwrap_err();
    assert!(called);
    assert!(format!("{error:#}").contains("upgrade rejected"));
    assert!(!client.is_connected());
    worker.join().unwrap();
}

#[test]
fn notifications_do_not_extend_the_single_request_deadline() {
    let (mut client, worker) = server(|listener| {
        let mut stream = accept(&listener);
        let _request = read_request(&mut stream);
        let notify = pack_frame(
            &pb::Message {
                r#type: 5,
                id: 59,
                ..Default::default()
            }
            .encode_to_vec(),
        );
        for _ in 0..12 {
            if stream.write_all(&notify).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
    client.set_timeout(Duration::from_millis(100)).unwrap();
    client.connect().unwrap();
    let start = Instant::now();
    assert!(client.set_int(9, 40).is_err());
    assert!(start.elapsed() < Duration::from_millis(400));
    assert!(!client.is_connected());
    worker.join().unwrap();
}

fn mock_camera(task: impl FnOnce(&mut TcpStream) + Send + 'static) -> (Camera, JoinHandle<()>) {
    let (client, worker) = server(move |listener| {
        let mut stream = accept(&listener);
        task(&mut stream);
    });
    let mut camera = Camera::new(client.ip, client.port, Duration::from_millis(400));
    camera.state.version = Some(CameraVersion {
        firmware: "YunxiCamera-ch131b-1.0.4".into(),
        ..Default::default()
    });
    camera.client.connect().unwrap();
    (camera, worker)
}

#[test]
fn negative_sharpness_uses_native_61_to_70_encoding_and_decoded_readback() {
    let (mut camera, worker) = mock_camera(|stream| {
        let write = read_request(stream);
        assert_eq!(write.id, 8);
        assert_eq!(
            write.body.as_ref().unwrap().value,
            Some(pb::command_body::Value::IntVal(67))
        );
        send(stream, &response(&write, None));
        let read = read_request(stream);
        send(
            stream,
            &response(&read, Some(pb::command_body::Value::IntVal(67))),
        );
    });
    assert_eq!(camera.set_param("sharpness", -7).unwrap(), -7);
    assert_eq!(camera.state.values["sharpness"], -7);
    worker.join().unwrap();
}

#[test]
fn ai_tracking_uses_the_compound_native_body() {
    let (mut camera, worker) = mock_camera(|stream| {
        let write = read_request(stream);
        assert_eq!(write.id, 99);
        assert_eq!(
            write.body.as_ref().unwrap().value,
            Some(pb::command_body::Value::AiTrackingCtrl(
                pb::AiTrackingCtrl { mode: 2 }
            ))
        );
        send(stream, &response(&write, None));
        let read = read_request(stream);
        send(
            stream,
            &response(
                &read,
                Some(pb::command_body::Value::AiTrackingCtrl(
                    pb::AiTrackingCtrl { mode: 2 },
                )),
            ),
        );
    });
    assert_eq!(camera.set_param("ai-tracking", 2).unwrap(), 2);
    worker.join().unwrap();
}

#[test]
fn profile_selection_preserves_hdr_bank_and_never_toggles_hdr() -> Result<()> {
    let (mut camera, worker) = mock_camera(|stream| {
        let hdr = read_request(stream);
        assert_eq!((hdr.r#type, hdr.id), (1, 98));
        send(
            stream,
            &response(&hdr, Some(pb::command_body::Value::IntVal(1))),
        );
        let select = read_request(stream);
        assert_eq!((select.r#type, select.id), (2, 53));
        let expected = pb::TuningParam {
            id: 2,
            scene_id: 3,
            hdr_flag: 1,
        };
        assert_eq!(
            select.body.as_ref().unwrap().value,
            Some(pb::command_body::Value::TuningParam(expected))
        );
        send(stream, &response(&select, None));
        let read = read_request(stream);
        assert_eq!(read.id, 53);
        send(
            stream,
            &response(&read, Some(pb::command_body::Value::TuningParam(expected))),
        );
        let info = read_request(stream);
        assert_eq!(info.id, 50);
        send(
            stream,
            &response(
                &info,
                Some(pb::command_body::Value::TuningFileInfo(
                    pb::TuningFileInfo::default(),
                )),
            ),
        );
    });
    let profile = camera.select_profile(2, 3)?;
    assert_eq!((profile.id, profile.scene_id, profile.hdr_flag), (2, 3, 1));
    // Fixture captured from the corrected camera-native protobuf schema.
    assert_eq!(profile.encode_to_vec(), [0x08, 2, 0x10, 3, 0x18, 1]);
    worker.join().unwrap();
    Ok(())
}

#[test]
fn native_crop_writes_float_coordinates_and_verifies_the_full_rectangle() {
    use yolocam_linux::camera::Crop;
    let desired = Crop::new(2.0, 0.1, 0.3).unwrap();
    let native = pb::Eptz {
        zoom: 2.0,
        left: 0.1,
        top: 0.3,
        right: 0.6,
        bottom: 0.8,
    };
    let (mut camera, worker) = mock_camera(move |stream| {
        let write = read_request(stream);
        assert_eq!((write.r#type, write.id), (2, 117));
        assert_eq!(
            write.body.as_ref().unwrap().value,
            Some(pb::command_body::Value::EptzParam(native))
        );
        send(stream, &response(&write, None));
        let read = read_request(stream);
        assert_eq!((read.r#type, read.id), (1, 117));
        send(
            stream,
            &response(&read, Some(pb::command_body::Value::EptzParam(native))),
        );
    });
    assert_eq!(camera.set_crop(desired.clone()).unwrap(), desired);
    worker.join().unwrap();
}

#[test]
fn hsl_writes_signed_strings_and_preserves_other_colours() {
    use yolocam_linux::camera::HslState;
    let mut expected = HslState::default();
    expected.values[0][0] = -100;
    expected.values[1][4] = 33;
    expected.values[2][7] = 100;
    let (mut camera, worker) = mock_camera(|stream| {
        let write = read_request(stream);
        assert_eq!(write.id, 118);
        let Some(pb::command_body::Value::HslParam(native)) =
            write.body.as_ref().unwrap().value.as_ref()
        else {
            panic!("HSL must use its native compound body");
        };
        assert_eq!(native.hue_red, "-100");
        assert_eq!(native.hue_blue, "0");
        assert_eq!(native.saturation_cyan, "33");
        assert_eq!(native.luminance_magenta, "100");
        let native = native.clone();
        send(stream, &response(&write, None));
        let read = read_request(stream);
        send(
            stream,
            &response(&read, Some(pb::command_body::Value::HslParam(native))),
        );
    });
    assert_eq!(camera.set_hsl(expected.clone()).unwrap(), expected);
    worker.join().unwrap();
}

#[test]
fn crop_preset_saves_the_current_rectangle_and_ignores_empty_slots() {
    use yolocam_linux::camera::Crop;
    let native = pb::Eptz {
        zoom: 2.0,
        left: 0.25,
        top: 0.25,
        right: 0.75,
        bottom: 0.75,
    };
    let (mut camera, worker) = mock_camera(move |stream| {
        let current = read_request(stream);
        assert_eq!((current.r#type, current.id), (1, 117));
        send(
            stream,
            &response(&current, Some(pb::command_body::Value::EptzParam(native))),
        );
        let save = read_request(stream);
        assert_eq!((save.r#type, save.id), (2, 127));
        let Some(pb::command_body::Value::EptzPresets(presets)) =
            save.body.as_ref().unwrap().value.as_ref()
        else {
            panic!("Presets must use their native compound body");
        };
        let Some(pb::eptz_presets::Param::EptzPreset(saved)) = presets.param.as_ref() else {
            panic!("Preset saving must contain a single slot");
        };
        assert_eq!(saved.preset_num, "3");
        assert_eq!(saved.eptz, Some(native));
        send(stream, &response(&save, None));
        let read = read_request(stream);
        send(
            stream,
            &response(
                &read,
                Some(pb::command_body::Value::EptzPresets(pb::EptzPresets {
                    param: Some(pb::eptz_presets::Param::Presets(pb::EptzPresetList {
                        entries: vec![
                            pb::EptzPreset {
                                preset_num: "0".into(),
                                eptz: Some(pb::Eptz::default()),
                            },
                            pb::EptzPreset {
                                preset_num: "3".into(),
                                eptz: Some(native),
                            },
                        ],
                    })),
                })),
            ),
        );
    });
    camera.save_crop(3).unwrap();
    assert_eq!(camera.state.presets.len(), 1);
    assert_eq!(camera.state.presets[&3], Crop::centred(2.0).unwrap());
    worker.join().unwrap();
}

#[test]
fn older_firmware_selects_sdr_profile_when_hdr_is_explicitly_unsupported() {
    let (mut camera, worker) = mock_camera(|stream| {
        let select = read_request(stream);
        assert_eq!((select.r#type, select.id), (2, 53));
        let expected = pb::TuningParam {
            id: 0,
            scene_id: 0,
            hdr_flag: 0,
        };
        assert_eq!(
            select.body.as_ref().unwrap().value,
            Some(pb::command_body::Value::TuningParam(expected))
        );
        send(stream, &response(&select, None));
        let readback = read_request(stream);
        send(
            stream,
            &response(
                &readback,
                Some(pb::command_body::Value::TuningParam(expected)),
            ),
        );
        let information = read_request(stream);
        send(
            stream,
            &response(
                &information,
                Some(pb::command_body::Value::TuningFileInfo(
                    pb::TuningFileInfo::default(),
                )),
            ),
        );
    });
    camera.state.version = Some(CameraVersion {
        firmware: "YunxiCamera-ch131b-0.0.7".into(),
        ..Default::default()
    });
    camera.state.unsupported.insert(
        "hdr".into(),
        "camera rejected command98 with code100".into(),
    );
    assert_eq!(camera.select_profile(0, 0).unwrap().hdr_flag, 0);
    worker.join().unwrap();
}

#[test]
fn a_missing_optional_control_is_disabled_without_losing_other_capabilities() {
    let failures = Arc::new(AtomicUsize::new(0));
    let observed = failures.clone();
    let (client, worker) = server(move |listener| {
        let mut stream = accept(&listener);
        loop {
            let request = read_request(&mut stream);
            if request.id == 77 {
                // An unsupported GET can leave an unusable stream. Both read
                // attempts fail, but an independent heartbeat remains healthy.
                observed.fetch_add(1, Ordering::Relaxed);
                drop(stream);
                stream = accept(&listener);
                continue;
            }
            let value = match request.id {
                1 => Some(pb::command_body::Value::VersionInfo(pb::VersionInfo {
                    serial_num: "TEST-ONLY".into(),
                    sys_version: "YunxiCamera-ch131b-1.0.4".into(),
                    sys_code: "1349".into(),
                    ..Default::default()
                })),
                9 => Some(pb::command_body::Value::IntVal(48)),
                _ => None,
            };
            let mut reply = response(&request, value.clone());
            if value.is_none() {
                reply.resp_code = 100;
            }
            send(&mut stream, &reply);
            if request.id == 33 {
                break;
            }
        }
    });
    let mut camera = Camera::new(client.ip, client.port, Duration::from_millis(400));
    let state = camera.connect().unwrap();
    assert_eq!(state.values["contrast"], 48);
    assert!(state.unsupported.contains_key("au-delay"));
    assert!(camera.client.is_connected());
    assert_eq!(failures.load(Ordering::Relaxed), 2);
    worker.join().unwrap();
}

#[test]
fn write_only_controls_use_acknowledgements_and_never_fabricate_readback() {
    let (mut camera, worker) = mock_camera(|stream| {
        let set = read_request(stream);
        assert_eq!((set.r#type, set.id), (2, 214));
        assert_eq!(
            set.body.as_ref().unwrap().value,
            Some(pb::command_body::Value::IntVal(1))
        );
        send(stream, &response(&set, None));
    });
    assert!(!camera.state.values.contains_key("face-roi"));
    assert!(
        camera
            .get_param("face-roi")
            .unwrap_err()
            .to_string()
            .contains("write-only")
    );
    assert_eq!(camera.set_param("face-roi", 1).unwrap(), 1);
    assert_eq!(camera.state.values["face-roi"], 1);
    worker.join().unwrap();
}
