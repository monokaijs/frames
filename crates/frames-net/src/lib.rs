//! Frames v2 networking on iroh (QUIC with hole punching and relay fallback).
//!
//! - [`identity`]: the device key, derived from the v1 identity so existing
//!   installations keep one stable identity without storing a new secret.
//! - [`local`]: nearby devices on the local network (mDNS), for direct
//!   connections without internet discovery.
//! - [`migration`]: proof that a v2 device key belongs to a v1 identity, so
//!   v1 trust entries carry over (docs/V2_PLAN.md, Phase 4).
//! - [`congestion`]: QUIC congestion control that follows the media engine's
//!   target while media flows, and plain Cubic otherwise.
//! - [`trust`]: the devices this one trusts, by device id, and whether a
//!   connecting device is let in or asked about.
//! - [`node`]: binding, dialing by device id, sessions, pairing codes, and
//!   the media datagram channel implementing
//!   [`frames_media::api::MediaTransport`].
//!
//! Peers are always dialed by device id with relay and address discovery:
//! forcing a single direct address can fail on multihomed hosts (see
//! spikes/iroh-transport/RESULTS.md).

pub mod congestion;
pub mod identity;
pub mod local;
pub mod migration;
pub mod node;
pub mod trust;

/// Media bitrate ceiling on n0's public relays: they deliver about
/// 8-10 Mbps per connection (spike 0-A), so leave headroom for FEC bursts.
pub const N0_RELAY_MAX_BPS: u32 = 6_000_000;

pub use identity::DeviceKey;
pub use node::{Incoming, MediaChannel, Node, NodeConfig, Relays, Session};
