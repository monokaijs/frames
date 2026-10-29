use crate::model::WindowId;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const HEADER_BYTES: usize = 34;
pub const AUDIO_WINDOW: WindowId = WindowId {
    id: 0,
    generation: 1,
};
const MAX_IN_FLIGHT: usize = 12;
const MAX_REASSEMBLY_BYTES: usize = 24 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}
impl RawFrame {
    pub fn validate(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= 8192
            && self.height <= 8192
            && self.bgra.len() as u64 == self.width as u64 * self.height as u64 * 4
    }
}

#[derive(Clone, Debug)]
pub struct EncodedFrame {
    pub window: WindowId,
    pub generation: u32,
    pub number: u64,
    pub keyframe: bool,
    pub bytes: Vec<u8>,
}

pub fn fragment(frame: &EncodedFrame, max_datagram: usize) -> anyhow::Result<Vec<Vec<u8>>> {
    anyhow::ensure!(
        !frame.bytes.is_empty() && frame.bytes.len() <= MAX_FRAME_BYTES,
        "invalid frame size"
    );
    anyhow::ensure!(max_datagram > HEADER_BYTES, "datagram MTU too small");
    let chunk = max_datagram - HEADER_BYTES;
    let count = frame.bytes.len().div_ceil(chunk);
    anyhow::ensure!(count <= u16::MAX as usize, "too many fragments");
    Ok(frame
        .bytes
        .chunks(chunk)
        .enumerate()
        .map(|(index, data)| {
            let mut packet = Vec::with_capacity(HEADER_BYTES + data.len());
            packet.extend_from_slice(b"FR");
            packet.extend_from_slice(&frame.window.id.to_be_bytes());
            packet.extend_from_slice(&frame.window.generation.to_be_bytes());
            packet.extend_from_slice(&frame.generation.to_be_bytes());
            packet.extend_from_slice(&frame.number.to_be_bytes());
            packet.extend_from_slice(&(index as u16).to_be_bytes());
            packet.extend_from_slice(&(count as u16).to_be_bytes());
            packet.push(u8::from(frame.keyframe));
            packet.extend_from_slice(&[0; 3]);
            packet.extend_from_slice(data);
            packet
        })
        .collect())
}

struct Partial {
    created: Instant,
    chunks: Vec<Option<Vec<u8>>>,
    bytes: usize,
    keyframe: bool,
}
#[derive(Default)]
pub struct Reassembler {
    frames: HashMap<(WindowId, u32, u64), Partial>,
    total_bytes: usize,
}
impl Reassembler {
    pub fn push(&mut self, packet: &[u8], now: Instant) -> Option<EncodedFrame> {
        self.expire(now);
        if packet.len() <= HEADER_BYTES || &packet[..2] != b"FR" {
            return None;
        }
        let window = WindowId {
            id: u64::from_be_bytes(packet[2..10].try_into().ok()?),
            generation: u32::from_be_bytes(packet[10..14].try_into().ok()?),
        };
        let generation = u32::from_be_bytes(packet[14..18].try_into().ok()?);
        let number = u64::from_be_bytes(packet[18..26].try_into().ok()?);
        let index = u16::from_be_bytes(packet[26..28].try_into().ok()?) as usize;
        let count = u16::from_be_bytes(packet[28..30].try_into().ok()?) as usize;
        if count == 0 || count > 8192 || index >= count {
            return None;
        }
        let key = (window, generation, number);
        if !self.frames.contains_key(&key) && self.frames.len() >= MAX_IN_FLIGHT {
            return None;
        }
        if self.total_bytes + packet.len() - HEADER_BYTES > MAX_REASSEMBLY_BYTES {
            return None;
        }
        let frame = self.frames.entry(key).or_insert_with(|| Partial {
            created: now,
            chunks: vec![None; count],
            bytes: 0,
            keyframe: packet[30] != 0,
        });
        if frame.chunks.len() != count || frame.chunks[index].is_some() {
            return None;
        }
        let bytes = &packet[HEADER_BYTES..];
        if frame.bytes + bytes.len() > MAX_FRAME_BYTES {
            return None;
        }
        frame.bytes += bytes.len();
        self.total_bytes += bytes.len();
        frame.chunks[index] = Some(bytes.to_vec());
        if !frame.chunks.iter().all(Option::is_some) {
            return None;
        }
        let partial = self.frames.remove(&key)?;
        self.total_bytes -= partial.bytes;
        Some(EncodedFrame {
            window,
            generation,
            number,
            keyframe: partial.keyframe,
            bytes: partial.chunks.into_iter().flatten().flatten().collect(),
        })
    }
    pub fn expire(&mut self, now: Instant) -> usize {
        let before = self.frames.len();
        self.frames
            .retain(|_, p| now.saturating_duration_since(p.created) < Duration::from_millis(150));
        self.total_bytes = self.frames.values().map(|p| p.bytes).sum();
        before - self.frames.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Metrics {
    pub captured: u64,
    pub encoded: u64,
    pub decoded: u64,
    pub dropped: u64,
    pub bytes_sent: u64,
    pub encode_micros: u64,
    pub cpu_readback: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame() -> EncodedFrame {
        EncodedFrame {
            window: WindowId {
                id: 9,
                generation: 3,
            },
            generation: 2,
            number: 4,
            keyframe: true,
            bytes: (0..10000).map(|n| (n % 255) as u8).collect(),
        }
    }
    #[test]
    fn reordered_datagrams_reassemble_and_duplicates_are_safe() {
        let f = frame();
        let mut packets = fragment(&f, 1200).unwrap();
        packets.reverse();
        let mut r = Reassembler::default();
        let now = Instant::now();
        assert!(r.push(&packets[0], now).is_none());
        assert!(r.push(&packets[0], now).is_none());
        let result = packets.iter().skip(1).find_map(|p| r.push(p, now)).unwrap();
        assert_eq!(result.bytes, f.bytes);
        assert_eq!(result.window, f.window);
    }
    #[test]
    fn incomplete_frames_expire() {
        let mut r = Reassembler::default();
        let now = Instant::now();
        r.push(&fragment(&frame(), 1200).unwrap()[0], now);
        assert_eq!(r.expire(now + Duration::from_millis(151)), 1);
        assert_eq!(r.total_bytes, 0);
    }
    #[test]
    fn hostile_lengths_do_not_allocate_unboundedly() {
        let mut p = fragment(&frame(), 1200).unwrap().remove(0);
        p[28..30].copy_from_slice(&u16::MAX.to_be_bytes());
        let mut r = Reassembler::default();
        assert!(r.push(&p, Instant::now()).is_none());
        assert_eq!(r.total_bytes, 0);
    }
}
