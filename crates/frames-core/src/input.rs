use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    Pointer {
        x: f64,
        y: f64,
    },
    Button {
        button: u8,
        down: bool,
        x: f64,
        y: f64,
    },
    Scroll {
        dx: f64,
        dy: f64,
        x: f64,
        y: f64,
    },
    Key {
        usage: u16,
        down: bool,
    },
    Text {
        text: String,
    },
}

/// USB HID usages, independent of local scan-code and layout conventions.
pub fn remap_modifier(usage: u16, swap: bool) -> u16 {
    if !swap {
        return usage;
    }
    match usage {
        0xe0 => 0xe3,
        0xe3 => 0xe0,
        0xe4 => 0xe7,
        0xe7 => 0xe4,
        _ => usage,
    }
}

/// Keyboard-page USB usages to Windows Scan Set 1. E0 prefixes occupy the high byte.
/// Numeric mapping: https://learn.microsoft.com/windows/win32/inputdev/about-keyboard-input#scan-codes
pub fn hid_pc_scan(usage: u16) -> Option<u16> {
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

pub fn pc_scan_hid(scan: u16) -> Option<u16> {
    let scan = if scan == 0xe045 { 0x45 } else { scan };
    (4..=0xe7).find(|&usage| hid_pc_scan(usage) == Some(scan))
}

#[derive(Default)]
pub struct PressedInput {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
}
impl PressedInput {
    pub fn track(&mut self, event: &InputEvent) {
        match *event {
            InputEvent::Key { usage, down } => {
                if down {
                    self.keys.insert(usage);
                } else {
                    self.keys.remove(&usage);
                }
            }
            InputEvent::Button { button, down, .. } => {
                if down {
                    self.buttons.insert(button);
                } else {
                    self.buttons.remove(&button);
                }
            }
            _ => {}
        }
    }
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let mut events: Vec<_> = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|usage| InputEvent::Key { usage, down: false })
            .collect();
        events.extend(std::mem::take(&mut self.buttons).into_iter().map(|button| {
            InputEvent::Button {
                button,
                down: false,
                x: 0.0,
                y: 0.0,
            }
        }));
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn physical_mapping_distinguishes_keypad_and_extended_modifiers() {
        assert_eq!(pc_scan_hid(0x10), Some(0x14)); // Physical Q position, regardless of layout.
        assert_eq!(pc_scan_hid(0x1c), Some(0x28));
        assert_eq!(pc_scan_hid(0xe01c), Some(0x58));
        assert_eq!(pc_scan_hid(0xe038), Some(0xe6));
        assert_eq!(hid_pc_scan(0xe4), Some(0xe01d));
        for usage in (4..=0x67).chain(0xe0..=0xe7) {
            if usage == 0x32 {
                continue;
            } // ISO hash shares a Windows scan with backslash.
            if let Some(scan) = hid_pc_scan(usage) {
                assert_eq!(pc_scan_hid(scan), Some(usage));
            }
        }
    }
    #[test]
    fn remapping_preserves_nonmodifiers_and_is_reversible() {
        for key in 0..=255 {
            assert_eq!(remap_modifier(remap_modifier(key, true), true), key);
        }
        assert_eq!(remap_modifier(0x06, true), 0x06);
    }
    #[test]
    fn disconnect_releases_every_held_input_once() {
        let mut input = PressedInput::default();
        input.track(&InputEvent::Key {
            usage: 0xe0,
            down: true,
        });
        input.track(&InputEvent::Key {
            usage: 0x06,
            down: true,
        });
        assert_eq!(input.release_all().len(), 2);
        assert!(input.release_all().is_empty());
    }
}
