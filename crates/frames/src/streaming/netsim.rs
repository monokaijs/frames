//! End-to-end media benchmark over an impaired UDP path.
//!
//! Real host pipeline (`start_host`: capture, in-process encoder, FEC, the
//! shared `MediaSender`) and real client pipeline (`ClientMediaWorker`:
//! reassembly, recovery, decoder) over real QUIC, through a UDP proxy that
//! adds delay, jitter, loss and a bandwidth cap. Control messages are relayed
//! in-process with the same one-way delay, handled as `session.rs` does. A
//! synthetic 1280x720 source scrolls content every frame and stamps a frame
//! counter into pixels; the receiver reads it back from decoded pictures to
//! measure capture-to-decoded latency.
//!
//! ```sh
//! cargo test --release -p frames --bin frames netsim -- --ignored --nocapture
//! FRAMES_NETSIM=lan,wifi FRAMES_NETSIM_SECONDS=20 FRAMES_CONGESTION=bbr ...
//! ```
use super::*;
use frames_core::{input::InputEvent, model::*};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Mutex, atomic::AtomicU64},
    time::Instant,
};
use tokio::net::UdpSocket;

const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 60;
const BLOCK: u32 = 32;
const BITS: u32 = 32;

#[derive(Clone, Copy, Debug)]
struct Impairment {
    delay_ms: f64,
    jitter_ms: f64,
    loss: f64,
    burst: f64,
    rate_mbps: f64,
    queue_ms: f64,
}
fn scenario(name: &str) -> Option<Impairment> {
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
#[derive(Default)]
struct LinkStats {
    packets: AtomicU64,
    dropped: AtomicU64,
    bytes: AtomicU64,
}
struct Link {
    impairment: Impairment,
    rng: Rng,
    in_burst: bool,
    next_free: Instant,
    last_delivery: Instant,
    stats: Arc<LinkStats>,
}
impl Link {
    fn new(impairment: Impairment, seed: u64, stats: Arc<LinkStats>) -> Self {
        let now = Instant::now();
        Self {
            impairment,
            rng: Rng(seed),
            in_burst: false,
            next_free: now,
            last_delivery: now,
            stats,
        }
    }
    /// Delivery time for a packet, or None if it is lost or tail-dropped.
    fn admit(&mut self, size: usize, now: Instant) -> Option<Instant> {
        let i = self.impairment;
        self.stats.packets.fetch_add(1, Ordering::Relaxed);
        let lost = if self.in_burst {
            self.rng.next() < i.burst
        } else {
            self.rng.next() < i.loss
        };
        self.in_burst = lost;
        if lost {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let mut departure = now;
        if i.rate_mbps > 0.0 {
            let start = self.next_free.max(now);
            if start.duration_since(now) > Duration::from_secs_f64(i.queue_ms / 1000.0) {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            departure = start + Duration::from_secs_f64(size as f64 * 8.0 / (i.rate_mbps * 1e6));
            self.next_free = departure;
        }
        let jitter = (self.rng.next() * 2.0 - 1.0) * i.jitter_ms;
        let delivery = departure + Duration::from_secs_f64((i.delay_ms + jitter).max(0.0) / 1000.0);
        // Variable delay without reordering, as on most single-path links.
        let delivery = delivery.max(self.last_delivery);
        self.last_delivery = delivery;
        self.stats.bytes.fetch_add(size as u64, Ordering::Relaxed);
        Some(delivery)
    }
}
/// Delivers packets in admission order at their scheduled times. A FIFO delay
/// line: per-packet timers would reorder packets due at the same instant,
/// which QUIC then treats as loss.
fn delay_line(
    send: impl Fn(Vec<u8>) + Send + 'static,
) -> tokio::sync::mpsc::UnboundedSender<(Instant, Vec<u8>)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    tokio::spawn(async move {
        while let Some((at, packet)) = rx.recv().await {
            tokio::time::sleep_until(at.into()).await;
            send(packet);
        }
    });
    tx
}
/// A proxy socket that never drops by itself: default UDP buffers (64 KB on
/// Windows) overflow during one picture's burst and would add loss that the
/// modelled link does not have.
fn proxy_socket() -> UdpSocket {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
    let _ = socket.set_recv_buffer_size(16 << 20);
    let _ = socket.set_send_buffer_size(16 << 20);
    socket
        .bind(&"127.0.0.1:0".parse::<SocketAddr>().unwrap().into())
        .unwrap();
    socket.set_nonblocking(true).unwrap();
    UdpSocket::from_std(socket.into()).unwrap()
}
/// UDP relay: client ⇄ front socket ⇄ back socket ⇄ server.
async fn proxy(server: SocketAddr, link: Impairment) -> (SocketAddr, Arc<LinkStats>) {
    let front = Arc::new(proxy_socket());
    let back = Arc::new(proxy_socket());
    back.connect(server).await.unwrap();
    let address = front.local_addr().unwrap();
    let down_stats = Arc::new(LinkStats::default());
    let client = Arc::new(Mutex::new(None::<SocketAddr>));
    {
        let (front, back, client) = (front.clone(), back.clone(), client.clone());
        let mut up = Link::new(link, 0x9e37_79b9_7f4a_7c15, Default::default());
        let sender = back.clone();
        let line = delay_line(move |packet| {
            let _ = sender.try_send(&packet);
        });
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 65536];
            while let Ok((size, from)) = front.recv_from(&mut buffer).await {
                *client.lock().unwrap() = Some(from);
                let Some(at) = up.admit(size, Instant::now()) else {
                    continue;
                };
                let _ = line.send((at, buffer[..size].to_vec()));
            }
        });
    }
    {
        let mut down = Link::new(link, 0x2545_f491_4f6c_dd1d, down_stats.clone());
        let target = client.clone();
        let line = delay_line(move |packet| {
            if let Some(to) = *target.lock().unwrap() {
                let _ = front.try_send_to(&packet, to);
            }
        });
        tokio::spawn(async move {
            let mut buffer = vec![0u8; 65536];
            while let Ok(size) = back.recv(&mut buffer).await {
                let Some(at) = down.admit(size, Instant::now()) else {
                    continue;
                };
                let _ = line.send((at, buffer[..size].to_vec()));
            }
        });
    }
    (address, down_stats)
}

struct NoCapture;
impl frames_native::Capture for NoCapture {
    fn stop(&mut self) {}
}
struct Synthetic {
    frames: watch::Receiver<Option<media::RawFrame>>,
}
impl Desktop for Synthetic {
    fn windows(&self) -> anyhow::Result<Vec<RemoteWindow>> {
        Ok(vec![])
    }
    fn action(&self, _: WindowId, _: &WindowAction) -> anyhow::Result<()> {
        Ok(())
    }
    fn input(&self, _: WindowId, _: &InputEvent) -> anyhow::Result<()> {
        Ok(())
    }
    fn release_input(&self) {}
    fn capture(&self, _: WindowId, _: bool) -> anyhow::Result<frames_native::CaptureStream> {
        Ok(frames_native::CaptureStream {
            frames: self.frames.clone(),
            guard: Box::new(NoCapture),
        })
    }
    fn clipboard(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn set_clipboard(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}
fn check(counter: u32) -> u32 {
    (counter.wrapping_mul(2_654_435_761) >> 24) & 0xff
}
/// Scrolling "document" content plus a 24-bit counter and 8-bit check code.
fn generator(
    stamps: Arc<Mutex<HashMap<u32, Instant>>>,
) -> (watch::Receiver<Option<media::RawFrame>>, Arc<AtomicU32>) {
    let (tx, rx) = watch::channel(None);
    let captured = Arc::new(AtomicU32::new(0));
    let count = captured.clone();
    let stride = WIDTH as usize * 4;
    let rows = HEIGHT as usize * 2;
    let mut page = vec![0u8; stride * rows];
    for y in 0..rows {
        for x in 0..WIDTH as usize {
            let line = y % 18 < 12 && x % 9 < 7 && (x / 63 + y / 18) % 5 != 0;
            let shade = if line { 40 } else { 235 } as u8;
            let tint = ((x * 3 + y) % 23) as u8;
            page[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&[
                shade,
                shade.saturating_sub(tint),
                shade,
                255,
            ]);
        }
    }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1) / FPS);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        for n in 0u32.. {
            tick.tick().await;
            let counter = n & 0x00ff_ffff;
            let offset = (n as usize * 3 % HEIGHT as usize) * stride;
            let mut bgra = page[offset..offset + stride * HEIGHT as usize].to_vec();
            let code = (counter << 8) | check(counter);
            for bit in 0..BITS {
                let value = if code >> (BITS - 1 - bit) & 1 == 1 {
                    255
                } else {
                    0
                };
                for y in 0..BLOCK as usize {
                    let start = y * stride + (bit * BLOCK) as usize * 4;
                    for px in bgra[start..start + BLOCK as usize * 4].chunks_exact_mut(4) {
                        px.copy_from_slice(&[value, value, value, 255]);
                    }
                }
            }
            let now = Instant::now();
            stamps.lock().unwrap().insert(counter, now);
            count.fetch_add(1, Ordering::Relaxed);
            let frame = media::RawFrame {
                width: WIDTH,
                height: HEIGHT,
                bgra: bgra.into(),
                captured_at: now,
                frame_number: u64::from(n),
                media_generation: 0,
                #[cfg(windows)]
                gpu: None,
            };
            if tx.send(Some(frame)).is_err() {
                break;
            }
        }
    });
    (rx, captured)
}
fn read_counter(frame: &media::RawFrame) -> Option<u32> {
    if (frame.width, frame.height) != (WIDTH, HEIGHT) || frame.bgra.is_empty() {
        return None;
    }
    read_counter_pixels(&frame.bgra)
}
fn read_counter_pixels(bgra: &[u8]) -> Option<u32> {
    if bgra.len() != (WIDTH * HEIGHT * 4) as usize {
        return None;
    }
    let mut code = 0u32;
    for bit in 0..BITS {
        let (x, y) = (bit * BLOCK + BLOCK / 2, BLOCK / 2);
        let green = bgra[((y * WIDTH + x) * 4 + 1) as usize];
        code = (code << 1) | u32::from(green > 128);
    }
    let counter = code >> 8;
    (code & 0xff == check(counter)).then_some(counter)
}

fn identity() -> frames_core::trust::Identity {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["frames.local".into()]).unwrap();
    let certificate = cert.der().clone();
    frames_core::trust::Identity {
        device_id: frames_core::trust::fingerprint(&certificate),
        certificate,
        key: rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
    }
}
/// FIFO with a fixed one-way delay for in-process control messages.
fn delayed(
    delay: Duration,
) -> (
    tokio::sync::mpsc::UnboundedSender<Message>,
    tokio::sync::mpsc::UnboundedReceiver<Message>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let (out, delivered) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            let at = Instant::now() + delay;
            tokio::time::sleep_until(at.into()).await;
            if out.send(message).is_err() {
                break;
            }
        }
    });
    (tx, delivered)
}
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

async fn run(name: &str, link: Impairment, seconds: u64) -> serde_json::Value {
    use frames_core::transport;
    let capabilities = codec::probe().await;
    let capability = capabilities
        .iter()
        .find(|c| c.codec == codec::Codec::H264)
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "no H.264 encoder: {capabilities:?} ({:?})",
                codec::runtime_status()
            )
        });
    let server = transport::server(&identity(), "127.0.0.1:0".parse().unwrap()).unwrap();
    let (address, down) = proxy(server.local_addr().unwrap(), link).await;
    let client_endpoint = transport::client(&identity()).unwrap();
    let connecting = client_endpoint.connect(address, "frames.local").unwrap();
    let (viewer, host) = tokio::join!(connecting, async { server.accept().await.unwrap().await });
    let (viewer, host) = (viewer.unwrap(), host.unwrap());

    let stamps = Arc::new(Mutex::new(HashMap::new()));
    let (frames, captured) = generator(stamps.clone());
    let window = WindowId {
        id: 1,
        generation: 1,
    };
    let sender = crate::media_sender::MediaSender::new(host.clone());
    let (mut host_out, mut host_rx) = crate::session::test_outbound();
    host_out.media = Some(sender.clone());
    let (client_out, mut client_rx) = crate::session::test_outbound();
    let stream = start_host(
        Arc::new(Synthetic { frames }),
        window,
        capability.clone(),
        VideoOptions {
            quality: Default::default(),
            fps: FPS,
            show_cursor: false,
            bandwidth_mbps: 0,
        },
        host.clone(),
        host_out,
        Arc::new(AtomicU32::new(1)),
    );
    let one_way = Duration::from_secs_f64(link.delay_ms / 1000.0);
    let (to_client, mut at_client) = delayed(one_way);
    let (to_host, mut at_host) = delayed(one_way);
    {
        let to_client = to_client.clone();
        tokio::spawn(async move {
            while let Some(m) = host_rx.recv().await {
                let _ = to_client.send(m);
            }
        });
    }
    tokio::spawn(async move {
        while let Some(m) = client_rx.recv().await {
            let _ = to_host.send(m);
        }
    });

    let (ui, updates) = frames_native::client_channel(64);
    let shown = Arc::new(Mutex::new(Vec::<(Instant, u32, f64)>::new()));
    let reader = {
        let (shown, stamps) = (shown.clone(), stamps.clone());
        std::thread::spawn(move || {
            // Windows decoders hand back GPU textures. Read them back off the
            // measured path: the arrival time is taken first.
            #[cfg(windows)]
            let mut converter: Option<codec::GpuConverter> = None;
            for update in updates {
                let ClientUpdate::Frame(_, frame) = update else {
                    continue;
                };
                let arrived = Instant::now();
                #[cfg(windows)]
                let counter = match &frame.gpu {
                    Some(gpu) => {
                        let converter = converter.get_or_insert_with(|| {
                            codec::GpuConverter::new(&gpu.texture, frame.width, frame.height)
                                .unwrap()
                        });
                        let mut pixels = vec![0; (WIDTH * HEIGHT * 4) as usize];
                        converter
                            .readback(gpu, frame.width, frame.height, &mut pixels)
                            .ok()
                            .and_then(|_| read_counter_pixels(&pixels))
                    }
                    None => read_counter(&frame),
                };
                #[cfg(not(windows))]
                let counter = read_counter(&frame);
                if let Some(counter) = counter {
                    let now = arrived;
                    if let Some(at) = stamps.lock().unwrap().get(&counter) {
                        shown.lock().unwrap().push((
                            now,
                            counter,
                            now.duration_since(*at).as_secs_f64() * 1e3,
                        ));
                    }
                }
            }
        })
    };
    let client_media = ClientMediaWorker::new(viewer.clone(), ui, client_out);
    let started = Instant::now();
    let end = started + Duration::from_secs(seconds);
    let (mut keyframes, mut starts, mut errors) = (0u32, 0u32, Vec::<String>::new());
    let mut losses = 0u64;
    while Instant::now() < end {
        tokio::select! {
            Some(message) = at_client.recv() => match message {
                Message::StreamStarted(config) => { starts += 1; let _ = client_media.send(MediaCommand::Start(config)).await; }
                Message::StreamStopped { window, generation } => { let _ = client_media.send(MediaCommand::Stop(window, generation)).await; }
                Message::MediaStatus { window, generation, number } => { let _ = client_media.send(MediaCommand::Status(window, generation, number)).await; }
                Message::ClockReply { client_us, received_us, sent_us } => {
                    let _ = client_media.send(MediaCommand::Clock(client_us, received_us, sent_us, media::monotonic_micros())).await;
                }
                Message::Error { message } => errors.push(message),
                _ => {}
            },
            Some(message) = at_host.recv() => match message {
                Message::StreamReady { generation, .. } => { stream.acknowledge(generation); }
                Message::Keyframe { generation, .. } => { keyframes += 1; stream.request_keyframe(generation); }
                Message::ClockProbe { client_us } => {
                    let received_us = media::monotonic_micros();
                    let _ = to_client.send(Message::ClockReply { client_us, received_us, sent_us: media::monotonic_micros() });
                }
                Message::Feedback { generation, mut stats, .. } => {
                    // As session.rs: sender loss note, queue and path statistics.
                    if stats.delivery.missing_shards > losses {
                        sender.note_loss();
                    }
                    losses = stats.delivery.missing_shards;
                    stats.queue_micros = stats.queue_micros.max(sender.queue_micros());
                    let path = host.stats().path;
                    stats.congestion_events = path.congestion_events;
                    stats.congestion_window_bytes = path.cwnd;
                    stats.transport_lost_packets = path.lost_packets;
                    stream.feedback(generation, stats, path.rtt, path.min_rtt);
                }
                _ => {}
            },
            // As session.rs: the viewer's retransmission requests.
            datagram = host.read_datagram() => {
                if let Ok(datagram) = datagram && let Some(nack) = media::Nack::parse(&datagram) {
                    sender.retransmit(&nack);
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
    let bytes = down.bytes.load(Ordering::Relaxed);
    let link_loss = down.dropped.load(Ordering::Relaxed) as f64
        / down.packets.load(Ordering::Relaxed).max(1) as f64;
    let path = host.stats().path;
    drop(stream);
    drop(client_media);
    viewer.close(0u32.into(), b"done");
    let _ = reader.join();
    let shown = shown.lock().unwrap().clone();
    let warmup = started + Duration::from_secs(2);
    let steady: Vec<_> = shown.iter().filter(|(at, ..)| *at >= warmup).collect();
    let mut latency: Vec<f64> = steady.iter().map(|(.., ms)| *ms).collect();
    latency.sort_by(f64::total_cmp);
    let gaps: Vec<f64> = steady
        .windows(2)
        .map(|pair| pair[1].0.duration_since(pair[0].0).as_secs_f64() * 1e3)
        .collect();
    let stall_ms: f64 = gaps.iter().filter(|g| **g > 100.0).sum();
    let mut distinct: Vec<_> = steady.iter().map(|(_, c, _)| *c).collect();
    distinct.dedup();
    let steady_seconds = end.duration_since(warmup).as_secs_f64();
    serde_json::json!({
        "scenario": name,
        "congestion": std::env::var("FRAMES_CONGESTION").unwrap_or_else(|_| "default".into()),
        "encoder": capability.encoder,
        "captured": captured.load(Ordering::Relaxed),
        "first_frame_ms": shown.first().map(|(at, ..)| at.duration_since(started).as_secs_f64() * 1e3),
        "shown_fps": distinct.len() as f64 / steady_seconds,
        "latency_p50_ms": percentile(&latency, 0.5),
        "latency_p95_ms": percentile(&latency, 0.95),
        "latency_p99_ms": percentile(&latency, 0.99),
        "max_gap_ms": gaps.iter().copied().fold(0.0, f64::max),
        "stall_ms_per_min": stall_ms / steady_seconds * 60.0,
        "keyframe_requests": keyframes,
        "stream_starts": starts,
        "link_loss_pct": link_loss * 100.0,
        "mbps": bytes as f64 * 8.0 / 1e6 / seconds as f64,
        "quic_lost_packets": path.lost_packets,
        "retransmitted": sender.retransmitted(),
        "quic_congestion_events": path.congestion_events,
        "errors": errors,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark: uses the hardware/bundled encoders; runs for minutes"]
async fn netsim() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let seconds = std::env::var("FRAMES_NETSIM_SECONDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let names =
        std::env::var("FRAMES_NETSIM").unwrap_or_else(|_| "lan,wifi,lossy,constrained".into());
    for name in names.split(',') {
        let link = scenario(name).expect("known scenario");
        let result = run(name, link, seconds).await;
        println!("NETSIM {result}");
    }
}
