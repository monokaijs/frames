//! Phase 0 spike 0-B: Rust owns `main`, Swift platform services run in-process
//! as a static library behind a C ABI. Pipeline proven here:
//! ScreenCaptureKit (or synthetic IOSurface) → +1 CVPixelBuffer to Rust (zero copy)
//! → Rust encode thread → Swift VideoToolbox encoder → encoded bytes to Rust.
//!
//! Usage: swift-static-spike [--seconds 12] [--fps 60] [--synthetic] [--codec h264|hevc] [--ltr] [--slices]

use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::{
        Arc, Mutex,
        atomic::{AtomicPtr, AtomicU64, Ordering},
        mpsc::{SyncSender, TrySendError, sync_channel},
    },
    thread,
};

// ---- C ABI (mirrors FramesMac/Sources/CFramesMac/include/frames_mac.h) ----

#[repr(C)]
#[derive(Clone, Copy)]
struct Callbacks {
    context: *mut c_void,
    on_frame: Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u32, u32, u64, u64)>,
    on_encoded: Option<unsafe extern "C" fn(*mut c_void, *const u8, usize, bool, i64, u64, u64)>,
    on_event: Option<unsafe extern "C" fn(*mut c_void, u32, *const u8, usize)>,
}
// The context points at `State`, which is Sync and outlives every callback.
unsafe impl Send for Callbacks {}
unsafe impl Sync for Callbacks {}

const CODEC_H264: u32 = 0;
const CODEC_HEVC: u32 = 1;
const ENCODER_LTR: u32 = 1 << 0;
const ENCODER_SLICES: u32 = 1 << 1;
const ENCODE_FORCE_KEYFRAME: u32 = 1 << 0;
const ENCODE_FORCE_LTR_REFRESH: u32 = 1 << 1;

unsafe extern "C" {
    fn frames_mac_abi_version() -> u32;
    fn frames_mac_screen_capture_preflight() -> bool;
    fn frames_mac_probe(codec: u32, output: *mut u8, capacity: usize) -> usize;
    fn frames_mac_capture_start(fps: u32, synthetic: bool, callbacks: Callbacks) -> *mut c_void;
    fn frames_mac_capture_stop(capture: *mut c_void);
    fn frames_mac_encoder_new(width: u32, height: u32, fps: u32, bitrate: u32, codec: u32, flags: u32, callbacks: Callbacks) -> *mut c_void;
    fn frames_mac_encoder_encode(encoder: *mut c_void, buffer: *mut c_void, pts_ns: u64, options: u32) -> i32;
    fn frames_mac_encoder_acknowledge_ltr(encoder: *mut c_void, token: i64);
    fn frames_mac_encoder_flush(encoder: *mut c_void);
    fn frames_mac_encoder_free(encoder: *mut c_void);
    fn frames_mac_run_app(seconds: f64);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}
#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferGetIOSurface(buffer: *mut c_void) -> *mut c_void;
}
#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetID(surface: *mut c_void) -> u32;
    fn IOSurfaceGetAllocSize(surface: *mut c_void) -> usize;
}
unsafe extern "C" {
    fn clock_gettime_nsec_np(clock: u32) -> u64;
    fn pthread_main_np() -> i32;
}
const CLOCK_UPTIME_RAW: u32 = 8;

fn now_ns() -> u64 {
    unsafe { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }
}

/// Owned +1 CVPixelBufferRef handed over by Swift; released exactly once.
struct PixelBuffer(*mut c_void);
unsafe impl Send for PixelBuffer {}
impl Drop for PixelBuffer {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

// ---- Shared state behind the callback context ----

#[derive(Default)]
struct Stats {
    handoff_us: Vec<u64>,
    encode_us: Vec<u64>,
    sizes: Vec<usize>,
    keyframes: Vec<usize>,
    dropped_encodes: usize,
    ltr_tokens: usize,
    // (encoded index, options) for frames submitted with special options.
    marks: Vec<(usize, u32, usize, bool)>,
    surfaces: std::collections::BTreeSet<u32>,
    surface_bytes: usize,
    width: u32,
    height: u32,
    events: Vec<String>,
}

struct State {
    frames: Mutex<Option<SyncSender<(PixelBuffer, u32, u32, u64)>>>,
    stats: Mutex<Stats>,
    pending_options: Mutex<VecDeque<u32>>,
    encoder: AtomicPtr<c_void>,
    frames_received: AtomicU64,
    channel_drops: AtomicU64,
    callbacks_on_main: AtomicU64,
    encoded_index: AtomicU64,
}

unsafe extern "C" fn on_frame(context: *mut c_void, buffer: *mut c_void, width: u32, height: u32, captured: u64, _delivered: u64) {
    let state = unsafe { &*(context as *const State) };
    let buffer = PixelBuffer(buffer);
    let arrived = now_ns();
    if unsafe { pthread_main_np() } == 1 {
        state.callbacks_on_main.fetch_add(1, Ordering::Relaxed);
    }
    state.frames_received.fetch_add(1, Ordering::Relaxed);
    {
        let mut stats = state.stats.lock().unwrap();
        stats.handoff_us.push(arrived.saturating_sub(captured) / 1000);
        let surface = unsafe { CVPixelBufferGetIOSurface(buffer.0) };
        if !surface.is_null() {
            stats.surfaces.insert(unsafe { IOSurfaceGetID(surface) });
            stats.surface_bytes = unsafe { IOSurfaceGetAllocSize(surface) };
        }
        stats.width = width;
        stats.height = height;
    }
    // Never block the capture queue: newest-wins is the engine's job; here we drop.
    let sender = state.frames.lock().unwrap().clone();
    match sender.map(|sender| sender.try_send((buffer, width, height, captured))) {
        Some(Ok(())) => {}
        Some(Err(TrySendError::Full(_) | TrySendError::Disconnected(_))) | None => {
            state.channel_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe extern "C" fn on_encoded(context: *mut c_void, data: *const u8, length: usize, keyframe: bool, token: i64, submitted: u64, completed: u64) {
    let state = unsafe { &*(context as *const State) };
    if unsafe { pthread_main_np() } == 1 {
        state.callbacks_on_main.fetch_add(1, Ordering::Relaxed);
    }
    let index = state.encoded_index.fetch_add(1, Ordering::Relaxed) as usize;
    let options = state.pending_options.lock().unwrap().pop_front().unwrap_or(0);
    let mut stats = state.stats.lock().unwrap();
    if data.is_null() || length == 0 {
        stats.dropped_encodes += 1;
        return;
    }
    stats.encode_us.push(completed.saturating_sub(submitted) / 1000);
    stats.sizes.push(length);
    if keyframe {
        stats.keyframes.push(index);
    }
    if options != 0 {
        stats.marks.push((index, options, length, keyframe));
    }
    if token >= 0 {
        stats.ltr_tokens += 1;
        // Simulates the client acknowledging receipt of this LTR frame.
        let encoder = state.encoder.load(Ordering::Acquire);
        if !encoder.is_null() {
            unsafe { frames_mac_encoder_acknowledge_ltr(encoder, token) };
        }
    }
}

unsafe extern "C" fn on_event(context: *mut c_void, kind: u32, data: *const u8, length: usize) {
    let state = unsafe { &*(context as *const State) };
    let text = if data.is_null() { String::new() } else { String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(data, length) }).into_owned() };
    println!("event {kind}: {text}");
    state.stats.lock().unwrap().events.push(format!("{kind}: {text}"));
}

fn percentile(values: &mut [u64], p: usize) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_unstable();
    values[(values.len() - 1) * p / 100] as f64 / 1000.0
}

fn probe(codec: u32) -> String {
    let needed = unsafe { frames_mac_probe(codec, std::ptr::null_mut(), 0) };
    let mut buffer = vec![0u8; needed];
    unsafe { frames_mac_probe(codec, buffer.as_mut_ptr(), buffer.len()) };
    String::from_utf8_lossy(&buffer[..needed.saturating_sub(1)]).into_owned()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let value = |name: &str, default: &str| -> String {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_owned())
    };
    let seconds: f64 = value("--seconds", "12").parse().unwrap();
    let fps: u32 = value("--fps", "60").parse().unwrap();
    let synthetic = args.iter().any(|a| a == "--synthetic");
    let codec = if value("--codec", "hevc") == "h264" { CODEC_H264 } else { CODEC_HEVC };
    let mut flags = 0;
    if args.iter().any(|a| a == "--ltr") {
        flags |= ENCODER_LTR;
    }
    if args.iter().any(|a| a == "--slices") {
        flags |= ENCODER_SLICES;
    }

    assert_eq!(unsafe { pthread_main_np() }, 1, "Rust main must be the process main thread");
    println!("abi version: {}", unsafe { frames_mac_abi_version() });
    let preflight = unsafe { frames_mac_screen_capture_preflight() };
    println!("screen capture preflight: {preflight}");
    for codec in [CODEC_H264, CODEC_HEVC] {
        let json = probe(codec);
        let short = json.split("\"supported\"").next().unwrap_or(&json);
        println!("probe: {short}...");
    }

    let (sender, receiver) = sync_channel(4);
    let state = Arc::new(State {
        frames: Mutex::new(Some(sender)),
        stats: Mutex::default(),
        pending_options: Mutex::default(),
        encoder: AtomicPtr::new(std::ptr::null_mut()),
        frames_received: AtomicU64::new(0),
        channel_drops: AtomicU64::new(0),
        callbacks_on_main: AtomicU64::new(0),
        encoded_index: AtomicU64::new(0),
    });
    let callbacks = Callbacks {
        context: Arc::as_ptr(&state) as *mut c_void,
        on_frame: Some(on_frame),
        on_encoded: Some(on_encoded),
        on_event: Some(on_event),
    };

    // Rust-owned encode thread: the engine's "encode stage" in Phase 2.
    let encode_state = state.clone();
    let encode_callbacks = callbacks;
    let encode_thread = thread::Builder::new()
        .name("frames-encode".into())
        .spawn(move || {
            let callbacks = encode_callbacks;
            let mut encoder: *mut c_void = std::ptr::null_mut();
            let mut submitted = 0usize;
            while let Ok((buffer, width, height, captured)) = receiver.recv() {
                if encoder.is_null() {
                    encoder = unsafe { frames_mac_encoder_new(width, height, fps, 20_000_000, codec, flags, callbacks) };
                    assert!(!encoder.is_null(), "encoder creation failed");
                    encode_state.encoder.store(encoder, Ordering::Release);
                }
                // Recovery drills: LTR refresh at 300 (simulated loss), IDR at 450.
                let options = match submitted {
                    300 if flags & ENCODER_LTR != 0 => ENCODE_FORCE_LTR_REFRESH,
                    450 => ENCODE_FORCE_KEYFRAME,
                    _ => 0,
                };
                encode_state.pending_options.lock().unwrap().push_back(options);
                let status = unsafe { frames_mac_encoder_encode(encoder, buffer.0, captured, options) };
                if status != 0 {
                    println!("encode status {status}");
                }
                submitted += 1;
                drop(buffer); // VideoToolbox holds its own reference.
            }
            if !encoder.is_null() {
                unsafe { frames_mac_encoder_flush(encoder) };
                encode_state.encoder.store(std::ptr::null_mut(), Ordering::Release);
                unsafe { frames_mac_encoder_free(encoder) };
            }
            submitted
        })
        .unwrap();

    let capture = unsafe { frames_mac_capture_start(fps, synthetic || !preflight, callbacks) };
    // Headless mode: the main thread runs NSApplication until the timer fires.
    let started = now_ns();
    unsafe { frames_mac_run_app(seconds) };
    let ran = (now_ns() - started) as f64 / 1e9;
    unsafe { frames_mac_capture_stop(capture) };
    // Capture is stopped; closing the channel lets the encode thread flush and exit.
    state.frames.lock().unwrap().take();
    let submitted = encode_thread.join().unwrap();
    report(ran, submitted, &state.stats, &state.frames_received, &state.channel_drops, &state.callbacks_on_main, codec, flags);
}

#[allow(clippy::too_many_arguments)]
fn report(ran: f64, submitted: usize, stats: &Mutex<Stats>, received: &AtomicU64, drops: &AtomicU64, on_main: &AtomicU64, codec: u32, flags: u32) {
    let mut stats = stats.lock().unwrap();
    let frames = received.load(Ordering::Relaxed);
    println!("---- results ({:.1} s app run on main thread) ----", ran);
    println!("codec {} flags ltr={} slices={}", if codec == CODEC_HEVC { "hevc" } else { "h264" }, flags & ENCODER_LTR != 0, flags & ENCODER_SLICES != 0);
    println!("frames delivered to Rust: {frames} ({:.1} fps) {}x{}, channel drops {}", frames as f64 / ran, stats.width, stats.height, drops.load(Ordering::Relaxed));
    println!("distinct IOSurfaces seen: {} (pool recycling, zero copy), alloc size {} B", stats.surfaces.len(), stats.surface_bytes);
    println!("callbacks on main thread: {}", on_main.load(Ordering::Relaxed));
    let mut handoff = std::mem::take(&mut stats.handoff_us);
    println!("capture→Rust handoff: p50 {:.2} ms p95 {:.2} ms p99 {:.2} ms", percentile(&mut handoff, 50), percentile(&mut handoff, 95), percentile(&mut handoff, 99));
    let mut encode = std::mem::take(&mut stats.encode_us);
    println!("encode submit→output: p50 {:.2} ms p95 {:.2} ms p99 {:.2} ms (n={})", percentile(&mut encode, 50), percentile(&mut encode, 95), percentile(&mut encode, 99), encode.len());
    let sizes = &stats.sizes;
    let average = if sizes.is_empty() { 0 } else { sizes.iter().sum::<usize>() / sizes.len() };
    println!("encoded {} / submitted {submitted}, dropped by encoder {}, avg size {average} B", sizes.len(), stats.dropped_encodes);
    println!("keyframes at encoded indices: {:?}", stats.keyframes);
    println!("LTR acknowledgement tokens received: {}", stats.ltr_tokens);
    for (index, options, size, keyframe) in &stats.marks {
        let kind = if options & ENCODE_FORCE_LTR_REFRESH != 0 { "LTR refresh" } else { "forced IDR" };
        println!("{kind} at {index}: {size} B keyframe={keyframe}");
    }
}
