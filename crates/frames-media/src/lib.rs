//! Frames v2 real-time media engine.
//!
//! - [`wire`]: the frozen binary packet format (media, feedback, probes).
//! - [`seq`]: unwrapping of the 32-bit sequence numbers and timestamps that
//!   the wire format carries into monotonic 64-bit values.
//! - [`api`]: the boundary traits that codec, capture, presentation, and
//!   transport backends implement. The engine (Phase 2) depends only on
//!   these, so the same pipeline runs against hardware backends and against
//!   the synthetic ones in `frames-testkit`.

pub mod api;
pub mod client;
pub mod clock;
pub mod fec;
pub mod host;
pub mod rate;
pub mod seq;
pub mod wire;
