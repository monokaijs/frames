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
