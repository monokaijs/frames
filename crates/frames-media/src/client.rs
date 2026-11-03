//! Receiving side of one video stream, independent of I/O and threads.
//!
//! [`ClientSession::on_datagram`] takes media packets, [`ClientSession::poll`]
//! decodes complete frames in order, and [`ClientSession::poll_feedback`]
//! produces the reports the host's rate control and recovery depend on.
//!
//! Hardware decoders cannot tell that a frame's reference is missing: they
//! decode garbage. So this session tracks the reference chain itself and
//! never submits a frame whose reference was lost, except intra-refresh
//! frames, which are designed to heal; it asks the host to recover instead.

use crate::api::{AccessUnit, Decoded, DecodedFrame, FrameKind, FrameReference, VideoDecoder};
use crate::fec;
use crate::seq::Unwrapper;
use crate::wire::{
    self, ARRIVAL_TICK_US, Feedback, MAX_FEEDBACK_SEQS, MAX_LTR_SLOTS, MediaFlags, MediaHeader,
    Packet, RecoveryRequest, Reference, SeqRange, StreamState,
};
use bytes::{Bytes, BytesMut};
use std::collections::{BTreeMap, BTreeSet};

/// Packets arriving within this time of a gap may just be reordered.
const REORDER_TOLERANCE_US: u64 = 2_000;
const MAX_NACKS_PER_PACKET: u8 = 2;
const MISSING_HORIZON_US: u64 = 250_000;
/// A frame still trickling in after this long is given up regardless.
const LONGEST_FRAME_US: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClientConfig {
    pub stream: u32,
    pub generation: u16,
    pub max_datagram: usize,
    pub feedback_interval_us: u64,
    /// A frame is declared lost this long after it started arriving (or
    /// after a later frame did), at least; see `frame_timeout_us`.
    pub min_frame_timeout_us: u64,
    pub nack: bool,
    /// The host's encoder can invalidate references (else ask for keyframes).
    pub invalidate: bool,
    /// Added to host capture times to express them on the local clock.
    pub clock_offset_us: i64,
}

impl ClientConfig {
    pub fn new(stream: u32, generation: u16) -> Self {
        Self {
            stream,
            generation,
            max_datagram: 1200,
            feedback_interval_us: 20_000,
            min_frame_timeout_us: 30_000,
            nack: true,
            invalidate: true,
            clock_offset_us: 0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientStats {
    pub frames_decoded: u64,
    pub frames_clean: u64,
    pub frames_lost: u64,
    /// Complete frames not submitted because their reference was lost.
    pub frames_skipped: u64,
    /// Frames the decoder itself rejected as broken; should stay zero.
    pub decoder_broken: u64,
    pub recoveries: u64,
    /// Time from the first lost frame to the next clean one, per recovery.
    pub recovery_us: Vec<u64>,
    /// Capture to decoded, per clean frame.
    pub latency_us: Vec<u64>,
    pub nacks: u64,
    pub feedbacks: u64,
    pub keyframes: u64,
}

struct BlockAssembly {
    data: u8,
    parity: u8,
    shards: Vec<Option<Vec<u8>>>,
    done: Option<Vec<Vec<u8>>>,
}

struct SliceAssembly {
    len: usize,
    blocks: Vec<Option<BlockAssembly>>,
    done: Option<Bytes>,
}

struct FrameAssembly {
    first_arrival_us: u64,
    last_arrival_us: u64,
    header: MediaHeader,
    slices: Vec<Option<SliceAssembly>>,
}

impl FrameAssembly {
    fn consistent(&self, h: &MediaHeader) -> bool {
        let f = &self.header;
        f.slices == h.slices
            && f.reference == h.reference
            && f.mark_long_term == h.mark_long_term
            && (f.flags.bits()
                & !MediaFlags::RETRANSMISSION.bits()
                & !MediaFlags::END_OF_FRAME.bits())
                == (h.flags.bits()
                    & !MediaFlags::RETRANSMISSION.bits()
                    & !MediaFlags::END_OF_FRAME.bits())
    }

    fn complete(&self) -> bool {
        self.slices
            .iter()
            .all(|s| s.as_ref().is_some_and(|s| s.done.is_some()))
    }

    fn kind(&self) -> FrameKind {
        let flags = self.header.flags;
        if flags.contains(MediaFlags::KEYFRAME) {
            FrameKind::Keyframe
        } else if flags.contains(MediaFlags::INTRA_REFRESH) {
            FrameKind::Refresh {
                complete: flags.contains(MediaFlags::RECOVERY_POINT),
            }
        } else {
            FrameKind::Predicted
        }
    }
}

struct Missing {
    first_seen_us: u64,
    nacks: u8,
    last_nack_us: u64,
}

pub struct ClientSession {
    config: ClientConfig,
    rtt_us: u64,
    seq_unwrap: Unwrapper,
    highest_seq: Option<u64>,
    arrivals: BTreeMap<u64, u64>,
    missing: BTreeMap<u64, Missing>,
    frame_unwrap: Option<Unwrapper>,
    capture_unwrap: Unwrapper,
    frames: BTreeMap<u64, FrameAssembly>,
    next_frame: Option<u64>,
    highest_frame: u64,
    decoded: BTreeSet<u64>,
    long_term: [Option<u64>; MAX_LTR_SLOTS as usize],
    last_decoded: Option<u64>,
    broken_since: Option<(u64, u64)>,
    feedback_seq: u32,
    last_feedback_us: Option<u64>,
    last_request: RecoveryRequest,
    stats: ClientStats,
}

impl ClientSession {
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            rtt_us: 10_000,
            seq_unwrap: Unwrapper::new(),
            highest_seq: None,
            arrivals: BTreeMap::new(),
            missing: BTreeMap::new(),
            frame_unwrap: None,
            capture_unwrap: Unwrapper::new(),
            frames: BTreeMap::new(),
            next_frame: None,
            highest_frame: 0,
            decoded: BTreeSet::new(),
            long_term: [None; MAX_LTR_SLOTS as usize],
            last_decoded: None,
            broken_since: None,
            feedback_seq: 0,
            last_feedback_us: None,
            last_request: RecoveryRequest::None,
            stats: ClientStats::default(),
        }
    }

    pub fn set_rtt(&mut self, rtt_us: u64) {
        self.rtt_us = rtt_us;
    }

    pub fn stats(&self) -> &ClientStats {
        &self.stats
    }

    /// Whether frames are currently unusable until the host recovers.
    pub fn recovering(&self) -> bool {
        self.broken_since.is_some()
    }

    fn frame_timeout_us(&self) -> u64 {
        self.config
            .min_frame_timeout_us
            .max(2 * self.rtt_us + 10_000)
    }

    pub fn on_datagram(&mut self, now_us: u64, datagram: &[u8]) {
        match wire::parse(datagram) {
            Ok(Packet::Media(header, shard)) => {
                self.track_seq(header.seq, now_us);
                if header.stream == self.config.stream
                    && header.generation == self.config.generation
                {
                    self.on_shard(now_us, header, shard);
                }
            }
            Ok(Packet::Probe(probe)) => self.track_seq(probe.seq, now_us),
            Ok(Packet::Feedback(_)) | Err(_) => {}
        }
    }

    fn track_seq(&mut self, seq: u32, now_us: u64) {
        let seq = self.seq_unwrap.unwrap(seq);
        self.arrivals.insert(seq, now_us);
        match self.highest_seq {
            Some(highest) if seq > highest => {
                for gap in (highest + 1)..seq.min(highest + 1 + MAX_FEEDBACK_SEQS as u64) {
                    self.missing.insert(
                        gap,
                        Missing {
                            first_seen_us: now_us,
                            nacks: 0,
                            last_nack_us: 0,
                        },
                    );
                }
                self.highest_seq = Some(seq);
            }
            Some(_) => {
                self.missing.remove(&seq);
            }
            None => self.highest_seq = Some(seq),
        }
    }

    fn on_shard(&mut self, now_us: u64, header: MediaHeader, shard: &[u8]) {
        let unwrapper = self
            .frame_unwrap
            .get_or_insert_with(|| Unwrapper::starting_at(u64::from(header.frame)));
        let frame = unwrapper.unwrap(header.frame);
        let next = *self.next_frame.get_or_insert(frame);
        if frame < next {
            return; // already decoded, skipped, or declared lost
        }
        self.highest_frame = self.highest_frame.max(frame);
        let assembly = self.frames.entry(frame).or_insert_with(|| FrameAssembly {
            first_arrival_us: now_us,
            last_arrival_us: now_us,
            header,
            slices: (0..header.slices).map(|_| None).collect(),
        });
        if !assembly.consistent(&header) {
            return;
        }
        assembly.last_arrival_us = now_us;
        if header.flags.contains(MediaFlags::END_OF_FRAME) {
            assembly.header.flags = assembly.header.flags.with(MediaFlags::END_OF_FRAME);
        }
        let slice =
            assembly.slices[usize::from(header.slice)].get_or_insert_with(|| SliceAssembly {
                len: header.slice_len as usize,
                blocks: (0..header.blocks).map(|_| None).collect(),
                done: None,
            });
        if slice.done.is_some()
            || slice.len != header.slice_len as usize
            || slice.blocks.len() != usize::from(header.blocks)
        {
            return;
        }
        let block = slice.blocks[usize::from(header.block)].get_or_insert_with(|| BlockAssembly {
            data: header.data_shards,
            parity: header.parity_shards,
            shards: vec![None; usize::from(header.data_shards) + usize::from(header.parity_shards)],
            done: None,
        });
        if block.done.is_some()
            || block.data != header.data_shards
            || block.parity != header.parity_shards
        {
            return;
        }
        block.shards[usize::from(header.shard)].get_or_insert_with(|| shard.to_vec());
        block.done = fec::reconstruct(
            fec::Block {
                data: block.data,
                parity: block.parity,
            },
            &block.shards,
        );
        if block.done.is_some() {
            block.shards = Vec::new();
        }
        if slice
            .blocks
            .iter()
            .all(|b| b.as_ref().is_some_and(|b| b.done.is_some()))
        {
            let mut bytes = BytesMut::with_capacity(slice.len);
            for block in slice.blocks.iter().flatten() {
                for shard in block.done.iter().flatten() {
                    bytes.extend_from_slice(shard);
                }
            }
            bytes.truncate(slice.len);
            slice.done = Some(bytes.freeze());
            slice.blocks = Vec::new();
        }
    }

    fn mark_broken(&mut self, frame: u64, now_us: u64) {
        self.broken_since.get_or_insert((frame, now_us));
    }

    fn reference_available(&self, frame: u64, reference: Reference) -> bool {
        match reference {
            Reference::None => true,
            Reference::Back(back) => frame
                .checked_sub(u64::from(back))
                .is_some_and(|r| self.decoded.contains(&r)),
            Reference::LongTerm { slot, back } => frame
                .checked_sub(u64::from(back))
                .is_some_and(|r| self.long_term[usize::from(slot)] == Some(r)),
        }
    }

    /// Decodes every frame that is ready, in order.
    pub fn poll(&mut self, now_us: u64, decoder: &mut dyn VideoDecoder) -> Vec<DecodedFrame> {
        let mut out = Vec::new();
        let timeout = self.frame_timeout_us();
        while let Some(frame) = self.next_frame {
            let ready = self.frames.get(&frame).is_some_and(FrameAssembly::complete);
            if !ready {
                // A frame still receiving packets is not lost: count from its
                // last packet (a large keyframe can take longer than the
                // timeout to arrive), or, if none of it arrived, from when a
                // later frame started arriving.
                let overdue = match self.frames.get(&frame) {
                    Some(assembly) => {
                        now_us > assembly.last_arrival_us + timeout
                            || now_us > assembly.first_arrival_us + LONGEST_FRAME_US
                    }
                    None => self
                        .frames
                        .range(frame + 1..)
                        .next()
                        .is_some_and(|(_, later)| now_us > later.first_arrival_us + timeout),
                };
                if !overdue {
                    break;
                }
                self.frames.remove(&frame);
                self.stats.frames_lost += 1;
                self.mark_broken(frame, now_us);
                self.next_frame = Some(frame + 1);
                continue;
            }
            let assembly = self.frames.remove(&frame).expect("ready");
            self.next_frame = Some(frame + 1);
            let kind = assembly.kind();
            let header = assembly.header;
            let available = self.reference_available(frame, header.reference);
            if !available && !matches!(kind, FrameKind::Refresh { .. }) {
                self.stats.frames_skipped += 1;
                self.mark_broken(frame, now_us);
                continue;
            }
            let mut data = BytesMut::new();
            for slice in assembly.slices.iter().flatten() {
                data.extend_from_slice(slice.done.as_ref().expect("complete"));
            }
            let captured_host = self.capture_unwrap.unwrap(header.capture_us);
            let captured_us = (captured_host as i64 + self.config.clock_offset_us).max(0) as u64;
            let unit = AccessUnit {
                frame,
                kind,
                reference: match header.reference {
                    Reference::None => FrameReference::None,
                    Reference::Back(back) => FrameReference::Frame(frame - u64::from(back)),
                    Reference::LongTerm { slot, .. } => FrameReference::LongTerm(slot),
                },
                mark_long_term: header.mark_long_term,
                captured_us,
                data: data.freeze(),
            };
            match decoder.decode(unit) {
                Ok(Decoded::Frame(decoded)) => {
                    if kind == FrameKind::Keyframe {
                        self.decoded.clear();
                        self.long_term = [None; MAX_LTR_SLOTS as usize];
                        self.stats.keyframes += 1;
                    }
                    self.decoded.insert(frame);
                    while self.decoded.len() > 64 {
                        self.decoded.pop_first();
                    }
                    self.stats.frames_decoded += 1;
                    if decoded.clean {
                        self.stats.frames_clean += 1;
                        self.last_decoded = Some(frame);
                        self.stats
                            .latency_us
                            .push(now_us.saturating_sub(captured_us));
                        if let Some(slot) = header.mark_long_term {
                            self.long_term[usize::from(slot)] = Some(frame);
                        }
                        if let Some((_, since)) = self.broken_since.take() {
                            self.stats.recoveries += 1;
                            self.stats.recovery_us.push(now_us.saturating_sub(since));
                        }
                    }
                    out.push(decoded);
                }
                Ok(Decoded::BrokenReference { .. }) | Err(_) => {
                    self.stats.decoder_broken += 1;
                    self.mark_broken(frame, now_us);
                }
                Ok(Decoded::Pending) => {}
            }
        }
        out
    }

    fn request(&self) -> RecoveryRequest {
        match self.broken_since {
            None => RecoveryRequest::None,
            Some((first, _)) if self.config.invalidate => RecoveryRequest::Invalidate {
                first: first as u32,
                last: self.highest_frame.max(first) as u32,
            },
            Some(_) => RecoveryRequest::Keyframe,
        }
    }

    fn due_nacks(&mut self, now_us: u64, limit: usize) -> Vec<SeqRange> {
        let rtt = self.rtt_us;
        self.missing.retain(|_, m| {
            now_us.saturating_sub(m.first_seen_us) < MISSING_HORIZON_US
                && !(m.nacks >= MAX_NACKS_PER_PACKET
                    && now_us.saturating_sub(m.last_nack_us) > 2 * rtt)
        });
        if !self.config.nack {
            return Vec::new();
        }
        let mut ranges: Vec<SeqRange> = Vec::new();
        let mut last: Option<u64> = None;
        for (&seq, missing) in &mut self.missing {
            let due = now_us >= missing.first_seen_us + REORDER_TOLERANCE_US
                && missing.nacks < MAX_NACKS_PER_PACKET
                && (missing.nacks == 0 || now_us >= missing.last_nack_us + rtt + 5_000);
            if !due {
                continue;
            }
            let extends = last == Some(seq - 1) && ranges.last().is_some_and(|r| r.len < u16::MAX);
            if extends {
                ranges.last_mut().expect("checked").len += 1;
            } else if ranges.len() >= limit {
                break;
            } else {
                ranges.push(SeqRange {
                    first: seq as u32,
                    len: 1,
                });
            }
            last = Some(seq);
            missing.nacks += 1;
            missing.last_nack_us = now_us;
            self.stats.nacks += 1;
        }
        ranges
    }

    fn nack_due(&self, now_us: u64) -> bool {
        self.config.nack
            && self.missing.values().any(|m| {
                m.nacks < MAX_NACKS_PER_PACKET
                    && now_us >= m.first_seen_us + REORDER_TOLERANCE_US
                    && (m.nacks == 0 || now_us >= m.last_nack_us + self.rtt_us + 5_000)
            })
    }

    /// A feedback datagram, when one is due.
    pub fn poll_feedback(&mut self, now_us: u64) -> Option<Bytes> {
        let since = self
            .last_feedback_us
            .map_or(u64::MAX, |at| now_us.saturating_sub(at));
        let request = self.request();
        let urgent = (request != self.last_request || self.nack_due(now_us)) && since >= 2_000;
        if !(since >= self.config.feedback_interval_us || urgent) {
            return None;
        }
        if self.arrivals.is_empty() && request == RecoveryRequest::None && !self.nack_due(now_us) {
            return None;
        }
        self.last_feedback_us = Some(now_us);
        self.last_request = request;
        let nacks = self.due_nacks(now_us, 32);
        let streams = vec![StreamState {
            stream: self.config.stream,
            generation: self.config.generation,
            last_decoded: self.last_decoded.unwrap_or(0) as u32,
            long_term: self
                .long_term
                .iter()
                .enumerate()
                .filter_map(|(slot, frame)| frame.map(|f| (f, slot as u8)))
                .max()
                .map(|(frame, slot)| (slot, frame as u32)),
            request,
        }];
        let mut feedback = Feedback {
            feedback_seq: self.feedback_seq,
            base_seq: 0,
            reference_us: 0,
            arrivals: Vec::new(),
            nacks,
            streams,
        };
        self.feedback_seq = self.feedback_seq.wrapping_add(1);
        if let Some((&base, &reference)) = self.arrivals.iter().next() {
            feedback.base_seq = base as u32;
            feedback.reference_us = reference as u32;
            let quantize =
                |us: i64| (us + ARRIVAL_TICK_US / 2).div_euclid(ARRIVAL_TICK_US) * ARRIVAL_TICK_US;
            let mut reported = Vec::new();
            let mut previous_tick = 0i64;
            for (&seq, &arrival) in &self.arrivals {
                let index = (seq - base) as usize;
                if index >= MAX_FEEDBACK_SEQS {
                    break;
                }
                let offset = quantize(arrival as i64 - reference as i64);
                let tick = offset / ARRIVAL_TICK_US;
                if (tick - previous_tick).abs() > i64::from(i16::MAX) {
                    break;
                }
                let mut candidate = feedback.clone();
                candidate.arrivals.resize(index, None);
                candidate.arrivals.push(Some(offset));
                if candidate.encoded_len() > self.config.max_datagram {
                    break;
                }
                feedback.arrivals = candidate.arrivals;
                previous_tick = tick;
                reported.push(seq);
            }
            for seq in reported {
                self.arrivals.remove(&seq);
            }
        }
        let mut out = Vec::with_capacity(feedback.encoded_len());
        feedback
            .encode(self.config.max_datagram, &mut out)
            .expect("feedback is sized to fit");
        self.stats.feedbacks += 1;
        Some(Bytes::from(out))
    }

    /// The next time `poll` or `poll_feedback` may have work, for
    /// event-driven callers.
    pub fn next_timeout_us(&self, now_us: u64) -> u64 {
        let mut next = self
            .last_feedback_us
            .map_or(now_us, |at| at + self.config.feedback_interval_us);
        if let Some((_, assembly)) = self.frames.iter().next() {
            next = next.min(assembly.last_arrival_us + self.frame_timeout_us() + 1);
        }
        if let Some(missing) = self.missing.values().next() {
            next = next.min(missing.first_seen_us + REORDER_TOLERANCE_US);
        }
        next.max(now_us + 1)
    }
}
