//! Turns a session process's stderr log into launcher status and messages.
//!
//! Client sessions print `Frames status: <phase>` lines (see
//! `session::report_phase`) and, on failure, a final `Frames: <error>` line.
//! The launcher reads the end of the log to show live progress in the
//! sessions list and to explain failures with a next step instead of a raw
//! error chain.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const STATUS_PREFIX: &str = "Frames status: ";
const ERROR_PREFIX: &str = "Frames: ";
/// v2 sessions (`frames next`) end with this instead.
const NEXT_ERROR_PREFIX: &str = "Frames Next: ";
/// Phases and the final error are always near the end of the log.
const TAIL_BYTES: u64 = 16 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Connecting,
    /// The local pairing prompt is open: compare the code on both devices.
    Pairing,
    /// Waiting for the other device to approve.
    Waiting,
    Connected,
}

impl Phase {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "connecting" => Some(Self::Connecting),
            "pairing" => Some(Self::Pairing),
            "waiting" => Some(Self::Waiting),
            "connected" => Some(Self::Connected),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Starting => "Starting…",
            Self::Connecting => "Connecting…",
            Self::Pairing => "Confirm the pairing code on both devices",
            Self::Waiting => "Waiting for the other device to approve",
            Self::Connected => "Connected",
        }
    }
}

/// The last few kilobytes of a log, or an empty string if it can't be read.
pub fn read_tail(path: &Path) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let length = file.metadata().map(|m| m.len()).unwrap_or(0);
    if length > TAIL_BYTES && file.seek(SeekFrom::Start(length - TAIL_BYTES)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    let _ = file.take(TAIL_BYTES).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The most recent phase a session reported.
pub fn latest_phase(log: &str) -> Option<Phase> {
    log.lines()
        .rev()
        .find_map(|line| line.strip_prefix(STATUS_PREFIX).and_then(Phase::parse))
}

/// Sessions-list detail, e.g. "Connected · per-window view · host audio".
pub fn session_detail(phase: Phase, mode: &str) -> String {
    match phase {
        Phase::Pairing | Phase::Waiting => phase.label().into(),
        _ if mode.is_empty() => phase.label().into(),
        _ => format!("{} · {mode}", phase.label()),
    }
}

/// A message for a session that exited with an error: what happened and
/// what to do next. `phase` is the last phase the session reached.
pub fn explain_failure(name: &str, log: &str, phase: Phase) -> String {
    let Some(raw) = log
        .lines()
        .rev()
        .find_map(|line| {
            line.strip_prefix(ERROR_PREFIX)
                .or_else(|| line.strip_prefix(NEXT_ERROR_PREFIX))
        })
        .map(str::trim)
    else {
        return format!(
            "{name} closed unexpectedly. Check that Frames is sharing on that device, then connect again."
        );
    };
    let error = raw.to_ascii_lowercase();
    let has = |needle: &str| error.contains(needle);
    if has("timed out") || has("timeout") {
        return if phase == Phase::Connected {
            format!(
                "Lost the connection to {name}. Check the network on both devices, then connect again."
            )
        } else {
            format!("Couldn't reach {name}. {}", reach_hint())
        };
    }
    if has("refused this device") {
        return format!(
            "{name} didn't approve this device. Ask the person there to accept it when Frames shows the pairing code."
        );
    }
    if has("did not start the stream") {
        return format!(
            "{name} didn't answer in time. Make sure someone there accepts the connection, then connect again."
        );
    }
    if has("no single nearby device") || has("not a device id") {
        return format!(
            "Couldn't find {name}. Check the device ID, and that Frames is sharing on that device."
        );
    }
    if has("connection declined") {
        return format!("{name} declined the connection.");
    }
    if has("peer did not approve pairing") {
        return format!(
            "The connection wasn't approved on {name}. Connect again and confirm the same pairing code on both devices."
        );
    }
    if has("incompatible protocol") || has("no application protocol") || has("known protocol") {
        return format!(
            "{name} is running a different version of Frames. Update Frames on both devices, then connect again."
        );
    }
    if has("pairing disabled") {
        return format!(
            "{name} isn't accepting new devices. Start sharing in the Frames app on {name}, then connect again."
        );
    }
    if has("another device is controlling") {
        return format!("{name} is already being controlled by another device.");
    }
    if has("device access revoked") {
        return format!(
            "{name} no longer trusts this device. Connect again and confirm the pairing code."
        );
    }
    if has("host ended the session") {
        return format!("{name} ended the session.");
    }
    if has("no working media codecs") {
        return "No working video decoder was found on this computer. Run \"frames doctor\" in a terminal for details.".into();
    }
    if has("refused") || has("unreachable") {
        return format!("Couldn't reach {name}. {}", reach_hint());
    }
    let raw: String = raw.chars().take(400).collect();
    format!("{name}: {raw}")
}

fn reach_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "Make sure Frames is sharing on that device and both devices are on the same network or VPN. On this Mac, allow Frames under System Settings > Privacy & Security > Local Network."
    } else {
        "Make sure Frames is sharing on that device, both devices are on the same network or VPN, and the firewall allows Frames (UDP port 47990)."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_phase_ignores_noise_and_unknown_phases() {
        let log = "INFO frames: starting\nFrames status: connecting\nwarn: x\nFrames status: waiting\nFrames status: later-version-phase\n";
        assert_eq!(latest_phase(log), Some(Phase::Waiting));
        assert_eq!(latest_phase("nothing here"), None);
        assert_eq!(
            session_detail(Phase::Connected, "per-window view · host audio"),
            "Connected · per-window view · host audio"
        );
        assert_eq!(
            session_detail(Phase::Waiting, "full desktop"),
            "Waiting for the other device to approve"
        );
        assert_eq!(session_detail(Phase::Starting, ""), "Starting…");
    }

    #[test]
    fn v2_session_errors_are_explained() {
        let log = "Frames status: waiting\nFrames Next: the host refused this device (not approved there)\n";
        assert!(explain_failure("Mac", log, Phase::Waiting).starts_with("Mac didn't approve"));
        let log = "Frames Next: not a device id, and no single nearby device starts with it\n";
        assert!(explain_failure("Mac", log, Phase::Connecting).starts_with("Couldn't find Mac."));
    }

    #[test]
    fn failures_get_a_cause_and_a_next_step() {
        let log = |error: &str| format!("Frames status: connecting\nFrames: {error}\n");
        let timeout = explain_failure(
            "Office PC",
            &log("QUIC connection timed out. Check the host address and UDP port"),
            Phase::Connecting,
        );
        assert!(
            timeout.starts_with("Couldn't reach Office PC."),
            "{timeout}"
        );
        assert!(
            explain_failure("Office PC", &log("timed out"), Phase::Connected)
                .starts_with("Lost the connection to Office PC.")
        );
        assert_eq!(
            explain_failure("Office PC", &log("connection declined"), Phase::Pairing),
            "Office PC declined the connection."
        );
        assert!(
            explain_failure(
                "Mac",
                &log("incompatible protocol version"),
                Phase::Connecting
            )
            .contains("different version of Frames")
        );
        assert!(
            explain_failure(
                "Mac",
                &log("new device pairing disabled; start host with --pair"),
                Phase::Connecting
            )
            .contains("isn't accepting new devices")
        );
        assert_eq!(
            explain_failure("Mac", &log("weird: low-level detail"), Phase::Connecting),
            "Mac: weird: low-level detail",
            "unknown errors keep their text but lose the internal prefix"
        );
        assert!(
            explain_failure("Mac", "no error line", Phase::Starting)
                .starts_with("Mac closed unexpectedly.")
        );
    }

    #[test]
    fn read_tail_returns_the_end_of_large_logs() {
        let path =
            std::env::temp_dir().join(format!("frames-launcher-status-{}.log", std::process::id()));
        let mut log = "x".repeat(40_000);
        log.push_str("\nFrames status: connected\n");
        std::fs::write(&path, &log).unwrap();
        let tail = read_tail(&path);
        let _ = std::fs::remove_file(&path);
        assert!(tail.len() <= TAIL_BYTES as usize);
        assert_eq!(latest_phase(&tail), Some(Phase::Connected));
        assert_eq!(read_tail(Path::new("/nonexistent/frames.log")), "");
    }
}
