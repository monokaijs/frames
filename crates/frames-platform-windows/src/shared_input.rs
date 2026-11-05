//! Isolated input for one shared window (games): instead of the host's real
//! mouse and keyboard (`SendInput`), input goes into the game process
//! through `FramesInput.dll`, so the person at the host keeps using their
//! own mouse and keyboard elsewhere. See docs/SHARED_GAME.md.
//!
//! The DLL is embedded in the binary, extracted to a folder named after
//! its hash and checked before loading. Attaching injects it into the
//! game's process (x64, same user, session and integrity level) and opens
//! an owner-only pipe; commands carry a generation and a rising sequence.
//! The adapter pauses input when its lease expires (no heartbeat within
//! 500 ms), the game window is minimized, or input overflows; a resume
//! starts it again.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{BOOL, PCSTR, PCWSTR};

const ARTIFACT: &[u8] = include_bytes!(env!("FRAMES_INPUT_ARTIFACT"));
const MAGIC: u32 = 0x4653_4931;
const ABI: u32 = 1;

// The adapter's local ABI (native/windows/shared-input/wire.h).
const KEY: u32 = 1;
const ABSOLUTE: u32 = 2;
const RELATIVE: u32 = 3;
const BUTTON: u32 = 4;
const SCROLL: u32 = 5;
const RELEASE: u32 = 6;
const HEARTBEAT: u32 = 7;
const RESUME: u32 = 8;

#[repr(C)]
#[derive(Default)]
struct Command {
    magic: u32,
    size: u32,
    abi: u32,
    kind: u32,
    generation: u64,
    sequence: u64,
    a: i32,
    b: i32,
    c: i32,
    d: i32,
    reserved: [u64; 2],
}

#[repr(C)]
struct NativeStatus {
    magic: u32,
    size: u32,
    abi: u32,
    phase: u32,
    error: u32,
    width: u32,
    height: u32,
    hot_x: u32,
    hot_y: u32,
    visible: u32,
    x: i32,
    y: i32,
    generation: u64,
    image_generation: u64,
    delivered: u64,
    latency_max_us: u64,
    latency_histogram: [u64; 32],
    rgba: [u8; 65536],
}

const _: () = {
    assert!(std::mem::size_of::<Command>() == 64);
    assert!(std::mem::size_of::<NativeStatus>() == 65872);
};

type Attach = unsafe extern "C" fn(*const u16, u64, u64, *mut u32) -> *mut c_void;
type SendCommand = unsafe extern "C" fn(*mut c_void, *const Command) -> BOOL;
type Poll = unsafe extern "C" fn(*mut c_void, *mut NativeStatus) -> BOOL;
type Close = unsafe extern "C" fn(*mut c_void);
type AudioStart = unsafe extern "C" fn(u32, *mut i32) -> *mut c_void;
type AudioRead = unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> u32;
type AudioClose = unsafe extern "C" fn(*mut c_void);

struct Library {
    _module: HMODULE,
    path: PathBuf,
    attach: Attach,
    send: SendCommand,
    poll: Poll,
    close: Close,
    audio_start: AudioStart,
    audio_read: AudioRead,
    audio_close: AudioClose,
}

// The functions are thread-safe per broker handle; the module stays loaded.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Loads the adapter, extracting it under `dir` the first time.
fn library(dir: &Path) -> Result<Arc<Library>> {
    static LIBRARY: OnceLock<std::result::Result<Arc<Library>, String>> = OnceLock::new();
    LIBRARY
        .get_or_init(|| load(dir).map(Arc::new).map_err(|e| format!("{e:#}")))
        .clone()
        .map_err(anyhow::Error::msg)
}

fn load(dir: &Path) -> Result<Library> {
    let hash = format!("{:x}", Sha256::digest(ARTIFACT));
    let directory = dir.join("shared-input").join(&hash);
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("FramesInput.dll");
    if !path.exists() {
        let temporary = directory.join(format!("FramesInput.{}.tmp", std::process::id()));
        std::fs::write(&temporary, ARTIFACT)?;
        if let Err(error) = std::fs::rename(&temporary, &path) {
            let _ = std::fs::remove_file(&temporary);
            if !path.exists() {
                return Err(error.into());
            }
        }
    }
    anyhow::ensure!(
        Sha256::digest(std::fs::read(&path)?).as_slice() == Sha256::digest(ARTIFACT).as_slice(),
        "shared input library checksum mismatch"
    );
    unsafe {
        let module = LoadLibraryExW(
            PCWSTR(wide(&path.to_string_lossy()).as_ptr()),
            None,
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32,
        )?;
        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                let address = GetProcAddress(module, PCSTR(concat!($name, "\0").as_ptr()))
                    .context(concat!("missing ", $name))?;
                std::mem::transmute::<unsafe extern "system" fn() -> isize, $ty>(address)
            }};
        }
        Ok(Library {
            _module: module,
            path,
            attach: symbol!("FramesBrokerAttach", Attach),
            send: symbol!("FramesBrokerSend", SendCommand),
            poll: symbol!("FramesBrokerPoll", Poll),
            close: symbol!("FramesBrokerClose", Close),
            audio_start: symbol!("FramesAudioStart", AudioStart),
            audio_read: symbol!("FramesAudioRead", AudioRead),
            audio_close: symbol!("FramesAudioClose", AudioClose),
        })
    }
}

/// One process's audio (and its children's), as v1 captured a shared
/// application's: process loopback, 48 kHz stereo f32 PCM.
pub struct ProcessAudio {
    library: Arc<Library>,
    handle: *mut c_void,
}

// The capture handle is read from one thread at a time.
unsafe impl Send for ProcessAudio {}

impl ProcessAudio {
    /// Starts capturing `pid`'s audio; `dir` holds the extracted adapter.
    /// Needs Windows build 20348 or later.
    pub fn start(pid: u32, dir: &Path) -> Result<Self> {
        let library = library(dir)?;
        let mut error = 0;
        let handle = unsafe { (library.audio_start)(pid, &mut error) };
        anyhow::ensure!(
            !handle.is_null(),
            "application audio is unavailable (HRESULT 0x{:08x}); it needs Windows build 20348 or later",
            error as u32
        );
        Ok(Self { library, handle })
    }

    /// Reads PCM into `buffer`: its length (0 for nothing yet), or `None`
    /// once the capture ended.
    pub fn read(&mut self, buffer: &mut [u8]) -> Option<usize> {
        let count = unsafe {
            (self.library.audio_read)(self.handle, buffer.as_mut_ptr(), buffer.len() as u32)
        };
        (count != u32::MAX).then_some(count as usize)
    }
}

impl Drop for ProcessAudio {
    fn drop(&mut self) {
        unsafe { (self.library.audio_close)(self.handle) };
    }
}

/// Whether the adapter is paused or playing, and its cursor.
#[derive(Clone, Debug, Default)]
pub struct Status {
    pub active: bool,
    /// A Windows error that paused input (0: none).
    pub error: u32,
    /// The game's cursor, when it shows one: straight RGBA, with hotspot.
    pub cursor: Option<SharedCursor>,
    /// Changes when the cursor image changes.
    pub image_generation: u64,
}

#[derive(Clone, Debug)]
pub struct SharedCursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub visible: bool,
    pub rgba: Vec<u8>,
}

/// The adapter attached to one window.
pub struct SharedInput {
    library: Arc<Library>,
    handle: *mut c_void,
    hwnd: HWND,
    generation: u64,
    sequence: u64,
    /// Relative mouse: buttons press where the game's cursor is.
    relative: bool,
}

// The broker handle is used from the owning thread only.
unsafe impl Send for SharedInput {}

impl SharedInput {
    /// Attaches to `hwnd`'s process; `dir` holds the extracted adapter.
    pub fn attach(hwnd: HWND, dir: &Path) -> Result<Self> {
        let library = library(dir)?;
        let generation = (u64::from(std::process::id()) << 32 | u64::from(rand_u32())).max(1);
        let mut error = 0;
        let handle = unsafe {
            (library.attach)(
                wide(&library.path.to_string_lossy()).as_ptr(),
                hwnd.0 as u64,
                generation,
                &mut error,
            )
        };
        anyhow::ensure!(
            !handle.is_null(),
            "cannot attach the isolated input adapter (Windows error {error}): the game must be x64, in this user session and at the same privilege level"
        );
        Ok(Self {
            library,
            handle,
            hwnd,
            generation,
            sequence: 0,
            relative: false,
        })
    }

    fn send(&mut self, kind: u32, a: i32, b: i32, c: i32, d: i32, flag: u64) -> Result<()> {
        self.sequence += 1;
        let command = Command {
            magic: MAGIC,
            size: std::mem::size_of::<Command>() as u32,
            abi: ABI,
            kind,
            generation: self.generation,
            sequence: self.sequence,
            a,
            b,
            c,
            d,
            reserved: [0, flag],
        };
        anyhow::ensure!(
            unsafe { (self.library.send)(self.handle, &command) }.as_bool(),
            "the game's input adapter stopped"
        );
        Ok(())
    }

    /// A normalized position (0-65535) in the window's client area, in
    /// client pixels.
    fn client_point(&self, x: u16, y: u16) -> (i32, i32) {
        use windows::Win32::UI::HiDpi::{
            GetWindowDpiAwarenessContext, SetThreadDpiAwarenessContext,
        };
        let mut rect = windows::Win32::Foundation::RECT::default();
        unsafe {
            // In the window's own coordinates: the adapter delivers them
            // inside the game, and a DPI-unaware game's client area is
            // smaller than what this DPI-aware thread would be told (by the
            // display's scale; clicks landed that much too far).
            let previous = SetThreadDpiAwarenessContext(GetWindowDpiAwarenessContext(self.hwnd));
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(self.hwnd, &mut rect);
            if !previous.is_invalid() {
                SetThreadDpiAwarenessContext(previous);
            }
        }
        let scale = |value: u16, extent: i32| {
            (f64::from(value) / 65535.0 * f64::from((extent - 1).max(0))).round() as i32
        };
        (
            scale(x, rect.right - rect.left),
            scale(y, rect.bottom - rect.top),
        )
    }

    pub fn set_relative(&mut self, relative: bool) {
        self.relative = relative;
    }

    /// A key by scan code (0xE0xx for extended keys).
    pub fn key(&mut self, scan: u16, down: bool) -> Result<()> {
        self.send(KEY, i32::from(scan), i32::from(down), 0, 0, 0)
    }

    pub fn move_to(&mut self, x: u16, y: u16) -> Result<()> {
        let (x, y) = self.client_point(x, y);
        self.send(ABSOLUTE, x, y, 0, 0, 0)
    }

    pub fn motion(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.send(RELATIVE, dx, dy, 0, 0, 0)
    }

    /// `button`: 1 left, 2 right, 3 middle, 4 back, 5 forward.
    pub fn button(&mut self, button: i32, down: bool, x: u16, y: u16) -> Result<()> {
        let (x, y) = self.client_point(x, y);
        let relative = u64::from(self.relative);
        self.send(BUTTON, button, i32::from(down), x, y, relative)
    }

    pub fn scroll(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.send(SCROLL, dx, dy, 0, 0, 0)
    }

    /// Releases everything held and pauses input until [`Self::resume`].
    pub fn release(&mut self) -> Result<()> {
        self.send(RELEASE, 0, 0, 0, 0, 0)
    }

    /// Keeps the lease (send at least every 500 ms).
    pub fn heartbeat(&mut self) -> Result<()> {
        self.send(HEARTBEAT, 0, 0, 0, 0, 0)
    }

    pub fn resume(&mut self) -> Result<()> {
        self.send(RESUME, 0, 0, 0, 0, 0)
    }

    pub fn status(&mut self) -> Result<Status> {
        let mut native = Box::new(unsafe { std::mem::zeroed::<NativeStatus>() });
        anyhow::ensure!(
            unsafe { (self.library.poll)(self.handle, &mut *native) }.as_bool(),
            "the game's input adapter disconnected"
        );
        let len = (native.width * native.height * 4) as usize;
        let cursor = (len > 0 && len <= native.rgba.len()).then(|| SharedCursor {
            width: native.width,
            height: native.height,
            hot_x: native.hot_x,
            hot_y: native.hot_y,
            visible: native.visible != 0,
            rgba: native.rgba[..len].to_vec(),
        });
        Ok(Status {
            active: native.phase == 1,
            error: native.error,
            cursor,
            image_generation: native.image_generation,
        })
    }
}

impl Drop for SharedInput {
    fn drop(&mut self) {
        unsafe { (self.library.close)(self.handle) }
    }
}

/// A random number from the system's generator (for session generations).
fn rand_u32() -> u32 {
    let mut bytes = [0u8; 4];
    let _ = unsafe {
        windows::Win32::Security::Cryptography::BCryptGenRandom(
            None,
            &mut bytes,
            windows::Win32::Security::Cryptography::BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    u32::from_le_bytes(bytes)
}
