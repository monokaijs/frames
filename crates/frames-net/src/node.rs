//! Endpoint, sessions, and the media datagram channel.

use crate::congestion::{MediaControllerFactory, MediaTarget};
use crate::identity::DeviceKey;
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use frames_media::api::{Backpressure, MediaTransport, PathInfo, PathKind};
use iroh::endpoint::{Connection, QuicTransportConfig, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Datagrams waiting for the engine beyond this are dropped (oldest first):
/// a stalled consumer must not grow memory, and stale media is useless.
const INBOX_LIMIT: usize = 8192;
/// A host's "still here" datagram, ten a second: a viewer whose selected
/// direct path delivers nothing (not even these) drops that path and
/// carries on over the relay ([`Session::guard_direct_paths`]). Media
/// datagrams start with their version in the top nibble (2), audio with
/// 0x02; this one never reaches the engine.
pub const HEARTBEAT: u8 = 0x03;
const HEARTBEAT_EVERY: Duration = Duration::from_millis(100);
/// How long a selected direct path may deliver nothing before it is dropped.
const DIRECT_SILENCE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug)]
pub enum Relays {
    /// n0's public relays (rate-limited; fine for development and pairing).
    N0,
    /// Self-hosted iroh-relay servers.
    Custom(Vec<RelayUrl>),
    /// Direct connections only (tests, LAN-only deployments).
    Disabled,
}

#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub alpn: Vec<u8>,
    pub relays: Relays,
    /// Publish and resolve addresses through n0's DNS discovery.
    pub discovery: bool,
    /// Bind a fixed IPv4 address/port instead of an ephemeral one.
    pub bind: Option<SocketAddr>,
    /// Use only relays (tests of degraded paths, UDP-blocked networks).
    pub relay_only: bool,
}

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            alpn: frames_proto::ALPN.to_vec(),
            relays: Relays::N0,
            discovery: true,
            bind: None,
            relay_only: false,
        }
    }
}

/// Cheap to clone: clones share the endpoint (a viewer connects its
/// windows' sessions from their own tasks).
#[derive(Clone)]
pub struct Node {
    endpoint: Endpoint,
    alpn: Vec<u8>,
    /// Congestion target shared by accepted connections. The endpoint's
    /// server configuration is per endpoint, so a host serves one media
    /// session at a time (as v1 allows one controlling device). Outgoing
    /// connections each get their own target.
    accepted_target: MediaTarget,
    _timer: Arc<TimerResolution>,
}

/// Windows timers tick every 15.6 ms by default. QUIC paces datagrams with
/// timers, so at that resolution it sends below the media target, datagrams
/// queue, and the oldest are dropped (a loopback stream lost 56% of frames at
/// 150 ms latency; with 1 ms, none at 8 ms). Held for the node's lifetime,
/// like v1's media sender.
struct TimerResolution;

#[cfg(windows)]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}

impl TimerResolution {
    fn raise() -> Self {
        #[cfg(windows)]
        unsafe {
            timeBeginPeriod(1);
        }
        Self
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            timeEndPeriod(1);
        }
    }
}

impl Node {
    pub async fn bind(key: &DeviceKey, config: NodeConfig) -> Result<Self> {
        let accepted_target = MediaTarget::default();
        let transport = transport_config(accepted_target.clone());
        let mut builder = if config.discovery {
            Endpoint::builder(presets::N0)
        } else {
            Endpoint::builder(presets::Minimal)
        }
        .secret_key(key.secret())
        .alpns(vec![config.alpn.clone()])
        .transport_config(transport);
        builder = match &config.relays {
            Relays::N0 => builder.relay_mode(RelayMode::Default),
            Relays::Custom(urls) => {
                builder.relay_mode(RelayMode::Custom(urls.iter().cloned().collect()))
            }
            Relays::Disabled => builder
                .relay_mode(RelayMode::Disabled)
                .clear_relay_transports(),
        };
        if config.relay_only {
            builder = builder.clear_ip_transports();
        }
        if let Some(address) = config.bind {
            builder = builder
                .bind_addr(address)
                .map_err(|error| anyhow!("invalid bind address {address}: {error}"))?;
        }
        let endpoint = builder.bind().await.context("binding the endpoint")?;
        Ok(Self {
            endpoint,
            alpn: config.alpn,
            accepted_target,
            _timer: Arc::new(TimerResolution::raise()),
        })
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Everything a peer needs to reach this node right now.
    pub fn addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    pub fn bound_sockets(&self) -> Vec<SocketAddr> {
        self.endpoint.bound_sockets()
    }

    /// Waits until a relay is reachable (needed before others can dial in
    /// from outside the LAN).
    pub async fn online(&self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, self.endpoint.online())
            .await
            .context("no relay reachable")
    }

    pub async fn connect(&self, peer: impl Into<EndpointAddr>) -> Result<Session> {
        let target = MediaTarget::default();
        let options = iroh::endpoint::ConnectOptions::new()
            .with_transport_config(transport_config(target.clone()));
        let connecting = self
            .endpoint
            .connect_with_opts(peer.into(), &self.alpn, options)
            .await
            .context("connecting")?;
        let connection = connecting.await.context("connecting")?;
        let session = Session::new(connection, target);
        session.guard_direct_paths();
        Ok(session)
    }

    /// The next incoming session, or `None` once the node is closed.
    pub async fn accept(&self) -> Option<Result<Session>> {
        Some(self.incoming().await?.session().await)
    }

    /// The next connection attempt, before its handshake. Hosts finish each
    /// one on its own task ([`Incoming::session`]): a handshake that stalls
    /// (a peer whose path does not work) must not hold up the others.
    pub async fn incoming(&self) -> Option<Incoming> {
        Some(Incoming {
            accept: self.endpoint.accept().await?,
            target: self.accepted_target.clone(),
        })
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }
}

/// A connection attempt whose handshake has not finished.
pub struct Incoming {
    accept: iroh::endpoint::Incoming,
    target: MediaTarget,
}

impl Incoming {
    /// Finishes the handshake, giving up after [`HANDSHAKE_TIMEOUT`].
    pub async fn session(self) -> Result<Session> {
        let connection = tokio::time::timeout(HANDSHAKE_TIMEOUT, self.accept)
            .await
            .map_err(|_| anyhow!("accepting: no handshake within {HANDSHAKE_TIMEOUT:?}"))?
            .map_err(|error| anyhow!("accepting: {error}"))?;
        let session = Session::new(connection, self.target);
        session.send_heartbeats();
        Ok(session)
    }
}

/// How long an incoming connection has to finish its handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// One authenticated connection to a peer.
#[derive(Clone)]
pub struct Session {
    connection: Connection,
    media: Arc<MediaChannel>,
}

impl Session {
    fn new(connection: Connection, target: MediaTarget) -> Self {
        let media = MediaChannel::spawn(connection.clone(), target);
        Self { connection, media }
    }

    pub fn remote(&self) -> EndpointId {
        self.connection.remote_id()
    }

    /// 32 bytes of keying material bound to this connection's TLS session;
    /// both ends compute the same value.
    pub fn exporter(&self, label: &[u8]) -> Result<[u8; 32]> {
        let mut out = [0u8; 32];
        self.connection
            .export_keying_material(&mut out, label, b"")
            .map_err(|error| anyhow!("exporter unavailable: {error:?}"))?;
        Ok(out)
    }

    /// The 8-digit code both users compare when pairing.
    pub fn pairing_code(&self) -> Result<String> {
        let bytes = self.exporter(b"frames-pairing-code")?;
        let value = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
        Ok(format!("{:08}", value % 100_000_000))
    }

    pub fn media(&self) -> Arc<MediaChannel> {
        self.media.clone()
    }

    /// Host: sends [`HEARTBEAT`]s until the session closes, so the viewer
    /// can tell a dead path from a still picture.
    fn send_heartbeats(&self) {
        let connection = self.connection.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(HEARTBEAT_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            while connection.close_reason().is_none() {
                tick.tick().await;
                let _ = connection.send_datagram(Bytes::from_static(&[HEARTBEAT]));
            }
        });
    }

    /// Viewer: drops the selected direct path when nothing (not even the
    /// host's heartbeats) arrived on it for [`DIRECT_SILENCE`], so the
    /// session carries on over the relay within a second instead of
    /// freezing until the path times out. Seen on a LAN with a Mac host and
    /// a Windows viewer, where the direct path goes silent one way (an
    /// iroh multipath issue, vendor/iroh/FRAMES-PATCH.md).
    fn guard_direct_paths(&self) {
        let (connection, media) = (self.connection.clone(), self.media.clone());
        tokio::spawn(async move {
            let clock = frames_media::clock::SystemClock::new();
            let mut tick = tokio::time::interval(Duration::from_millis(200));
            // The direct path selected, and when it was first seen selected.
            let mut watching: Option<(iroh::endpoint::PathId, u64)> = None;
            while connection.close_reason().is_none() {
                tick.tick().await;
                let paths = connection.paths();
                let selected = paths.iter().find(|p| p.is_selected());
                let has_relay = paths.iter().any(|p| p.is_relay());
                let Some(path) = selected.filter(|p| p.is_ip() && has_relay) else {
                    watching = None;
                    continue;
                };
                let now = frames_media::api::MediaClock::now_us(&clock);
                let since = match watching {
                    Some((id, since)) if id == path.id() => since,
                    _ => {
                        watching = Some((path.id(), now));
                        now
                    }
                };
                let last = media
                    .last_arrival
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .max(since);
                if now.saturating_sub(last) > DIRECT_SILENCE.as_micros() as u64 {
                    println!(
                        "network: the direct path to {:?} stopped delivering; using the relay",
                        path.remote_addr()
                    );
                    connection.close_path(path.id());
                    watching = None;
                }
            }
        });
    }

    /// A reliable, ordered channel (one QUIC stream pair).
    pub async fn open_channel(
        &self,
    ) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)> {
        self.connection.open_bi().await.context("opening a channel")
    }

    pub async fn accept_channel(
        &self,
    ) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream)> {
        self.connection
            .accept_bi()
            .await
            .context("accepting a channel")
    }

    pub fn close(&self, reason: &str) {
        self.connection.close(0u32.into(), reason.as_bytes());
    }

    /// Why the connection closed, once it has: the peer's reason text when
    /// it closed deliberately (for example "not approved").
    pub fn close_reason(&self) -> Option<String> {
        self.connection.close_reason().map(|reason| match reason {
            iroh::endpoint::ConnectionError::ApplicationClosed(close) => {
                String::from_utf8_lossy(&close.reason).into_owned()
            }
            other => other.to_string(),
        })
    }

    /// True once the connection closed, from either side.
    pub fn is_closed(&self) -> bool {
        self.connection.close_reason().is_some()
    }

    pub async fn closed(&self) {
        self.connection.closed().await;
    }
}

/// Media datagrams for the engine: non-blocking send, polled receive.
pub struct MediaChannel {
    connection: Connection,
    /// When the last datagram (heartbeats included) arrived, SystemClock µs.
    last_arrival: std::sync::atomic::AtomicU64,
    /// Datagrams with their arrival time (SystemClock µs).
    inbox: Mutex<VecDeque<(Bytes, u64)>>,
    target: MediaTarget,
}

impl MediaChannel {
    fn spawn(connection: Connection, target: MediaTarget) -> Arc<Self> {
        let channel = Arc::new(Self {
            connection: connection.clone(),
            last_arrival: std::sync::atomic::AtomicU64::new(0),
            inbox: Mutex::new(VecDeque::new()),
            target,
        });
        let weak = Arc::downgrade(&channel);
        tokio::spawn(async move {
            let clock = frames_media::clock::SystemClock::new();
            while let Ok(datagram) = connection.read_datagram().await {
                let arrived = frames_media::api::MediaClock::now_us(&clock);
                let Some(channel) = weak.upgrade() else { break };
                channel
                    .last_arrival
                    .store(arrived, std::sync::atomic::Ordering::Relaxed);
                if datagram.as_ref() == [HEARTBEAT] {
                    continue;
                }
                let mut inbox = channel.inbox.lock().unwrap();
                if inbox.len() >= INBOX_LIMIT {
                    inbox.pop_front();
                }
                inbox.push_back((datagram, arrived));
            }
        });
        channel
    }

    /// Identifies the selected path (its remote address), so callers can
    /// tell when the connection moved, including between two direct paths
    /// (Wi-Fi to Ethernet).
    pub fn selected_path_key(&self) -> Option<String> {
        self.connection
            .paths()
            .iter()
            .find(|path| path.is_selected())
            .map(|path| format!("{:?}", path.remote_addr()))
    }

    /// Every known path, for diagnostics: `local -> remote`, selected first.
    pub fn describe_paths(&self) -> Vec<String> {
        let mut out: Vec<(bool, String)> = self
            .connection
            .paths()
            .iter()
            .map(|path| {
                (
                    path.is_selected(),
                    format!(
                        "{}{:?} -> {:?} rtt {:.1} ms",
                        if path.is_selected() { "* " } else { "  " },
                        path.local_addr(),
                        path.remote_addr(),
                        path.rtt().as_secs_f64() * 1e3
                    ),
                )
            })
            .collect();
        out.sort_by_key(|(selected, _)| !selected);
        out.into_iter().map(|(_, text)| text).collect()
    }

    /// The selected path: relay or direct, its RTT, and datagram limit.
    fn selected_path(&self) -> (PathKind, Option<Duration>) {
        for path in self.connection.paths().iter() {
            if path.is_selected() {
                let kind = if path.is_relay() {
                    PathKind::Relay
                } else {
                    PathKind::Direct
                };
                return (kind, Some(path.rtt()));
            }
        }
        (PathKind::Direct, None)
    }
}

impl MediaTransport for MediaChannel {
    fn path(&self) -> PathInfo {
        let (kind, rtt) = self.selected_path();
        PathInfo {
            kind,
            rtt_us: rtt.map_or(50_000, |rtt| rtt.as_micros() as u64),
            max_datagram: self.connection.max_datagram_size().unwrap_or(1162),
        }
    }

    fn try_send(&self, datagram: Bytes) -> Result<(), Backpressure> {
        self.connection
            .send_datagram(datagram)
            .map_err(|_| Backpressure)
    }

    fn try_recv(&self) -> Option<Bytes> {
        self.inbox
            .lock()
            .unwrap()
            .pop_front()
            .map(|(datagram, _)| datagram)
    }

    fn try_recv_timed(&self) -> Option<(Bytes, Option<u64>)> {
        self.inbox
            .lock()
            .unwrap()
            .pop_front()
            .map(|(datagram, arrived)| (datagram, Some(arrived)))
    }

    fn set_target_rate(&self, bits_per_second: u64) {
        self.target.set(bits_per_second);
    }
}

/// Builds the transport config whose congestion controller follows `target`.
pub fn transport_config(target: MediaTarget) -> QuicTransportConfig {
    QuicTransportConfig::builder()
        .datagram_receive_buffer_size(Some(8 << 20))
        .datagram_send_buffer_size(1 << 20)
        .keep_alive_interval(Duration::from_secs(5))
        .congestion_controller_factory(Arc::new(MediaControllerFactory { target }))
        .build()
}
