//! ScreenCaptureKit capture as a [`CaptureSource`].

use crate::ffi;
use crate::pixel::{PixelBuffer, uptime_to_clock_us};
use frames_media::api::{
    CaptureCaps, CaptureConfig, CaptureEvent, CaptureSource, CursorUpdate, MediaClock, MediaError,
    VideoFrame,
};
use frames_media::clock::SystemClock;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    /// Newest frame only: an older one waiting here is pure latency.
    frame: Option<(PixelBuffer, u64)>,
    cursor: Option<CursorUpdate>,
    events: VecDeque<String>,
    synthetic: Option<bool>,
}

unsafe extern "C" fn on_frame(
    context: *mut c_void,
    buffer: *mut c_void,
    _: u32,
    _: u32,
    captured_ns: u64,
    _: u64,
) {
    // SAFETY: `context` is the `Shared` owned by the live `MacCapture`, and
    // Swift hands over a +1 buffer.
    let shared = unsafe { &*(context as *const Shared) };
    let Some(buffer) = (unsafe { PixelBuffer::from_retained(buffer) }) else {
        return;
    };
    let captured_us = uptime_to_clock_us(captured_ns);
    shared.state.lock().unwrap().frame = Some((buffer, captured_us));
    shared.ready.notify_one();
}

unsafe extern "C" fn on_cursor(context: *mut c_void, x: f64, y: f64, visible: bool) {
    // SAFETY: as in `on_frame`.
    let shared = unsafe { &*(context as *const Shared) };
    shared.state.lock().unwrap().cursor = Some(CursorUpdate {
        visible,
        x: x.round() as i32,
        y: y.round() as i32,
        shape: None,
    });
}

unsafe extern "C" fn on_event(context: *mut c_void, kind: u32, data: *const u8, length: usize) {
    // SAFETY: as in `on_frame`; `data` is UTF-8 of `length` bytes.
    let shared = unsafe { &*(context as *const Shared) };
    let text = if data.is_null() {
        String::new()
    } else {
        String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(data, length) }).into_owned()
    };
    let mut state = shared.state.lock().unwrap();
    let entry = match kind {
        ffi::EVENT_CAPTURE_STARTED => {
            state.synthetic = Some(text.starts_with("synthetic"));
            format!("started: {text}")
        }
        ffi::EVENT_CAPTURE_FAILED => format!("failed: {text}"),
        ffi::EVENT_LOG => text,
        other => format!("event {other}: {text}"),
    };
    state.events.push_back(entry);
    while state.events.len() > 32 {
        state.events.pop_front();
    }
}

/// Main-display capture, cursor excluded from the pixels. Without Screen
/// Recording permission (or when asked), a generated 1920x1080 source.
pub struct MacCapture {
    handle: *mut c_void,
    shared: Box<Shared>,
    fps: u32,
    next_id: u64,
}

// SAFETY: the Swift capture object is internally synchronized; the handle is
// only used from the owning thread and freed once.
unsafe impl Send for MacCapture {}

/// Asks another application's window to close, as its close button would
/// (needs Accessibility). False when it could not.
pub fn close_window(window_id: u32) -> bool {
    // SAFETY: plain call.
    unsafe { ffi::frames_mac_close_window(window_id) }
}

/// The menus of the application owning `window_id`, as JSON (needs
/// Accessibility); `None` when it has none or cannot be read.
pub fn app_menus_json(window_id: u32) -> Option<String> {
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        // SAFETY: Swift writes at most `capacity` bytes.
        let length =
            unsafe { ffi::frames_mac_app_menus(window_id, buffer.as_mut_ptr(), buffer.len()) };
        if length == 0 {
            return None;
        }
        if length <= buffer.len() {
            buffer.truncate(length);
            return Some(String::from_utf8_lossy(&buffer).into_owned());
        }
        buffer.resize(length, 0);
    }
}

/// Presses an item of [`app_menus_json`]'s tree by its id.
pub fn press_menu(window_id: u32, id: u32) -> bool {
    // SAFETY: plain call.
    unsafe { ffi::frames_mac_press_menu(window_id, id) }
}

/// Other processes' on-screen application windows as JSON:
/// `[{"id": CGWindowID, "title": "..."}]`, front to back.
pub fn windows_json() -> String {
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: Swift writes at most `capacity` bytes.
        let length = unsafe { ffi::frames_mac_windows(buffer.as_mut_ptr(), buffer.len()) };
        if length <= buffer.len() {
            buffer.truncate(length);
            return String::from_utf8_lossy(&buffer).into_owned();
        }
        buffer.resize(length, 0);
    }
}

impl MacCapture {
    pub fn start(fps: u32, synthetic: bool) -> Result<Self, MediaError> {
        Self::open(fps, |callbacks| unsafe {
            // SAFETY: the callbacks' context outlives the capture.
            ffi::frames_mac_capture_start(fps, synthetic, callbacks)
        })
    }

    /// One window by CGWindowID. It reports a failure (see [`Self::events`])
    /// and delivers nothing when that window cannot be captured.
    pub fn window(window_id: u32, fps: u32) -> Result<Self, MediaError> {
        Self::open(fps, |callbacks| unsafe {
            // SAFETY: as in `start`.
            ffi::frames_mac_capture_start_window(window_id, fps, callbacks)
        })
    }

    fn open(
        fps: u32,
        start: impl FnOnce(ffi::Callbacks) -> *mut c_void,
    ) -> Result<Self, MediaError> {
        let shared = Box::new(Shared::default());
        let callbacks = ffi::Callbacks {
            on_frame: Some(on_frame),
            on_cursor: Some(on_cursor),
            on_event: Some(on_event),
            ..ffi::Callbacks::empty(&*shared as *const Shared as *mut c_void)
        };
        // `shared` outlives the capture (stopped in Drop first).
        let handle = start(callbacks);
        if handle.is_null() {
            return Err(MediaError::Backend("capture could not start".into()));
        }
        Ok(Self {
            handle,
            shared,
            fps,
            next_id: 0,
        })
    }

    /// Whether frames come from the generated source, once known.
    pub fn synthetic(&self) -> Option<bool> {
        self.shared.state.lock().unwrap().synthetic
    }

    pub fn events(&self) -> Vec<String> {
        self.shared
            .state
            .lock()
            .unwrap()
            .events
            .iter()
            .cloned()
            .collect()
    }
}

impl CaptureSource for MacCapture {
    fn caps(&self) -> CaptureCaps {
        CaptureCaps {
            max_fps: 240,
            separate_cursor: true,
            dirty_rects: false,
        }
    }

    fn configure(&mut self, config: CaptureConfig) -> Result<(), MediaError> {
        // ScreenCaptureKit's frame interval is fixed per stream; a different
        // rate needs a new capture.
        if config.fps != self.fps {
            return Err(MediaError::Unsupported(
                "changing the rate of a running capture",
            ));
        }
        Ok(())
    }

    fn next(&mut self, deadline_us: u64) -> Result<CaptureEvent, MediaError> {
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if let Some((buffer, captured_us)) = state.frame.take() {
                let id = self.next_id;
                self.next_id += 1;
                return Ok(CaptureEvent::Frame(VideoFrame {
                    id,
                    captured_us,
                    surface: buffer.into_surface(),
                    dirty: None,
                }));
            }
            let now = SystemClock::new().now_us();
            if now >= deadline_us {
                return Ok(CaptureEvent::Timeout);
            }
            state = self
                .shared
                .ready
                .wait_timeout(state, Duration::from_micros(deadline_us - now))
                .unwrap()
                .0;
        }
    }

    fn cursor(&mut self) -> Option<CursorUpdate> {
        self.shared.state.lock().unwrap().cursor.take()
    }
}

impl Drop for MacCapture {
    fn drop(&mut self) {
        // SAFETY: stops callbacks and releases the Swift object exactly once.
        unsafe { ffi::frames_mac_capture_stop(self.handle) };
    }
}
