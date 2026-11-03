//! Frames v2 connection protocol: the part every peer must agree on before any
//! service runs.
//!
//! This crate deliberately owns very little:
//! - [`channel`]: channel identifiers that route reliable-stream frames to a
//!   service.
//! - [`envelope`]: length-prefixed framing of those reliable streams.
//! - [`handshake`]: the `Hello` exchange and per-feature version negotiation,
//!   which replace the v1 rule that both builds must match exactly.
//!
//! Domain messages (media control, input, transfers, devices, ...) belong to
//! the crate that implements the service and are versioned through features.
//! The real-time media packet format is binary and lives in `frames-media`.

pub mod channel;
pub mod envelope;
pub mod handshake;

/// ALPN for v2 connections. v1 peers (`frames/11`) never negotiate it.
pub const ALPN: &[u8] = b"frames/2";

/// Incompatible protocol generations use a different major and ALPN.
pub const PROTOCOL_MAJOR: u16 = 2;
