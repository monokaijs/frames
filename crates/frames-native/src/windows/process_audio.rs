use crate::{Capture, audio::AudioCapture};
use anyhow::{Context, Result};
use std::{
    ffi::c_void,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::{Win32::System::LibraryLoader::GetProcAddress, core::PCSTR};
type Start = unsafe extern "C" fn(u32, *mut i32) -> *mut c_void;
type Read = unsafe extern "C" fn(*mut c_void, *mut u8, u32) -> u32;
type Close = unsafe extern "C" fn(*mut c_void);
struct Guard(Arc<AtomicBool>);
impl Capture for Guard {
    fn stop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.stop();
    }
}
pub(crate) fn start(pid: u32) -> Result<AudioCapture> {
    let library = super::shared_game::library()?;
    let (start, read, close) = unsafe {
        let start = GetProcAddress(library.module, PCSTR(c"FramesAudioStart".as_ptr().cast()))
            .context("missing process audio activation")?;
        let read = GetProcAddress(library.module, PCSTR(c"FramesAudioRead".as_ptr().cast()))
            .context("missing process audio reader")?;
        let close = GetProcAddress(library.module, PCSTR(c"FramesAudioClose".as_ptr().cast()))
            .context("missing process audio cleanup")?;
        (
            std::mem::transmute::<unsafe extern "system" fn() -> isize, Start>(start),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, Read>(read),
            std::mem::transmute::<unsafe extern "system" fn() -> isize, Close>(close),
        )
    };
    let (tx, packets) = tokio::sync::mpsc::channel(2);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("frames-game-audio".into())
        .spawn(move || {
            let _library = library;
            let mut error = 0;
            let handle = unsafe { start(pid, &mut error) };
            if handle.is_null() {
                let _ = ready_tx.send(Err(format!(
                    "Game-only audio is unavailable (HRESULT 0x{:08x}); requires Windows build 20348 or later",
                    error as u32
                )));
                return;
            }
            if ready_tx.send(Ok(())).is_err() {
                unsafe { close(handle) };
                return;
            }
            let mut bytes = vec![0u8; 38400];
            while !flag.load(Ordering::Acquire) {
                let n = unsafe { read(handle, bytes.as_mut_ptr(), bytes.len() as u32) };
                if n == u32::MAX {
                    break;
                }
                if n > 0 {
                    let _ = tx.try_send(bytes[..n as usize].to_vec());
                }
            }
            unsafe { close(handle) };
        })?;
    let ready = ready_rx.recv_timeout(std::time::Duration::from_secs(8));
    if !matches!(ready, Ok(Ok(()))) {
        stop.store(true, Ordering::Release);
        ready?.map_err(anyhow::Error::msg)?;
    }
    Ok(AudioCapture {
        format: frames_core::audio::PcmFormat {
            rate: 48000,
            channels: 2,
        },
        packets,
        guard: Box::new(Guard(stop)),
    })
}
