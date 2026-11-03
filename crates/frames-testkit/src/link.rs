//! One-way simulated network link.
//!
//! A packet first passes the bottleneck (serialization at `rate_bps` behind a
//! drop-tail queue of `queue_bytes`), then random or burst loss, then
//! propagation `delay_us` plus uniform `jitter_us`. With `reorder: false`
//! delivery stays FIFO even with jitter, like most real paths; with `true`
//! jitter may reorder packets.

use crate::rng::Rng;
use bytes::Bytes;
use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// Gilbert-Elliott two-state burst loss.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Burst {
    /// Per-packet probability of entering the bad state.
    pub enter: f64,
    /// Per-packet probability of leaving it.
    pub exit: f64,
    /// Loss probability while bad.
    pub loss: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkConfig {
    pub delay_us: u64,
    pub jitter_us: u64,
    pub reorder: bool,
    /// Independent loss probability (in the good state when `burst` is set).
    pub loss: f64,
    pub burst: Option<Burst>,
    /// Bottleneck rate; `None` = unlimited.
    pub rate_bps: Option<u64>,
    /// Drop-tail queue in front of the bottleneck.
    pub queue_bytes: usize,
}

impl LinkConfig {
    /// A clean wired LAN: 0.5 ms one way, 1 Gbps, deep queue.
    pub fn lan() -> Self {
        Self {
            delay_us: 500,
            jitter_us: 0,
            reorder: false,
            loss: 0.0,
            burst: None,
            rate_bps: Some(1_000_000_000),
            queue_bytes: 4 << 20,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkStats {
    pub sent: u64,
    pub delivered: u64,
    pub lost: u64,
    pub dropped_queue: u64,
    pub delivered_bytes: u64,
}

pub struct Link {
    config: LinkConfig,
    rng: Rng,
    bad: bool,
    busy_until_us: u64,
    last_delivery_us: u64,
    order: u64,
    in_flight: BinaryHeap<Reverse<(u64, u64, PacketSlot)>>,
    stats: LinkStats,
}

/// Heap payload ordered only by the (time, order) key in front of it.
#[derive(Clone, Debug)]
struct PacketSlot(Bytes);

impl PartialEq for PacketSlot {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}
impl Eq for PacketSlot {}
impl PartialOrd for PacketSlot {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for PacketSlot {
    fn cmp(&self, _: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

impl Link {
    pub fn new(config: LinkConfig, seed: u64) -> Self {
        Self {
            config,
            rng: Rng::new(seed),
            bad: false,
            busy_until_us: 0,
            last_delivery_us: 0,
            order: 0,
            in_flight: BinaryHeap::new(),
            stats: LinkStats::default(),
        }
    }

    pub fn config(&self) -> &LinkConfig {
        &self.config
    }

    /// Changes conditions mid-run (e.g. a bandwidth step-down).
    pub fn set_config(&mut self, config: LinkConfig) {
        self.config = config;
    }

    pub fn stats(&self) -> LinkStats {
        self.stats
    }

    /// Bytes waiting at the bottleneck at `now_us`.
    pub fn queued_bytes(&self, now_us: u64) -> u64 {
        match self.config.rate_bps {
            Some(rate) => self.busy_until_us.saturating_sub(now_us) * rate / 8_000_000,
            None => 0,
        }
    }

    pub fn send(&mut self, now_us: u64, packet: Bytes) {
        self.stats.sent += 1;
        let departure = match self.config.rate_bps {
            Some(rate) => {
                let start = self.busy_until_us.max(now_us);
                let backlog = self.queued_bytes(now_us);
                if backlog + packet.len() as u64 > self.config.queue_bytes as u64 {
                    self.stats.dropped_queue += 1;
                    return;
                }
                let serialization = (packet.len() as u64 * 8_000_000).div_ceil(rate.max(1));
                self.busy_until_us = start + serialization;
                self.busy_until_us
            }
            None => now_us,
        };
        let loss = match self.config.burst {
            Some(burst) => {
                if self.bad {
                    self.bad = !self.rng.chance(burst.exit);
                } else {
                    self.bad = self.rng.chance(burst.enter);
                }
                if self.bad {
                    burst.loss
                } else {
                    self.config.loss
                }
            }
            None => self.config.loss,
        };
        if self.rng.chance(loss) {
            self.stats.lost += 1;
            return;
        }
        let mut arrival =
            departure + self.config.delay_us + self.rng.below(self.config.jitter_us + 1);
        if !self.config.reorder {
            arrival = arrival.max(self.last_delivery_us);
            self.last_delivery_us = arrival;
        }
        self.order += 1;
        self.in_flight
            .push(Reverse((arrival, self.order, PacketSlot(packet))));
    }

    /// Packets that have arrived by `now_us`, in arrival order.
    pub fn deliver(&mut self, now_us: u64) -> Vec<Bytes> {
        let mut out = Vec::new();
        while let Some(Reverse((arrival, _, _))) = self.in_flight.peek() {
            if *arrival > now_us {
                break;
            }
            let Reverse((_, _, PacketSlot(packet))) = self.in_flight.pop().expect("peeked");
            self.stats.delivered += 1;
            self.stats.delivered_bytes += packet.len() as u64;
            out.push(packet);
        }
        out
    }

    /// When the next packet arrives, if any is in flight.
    pub fn next_arrival_us(&self) -> Option<u64> {
        self.in_flight
            .peek()
            .map(|Reverse((arrival, _, _))| *arrival)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(byte: u8, len: usize) -> Bytes {
        Bytes::from(vec![byte; len])
    }

    #[test]
    fn delay_and_bottleneck_serialization_are_exact() {
        let mut link = Link::new(
            LinkConfig {
                delay_us: 1_000,
                rate_bps: Some(8_000_000), // 1 byte per µs
                ..LinkConfig::lan()
            },
            1,
        );
        link.send(0, packet(1, 1_000));
        link.send(0, packet(2, 500));
        assert_eq!(link.queued_bytes(0), 1_500);
        assert_eq!(link.next_arrival_us(), Some(2_000));
        assert!(link.deliver(1_999).is_empty());
        assert_eq!(link.deliver(2_000), vec![packet(1, 1_000)]);
        assert_eq!(link.deliver(2_500), vec![packet(2, 500)]);
        assert_eq!(link.stats().delivered_bytes, 1_500);
    }

    #[test]
    fn drop_tail_queue_limits_the_backlog() {
        let mut link = Link::new(
            LinkConfig {
                rate_bps: Some(8_000_000),
                queue_bytes: 2_500,
                ..LinkConfig::lan()
            },
            1,
        );
        for i in 0..5 {
            link.send(0, packet(i, 1_000));
        }
        let stats = link.stats();
        assert_eq!(stats.dropped_queue, 3, "only two 1000-byte packets fit");
        assert_eq!(link.deliver(u64::MAX).len(), 2);
    }

    #[test]
    fn jitter_keeps_fifo_unless_reordering_is_allowed() {
        let config = LinkConfig {
            delay_us: 100,
            jitter_us: 5_000,
            rate_bps: None,
            ..LinkConfig::lan()
        };
        let order = |reorder| {
            let mut link = Link::new(LinkConfig { reorder, ..config }, 42);
            for i in 0..200u8 {
                link.send(u64::from(i) * 10, packet(i, 1));
            }
            link.deliver(u64::MAX)
                .iter()
                .map(|p| p[0])
                .collect::<Vec<_>>()
        };
        let fifo = order(false);
        assert!(fifo.windows(2).all(|w| w[0] < w[1]));
        let shuffled = order(true);
        assert_eq!(shuffled.len(), 200);
        assert!(shuffled.windows(2).any(|w| w[0] > w[1]));
    }

    #[test]
    fn random_and_burst_loss_are_seeded_and_bursty() {
        const N: u32 = 20_000;
        // Returns which packets were lost, by index.
        let run = |seed, loss, burst| {
            let mut link = Link::new(
                LinkConfig {
                    loss,
                    burst,
                    rate_bps: None,
                    ..LinkConfig::lan()
                },
                seed,
            );
            for i in 0..N {
                link.send(u64::from(i), Bytes::copy_from_slice(&i.to_le_bytes()));
            }
            let mut lost = vec![true; N as usize];
            for packet in link.deliver(u64::MAX) {
                lost[u32::from_le_bytes(packet[..4].try_into().unwrap()) as usize] = false;
            }
            lost
        };
        // Mean length of consecutive-loss runs.
        let mean_run = |lost: &[bool]| {
            let runs = lost.windows(2).filter(|w| w[1] && !w[0]).count() + usize::from(lost[0]);
            lost.iter().filter(|l| **l).count() as f64 / runs.max(1) as f64
        };
        let burst = Some(Burst {
            enter: 0.01,
            exit: 0.2,
            loss: 0.8,
        });
        let bursty = run(5, 0.0, burst);
        assert_eq!(bursty, run(5, 0.0, burst), "same seed, same losses");
        let rate = bursty.iter().filter(|l| **l).count() as f64 / f64::from(N);
        // Stationary bad fraction 0.01 / (0.01 + 0.2) ≈ 4.8%, × 0.8 loss.
        assert!((0.02..0.06).contains(&rate), "{rate}");
        let random = run(5, rate, None);
        assert!(
            mean_run(&bursty) > 2.0 * mean_run(&random),
            "bursts {} vs random {}",
            mean_run(&bursty),
            mean_run(&random)
        );
    }
}
