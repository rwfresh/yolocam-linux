// SPDX-License-Identifier: GPL-3.0-or-later
//! Focus fixtures use local TCP listeners and synthetic camera identities.
use prost::Message as _;
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use yolocam_linux::{
    camera::{Camera, CameraState, CameraVersion, Crop},
    focus::{FocusTarget, READBACK_TOLERANCE, control_reason, firmware_supported},
    pb,
    protocol::{MAX_PAYLOAD_SIZE, pack_frame, unpack_frame},
};

fn version() -> CameraVersion {
    CameraVersion {
        serial: "fixture".into(),
        firmware: "YunxiCamera-ch131b-1.0.4".into(),
        build: 1349,
    }
}

fn afc_target() -> FocusTarget {
    FocusTarget::new(0.299_740, 0.228_704, 0.699_479, 0.768_519).unwrap()
}

fn afs_target() -> FocusTarget {
    FocusTarget::new(0.457_812, 0.425_926, 0.541_667, 0.573_611).unwrap()
}

fn rect(target: FocusTarget) -> pb::command_body::Value {
    pb::command_body::Value::RectF(pb::RectF {
        left: target.left,
        top: target.top,
        right: target.right,
        bottom: target.bottom,
    })
}

fn assert_target(request: &pb::Message, expected: FocusTarget) {
    let Some(pb::command_body::Value::RectF(actual)) =
        request.body.as_ref().and_then(|body| body.value.as_ref())
    else {
        panic!("Focus action did not contain a RectF");
    };
    // Re-centering a float32 rectangle can differ by one arithmetic ULP.
    for (actual, expected) in [
        (actual.left, expected.left),
        (actual.top, expected.top),
        (actual.right, expected.right),
        (actual.bottom, expected.bottom),
    ] {
        assert!((actual - expected).abs() < 1e-6);
    }
}

fn request(stream: &mut TcpStream, kind: i32, cid: u32) -> pb::Message {
    let mut header = [0; 8];
    stream.read_exact(&mut header).unwrap();
    let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    assert!(size <= MAX_PAYLOAD_SIZE + 8);
    let mut frame = header.to_vec();
    frame.resize(size + 8, 0);
    stream.read_exact(&mut frame[8..]).unwrap();
    let request = pb::Message::decode(unpack_frame(&frame).unwrap().as_slice()).unwrap();
    assert_eq!(
        (request.r#type, request.id),
        (kind, i32::try_from(cid).unwrap())
    );
    assert_ne!(request.id, 214);
    request
}

fn send(stream: &mut TcpStream, message: &pb::Message) {
    let mut frame = pack_frame(&message.encode_to_vec());
    frame[1] = 0;
    stream.write_all(&frame).unwrap();
}

fn reply(request: &pb::Message, value: Option<pb::command_body::Value>) -> pb::Message {
    pb::Message {
        r#type: 4,
        seq: request.seq,
        id: request.id,
        resp_code: 200,
        body: value.map(|value| pb::CommandBody { value: Some(value) }),
        ..Default::default()
    }
}

fn answer(stream: &mut TcpStream, cid: u32, value: pb::command_body::Value) {
    let incoming = request(stream, 1, cid);
    send(stream, &reply(&incoming, Some(value)));
}

fn heartbeat(stream: &mut TcpStream) {
    let incoming = request(stream, 2, 215);
    assert_eq!(
        incoming.body.as_ref().unwrap().value,
        Some(pb::command_body::Value::IntVal(1))
    );
    send(stream, &reply(&incoming, None));
}

fn accept(listener: &TcpListener) -> TcpStream {
    let (mut stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    heartbeat(&mut stream);
    stream
}

fn server(task: impl FnOnce(TcpListener) + Send + 'static) -> (Camera, JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let worker = thread::spawn(move || task(listener));
    let mut camera = Camera::new(Some(Ipv4Addr::LOCALHOST), port, Duration::from_millis(400));
    camera.state.version = Some(version());
    camera.state.values.insert("focus-mode".into(), 4); // Deliberately stale.
    camera.state.values.insert("focus-level".into(), 0);
    (camera, worker)
}

fn prepare(stream: &mut TcpStream, mode: u32, area: u32, target: FocusTarget) {
    answer(stream, 23, pb::command_body::Value::IntVal(mode));
    answer(stream, 38, pb::command_body::Value::IntVal(area));
    answer(stream, 24, rect(target));
}

#[test]
fn normalized_geometry_rejects_invalid_bounds_and_nonfinite_coordinates() {
    for edges in [
        [f32::NAN, 0.0, 1.0, 1.0],
        [0.0, f32::INFINITY, 1.0, 1.0],
        [-0.001, 0.0, 1.0, 1.0],
        [0.0, 0.0, 1.001, 1.0],
        [0.4, 0.2, 0.4, 0.8],
        [0.5, 0.2, 0.4, 0.8],
        [0.2, 0.8, 0.4, 0.7],
    ] {
        assert!(FocusTarget::new(edges[0], edges[1], edges[2], edges[3]).is_err());
    }
    let target = afc_target();
    for point in [
        (f32::NAN, 0.5),
        (0.5, f32::NEG_INFINITY),
        (-0.01, 0.5),
        (0.5, 1.01),
    ] {
        assert!(target.moved_to(point.0, point.1).is_err());
    }
    let mut invalid = target;
    invalid.right = f32::NAN;
    assert!(!target.approximately_equal(&invalid));
}

#[test]
fn moving_clamps_at_all_image_edges_and_preserves_actual_size() {
    let target = afc_target();
    for point in [(0.0, 0.0), (1.0, 1.0), (0.0, 1.0), (1.0, 0.0), (0.5, 0.5)] {
        let moved = target.moved_to(point.0, point.1).unwrap();
        moved.validate().unwrap();
        assert!((moved.width() - target.width()).abs() < 1e-6);
        assert!((moved.height() - target.height()).abs() < 1e-6);
    }
    let corner = target.moved_to(1.0, 0.0).unwrap();
    assert_eq!(corner.right, 1.0);
    assert_eq!(corner.top, 0.0);
    let full = FocusTarget::new(0.0, 0.0, 1.0, 1.0).unwrap();
    assert_eq!(full.moved_to(0.0, 1.0).unwrap(), full);
    let encoded = serde_json::to_string(&target).unwrap();
    assert_eq!(
        serde_json::from_str::<FocusTarget>(&encoded).unwrap(),
        target
    );
}

#[test]
fn target_is_command_body_field14_with_four_little_endian_float32_edges() {
    let target = FocusTarget::new(0.25, 0.125, 0.75, 0.625).unwrap();
    let body = pb::CommandBody {
        value: Some(rect(target)),
    };
    let mut expected = vec![0x72, 20]; // RectF, field14, length20.
    for (tag, value) in [
        (0x0d, target.left),
        (0x15, target.top),
        (0x1d, target.right),
        (0x25, target.bottom),
    ] {
        expected.push(tag);
        expected.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(body.encode_to_vec(), expected);
    assert_eq!(unpack_frame(&pack_frame(&expected)).unwrap(), expected);
}

#[test]
fn only_the_verified_model_firmware_and_build_enable_the_control() {
    assert!(firmware_supported(&version()));
    let mut state = CameraState {
        version: Some(version()),
        focus_target: Some(afc_target()),
        ..Default::default()
    };
    state.values.insert("focus-mode".into(), 3);
    state.values.insert("focus-level".into(), 2);
    assert!(control_reason(&state).is_none());
    for firmware in [
        "1.0.4",
        "YunxiCamera-ch131b-1.0.5",
        "YunxiCamera-ch131b-0.0.7",
        "OtherCamera-1.0.4",
    ] {
        let mut unsupported = version();
        unsupported.firmware = firmware.into();
        assert!(!firmware_supported(&unsupported));
        state.version = Some(unsupported);
        assert!(control_reason(&state).is_some());
    }
    state.version = Some(CameraVersion {
        build: 1350,
        ..version()
    });
    assert!(control_reason(&state).is_some());
    state.version = Some(version());
    for mode in [2, 4, 99] {
        state.values.insert("focus-mode".into(), mode);
        assert!(control_reason(&state).is_some());
    }
    state.values.insert("focus-mode".into(), 1);
    state.values.insert("focus-level".into(), 3);
    assert!(control_reason(&state).is_some());
    state.values.insert("focus-level".into(), 2);
    state.focus_target = None;
    assert!(control_reason(&state).is_some());
    state.focus_target = Some(afs_target());
    state
        .unsupported
        .insert("focus-target".into(), "Readback unavailable".into());
    assert_eq!(
        control_reason(&state).as_deref(),
        Some("Readback unavailable")
    );
}

#[test]
fn unknown_firmware_and_invalid_points_are_rejected_without_network_access() {
    let mut camera = Camera::new(None, 12345, Duration::from_millis(400));
    assert!(
        camera
            .read_focus_target()
            .unwrap_err()
            .to_string()
            .contains("verified S3")
    );
    assert!(
        camera
            .set_focus_target(afc_target())
            .unwrap_err()
            .to_string()
            .contains("verified S3")
    );
    assert!(
        camera
            .move_focus_target(f32::NAN, 0.5)
            .unwrap_err()
            .to_string()
            .contains("Focus point")
    );
    camera.state.version = Some(version());
    let mut invalid = afc_target();
    invalid.left = -0.1;
    assert!(
        camera
            .set_focus_target(invalid)
            .unwrap_err()
            .to_string()
            .contains("coordinates")
    );
    assert!(!camera.client.is_connected());
}

#[test]
fn afs_and_afc_moves_use_fresh_geometry_and_accept_sensor_pixel_quantization() {
    for (mode, initial) in [(1, afs_target()), (3, afc_target())] {
        let desired = initial.moved_to(0.62, 0.42).unwrap();
        let quantize = |edge: f32| (edge * 1080.0).floor() / 1080.0;
        let actual = FocusTarget::new(
            quantize(desired.left),
            quantize(desired.top),
            quantize(desired.right),
            quantize(desired.bottom),
        )
        .unwrap();
        let (mut camera, worker) = server(move |listener| {
            let mut stream = accept(&listener);
            prepare(&mut stream, mode, 2, initial);
            let action = request(&mut stream, 2, 24);
            assert_target(&action, desired);
            send(
                &mut stream,
                &pb::Message {
                    r#type: 5,
                    seq: action.seq,
                    id: 59,
                    body: Some(pb::CommandBody {
                        value: Some(pb::command_body::Value::ListRectF(pb::ListRectF {
                            values: vec![pb::RectF {
                                left: 0.2,
                                top: 0.1,
                                right: 0.4,
                                bottom: 0.5,
                            }],
                        })),
                    }),
                    ..Default::default()
                },
            );
            let mut stale = reply(&action, None);
            stale.seq -= 1;
            send(&mut stream, &stale);
            let mut wrong_id = reply(&action, None);
            wrong_id.id = 214;
            send(&mut stream, &wrong_id);
            send(&mut stream, &reply(&action, None));
            answer(&mut stream, 24, rect(actual));
        });
        // ePTZ is intentionally present: CID24 stays in output coordinates.
        camera.state.eptz = Some(Crop::new(2.0, 0.3, 0.1).unwrap());
        camera.client.connect().unwrap();
        let returned = camera.set_focus_target(desired).unwrap();
        assert_eq!(returned, actual);
        assert_eq!(camera.state.focus_target, Some(actual));
        assert_eq!(camera.state.values.get("focus-mode"), Some(&(mode as i32)));
        assert_eq!(camera.state.values.get("focus-level"), Some(&2));
        worker.join().unwrap();
    }
}

#[test]
fn manual_and_face_modes_are_rejected_after_fresh_mode_read_without_target_writes() {
    for mode in [2, 4] {
        let (mut camera, worker) = server(move |listener| {
            let mut stream = accept(&listener);
            answer(&mut stream, 23, pb::command_body::Value::IntVal(mode));
            heartbeat(&mut stream); // No GET/SET24, no CID214.
        });
        camera.state.values.insert("focus-mode".into(), 1);
        camera.client.connect().unwrap();
        let error = camera.set_focus_target(afs_target()).unwrap_err();
        assert!(error.to_string().contains("AF-S or AF-C"));
        assert_eq!(camera.state.values.get("focus-mode"), Some(&(mode as i32)));
        camera.heartbeat().unwrap();
        worker.join().unwrap();
    }
}

#[test]
fn custom_area_is_rejected_after_fresh_level_read_without_target_writes() {
    let (mut camera, worker) = server(|listener| {
        let mut stream = accept(&listener);
        answer(&mut stream, 23, pb::command_body::Value::IntVal(3));
        answer(&mut stream, 38, pb::command_body::Value::IntVal(3));
        heartbeat(&mut stream);
    });
    camera.client.connect().unwrap();
    assert!(
        camera
            .set_focus_target(afc_target())
            .unwrap_err()
            .to_string()
            .contains("Large, Medium and Small")
    );
    camera.heartbeat().unwrap();
    worker.join().unwrap();
}

#[test]
fn resizing_is_rejected_but_sensor_pixel_extent_rounding_preserves_fresh_size() {
    let initial = afc_target();
    let desired = initial.moved_to(0.62, 0.42).unwrap();
    let (mut camera, worker) = server(move |listener| {
        let mut stream = accept(&listener);
        prepare(&mut stream, 3, 0, initial);
        heartbeat(&mut stream);
        prepare(&mut stream, 3, 0, initial);
        let action = request(&mut stream, 2, 24);
        assert_target(&action, desired);
        send(&mut stream, &reply(&action, None));
        answer(&mut stream, 24, rect(desired));
    });
    camera.client.connect().unwrap();
    let resized = FocusTarget::new(0.2, 0.2, 0.3, 0.3).unwrap();
    assert!(
        camera
            .set_focus_target(resized)
            .unwrap_err()
            .to_string()
            .contains("preserves")
    );
    assert_eq!(camera.state.focus_target, Some(initial));
    camera.heartbeat().unwrap();
    let mut rounded = desired;
    rounded.left -= 0.0004;
    rounded.right += 0.0004;
    assert_eq!(camera.set_focus_target(rounded).unwrap(), desired);
    worker.join().unwrap();
}

#[test]
fn a_meaningful_readback_mismatch_returns_error_and_retains_actual_bounds() {
    let initial = afc_target();
    let desired = initial.moved_to(0.62, 0.42).unwrap();
    let different = desired
        .moved_to(0.62 + READBACK_TOLERANCE * 20.0, 0.42)
        .unwrap();
    assert!(!desired.approximately_equal(&different));
    let (mut camera, worker) = server(move |listener| {
        let mut stream = accept(&listener);
        prepare(&mut stream, 3, 1, initial);
        let action = request(&mut stream, 2, 24);
        send(&mut stream, &reply(&action, None));
        answer(&mut stream, 24, rect(different));
    });
    camera.client.connect().unwrap();
    let error = camera.set_focus_target(desired).unwrap_err();
    assert!(error.to_string().contains("different bounds"));
    assert_eq!(camera.state.focus_target, Some(different));
    worker.join().unwrap();
}

#[test]
fn a_focus_action_with_lost_ack_is_never_replayed() {
    let initial = afs_target();
    let desired = initial.moved_to(0.62, 0.42).unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let writes = count.clone();
    let (mut camera, worker) = server(move |listener| {
        let mut first = accept(&listener);
        prepare(&mut first, 1, 2, initial);
        let action = request(&mut first, 2, 24);
        assert_target(&action, desired);
        writes.fetch_add(1, Ordering::Relaxed);
        drop(first);
        // Only the caller's subsequent explicit read reconnects. No SET replay.
        let mut second = accept(&listener);
        answer(&mut second, 24, rect(desired));
    });
    camera.client.connect().unwrap();
    let error = camera.set_focus_target(desired).unwrap_err();
    assert!(format!("{error:#}").contains("was not replayed"));
    assert_eq!(camera.state.focus_target, None);
    assert!(!camera.client.is_connected());
    assert_eq!(camera.read_focus_target().unwrap(), desired);
    assert_eq!(count.load(Ordering::Relaxed), 1);
    worker.join().unwrap();
}

#[test]
fn acknowledged_action_with_failed_readback_is_reported_as_acknowledged_but_unverified() {
    let initial = afc_target();
    let (mut camera, worker) = server(move |listener| {
        let mut stream = accept(&listener);
        prepare(&mut stream, 3, 2, initial);
        let action = request(&mut stream, 2, 24);
        send(&mut stream, &reply(&action, None));
        let read = request(&mut stream, 1, 24);
        let mut rejected = reply(&read, None);
        rejected.resp_code = 500;
        send(&mut stream, &rejected);
    });
    camera.client.connect().unwrap();
    let error = camera.move_focus_target(0.62, 0.42).unwrap_err();
    assert!(error.to_string().contains("acknowledged the focus move"));
    assert!(error.to_string().contains("readback is unavailable"));
    assert_eq!(camera.state.focus_target, None);
    worker.join().unwrap();
}

#[test]
fn readbacks_must_be_valid_rectangles_and_cannot_reuse_stale_cached_bounds() {
    for value in [
        pb::command_body::Value::IntVal(1),
        pb::command_body::Value::RectF(pb::RectF {
            left: f32::NAN,
            top: 0.1,
            right: 0.8,
            bottom: 0.9,
        }),
        pb::command_body::Value::RectF(pb::RectF {
            left: 0.5,
            top: 0.1,
            right: 0.4,
            bottom: 0.9,
        }),
    ] {
        let (mut camera, worker) = server(move |listener| {
            let mut stream = accept(&listener);
            answer(&mut stream, 24, value);
        });
        camera.state.focus_target = Some(afc_target());
        camera.client.connect().unwrap();
        assert!(camera.read_focus_target().is_err());
        assert_eq!(camera.state.focus_target, None);
        assert!(camera.state.unsupported.contains_key("focus-target"));
        worker.join().unwrap();
    }
}
