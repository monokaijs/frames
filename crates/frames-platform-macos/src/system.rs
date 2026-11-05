//! Process-level macOS services: the AppKit run loop, permissions, real-time
//! threads, and Wi-Fi traffic class marking.

use crate::ffi;
use std::ffi::c_int;
use std::time::Duration;

/// The FramesMac ABI version linked into this binary.
pub fn abi_version() -> u32 {
    // SAFETY: a constant query.
    unsafe { ffi::frames_mac_abi_version() }
}

/// Whether the linked Swift library implements the ABI these bindings
/// mirror. Check once at startup; a mismatch is a build error.
pub fn abi_compatible() -> bool {
    abi_version() == ffi::ABI_VERSION
}

/// Whether this process may capture the screen without a prompt. A binary
/// started from a terminal inherits the terminal's grant, not the app's.
pub fn screen_capture_permitted() -> bool {
    // SAFETY: a read-only TCC query.
    unsafe { ffi::frames_mac_screen_capture_preflight() }
}

/// Headless host mode: runs NSApplication on the calling thread, which must
/// be the process main thread, for `seconds` (zero = until [`stop_app`]).
/// Never call while another toolkit (Slint/winit) owns NSApplication.
pub fn run_app(seconds: f64) {
    // SAFETY: the Swift side asserts it runs on the main thread.
    unsafe { ffi::frames_mac_run_app(seconds) }
}

/// A regular app with a Dock icon and the viewer's Frames menu, whose
/// commands arrive as [`crate::presenter::MacInput::Command`].
pub fn viewer_app() {
    // SAFETY: posts to the main queue.
    unsafe { ffi::frames_mac_viewer_app() }
}

pub fn stop_app() {
    // SAFETY: posts to the main queue.
    unsafe { ffi::frames_mac_stop_app() }
}

#[repr(C)]
struct TimebaseInfo {
    numer: u32,
    denom: u32,
}

#[repr(C)]
struct TimeConstraintPolicy {
    period: u32,
    computation: u32,
    constraint: u32,
    preemptible: c_int,
}

const THREAD_TIME_CONSTRAINT_POLICY: c_int = 2;
const THREAD_TIME_CONSTRAINT_POLICY_COUNT: u32 = 4;

unsafe extern "C" {
    fn mach_timebase_info(info: *mut TimebaseInfo) -> c_int;
    fn pthread_mach_thread_np(thread: usize) -> u32;
    fn pthread_self() -> usize;
    fn thread_policy_set(thread: u32, flavor: c_int, policy: *mut c_int, count: u32) -> c_int;
    fn setsockopt(
        socket: c_int,
        level: c_int,
        name: c_int,
        value: *const c_int,
        length: u32,
    ) -> c_int;
}

/// Makes the calling thread a real-time thread for a periodic workload:
/// every `period` it needs `computation` of CPU, finished within
/// `constraint` (THREAD_TIME_CONSTRAINT_POLICY). Use it for the capture,
/// encode, receive, decode, and present threads of an active stream.
pub fn set_realtime_thread(
    period: Duration,
    computation: Duration,
    constraint: Duration,
) -> Result<(), i32> {
    let mut timebase = TimebaseInfo { numer: 0, denom: 0 };
    // SAFETY: fills the struct.
    unsafe { mach_timebase_info(&mut timebase) };
    if timebase.numer == 0 {
        return Err(-1);
    }
    let ticks = |d: Duration| {
        (d.as_nanos() * u128::from(timebase.denom) / u128::from(timebase.numer))
            .min(u128::from(u32::MAX)) as u32
    };
    let mut policy = TimeConstraintPolicy {
        period: ticks(period),
        computation: ticks(computation),
        constraint: ticks(constraint),
        preemptible: 1,
    };
    // SAFETY: applies a policy to the calling thread's own Mach port.
    let result = unsafe {
        thread_policy_set(
            pthread_mach_thread_np(pthread_self()),
            THREAD_TIME_CONSTRAINT_POLICY,
            &mut policy as *mut TimeConstraintPolicy as *mut c_int,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT,
        )
    };
    if result == 0 { Ok(()) } else { Err(result) }
}

const SOL_SOCKET: c_int = 0xFFFF;
const SO_NET_SERVICE_TYPE: c_int = 0x1116;
/// "Interactive video": mapped to the Wi-Fi video access category (WMM AC_VI).
const NET_SERVICE_TYPE_VI: c_int = 3;

/// Marks a UDP socket's traffic as interactive video, so macOS uses the Wi-Fi
/// video queue and sets the matching DSCP. The transport calls this on its
/// media socket.
pub fn set_video_service_type(socket: c_int) -> Result<(), i32> {
    let value = NET_SERVICE_TYPE_VI;
    // SAFETY: a socket option on a caller-owned descriptor.
    let result = unsafe {
        setsockopt(
            socket,
            SOL_SOCKET,
            SO_NET_SERVICE_TYPE,
            &value,
            std::mem::size_of::<c_int>() as u32,
        )
    };
    if result == 0 { Ok(()) } else { Err(result) }
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        kind: *const std::ffi::c_void,
        level: u32,
        name: *const std::ffi::c_void,
        id: *mut u32,
    ) -> i32;
    fn IOPMAssertionRelease(id: u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const std::ffi::c_void,
        text: *const std::ffi::c_char,
        encoding: u32,
    ) -> *const std::ffi::c_void;
    fn CFRelease(object: *const std::ffi::c_void);
}

/// Keeps the display awake while the guard lives: a viewer does not count
/// as local activity, and ScreenCaptureKit has nothing to capture from a
/// sleeping display.
pub struct KeepAwake(u32);

impl KeepAwake {
    pub fn display() -> Option<Self> {
        const UTF8: u32 = 0x0800_0100;
        const ON: u32 = 255;
        // SAFETY: NUL-terminated literals; both strings are released after
        // the call, which copies what it keeps.
        unsafe {
            let kind = CFStringCreateWithCString(
                std::ptr::null(),
                c"PreventUserIdleDisplaySleep".as_ptr(),
                UTF8,
            );
            let name = CFStringCreateWithCString(
                std::ptr::null(),
                c"Frames is sharing this screen".as_ptr(),
                UTF8,
            );
            let mut id = 0;
            let result = IOPMAssertionCreateWithName(kind, ON, name, &mut id);
            CFRelease(kind);
            CFRelease(name);
            (result == 0).then_some(Self(id))
        }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        // SAFETY: an assertion this guard created.
        unsafe { IOPMAssertionRelease(self.0) };
    }
}
