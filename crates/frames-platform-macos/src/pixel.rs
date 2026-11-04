//! Retained CVPixelBuffers as engine surfaces.

use crate::ffi;
use frames_media::api::{MediaClock, PlatformSurface, Surface};
use frames_media::clock::SystemClock;
use std::any::Any;
use std::ffi::c_void;
use std::sync::Arc;

/// One retain on an IOSurface-backed CVPixelBuffer, released on drop.
/// CoreVideo buffers are safe to retain, release, and read from any thread.
#[derive(Debug)]
pub struct PixelBuffer(*mut c_void);

// SAFETY: CVPixelBuffer reference counting is thread-safe, and this type
// only hands the pointer to APIs that accept it from any thread.
unsafe impl Send for PixelBuffer {}
unsafe impl Sync for PixelBuffer {}

impl PixelBuffer {
    /// Takes ownership of a +1 retained buffer; `None` for null.
    ///
    /// # Safety
    /// `raw` must be null or a CVPixelBufferRef whose retain the caller
    /// transfers.
    pub unsafe fn from_retained(raw: *mut c_void) -> Option<Self> {
        (!raw.is_null()).then_some(Self(raw))
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// A generated BGRA picture; identical pixels for the same `index`.
    pub fn synthetic(width: u32, height: u32, index: u64) -> Option<Self> {
        // SAFETY: returns a +1 buffer or null.
        unsafe { Self::from_retained(ffi::frames_mac_synthetic_frame(width, height, index)) }
    }

    /// Mean absolute difference per B, G, R channel (0-255), if comparable.
    pub fn difference(&self, other: &Self) -> Option<f64> {
        // SAFETY: both buffers are alive for the call.
        let difference = unsafe { ffi::frames_mac_pixel_buffer_difference(self.0, other.0) };
        (difference >= 0.0).then_some(difference)
    }

    pub fn into_surface(self) -> Surface {
        Surface::Platform(Arc::new(self))
    }

    /// The buffer behind a platform surface, if it is one of ours.
    pub fn from_surface(surface: &Surface) -> Option<&Self> {
        match surface {
            Surface::Platform(surface) => surface.as_any().downcast_ref::<Self>(),
            Surface::Cpu(_) => None,
        }
    }
}

impl Clone for PixelBuffer {
    fn clone(&self) -> Self {
        // SAFETY: an extra retain, released by the clone's drop.
        unsafe { ffi::CFRetain(self.0) };
        Self(self.0)
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        // SAFETY: this value owns exactly one retain.
        unsafe { ffi::CFRelease(self.0) };
    }
}

impl PlatformSurface for PixelBuffer {
    fn width(&self) -> u32 {
        // SAFETY: the buffer is alive.
        unsafe { ffi::CVPixelBufferGetWidth(self.0) as u32 }
    }

    fn height(&self) -> u32 {
        // SAFETY: the buffer is alive.
        unsafe { ffi::CVPixelBufferGetHeight(self.0) as u32 }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Converts a Swift-side uptime timestamp to [`SystemClock`] microseconds by
/// its distance from now on both clocks.
pub fn uptime_to_clock_us(uptime_ns: u64) -> u64 {
    let age_us = ffi::uptime_ns().saturating_sub(uptime_ns) / 1000;
    SystemClock::new().now_us().saturating_sub(age_us)
}
