//! USB HID keyboard usages (page 7), Windows Scan Set 1 codes, and macOS
//! virtual key codes.
//!
//! Keys travel as HID usages so the viewer's and host's keyboard conventions
//! never meet; each platform maps at its edge. Copied from v1
//! (`frames-core::input`), which is deleted at the v2 cutover.

/// Keyboard-page USB usages to Windows Scan Set 1. E0 prefixes occupy the high byte.
/// Numeric mapping: https://learn.microsoft.com/windows/win32/inputdev/about-keyboard-input#scan-codes
pub fn usage_to_scan(usage: u16) -> Option<u16> {
    const STANDARD: &[u16] = &[
        0x1e, 0x30, 0x2e, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1f, 0x14, 0x16, 0x2f, 0x11, 0x2d, 0x15, 0x2c, 0x02, 0x03, 0x04, 0x05,
        0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x1c, 0x01, 0x0e, 0x0f, 0x39, 0x0c, 0x0d, 0x1a, 0x1b,
        0x2b, 0x2b, 0x27, 0x28, 0x29, 0x33, 0x34, 0x35, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40,
        0x41, 0x42, 0x43, 0x44, 0x57, 0x58, 0xe037, 0x46, 0, 0xe052, 0xe047, 0xe049, 0xe053,
        0xe04f, 0xe051, 0xe04d, 0xe04b, 0xe050, 0xe048, 0x45, 0xe035, 0x37, 0x4a, 0x4e, 0xe01c,
        0x4f, 0x50, 0x51, 0x4b, 0x4c, 0x4d, 0x47, 0x48, 0x49, 0x52, 0x53, 0x56, 0xe05d, 0xe05e,
        0x59,
    ];
    let scan = match usage {
        4..=0x67 => STANDARD[(usage - 4) as usize],
        0x68..=0x72 => 0x64 + usage - 0x68,
        0x73 => 0x76,
        0xe0 => 0x1d,
        0xe1 => 0x2a,
        0xe2 => 0x38,
        0xe3 => 0xe05b,
        0xe4 => 0xe01d,
        0xe5 => 0x36,
        0xe6 => 0xe038,
        0xe7 => 0xe05c,
        _ => return None,
    };
    (scan != 0).then_some(scan)
}

pub fn scan_to_usage(scan: u16) -> Option<u16> {
    let scan = if scan == 0xe045 { 0x45 } else { scan };
    (4..=0xe7).find(|&usage| usage_to_scan(usage) == Some(scan))
}

/// Swaps Control and GUI (Windows / Command) keys, left and right, for
/// sessions between a Mac and a PC: Command+C on the Mac is Ctrl+C on the
/// PC, and the reverse. Other keys pass through. (v1's `remap_modifier`.)
pub fn swap_control_and_command(usage: u16) -> u16 {
    match usage {
        0xe0 => 0xe3,
        0xe3 => 0xe0,
        0xe4 => 0xe7,
        0xe7 => 0xe4,
        other => other,
    }
}

/// USB HID usage → macOS virtual key code (`kVK_*`), from v1's
/// `native/macos/Sources/Host.swift`.
const MAC_KEYS: &[(u16, u16)] = &[
    (0x04, 0),
    (0x05, 11),
    (0x06, 8),
    (0x07, 2),
    (0x08, 14),
    (0x09, 3),
    (0x0a, 5),
    (0x0b, 4),
    (0x0c, 34),
    (0x0d, 38),
    (0x0e, 40),
    (0x0f, 37),
    (0x10, 46),
    (0x11, 45),
    (0x12, 31),
    (0x13, 35),
    (0x14, 12),
    (0x15, 15),
    (0x16, 1),
    (0x17, 17),
    (0x18, 32),
    (0x19, 9),
    (0x1a, 13),
    (0x1b, 7),
    (0x1c, 16),
    (0x1d, 6),
    (0x1e, 18),
    (0x1f, 19),
    (0x20, 20),
    (0x21, 21),
    (0x22, 23),
    (0x23, 22),
    (0x24, 26),
    (0x25, 28),
    (0x26, 25),
    (0x27, 29),
    (0x28, 36),
    (0x29, 53),
    (0x2a, 51),
    (0x2b, 48),
    (0x2c, 49),
    (0x2d, 27),
    (0x2e, 24),
    (0x2f, 33),
    (0x30, 30),
    (0x31, 42),
    (0x33, 41),
    (0x34, 39),
    (0x35, 50),
    (0x36, 43),
    (0x37, 47),
    (0x38, 44),
    (0x39, 57),
    (0x3a, 122),
    (0x3b, 120),
    (0x3c, 99),
    (0x3d, 118),
    (0x3e, 96),
    (0x3f, 97),
    (0x40, 98),
    (0x41, 100),
    (0x42, 101),
    (0x43, 109),
    (0x44, 103),
    (0x45, 111),
    (0x49, 114),
    (0x4a, 115),
    (0x4b, 116),
    (0x4c, 117),
    (0x4d, 119),
    (0x4e, 121),
    (0x4f, 124),
    (0x50, 123),
    (0x51, 125),
    (0x52, 126),
    (0xe0, 59),
    (0xe1, 56),
    (0xe2, 58),
    (0xe3, 55),
    (0xe4, 62),
    (0xe5, 60),
    (0xe6, 61),
    (0xe7, 54),
];

/// The macOS virtual key code for a HID usage.
pub fn usage_to_mac(usage: u16) -> Option<u16> {
    MAC_KEYS
        .iter()
        .find(|&&(u, _)| u == usage)
        .map(|&(_, code)| code)
}

/// The HID usage for a macOS virtual key code.
pub fn mac_to_usage(code: u16) -> Option<u16> {
    MAC_KEYS
        .iter()
        .find(|&&(_, c)| c == code)
        .map(|&(usage, _)| usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mapped_usage_round_trips_except_shared_scan_codes() {
        let mut mapped = 0;
        for usage in 4..=0xe7u16 {
            if let Some(scan) = usage_to_scan(usage) {
                mapped += 1;
                let back = scan_to_usage(scan).expect("mapped back");
                // Usage 0x32 (non-US #) shares scan code 0x2b with 0x31.
                assert!(back == usage || (usage, back) == (0x32, 0x31), "{usage:#x}");
            }
        }
        assert!(mapped > 100, "{mapped}");
        assert_eq!(usage_to_scan(0x04), Some(0x1e), "A");
        assert_eq!(usage_to_scan(0x50), Some(0xe04b), "left arrow is extended");
        assert_eq!(scan_to_usage(0xe045), Some(0x53), "Num Lock alias");
    }

    #[test]
    fn control_and_command_swap_both_sides_and_nothing_else() {
        assert_eq!(swap_control_and_command(0xe0), 0xe3, "left Ctrl → left GUI");
        assert_eq!(
            swap_control_and_command(0xe7),
            0xe4,
            "right GUI → right Ctrl"
        );
        assert_eq!(swap_control_and_command(0x04), 0x04, "A");
        assert_eq!(swap_control_and_command(0xe1), 0xe1, "Shift");
        for usage in 0..=0xffu16 {
            assert_eq!(
                swap_control_and_command(swap_control_and_command(usage)),
                usage
            );
        }
    }

    #[test]
    fn mac_key_codes_round_trip_and_agree_with_the_windows_table() {
        assert_eq!(MAC_KEYS.len(), 83, "v1's table");
        for &(usage, code) in MAC_KEYS {
            assert_eq!(mac_to_usage(code), Some(usage), "{usage:#x}");
            assert!(
                usage_to_scan(usage).is_some(),
                "{usage:#x} has no Windows scan code"
            );
        }
        assert_eq!(usage_to_mac(0x04), Some(0), "A is kVK_ANSI_A");
        assert_eq!(usage_to_mac(0xe3), Some(55), "left GUI is Command");
    }
}
