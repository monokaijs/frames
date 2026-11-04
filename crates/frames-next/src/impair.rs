//! `frames-next impair`: a UDP proxy that impairs traffic like v1's netsim
//! (`crates/frames/src/streaming/netsim.rs`), so v2 over real QUIC can be
//! compared with v1 under the same conditions (docs/V2_PLAN.md, Phase 2).
//!
//! The viewer dials the proxy (`connect ID --addr PROXY --direct-only`); the
//! proxy forwards to the host and back, each direction through its own
//! impaired link: loss with bursts, a rate cap with a bounded queue, delay
//! with jitter, delivered in order by a FIFO delay line (per-packet timers
//! reorder packets due at the same instant, which QUIC treats as loss).

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Impairment {
    delay_ms: f64,
    jitter_ms: f64,
    loss: f64,
    burst: f64,
    rate_mbps: f64,
    queue_ms: f64,
}

/// v1's netsim scenarios, unchanged.
pub fn scenario(name: &str) -> Option<Impairment> {
    let base = Impairment {
        delay_ms: 1.0,
        jitter_ms: 0.0,
        loss: 0.0,
        burst: 0.0,
        rate_mbps: 0.0,
        queue_ms: 100.0,
    };
    Some(match name {
        "lan" => base,
        "wifi" => Impairment {
            delay_ms: 4.0,
            jitter_ms: 3.0,
            loss: 0.01,
            burst: 0.3,
            ..base
        },
        "lossy" => Impairment {
            delay_ms: 10.0,
            jitter_ms: 2.0,
            loss: 0.03,
            burst: 0.5,
            ..base
        },
        "constrained" => Impairment {
            delay_ms: 15.0,
            jitter_ms: 1.0,
            loss: 0.002,
            rate_mbps: 4.0,
            queue_ms: 80.0,
            ..base
        },
        _ => return None,
    })
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

struct Link {
    impairment: Impairment,
    rng: Rng,
    in_burst: bool,
    next_free: Instant,
    last_delivery: Instant,
    dropped: Arc<AtomicU64>,
}

impl Link {
    fn new(impairment: Impairment, seed: u64, dropped: Arc<AtomicU64>) -> Self {
        let now = Instant::now();
        Self {
            impairment,
            rng: Rng(seed),
            in_burst: false,
            next_free: now,
            last_delivery: now,
            dropped,
        }
    }

    /// When to deliver a packet, or `None` if it is lost or tail-dropped.
    fn admit(&mut self, size: usize, now: Instant) -> Option<Instant> {
        let i = self.impairment;
        let lost = if self.in_burst {
            self.rng.next() < i.burst
        } else {
            self.rng.next() < i.loss
        };
        self.in_burst = lost;
        if lost {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let mut departure = now;
        if i.rate_mbps > 0.0 {
            let start = self.next_free.max(now);
            if start.duration_since(now) > Duration::from_secs_f64(i.queue_ms / 1000.0) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            departure = start + Duration::from_secs_f64(size as f64 * 8.0 / (i.rate_mbps * 1e6));
            self.next_free = departure;
        }
        let jitter = (self.rng.next() * 2.0 - 1.0) * i.jitter_ms;
        let delivery = departure + Duration::from_secs_f64((i.delay_ms + jitter).max(0.0) / 1000.0);
        let delivery = delivery.max(self.last_delivery);
        self.last_delivery = delivery;
        Some(delivery)
    }
}

type Queue = Arc<std::sync::Mutex<VecDeque<(Instant, Vec<u8>, SocketAddr)>>>;

/// Sends queued packets at their delivery times, in order.
fn delay_line(socket: Arc<tokio::net::UdpSocket>, queue: Queue, wake: Arc<tokio::sync::Notify>) {
    tokio::spawn(async move {
        loop {
            let next = queue.lock().unwrap().front().map(|(at, _, _)| *at);
            match next {
                None => wake.notified().await,
                Some(at) if at > Instant::now() => {
                    tokio::select! {
                        _ = tokio::time::sleep_until(at.into()) => {}
                        _ = wake.notified() => {}
                    }
                }
                Some(_) => {
                    let Some((_, bytes, to)) = queue.lock().unwrap().pop_front() else {
                        continue;
                    };
                    let _ = socket.send_to(&bytes, to).await;
                }
            }
        }
    });
}

pub async fn run(args: &[String]) -> Result<()> {
    let value = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    };
    let listen: SocketAddr = value("--listen").unwrap_or("0.0.0.0:47100").parse()?;
    let host: SocketAddr = value("--to")
        .context("impair needs --to HOST:PORT")?
        .parse()?;
    let name = value("--scenario").unwrap_or("lan");
    let impairment = scenario(name).with_context(|| format!("unknown scenario {name}"))?;
    let seconds: u64 = value("--seconds").unwrap_or("60").parse()?;
    #[cfg(windows)]
    let _timer = frames_platform_windows::system::TimerResolution::one_millisecond();

    let front = Arc::new(tokio::net::UdpSocket::bind(listen).await?);
    let back = Arc::new(
        tokio::net::UdpSocket::bind(if host.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })
        .await?,
    );
    println!("impair {name}: {listen} <-> {host}");
    let dropped = Arc::new(AtomicU64::new(0));
    let (to_host, to_viewer): (Queue, Queue) = (Arc::default(), Arc::default());
    let (wake_host, wake_viewer) = (
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(tokio::sync::Notify::new()),
    );
    delay_line(back.clone(), to_host.clone(), wake_host.clone());
    delay_line(front.clone(), to_viewer.clone(), wake_viewer.clone());
    let viewer: Arc<std::sync::Mutex<Option<SocketAddr>>> = Arc::default();

    let up = {
        let (front, viewer, dropped) = (front.clone(), viewer.clone(), dropped.clone());
        tokio::spawn(async move {
            let mut link = Link::new(impairment, 7, dropped);
            let mut buffer = vec![0u8; 65536];
            while let Ok((n, from)) = front.recv_from(&mut buffer).await {
                *viewer.lock().unwrap() = Some(from);
                if let Some(at) = link.admit(n, Instant::now()) {
                    to_host
                        .lock()
                        .unwrap()
                        .push_back((at, buffer[..n].to_vec(), host));
                    wake_host.notify_one();
                }
            }
        })
    };
    let down = {
        let (back, viewer, dropped) = (back.clone(), viewer.clone(), dropped.clone());
        tokio::spawn(async move {
            let mut link = Link::new(impairment, 11, dropped);
            let mut buffer = vec![0u8; 65536];
            while let Ok((n, _)) = back.recv_from(&mut buffer).await {
                let Some(to) = *viewer.lock().unwrap() else {
                    continue;
                };
                if let Some(at) = link.admit(n, Instant::now()) {
                    to_viewer
                        .lock()
                        .unwrap()
                        .push_back((at, buffer[..n].to_vec(), to));
                    wake_viewer.notify_one();
                }
            }
        })
    };
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    up.abort();
    down.abort();
    println!(
        "impair {name}: dropped {} packets",
        dropped.load(Ordering::Relaxed)
    );
    Ok(())
}
