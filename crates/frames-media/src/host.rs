//! Sending side of one video stream, independent of I/O and threads.
//!
//! The caller drives it with its own clock: [`HostSession::encode`] for each
//! captured frame, [`HostSession::poll_transmit`] for paced datagrams, and
//! [`HostSession::on_datagram`] for receiver feedback. The same code runs in
//! virtual-time simulation and on real hardware.

use crate::api::{
    Chunk, EncoderControl, EncoderFeatures, FrameKind, FrameReference, MediaError, VideoEncoder,
    VideoFrame,
};
use crate::fec;
use crate::rate::{RateConfig, RateController};
use crate::seq::Unwrapper;
use crate::wire::{
    self, MAX_LTR_SLOTS, MEDIA_HEADER_BYTES, MediaFlags, MediaHeader, MediaKind, Packet,
    RecoveryRequest, Reference,
};
use bytes::Bytes;
use std::collections::VecDeque;

/// Frames the encoder can still predict from after an invalidation.
const INVALIDATION_REACH: u64 = 15;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HostConfig {
    pub stream: u32,
    pub generation: u16,
    pub max_datagram: usize,
    /// Parity shards per 32 data shards for predicted frames.
    pub parity_per_32: u8,
    /// Keyframes are larger and costlier to lose.
    pub keyframe_parity_per_32: u8,
    /// Unsent packets of a frame older than this are dropped.
    pub frame_budget_us: u64,
    /// Sent packets are kept this long for retransmission.
    pub retransmit_window_us: u64,
    /// Pacing rate as a multiple of the target bitrate.
    pub pacing_factor: f64,
    pub rate: RateConfig,
    /// Mark a long-term reference every this many frames (0 = never).
    pub long_term_interval: u64,
}

impl HostConfig {
    pub fn new(stream: u32, generation: u16, rate: RateConfig) -> Self {
        Self {
            stream,
            generation,
            max_datagram: 1200,
            parity_per_32: 4,
            keyframe_parity_per_32: 8,
            frame_budget_us: 150_000,
            retransmit_window_us: 250_000,
            pacing_factor: 2.5,
            rate,
            long_term_interval: 30,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostStats {
    pub frames: u64,
    pub keyframes: u64,
    pub packets: u64,
    pub bytes: u64,
    pub retransmissions: u64,
    pub dropped_stale: u64,
    pub invalidations: u64,
    pub long_term_recoveries: u64,
    pub forced_keyframes: u64,
}

#[derive(Clone)]
struct Pending {
    header: MediaHeader,
    shard: Bytes,
    ready_us: u64,
    captured_us: u64,
}

struct Sent {
    seq: u64,
    send_us: u64,
    bytes: usize,
    header: MediaHeader,
    shard: Bytes,
    captured_us: u64,
    retransmitted: bool,
}

pub struct HostSession {
    config: HostConfig,
    next_seq: u64,
    queue: VecDeque<Pending>,
    retransmit: VecDeque<Pending>,
    sent: VecDeque<Sent>,
    rate: RateController,
    budget_bytes: f64,
    last_pace_us: Option<u64>,
    applied_bps: u32,
    control: Option<EncoderControl>,
    handled: Option<(RecoveryRequest, u64)>,
    last_encoded: Option<u64>,
    features: EncoderFeatures,
    slot_frames: [Option<u64>; MAX_LTR_SLOTS as usize],
    confirmed: Option<(u64, u8)>,
    next_slot: u8,
    feedback_unwrap: Unwrapper,
    last_feedback_seq: Option<u32>,
    stats: HostStats,
}

impl HostSession {
    pub fn new(config: HostConfig) -> Self {
        Self {
            rate: RateController::new(config.rate),
            config,
            next_seq: 0,
            queue: VecDeque::new(),
            retransmit: VecDeque::new(),
            sent: VecDeque::new(),
            budget_bytes: 0.0,
            last_pace_us: None,
            applied_bps: 0,
            control: None,
            handled: None,
            last_encoded: None,
            features: EncoderFeatures::empty(),
            slot_frames: [None; MAX_LTR_SLOTS as usize],
            confirmed: None,
            next_slot: 0,
            feedback_unwrap: Unwrapper::new(),
            last_feedback_seq: None,
            stats: HostStats::default(),
        }
    }

    pub fn stats(&self) -> HostStats {
        self.stats
    }

    pub fn target_bps(&self) -> u32 {
        self.rate.target_bps()
    }

    /// Bitrate for the encoder: the target minus FEC and header overhead.
    fn video_bps(&self) -> u32 {
        let parity = 1.0 + f64::from(self.config.parity_per_32) / 32.0;
        let headers = 1.0 + MEDIA_HEADER_BYTES as f64 / self.config.max_datagram as f64;
        (f64::from(self.rate.target_bps()) / (parity * headers)) as u32
    }

    /// Encodes one captured frame and queues its packets.
    pub fn encode(
        &mut self,
        now_us: u64,
        encoder: &mut dyn VideoEncoder,
        frame: &VideoFrame,
    ) -> Result<(), MediaError> {
        let video_bps = self.video_bps();
        let change = f64::from(video_bps) / f64::from(self.applied_bps.max(1));
        if self.applied_bps == 0 || !(0.95..=1.05).contains(&change) {
            encoder.control(EncoderControl::Bitrate(video_bps))?;
            self.applied_bps = video_bps;
        }
        if let Some(control) = self.control.take() {
            encoder.control(control)?;
        }
        let caps = *encoder.caps();
        self.features = caps.features;
        let next = self.last_encoded.map_or(0, |f| f + 1);
        if self.config.long_term_interval > 0
            && caps
                .features
                .contains(EncoderFeatures::LONG_TERM_REFERENCES)
            && caps.long_term_slots >= 2
            && next.is_multiple_of(self.config.long_term_interval)
        {
            // Keep the confirmed slot as the recovery anchor; mark the other.
            let slot = match self.confirmed {
                Some((_, confirmed)) => confirmed ^ 1,
                None => {
                    self.next_slot ^= 1;
                    self.next_slot
                }
            };
            encoder.control(EncoderControl::MarkLongTerm(slot))?;
        }
        let mut chunks = Vec::new();
        encoder.encode(frame, &mut |chunk: Chunk<'_>| {
            chunks.push((
                chunk.frame,
                chunk.slice,
                chunk.slices,
                chunk.kind,
                chunk.reference,
                chunk.mark_long_term,
                Bytes::copy_from_slice(chunk.data),
                chunk.captured_us,
                chunk.ready_us.max(now_us),
            ));
        })?;
        for (number, slice, slices, kind, reference, mark, data, captured_us, ready_us) in chunks {
            if slice == 0 {
                self.stats.frames += 1;
                if kind == FrameKind::Keyframe {
                    self.stats.keyframes += 1;
                    self.slot_frames = [None; MAX_LTR_SLOTS as usize];
                    self.confirmed = None;
                }
                self.last_encoded = Some(number);
            }
            if let Some(slot) = mark {
                self.slot_frames[usize::from(slot)] = Some(number);
                if self.confirmed.is_some_and(|(_, s)| s == slot) {
                    self.confirmed = None;
                }
            }
            self.packetize(
                number,
                slice,
                slices,
                kind,
                reference,
                mark,
                &data,
                captured_us,
                ready_us,
            )?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn packetize(
        &mut self,
        number: u64,
        slice: u8,
        slices: u8,
        kind: FrameKind,
        reference: FrameReference,
        mark: Option<u8>,
        data: &[u8],
        captured_us: u64,
        ready_us: u64,
    ) -> Result<(), MediaError> {
        if data.is_empty() {
            return Err(MediaError::Backend(
                "encoder produced an empty slice".into(),
            ));
        }
        let parity = if kind == FrameKind::Keyframe {
            self.config.keyframe_parity_per_32
        } else {
            self.config.parity_per_32
        };
        let layout = fec::Layout::new(
            data.len(),
            self.config.max_datagram - MEDIA_HEADER_BYTES,
            parity,
        );
        let reference = match reference {
            FrameReference::None => Reference::None,
            FrameReference::Frame(r) => {
                let back = number
                    .checked_sub(r)
                    .filter(|b| (1..=u64::from(u16::MAX)).contains(b));
                Reference::Back(
                    back.ok_or(MediaError::Backend("reference out of range".into()))? as u16,
                )
            }
            FrameReference::LongTerm(slot) => {
                let back = self
                    .slot_frames
                    .get(usize::from(slot))
                    .copied()
                    .flatten()
                    .and_then(|frame| number.checked_sub(frame))
                    .filter(|b| (1..=u64::from(u16::MAX)).contains(b))
                    .ok_or(MediaError::Backend(
                        "long-term reference out of range".into(),
                    ))?;
                Reference::LongTerm {
                    slot,
                    back: back as u16,
                }
            }
        };
        let mut flags = MediaFlags::empty();
        match kind {
            FrameKind::Keyframe => flags = flags.with(MediaFlags::KEYFRAME),
            FrameKind::Refresh { complete } => {
                flags = flags.with(MediaFlags::INTRA_REFRESH);
                if complete {
                    flags = flags.with(MediaFlags::RECOVERY_POINT);
                }
            }
            FrameKind::Predicted => {
                if matches!(reference, Reference::LongTerm { .. }) {
                    flags = flags.with(MediaFlags::RECOVERY_POINT);
                }
            }
        }
        if slice + 1 == slices {
            flags = flags.with(MediaFlags::END_OF_FRAME);
        }
        let blocks = fec::encode(data, &layout);
        for (b, (block, encoded)) in layout.blocks.iter().zip(blocks).enumerate() {
            for (shard_index, shard) in encoded.shards.into_iter().enumerate() {
                self.queue.push_back(Pending {
                    header: MediaHeader {
                        kind: MediaKind::Video,
                        flags,
                        stream: self.config.stream,
                        generation: self.config.generation,
                        seq: 0,
                        send_us: 0,
                        frame: number as u32,
                        capture_us: captured_us as u32,
                        slice,
                        slices,
                        reference,
                        mark_long_term: mark,
                        slice_len: data.len() as u32,
                        block: b as u8,
                        blocks: layout.blocks.len() as u8,
                        shard: shard_index as u8,
                        data_shards: block.data,
                        parity_shards: block.parity,
                        shard_len: layout.shard_len,
                    },
                    shard: Bytes::from(shard),
                    ready_us,
                    captured_us,
                });
            }
        }
        Ok(())
    }

    fn pacing_bytes_per_us(&self) -> f64 {
        f64::from(self.rate.target_bps()) * self.config.pacing_factor / 8.0 / 1e6
    }

    fn refill(&mut self, now_us: u64) {
        let rate = self.pacing_bytes_per_us();
        let elapsed = self.last_pace_us.map_or(0, |at| now_us.saturating_sub(at));
        self.last_pace_us = Some(now_us);
        // At most ~5 ms of burst, and at least one full datagram.
        let cap = (rate * 5_000.0).max(self.config.max_datagram as f64);
        self.budget_bytes = (self.budget_bytes + rate * elapsed as f64).min(cap);
    }

    fn drop_stale(&mut self, now_us: u64) {
        let budget = self.config.frame_budget_us;
        for queue in [&mut self.queue, &mut self.retransmit] {
            let before = queue.len();
            queue.retain(|p| now_us.saturating_sub(p.captured_us) <= budget);
            self.stats.dropped_stale += (before - queue.len()) as u64;
        }
    }

    /// The next datagram to send now, if pacing allows one.
    pub fn poll_transmit(&mut self, now_us: u64) -> Option<Bytes> {
        self.drop_stale(now_us);
        self.refill(now_us);
        let from_retransmit = !self.retransmit.is_empty();
        let next = if from_retransmit {
            self.retransmit.front()
        } else {
            self.queue.front().filter(|p| p.ready_us <= now_us)
        }?;
        let size = MEDIA_HEADER_BYTES + next.shard.len();
        if self.budget_bytes < size as f64 {
            return None;
        }
        let mut pending = if from_retransmit {
            self.retransmit.pop_front()
        } else {
            self.queue.pop_front()
        }
        .expect("peeked");
        self.budget_bytes -= size as f64;
        let seq = self.next_seq;
        self.next_seq += 1;
        pending.header.seq = seq as u32;
        pending.header.send_us = now_us as u32;
        if from_retransmit {
            pending.header.flags = pending.header.flags.with(MediaFlags::RETRANSMISSION);
            self.stats.retransmissions += 1;
        }
        let mut datagram = Vec::with_capacity(size);
        pending
            .header
            .encode(&pending.shard, &mut datagram)
            .expect("the host only builds valid headers");
        self.stats.packets += 1;
        self.stats.bytes += datagram.len() as u64;
        self.sent.push_back(Sent {
            seq,
            send_us: now_us,
            bytes: datagram.len(),
            header: pending.header,
            shard: pending.shard,
            captured_us: pending.captured_us,
            retransmitted: false,
        });
        while self
            .sent
            .front()
            .is_some_and(|s| now_us.saturating_sub(s.send_us) > self.config.retransmit_window_us)
        {
            self.sent.pop_front();
        }
        Some(Bytes::from(datagram))
    }

    /// When the next datagram could be sent, for event-driven callers.
    pub fn next_transmit_us(&self, now_us: u64) -> Option<u64> {
        let next = self.retransmit.front().or_else(|| self.queue.front())?;
        let size = (MEDIA_HEADER_BYTES + next.shard.len()) as f64;
        let rate = self.pacing_bytes_per_us().max(1e-6);
        let wait = ((size - self.budget_bytes).max(0.0) / rate).ceil() as u64;
        Some(
            now_us
                .saturating_add(wait)
                .max(next.ready_us)
                .max(now_us + 1),
        )
    }

    fn sent_index(&self, seq: u64) -> Option<usize> {
        let first = self.sent.front()?.seq;
        let index = seq.checked_sub(first)? as usize;
        (index < self.sent.len()).then_some(index)
    }

    /// Handles receiver feedback.
    pub fn on_datagram(&mut self, now_us: u64, datagram: &[u8], rtt_us: u64) {
        let Ok(Packet::Feedback(feedback)) = wire::parse(datagram) else {
            return;
        };
        if self
            .last_feedback_seq
            .is_some_and(|last| feedback.feedback_seq.wrapping_sub(last) as i32 <= 0)
        {
            return; // duplicate or reordered older report
        }
        self.last_feedback_seq = Some(feedback.feedback_seq);
        if self.next_seq > 0 {
            self.feedback_unwrap = Unwrapper::starting_at(self.next_seq - 1);
        }
        let base = self.feedback_unwrap.unwrap(feedback.base_seq);
        let highest_received = feedback.arrivals.iter().rposition(Option::is_some);
        let (mut received, mut lost) = (0, 0);
        for (i, arrival) in feedback.arrivals.iter().enumerate() {
            let Some(index) = self.sent_index(base + i as u64) else {
                continue;
            };
            match arrival {
                Some(offset) => {
                    received += 1;
                    let sent = &self.sent[index];
                    let arrival_us = i64::from(feedback.reference_us) + offset;
                    self.rate.on_packet(sent.send_us, arrival_us, sent.bytes);
                }
                None if highest_received.is_some_and(|h| i < h) => lost += 1,
                None => {}
            }
        }
        self.rate.on_loss(received, lost);
        self.rate.update(now_us, rtt_us);

        for range in &feedback.nacks {
            let first = self.feedback_unwrap.peek(range.first);
            for seq in first..first + u64::from(range.len) {
                let Some(index) = self.sent_index(seq) else {
                    continue;
                };
                let sent = &mut self.sent[index];
                let deadline = sent.captured_us + self.config.frame_budget_us;
                if sent.retransmitted || now_us + rtt_us / 2 > deadline {
                    continue;
                }
                sent.retransmitted = true;
                self.retransmit.push_back(Pending {
                    header: sent.header,
                    shard: sent.shard.clone(),
                    ready_us: now_us,
                    captured_us: sent.captured_us,
                });
            }
        }

        for state in &feedback.streams {
            if state.stream != self.config.stream || state.generation != self.config.generation {
                continue;
            }
            // Only a slot the receiver reports holding, with the same frame
            // the encoder keeps there, is safe to recover from.
            self.confirmed = state.long_term.and_then(|(slot, frame)| {
                let frame = self.frame_near(frame);
                (self.slot_frames.get(usize::from(slot)).copied().flatten() == Some(frame))
                    .then_some((frame, slot))
            });
            self.recover(now_us, rtt_us, state.request);
        }
    }

    /// Maps a 32-bit wire frame number near the newest encoded frame.
    fn frame_near(&self, frame: u32) -> u64 {
        Unwrapper::starting_at(self.last_encoded.unwrap_or(0)).unwrap(frame)
    }

    fn recover(&mut self, now_us: u64, rtt_us: u64, request: RecoveryRequest) {
        if request == RecoveryRequest::None {
            self.handled = None;
            return;
        }
        // The receiver repeats a request in every report until it recovers
        // (with a growing `last`); act again for the same loss episode only
        // if the first response had time to arrive, even behind a queue.
        let episode = |request: RecoveryRequest| match request {
            RecoveryRequest::Invalidate { first, .. } => Some(first),
            _ => None,
        };
        if let Some((previous, at)) = self.handled
            && episode(previous) == episode(request)
            && std::mem::discriminant(&previous) == std::mem::discriminant(&request)
            && now_us.saturating_sub(at) < 2 * rtt_us + 150_000
        {
            return;
        }
        self.handled = Some((request, now_us));
        let newest = self.last_encoded.unwrap_or(0);
        self.control = Some(match request {
            RecoveryRequest::Keyframe => {
                self.stats.forced_keyframes += 1;
                EncoderControl::ForceKeyframe
            }
            RecoveryRequest::Invalidate { first, last } => {
                let first = self.frame_near(first);
                let last = self.frame_near(last).max(first);
                if self.features.contains(EncoderFeatures::INVALIDATE)
                    && newest.saturating_sub(first) < INVALIDATION_REACH
                {
                    self.stats.invalidations += 1;
                    EncoderControl::Invalidate {
                        first,
                        last: last.max(newest),
                    }
                } else if let Some((frame, slot)) = self.confirmed
                    && frame < first
                {
                    self.stats.long_term_recoveries += 1;
                    EncoderControl::UseLongTerm(slot)
                } else {
                    self.stats.forced_keyframes += 1;
                    EncoderControl::ForceKeyframe
                }
            }
            RecoveryRequest::None => unreachable!("handled above"),
        });
    }
}
