//! Scheduling and network helpers for a streaming session.
//!
//! Every entry point is loaded at run time from its system DLL, needs no
//! administrator rights, and degrades to a logged no-op when unavailable.
//! Guards undo the change when dropped, so settings last exactly as long as
//! the session holds them.

use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;
use windows::Win32::Foundation::{HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::{GUID, Interface, PCSTR, PCWSTR, w};

fn function(library: PCWSTR, name: &[u8]) -> Option<unsafe extern "system" fn() -> isize> {
    debug_assert_eq!(name.last(), Some(&0));
    unsafe {
        let module: HMODULE = LoadLibraryW(library).ok()?;
        GetProcAddress(module, PCSTR(name.as_ptr()))
    }
}

/// Registers the calling thread with MMCSS ("Games" task), raising its
/// scheduling priority for media work. Reverted on drop.
pub struct Mmcss(HANDLE);

impl Mmcss {
    pub fn games() -> Option<Self> {
        type Set = unsafe extern "system" fn(PCWSTR, *mut u32) -> HANDLE;
        let set: Set = unsafe {
            std::mem::transmute(function(
                w!("avrt.dll"),
                b"AvSetMmThreadCharacteristicsW\0",
            )?)
        };
        let mut index = 0;
        let handle = unsafe { set(w!("Games"), &mut index) };
        if handle.is_invalid() || handle.0.is_null() {
            tracing::info!("MMCSS registration unavailable");
            return None;
        }
        Some(Self(handle))
    }
}

impl Drop for Mmcss {
    fn drop(&mut self) {
        type Revert = unsafe extern "system" fn(HANDLE) -> i32;
        if let Some(revert) = function(w!("avrt.dll"), b"AvRevertMmThreadCharacteristics\0") {
            let revert: Revert = unsafe { std::mem::transmute(revert) };
            unsafe { revert(self.0) };
        }
    }
}

/// Requests 1 ms system timer resolution while the guard lives, so short
/// waits in media threads are not rounded up to the default 15.6 ms tick.
pub struct TimerResolution;

impl TimerResolution {
    pub fn one_millisecond() -> Option<Self> {
        type Period = unsafe extern "system" fn(u32) -> u32;
        let begin: Period =
            unsafe { std::mem::transmute(function(w!("winmm.dll"), b"timeBeginPeriod\0")?) };
        (unsafe { begin(1) } == 0).then_some(Self)
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        type Period = unsafe extern "system" fn(u32) -> u32;
        if let Some(end) = function(w!("winmm.dll"), b"timeEndPeriod\0") {
            let end: Period = unsafe { std::mem::transmute(end) };
            unsafe { end(1) };
        }
    }
}

/// Raises GPU scheduling priority for this process (high, not realtime: no
/// administrator rights needed) and for `device`'s submissions, so a game
/// saturating the GPU does not starve capture, conversion, and encode.
pub fn raise_gpu_priority(device: &ID3D11Device) {
    const HIGH: i32 = 4;
    type SetClass = unsafe extern "system" fn(HANDLE, i32) -> i32;
    match function(
        w!("gdi32.dll"),
        b"D3DKMTSetProcessSchedulingPriorityClass\0",
    ) {
        Some(set) => {
            let set: SetClass = unsafe { std::mem::transmute(set) };
            let status = unsafe { set(GetCurrentProcess(), HIGH) };
            if status != 0 {
                tracing::info!(status, "GPU process priority unchanged");
            }
        }
        None => tracing::info!("D3DKMTSetProcessSchedulingPriorityClass unavailable"),
    }
    if let Ok(dxgi) = device.cast::<IDXGIDevice>()
        && let Err(error) = unsafe { dxgi.SetGPUThreadPriority(7) }
    {
        tracing::info!(%error, "GPU thread priority unchanged");
    }
}

#[repr(C)]
struct WlanInterfaceInfo {
    guid: GUID,
    description: [u16; 256],
    state: u32,
}

#[repr(C)]
struct WlanInterfaceList {
    count: u32,
    index: u32,
    interfaces: [WlanInterfaceInfo; 1],
}

/// Puts every Wi-Fi interface into media-streaming mode (fewer background
/// scans, which otherwise stall the radio for tens of milliseconds) while the
/// guard lives.
pub struct WlanStreamingMode {
    handle: HANDLE,
    interfaces: Vec<GUID>,
}

const WLAN_INTF_OPCODE_MEDIA_STREAMING_MODE: u32 = 0x0e;

type WlanOpen = unsafe extern "system" fn(u32, *mut c_void, *mut u32, *mut HANDLE) -> u32;
type WlanEnum = unsafe extern "system" fn(HANDLE, *mut c_void, *mut *mut WlanInterfaceList) -> u32;
type WlanSet =
    unsafe extern "system" fn(HANDLE, *const GUID, u32, u32, *const c_void, *mut c_void) -> u32;
type WlanFree = unsafe extern "system" fn(*mut c_void);
type WlanClose = unsafe extern "system" fn(HANDLE, *mut c_void) -> u32;

impl WlanStreamingMode {
    pub fn enable() -> Option<Self> {
        unsafe {
            let open: WlanOpen =
                std::mem::transmute(function(w!("wlanapi.dll"), b"WlanOpenHandle\0")?);
            let enumerate: WlanEnum =
                std::mem::transmute(function(w!("wlanapi.dll"), b"WlanEnumInterfaces\0")?);
            let set: WlanSet =
                std::mem::transmute(function(w!("wlanapi.dll"), b"WlanSetInterface\0")?);
            let free: WlanFree =
                std::mem::transmute(function(w!("wlanapi.dll"), b"WlanFreeMemory\0")?);
            let mut version = 0;
            let mut handle = HANDLE::default();
            if open(2, std::ptr::null_mut(), &mut version, &mut handle) != 0 {
                return None;
            }
            let mut list = std::ptr::null_mut();
            let mut interfaces = Vec::new();
            if enumerate(handle, std::ptr::null_mut(), &mut list) == 0 && !list.is_null() {
                let count = (*list).count as usize;
                let first = std::ptr::addr_of!((*list).interfaces) as *const WlanInterfaceInfo;
                for i in 0..count {
                    let guid = (*first.add(i)).guid;
                    let on: u32 = 1;
                    let status = set(
                        handle,
                        &guid,
                        WLAN_INTF_OPCODE_MEDIA_STREAMING_MODE,
                        4,
                        &on as *const u32 as *const c_void,
                        std::ptr::null_mut(),
                    );
                    if status == 0 {
                        interfaces.push(guid);
                    } else {
                        tracing::info!(status, "WLAN media-streaming mode unavailable");
                    }
                }
                free(list.cast());
            }
            Some(Self { handle, interfaces })
        }
    }

    pub fn interfaces(&self) -> usize {
        self.interfaces.len()
    }
}

impl Drop for WlanStreamingMode {
    fn drop(&mut self) {
        unsafe {
            if let Some(set) = function(w!("wlanapi.dll"), b"WlanSetInterface\0") {
                let set: WlanSet = std::mem::transmute(set);
                let off: u32 = 0;
                for guid in &self.interfaces {
                    set(
                        self.handle,
                        guid,
                        WLAN_INTF_OPCODE_MEDIA_STREAMING_MODE,
                        4,
                        &off as *const u32 as *const c_void,
                        std::ptr::null_mut(),
                    );
                }
            }
            if let Some(close) = function(w!("wlanapi.dll"), b"WlanCloseHandle\0") {
                let close: WlanClose = std::mem::transmute(close);
                close(self.handle, std::ptr::null_mut());
            }
        }
    }
}

/// Marks a UDP socket's traffic to one peer as audio/video through qWAVE
/// (DSCP CS5 / WMM video), which Windows applies without administrator
/// rights; setting `IP_TOS` directly is ignored on Windows.
pub struct QosFlow {
    handle: HANDLE,
    socket: usize,
    flow: u32,
}

#[repr(C)]
struct QosVersion {
    major: u16,
    minor: u16,
}

const QOS_TRAFFIC_TYPE_AUDIO_VIDEO: i32 = 3;
const QOS_NON_ADAPTIVE_FLOW: u32 = 2;

type QosCreate = unsafe extern "system" fn(*const QosVersion, *mut HANDLE) -> i32;
type QosAdd = unsafe extern "system" fn(HANDLE, usize, *const c_void, i32, u32, *mut u32) -> i32;
type QosRemove = unsafe extern "system" fn(HANDLE, usize, u32, u32) -> i32;
type QosClose = unsafe extern "system" fn(HANDLE) -> i32;

impl QosFlow {
    /// # Safety
    /// `socket` must be a valid Winsock SOCKET and `destination` a valid
    /// `SOCKADDR` (or null for a connected socket) for the call's duration.
    pub unsafe fn add(socket: usize, destination: *const c_void) -> Option<Self> {
        unsafe {
            let create: QosCreate =
                std::mem::transmute(function(w!("qwave.dll"), b"QOSCreateHandle\0")?);
            let add: QosAdd =
                std::mem::transmute(function(w!("qwave.dll"), b"QOSAddSocketToFlow\0")?);
            let mut handle = HANDLE::default();
            if create(&QosVersion { major: 1, minor: 0 }, &mut handle) == 0 {
                return None;
            }
            let mut flow = 0;
            if add(
                handle,
                socket,
                destination,
                QOS_TRAFFIC_TYPE_AUDIO_VIDEO,
                QOS_NON_ADAPTIVE_FLOW,
                &mut flow,
            ) == 0
            {
                tracing::info!("qWAVE flow unavailable for this socket");
                if let Some(close) = function(w!("qwave.dll"), b"QOSCloseHandle\0") {
                    let close: QosClose = std::mem::transmute(close);
                    close(handle);
                }
                return None;
            }
            Some(Self {
                handle,
                socket,
                flow,
            })
        }
    }
}

impl Drop for QosFlow {
    fn drop(&mut self) {
        unsafe {
            if let Some(remove) = function(w!("qwave.dll"), b"QOSRemoveSocketFromFlow\0") {
                let remove: QosRemove = std::mem::transmute(remove);
                remove(self.handle, self.socket, self.flow, 0);
            }
            if let Some(close) = function(w!("qwave.dll"), b"QOSCloseHandle\0") {
                let close: QosClose = std::mem::transmute(close);
                close(self.handle);
            }
        }
    }
}

/// Keeps the GPU out of its idle power state while a session streams.
///
/// NVENC latency scales with the video clock, and NVENC load alone does not
/// raise the P-state (spike 0-C: 7.9 ms at P8 vs 3.3 ms at a higher state for
/// a 1080p H.264 picture). A small, regular 3D workload on the encoding GPU
/// does. `clears_per_tick` render-target clears of a 1024x1024 texture run
/// every `tick`; stop by dropping the guard.
pub struct GpuClockHold {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl GpuClockHold {
    pub fn start(
        device: &ID3D11Device,
        tick: Duration,
        clears_per_tick: u32,
    ) -> anyhow::Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 1024,
            Height: 1024,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture))? };
        let texture = texture.expect("created");
        let mut view = None;
        unsafe { device.CreateRenderTargetView(&texture, None, Some(&mut view))? };
        let view = view.expect("created");
        let context = unsafe { device.GetImmediateContext()? };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        struct Parts(
            windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
            windows::Win32::Graphics::Direct3D11::ID3D11RenderTargetView,
        );
        unsafe impl Send for Parts {}
        let parts = Parts(context, view);
        let worker = std::thread::Builder::new()
            .name("frames-gpu-hold".into())
            .spawn(move || {
                let parts = parts;
                let mut n = 0u32;
                while !flag.load(Ordering::Relaxed) {
                    for _ in 0..clears_per_tick {
                        n = n.wrapping_add(1);
                        let colour = [(n % 7) as f32 / 7.0, 0.5, 0.25, 1.0];
                        unsafe {
                            parts.0.ClearRenderTargetView(&parts.1, &colour);
                        }
                    }
                    unsafe { parts.0.Flush() };
                    std::thread::sleep(tick);
                }
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}

impl Drop for GpuClockHold {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
