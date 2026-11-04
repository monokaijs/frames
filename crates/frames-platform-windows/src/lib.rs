//! Windows platform layer for the v2 media engine.
//!
//! - [`capture`]: Windows Graphics Capture as a `CaptureSource`, with the
//!   cursor reported separately and the update interval matched to the target
//!   frame rate.
//! - [`input`]: applies v2 input events with `SendInput`, or by message to
//!   one window for QA runs.
//! - [`present`]: a flip-model `Presenter` that blits decoded D3D11 surfaces
//!   with the video processor and presents with tearing when allowed.
//! - [`system`]: MMCSS, GPU scheduling priority, WLAN media-streaming mode,
//!   qWAVE DSCP marking, and a GPU clock hold for low encode latency.

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod input;
#[cfg(windows)]
pub mod present;
#[cfg(windows)]
pub mod system;
