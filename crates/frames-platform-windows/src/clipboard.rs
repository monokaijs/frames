//! The Windows clipboard for v2 clipboard sharing (Unicode text and PNG),
//! ported from v1 (`frames-native::windows::host`).

use frames_clipboard::{Clipboard, Content, MAX_PNG, MAX_TEXT};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::w;

const CF_UNICODETEXT: u32 = 13;

/// The clipboard of this Windows session.
#[derive(Debug, Default)]
pub struct WindowsClipboard;

/// Opens the clipboard with a hidden owner window (writes need an owner so
/// other applications see the change), and closes it on drop.
struct Open(Option<HWND>);

impl Open {
    fn for_reading() -> Option<Self> {
        unsafe { OpenClipboard(None).ok().map(|_| Self(None)) }
    }

    fn for_writing() -> Result<Self, String> {
        unsafe {
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("STATIC"),
                w!("Frames clipboard"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
            .map_err(|e| e.to_string())?;
            if let Err(error) = OpenClipboard(Some(hwnd)) {
                let _ = DestroyWindow(hwnd);
                return Err(error.to_string());
            }
            Ok(Self(Some(hwnd)))
        }
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
            if let Some(hwnd) = self.0 {
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

fn png_format() -> u32 {
    unsafe { RegisterClipboardFormatW(w!("PNG")) }
}

unsafe fn global_bytes(handle: HANDLE, limit: usize) -> Option<Vec<u8>> {
    let global = HGLOBAL(handle.0);
    let size = unsafe { GlobalSize(global) };
    if size > limit {
        return None;
    }
    let pointer = unsafe { GlobalLock(global) } as *const u8;
    if pointer.is_null() {
        return None;
    }
    let bytes = unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec();
    let _ = unsafe { GlobalUnlock(global) };
    Some(bytes)
}

unsafe fn put(format: u32, bytes: &[u8]) -> Result<(), String> {
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) }.map_err(|e| e.to_string())?;
    let pointer = unsafe { GlobalLock(handle) } as *mut u8;
    if pointer.is_null() {
        let _ = unsafe { GlobalFree(Some(handle)) };
        return Err("clipboard allocation failed".into());
    }
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
    let _ = unsafe { GlobalUnlock(handle) };
    if let Err(error) = unsafe { SetClipboardData(format, Some(HANDLE(handle.0))) } {
        let _ = unsafe { GlobalFree(Some(handle)) };
        return Err(error.to_string());
    }
    Ok(())
}

impl Clipboard for WindowsClipboard {
    fn change_count(&self) -> u64 {
        u64::from(unsafe { GetClipboardSequenceNumber() })
    }

    fn read(&self) -> Option<Content> {
        let _open = Open::for_reading()?;
        unsafe {
            if IsClipboardFormatAvailable(CF_UNICODETEXT).is_ok()
                && let Ok(handle) = GetClipboardData(CF_UNICODETEXT)
            {
                let bytes = global_bytes(handle, MAX_TEXT * 4)?;
                let units: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                    .take_while(|&unit| unit != 0)
                    .collect();
                return Some(Content::Text(String::from_utf16_lossy(&units)));
            }
            let png = png_format();
            if IsClipboardFormatAvailable(png).is_ok()
                && let Ok(handle) = GetClipboardData(png)
            {
                return Some(Content::Png(global_bytes(handle, MAX_PNG)?));
            }
        }
        None
    }

    fn write(&mut self, content: &Content) -> Result<(), String> {
        let _open = Open::for_writing()?;
        unsafe {
            EmptyClipboard().map_err(|e| e.to_string())?;
            match content {
                Content::Text(text) => {
                    let mut units: Vec<u16> = text.encode_utf16().collect();
                    units.push(0);
                    let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
                    put(CF_UNICODETEXT, &bytes)
                }
                Content::Png(png) => put(png_format(), png),
            }
        }
    }
}
