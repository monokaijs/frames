//! Phase 0 spike 0-A: can iroh carry Frames media?
//!
//! Questions answered by running this:
//! 1. A custom congestion controller, driven by an application-set target rate,
//!    actually paces datagrams (the media rate controller must own the rate).
//! 2. 1200-byte datagrams over direct and relay paths: loss, reordering,
//!    queueing-delay jitter, RTT, and which path is selected.
//! 3. `export_keying_material` works (pairing codes and identity migration
//!    depend on it): both sides must print the same exporter value.
//!
//! Usage:
//!   iroh-transport-spike host [--relay-only] [--cc target|cubic] [--rates 10,40,10] [--step-secs 5]
//!   iroh-transport-spike connect <endpoint-id> [--relay-only] [--direct ip:port] [--secs 20]

use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use bytes::{BufMut, Bytes, BytesMut};
use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, QuicTransportConfig, RttEstimator, presets},
};
use noq_proto::congestion::{Controller, ControllerFactory, ControllerMetrics};

const ALPN: &[u8] = b"frames-spike/0";
const HEADER: usize = 20; // seq u64, send_us u64, rate_mbps u32

/// Window and pacing follow an externally set target instead of loss. In the
/// real engine the media rate controller (delay gradient + loss) sets the target.
#[derive(Debug, Clone)]
struct TargetRate {
    target_bps: Arc<AtomicU64>,
    congestion_events: Arc<AtomicU64>,
    mtu: u16,
    rtt: Duration,
}

impl TargetRate {
    fn bytes_per_second(&self) -> u64 {
        self.target_bps.load(Ordering::Relaxed) / 8
    }
}

impl Controller for TargetRate {
    fn on_ack(&mut self, _: Instant, _: Instant, _: u64, _: u64, _: bool, rtt: &RttEstimator) {
        self.rtt = rtt.get();
    }

    fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: bool, _: u64, _: u64) {
        // Loss is reported to the media controller, which decides the target.
        self.congestion_events.fetch_add(1, Ordering::Relaxed);
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = new_mtu;
    }

    fn window(&self) -> u64 {
        // Two RTTs of the target rate, never below 16 packets.
        let rtt = self.rtt.max(Duration::from_millis(5)).as_secs_f64();
        let window = (self.bytes_per_second() as f64 * rtt * 2.0) as u64;
        window.max(16 * u64::from(self.mtu))
    }

    fn metrics(&self) -> ControllerMetrics {
        let mut metrics = ControllerMetrics::default();
        metrics.congestion_window = self.window();
        // 25% headroom so pacing never becomes the bottleneck for the target.
        metrics.pacing_rate = Some(self.bytes_per_second() * 5 / 4);
        metrics
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        16 * u64::from(self.mtu)
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[derive(Debug)]
struct TargetRateFactory {
    target_bps: Arc<AtomicU64>,
    congestion_events: Arc<AtomicU64>,
}

impl ControllerFactory for TargetRateFactory {
    fn build(self: Arc<Self>, _: Instant, current_mtu: u16) -> Box<dyn Controller> {
        // One controller per path (multipath): relay and direct paths each get one.
        Box::new(TargetRate {
            target_bps: self.target_bps.clone(),
            congestion_events: self.congestion_events.clone(),
            mtu: current_mtu,
            rtt: Duration::from_millis(50),
        })
    }
}

struct Options {
    relay_only: bool,
    target_cc: bool,
    rates_mbps: Vec<u64>,
    step: Duration,
    seconds: u64,
    /// Dial only this address, with relays and address lookup disabled, so a
    /// wrong reply source address cannot be masked by another working path.
    direct: Option<std::net::SocketAddr>,
}

fn parse(args: &[String]) -> Result<Options> {
    let mut options = Options {
        relay_only: false,
        target_cc: true,
        rates_mbps: vec![10, 40, 10],
        step: Duration::from_secs(5),
        seconds: 20,
        direct: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = || iter.next().context("missing option value");
        match arg.as_str() {
            "--relay-only" => options.relay_only = true,
            "--cc" => options.target_cc = value()? == "target",
            "--rates" => {
                options.rates_mbps = value()?
                    .split(',')
                    .map(str::parse)
                    .collect::<Result<_, _>>()?
            }
            "--step-secs" => options.step = Duration::from_secs(value()?.parse()?),
            "--secs" => options.seconds = value()?.parse()?,
            "--direct" => options.direct = Some(value()?.parse()?),
            other => bail!("unknown option {other}"),
        }
    }
    Ok(options)
}

async fn endpoint(options: &Options, target: &Arc<AtomicU64>, events: &Arc<AtomicU64>) -> Result<Endpoint> {
    let mut transport = QuicTransportConfig::builder()
        .datagram_receive_buffer_size(Some(8 << 20))
        .datagram_send_buffer_size(64 << 10);
    if options.target_cc {
        transport = transport.congestion_controller_factory(Arc::new(TargetRateFactory {
            target_bps: target.clone(),
            congestion_events: events.clone(),
        }));
    }
    let mut builder = Endpoint::builder(presets::N0)
        .alpns(vec![ALPN.to_vec()])
        .transport_config(transport.build());
    if options.relay_only {
        builder = builder.clear_ip_transports();
    }
    if options.direct.is_some() {
        builder = builder.clear_relay_transports().clear_address_lookup();
    }
    let endpoint = builder.bind().await?;
    println!("endpoint id: {}", endpoint.id());
    println!("bound sockets: {:?}", endpoint.bound_sockets());
    if options.direct.is_none() {
        tokio::time::timeout(Duration::from_secs(10), endpoint.online())
            .await
            .context("endpoint did not reach a relay within 10 s")?;
    }
    println!("address: {:?}", endpoint.addr());
    Ok(endpoint)
}

fn exporter(connection: &Connection) -> Result<String> {
    let mut output = [0u8; 16];
    connection
        .export_keying_material(&mut output, b"frames-spike-exporter", b"")
        .map_err(|error| anyhow::anyhow!("export_keying_material failed: {error:?}"))?;
    Ok(hex::encode(output))
}

fn describe_paths(connection: &Connection) -> String {
    let mut parts = Vec::new();
    for path in connection.paths().iter() {
        parts.push(format!(
            "{}{} {:?} rtt={:.1}ms",
            if path.is_selected() { "*" } else { "" },
            if path.is_relay() { "relay" } else { "direct" },
            path.remote_addr(),
            path.rtt().as_secs_f64() * 1e3,
        ));
    }
    parts.join(" | ")
}

fn micros(epoch: Instant) -> u64 {
    epoch.elapsed().as_micros() as u64
}

async fn host(options: Options) -> Result<()> {
    let target = Arc::new(AtomicU64::new(options.rates_mbps[0] * 1_000_000));
    let events = Arc::new(AtomicU64::new(0));
    let endpoint = endpoint(&options, &target, &events).await?;
    println!("waiting for `connect {}`", endpoint.id());
    let incoming = endpoint.accept().await.context("endpoint closed")?;
    let connection = incoming.await?;
    println!("connected: {} exporter={}", connection.remote_id(), exporter(&connection)?);

    let epoch = Instant::now();
    let mut sequence = 0u64;
    let mut dropped_by_sender = 0u64;
    for &rate in &options.rates_mbps {
        target.store(rate * 1_000_000, Ordering::Relaxed);
        let size = connection.max_datagram_size().context("peer disabled datagrams")?.min(1200);
        let packets_per_ms = (rate * 1_000_000 / 8 / size as u64) as f64 / 1000.0;
        println!("rate {rate} Mbps, datagram {size} B, paths: {}", describe_paths(&connection));
        let stop = Instant::now() + options.step;
        let mut tick = tokio::time::interval(Duration::from_millis(1));
        let mut owed = 0.0f64;
        while Instant::now() < stop {
            tick.tick().await;
            owed += packets_per_ms;
            while owed >= 1.0 {
                owed -= 1.0;
                let mut packet = BytesMut::with_capacity(size);
                packet.put_u64(sequence);
                packet.put_u64(micros(epoch));
                packet.put_u32(rate as u32);
                packet.resize(size.max(HEADER), 0);
                sequence += 1;
                // Never block the media clock: a full send buffer drops, like the real engine.
                if connection.send_datagram(packet.freeze()).is_err() {
                    dropped_by_sender += 1;
                }
            }
        }
    }
    println!(
        "host done: sent={sequence} send-buffer drops={dropped_by_sender} congestion events={}",
        events.load(Ordering::Relaxed)
    );
    connection.close(0u32.into(), b"done");
    tokio::time::sleep(Duration::from_millis(500)).await;
    Ok(())
}

#[derive(Default)]
struct Window {
    received: u64,
    bytes: u64,
    lost: u64,
    reordered: u64,
    queueing_us: Vec<u64>,
    rate_mbps: u32,
}

impl Window {
    fn report(&mut self, connection: &Connection) {
        self.queueing_us.sort_unstable();
        let percentile = |p: usize| {
            self.queueing_us
                .get(self.queueing_us.len().saturating_sub(1) * p / 100)
                .map_or(0.0, |&us| us as f64 / 1e3)
        };
        let expected = self.received + self.lost;
        println!(
            "target {:>3} Mbps | recv {:6.1} Mbps | loss {:5.2}% | reorder {:4} | queueing p50 {:5.1} ms p95 {:5.1} ms p99 {:5.1} ms | {}",
            self.rate_mbps,
            self.bytes as f64 * 8.0 / 1e6,
            if expected == 0 { 0.0 } else { self.lost as f64 * 100.0 / expected as f64 },
            self.reordered,
            percentile(50),
            percentile(95),
            percentile(99),
            describe_paths(connection),
        );
        *self = Window::default();
    }
}

async fn connect(remote: EndpointId, options: Options) -> Result<()> {
    let target = Arc::new(AtomicU64::new(1_000_000));
    let events = Arc::new(AtomicU64::new(0));
    let endpoint = endpoint(&options, &target, &events).await?;
    let started = Instant::now();
    let address = match options.direct {
        Some(direct) => EndpointAddr::new(remote).with_ip_addr(direct),
        None => EndpointAddr::from(remote),
    };
    let connection = tokio::time::timeout(Duration::from_secs(15), endpoint.connect(address, ALPN))
        .await
        .context("connect timed out after 15 s")??;
    println!(
        "connected in {:.0} ms exporter={} paths: {}",
        started.elapsed().as_secs_f64() * 1e3,
        exporter(&connection)?,
        describe_paths(&connection)
    );

    let epoch = Instant::now();
    // Relative one-way delay: arrival - send has an unknown constant offset;
    // subtracting the minimum seen leaves the queueing delay and jitter.
    let mut min_offset = i64::MAX;
    let mut highest: Option<u64> = None;
    let mut window = Window::default();
    let mut next_report = Instant::now() + Duration::from_secs(1);
    let deadline = Instant::now() + Duration::from_secs(options.seconds);
    while Instant::now() < deadline {
        let datagram = tokio::select! {
            datagram = connection.read_datagram() => match datagram {
                Ok(datagram) => Some(datagram),
                Err(error) => { println!("connection ended: {error}"); break; }
            },
            _ = tokio::time::sleep_until(next_report.into()) => None,
        };
        if let Some(datagram) = datagram {
            record(&datagram, epoch, &mut min_offset, &mut highest, &mut window);
        }
        if Instant::now() >= next_report {
            window.report(&connection);
            next_report += Duration::from_secs(1);
        }
    }
    println!("final paths: {}", describe_paths(&connection));
    Ok(())
}

fn record(datagram: &Bytes, epoch: Instant, min_offset: &mut i64, highest: &mut Option<u64>, window: &mut Window) {
    if datagram.len() < HEADER {
        return;
    }
    let sequence = u64::from_be_bytes(datagram[0..8].try_into().unwrap());
    let sent_us = u64::from_be_bytes(datagram[8..16].try_into().unwrap());
    window.rate_mbps = u32::from_be_bytes(datagram[16..20].try_into().unwrap());
    let offset = micros(epoch) as i64 - sent_us as i64;
    *min_offset = (*min_offset).min(offset);
    window.queueing_us.push((offset - *min_offset) as u64);
    window.received += 1;
    window.bytes += datagram.len() as u64;
    match *highest {
        Some(top) if sequence <= top => {
            window.reordered += 1;
            window.lost = window.lost.saturating_sub(1);
        }
        Some(top) => {
            window.lost += sequence - top - 1;
            *highest = Some(sequence);
        }
        None => *highest = Some(sequence),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_writer(std::io::stderr).init();
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("host") => host(parse(&args[1..])?).await,
        Some("connect") => {
            let remote: EndpointId = args.get(1).context("connect needs an endpoint id")?.parse()?;
            connect(remote, parse(&args[2..])?).await
        }
        _ => bail!("usage: host [--relay-only] [--cc target|cubic] [--rates 10,40,10] [--step-secs 5] | connect <id> [--relay-only] [--direct ip:port] [--secs 20]"),
    }
}
