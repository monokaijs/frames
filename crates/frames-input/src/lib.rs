//! The v2 input lane (docs/V2_PLAN.md, Phase 3).
//!
//! Pointer motion travels as QUIC datagrams: each carries the absolute
//! position, so a lost one is replaced by the next and the newest always wins.
//! Buttons, keys, and wheel steps must all arrive, in order, so they travel on
//! a reliable stream as length-prefixed records.
//!
//! Positions are normalized to the shared surface: `0..=65535` maps to the
//! left/top through the right/bottom edge, so the viewer's window size and the
//! host's capture size never need to agree.
//!
//! # Datagram (client → host)
//!
//! | off | size | field |
//! |----:|-----:|-------|
//! | 0 | 1 | [`DATAGRAM_TAG`] (never a media wire version byte) |
//! | 1 | 4 | sequence, increasing; older datagrams are dropped |
//! | 5 | 4 | client event time, µs (wraps) |
//! | 9 | 2 | x |
//! | 11 | 2 | y |
//!
//! # Relative motion datagram (client → host, games)
//!
//! While the viewer has relative mouse on ([`Event::Mode`]), motion goes as
//! [`Motion`] datagrams instead: each carries the running total of motion
//! since relative mode began, so the host applies the difference from the
//! last total it saw. A lost or reordered datagram costs no motion, only
//! the delay until the next one.
//!
//! | off | size | field |
//! |----:|-----:|-------|
//! | 0 | 1 | [`MOTION_TAG`] |
//! | 1 | 4 | sequence, increasing; older datagrams are dropped |
//! | 5 | 4 | client event time, µs (wraps) |
//! | 9 | 4 | total x motion, i32, wraps |
//! | 13 | 4 | total y motion (positive is down), i32, wraps |
//!
//! # Stream record
//!
//! One byte length `n`, then `n` bytes: a kind byte and its fields (see
//! [`Event::encode`]). Unknown kinds are skipped by length, so new kinds can
//! be added without breaking older hosts.

pub mod cursor;
pub mod keys;

/// First byte of an input datagram. Media packets start with
/// `VERSION << 4 | kind` (`0x2_`), so the two never collide.
pub const DATAGRAM_TAG: u8 = 0x01;
const DATAGRAM_LEN: usize = 13;

/// First byte of a [`Motion`] datagram (0x02 is host audio).
pub const MOTION_TAG: u8 = 0x03;
const MOTION_LEN: usize = 17;

/// Relative mouse motion as running totals (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Motion {
    pub seq: u32,
    pub time_us: u32,
    pub total_x: i32,
    pub total_y: i32,
}

impl Motion {
    pub fn encode(&self) -> [u8; MOTION_LEN] {
        let mut out = [0u8; MOTION_LEN];
        out[0] = MOTION_TAG;
        out[1..5].copy_from_slice(&self.seq.to_le_bytes());
        out[5..9].copy_from_slice(&self.time_us.to_le_bytes());
        out[9..13].copy_from_slice(&self.total_x.to_le_bytes());
        out[13..17].copy_from_slice(&self.total_y.to_le_bytes());
        out
    }

    /// `None` unless `datagram` is exactly a motion datagram.
    pub fn decode(datagram: &[u8]) -> Option<Self> {
        if datagram.len() != MOTION_LEN || datagram[0] != MOTION_TAG {
            return None;
        }
        let at = |i: usize| <[u8; 4]>::try_from(&datagram[i..i + 4]).unwrap();
        Some(Self {
            seq: u32::from_le_bytes(at(1)),
            time_us: u32::from_le_bytes(at(5)),
            total_x: i32::from_le_bytes(at(9)),
            total_y: i32::from_le_bytes(at(13)),
        })
    }
}

/// Viewer: accumulates motion and numbers the datagrams.
#[derive(Debug, Default)]
pub struct MotionSender {
    seq: u32,
    total: (i32, i32),
}

impl MotionSender {
    /// Relative mode starts again: totals count from zero (the host resets
    /// its baseline on the same [`Event::Mode`]). Sequence numbers keep
    /// increasing, so datagrams from an earlier period are dropped.
    pub fn restart(&mut self) {
        self.total = (0, 0);
    }

    pub fn add(&mut self, dx: i32, dy: i32, time_us: u32) -> Motion {
        self.seq = self.seq.wrapping_add(1);
        self.total = (self.total.0.wrapping_add(dx), self.total.1.wrapping_add(dy));
        Motion {
            seq: self.seq,
            time_us,
            total_x: self.total.0,
            total_y: self.total.1,
        }
    }
}

/// Host: turns motion datagrams back into deltas, applying each bit of
/// motion once whatever was lost or reordered.
#[derive(Debug, Default)]
pub struct MotionReceiver {
    last_seq: Option<u32>,
    applied: (i32, i32),
}

impl MotionReceiver {
    /// The viewer turned relative mode on: its totals restart from zero.
    pub fn restart(&mut self) {
        self.applied = (0, 0);
    }

    /// The motion not yet applied, or `None` for an old or duplicate datagram.
    pub fn accept(&mut self, motion: &Motion) -> Option<(i32, i32)> {
        let newer = self
            .last_seq
            .is_none_or(|last| (motion.seq.wrapping_sub(last) as i32) > 0);
        if !newer {
            return None;
        }
        self.last_seq = Some(motion.seq);
        let delta = (
            motion.total_x.wrapping_sub(self.applied.0),
            motion.total_y.wrapping_sub(self.applied.1),
        );
        self.applied = (motion.total_x, motion.total_y);
        Some(delta)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pointer {
    pub seq: u32,
    pub time_us: u32,
    pub x: u16,
    pub y: u16,
}

impl Pointer {
    pub fn encode(&self) -> [u8; DATAGRAM_LEN] {
        let mut out = [0u8; DATAGRAM_LEN];
        out[0] = DATAGRAM_TAG;
        out[1..5].copy_from_slice(&self.seq.to_le_bytes());
        out[5..9].copy_from_slice(&self.time_us.to_le_bytes());
        out[9..11].copy_from_slice(&self.x.to_le_bytes());
        out[11..13].copy_from_slice(&self.y.to_le_bytes());
        out
    }

    /// `None` unless `datagram` is exactly an input datagram.
    pub fn decode(datagram: &[u8]) -> Option<Self> {
        if datagram.len() != DATAGRAM_LEN || datagram[0] != DATAGRAM_TAG {
            return None;
        }
        let u32_at = |i: usize| u32::from_le_bytes(datagram[i..i + 4].try_into().unwrap());
        let u16_at = |i: usize| u16::from_le_bytes(datagram[i..i + 2].try_into().unwrap());
        Some(Self {
            seq: u32_at(1),
            time_us: u32_at(5),
            x: u16_at(9),
            y: u16_at(11),
        })
    }
}

/// Keeps only pointer datagrams newer than the last one applied.
#[derive(Debug, Default)]
pub struct PointerFilter {
    last: Option<u32>,
}

impl PointerFilter {
    /// True when `pointer` should be applied. Sequence numbers compare
    /// modulo 2^32, so wrapping is handled.
    pub fn accept(&mut self, pointer: &Pointer) -> bool {
        let newer = self
            .last
            .is_none_or(|last| (pointer.seq.wrapping_sub(last) as i32) > 0);
        if newer {
            self.last = Some(pointer.seq);
        }
        newer
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl Button {
    fn code(self) -> u8 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Middle => 2,
            Self::Back => 3,
            Self::Forward => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Left,
            1 => Self::Right,
            2 => Self::Middle,
            3 => Self::Back,
            4 => Self::Forward,
            _ => return None,
        })
    }
}

/// Reliable input events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A button changed at the given normalized position (sent with the
    /// button so a click lands where it was made, even if pointer datagrams
    /// were lost).
    Button {
        button: Button,
        down: bool,
        x: u16,
        y: u16,
    },
    /// Wheel movement in 1/120 notches (Windows `WHEEL_DELTA` units).
    Wheel { dx: i16, dy: i16 },
    /// A key by USB HID usage (page 7), which every platform can map.
    Key { usage: u16, down: bool },
    /// The viewer lost focus: the host releases every held key and button.
    ReleaseAll,
    /// Relative mouse (games) turned on or off. While on, motion arrives as
    /// [`Motion`] datagrams and button presses do not move the pointer.
    Mode { relative: bool },
    /// The viewer closed its window of a shared application window: the
    /// host asks that window to close (as its close button would).
    CloseWindow,
    /// The most the viewer wants to receive now, in Mbps (its image
    /// quality, changed during the session).
    MaxBitrate { mbps: u16 },
    /// The viewer chose an item of the shared window's application menu,
    /// by the id the host gave it.
    Menu { id: u32 },
}

const KIND_BUTTON: u8 = 1;
const KIND_WHEEL: u8 = 2;
const KIND_KEY: u8 = 3;
const KIND_RELEASE_ALL: u8 = 4;
const KIND_MODE: u8 = 5;
const KIND_CLOSE_WINDOW: u8 = 6;
const KIND_MAX_BITRATE: u8 = 7;
const KIND_MENU: u8 = 8;

impl Event {
    /// Appends the length-prefixed record to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.push(0);
        match *self {
            Self::Button { button, down, x, y } => {
                out.extend([KIND_BUTTON, button.code(), u8::from(down)]);
                out.extend(x.to_le_bytes());
                out.extend(y.to_le_bytes());
            }
            Self::Wheel { dx, dy } => {
                out.push(KIND_WHEEL);
                out.extend(dx.to_le_bytes());
                out.extend(dy.to_le_bytes());
            }
            Self::Key { usage, down } => {
                out.push(KIND_KEY);
                out.extend(usage.to_le_bytes());
                out.push(u8::from(down));
            }
            Self::ReleaseAll => out.push(KIND_RELEASE_ALL),
            Self::Mode { relative } => out.extend([KIND_MODE, u8::from(relative)]),
            Self::CloseWindow => out.push(KIND_CLOSE_WINDOW),
            Self::MaxBitrate { mbps } => {
                out.push(KIND_MAX_BITRATE);
                out.extend(mbps.to_le_bytes());
            }
            Self::Menu { id } => {
                out.push(KIND_MENU);
                out.extend(id.to_le_bytes());
            }
        }
        out[start] = (out.len() - start - 1) as u8;
    }
}

/// Splits a byte stream into events. Feed bytes as they arrive; incomplete
/// records wait for more, unknown or malformed records are skipped.
#[derive(Debug, Default)]
pub struct EventReader {
    buffer: Vec<u8>,
}

impl EventReader {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        let mut at = 0;
        while let Some(&length) = self.buffer.get(at) {
            let end = at + 1 + usize::from(length);
            if end > self.buffer.len() {
                break;
            }
            if let Some(event) = decode_record(&self.buffer[at + 1..end]) {
                events.push(event);
            }
            at = end;
        }
        self.buffer.drain(..at);
        events
    }
}

fn decode_record(record: &[u8]) -> Option<Event> {
    let (&kind, body) = record.split_first()?;
    let u16_at = |i: usize| Some(u16::from_le_bytes(body.get(i..i + 2)?.try_into().ok()?));
    let flag = |i: usize| match body.get(i) {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    };
    let event = match kind {
        KIND_BUTTON if body.len() == 6 => Event::Button {
            button: Button::from_code(body[0])?,
            down: flag(1)?,
            x: u16_at(2)?,
            y: u16_at(4)?,
        },
        KIND_WHEEL if body.len() == 4 => Event::Wheel {
            dx: u16_at(0)? as i16,
            dy: u16_at(2)? as i16,
        },
        KIND_KEY if body.len() == 3 => Event::Key {
            usage: u16_at(0)?,
            down: flag(2)?,
        },
        KIND_RELEASE_ALL if body.is_empty() => Event::ReleaseAll,
        KIND_MODE if body.len() == 1 => Event::Mode { relative: flag(0)? },
        KIND_CLOSE_WINDOW if body.is_empty() => Event::CloseWindow,
        KIND_MAX_BITRATE if body.len() == 2 => Event::MaxBitrate { mbps: u16_at(0)? },
        KIND_MENU if body.len() == 4 => Event::Menu {
            id: u32::from_le_bytes(body.try_into().ok()?),
        },
        _ => return None,
    };
    Some(event)
}

/// What the host currently holds down, so it can release everything when the
/// viewer loses focus or leaves (no stuck keys or buttons).
#[derive(Debug, Default)]
pub struct Held {
    keys: Vec<u16>,
    buttons: Vec<Button>,
}

impl Held {
    pub fn track(&mut self, event: &Event) {
        match *event {
            Event::Key { usage, down } => {
                self.keys.retain(|&k| k != usage);
                if down {
                    self.keys.push(usage);
                }
            }
            Event::Button { button, down, .. } => {
                self.buttons.retain(|&b| b != button);
                if down {
                    self.buttons.push(button);
                }
            }
            Event::Wheel { .. }
            | Event::ReleaseAll
            | Event::Mode { .. }
            | Event::CloseWindow
            | Event::MaxBitrate { .. }
            | Event::Menu { .. } => {}
        }
    }

    /// Release events for everything held, newest first; clears the state.
    pub fn release_all(&mut self, x: u16, y: u16) -> Vec<Event> {
        let mut out: Vec<Event> = self
            .keys
            .drain(..)
            .rev()
            .map(|usage| Event::Key { usage, down: false })
            .collect();
        out.extend(self.buttons.drain(..).rev().map(|button| Event::Button {
            button,
            down: false,
            x,
            y,
        }));
        out
    }
}

/// Normalizes a pixel coordinate inside `0..extent` to `0..=65535`.
pub fn normalize(pixel: i32, extent: u32) -> u16 {
    if extent <= 1 {
        return 0;
    }
    let clamped = pixel.clamp(0, extent as i32 - 1) as u64;
    (clamped * 65535 / (u64::from(extent) - 1)) as u16
}

/// Maps a normalized coordinate back onto `0..extent` pixels.
pub fn denormalize(value: u16, extent: u32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    ((u64::from(value) * (u64::from(extent) - 1) + 32767) / 65535) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_datagrams_round_trip_and_reject_media_packets() {
        let pointer = Pointer {
            seq: 7,
            time_us: 123_456,
            x: 65535,
            y: 1,
        };
        assert_eq!(Pointer::decode(&pointer.encode()), Some(pointer));
        let mut media = pointer.encode();
        media[0] = 0x21;
        assert_eq!(Pointer::decode(&media), None);
        assert_eq!(Pointer::decode(&pointer.encode()[..12]), None);
    }

    #[test]
    fn motion_datagrams_round_trip_and_stay_apart_from_other_datagrams() {
        let motion = Motion {
            seq: 9,
            time_us: 77,
            total_x: -5,
            total_y: i32::MAX,
        };
        assert_eq!(Motion::decode(&motion.encode()), Some(motion));
        assert_eq!(Pointer::decode(&motion.encode()), None);
        let pointer = Pointer {
            seq: 1,
            time_us: 0,
            x: 0,
            y: 0,
        };
        assert_eq!(Motion::decode(&pointer.encode()), None);
        assert_eq!(Motion::decode(&motion.encode()[..16]), None);
    }

    #[test]
    fn motion_is_applied_once_despite_loss_and_reordering() {
        let mut sender = MotionSender::default();
        let mut receiver = MotionReceiver::default();
        let sent: Vec<Motion> = [(3, 1), (4, -2), (-1, 0), (10, 10), (2, 2)]
            .iter()
            .map(|&(dx, dy)| sender.add(dx, dy, 0))
            .collect();
        // Lose the second, deliver the fourth before the third, repeat one.
        let mut applied = (0, 0);
        for motion in [sent[0], sent[3], sent[2], sent[3], sent[4]] {
            if let Some((dx, dy)) = receiver.accept(&motion) {
                applied = (applied.0 + dx, applied.1 + dy);
            }
        }
        assert_eq!(applied, (18, 11), "every bit of motion, once");

        // A new relative period counts from zero on both ends; a straggler
        // from the old one is dropped by its sequence number.
        sender.restart();
        receiver.restart();
        let straggler = sent[1];
        let next = sender.add(5, -5, 0);
        assert_eq!(receiver.accept(&next), Some((5, -5)));
        assert_eq!(receiver.accept(&straggler), None);
    }

    #[test]
    fn motion_totals_wrap() {
        let mut receiver = MotionReceiver::default();
        let at = |seq, total_x| Motion {
            seq,
            time_us: 0,
            total_x,
            total_y: 0,
        };
        receiver.accept(&at(1, i32::MAX - 1));
        assert_eq!(receiver.accept(&at(2, i32::MIN + 2)), Some((4, 0)));
    }

    #[test]
    fn mode_records_round_trip() {
        let mut bytes = Vec::new();
        Event::Mode { relative: true }.encode(&mut bytes);
        Event::Mode { relative: false }.encode(&mut bytes);
        Event::CloseWindow.encode(&mut bytes);
        Event::MaxBitrate { mbps: 12 }.encode(&mut bytes);
        Event::Menu { id: 70_000 }.encode(&mut bytes);
        assert_eq!(
            EventReader::default().push(&bytes),
            vec![
                Event::Mode { relative: true },
                Event::Mode { relative: false },
                Event::CloseWindow,
                Event::MaxBitrate { mbps: 12 },
                Event::Menu { id: 70_000 }
            ]
        );
    }

    #[test]
    fn only_newer_pointer_positions_apply_across_wraparound() {
        let at = |seq| Pointer {
            seq,
            time_us: 0,
            x: 0,
            y: 0,
        };
        let mut filter = PointerFilter::default();
        assert!(filter.accept(&at(u32::MAX - 1)));
        assert!(
            !filter.accept(&at(u32::MAX - 2)),
            "reordered older datagram"
        );
        assert!(filter.accept(&at(1)), "wrapped past zero");
        assert!(!filter.accept(&at(1)), "duplicate");
    }

    #[test]
    fn events_survive_arbitrary_splits_and_unknown_kinds_are_skipped() {
        let events = [
            Event::Button {
                button: Button::Right,
                down: true,
                x: 10,
                y: 65000,
            },
            Event::Wheel { dx: -120, dy: 240 },
            Event::Key {
                usage: 0x04,
                down: false,
            },
            Event::ReleaseAll,
        ];
        let mut bytes = Vec::new();
        events[0].encode(&mut bytes);
        bytes.extend([3, 99, 1, 2]); // a future kind
        for event in &events[1..] {
            event.encode(&mut bytes);
        }
        for split in 0..bytes.len() {
            let mut reader = EventReader::default();
            let mut got = reader.push(&bytes[..split]);
            got.extend(reader.push(&bytes[split..]));
            assert_eq!(got, events, "split at {split}");
        }
    }

    #[test]
    fn malformed_records_never_panic() {
        // Deterministic pseudo-random bytes.
        let mut state = 0x1234_5678u32;
        let mut reader = EventReader::default();
        for _ in 0..20_000 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let _ = reader.push(&state.to_le_bytes());
            let _ = Pointer::decode(&state.to_le_bytes());
        }
    }

    #[test]
    fn release_all_lets_go_of_everything_held_once() {
        let mut held = Held::default();
        for event in [
            Event::Key {
                usage: 0xe1,
                down: true,
            },
            Event::Key {
                usage: 0x04,
                down: true,
            },
            Event::Key {
                usage: 0x04,
                down: true,
            },
            Event::Button {
                button: Button::Left,
                down: true,
                x: 0,
                y: 0,
            },
            Event::Key {
                usage: 0xe1,
                down: false,
            },
        ] {
            held.track(&event);
        }
        assert_eq!(
            held.release_all(9, 9),
            [
                Event::Key {
                    usage: 0x04,
                    down: false
                },
                Event::Button {
                    button: Button::Left,
                    down: false,
                    x: 9,
                    y: 9
                },
            ]
        );
        assert!(held.release_all(0, 0).is_empty());
    }

    #[test]
    fn normalization_hits_both_edges_and_round_trips() {
        assert_eq!(normalize(0, 1920), 0);
        assert_eq!(normalize(1919, 1920), 65535);
        assert_eq!(normalize(-5, 1920), 0);
        assert_eq!(normalize(5000, 1920), 65535);
        for pixel in [0, 1, 959, 1918, 1919] {
            assert_eq!(denormalize(normalize(pixel, 1920), 1920), pixel);
        }
        assert_eq!(denormalize(normalize(700, 1920), 960), 350);
    }
}
