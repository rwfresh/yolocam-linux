// SPDX-License-Identifier: GPL-3.0-or-later
//! Read-only USB discovery; this module never modifies network configuration.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn matches_camera(mut device: &Path) -> bool {
    loop {
        if let (Ok(vendor), Ok(product)) = (
            fs::read_to_string(device.join("idVendor")),
            fs::read_to_string(device.join("idProduct")),
        ) {
            return vendor.trim().eq_ignore_ascii_case("46f8")
                && product.trim().eq_ignore_ascii_case("0855");
        }
        match device.parent() {
            Some(parent) => device = parent,
            None => return false,
        }
    }
}

fn camera_interfaces_at(net_root: &Path) -> Vec<String> {
    let mut interfaces = Vec::new();
    if let Ok(entries) = fs::read_dir(net_root) {
        for entry in entries.flatten() {
            if let Ok(device) = entry.path().join("device").canonicalize()
                && matches_camera(&device)
            {
                interfaces.push(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    interfaces.sort();
    interfaces
}

pub fn camera_interfaces() -> Vec<String> {
    camera_interfaces_at(Path::new("/sys/class/net"))
}

fn output(program: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                bail!("{program} exited with {status}");
            }
            let mut text = String::new();
            child
                .stdout
                .take()
                .context("missing command output")?
                .read_to_string(&mut text)?;
            return Ok(text);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn dhcp_server(text: &str) -> Option<Ipv4Addr> {
    text.lines().find_map(|line| {
        let (_, value) = line.split_once("dhcp_server_identifier")?;
        value.trim().strip_prefix('=')?.trim().parse().ok()
    })
}

fn private_link_camera(text: &str) -> Option<Ipv4Addr> {
    let devices: Value = serde_json::from_str(text).ok()?;
    for device in devices.as_array()? {
        let Some(addresses) = device.get("addr_info").and_then(Value::as_array) else {
            continue;
        };
        for address in addresses {
            if address.get("prefixlen").and_then(Value::as_u64) != Some(24) {
                continue;
            }
            let Some(local) = address
                .get("local")
                .and_then(Value::as_str)
                .and_then(|value| value.parse::<Ipv4Addr>().ok())
            else {
                continue;
            };
            if local.is_private() {
                let mut octets = local.octets();
                octets[3] = 10;
                return Some(Ipv4Addr::from(octets));
            }
        }
    }
    None
}

pub fn discover_ip() -> Result<Ipv4Addr> {
    if let Some(value) = std::env::var_os("YOLOCAM_IP") {
        return value
            .to_string_lossy()
            .parse()
            .context("YOLOCAM_IP must contain an IPv4 address");
    }
    for interface in camera_interfaces() {
        if let Ok(text) = output(
            "nmcli",
            &["-g", "DHCP4.OPTION", "device", "show", &interface],
        ) && let Some(ip) = dhcp_server(&text)
        {
            return Ok(ip);
        }
        if let Ok(text) = output("ip", &["-j", "-4", "address", "show", "dev", &interface])
            && let Some(ip) = private_link_camera(&text)
        {
            return Ok(ip);
        }
    }
    // Default used by early S3 firmware. Callers may override it explicitly.
    Ok(Ipv4Addr::new(192, 168, 126, 10))
}

pub fn discover_video() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("YOLOCAM_VIDEO") {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }
    let mut persistent = Vec::new();
    if let Ok(entries) = fs::read_dir("/dev/v4l/by-id") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.to_ascii_lowercase().contains("yolocam")
                && name.ends_with("-video-index0")
                && entry.path().exists()
            {
                persistent.push(entry.path());
            }
        }
    }
    persistent.sort();
    if let Some(path) = persistent.into_iter().next() {
        return Some(path);
    }
    let mut devices = Vec::new();
    for entry in fs::read_dir("/sys/class/video4linux").ok()?.flatten() {
        let path = entry.path();
        if fs::read_to_string(path.join("index"))
            .ok()
            .is_none_or(|index| index.trim() != "0")
        {
            continue;
        }
        if path
            .join("device")
            .canonicalize()
            .ok()
            .is_some_and(|device| matches_camera(&device))
        {
            devices.push(Path::new("/dev").join(entry.file_name()));
        }
    }
    devices.sort();
    devices.into_iter().next()
}

pub fn diagnostics() -> Value {
    json!({
        "usb_vendor": "46f8", "usb_product": "0855",
        "interfaces": camera_interfaces(),
        "camera_ip": discover_ip().map(|ip| ip.to_string()).ok(),
        "video_device": discover_video(),
        "kernel_drivers": ["uvcvideo", "cdc_ncm"],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dhcp_address_is_taken_from_server_not_client() {
        assert_eq!(
            dhcp_server(
                "requested_routers = 1\ndhcp_server_identifier = 192.168.127.10\nip_address = 192.168.127.11\n"
            ),
            Some(Ipv4Addr::new(192, 168, 127, 10))
        );
    }

    #[test]
    fn fallback_only_uses_private_camera_subnets() {
        assert_eq!(
            private_link_camera(r#"[{"addr_info":[{"local":"192.168.127.11","prefixlen":24}]}]"#),
            Some(Ipv4Addr::new(192, 168, 127, 10))
        );
        assert!(
            private_link_camera(r#"[{"addr_info":[{"local":"8.8.8.11","prefixlen":24}]}]"#)
                .is_none()
        );
        assert!(
            private_link_camera(r#"[{"addr_info":[{"local":"192.168.127.11","prefixlen":16}]}]"#)
                .is_none()
        );
    }

    #[test]
    fn network_discovery_follows_usb_ancestors_and_excludes_other_hardware() {
        use std::os::unix::fs::symlink;
        let fixture = tempfile::tempdir().unwrap();
        let net = fixture.path().join("net");
        fs::create_dir_all(&net).unwrap();
        for (name, vendor, product) in [("enxcam", "46f8", "0855"), ("enxother", "46f8", "ffff")] {
            let usb = fixture.path().join(name);
            let child = usb.join("interface");
            fs::create_dir_all(&child).unwrap();
            fs::write(usb.join("idVendor"), vendor).unwrap();
            fs::write(usb.join("idProduct"), product).unwrap();
            fs::create_dir_all(net.join(name)).unwrap();
            symlink(&child, net.join(name).join("device")).unwrap();
        }
        assert_eq!(camera_interfaces_at(&net), vec!["enxcam"]);
    }
}
