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
    /// The installed input handler, freed after Swift lets go of it.
    input: Option<*mut InputHandler>,
}

/// Input from the presenter window, with the event's time on the
/// [`frames_media::clock::SystemClock`] (µs). Positions are 0-65535 over the
/// view, origin top left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MacInput {
    Move {
        x: u16,
        y: u16,
        at_us: u64,
    },
    /// `button`: 0 left, 1 right, 2 middle, 3 back, 4 forward.
    Button {
        button: u8,
        down: bool,
        x: u16,
        y: u16,
        at_us: u64,
    },
    /// In 1/120 notches; positive is right and up.
    Wheel {
        dx: i16,
        dy: i16,
        at_us: u64,
    },
    /// A macOS virtual key code.
    Key {
        code: u16,
        down: bool,
        at_us: u64,
    },
    FocusLost {
        at_us: u64,
    },
}

type InputHandler = Box<dyn FnMut(MacInput) + Send>;

unsafe extern "C" fn on_input(
    context: *mut c_void,
    kind: u32,
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    event_ns: u64,
) {
    let at_us = uptime_to_clock_us(event_ns);
    let clamp16 = |v: i32| v.clamp(0, 65535) as u16;
    let event = match kind {
        ffi::INPUT_MOVE => MacInput::Move {
            x: clamp16(a),
            y: clamp16(b),
            at_us,
        },
        ffi::INPUT_BUTTON => MacInput::Button {
            button: a.clamp(0, 4) as u8,
            down: b != 0,
            x: clamp16(c),
            y: clamp16(d),
            at_us,
        },
        ffi::INPUT_WHEEL => MacInput::Wheel {
            dx: a.clamp(i16::MIN.into(), i16::MAX.into()) as i16,
            dy: b.clamp(i16::MIN.into(), i16::MAX.into()) as i16,
            at_us,
        },
        ffi::INPUT_KEY => MacInput::Key {
            code: a.clamp(0, u16::MAX.into()) as u16,
            down: b != 0,
            at_us,
        },
        ffi::INPUT_FOCUS_LOST => MacInput::FocusLost { at_us },
        _ => return,
    };
    // SAFETY: the context is the handler installed by `set_input`, which
    // stays alive until Swift confirms it no longer calls it.
    let handler = unsafe { &mut *context.cast::<InputHandler>() };
    handler(event);
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
        Ok(Self {
            handle,
            refresh_hz,
            input: None,
        })
    }
}

impl MacPresenter {
    /// Sends the window's mouse, wheel, and key events to `handler` (on the
    /// main thread). Replaces any earlier handler.
    pub fn set_input(&mut self, handler: impl FnMut(MacInput) + Send + 'static) {
        self.clear_input();
        let boxed: *mut InputHandler = Box::into_raw(Box::new(Box::new(handler)));
        // SAFETY: live presenter; `boxed` lives until `clear_input`.
        unsafe { ffi::frames_mac_presenter_set_input(self.handle, boxed.cast(), Some(on_input)) };
        self.input = Some(boxed);
    }

    fn clear_input(&mut self) {
        if let Some(boxed) = self.input.take() {
            // SAFETY: removing waits for a call in progress; afterwards Swift
            // never uses the context again, so it can be freed.
            unsafe {
                ffi::frames_mac_presenter_set_input(self.handle, std::ptr::null_mut(), None);
                drop(Box::from_raw(boxed));
            }
        }
    }

    /// Shows the host's cursor over the window: premultiplied BGRA, top row
    /// first (as the host sends it).
    pub fn set_cursor(&self, width: u32, height: u32, hotspot: (u32, u32), premultiplied: &[u8]) {
        if premultiplied.len() != (width * height * 4) as usize {
            return;
        }
        let straight: Vec<u8> = premultiplied
            .chunks_exact(4)
            .flat_map(|p| {
                let alpha = u32::from(p[3]);
                let channel = |c: u8| {
                    if alpha == 0 {
                        0
                    } else {
                        (u32::from(c) * 255 / alpha).min(255) as u8
                    }
                };
                [channel(p[0]), channel(p[1]), channel(p[2]), p[3]]
            })
            .collect();
        // SAFETY: live presenter; the pixels outlive the call (copied there).
        unsafe {
            ffi::frames_mac_presenter_set_cursor(
                self.handle,
                straight.as_ptr(),
                width,
                height,
                hotspot.0,
                hotspot.1,
            )
        };
    }

    pub fn set_cursor_visible(&self, visible: bool) {
        // SAFETY: live presenter.
        unsafe { ffi::frames_mac_presenter_set_cursor_visible(self.handle, visible) };
    }

    /// Feeds one step of generated input through the window's real event
    /// handlers (unattended tests).
    pub fn post_synthetic_input(&self, step: u64) {
        // SAFETY: live presenter.
        unsafe { ffi::frames_mac_presenter_post_synthetic_input(self.handle, step) };
    }

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
        self.clear_input();
        // SAFETY: closes the window and frees the presenter exactly once.
        unsafe { ffi::frames_mac_presenter_free(self.handle) };
    }
}
