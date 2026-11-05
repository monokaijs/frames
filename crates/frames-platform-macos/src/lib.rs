//! Frames v2 macOS backends for the `frames_media::api` traits.
//!
//! - [`capture::MacCapture`]: ScreenCaptureKit, cursor reported separately.
//! - [`encoder::MacEncoder`]: hardware VideoToolbox, infinite GOP, long-term
//!   references through VideoToolbox's acknowledgement tokens.
//! - [`decoder::MacDecoder`]: hardware VideoToolbox to IOSurface BGRA.
//! - [`presenter::MacPresenter`]: Metal, immediate or vsync presentation.
//! - [`audio::MacAudio`]: host audio from ScreenCaptureKit (the whole Mac
//!   or one application).
//! - [`clipboard::MacClipboard`]: the general pasteboard (text and PNG).
//! - [`input::MacInjector`]: applies viewer input with Quartz events
//!   (needs the Accessibility permission).
//! - [`system`]: the AppKit run loop for headless hosts, real-time threads,
//!   and Wi-Fi video traffic marking.
//!
//! Surfaces stay on the GPU: frames travel as [`pixel::PixelBuffer`]
//! (a retained CVPixelBuffer) inside `Surface::Platform`, from capture to
//! encoder and from decoder to presenter, without copies. The Swift side is
//! native/macos/FramesMac, built and linked by build.rs. On other platforms
//! this crate is empty.
#![cfg(target_os = "macos")]

pub mod audio;
pub mod capture;
pub mod clipboard;
pub mod decoder;
pub mod encoder;
mod ffi;
pub mod input;
pub mod pixel;
pub mod presenter;
pub mod system;

#[cfg(test)]
mod tests;
