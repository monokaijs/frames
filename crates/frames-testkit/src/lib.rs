//! Deterministic, virtual-time test bed for the Frames v2 media engine.
//!
//! Everything here is driven by a [`clock::VirtualClock`] and seeded
//! randomness, so a scenario replays bit-for-bit: the same seed gives the same
//! losses, the same queueing, the same decoded frames. Real backends never
//! appear; the synthetic ones implement the `frames_media::api` traits.
//!
//! - [`clock`]: virtual microsecond clock.
//! - [`rng`]: small seeded generator (no dependency).
//! - [`link`]: one-way network link: delay, jitter, random and burst loss,
//!   bottleneck rate, drop-tail queue.
//! - [`transport`]: a pair of links as a [`frames_media::api::MediaTransport`].
//! - [`capture`]: synthetic capture source at a fixed frame rate.
//! - [`codec`]: synthetic encoder and decoder that track reference chains,
//!   so recovery (invalidation, long-term references, intra refresh) is
//!   observable: a frame predicted from a lost frame fails to decode.

pub mod capture;
pub mod clock;
pub mod codec;
pub mod link;
pub mod rng;
pub mod transport;
