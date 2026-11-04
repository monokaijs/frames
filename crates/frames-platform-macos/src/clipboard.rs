//! The macOS general pasteboard for v2 clipboard sharing (text and PNG).
//! Pasteboard calls run on the main thread, which must be running
//! NSApplication ([`crate::system::run_app`]).

use crate::ffi;
use frames_clipboard::{Clipboard, Content, MAX_PNG, MAX_TEXT};

#[derive(Debug, Default)]
pub struct MacClipboard;

fn take(read: unsafe extern "C" fn(*mut *mut u8) -> i64, limit: usize) -> Option<Vec<u8>> {
    let mut pointer = std::ptr::null_mut();
    // SAFETY: on success Swift stores a malloc'd buffer of that length.
    let length = unsafe { read(&mut pointer) };
    if length < 0 || pointer.is_null() {
        return None;
    }
    // SAFETY: `length` bytes at `pointer`, freed right after copying.
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length as usize) }.to_vec();
    unsafe { ffi::frames_mac_free(pointer.cast()) };
    (bytes.len() <= limit).then_some(bytes)
}

impl Clipboard for MacClipboard {
    fn change_count(&self) -> u64 {
        // SAFETY: plain query.
        unsafe { ffi::frames_mac_clipboard_change_count() }
    }

    fn read(&self) -> Option<Content> {
        if let Some(text) = take(ffi::frames_mac_clipboard_text, MAX_TEXT) {
            return String::from_utf8(text).ok().map(Content::Text);
        }
        take(ffi::frames_mac_clipboard_png, MAX_PNG).map(Content::Png)
    }

    fn write(&mut self, content: &Content) -> Result<(), String> {
        // SAFETY: the bytes outlive the call (Swift copies them).
        let status = unsafe {
            match content {
                Content::Text(text) => {
                    ffi::frames_mac_clipboard_set_text(text.as_ptr(), text.len())
                }
                Content::Png(png) => ffi::frames_mac_clipboard_set_png(png.as_ptr(), png.len()),
            }
        };
        if status == 0 {
            Ok(())
        } else {
            Err(format!("pasteboard write failed ({status})"))
        }
    }
}
