//! Metal presentation as a [`Presenter`].

use crate::ffi;
use crate::pixel::{PixelBuffer, uptime_to_clock_us};
use frames_media::api::{
    DecodedFrame, MediaError, PresentCaps, PresentMode, PresentStamp, Presenter,
};
use std::ffi::c_void;

/// Draws decoded IOSurfaces with Metal on the calling thread (the engine's
/// presenter thread). [`PresentMode::Immediate`] disables display sync so a
/// frame reaches the glass as soon as it is drawn.
pub struct MacPresenter {
    handle: *mut c_void,
    refresh_hz: u32,
}

// SAFETY: the Swift presenter renders on whichever thread calls it, one at a
// time (`&mut self`), and is freed once.
unsafe impl Send for MacPresenter {}

impl MacPresenter {
    /// With `window`, the main thread must be running NSApplication
    /// ([`crate::system::run_app`]); without, it renders offscreen.
    pub fn new(width: u32, height: u32, window: bool) -> Result<Self, MediaError> {
        // SAFETY: returns a retained Swift object or null.
        let handle = unsafe { ffi::frames_mac_presenter_new(width, height, window) };
        if handle.is_null() {
            return Err(MediaError::Backend("Metal presenter unavailable".into()));
        }
        // SAFETY: live presenter.
        let refresh_hz = unsafe { ffi::frames_mac_presenter_refresh_hz(handle) }.round() as u32;
        Ok(Self { handle, refresh_hz })
    }
}

impl MacPresenter {
    /// When the most recently presented frame reached the glass
    /// ([`frames_media::clock::SystemClock`] µs), for windowed presenters.
    /// `present` returns once the GPU work is scheduled; with vsync the glass
    /// time can be up to one refresh later.
    pub fn last_on_glass_us(&self) -> Option<u64> {
        // SAFETY: live presenter.
        let ns = unsafe { ffi::frames_mac_presenter_last_on_glass_ns(self.handle) };
        (ns > 0).then(|| uptime_to_clock_us(ns))
    }
}

impl Presenter for MacPresenter {
    fn caps(&self) -> PresentCaps {
        PresentCaps {
            tearing: true,
            variable_refresh: self.refresh_hz > 60,
            refresh_hz: self.refresh_hz,
        }
    }

    fn wait_ready(&mut self, _deadline_us: u64) -> bool {
        // `present` itself waits for a drawable (at most two are in flight).
        true
    }

    fn present(
        &mut self,
        frame: DecodedFrame,
        mode: PresentMode,
    ) -> Result<PresentStamp, MediaError> {
        let buffer = PixelBuffer::from_surface(&frame.surface)
            .ok_or(MediaError::Unsupported("presenting a non-IOSurface frame"))?;
        let mut presented_ns = 0;
        // SAFETY: live presenter; the buffer outlives the call.
        let status = unsafe {
            ffi::frames_mac_presenter_present(
                self.handle,
                buffer.as_ptr(),
                mode == PresentMode::Vsync,
                &mut presented_ns,
            )
        };
        if status != 0 {
            return Err(MediaError::Backend(format!(
                "Metal present failed ({status})"
            )));
        }
        Ok(PresentStamp {
            frame: frame.frame,
            presented_us: uptime_to_clock_us(presented_ns),
        })
    }
}

impl Drop for MacPresenter {
    fn drop(&mut self) {
        // SAFETY: closes the window and frees the presenter exactly once.
        unsafe { ffi::frames_mac_presenter_free(self.handle) };
    }
}
