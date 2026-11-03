//! Frozen v2 media wire format (QUIC datagrams). All integers little endian.
//!
//! Every packet starts with one byte: `VERSION << 4 | kind`. A peer that sees
//! another version drops the packet; incompatible changes bump [`VERSION`]
//! together with the negotiated `media` feature version.
//!
//! # Media packet (video, audio): 41-byte header + one shard
//!
//! | off | size | field |
//! |----:|-----:|-------|
//! | 0 | 1 | version/kind |
//! | 1 | 1 | flags: keyframe, recovery point, retransmission, end of frame |
//! | 2 | 4 | stream id |
//! | 6 | 2 | stream generation (configuration epoch) |
//! | 8 | 4 | transport sequence: per connection, every packet, never reused |
//! | 12 | 4 | send time, sender clock, µs (wraps every ~71 min) |
//! | 16 | 4 | frame number within the stream generation |
//! | 20 | 4 | capture time, sender clock, µs |
//! | 24 | 1 | slice index |
//! | 25 | 1 | slice count |
//! | 26 | 2 | reference: frames back (0 = none / intra) |
//! | 28 | 1 | reference long-term slot, `0xFF` = none |
//! | 29 | 1 | marks this frame as long-term slot, `0xFF` = none |
//! | 30 | 4 | slice length before FEC |
//! | 34 | 1 | FEC block index |
//! | 35 | 1 | FEC block count |
//! | 36 | 1 | shard index in block (`>= data shards` is parity) |
//! | 37 | 1 | data shards in block (1..=32) |
//! | 38 | 1 | parity shards in block (0..=32) |
//! | 39 | 2 | shard length (every shard of the block, last data shard padded) |
//! | 41 | n | shard bytes, exactly `shard length` |
//!
//! # Feedback packet (receiver → sender)
//!
//! Transport-wide arrival report for delay-based rate control, retransmission
//! requests, and per-stream recovery state. See [`Feedback`].
//!
//! # Probe packet (sender → receiver)
//!
//! Padding with a transport sequence and send time, for bandwidth probing.
//! Probes share the sequence space, so feedback reports their arrival.
//!
//! Parsing is strict and canonical: any packet that parses re-encodes to the
//! same bytes. Reserved bits must be zero.

use std::fmt;

pub const VERSION: u8 = 2;
pub const MEDIA_HEADER_BYTES: usize = 41;
pub const MAX_DATA_SHARDS: u8 = 32;
pub const MAX_PARITY_SHARDS: u8 = 32;
/// Largest slice (one encoder output unit) the format can describe.
pub const MAX_SLICE_BYTES: u32 = 8 << 20;
/// Long-term reference slots are `0..MAX_LTR_SLOTS`.
pub const MAX_LTR_SLOTS: u8 = 16;
/// Feedback covers at most this many consecutive transport sequences.
pub const MAX_FEEDBACK_SEQS: usize = 4096;
/// Arrival times in feedback are quantized to this many microseconds.
pub const ARRIVAL_TICK_US: i64 = 32;
const NONE_U8: u8 = 0xFF;
const FEEDBACK_FIXED_BYTES: usize = 16;
const NACK_BYTES: usize = 6;
const STREAM_STATE_BYTES: usize = 19;
const PROBE_HEADER_BYTES: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Video = 1,
    Audio = 2,
    Feedback = 3,
    Probe = 4,
}

impl Kind {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Video),
            2 => Some(Self::Audio),
            3 => Some(Self::Feedback),
            4 => Some(Self::Probe),
            _ => None,
        }
    }

    fn tag(self) -> u8 {
        (VERSION << 4) | self as u8
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WireError {
    Truncated {
        needed: usize,
        got: usize,
    },
    Version(u8),
    UnknownKind(u8),
    Invalid(&'static str),
    /// Encoding: the packet does not fit `limit` bytes.
    TooLarge {
        size: usize,
        limit: usize,
    },
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, got } => {
                write!(f, "truncated packet: {got} of {needed} bytes")
            }
            Self::Version(version) => write!(f, "unsupported media wire version {version}"),
            Self::UnknownKind(kind) => write!(f, "unknown media packet kind {kind}"),
            Self::Invalid(reason) => write!(f, "invalid media packet: {reason}"),
            Self::TooLarge { size, limit } => write!(f, "packet of {size} bytes exceeds {limit}"),
        }
    }
}

impl std::error::Error for WireError {}

/// What a frame was predicted from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reference {
    /// Independently decodable (IDR, or an intra slice of a refresh wave).
    None,
    /// The frame `n` frames earlier in the same stream generation (n >= 1).
    Back(u16),
    /// A long-term reference slot.
    LongTerm(u8),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaFlags(u8);

impl MediaFlags {
    /// Instantaneous decoder refresh: decodable with no prior state.
    pub const KEYFRAME: Self = Self(1);
    /// Decoding is clean again from this frame on (end of an intra-refresh
    /// wave, or the first frame predicted from an invalidation-safe reference).
    pub const RECOVERY_POINT: Self = Self(1 << 1);
    /// A retransmitted copy of an earlier shard (new transport sequence).
    pub const RETRANSMISSION: Self = Self(1 << 2);
    /// The last slice of the frame.
    pub const END_OF_FRAME: Self = Self(1 << 3);
    const KNOWN: u8 = 0x0F;

    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MediaKind {
    Video,
    Audio,
}

/// Header of one media shard. See the module docs for the layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaHeader {
    pub kind: MediaKind,
    pub flags: MediaFlags,
    pub stream: u32,
    pub generation: u16,
    pub seq: u32,
    pub send_us: u32,
    pub frame: u32,
    pub capture_us: u32,
    pub slice: u8,
    pub slices: u8,
    pub reference: Reference,
    pub mark_long_term: Option<u8>,
    pub slice_len: u32,
    pub block: u8,
    pub blocks: u8,
    pub shard: u8,
    pub data_shards: u8,
    pub parity_shards: u8,
    pub shard_len: u16,
}

impl MediaHeader {
    pub fn is_parity(&self) -> bool {
        self.shard >= self.data_shards
    }

    fn validate(&self) -> Result<(), WireError> {
        let invalid = |reason| Err(WireError::Invalid(reason));
        if self.slices == 0 || self.slice >= self.slices {
            return invalid("slice index out of range");
        }
        if self.blocks == 0 || self.block >= self.blocks {
            return invalid("block index out of range");
        }
        if self.data_shards == 0 || self.data_shards > MAX_DATA_SHARDS {
            return invalid("data shard count out of range");
        }
        if self.parity_shards > MAX_PARITY_SHARDS {
            return invalid("parity shard count out of range");
        }
        if u16::from(self.shard) >= u16::from(self.data_shards) + u16::from(self.parity_shards) {
            return invalid("shard index out of range");
        }
        if self.shard_len == 0 {
            return invalid("empty shard");
        }
        if self.slice_len == 0 || self.slice_len > MAX_SLICE_BYTES {
            return invalid("slice length out of range");
        }
        let capacity =
            u64::from(self.blocks) * u64::from(MAX_DATA_SHARDS) * u64::from(self.shard_len);
        if u64::from(self.slice_len) > capacity {
            return invalid("slice length exceeds its blocks");
        }
        match self.reference {
            Reference::Back(0) => return invalid("reference of zero frames back"),
            Reference::LongTerm(slot) if slot >= MAX_LTR_SLOTS => {
                return invalid("long-term reference slot out of range");
            }
            _ => {}
        }
        if self
            .mark_long_term
            .is_some_and(|slot| slot >= MAX_LTR_SLOTS)
        {
            return invalid("long-term mark slot out of range");
        }
        if self.flags.contains(MediaFlags::KEYFRAME) && self.reference != Reference::None {
            return invalid("keyframe with a reference");
        }
        if self.kind == MediaKind::Audio
            && (self.slices != 1
                || self.reference != Reference::None
                || self.mark_long_term.is_some())
        {
            return invalid("audio with video-only fields");
        }
        Ok(())
    }

    /// Appends header and shard to `out`.
    pub fn encode(&self, shard: &[u8], out: &mut Vec<u8>) -> Result<(), WireError> {
        self.validate()?;
        if shard.len() != usize::from(self.shard_len) {
            return Err(WireError::Invalid("shard length does not match the header"));
        }
        let (ref_back, ref_slot) = match self.reference {
            Reference::None => (0, NONE_U8),
            Reference::Back(back) => (back, NONE_U8),
            Reference::LongTerm(slot) => (0, slot),
        };
        let kind = match self.kind {
            MediaKind::Video => Kind::Video,
            MediaKind::Audio => Kind::Audio,
        };
        out.reserve(MEDIA_HEADER_BYTES + shard.len());
        out.push(kind.tag());
        out.push(self.flags.bits());
        out.extend_from_slice(&self.stream.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.send_us.to_le_bytes());
        out.extend_from_slice(&self.frame.to_le_bytes());
        out.extend_from_slice(&self.capture_us.to_le_bytes());
        out.push(self.slice);
        out.push(self.slices);
        out.extend_from_slice(&ref_back.to_le_bytes());
        out.push(ref_slot);
        out.push(self.mark_long_term.unwrap_or(NONE_U8));
        out.extend_from_slice(&self.slice_len.to_le_bytes());
        out.push(self.block);
        out.push(self.blocks);
        out.push(self.shard);
        out.push(self.data_shards);
        out.push(self.parity_shards);
        out.extend_from_slice(&self.shard_len.to_le_bytes());
        out.extend_from_slice(shard);
        Ok(())
    }

    fn parse(kind: MediaKind, packet: &[u8]) -> Result<(Self, &[u8]), WireError> {
        let mut r = Reader::new(packet);
        r.skip(1)?;
        let flags = r.u8()?;
        if flags & !MediaFlags::KNOWN != 0 {
            return Err(WireError::Invalid("reserved flag bits set"));
        }
        let stream = r.u32()?;
        let generation = r.u16()?;
        let seq = r.u32()?;
        let send_us = r.u32()?;
        let frame = r.u32()?;
        let capture_us = r.u32()?;
        let slice = r.u8()?;
        let slices = r.u8()?;
        let ref_back = r.u16()?;
        let ref_slot = r.u8()?;
        let mark = r.u8()?;
        let reference = match (ref_back, ref_slot) {
            (0, NONE_U8) => Reference::None,
            (back, NONE_U8) => Reference::Back(back),
            (0, slot) => Reference::LongTerm(slot),
            _ => return Err(WireError::Invalid("both frame and long-term reference")),
        };
        let header = Self {
            kind,
            flags: MediaFlags(flags),
            stream,
            generation,
            seq,
            send_us,
            frame,
            capture_us,
            slice,
            slices,
            reference,
            mark_long_term: (mark != NONE_U8).then_some(mark),
            slice_len: r.u32()?,
            block: r.u8()?,
            blocks: r.u8()?,
            shard: r.u8()?,
            data_shards: r.u8()?,
            parity_shards: r.u8()?,
            shard_len: r.u16()?,
        };
        header.validate()?;
        let shard = r.rest();
        if shard.len() != usize::from(header.shard_len) {
            return Err(WireError::Invalid("shard length does not match the header"));
        }
        Ok((header, shard))
    }
}

/// A range of transport sequences the receiver asks to be resent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeqRange {
    pub first: u32,
    /// Number of sequences, at least 1.
    pub len: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryRequest {
    None,
    /// Frames `first..=last` are lost: predict from a frame before them.
    Invalidate {
        first: u32,
        last: u32,
    },
    /// No usable reference remains: send an independently decodable frame.
    Keyframe,
}

/// Receiver state for one video stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamState {
    pub stream: u32,
    pub generation: u16,
    /// Highest frame decoded cleanly; the sender may use it as a reference.
    pub last_decoded: u32,
    pub request: RecoveryRequest,
}

/// Receiver report.
///
/// `arrivals[i]` describes transport sequence `base_seq + i`: `None` if not
/// received (yet), or its arrival time in µs relative to `reference_us` on the
/// receiver clock, quantized to [`ARRIVAL_TICK_US`]. Offsets may be negative
/// when packets were reordered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Feedback {
    /// Increments per feedback packet, so the sender can detect lost reports.
    pub feedback_seq: u32,
    pub base_seq: u32,
    pub reference_us: u32,
    pub arrivals: Vec<Option<i64>>,
    pub nacks: Vec<SeqRange>,
    pub streams: Vec<StreamState>,
}

impl Feedback {
    /// Exact encoded size.
    pub fn encoded_len(&self) -> usize {
        let received = self.arrivals.iter().filter(|a| a.is_some()).count();
        FEEDBACK_FIXED_BYTES
            + self.arrivals.len().div_ceil(8)
            + 2 * received
            + 1
            + NACK_BYTES * self.nacks.len()
            + 1
            + STREAM_STATE_BYTES * self.streams.len()
    }

    /// Appends the packet to `out` if it fits in `limit` bytes.
    pub fn encode(&self, limit: usize, out: &mut Vec<u8>) -> Result<(), WireError> {
        let size = self.encoded_len();
        if size > limit {
            return Err(WireError::TooLarge { size, limit });
        }
        if self.arrivals.len() > MAX_FEEDBACK_SEQS {
            return Err(WireError::Invalid("too many sequences in one feedback"));
        }
        if self.nacks.len() > usize::from(u8::MAX) || self.streams.len() > usize::from(u8::MAX) {
            return Err(WireError::Invalid("too many feedback entries"));
        }
        if self.nacks.iter().any(|nack| nack.len == 0) {
            return Err(WireError::Invalid("empty retransmission range"));
        }
        let mut ticks = Vec::new();
        let mut previous = 0i64;
        for arrival in self.arrivals.iter().flatten() {
            if arrival % ARRIVAL_TICK_US != 0 {
                return Err(WireError::Invalid("arrival not quantized to the tick"));
            }
            let tick = arrival / ARRIVAL_TICK_US;
            let delta = i16::try_from(tick - previous)
                .map_err(|_| WireError::Invalid("arrival delta exceeds the i16 tick range"))?;
            ticks.push(delta);
            previous = tick;
        }
        let start = out.len();
        out.reserve(size);
        out.push(Kind::Feedback.tag());
        out.push(0);
        out.extend_from_slice(&self.feedback_seq.to_le_bytes());
        out.extend_from_slice(&self.base_seq.to_le_bytes());
        out.extend_from_slice(&self.reference_us.to_le_bytes());
        out.extend_from_slice(&(self.arrivals.len() as u16).to_le_bytes());
        for chunk in self.arrivals.chunks(8) {
            let mut byte = 0u8;
            for (bit, arrival) in chunk.iter().enumerate() {
                if arrival.is_some() {
                    byte |= 1 << bit;
                }
            }
            out.push(byte);
        }
        for delta in ticks {
            out.extend_from_slice(&delta.to_le_bytes());
        }
        out.push(self.nacks.len() as u8);
        for nack in &self.nacks {
            out.extend_from_slice(&nack.first.to_le_bytes());
            out.extend_from_slice(&nack.len.to_le_bytes());
        }
        out.push(self.streams.len() as u8);
        for state in &self.streams {
            let (code, first, last) = match state.request {
                RecoveryRequest::None => (0u8, 0u32, 0u32),
                RecoveryRequest::Invalidate { first, last } => (1, first, last),
                RecoveryRequest::Keyframe => (2, 0, 0),
            };
            out.extend_from_slice(&state.stream.to_le_bytes());
            out.extend_from_slice(&state.generation.to_le_bytes());
            out.extend_from_slice(&state.last_decoded.to_le_bytes());
            out.push(code);
            out.extend_from_slice(&first.to_le_bytes());
            out.extend_from_slice(&last.to_le_bytes());
        }
        debug_assert_eq!(out.len() - start, size, "encoded_len matches the encoding");
        Ok(())
    }

    fn parse(packet: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(packet);
        r.skip(1)?;
        if r.u8()? != 0 {
            return Err(WireError::Invalid("reserved feedback flags set"));
        }
        let feedback_seq = r.u32()?;
        let base_seq = r.u32()?;
        let reference_us = r.u32()?;
        let count = usize::from(r.u16()?);
        if count > MAX_FEEDBACK_SEQS {
            return Err(WireError::Invalid("too many sequences in one feedback"));
        }
        let bitmap = r.bytes(count.div_ceil(8))?;
        if count % 8 != 0 && bitmap[count / 8] >> (count % 8) != 0 {
            return Err(WireError::Invalid("bits set past the sequence count"));
        }
        let mut arrivals = Vec::with_capacity(count);
        let mut tick = 0i64;
        for i in 0..count {
            if bitmap[i / 8] & (1 << (i % 8)) != 0 {
                tick += i64::from(r.i16()?);
                arrivals.push(Some(tick * ARRIVAL_TICK_US));
            } else {
                arrivals.push(None);
            }
        }
        let nack_count = usize::from(r.u8()?);
        let mut nacks = Vec::with_capacity(nack_count);
        for _ in 0..nack_count {
            let first = r.u32()?;
            let len = r.u16()?;
            if len == 0 {
                return Err(WireError::Invalid("empty retransmission range"));
            }
            nacks.push(SeqRange { first, len });
        }
        let stream_count = usize::from(r.u8()?);
        let mut streams = Vec::with_capacity(stream_count);
        for _ in 0..stream_count {
            let stream = r.u32()?;
            let generation = r.u16()?;
            let last_decoded = r.u32()?;
            let code = r.u8()?;
            let first = r.u32()?;
            let last = r.u32()?;
            let request = match (code, first, last) {
                (0, 0, 0) => RecoveryRequest::None,
                (1, first, last) => RecoveryRequest::Invalidate { first, last },
                (2, 0, 0) => RecoveryRequest::Keyframe,
                _ => return Err(WireError::Invalid("unknown recovery request")),
            };
            streams.push(StreamState {
                stream,
                generation,
                last_decoded,
                request,
            });
        }
        if !r.rest().is_empty() {
            return Err(WireError::Invalid("trailing bytes after feedback"));
        }
        Ok(Self {
            feedback_seq,
            base_seq,
            reference_us,
            arrivals,
            nacks,
            streams,
        })
    }
}

/// Bandwidth probe: `padding` zero bytes after the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    pub seq: u32,
    pub send_us: u32,
    /// Probes sent together at one target rate share a cluster id.
    pub cluster: u16,
    pub padding: u16,
}

impl Probe {
    pub fn encoded_len(&self) -> usize {
        PROBE_HEADER_BYTES + usize::from(self.padding)
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.reserve(self.encoded_len());
        out.push(Kind::Probe.tag());
        out.push(0);
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.send_us.to_le_bytes());
        out.extend_from_slice(&self.cluster.to_le_bytes());
        out.resize(out.len() + usize::from(self.padding), 0);
    }

    fn parse(packet: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(packet);
        r.skip(1)?;
        if r.u8()? != 0 {
            return Err(WireError::Invalid("reserved probe flags set"));
        }
        let seq = r.u32()?;
        let send_us = r.u32()?;
        let cluster = r.u16()?;
        let padding = r.rest();
        if padding.iter().any(|byte| *byte != 0) {
            return Err(WireError::Invalid("non-zero probe padding"));
        }
        let padding = u16::try_from(padding.len())
            .map_err(|_| WireError::Invalid("probe padding too large"))?;
        Ok(Self {
            seq,
            send_us,
            cluster,
            padding,
        })
    }
}

/// A parsed datagram. Media shards borrow from the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Media(MediaHeader, &'a [u8]),
    Feedback(Feedback),
    Probe(Probe),
}

impl Packet<'_> {
    /// Transport sequence of a sender-side packet; feedback has none.
    pub fn seq(&self) -> Option<u32> {
        match self {
            Self::Media(header, _) => Some(header.seq),
            Self::Probe(probe) => Some(probe.seq),
            Self::Feedback(_) => None,
        }
    }
}

/// Parses one datagram. Never panics; see the fuzz tests.
pub fn parse(packet: &[u8]) -> Result<Packet<'_>, WireError> {
    let Some(&tag) = packet.first() else {
        return Err(WireError::Truncated { needed: 1, got: 0 });
    };
    if tag >> 4 != VERSION {
        return Err(WireError::Version(tag >> 4));
    }
    match Kind::from_u8(tag & 0x0F) {
        Some(Kind::Video) => {
            MediaHeader::parse(MediaKind::Video, packet).map(|(h, s)| Packet::Media(h, s))
        }
        Some(Kind::Audio) => {
            MediaHeader::parse(MediaKind::Audio, packet).map(|(h, s)| Packet::Media(h, s))
        }
        Some(Kind::Feedback) => Feedback::parse(packet).map(Packet::Feedback),
        Some(Kind::Probe) => Probe::parse(packet).map(Packet::Probe),
        None => Err(WireError::UnknownKind(tag & 0x0F)),
    }
}

/// Re-encodes a parsed packet; used to check that parsing is canonical.
pub fn encode(packet: &Packet<'_>, out: &mut Vec<u8>) -> Result<(), WireError> {
    match packet {
        Packet::Media(header, shard) => header.encode(shard, out),
        Packet::Feedback(feedback) => feedback.encode(usize::MAX, out),
        Packet::Probe(probe) => {
            probe.encode(out);
            Ok(())
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len());
        let Some(end) = end else {
            return Err(WireError::Truncated {
                needed: self.at.saturating_add(n),
                got: self.bytes.len(),
            });
        };
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn skip(&mut self, n: usize) -> Result<(), WireError> {
        self.bytes(n).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, WireError> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn i16(&mut self) -> Result<i16, WireError> {
        let b = self.bytes(2)?;
        Ok(i16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, WireError> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn rest(&mut self) -> &'a [u8] {
        let rest = &self.bytes[self.at..];
        self.at = self.bytes.len();
        rest
    }
}

#[cfg(test)]
mod tests;
