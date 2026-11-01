use crate::model::WindowId;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::{Duration, Instant},
};

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_REASSEMBLY_BYTES: usize = 24 * 1024 * 1024;
pub const DATAGRAM_SEND_BUFFER_BYTES: usize = 16 * 1024;
pub const HEADER_BYTES: usize = 60;
const MAX_IN_FLIGHT: usize = 12;
const MAX_WINDOW_BYTES: usize = 20 * 1024 * 1024;
const DATA_PER_BLOCK: usize = 32;
const SCHEME: u8 = 1; // reed-solomon-simd 3, DefaultRate, even-sized shards.
pub const AUDIO_WINDOW: WindowId = WindowId {
    id: 0,
    generation: 1,
};

/// Ordinary references remain short-lived. A detailed text refresh can exceed
/// 500 KiB; give its protected bytes time to cross the 30 Mbps reference link,
/// with bounded jitter allowance. This is an expiry ceiling, never a playout wait.
pub fn assembly_timeout(keyframe: bool, bytes: usize) -> Duration {
    if !keyframe {
        return Duration::from_millis(25);
    }
    let wire_us = (bytes.min(MAX_FRAME_BYTES) as u64 * 10 * 1_000_000).div_ceil(30_000_000);
    Duration::from_micros((wire_us + 20_000).clamp(50_000, 200_000))
}

/// A small ownership-aware pool. A picture returns its storage only after the
/// last capture/decoder/UI/render reference has gone away.
#[derive(Clone, Default)]
pub struct PixelPool(std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>);
pub struct PixelBuffer {
    bytes: Vec<u8>,
    pool: PixelPool,
}
impl PixelPool {
    pub fn acquire(&self, size: usize) -> PixelBuffer {
        let mut bytes = self.0.lock().unwrap().pop().unwrap_or_default();
        bytes.resize(size, 0);
        PixelBuffer {
            bytes,
            pool: self.clone(),
        }
    }
}
impl std::ops::Deref for PixelBuffer {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.bytes
    }
}
impl std::ops::DerefMut for PixelBuffer {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }
}
impl AsRef<[u8]> for PixelBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
impl PixelBuffer {
    pub fn into_bytes(self) -> bytes::Bytes {
        bytes::Bytes::from_owner(self)
    }
}
impl Drop for PixelBuffer {
    fn drop(&mut self) {
        let mut pool = self.pool.0.lock().unwrap();
        if pool.len() < 3 {
            pool.push(std::mem::take(&mut self.bytes));
        }
    }
}
#[derive(Clone, Debug)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: bytes::Bytes,
    pub captured_at: Instant,
    pub frame_number: u64,
    pub media_generation: u32,
    #[cfg(windows)]
    pub gpu: Option<std::sync::Arc<WindowsFrame>>,
}
impl RawFrame {
    pub fn validate(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= 8192
            && self.height <= 8192
            && (self.bgra.len() as u64 == self.width as u64 * self.height as u64 * 4
                || self.has_gpu())
    }
    /// Diagnostic readback; live GPU presentation never calls this method.
    pub fn cpu_copy(&self) -> anyhow::Result<Self> {
        #[cfg(windows)]
        if let Some(surface) = &self.gpu {
            let mut converter =
                crate::codec::GpuConverter::new(&surface.texture, self.width, self.height)?;
            let mut pixels = vec![0; self.width as usize * self.height as usize * 4];
            converter.readback(surface, self.width, self.height, &mut pixels)?;
            let mut frame = self.clone();
            frame.bgra = pixels.into();
            frame.gpu = None;
            return Ok(frame);
        }
        Ok(self.clone())
    }
    pub fn has_gpu(&self) -> bool {
        #[cfg(windows)]
        {
            self.gpu.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }
}
#[cfg(windows)]
#[derive(Debug)]
pub struct WindowsFrame {
    pub texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    pub subresource: u32,
    pub matrix_709: bool,
    pub full_range: bool,
    pub lease: Option<crate::codec::HardwareLease>,
}
#[derive(Clone, Debug)]
pub struct EncodedFrame {
    pub window: WindowId,
    pub generation: u32,
    pub number: u64,
    pub keyframe: bool,
    pub bytes: Vec<u8>,
    pub captured_us: u64,
    pub encoded_us: u64,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct FecProfile {
    pub parity_per_32: u8,
}
impl Default for FecProfile {
    fn default() -> Self {
        Self { parity_per_32: 4 }
    }
}
impl FecProfile {
    pub fn validate(self) -> bool {
        matches!(self.parity_per_32, 0 | 2 | 4 | 8)
    }
    fn parity(self, data: usize) -> usize {
        (data * self.parity_per_32 as usize).div_ceil(DATA_PER_BLOCK)
    }
}

/// The systematic data is transmitted without waiting for another picture.
/// Block striping and round-robin transmission spread contiguous loss bursts.
pub fn fragment(frame: &EncodedFrame, mtu: usize) -> anyhow::Result<Vec<Vec<u8>>> {
    fragment_fec(frame, mtu, FecProfile::default(), 0)
}
pub fn fragment_fec(
    frame: &EncodedFrame,
    mtu: usize,
    profile: FecProfile,
    captured_us: u64,
) -> anyhow::Result<Vec<Vec<u8>>> {
    anyhow::ensure!(
        !frame.bytes.is_empty() && frame.bytes.len() <= MAX_FRAME_BYTES,
        "invalid frame size"
    );
    anyhow::ensure!(profile.validate(), "invalid FEC profile");
    let shard_size =
        ((mtu.min(1200).saturating_sub(HEADER_BYTES)) & !1).min((frame.bytes.len() + 1) & !1);
    anyhow::ensure!(shard_size >= 2, "datagram MTU too small");
    let count = frame.bytes.len().div_ceil(shard_size);
    anyhow::ensure!(count <= u16::MAX as usize, "too many fragments");
    let block_count = count.div_ceil(DATA_PER_BLOCK);
    let mut blocks = Vec::with_capacity(block_count);
    for block in 0..block_count {
        let data_count = (count - block).div_ceil(block_count);
        let parity = profile.parity(data_count);
        let mut shards = Vec::with_capacity(data_count + parity);
        for index in 0..data_count {
            let offset = (index * block_count + block) * shard_size;
            let len = shard_size.min(frame.bytes.len() - offset);
            let mut shard = vec![0; shard_size];
            shard[..len].copy_from_slice(&frame.bytes[offset..offset + len]);
            shards.push(shard);
        }
        if parity > 0 {
            let parity_shards = FEC_ENCODER.with(|slot| -> anyhow::Result<Vec<Vec<u8>>> {
                let mut slot = slot.borrow_mut();
                if let Some(encoder) = slot.as_mut() {
                    encoder.reset(data_count, parity, shard_size)?;
                } else {
                    *slot = Some(reed_solomon_simd::ReedSolomonEncoder::new(
                        data_count, parity, shard_size,
                    )?);
                }
                let encoder = slot.as_mut().unwrap();
                for shard in &shards {
                    encoder.add_original_shard(shard)?;
                }
                Ok(encoder
                    .encode()?
                    .recovery_iter()
                    .map(<[u8]>::to_vec)
                    .collect())
            })?;
            shards.extend(parity_shards);
        }
        blocks.push((data_count, parity, shards));
    }
    let mut packets = Vec::with_capacity(blocks.iter().map(|(_, _, s)| s.len()).sum());
    let rounds = blocks.iter().map(|(_, _, s)| s.len()).max().unwrap_or(0);
    for index in 0..rounds {
        for (block, (data_count, parity, shards)) in blocks.iter().enumerate() {
            let Some(shard) = shards.get(index) else {
                continue;
            };
            let mut p = Vec::with_capacity(HEADER_BYTES + shard_size);
            p.extend_from_slice(b"F6");
            p.extend_from_slice(&frame.window.id.to_be_bytes());
            p.extend_from_slice(&frame.window.generation.to_be_bytes());
            p.extend_from_slice(&frame.generation.to_be_bytes());
            p.extend_from_slice(&frame.number.to_be_bytes());
            p.extend_from_slice(&captured_us.to_be_bytes());
            p.extend_from_slice(&(frame.bytes.len() as u32).to_be_bytes());
            p.extend_from_slice(&(shard_size as u16).to_be_bytes());
            p.extend_from_slice(&(block as u16).to_be_bytes());
            p.extend_from_slice(&(block_count as u16).to_be_bytes());
            p.push(*data_count as u8);
            p.push(*parity as u8);
            p.push(index as u8);
            p.push(u8::from(frame.keyframe));
            p.extend_from_slice(&(count as u16).to_be_bytes());
            p.push(SCHEME);
            p.push(profile.parity_per_32);
            p.extend_from_slice(&frame.encoded_us.to_be_bytes());
            p.extend_from_slice(shard);
            packets.push(p);
        }
    }
    Ok(packets)
}

pub fn prewarm_fec() {
    static WARM: std::sync::Once = std::sync::Once::new();
    WARM.call_once(|| {
        let original = [[0u8; 64]; 32];
        if let Ok(parity) = reed_solomon_simd::encode(32, 8, original) {
            // Decoder transforms have additional lazy tables. Initialize both
            // paths before the first lost live picture, not only the encoder.
            let _ = reed_solomon_simd::decode(
                32,
                8,
                original.iter().enumerate().skip(1),
                parity.iter().enumerate().take(1),
            );
        }
    });
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    window: WindowId,
    generation: u32,
    number: u64,
    captured_us: u64,
    encoded_us: u64,
    length: usize,
    shard_size: usize,
    block: usize,
    blocks: usize,
    data: usize,
    parity: usize,
    index: usize,
    keyframe: bool,
    count: usize,
    profile: u8,
}
impl Header {
    fn parse(p: &[u8]) -> Option<Self> {
        if p.len() <= HEADER_BYTES
            || &p[..2] != b"F6"
            || p[50] != SCHEME
            || !(FecProfile {
                parity_per_32: p[51],
            })
            .validate()
            || p[47] > 1
        {
            return None;
        }
        let h = Self {
            window: WindowId {
                id: u64::from_be_bytes(p[2..10].try_into().ok()?),
                generation: u32::from_be_bytes(p[10..14].try_into().ok()?),
            },
            generation: u32::from_be_bytes(p[14..18].try_into().ok()?),
            number: u64::from_be_bytes(p[18..26].try_into().ok()?),
            captured_us: u64::from_be_bytes(p[26..34].try_into().ok()?),
            encoded_us: u64::from_be_bytes(p[52..60].try_into().ok()?),
            length: u32::from_be_bytes(p[34..38].try_into().ok()?) as usize,
            shard_size: u16::from_be_bytes(p[38..40].try_into().ok()?) as usize,
            block: u16::from_be_bytes(p[40..42].try_into().ok()?) as usize,
            blocks: u16::from_be_bytes(p[42..44].try_into().ok()?) as usize,
            data: p[44] as usize,
            parity: p[45] as usize,
            index: p[46] as usize,
            keyframe: p[47] == 1,
            count: u16::from_be_bytes(p[48..50].try_into().ok()?) as usize,
            profile: p[51],
        };
        if h.length == 0
            || h.length > MAX_FRAME_BYTES
            || h.shard_size < 2
            || !h.shard_size.is_multiple_of(2)
            || p.len() != HEADER_BYTES + h.shard_size
            || p.len() > 1200
            || h.count != h.length.div_ceil(h.shard_size)
            || h.blocks != h.count.div_ceil(DATA_PER_BLOCK)
            || h.block >= h.blocks
            || h.data != (h.count - h.block).div_ceil(h.blocks)
            || h.data == 0
            || h.data > DATA_PER_BLOCK
            || h.parity != (h.data * h.profile as usize).div_ceil(32)
            || h.index >= h.data + h.parity
        {
            return None;
        }
        Some(h)
    }
    fn same_frame(self, other: Self) -> bool {
        self.window == other.window
            && self.generation == other.generation
            && self.number == other.number
            && self.captured_us == other.captured_us
            && self.encoded_us == other.encoded_us
            && self.length == other.length
            && self.shard_size == other.shard_size
            && self.blocks == other.blocks
            && self.count == other.count
            && self.keyframe == other.keyframe
            && self.profile == other.profile
    }
}
struct Block {
    shards: Vec<Option<Vec<u8>>>,
    received: usize,
    data: usize,
    parity: usize,
    complete: bool,
}
struct Partial {
    header: Header,
    deadline: Instant,
    blocks: Vec<Option<Block>>,
    output: Vec<u8>,
    remaining: usize,
    reserved: usize,
    repaired: bool,
    seen: Vec<u64>,
}
type FrameKey = (WindowId, u32, u64);
struct Retired {
    key: FrameKey,
    header: Header,
    deadline: Instant,
    seen: Vec<u64>,
    finalized: bool,
}
impl Retired {
    fn bytes(&self) -> usize {
        self.seen.len() * 8 + std::mem::size_of::<Self>() + 64
    }
    fn finalize(&mut self, stats: &mut HashMap<WindowId, DeliveryStats>) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        let missing: usize = self
            .seen
            .iter()
            .enumerate()
            .map(|(block, bits)| {
                let data = (self.header.count - block).div_ceil(self.header.blocks);
                let total = data + (data * self.header.profile as usize).div_ceil(32);
                total.saturating_sub((bits & ((1u64 << total) - 1)).count_ones() as usize)
            })
            .sum();
        stats.entry(self.key.0).or_default().missing_shards += missing as u64;
    }
}
thread_local! {
    static FEC_ENCODER: std::cell::RefCell<Option<reed_solomon_simd::ReedSolomonEncoder>> = const { std::cell::RefCell::new(None) };
    static FEC_DECODER: std::cell::RefCell<Option<reed_solomon_simd::ReedSolomonDecoder>> = const { std::cell::RefCell::new(None) };
}
#[derive(Default, Clone, Copy, Debug, Serialize, Deserialize)]
pub struct DeliveryStats {
    pub received_bytes: u64,
    pub received_shards: u64,
    pub missing_shards: u64,
    pub recovered_frames: u64,
    pub unrecoverable_frames: u64,
    pub completed_frames: u64,
    pub fec_micros: u64,
}
#[derive(Default)]
pub struct Reassembler {
    frames: HashMap<FrameKey, Partial>,
    total_bytes: usize,
    external_bytes: usize,
    expired: HashSet<FrameKey>,
    retired: VecDeque<Retired>,
    retired_keys: HashSet<FrameKey>,
    deadline: Option<Instant>,
    stats: HashMap<WindowId, DeliveryStats>,
}
impl Reassembler {
    pub fn identity(packet: &[u8]) -> Option<FrameKey> {
        Header::parse(packet).map(|h| (h.window, h.generation, h.number))
    }
    pub fn keyframe(packet: &[u8]) -> bool {
        Header::parse(packet).is_some_and(|h| h.keyframe)
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.deadline
    }
    pub fn has_keyframe(&self, window: WindowId, generation: u32) -> bool {
        self.frames.values().any(|frame| {
            frame.header.window == window
                && frame.header.generation == generation
                && frame.header.keyframe
        })
    }
    fn refresh_deadline(&mut self) {
        self.deadline = self
            .frames
            .values()
            .map(|p| p.deadline)
            .chain(
                self.retired
                    .iter()
                    .filter(|r| !r.finalized)
                    .map(|r| r.deadline),
            )
            .min();
    }
    pub fn retained_bytes(&self) -> usize {
        self.total_bytes
    }
    pub fn set_external_bytes(&mut self, bytes: usize) {
        self.external_bytes = bytes;
    }
    pub fn stats(&self, window: WindowId) -> DeliveryStats {
        self.stats.get(&window).copied().unwrap_or_default()
    }
    pub fn forget_window(&mut self, window: WindowId) {
        self.frames.retain(|(w, _, _), _| *w != window);
        self.expired.retain(|(w, _, _)| *w != window);
        self.retired.retain(|r| r.key.0 != window);
        self.retired_keys.retain(|(w, _, _)| *w != window);
        self.refresh_deadline();
        self.stats.remove(&window);
        self.total_bytes = self.frames.values().map(|p| p.reserved).sum::<usize>()
            + self.retired.iter().map(Retired::bytes).sum::<usize>();
    }
    pub fn forget_before(&mut self, window: WindowId, generation: u32, number: u64) {
        self.frames
            .retain(|(w, g, n), _| *w != window || *g != generation || *n >= number);
        self.refresh_deadline();
        self.total_bytes = self.frames.values().map(|p| p.reserved).sum::<usize>()
            + self.retired.iter().map(Retired::bytes).sum::<usize>();
    }
    pub fn take_expired(&mut self) -> Vec<FrameKey> {
        self.expired.drain().collect()
    }
    fn retire(&mut self, partial: &mut Partial, key: FrameKey, now: Instant) {
        let mut retired = Retired {
            key,
            header: partial.header,
            deadline: partial.deadline,
            seen: std::mem::take(&mut partial.seen),
            finalized: false,
        };
        if retired.deadline <= now {
            retired.finalize(&mut self.stats);
        }
        self.total_bytes += retired.bytes();
        self.retired_keys.insert(key);
        self.retired.push_back(retired);
        while self.retired.len() > 128 {
            if let Some(mut r) = self.retired.pop_front() {
                self.total_bytes -= r.bytes();
                self.retired_keys.remove(&r.key);
                r.finalize(&mut self.stats);
            }
        }
    }
    pub fn push(&mut self, packet: &[u8], now: Instant) -> Option<EncodedFrame> {
        self.expire(now);
        let h = Header::parse(packet)?;
        let key = (h.window, h.generation, h.number);
        if self.retired_keys.contains(&key) {
            let retired = self.retired.iter_mut().find(|r| r.key == key).unwrap();
            if retired.header.same_frame(h)
                && !retired.finalized
                && retired.seen[h.block] & (1u64 << h.index) == 0
            {
                retired.seen[h.block] |= 1u64 << h.index;
                let stats = self.stats.entry(h.window).or_default();
                stats.received_shards += 1;
                stats.received_bytes += packet.len() as u64;
            }
            return None;
        }
        if !self.frames.contains_key(&key) {
            // Reserve output, all possible shards, metadata and one reconstruction scratch block.
            let reserved = h.length
                + h.count * (h.shard_size + 48)
                + h.blocks * (8 * (h.shard_size + 48) + 512)
                + 80 * h.shard_size
                + 1024;
            let window_bytes: usize = self
                .frames
                .iter()
                .filter(|((w, _, _), _)| *w == h.window)
                .map(|(_, p)| p.reserved)
                .sum();
            if self.frames.len() >= MAX_IN_FLIGHT
                || reserved + window_bytes > MAX_WINDOW_BYTES
                || reserved + self.total_bytes + self.external_bytes > MAX_REASSEMBLY_BYTES
            {
                return None;
            }
            self.frames.insert(
                key,
                Partial {
                    header: h,
                    deadline: now
                        + if h.window == AUDIO_WINDOW {
                            Duration::from_millis(20)
                        } else {
                            assembly_timeout(h.keyframe, h.length)
                        },
                    blocks: (0..h.blocks).map(|_| None).collect(),
                    output: vec![0; h.length],
                    remaining: h.blocks,
                    reserved,
                    repaired: false,
                    seen: vec![0; h.blocks],
                },
            );
            self.total_bytes += reserved;
            let deadline = self.frames[&key].deadline;
            self.deadline = Some(self.deadline.map_or(deadline, |old| old.min(deadline)));
        }
        let frame = self.frames.get_mut(&key)?;
        if !frame.header.same_frame(h) || frame.seen[h.block] & (1u64 << h.index) != 0 {
            return None;
        }
        frame.seen[h.block] |= 1u64 << h.index;
        let stats = self.stats.entry(h.window).or_default();
        stats.received_bytes += packet.len() as u64;
        stats.received_shards += 1;
        let block = frame.blocks[h.block].get_or_insert_with(|| Block {
            shards: vec![None; h.data + h.parity],
            received: 0,
            data: h.data,
            parity: h.parity,
            complete: false,
        });
        if block.data != h.data
            || block.parity != h.parity
            || block.complete
            || block.shards[h.index].is_some()
        {
            return None;
        }
        block.shards[h.index] = Some(packet[HEADER_BYTES..].to_vec());
        block.received += 1;
        if block.received < block.data {
            return None;
        }
        let missing = block.shards[..block.data]
            .iter()
            .filter(|s| s.is_none())
            .count();
        let started = Instant::now();
        if missing > 0 {
            FEC_DECODER.with(|slot| -> Option<()> {
                let mut slot = slot.borrow_mut();
                if let Some(decoder) = slot.as_mut() {
                    decoder.reset(block.data, block.parity, h.shard_size).ok()?;
                } else {
                    *slot = Some(
                        reed_solomon_simd::ReedSolomonDecoder::new(
                            block.data,
                            block.parity,
                            h.shard_size,
                        )
                        .ok()?,
                    );
                }
                let decoder = slot.as_mut()?;
                for (index, shard) in block.shards.iter().enumerate() {
                    if let Some(shard) = shard {
                        if index < block.data {
                            decoder.add_original_shard(index, shard).ok()?;
                        } else {
                            decoder.add_recovery_shard(index - block.data, shard).ok()?;
                        }
                    }
                }
                let decoded = decoder.decode().ok()?;
                for (index, shard) in decoded.restored_original_iter() {
                    block.shards[index] = Some(shard.to_vec());
                }
                Some(())
            })?;
            frame.repaired = true;
        }
        stats.fec_micros += started.elapsed().as_micros() as u64;
        for index in 0..block.data {
            let offset = (index * h.blocks + h.block) * h.shard_size;
            let len = h.shard_size.min(h.length - offset);
            frame.output[offset..offset + len]
                .copy_from_slice(&block.shards[index].as_ref()?[..len]);
        }
        block.shards.clear();
        block.complete = true;
        frame.remaining -= 1;
        if frame.remaining != 0 {
            return None;
        }
        let mut complete = self.frames.remove(&key)?;
        self.total_bytes -= complete.reserved;
        stats.completed_frames += 1;
        stats.recovered_frames += u64::from(complete.repaired);
        self.retire(&mut complete, key, now);
        Some(EncodedFrame {
            captured_us: h.captured_us,
            encoded_us: h.encoded_us,
            window: h.window,
            generation: h.generation,
            number: h.number,
            keyframe: h.keyframe,
            bytes: complete.output,
        })
    }
    pub fn expire(&mut self, now: Instant) -> usize {
        if self.deadline.is_none_or(|at| at > now) {
            return 0;
        }
        for r in &mut self.retired {
            if r.deadline <= now {
                r.finalize(&mut self.stats);
            }
        }
        let keys: Vec<_> = self
            .frames
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(k, _)| *k)
            .collect();
        for key in &keys {
            if let Some(mut frame) = self.frames.remove(key) {
                self.total_bytes -= frame.reserved;
                let stats = self.stats.entry(key.0).or_default();
                stats.unrecoverable_frames += 1;
                if self.expired.len() < MAX_IN_FLIGHT * 4 {
                    self.expired.insert(*key);
                }
                self.retire(&mut frame, *key, now);
            }
        }
        self.refresh_deadline();
        keys.len()
    }
}

/// Fixed 4 KiB histogram, 250 us buckets through 127 ms. The overflow bucket
/// reports the actual maximum, so long stalls are never hidden by saturation.
#[derive(Clone, Debug)]
pub struct LatencyHistogram {
    buckets: [u64; 512],
    count: u64,
    maximum: u64,
}
impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; 512],
            count: 0,
            maximum: 0,
        }
    }
}
impl LatencyHistogram {
    pub fn record(&mut self, micros: u64) {
        self.buckets[micros.div_ceil(250).min(511) as usize] += 1;
        self.count += 1;
        self.maximum = self.maximum.max(micros);
    }
    pub fn percentile(&self, percent: u64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = (self.count * percent.min(100)).div_ceil(100).max(1);
        let mut total = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            total += n;
            if total >= target {
                return if i == 511 {
                    self.maximum
                } else {
                    i as u64 * 250
                };
            }
        }
        self.maximum
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
    fn frame(length: usize) -> EncodedFrame {
        EncodedFrame {
            encoded_us: 0,
            captured_us: 0,
            window: WindowId {
                id: 9,
                generation: 3,
            },
            generation: 2,
            number: 4,
            keyframe: false,
            bytes: (0..length).map(|n| (n % 251) as u8).collect(),
        }
    }
    #[test]
    fn loss_reorder_duplicates_and_mtu_roundtrip() {
        for mtu in [576, 1199, 1200, 1500] {
            for length in [1, 1149, 10000, 100000] {
                let f = frame(length);
                let packets = fragment_fec(&f, mtu, FecProfile { parity_per_32: 8 }, 77).unwrap();
                let mut r = Reassembler::default();
                let now = Instant::now();
                let mut result = None;
                for p in packets.iter().rev().filter(|p| p[46] != 0) {
                    if let Some(decoded) = r.push(p, now) {
                        assert!(result.is_none());
                        result = Some(decoded);
                    }
                    assert!(r.push(p, now).is_none());
                }
                assert_eq!(result.unwrap().bytes, f.bytes);
                assert!(r.total_bytes < 4096);
                assert_eq!(r.stats(f.window).recovered_frames, 1);
            }
        }
    }
    #[test]
    fn every_erasure_combination_within_parity_recovers() {
        let f = frame(3000);
        let packets = fragment_fec(&f, 1200, FecProfile { parity_per_32: 8 }, 0).unwrap();
        for lost in 0..packets.len() {
            let mut r = Reassembler::default();
            let decoded = packets
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != lost)
                .find_map(|(_, p)| r.push(p, Instant::now()))
                .unwrap();
            assert_eq!(decoded.bytes, f.bytes);
        }
    }
    #[test]
    fn expires_on_deadline_and_never_resurrects() {
        let f = frame(10000);
        let packets = fragment(&f, 1200).unwrap();
        let mut r = Reassembler::default();
        let at = Instant::now();
        r.push(&packets[0], at);
        assert_eq!(r.next_deadline(), Some(at + Duration::from_millis(25)));
        assert_eq!(r.expire(at + Duration::from_millis(25)), 1);
        assert!(r.total_bytes < 4096);
        assert_eq!(r.take_expired(), vec![(f.window, f.generation, f.number)]);
        for p in packets {
            assert!(r.push(&p, at + Duration::from_millis(26)).is_none());
        }
    }
    #[test]
    fn detailed_refresh_has_a_bounded_size_aware_expiry_and_completes_immediately() {
        let mut f = frame(512 * 1024);
        f.keyframe = true;
        let packets = fragment_fec(&f, 1200, FecProfile { parity_per_32: 8 }, 0).unwrap();
        let at = Instant::now();
        let mut r = Reassembler::default();
        assert!(r.push(&packets[0], at).is_none());
        assert_eq!(
            r.next_deadline(),
            Some(at + assembly_timeout(true, f.bytes.len()))
        );
        assert!(r.has_keyframe(f.window, f.generation));
        let later = at + Duration::from_millis(100);
        assert_eq!(r.expire(later), 0);
        let result = packets
            .iter()
            .skip(1)
            .find_map(|p| r.push(p, later))
            .unwrap();
        assert_eq!(result.bytes, f.bytes);
        assert!(!r.has_keyframe(f.window, f.generation));
        assert_eq!(
            assembly_timeout(false, MAX_FRAME_BYTES),
            Duration::from_millis(25)
        );
        assert_eq!(
            assembly_timeout(true, MAX_FRAME_BYTES),
            Duration::from_millis(200)
        );
    }
    #[test]
    fn malformed_headers_and_metadata_conflicts_are_bounded() {
        let f = frame(10000);
        let packets = fragment(&f, 1200).unwrap();
        let at = Instant::now();
        for offset in [34, 38, 40, 42, 44, 45, 46, 47, 48, 50, 51] {
            let mut p = packets[0].clone();
            p[offset] = 255;
            let mut r = Reassembler::default();
            assert!(r.push(&p, at).is_none());
            assert_eq!(r.total_bytes, 0, "offset {offset}");
        }
        let mut r = Reassembler::default();
        r.push(&packets[0], at);
        let charged = r.total_bytes;
        let mut conflict = packets[1].clone();
        conflict[26] ^= 1;
        assert!(r.push(&conflict, at).is_none());
        assert_eq!(r.total_bytes, charged);
    }
    #[test]
    fn session_budget_includes_pending_and_reconstruction() {
        let f = frame(100000);
        let packets = fragment(&f, 1200).unwrap();
        let mut r = Reassembler::default();
        r.set_external_bytes(MAX_REASSEMBLY_BYTES);
        assert!(r.push(&packets[0], Instant::now()).is_none());
        assert_eq!(r.total_bytes, 0);
    }
    #[test]
    fn parity_disabled_still_reassembles_and_parity_only_recovers_small_frame() {
        for profile in [FecProfile { parity_per_32: 0 }, FecProfile::default()] {
            let f = frame(9);
            let packets = fragment_fec(&f, 1200, profile, 0).unwrap();
            let mut r = Reassembler::default();
            assert_eq!(
                r.push(packets.last().unwrap(), Instant::now())
                    .unwrap()
                    .bytes,
                f.bytes
            );
        }
    }
}

pub fn monotonic_micros() -> u64 {
    clock_epoch().elapsed().as_micros().min(u64::MAX as u128) as u64
}
fn clock_epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}
pub fn timestamp_micros(at: Instant) -> u64 {
    at.saturating_duration_since(clock_epoch())
        .as_micros()
        .min(u64::MAX as u128) as u64
}
#[derive(Default)]
pub struct ClockEstimator {
    offset: Option<i64>,
    pub uncertainty_us: u32,
}
impl ClockEstimator {
    pub fn observe(&mut self, t0: u64, t1: u64, t2: u64, t3: u64) {
        if t3 < t0 || t2 < t1 || t3 - t0 < t2 - t1 || t3 - t0 > 1_000_000 {
            return;
        }
        let uncertainty = ((t3 - t0 - (t2 - t1)) / 2).min(u32::MAX as u64) as u32;
        let offset = ((t1 as i128 - t0 as i128 + t2 as i128 - t3 as i128) / 2)
            .clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        if self.offset.is_none() || uncertainty <= self.uncertainty_us.saturating_add(1_000) {
            self.offset = Some(offset);
            self.uncertainty_us = uncertainty;
        }
    }
    pub fn age(&self, remote: u64, local: u64) -> Option<Duration> {
        let mapped = remote as i128 - self.offset? as i128;
        let age = local as i128 - mapped;
        if age < -(self.uncertainty_us as i128) {
            return None;
        }
        Some(Duration::from_micros(age.clamp(0, u64::MAX as i128) as u64))
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    #[test]
    fn originals_arriving_after_repair_are_not_counted_as_network_loss() {
        let frame = EncodedFrame {
            encoded_us: 0,
            window: WindowId {
                id: 41,
                generation: 1,
            },
            generation: 1,
            number: 0,
            keyframe: false,
            bytes: vec![7; 100_000],
            captured_us: 99,
        };
        let packets = fragment(&frame, 1200).unwrap();
        let now = Instant::now();
        let mut receiver = Reassembler::default();
        let mut completed = 0;
        for packet in packets.iter().rev() {
            if let Some(decoded) = receiver.push(packet, now) {
                assert_eq!(decoded.bytes, frame.bytes);
                completed += 1;
            }
        }
        receiver.expire(now + Duration::from_millis(26));
        assert_eq!(completed, 1);
        assert_eq!(receiver.stats(frame.window).missing_shards, 0);
        assert_eq!(
            receiver.stats(frame.window).received_shards,
            packets.len() as u64
        );
    }
    #[test]
    fn scalar_and_runtime_simd_have_identical_wire_parity() {
        use reed_solomon_simd::{
            engine::NoSimd,
            rate::{DefaultRateEncoder, RateEncoder},
        };
        fn check_engine<E: reed_solomon_simd::engine::Engine + Clone>(
            engine: E,
            original: &[Vec<u8>],
            expected: &[Vec<u8>],
            size: usize,
        ) {
            use reed_solomon_simd::rate::{DefaultRateDecoder, RateDecoder};
            let mut encoder = DefaultRateEncoder::new(32, 8, size, engine.clone(), None).unwrap();
            for shard in original {
                encoder.add_original_shard(shard).unwrap();
            }
            let parity: Vec<Vec<u8>> = encoder
                .encode()
                .unwrap()
                .recovery_iter()
                .map(<[u8]>::to_vec)
                .collect();
            assert_eq!(parity, expected);
            let mut decoder = DefaultRateDecoder::new(32, 8, size, engine, None).unwrap();
            for (n, shard) in original.iter().enumerate().skip(8) {
                decoder.add_original_shard(n, shard).unwrap();
            }
            for (n, shard) in parity.iter().enumerate() {
                decoder.add_recovery_shard(n, shard).unwrap();
            }
            let restored = decoder.decode().unwrap();
            assert_eq!(restored.restored_original_iter().count(), 8);
            for (n, shard) in restored.restored_original_iter() {
                assert_eq!(shard, original[n]);
            }
        }
        use sha2::{Digest, Sha256};
        for (size, expected) in [
            (
                64,
                "ebaead42623379b2f5565e5c695e16aaee5e6374e7b8f31e84b33476f859ef00",
            ),
            (
                1148,
                "b38af81dcefcefb22d67b3186637327879576c109ef9b2e7be0c2230e9f838e9",
            ),
        ] {
            let original: Vec<_> = (0..32)
                .map(|n| {
                    (0..size)
                        .map(|i| ((n * 17 + i * 31) % 251) as u8)
                        .collect::<Vec<_>>()
                })
                .collect();
            let mut scalar = DefaultRateEncoder::new(32, 8, size, NoSimd::new(), None).unwrap();
            for shard in &original {
                scalar.add_original_shard(shard).unwrap();
            }
            let scalar: Vec<Vec<u8>> = scalar
                .encode()
                .unwrap()
                .recovery_iter()
                .map(<[u8]>::to_vec)
                .collect();
            let simd = reed_solomon_simd::encode(32, 8, &original).unwrap();
            assert_eq!(scalar, simd);
            check_engine(NoSimd::new(), &original, &simd, size);
            #[cfg(target_arch = "x86_64")]
            {
                if std::is_x86_feature_detected!("ssse3") {
                    check_engine(
                        reed_solomon_simd::engine::Ssse3::new(),
                        &original,
                        &simd,
                        size,
                    );
                }
                if std::is_x86_feature_detected!("avx2") {
                    check_engine(
                        reed_solomon_simd::engine::Avx2::new(),
                        &original,
                        &simd,
                        size,
                    );
                }
            }
            #[cfg(target_arch = "aarch64")]
            if std::arch::is_aarch64_feature_detected!("neon") {
                check_engine(
                    reed_solomon_simd::engine::Neon::new(),
                    &original,
                    &simd,
                    size,
                );
            }
            // The same fixed vectors run on x64, ARM Neon, and scalar engines.
            let mut digest = Sha256::new();
            for shard in &simd {
                digest.update(shard);
            }
            assert_eq!(format!("{:x}", digest.finalize()), expected);
        }
    }
    #[test]
    fn clock_offset_never_compares_remote_instants_directly() {
        let mut clock = ClockEstimator::default();
        clock.observe(1000, 2010, 2020, 1030);
        assert_eq!(clock.uncertainty_us, 10);
        assert_eq!(clock.age(2500, 1600), Some(Duration::from_micros(100)));
        assert_eq!(clock.age(2500, 1000), None);
        assert_eq!(clock.age(1000, 60_000_000), Some(Duration::from_secs(60)));
        clock.observe(2000, 3000, 2999, 2100);
        assert_eq!(clock.uncertainty_us, 10);
    }
    #[test]
    fn pixel_storage_is_not_recycled_while_a_picture_is_still_referenced() {
        let pool = PixelPool::default();
        let mut first = pool.acquire(64);
        first.fill(7);
        let first = first.into_bytes();
        let copy = first.clone();
        drop(first);
        let mut second = pool.acquire(64);
        second.fill(9);
        assert_eq!(copy.as_ref(), &[7; 64]);
        drop(copy);
        drop(second);
        assert_eq!(pool.0.lock().unwrap().len(), 2);
    }
}
