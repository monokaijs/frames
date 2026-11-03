//! Two simulated links as a pair of [`MediaTransport`] endpoints.

use crate::clock::VirtualClock;
use crate::link::{Link, LinkConfig, LinkStats};
use bytes::Bytes;
use frames_media::api::{Backpressure, MediaClock, MediaTransport, PathInfo, PathKind};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

struct Shared {
    clock: VirtualClock,
    links: [Mutex<Link>; 2],
    inboxes: [Mutex<VecDeque<Bytes>>; 2],
    target_bps: [AtomicU64; 2],
    max_datagram: usize,
}

/// One side of a simulated connection. Endpoint 0 sends on link 0 and
/// receives from link 1; endpoint 1 the reverse.
#[derive(Clone)]
pub struct SimEndpoint {
    shared: Arc<Shared>,
    side: usize,
}

/// Creates a connected pair: `(host, client)`.
pub fn pair(
    clock: &VirtualClock,
    host_to_client: LinkConfig,
    client_to_host: LinkConfig,
    seed: u64,
) -> (SimEndpoint, SimEndpoint) {
    let shared = Arc::new(Shared {
        clock: clock.clone(),
        links: [
            Mutex::new(Link::new(host_to_client, seed)),
            Mutex::new(Link::new(client_to_host, seed ^ 0xA5A5_A5A5)),
        ],
        inboxes: [Mutex::new(VecDeque::new()), Mutex::new(VecDeque::new())],
        target_bps: [AtomicU64::new(0), AtomicU64::new(0)],
        max_datagram: 1200,
    });
    (
        SimEndpoint {
            shared: shared.clone(),
            side: 0,
        },
        SimEndpoint { shared, side: 1 },
    )
}

impl SimEndpoint {
    fn outgoing(&self) -> &Mutex<Link> {
        &self.shared.links[self.side]
    }

    fn incoming(&self) -> &Mutex<Link> {
        &self.shared.links[1 - self.side]
    }

    /// Statistics of the link this endpoint sends on.
    pub fn send_stats(&self) -> LinkStats {
        self.outgoing().lock().unwrap().stats()
    }

    /// Bytes waiting at the bottleneck of the link this endpoint sends on.
    pub fn send_queued_bytes(&self) -> u64 {
        let now = self.shared.clock.now_us();
        self.outgoing().lock().unwrap().queued_bytes(now)
    }

    /// Changes the conditions of the link this endpoint sends on.
    pub fn set_send_config(&self, config: LinkConfig) {
        self.outgoing().lock().unwrap().set_config(config);
    }

    /// The rate last requested through [`MediaTransport::set_target_rate`].
    pub fn target_rate(&self) -> u64 {
        self.shared.target_bps[self.side].load(Ordering::Relaxed)
    }

    /// When the next packet reaches this endpoint, for virtual-time drivers.
    pub fn next_arrival_us(&self) -> Option<u64> {
        if !self.shared.inboxes[self.side].lock().unwrap().is_empty() {
            return Some(self.shared.clock.now_us());
        }
        self.incoming().lock().unwrap().next_arrival_us()
    }
}

impl MediaTransport for SimEndpoint {
    fn path(&self) -> PathInfo {
        let forward = self.outgoing().lock().unwrap().config().delay_us;
        let back = self.incoming().lock().unwrap().config().delay_us;
        PathInfo {
            kind: PathKind::Direct,
            rtt_us: forward + back,
            max_datagram: self.shared.max_datagram,
        }
    }

    fn try_send(&self, datagram: Bytes) -> Result<(), Backpressure> {
        if datagram.len() > self.shared.max_datagram {
            return Err(Backpressure);
        }
        let now = self.shared.clock.now_us();
        self.outgoing().lock().unwrap().send(now, datagram);
        Ok(())
    }

    fn try_recv(&self) -> Option<Bytes> {
        let now = self.shared.clock.now_us();
        let mut inbox = self.shared.inboxes[self.side].lock().unwrap();
        if inbox.is_empty() {
            inbox.extend(self.incoming().lock().unwrap().deliver(now));
        }
        inbox.pop_front()
    }

    fn set_target_rate(&self, bits_per_second: u64) {
        self.shared.target_bps[self.side].store(bits_per_second, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_exchange_datagrams_in_virtual_time() {
        let clock = VirtualClock::new(0);
        let (host, client) = pair(&clock, LinkConfig::lan(), LinkConfig::lan(), 3);
        host.try_send(Bytes::from_static(b"frame")).unwrap();
        client.try_send(Bytes::from_static(b"feedback")).unwrap();
        assert!(
            client.try_recv().is_none(),
            "nothing arrives before the delay"
        );
        let arrival = client.next_arrival_us().unwrap();
        assert!(arrival >= 500);
        clock.advance_to(arrival);
        assert_eq!(client.try_recv().unwrap(), Bytes::from_static(b"frame"));
        assert_eq!(host.try_recv().unwrap(), Bytes::from_static(b"feedback"));
        assert_eq!(host.path().rtt_us, 1_000);
        assert!(host.try_send(Bytes::from(vec![0; 1201])).is_err());
        host.set_target_rate(5_000_000);
        assert_eq!(host.target_rate(), 5_000_000);
        assert_eq!(client.target_rate(), 0);
        assert_eq!(host.send_stats().sent, 1);
    }
}
