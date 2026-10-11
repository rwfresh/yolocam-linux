// SPDX-License-Identifier: GPL-3.0-or-later
// Wire format based on yolocam-linux, Copyright (C) 2026 Seth Hillbrand.
//! Bounded, synchronous camera transport. A single application worker owns this
//! client: writes are never replayed after an uncertain acknowledgement.

use crate::{discovery, pb};
use anyhow::{Context, Result, anyhow, bail, ensure};
use prost::Message as _;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

pub const DEFAULT_PORT: u16 = 12345;
pub const HEARTBEAT_CID: u32 = 215;
/// A firmware chunk is 2 MiB, plus its protobuf envelope.
pub const MAX_PAYLOAD_SIZE: usize = 4 * 1024 * 1024;
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(100);
const TYPE_REQUEST: i32 = 1;
const TYPE_SET: i32 = 2;
const TYPE_RESPONSE: i32 = 4;
const TYPE_NOTIFY: i32 = 5;

fn xor(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0, |sum, byte| sum ^ byte)
}

pub fn pack_frame(payload: &[u8]) -> Vec<u8> {
    assert!(
        payload.len() <= MAX_PAYLOAD_SIZE,
        "camera payload exceeds frame limit"
    );
    let mut out = Vec::with_capacity(payload.len() + 16);
    out.extend_from_slice(&[0x08, 0x01, 0, 0]);
    out.extend_from_slice(&((payload.len() + 8) as u32).to_le_bytes());
    out.push(0xa5);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.push(xor(&out[8..13]));
    out.extend_from_slice(payload);
    out.extend_from_slice(&[xor(payload), 0x5a]);
    out
}

fn outer_size(header: &[u8]) -> Result<usize> {
    ensure!(header.len() == 8, "incomplete outer camera header");
    ensure!(header[0] == 0x08, "invalid camera outer start flag");
    // The camera sends 0; clients send 1. Accept both for offline round trips.
    ensure!(header[1] <= 1, "invalid camera client type");
    ensure!(
        header[2..4] == [0, 0],
        "invalid camera reserved header bytes"
    );
    let size = u32::from_le_bytes(header[4..8].try_into()?) as usize;
    ensure!(
        (8..=MAX_PAYLOAD_SIZE + 8).contains(&size),
        "camera frame length out of bounds: {size}"
    );
    Ok(size)
}

pub fn unpack_frame(frame: &[u8]) -> Result<Vec<u8>> {
    ensure!(frame.len() >= 16, "camera frame too short");
    let size = outer_size(&frame[..8])?;
    ensure!(frame.len() == size + 8, "camera frame length mismatch");
    let inner = &frame[8..];
    ensure!(inner[0] == 0xa5, "invalid camera inner start flag");
    let payload_size = u32::from_be_bytes(inner[1..5].try_into()?) as usize;
    ensure!(
        payload_size <= MAX_PAYLOAD_SIZE && payload_size + 8 == size,
        "camera outer and inner lengths disagree"
    );
    ensure!(
        xor(&inner[..5]) == inner[5],
        "camera header checksum mismatch"
    );
    let payload = &inner[6..6 + payload_size];
    ensure!(
        xor(payload) == inner[6 + payload_size],
        "camera data checksum mismatch"
    );
    ensure!(inner[7 + payload_size] == 0x5a, "invalid camera end flag");
    Ok(payload.to_vec())
}

pub struct Client {
    pub ip: Option<Ipv4Addr>,
    pub port: u16,
    explicit_ip: Option<Ipv4Addr>,
    timeout: Duration,
    stream: Option<TcpStream>,
    enabled: bool,
    sequence: u32,
    last_request: Option<Instant>,
}

impl Client {
    pub fn new(ip: Option<Ipv4Addr>, port: u16, timeout: Duration) -> Self {
        Self {
            ip,
            port,
            explicit_ip: ip,
            timeout,
            stream: None,
            enabled: false,
            sequence: 0,
            last_request: None,
        }
    }

    pub fn address(&self) -> Option<SocketAddrV4> {
        self.ip.map(|ip| SocketAddrV4::new(ip, self.port))
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
    pub fn set_timeout(&mut self, timeout: Duration) -> Result<()> {
        ensure!(
            !timeout.is_zero(),
            "camera timeout must be greater than zero"
        );
        self.timeout = timeout;
        Ok(())
    }

    pub fn connect(&mut self) -> Result<()> {
        ensure!(
            !self.timeout.is_zero(),
            "camera timeout must be greater than zero"
        );
        self.enabled = true;
        self.drop_stream();
        self.open_stream()
    }

    fn open_stream(&mut self) -> Result<()> {
        ensure!(self.enabled, "camera connection is closed");
        let ip = match self.explicit_ip {
            Some(ip) => ip,
            None => discovery::discover_ip()?,
        };
        self.ip = Some(ip);
        let address = SocketAddr::V4(SocketAddrV4::new(ip, self.port));
        let stream = TcpStream::connect_timeout(&address, self.timeout)
            .with_context(|| format!("cannot connect to camera at {address}"))?;
        stream.set_nodelay(true)?;
        self.stream = Some(stream);
        // Firmware expects an immediate heartbeat, then one every 30 seconds.
        let response = self.exchange(
            TYPE_SET,
            HEARTBEAT_CID,
            Some(pb::command_body::Value::IntVal(1)),
            &mut |_| Ok(()),
        );
        match response {
            Ok(response) if response.resp_code == 200 => Ok(()),
            Ok(response) => {
                self.drop_stream();
                bail!("camera rejected heartbeat: code {}", response.resp_code)
            }
            Err(error) => {
                self.drop_stream();
                Err(error.context("camera heartbeat handshake failed"))
            }
        }
    }

    pub fn disconnect(&mut self) {
        self.enabled = false;
        self.drop_stream();
    }

    /// Discard an invalid transport while retaining the requested connection
    /// state. The next operation establishes a fresh socket and heartbeat.
    /// Unlike `disconnect`, this also permits normal connection recovery.
    pub fn abort_transport(&mut self) {
        self.drop_stream();
    }

    fn drop_stream(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn ensure_stream(&mut self) -> Result<()> {
        ensure!(self.enabled, "camera connection is closed; connect first");
        if self.stream.is_none() {
            self.open_stream()?;
        }
        Ok(())
    }

    pub fn get(&mut self, cid: u32) -> Result<pb::Message> {
        ensure!(cid <= i32::MAX as u32, "camera command ID out of range");
        let mut first_error = None;
        for attempt in 0..2 {
            let response = self
                .ensure_stream()
                .and_then(|()| self.exchange(TYPE_REQUEST, cid, None, &mut |_| Ok(())));
            match response {
                Ok(message) => return Ok(message),
                Err(error) => {
                    if !self.enabled || attempt == 1 {
                        return Err(error.context("camera read failed after connection recovery"));
                    }
                    first_error = Some(error);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| anyhow!("camera read failed")))
    }

    pub fn set_int(&mut self, cid: u32, value: u32) -> Result<pb::Message> {
        self.set(cid, pb::command_body::Value::IntVal(value))
    }

    pub fn set(&mut self, cid: u32, value: pb::command_body::Value) -> Result<pb::Message> {
        self.set_with_notifications(cid, value, &mut |_| Ok(()))
    }

    pub fn set_with_notifications(
        &mut self,
        cid: u32,
        value: pb::command_body::Value,
        notify: &mut dyn FnMut(&pb::Message) -> Result<()>,
    ) -> Result<pb::Message> {
        ensure!(cid <= i32::MAX as u32, "camera command ID out of range");
        self.ensure_stream()?;
        self.exchange(TYPE_SET, cid, Some(value), notify)
            .context("camera did not acknowledge write; it may have applied and was not replayed")
    }

    fn exchange(
        &mut self,
        kind: i32,
        cid: u32,
        value: Option<pb::command_body::Value>,
        notify: &mut dyn FnMut(&pb::Message) -> Result<()>,
    ) -> Result<pb::Message> {
        if let Some(last) = self.last_request
            && let Some(delay) = MIN_REQUEST_INTERVAL.checked_sub(last.elapsed())
        {
            std::thread::sleep(delay);
        }
        self.sequence = self.sequence.wrapping_add(1);
        // Reserve zero for unsolicited messages even after wrapping.
        if self.sequence == 0 {
            self.sequence = 1;
        }
        let sequence = self.sequence;
        let request = pb::Message {
            r#type: kind,
            seq: sequence,
            timeout: self.timeout.as_millis().min(u32::MAX as u128) as u32,
            resp_code: 0,
            id: cid as i32,
            body: value.map(|value| pb::CommandBody { value: Some(value) }),
        };
        let payload = request.encode_to_vec();
        ensure!(
            payload.len() <= MAX_PAYLOAD_SIZE,
            "camera request exceeds frame limit"
        );
        let frame = pack_frame(&payload);
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .context("camera timeout is too large")?;
        self.last_request = Some(Instant::now());
        let result = (|| {
            let stream = self
                .stream
                .as_mut()
                .context("camera control socket is closed")?;
            write_until(stream, &frame, deadline)?;
            loop {
                let message = self.receive_until(deadline)?;
                if message.r#type == TYPE_NOTIFY {
                    notify(&message)?;
                    if message.id == 66 {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "camera control connection was replaced by another controller",
                        )
                        .into());
                    }
                    continue;
                }
                // Firmware can leave old replies and face notifications queued.
                if message.r#type == TYPE_RESPONSE
                    && message.seq == sequence
                    && message.id == cid as i32
                {
                    return Ok(message);
                }
            }
        })();
        if result.is_err() {
            self.drop_stream();
        }
        result
    }

    /// Read exactly one validated protobuf frame within a single wall-clock
    /// deadline. A partial timeout invalidates the socket, preventing reuse of a
    /// half-consumed stream. This also exposes upgrade notifications to firmware.
    pub fn receive_until(&mut self, deadline: Instant) -> Result<pb::Message> {
        let result = (|| {
            let stream = self
                .stream
                .as_mut()
                .context("camera control socket is closed")?;
            let mut header = [0; 8];
            read_until(stream, &mut header, deadline)?;
            let size = outer_size(&header)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let mut frame = vec![0; size + 8];
            frame[..8].copy_from_slice(&header);
            read_until(stream, &mut frame[8..], deadline)?;
            let payload = unpack_frame(&frame)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            pb::Message::decode(payload.as_slice())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
                .context("invalid camera protobuf message")
        })();
        if result.is_err() {
            self.drop_stream();
        }
        result
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.disconnect();
    }
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "camera request exceeded its deadline",
            )
        })?;
    Ok(remaining)
}

fn read_until(stream: &mut TcpStream, mut buffer: &mut [u8], deadline: Instant) -> Result<()> {
    while !buffer.is_empty() {
        stream.set_read_timeout(Some(remaining(deadline)?))?;
        match stream.read(buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "camera closed the control connection",
                )
                .into());
            }
            Ok(count) => buffer = &mut buffer[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn write_until(stream: &mut TcpStream, mut buffer: &[u8], deadline: Instant) -> Result<()> {
    while !buffer.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "camera accepted no data on its control connection",
                )
                .into());
            }
            Ok(count) => buffer = &buffer[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
