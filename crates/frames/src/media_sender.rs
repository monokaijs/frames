//! One deadline-aware datagram admission queue for the whole connection.
use frames_core::{
    media::{self, EncodedFrame, FecProfile},
    model::WindowId,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, oneshot};

#[derive(Clone)]
pub struct MediaSender(Arc<Owner>);
pub struct MediaRegistration {
    sender: MediaSender,
    window: WindowId,
}
impl Drop for MediaRegistration {
    fn drop(&mut self) {
        self.sender.remove(self.window);
    }
}
struct Owner {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct Shared {
    queue: Mutex<Vec<Work>>,
    wake: Notify,
    budgets: Mutex<HashMap<WindowId, StreamBudget>>,
    limit_bps: AtomicU32,
    overhead: Mutex<HashMap<WindowId, WireOverhead>>,
    wire_bps: AtomicU32,
    queue_micros: AtomicU32,
    late: AtomicU64,
    bytes: AtomicU64,
    bulk_next: tokio::sync::Mutex<tokio::time::Instant>,
    loss_until: Mutex<Option<std::time::Instant>>,
    timings: Mutex<SendTimings>,
}
#[derive(Default)]
struct SendTimings {
    fec: media::LatencyHistogram,
    admission: media::LatencyHistogram,
    queue_age: media::LatencyHistogram,
}
#[derive(Clone, Copy)]
struct StreamBudget {
    bitrate: u32,
    parity: u8,
    focused: bool,
}
impl StreamBudget {
    fn wire(self) -> u64 {
        self.bitrate as u64 * (32 + self.parity as u64) / 32
    }
    fn weight(self) -> u64 {
        if self.focused { 4 } else { 1 }
    }
}
fn allocated_wire(
    budgets: &HashMap<WindowId, StreamBudget>,
    window: WindowId,
    available: u64,
) -> u64 {
    let mut active: Vec<_> = budgets
        .iter()
        .filter(|(id, _)| **id != media::AUDIO_WINDOW)
        .map(|(&id, &b)| (id, b.wire(), b.weight()))
        .collect();
    let mut remaining = available;
    loop {
        let weights = active.iter().map(|v| v.2).sum::<u64>();
        if weights == 0 {
            return 0;
        }
        if let Some(index) = active.iter().position(|v| v.1 <= remaining * v.2 / weights) {
            let (id, need, _) = active.swap_remove(index);
            if id == window {
                return need;
            }
            remaining = remaining.saturating_sub(need);
        } else {
            return active
                .iter()
                .find(|v| v.0 == window)
                .map_or(0, |v| remaining * v.2 / weights);
        }
    }
}
fn pacing_rate(budgets: &HashMap<WindowId, StreamBudget>, limit: u32) -> u32 {
    // Leave room for reliable input/control and transport framing. Audio and
    // parity use this same media pace; bulk traffic has its own shared slice.
    if limit != 0 {
        return (limit as u64 * 95 / 100) as u32;
    }
    if budgets.keys().all(|w| *w == media::AUDIO_WINDOW) {
        17_000_000
    } else {
        (budgets.values().map(|b| b.wire() * 5 / 4).sum::<u64>()).clamp(2_000_000, 200_000_000)
            as u32
    }
}
// Charge the measured extra bytes per second, rather than multiplying the
// entire encoder budget by a tiny idle picture's header/padding ratio.
struct WireOverhead {
    started: std::time::Instant,
    bytes: f64,
    bps: f64,
}
impl WireOverhead {
    fn new(now: std::time::Instant) -> Self {
        Self {
            started: now,
            bytes: 0.0,
            bps: 0.0,
        }
    }
    fn record(&mut self, extra_bytes: f64, now: std::time::Instant) {
        self.bytes += extra_bytes;
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= Duration::from_millis(250) {
            let sample = self.bytes * 8.0 / elapsed.as_secs_f64();
            self.bps = self.bps * 0.5 + sample * 0.5;
            self.bytes = 0.0;
            self.started = now;
        }
    }
    fn target(&self, bitrate: u32, now: std::time::Instant) -> u32 {
        let overhead = if now.saturating_duration_since(self.started) > Duration::from_secs(2) {
            0
        } else {
            self.bps.ceil() as u32
        };
        bitrate.saturating_sub(overhead).max(128_000)
    }
}
struct Work {
    window: WindowId,
    packets: std::collections::VecDeque<bytes::Bytes>,
    deadline: tokio::time::Instant,
    focused: bool,
    submitted: tokio::time::Instant,
    done: oneshot::Sender<bool>,
}
// Audio preempts video. A small deadline credit favors the focused window,
// without allowing it to starve an older background picture.
fn select_work(queue: &[Work], last_window: Option<WindowId>, consecutive: usize) -> Option<usize> {
    queue
        .iter()
        .enumerate()
        .min_by_key(|(_, w)| {
            let credit = if w.focused {
                Duration::from_millis(2)
            } else {
                Duration::ZERO
            };
            let quantum =
                if last_window == Some(w.window) && consecutive >= if w.focused { 8 } else { 2 } {
                    Duration::from_millis(1)
                } else {
                    Duration::ZERO
                };
            (
                w.window != media::AUDIO_WINDOW,
                w.deadline - credit + quantum,
                w.submitted,
            )
        })
        .map(|(i, _)| i)
}
impl MediaSender {
    pub fn new(connection: quinn::Connection) -> Self {
        media::prewarm_fec();
        let shared = Arc::new(Shared {
            queue: Mutex::new(Vec::new()),
            wake: Notify::new(),
            budgets: Mutex::new(HashMap::new()),
            limit_bps: AtomicU32::new(0),
            overhead: Mutex::new(HashMap::new()),
            wire_bps: AtomicU32::new(17_000_000),
            queue_micros: AtomicU32::new(0),
            late: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            loss_until: Mutex::new(None),
            timings: Mutex::default(),
            bulk_next: tokio::sync::Mutex::new(tokio::time::Instant::now()),
        });
        let task = tokio::spawn(run(connection, shared.clone()));
        Self(Arc::new(Owner { shared, task }))
    }
    pub fn register(&self, window: WindowId) -> MediaRegistration {
        self.set_budget(
            window,
            if window == media::AUDIO_WINDOW {
                128_000
            } else {
                12_000_000
            },
            if window == media::AUDIO_WINDOW { 0 } else { 4 },
            false,
        );
        MediaRegistration {
            sender: self.clone(),
            window,
        }
    }
    pub fn encoder_target(&self, window: WindowId, bitrate: u32) -> u32 {
        let limit = self.0.shared.limit_bps.load(Ordering::Relaxed);
        let bitrate = if limit == 0 {
            bitrate
        } else {
            let budgets = self.0.shared.budgets.lock().unwrap();
            let parity = budgets.get(&window).map_or(4, |b| b.parity) as u64;
            let available = (limit as u64 * 95 / 100).saturating_sub(256_000);
            let allocated = allocated_wire(&budgets, window, available) * 32 / (32 + parity);
            bitrate.min(allocated.min(u32::MAX as u64) as u32)
        };
        let overhead = self.0.shared.overhead.lock().unwrap();
        overhead
            .get(&window)
            .map_or(bitrate, |sample| {
                sample.target(bitrate, std::time::Instant::now())
            })
            .clamp(128_000, 100_000_000)
    }
    pub fn set_limit_mbps(&self, mbps: u32) {
        let limit = mbps.min(200) * 1_000_000;
        self.0.shared.limit_bps.store(limit, Ordering::Relaxed);
        let budgets = self.0.shared.budgets.lock().unwrap();
        self.0
            .shared
            .wire_bps
            .store(pacing_rate(&budgets, limit), Ordering::Relaxed);
        self.0.shared.wake.notify_one();
    }
    pub fn set_budget(&self, window: WindowId, bitrate: u32, parity: u8, focused: bool) {
        let mut budgets = self.0.shared.budgets.lock().unwrap();
        budgets.insert(
            window,
            StreamBudget {
                bitrate,
                parity,
                focused,
            },
        );
        self.0.shared.wire_bps.store(
            pacing_rate(&budgets, self.0.shared.limit_bps.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
    }
    pub fn note_loss(&self) {
        *self.0.shared.loss_until.lock().unwrap() =
            Some(std::time::Instant::now() + Duration::from_secs(5));
    }
    pub fn audio_redundancy(&self) -> bool {
        self.0
            .shared
            .loss_until
            .lock()
            .unwrap()
            .is_some_and(|until| until > std::time::Instant::now())
    }
    pub fn remove(&self, window: WindowId) {
        self.0.shared.overhead.lock().unwrap().remove(&window);
        let mut budgets = self.0.shared.budgets.lock().unwrap();
        budgets.remove(&window);
        let bps = pacing_rate(&budgets, self.0.shared.limit_bps.load(Ordering::Relaxed));
        self.0.shared.wire_bps.store(bps, Ordering::Relaxed);
    }
    pub fn report_timings(&self) {
        let timings = self.0.shared.timings.lock().unwrap();
        tracing::info!(
            fec_p50_us = timings.fec.percentile(50),
            fec_p95_us = timings.fec.percentile(95),
            fec_p99_us = timings.fec.percentile(99),
            admission_p50_us = timings.admission.percentile(50),
            admission_p95_us = timings.admission.percentile(95),
            admission_p99_us = timings.admission.percentile(99),
            queue_p95_us = timings.queue_age.percentile(95),
            late = self.late(),
            bytes = self.bytes(),
            "media sender timing"
        );
    }
    pub fn queue_micros(&self) -> u32 {
        self.0.shared.queue_micros.load(Ordering::Relaxed)
    }
    pub fn late(&self) -> u64 {
        self.0.shared.late.load(Ordering::Relaxed)
    }
    pub fn bytes(&self) -> u64 {
        self.0.shared.bytes.load(Ordering::Relaxed)
    }
    pub async fn throttle_bulk(&self, bytes: usize) {
        let video = self
            .0
            .shared
            .budgets
            .lock()
            .unwrap()
            .keys()
            .any(|w| *w != media::AUDIO_WINDOW);
        let bps = self.0.shared.wire_bps.load(Ordering::Relaxed);
        let rate = if video {
            bps.max(2_000_000) as f64 * 0.1
        } else {
            bps.max(17_000_000) as f64 * 0.9
        };
        let when = {
            let mut next = self.0.shared.bulk_next.lock().await;
            let now = tokio::time::Instant::now();
            let when = (*next).max(now);
            *next = when + Duration::from_secs_f64(bytes as f64 * 8.0 / rate);
            when
        };
        tokio::time::sleep_until(when).await;
    }
    pub async fn send(
        &self,
        frame: EncodedFrame,
        mtu: usize,
        fec: FecProfile,
        captured_us: u64,
        deadline: tokio::time::Instant,
        focused: bool,
    ) -> anyhow::Result<bool> {
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        let window = frame.window;
        let payload = frame.bytes.len();
        let (packets, cost) = tokio::task::spawn_blocking(move || {
            let start = std::time::Instant::now();
            let packets = media::fragment_fec(&frame, mtu, fec, captured_us)?;
            Ok::<_, anyhow::Error>((packets, start.elapsed().as_micros() as u64))
        })
        .await??;
        self.0.shared.timings.lock().unwrap().fec.record(cost);
        let wire = packets.iter().map(Vec::len).sum::<usize>();
        // Controller budgets already include nominal parity. Charge the extra
        // padding, small-block rounding, and every media header to the encoder.
        let extra =
            (wire as f64 - payload as f64 * (1.0 + fec.parity_per_32 as f64 / 32.0)).max(0.0);
        {
            let mut overhead = self.0.shared.overhead.lock().unwrap();
            let now = std::time::Instant::now();
            overhead
                .entry(window)
                .or_insert_with(|| WireOverhead::new(now))
                .record(extra, now);
        }
        let (done, result) = oneshot::channel();
        {
            let mut queue = self.0.shared.queue.lock().unwrap();
            let retained: usize = queue.iter().flat_map(|w| &w.packets).map(|p| p.len()).sum();
            if queue.len() >= 12
                || retained + packets.iter().map(Vec::len).sum::<usize>()
                    > media::MAX_REASSEMBLY_BYTES
            {
                return Ok(false);
            }
            queue.push(Work {
                window,
                packets: packets.into_iter().map(bytes::Bytes::from).collect(),
                deadline,
                focused,
                submitted: tokio::time::Instant::now(),
                done,
            });
        }
        self.0.shared.wake.notify_one();
        Ok(tokio::time::timeout_at(deadline, result)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(false))
    }
}
struct SenderLiveness(quinn::Connection);
impl Drop for SenderLiveness {
    fn drop(&mut self) {
        self.0.close(1u32.into(), b"media sender stopped");
    }
}
async fn run(connection: quinn::Connection, shared: Arc<Shared>) {
    let _liveness = SenderLiveness(connection.clone());
    #[cfg(windows)]
    let _timer = TimerResolution::new();
    let mut next_send = tokio::time::Instant::now();
    let mut last_window = None;
    let mut consecutive = 0;
    loop {
        let now = tokio::time::Instant::now();
        {
            let mut queue = shared.queue.lock().unwrap();
            let mut i = 0;
            while i < queue.len() {
                if queue[i].deadline <= now || queue[i].done.is_closed() {
                    let expired = queue.swap_remove(i);
                    shared.late.fetch_add(1, Ordering::Relaxed);
                    let _ = expired.done.send(false);
                } else {
                    i += 1;
                }
            }
        }
        if shared.queue.lock().unwrap().is_empty() {
            shared.queue_micros.store(0, Ordering::Relaxed);
            tokio::select! { _ = connection.closed() => break, _ = shared.wake.notified() => {} }
            continue;
        }
        if next_send > now {
            tokio::select! { _ = connection.closed() => break, _ = tokio::time::sleep_until(next_send) => {}, _ = shared.wake.notified() => continue }
        }
        let bps = shared.wire_bps.load(Ordering::Relaxed) as usize;
        let cap = (bps / 8 / 200).clamp(1200, media::DATAGRAM_SEND_BUFFER_BYTES);
        let queued = media::DATAGRAM_SEND_BUFFER_BYTES
            .saturating_sub(connection.datagram_send_buffer_space());
        if queued + 1200 > cap {
            shared.queue_micros.store(
                (queued * 8_000_000 / bps.max(1)).min(u32::MAX as usize) as u32,
                Ordering::Relaxed,
            );
            tokio::select! { _ = connection.closed() => break, _ = tokio::time::sleep(Duration::from_millis(1)) => {} }
            continue;
        }
        // At most four packets per wake; reselect after each packet so audio can preempt.
        for _ in 0..4 {
            let selected = {
                let mut queue = shared.queue.lock().unwrap();
                let index = select_work(&queue, last_window, consecutive);
                index.and_then(|i| {
                    let w = &mut queue[i];
                    w.packets
                        .pop_front()
                        .map(|p| (i, p, w.window, w.deadline, w.submitted))
                })
            };
            let Some((index, packet, window, deadline, _submitted)) = selected else {
                break;
            };
            let len = packet.len();
            let result =
                tokio::time::timeout_at(deadline, connection.send_datagram_wait(packet)).await;
            let success = matches!(result, Ok(Ok(())));
            if let Ok(Err(error)) = result {
                if matches!(
                    error,
                    quinn::SendDatagramError::ConnectionLost(_)
                        | quinn::SendDatagramError::Disabled
                        | quinn::SendDatagramError::UnsupportedByPeer
                ) {
                    tracing::debug!(%error, "media sender ended");
                    return;
                }
                // An MTU change retires this picture. Its caller forces a fresh
                // keyframe using the connection's new datagram limit.
                tracing::debug!(%error, "retiring media picture after MTU change");
            }
            let mut queue = shared.queue.lock().unwrap();
            // The sender is the only remover; producers only append.
            if !success || queue[index].packets.is_empty() {
                let work = queue.remove(index);
                shared
                    .timings
                    .lock()
                    .unwrap()
                    .admission
                    .record(work.submitted.elapsed().as_micros() as u64);
                let _ = work.done.send(success);
                if !success {
                    shared.late.fetch_add(1, Ordering::Relaxed);
                }
            }
            drop(queue);
            if last_window == Some(window) {
                consecutive += 1;
            } else {
                consecutive = 1;
                last_window = Some(window);
            }
            if success {
                shared.bytes.fetch_add(len as u64, Ordering::Relaxed);
            }
            let retained = media::DATAGRAM_SEND_BUFFER_BYTES
                .saturating_sub(connection.datagram_send_buffer_space());
            let queue_age = (retained * 8_000_000 / bps.max(1)).min(u32::MAX as usize) as u32;
            shared.queue_micros.store(queue_age, Ordering::Relaxed);
            shared
                .timings
                .lock()
                .unwrap()
                .queue_age
                .record(queue_age as u64);
            next_send = next_send.max(tokio::time::Instant::now() - Duration::from_millis(5))
                + Duration::from_secs_f64(len as f64 * 8.0 / bps.max(1) as f64);
            if media::DATAGRAM_SEND_BUFFER_BYTES
                .saturating_sub(connection.datagram_send_buffer_space())
                + 1200
                > cap
            {
                break;
            }
        }
    }
}
#[cfg(windows)]
struct TimerResolution;
#[cfg(windows)]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}
#[cfg(windows)]
impl TimerResolution {
    fn new() -> Self {
        unsafe {
            timeBeginPeriod(1);
        }
        Self
    }
}
#[cfg(windows)]
impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_limit_is_shared_with_focus_weight_and_reclaims_small_demands() {
        let focused = WindowId {
            id: 1,
            generation: 1,
        };
        let background = WindowId {
            id: 2,
            generation: 1,
        };
        let mut budgets = HashMap::from([
            (
                focused,
                StreamBudget {
                    bitrate: 50_000_000,
                    parity: 4,
                    focused: true,
                },
            ),
            (
                background,
                StreamBudget {
                    bitrate: 50_000_000,
                    parity: 4,
                    focused: false,
                },
            ),
        ]);
        let limit = 50_000_000;
        let available = (pacing_rate(&budgets, limit) as u64).saturating_sub(256_000);
        let foreground = allocated_wire(&budgets, focused, available);
        let other = allocated_wire(&budgets, background, available);
        assert_eq!(foreground, other * 4);
        assert!(foreground + other <= available);
        budgets.get_mut(&background).unwrap().bitrate = 1_000_000;
        assert_eq!(allocated_wire(&budgets, background, available), 1_125_000);
        assert_eq!(
            allocated_wire(&budgets, focused, available),
            available - 1_125_000
        );
        budgets.remove(&background);
        assert_eq!(allocated_wire(&budgets, focused, available), available);
    }
    #[test]
    fn every_custom_limit_bounds_combined_video_parity_and_audio() {
        for mbps in [1, 5, 10, 25, 50, 100, 200] {
            for streams in 1..=32 {
                let budgets: HashMap<_, _> = (0..streams)
                    .map(|id| {
                        (
                            WindowId {
                                id: id + 1,
                                generation: 1,
                            },
                            StreamBudget {
                                bitrate: ((id % 4) + 1) as u32 * 12_000_000,
                                parity: if id % 2 == 0 { 8 } else { 4 },
                                focused: id == 0,
                            },
                        )
                    })
                    .collect();
                let pace = pacing_rate(&budgets, mbps * 1_000_000) as u64;
                let available = pace - 256_000;
                let total = budgets
                    .keys()
                    .map(|&window| allocated_wire(&budgets, window, available))
                    .sum::<u64>();
                assert!(total + 256_000 <= pace && pace < mbps as u64 * 1_000_000);
            }
        }
    }
    #[test]
    fn tiny_idle_updates_charge_bytes_per_second_without_collapsing_quality() {
        let now = std::time::Instant::now();
        let mut overhead = WireOverhead::new(now);
        // A tiny picture can have a huge header/parity ratio, while using only
        // 16 kbps of overhead. It must not divide the entire 12 Mbps budget.
        for second in 1..=10 {
            overhead.record(2000.0, now + Duration::from_secs(second));
        }
        assert!(overhead.target(12_000_000, now + Duration::from_secs(10)) > 11_980_000);
        assert_eq!(
            overhead.target(12_000_000, now + Duration::from_secs(13)),
            12_000_000
        );
    }
    #[test]
    fn sustained_header_and_padding_bytes_are_charged_to_the_encoder_budget() {
        let now = std::time::Instant::now();
        let mut overhead = WireOverhead::new(now);
        for n in 1..=120 {
            overhead.record(1000.0, now + Duration::from_micros(n * 16_667));
        }
        let target = overhead.target(12_000_000, now + Duration::from_secs(2));
        assert!(
            (11_510_000..11_550_000).contains(&target),
            "target={target}"
        );
    }
    fn work(id: u64, focused: bool, ms: u64, now: tokio::time::Instant) -> Work {
        Work {
            window: WindowId { id, generation: 1 },
            packets: Default::default(),
            deadline: now + Duration::from_millis(ms),
            focused,
            submitted: now,
            done: oneshot::channel().0,
        }
    }
    #[test]
    fn audio_preempts_and_focus_cannot_starve_earlier_video() {
        let now = tokio::time::Instant::now();
        let mut queue = vec![work(1, true, 25, now), work(2, false, 10, now)];
        assert_eq!(select_work(&queue, None, 0), Some(1));
        queue[1].deadline = now + Duration::from_millis(24);
        assert_eq!(select_work(&queue, None, 0), Some(0));
        queue.push(work(media::AUDIO_WINDOW.id, false, 50, now));
        queue[2].window = media::AUDIO_WINDOW;
        assert_eq!(select_work(&queue, None, 0), Some(2));
    }
    #[test]
    fn focused_quantum_yields_to_a_peer_with_a_similar_deadline() {
        let now = tokio::time::Instant::now();
        let mut queue = vec![work(1, true, 20, now), work(2, true, 20, now)];
        queue[1].submitted += Duration::from_micros(1);
        assert_eq!(select_work(&queue, Some(queue[0].window), 7), Some(0));
        assert_eq!(select_work(&queue, Some(queue[0].window), 8), Some(1));
    }
}
