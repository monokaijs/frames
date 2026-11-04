//! `frames-next doctor`: what this machine can do as a v2 host or viewer,
//! and what is in the way. Read-only apart from creating this device's key
//! if it has none (as `peers` does); it never captures, injects input, or
//! accepts a connection.

use crate::{Options, admit};
use anyhow::Result;
use std::time::{Duration, Instant};

/// One finding: `ok` false is something to fix (with what to do).
struct Report {
    problems: usize,
}

impl Report {
    fn line(&mut self, ok: bool, what: &str, detail: impl std::fmt::Display) {
        if !ok {
            self.problems += 1;
        }
        println!("  [{}] {what}: {detail}", if ok { " ok " } else { "fix " });
    }

    fn info(&self, what: &str, detail: impl std::fmt::Display) {
        println!("  [    ] {what}: {detail}");
    }
}

pub async fn run(o: Options) -> Result<()> {
    let mut report = Report { problems: 0 };
    println!(
        "Frames Next {} on {}",
        env!("CARGO_PKG_VERSION"),
        crate::platform()
    );

    println!("identity and trust");
    let config = admit::config_dir(o.config_dir.as_ref())?;
    report.info("config folder", config.display());
    let key = admit::device_key(&config)?;
    report.info("this device", key.id());
    let path = frames_net::trust::TrustList::path_in(&config);
    match frames_net::trust::TrustList::load(&path) {
        Ok(list) => report.info(
            "trusted devices",
            format!(
                "{} ({} from v1)",
                list.devices().count(),
                list.devices()
                    .filter(|(_, d)| d.legacy_fingerprint.is_some())
                    .count()
            ),
        ),
        Err(error) => report.line(
            false,
            "trusted devices",
            format!("{} cannot be read: {error:#}", path.display()),
        ),
    }
    report.info(
        "v1 trust list",
        if config.join("trusted.json").exists() {
            "present (trusted v1 devices can migrate with --migrate-v1)"
        } else {
            "none"
        },
    );

    #[cfg(target_os = "macos")]
    macos(&mut report);
    #[cfg(windows)]
    windows(&mut report, &o);

    println!("network");
    let started = Instant::now();
    let node = frames_net::Node::bind(&key, crate::node_config(&o)?).await?;
    for socket in node.bound_sockets() {
        report.info("listening on", socket);
    }
    match node.online(Duration::from_secs(8)).await {
        Ok(()) => {
            let addr = node.addr();
            let relays: Vec<String> = addr.relay_urls().map(ToString::to_string).collect();
            report.line(
                true,
                "relay",
                format!(
                    "{} reachable after {} ms (internet viewers can connect)",
                    relays.join(" "),
                    started.elapsed().as_millis()
                ),
            );
            let direct: Vec<String> = addr.ip_addrs().map(ToString::to_string).collect();
            report.info(
                "direct addresses",
                if direct.is_empty() {
                    "none found".to_string()
                } else {
                    direct.join(" ")
                },
            );
        }
        Err(_) if o.direct_only => report.info("relay", "off (--direct-only)"),
        Err(_) => report.line(
            false,
            "relay",
            "none reachable within 8 s: only viewers on this network can connect (check the internet connection, or a firewall blocking HTTPS)",
        ),
    }
    match frames_net::local::browse(Duration::from_secs(2)).await {
        Ok(devices) => {
            let others: Vec<String> = devices
                .iter()
                .filter(|d| d.id != node.id())
                .map(|d| format!("{} ({})", d.name, &d.id.to_string()[..8]))
                .collect();
            report.line(
                true,
                "nearby hosts",
                if others.is_empty() {
                    "none sharing right now".to_string()
                } else {
                    others.join(", ")
                },
            );
        }
        Err(error) => report.line(
            false,
            "nearby hosts",
            format!("local discovery unavailable: {error:#}"),
        ),
    }

    println!(
        "{}",
        match report.problems {
            0 => "no problems found".to_string(),
            1 => "1 thing to fix".to_string(),
            n => format!("{n} things to fix"),
        }
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos(report: &mut Report) {
    use frames_media::api::{EncodeParams, Gop};
    use frames_platform_macos::{decoder::MacDecoder, encoder::MacEncoder, input, system};

    println!("this Mac");
    report.line(
        system::abi_compatible(),
        "native library",
        if system::abi_compatible() {
            "matches this build".to_string()
        } else {
            format!(
                "version {} does not match this build: rebuild the app",
                system::abi_version()
            )
        },
    );
    report.line(
        system::screen_capture_permitted(),
        "Screen Recording",
        if system::screen_capture_permitted() {
            "allowed (hosting shows the screen)"
        } else {
            "not allowed: hosting sends a generated test picture. Allow it in System Settings > Privacy & Security > Screen Recording (a binary started from a terminal uses the terminal's permission)"
        },
    );
    let accessibility = input::MacInjector::trusted();
    report.line(
        accessibility,
        "Accessibility",
        if accessibility {
            "allowed (viewers trusted for control can use the mouse and keyboard)"
        } else {
            "not allowed: viewers' input is ignored. Allow it in System Settings > Privacy & Security > Accessibility"
        },
    );
    for codec in [crate::Codec::Hevc, crate::Codec::H264] {
        let params = EncodeParams {
            width: 1280,
            height: 720,
            fps: 60,
            bitrate_bps: 10_000_000,
            vbv_frames: 1.0,
            gop: Gop::Infinite,
            intra_refresh_frames: None,
            slices: 1,
        };
        let encode = MacEncoder::new(codec.into(), params).map(drop);
        let decode = MacDecoder::new(codec.into()).map(drop);
        report.line(
            encode.is_ok() && decode.is_ok(),
            &format!("{codec:?}"),
            match (&encode, &decode) {
                (Ok(()), Ok(())) => "VideoToolbox encodes and decodes".to_string(),
                (Err(error), _) => format!("encoder unavailable: {error}"),
                (_, Err(error)) => format!("decoder unavailable: {error}"),
            },
        );
    }
}

/// Not run on Windows yet: the checks there only report what was chosen.
#[cfg(windows)]
fn windows(report: &mut Report, o: &Options) {
    println!("this PC");
    report.info("encoder", format!("{} (change with --encoder)", o.encoder));
    report.info("shareable windows", crate::win::shareable_windows().len());
}
