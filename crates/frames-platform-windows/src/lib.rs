//! Windows platform layer for the v2 media engine.
//!
//! - [`capture`]: Windows Graphics Capture as a `CaptureSource`, with the
//!   cursor reported separately and the update interval matched to the target
//!   frame rate.
//! - [`clipboard`]: the Windows clipboard (text and PNG) for clipboard
//!   sharing.
//! - [`input`]: applies v2 input events with `SendInput`, or by message to
//!   one window for QA runs.
//! - [`nvprofile`]: the NVIDIA driver profile that holds the GPU in its
//!   high-performance state for this program while a session runs.
//! - [`present`]: a flip-model `Presenter` that blits decoded D3D11 surfaces
//!   with the video processor and presents with tearing when allowed.
//! - [`system`]: MMCSS, GPU scheduling priority, WLAN media-streaming mode,
//!   qWAVE DSCP marking, and a GPU clock hold for low encode latency.

#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod clipboard;
#[cfg(windows)]
pub mod input;
#[cfg(windows)]
pub mod nvprofile;
#[cfg(windows)]
pub mod present;
#[cfg(windows)]
pub mod secure;
#[cfg(windows)]
pub mod system;
// Isolated input needs the adapter, built for x64 MSVC only (build.rs);
// other Windows builds get a stand-in that cannot attach.
#[cfg(all(windows, frames_shared_input))]
pub mod shared_input;
#[cfg(all(windows, not(frames_shared_input)))]
#[path = "shared_input_unavailable.rs"]
pub mod shared_input;
