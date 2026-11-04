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

pub struct Node {
    endpoint: Endpoint,
    alpn: Vec<u8>,
    /// Congestion target shared by accepted connections. The endpoint's
    /// server configuration is per endpoint, so a host serves one media
    /// session at a time (as v1 allows one controlling device). Outgoing
    /// connections each get their own target.
    accepted_target: MediaTarget,
    _timer: TimerResolution,
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
            _timer: TimerResolution::raise(),
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
        Ok(Session::new(connection, target))
    }

    /// The next incoming session, or `None` once the node is closed.
    pub async fn accept(&self) -> Option<Result<Session>> {
        let incoming = self.endpoint.accept().await?;
        Some(
            incoming
                .await
                .map(|connection| Session::new(connection, self.accepted_target.clone()))
                .map_err(|error| anyhow!("accepting: {error}")),
        )
    }

    pub async fn close(&self) {
        self.endpoint.close().await;
    }
}

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
    inbox: Mutex<VecDeque<Bytes>>,
    target: MediaTarget,
}

impl MediaChannel {
    fn spawn(connection: Connection, target: MediaTarget) -> Arc<Self> {
        let channel = Arc::new(Self {
            connection: connection.clone(),
            inbox: Mutex::new(VecDeque::new()),
            target,
        });
        let weak = Arc::downgrade(&channel);
        tokio::spawn(async move {
            while let Ok(datagram) = connection.read_datagram().await {
                let Some(channel) = weak.upgrade() else { break };
                let mut inbox = channel.inbox.lock().unwrap();
                if inbox.len() >= INBOX_LIMIT {
                    inbox.pop_front();
                }
                inbox.push_back(datagram);
            }
        });
        channel
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
        self.inbox.lock().unwrap().pop_front()
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
