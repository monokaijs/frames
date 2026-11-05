//! Launcher sessions on the v2 stack (`frames next host|connect`): the
//! command lines built from saved settings, the events a session process
//! reports in its log (`frames_next::launcher`), and the device ids the
//! launcher dials.

use super::{Device, SessionOptions};
use anyhow::Result;
use frames_core::adaptive::QualityPreset;
use std::io::{Read, Seek, SeekFrom};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

/// The most a viewer asks to receive for its quality setting, as the
/// Settings descriptions say (Mbps at 1080p); a bandwidth limit lowers it.
pub(super) fn max_mbps(options: &SessionOptions) -> u32 {
    let preset = match options.quality {
        QualityPreset::High => 50,
        QualityPreset::Medium => 12,
        QualityPreset::Low => 4,
    };
    match options.bandwidth_mbps {
        0 => preset,
        limit => preset.min(limit),
    }
}

fn on_off(on: bool) -> String {
    if on { "on" } else { "off" }.into()
}

/// `frames next connect` for `target` (a device id, or a prefix of a nearby
/// one). `--migrate-v1` lets hosts that trusted this device in v1 trust it
/// now without a new pairing code.
pub(super) fn connect_args(options: &SessionOptions, target: &str) -> Vec<String> {
    let mut args = vec![
        "next".into(),
        "connect".into(),
        target.into(),
        "--migrate-v1".into(),
        "--audio".into(),
        on_off(options.audio),
        "--clipboard".into(),
        on_off(options.clipboard),
        "--max-mbps".into(),
        max_mbps(options).to_string(),
    ];
    if options.physical_keys {
        args.push("--physical-keys".into());
    }
    if options.adb {
        args.push("--adb".into());
    }
    if options.gamepads {
        args.push("--gamepads".into());
    }
    for bus in &options.usb {
        args.extend(["--usb".into(), bus.clone()]);
    }
    // "Application windows": each host window in its own window.
    if !options.desktop {
        args.push("--apps".into());
    }
    // v2 has no VP9; automatic lets the host pick what both ends decode.
    if matches!(options.codec.as_str(), "h264" | "hevc" | "av1") {
        args.extend(["--codec".into(), options.codec.clone()]);
    }
    args
}

/// `frames next host`, sharing this screen until the launcher stops it and
/// asking the launcher about new devices.
pub(super) fn sharing_args(options: &SessionOptions) -> Vec<String> {
    let mut args = vec![
        "next".into(),
        "host".into(),
        "--stay".into(),
        "--ask-launcher".into(),
        // Each viewer chooses whether to receive it.
        "--audio".into(),
        "on".into(),
        "--clipboard".into(),
        on_off(options.clipboard),
        "--max-windows".into(),
        options.max_streams.to_string(),
        // The ADB bridge listens on loopback, for viewers trusted for
        // control that turn it on.
        "--adb".into(),
    ];
    if options.encoder != "auto" {
        args.extend(["--prefer-encoder".into(), options.encoder.clone()]);
    }
    args
}

/// Something a session process reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Report {
    /// Host: a new device asks to connect; answer on the process's stdin.
    Pairing {
        device: String,
        name: String,
        code: String,
    },
    /// Host: that request was answered or timed out.
    PairingDone { device: String },
    /// Viewer: the pairing code a new host shows.
    Code(String),
    /// Viewer: the host's id and name, once it started the stream.
    Host { device: String, name: String },
    /// Viewer: the user switched views in the connection menu.
    View { desktop: bool },
    /// Viewer: image quality (0 high, 1 medium, 2 low) or the remote cursor
    /// changed in the connection menu.
    Choices { quality: i32, remote_cursor: bool },
    /// Host: a device it let in.
    Viewer {
        device: String,
        name: String,
        control: bool,
    },
}

/// One log line as a [`Report`].
pub(super) fn parse(line: &str) -> Option<Report> {
    let (kind, json) = line.strip_prefix("Frames ")?.split_once(": ")?;
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
    Some(match kind {
        "pairing" => Report::Pairing {
            device: text("device"),
            name: text("name"),
            code: text("code"),
        },
        "pairing-done" => Report::PairingDone {
            device: text("device"),
        },
        "code" => Report::Code(text("code")),
        "host" => Report::Host {
            device: text("device"),
            name: text("name"),
        },
        "choices" => Report::Choices {
            quality: value["quality"].as_i64().unwrap_or(0) as i32,
            remote_cursor: value["remote_cursor"].as_bool().unwrap_or(false),
        },
        "view" => Report::View {
            desktop: value["view"].as_str() == Some("desktop"),
        },
        "viewer" => Report::Viewer {
            device: text("device"),
            name: text("name"),
            control: value["control"].as_bool().unwrap_or(false),
        },
        _ => return None,
    })
}

/// Reports in the part of a log written since `offset`, which moves past
/// the last complete line read.
pub(super) fn read_reports(path: &Path, offset: &mut u64) -> Vec<Report> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    if file.seek(SeekFrom::Start(*offset)).is_err() {
        return Vec::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return Vec::new();
    }
    let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
        return Vec::new();
    };
    *offset += end as u64 + 1;
    String::from_utf8_lossy(&bytes[..end])
        .lines()
        .filter_map(parse)
        .collect()
}

/// What the launcher dials for what someone typed or clicked: a device id;
/// an address of a nearby device (its id); or a short id prefix, which the
/// session resolves among nearby devices.
pub(super) fn target(value: &str, devices: &[Device]) -> Result<String> {
    let value = value.trim();
    if frames_next::directory::is_device_id(value) {
        return Ok(value.to_ascii_lowercase());
    }
    let ip = value
        .parse::<SocketAddr>()
        .map(|a| a.ip())
        .or_else(|_| value.parse::<IpAddr>());
    if let Ok(ip) = ip {
        return devices
            .iter()
            .find(|d| d.online && d.addresses.contains(&ip))
            .map(|d| d.fingerprint.clone())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No device sharing at {ip} was found on this network. Enter the device ID shown on the other device."
                )
            });
    }
    // A short code ("9710-3C89") or the start of an id: the nearby device
    // it names, when exactly one does; otherwise the session looks for it.
    let code = frames_next::directory::normalize_code(value);
    anyhow::ensure!(
        code.len() >= 8 && code.bytes().all(|b| b.is_ascii_hexdigit()),
        "Enter the code shown on the other device (such as 9710-3C89), its device ID, or its IP address on this network."
    );
    if frames_next::directory::is_device_id(&code) {
        return Ok(code);
    }
    let matches: Vec<&Device> = devices
        .iter()
        .filter(|d| d.online && d.fingerprint.starts_with(&code))
        .collect();
    Ok(match matches.as_slice() {
        [device] => device.fingerprint.clone(),
        _ => code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_round_trip_through_the_log_format() {
        assert_eq!(
            parse(r#"Frames pairing: {"device":"ab","name":"Mac","code":"12345678"}"#),
            Some(Report::Pairing {
                device: "ab".into(),
                name: "Mac".into(),
                code: "12345678".into()
            })
        );
        assert_eq!(
            parse(r#"Frames viewer: {"device":"ab","name":"PC","control":true}"#),
            Some(Report::Viewer {
                device: "ab".into(),
                name: "PC".into(),
                control: true
            })
        );
        assert_eq!(parse("Frames status: connected"), None);
        assert_eq!(parse("codec: Hevc"), None);
    }

    #[test]
    fn only_complete_lines_are_read_and_each_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.log");
        std::fs::write(
            &path,
            "noise\nFrames code: {\"code\":\"1234\"}\nFrames host: {\"dev",
        )
        .unwrap();
        let mut offset = 0;
        assert_eq!(
            read_reports(&path, &mut offset),
            [Report::Code("1234".into())]
        );
        assert!(read_reports(&path, &mut offset).is_empty());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        std::io::Write::write_all(&mut file, b"ice\":\"ab\",\"name\":\"Mac\"}\n").unwrap();
        assert_eq!(
            read_reports(&path, &mut offset),
            [Report::Host {
                device: "ab".into(),
                name: "Mac".into()
            }]
        );
    }

    #[test]
    fn typed_addresses_become_device_ids() {
        let id = "a".repeat(64);
        let devices = [Device {
            fingerprint: id.clone(),
            online: true,
            addresses: vec!["192.168.0.5".parse().unwrap()],
            ..Default::default()
        }];
        assert_eq!(target(&id.to_uppercase(), &devices).unwrap(), id);
        assert_eq!(target("192.168.0.5", &devices).unwrap(), id);
        assert_eq!(target("192.168.0.5:47990", &devices).unwrap(), id);
        assert!(target("192.168.0.9", &devices).is_err());
        assert_eq!(target("ABCDEF12", &devices).unwrap(), "abcdef12");
        assert!(target("abc", &devices).is_err());
        // The short code as shown, with its dash or spaces, finds the
        // nearby device.
        assert_eq!(target("AAAA-AAAA", &devices).unwrap(), id);
        assert_eq!(target(" aaaa aaaa ", &devices).unwrap(), id);
    }
}
