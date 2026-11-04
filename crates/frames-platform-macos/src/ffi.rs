//! Mirror of native/macos/FramesMac/Sources/CFramesMac/include/frames_mac.h.
//! Keep both in sync; `abi_version_matches` checks the version at run time.

use std::ffi::c_void;

pub const ABI_VERSION: u32 = 3;

pub const CODEC_H264: u32 = 0;
pub const CODEC_HEVC: u32 = 1;
pub const ENCODER_LTR: u32 = 1 << 0;
pub const ENCODE_FORCE_KEYFRAME: u32 = 1 << 0;
pub const ENCODE_FORCE_LTR_REFRESH: u32 = 1 << 1;
pub const ENCODED_KEYFRAME: u32 = 1 << 0;

pub const DECODE_OK: i32 = 0;
pub const DECODE_NEED_KEYFRAME: i32 = 1;
pub const DECODE_TIMEOUT: i32 = -1;

pub const EVENT_LOG: u32 = 1;
pub const EVENT_CAPTURE_FAILED: u32 = 2;
pub const EVENT_CAPTURE_STARTED: u32 = 3;

pub type OnFrame = unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32, u64, u64);
pub type OnEncoded = unsafe extern "C" fn(*mut c_void, *const u8, usize, u32, i64, u64, u64, i32);
pub type OnCursor = unsafe extern "C" fn(*mut c_void, f64, f64, bool);
pub type OnEvent = unsafe extern "C" fn(*mut c_void, u32, *const u8, usize);
pub type OnInput = unsafe extern "C" fn(*mut c_void, u32, i32, i32, i32, i32, u64);

pub const INPUT_MOVE: u32 = 1;
pub const INPUT_BUTTON: u32 = 2;
pub const INPUT_WHEEL: u32 = 3;
pub const INPUT_KEY: u32 = 4;
pub const INPUT_FOCUS_LOST: u32 = 5;
pub const INPUT_MOTION: u32 = 6;
pub const INPUT_MODE: u32 = 7;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Callbacks {
    pub context: *mut c_void,
    pub on_frame: Option<OnFrame>,
    pub on_encoded: Option<OnEncoded>,
    pub on_cursor: Option<OnCursor>,
    pub on_event: Option<OnEvent>,
}

impl Callbacks {
    pub fn empty(context: *mut c_void) -> Self {
        Self {
            context,
            on_frame: None,
            on_encoded: None,
            on_cursor: None,
            on_event: None,
        }
    }
}

unsafe extern "C" {
    pub fn frames_mac_abi_version() -> u32;
    pub fn frames_mac_screen_capture_preflight() -> bool;
    pub fn frames_mac_capture_start(fps: u32, synthetic: bool, callbacks: Callbacks)
    -> *mut c_void;
    pub fn frames_mac_capture_stop(capture: *mut c_void);
    pub fn frames_mac_synthetic_frame(width: u32, height: u32, index: u64) -> *mut c_void;
    pub fn frames_mac_pixel_buffer_difference(a: *mut c_void, b: *mut c_void) -> f64;
    pub fn frames_mac_encoder_new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        codec: u32,
        flags: u32,
        callbacks: Callbacks,
    ) -> *mut c_void;
    pub fn frames_mac_encoder_encode(
        encoder: *mut c_void,
        pixel_buffer: *mut c_void,
        pts_ns: u64,
        options: u32,
        acknowledged: *const i64,
        acknowledged_count: usize,
    ) -> i32;
    pub fn frames_mac_encoder_set_bitrate(encoder: *mut c_void, bitrate: u32, fps: u32) -> i32;
    pub fn frames_mac_encoder_free(encoder: *mut c_void);
    pub fn frames_mac_decoder_new(codec: u32) -> *mut c_void;
    pub fn frames_mac_decoder_decode(
        decoder: *mut c_void,
        data: *const u8,
        length: usize,
        keyframe: bool,
        output: *mut *mut c_void,
    ) -> i32;
    pub fn frames_mac_decoder_free(decoder: *mut c_void);
    pub fn frames_mac_presenter_new(width: u32, height: u32, window: bool) -> *mut c_void;
    pub fn frames_mac_presenter_present(
        presenter: *mut c_void,
        pixel_buffer: *mut c_void,
        vsync: bool,
        presented_ns: *mut u64,
    ) -> i32;
    pub fn frames_mac_presenter_last_on_glass_ns(presenter: *mut c_void) -> u64;
    pub fn frames_mac_presenter_refresh_hz(presenter: *mut c_void) -> f64;
    pub fn frames_mac_presenter_free(presenter: *mut c_void);
    pub fn frames_mac_presenter_set_input(
        presenter: *mut c_void,
        context: *mut c_void,
        callback: Option<OnInput>,
    );
    pub fn frames_mac_presenter_post_synthetic_input(presenter: *mut c_void, step: u64);
    pub fn frames_mac_presenter_set_relative(
        presenter: *mut c_void,
        relative: bool,
        lock_pointer: bool,
    );
    pub fn frames_mac_presenter_set_cursor(
        presenter: *mut c_void,
        bgra: *const u8,
        width: u32,
        height: u32,
        hotspot_x: u32,
        hotspot_y: u32,
    );
    pub fn frames_mac_presenter_set_cursor_visible(presenter: *mut c_void, visible: bool);
    pub fn frames_mac_presenter_set_title(presenter: *mut c_void, utf8: *const u8, length: usize);
    pub fn frames_mac_clipboard_change_count() -> u64;
    pub fn frames_mac_clipboard_text(out: *mut *mut u8) -> i64;
    pub fn frames_mac_clipboard_png(out: *mut *mut u8) -> i64;
    pub fn frames_mac_clipboard_set_text(utf8: *const u8, length: usize) -> i32;
    pub fn frames_mac_clipboard_set_png(png: *const u8, length: usize) -> i32;
    pub fn frames_mac_free(pointer: *mut c_void);
    pub fn frames_mac_run_app(seconds: f64);
    pub fn frames_mac_stop_app();
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRetain(object: *const c_void) -> *const c_void;
    pub fn CFRelease(object: *const c_void);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    pub fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    pub fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
}

unsafe extern "C" {
    pub fn clock_gettime_nsec_np(clock: u32) -> u64;
}
pub const CLOCK_UPTIME_RAW: u32 = 8;

/// The clock the Swift side stamps everything with.
pub fn uptime_ns() -> u64 {
    // SAFETY: a plain libc clock read.
    unsafe { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }
}
