//! Delay-based rate control with a loss ceiling.
//!
//! The receiver reports when each packet arrived (`wire::Feedback`). Packets
//! sent within [`BURST_US`] of each other form a group. For consecutive
//! groups the controller compares how far apart they arrived with how far
//! apart they were sent; the difference accumulates into a one-way queueing
//! delay estimate whose least-squares trend over recent groups says whether
//! a queue is building (overuse), draining (underuse), or stable. This reacts
//! to queue growth before packets are lost, which loss-based congestion
//! control (and QUIC's smoothed RTT on a relayed path) does not.
//!
//! The rate then follows AIMD: on overuse it drops to 85% of what the
//! receiver actually got; when stable it grows multiplicatively while far
//! from the last congestion point and additively near it, and never runs
//! more than 50% ahead of the delivered rate. Loss above 10% cuts it further.

use std::collections::VecDeque;

const BURST_US: u64 = 5_000;
const TREND_WINDOW: usize = 20;
const TREND_GAIN: f64 = 4.0;
const SMOOTHING: f64 = 0.9;
const OVERUSE_TIME_US: u64 = 10_000;
const THRESHOLD_UP: f64 = 0.0087;
const THRESHOLD_DOWN: f64 = 0.039;
const ACKED_WINDOW_US: i64 = 500_000;
const ACKED_MIN_SPAN_US: i64 = 200_000;
/// A standing queue above this is overuse even when the delay trend is flat.
const QUEUE_LIMIT_MS: f64 = 30.0;
/// The empty-queue one-way delay is the minimum over two windows this long,
/// so clock drift between the machines cannot accumulate.
const BASE_WINDOW_US: i64 = 5_000_000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RateConfig {
    pub min_bps: u32,
    pub max_bps: u32,
    pub start_bps: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Normal,
    Overuse,
    Underuse,
}

#[derive(Clone, Copy, Debug)]
struct Group {
    first_send_us: u64,
    last_send_us: u64,
    last_arrival_us: i64,
}

pub struct RateController {
    config: RateConfig,
    target: f64,
    group: Option<Group>,
    previous: Option<Group>,
    first_arrival_us: Option<i64>,
    accumulated_ms: f64,
    smoothed_ms: f64,
    history: VecDeque<(f64, f64)>,
    deltas: u32,
    threshold_ms: f64,
    last_threshold_us: Option<i64>,
    overuse_since: Option<i64>,
    previous_trend: f64,
    signal: Signal,
    acked: VecDeque<(i64, usize)>,
    congestion_bps: Option<f64>,
    last_update_us: Option<u64>,
    last_decrease_us: Option<u64>,
    loss: f64,
    last_loss_cut_us: Option<u64>,
    base: Option<(i64, i64, i64)>,
    queue_ms: f64,
}

impl RateController {
    pub fn new(config: RateConfig) -> Self {
        Self {
            config,
            target: f64::from(config.start_bps),
            group: None,
            previous: None,
            first_arrival_us: None,
            accumulated_ms: 0.0,
            smoothed_ms: 0.0,
            history: VecDeque::new(),
            deltas: 0,
            threshold_ms: 12.5,
            last_threshold_us: None,
            overuse_since: None,
            previous_trend: 0.0,
            signal: Signal::Normal,
            acked: VecDeque::new(),
            congestion_bps: None,
            last_update_us: None,
            last_decrease_us: None,
            loss: 0.0,
            last_loss_cut_us: None,
            base: None,
            queue_ms: 0.0,
        }
    }

    pub fn target_bps(&self) -> u32 {
        self.target as u32
    }

    pub fn signal(&self) -> Signal {
        self.signal
    }

    /// Smoothed queueing delay above the empty-queue baseline.
    pub fn queue_ms(&self) -> f64 {
        self.queue_ms
    }

    fn track_queue(&mut self, send_us: u64, arrival_us: i64) {
        let one_way = arrival_us - send_us as i64;
        // (window start, minimum in this window, minimum in the previous one)
        let (start, current, previous) = self.base.get_or_insert((arrival_us, one_way, one_way));
        if arrival_us - *start > BASE_WINDOW_US {
            *previous = *current;
            *current = one_way;
            *start = arrival_us;
        } else {
            *current = (*current).min(one_way);
        }
        let floor = (*current).min(*previous);
        let queue = (one_way - floor) as f64 / 1000.0;
        self.queue_ms = 0.95 * self.queue_ms + 0.05 * queue;
    }

    /// Delivered rate over the most recent `window_us` of arrivals.
    fn recent_bps(&self, window_us: i64) -> Option<f64> {
        let &(last, _) = self.acked.back()?;
        let mut bytes = 0;
        let mut first = last;
        for &(at, size) in self.acked.iter().rev() {
            if last - at > window_us {
                break;
            }
            bytes += size;
            first = at;
        }
        let span = last - first;
        (span >= window_us / 2).then(|| bytes as f64 * 8.0 * 1e6 / span as f64)
    }

    /// Delivered rate over the last half second of arrivals, once at least
    /// [`ACKED_MIN_SPAN_US`] of arrivals back it up.
    pub fn acked_bps(&self) -> Option<f64> {
        let (&(first, _), &(last, _)) = (self.acked.front()?, self.acked.back()?);
        let span = last - first;
        if span < ACKED_MIN_SPAN_US {
            return None;
        }
        let bytes: usize = self.acked.iter().map(|(_, b)| b).sum();
        Some(bytes as f64 * 8.0 * 1e6 / span as f64)
    }

    /// One received packet, in send order. `arrival_us` is on the
    /// receiver's clock; only differences matter.
    pub fn on_packet(&mut self, send_us: u64, arrival_us: i64, bytes: usize) {
        self.track_queue(send_us, arrival_us);
        self.acked.push_back((arrival_us, bytes));
        while self
            .acked
            .front()
            .is_some_and(|(at, _)| arrival_us - at > ACKED_WINDOW_US)
        {
            self.acked.pop_front();
        }
        match &mut self.group {
            Some(group) if send_us.saturating_sub(group.first_send_us) < BURST_US => {
                group.last_send_us = group.last_send_us.max(send_us);
                group.last_arrival_us = group.last_arrival_us.max(arrival_us);
            }
            _ => {
                if let Some(done) = self.group.take() {
                    self.close_group(done);
                }
                self.group = Some(Group {
                    first_send_us: send_us,
                    last_send_us: send_us,
                    last_arrival_us: arrival_us,
                });
            }
        }
    }

    /// Packets the receiver reported received and lost in one feedback.
    pub fn on_loss(&mut self, received: usize, lost: usize) {
        let total = received + lost;
        if total > 0 {
            let fraction = lost as f64 / total as f64;
            self.loss = 0.8 * self.loss + 0.2 * fraction;
        }
    }

    fn close_group(&mut self, group: Group) {
        let Some(previous) = self.previous.replace(group) else {
            self.first_arrival_us = Some(group.last_arrival_us);
            return;
        };
        let send_delta = group.last_send_us as f64 - previous.last_send_us as f64;
        let arrival_delta = (group.last_arrival_us - previous.last_arrival_us) as f64;
        let delay_ms = (arrival_delta - send_delta) / 1000.0;
        self.accumulated_ms += delay_ms;
        self.smoothed_ms = SMOOTHING * self.smoothed_ms + (1.0 - SMOOTHING) * self.accumulated_ms;
        let at_ms = (group.last_arrival_us - self.first_arrival_us.unwrap_or(0)) as f64 / 1000.0;
        self.history.push_back((at_ms, self.smoothed_ms));
        if self.history.len() > TREND_WINDOW {
            self.history.pop_front();
        }
        self.deltas = (self.deltas + 1).min(60);
        if self.history.len() == TREND_WINDOW {
            let trend = slope(&self.history) * f64::from(self.deltas) * TREND_GAIN;
            self.detect(trend, group.last_arrival_us);
        }
    }

    fn detect(&mut self, trend: f64, now_us: i64) {
        if trend > self.threshold_ms {
            let since = *self.overuse_since.get_or_insert(now_us);
            if now_us - since >= OVERUSE_TIME_US as i64 && trend >= self.previous_trend {
                self.signal = Signal::Overuse;
            }
        } else {
            self.overuse_since = None;
            self.signal = if trend < -self.threshold_ms {
                Signal::Underuse
            } else {
                Signal::Normal
            };
        }
        self.previous_trend = trend;
        // Adapt the threshold so it tracks the trend's natural range, except
        // for spikes far above it.
        let elapsed_ms = self
            .last_threshold_us
            .map_or(0.0, |at| ((now_us - at) as f64 / 1000.0).min(100.0));
        self.last_threshold_us = Some(now_us);
        if trend.abs() <= self.threshold_ms + 15.0 {
            let k = if trend.abs() < self.threshold_ms {
                THRESHOLD_DOWN
            } else {
                THRESHOLD_UP
            };
            self.threshold_ms += k * (trend.abs() - self.threshold_ms) * elapsed_ms;
            self.threshold_ms = self.threshold_ms.clamp(6.0, 600.0);
        }
    }

    /// Advances the controller; call after each feedback. Returns the target.
    pub fn update(&mut self, now_us: u64, rtt_us: u64) -> u32 {
        let elapsed_s = self
            .last_update_us
            .map_or(0.0, |at| (now_us.saturating_sub(at)) as f64 / 1e6)
            .min(1.0);
        self.last_update_us = Some(now_us);
        let acked = self.acked_bps();
        // A decrease takes a queue-drain plus a round trip to show in the
        // delay trend; do not cut again before that.
        let hold = rtt_us + 300_000;
        let signal = if self.queue_ms > QUEUE_LIMIT_MS {
            Signal::Overuse
        } else {
            self.signal
        };
        match signal {
            Signal::Overuse => {
                let recent = self
                    .last_decrease_us
                    .is_some_and(|at| now_us.saturating_sub(at) < hold);
                if !recent {
                    // While a queue builds, what still gets through is the
                    // bottleneck's capacity: remember it, and go below it.
                    let delivered = self.recent_bps(150_000).or(acked);
                    let basis = delivered.unwrap_or(self.target).min(self.target);
                    self.target = 0.85 * basis;
                    self.congestion_bps = delivered.or(Some(basis));
                    self.last_decrease_us = Some(now_us);
                }
            }
            Signal::Normal => {
                let before = self.target;
                let near = self
                    .congestion_bps
                    .is_some_and(|congestion| self.target > 0.85 * congestion);
                if near {
                    // About one packet per round trip.
                    let per_packet = 1200.0 * 8.0 * 1e6 / rtt_us.max(10_000) as f64;
                    self.target += per_packet.max(0.05 * self.target) * elapsed_s;
                } else {
                    self.target *= 1.08f64.powf(elapsed_s * 10.0);
                }
                // Growth may not run far ahead of what actually arrives; the
                // cap never lowers the rate, since an encoder that sends less
                // than the target is not congestion.
                if let Some(acked) = acked {
                    let ceiling = (1.5 * acked + 100_000.0).max(before);
                    self.target = self.target.min(ceiling);
                }
            }
            Signal::Underuse => {}
        }
        if self.loss > 0.1
            && self
                .last_loss_cut_us
                .is_none_or(|at| now_us.saturating_sub(at) >= 300_000)
        {
            self.target *= 1.0 - 0.5 * self.loss;
            self.last_loss_cut_us = Some(now_us);
        }
        self.target = self.target.clamp(
            f64::from(self.config.min_bps),
            f64::from(self.config.max_bps),
        );
        self.target_bps()
    }
}

/// Least-squares slope of `(x, y)` points.
fn slope(points: &VecDeque<(f64, f64)>) -> f64 {
    let n = points.len() as f64;
    let mean_x = points.iter().map(|(x, _)| x).sum::<f64>() / n;
    let mean_y = points.iter().map(|(_, y)| y).sum::<f64>() / n;
    let (mut num, mut den) = (0.0, 0.0);
    for (x, y) in points {
        num += (x - mean_x) * (y - mean_y);
        den += (x - mean_x) * (x - mean_x);
    }
    if den == 0.0 { 0.0 } else { num / den }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn controller() -> RateController {
        RateController::new(RateConfig {
            min_bps: 500_000,
            max_bps: 100_000_000,
            start_bps: 10_000_000,
        })
    }

    /// Feeds packets of `bytes` sent at `send_bps` through a bottleneck of
    /// `link_bps` for `seconds`, with feedback every 50 ms.
    fn drive(rc: &mut RateController, start_us: u64, seconds: f64, link_bps: f64, bytes: usize) {
        let mut now = start_us;
        let mut link_free = start_us as f64;
        let end = start_us + (seconds * 1e6) as u64;
        let mut next_update = now + 50_000;
        while now < end {
            let send_bps = f64::from(rc.target_bps());
            let gap = bytes as f64 * 8.0 * 1e6 / send_bps;
            let departure = link_free.max(now as f64) + bytes as f64 * 8.0 * 1e6 / link_bps;
            link_free = departure;
            rc.on_packet(now, (departure + 1_000.0) as i64, bytes);
            now += gap as u64;
            if now >= next_update {
                rc.update(now, 2_000);
                next_update += 50_000;
            }
        }
    }

    #[test]
    fn ramps_up_on_a_free_link() {
        let mut rc = controller();
        drive(&mut rc, 0, 3.0, 1e9, 1200);
        assert_eq!(rc.signal(), Signal::Normal);
        assert!(rc.target_bps() > 30_000_000, "{}", rc.target_bps());
    }

    #[test]
    fn backs_off_below_a_bottleneck_within_two_seconds() {
        let mut rc = RateController::new(RateConfig {
            min_bps: 500_000,
            max_bps: 100_000_000,
            start_bps: 30_000_000,
        });
        drive(&mut rc, 0, 2.0, 10e6, 1200);
        assert!(rc.target_bps() <= 10_000_000, "{}", rc.target_bps());
        assert!(rc.target_bps() >= 5_000_000, "{}", rc.target_bps());
        // And it settles near the bottleneck rather than collapsing.
        drive(&mut rc, 2_000_000, 4.0, 10e6, 1200);
        assert!(
            (6_000_000..=11_000_000).contains(&rc.target_bps()),
            "{}",
            rc.target_bps()
        );
    }

    #[test]
    fn heavy_loss_cuts_the_rate() {
        let mut rc = controller();
        rc.on_loss(70, 30);
        rc.on_loss(70, 30);
        let before = rc.target_bps();
        rc.update(1_000_000, 10_000);
        assert!(rc.target_bps() < before);
        assert!(
            rc.update(1_000_000, 10_000) <= rc.target_bps(),
            "rate-limited"
        );
    }

    #[test]
    fn slope_of_a_line() {
        let points: VecDeque<(f64, f64)> = (0..10)
            .map(|i| (f64::from(i), 2.0 * f64::from(i) + 1.0))
            .collect();
        assert!((slope(&points) - 2.0).abs() < 1e-9);
    }
}
