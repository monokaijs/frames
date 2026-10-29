//! Owned UTF-8 buffers at the native boundary. No Rust allocation crosses an ABI unowned.
use crate::{PROTOCOL_VERSION, model::WindowGraph, protocol::Message};
use std::ffi::{CStr, CString, c_char};

#[unsafe(no_mangle)]
pub extern "C" fn frames_abi_version() -> u32 {
    PROTOCOL_VERSION as u32
}
#[unsafe(no_mangle)]
pub extern "C" fn frames_session_new() -> *mut WindowGraph {
    Box::into_raw(Box::new(WindowGraph::default()))
}

/// # Safety
/// `session` must be a live handle returned by frames_session_new; json must be NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frames_session_apply(
    session: *mut WindowGraph,
    json: *const c_char,
) -> *mut c_char {
    if session.is_null() || json.is_null() {
        return std::ptr::null_mut();
    }
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> anyhow::Result<()> {
            let message: Message =
                serde_json::from_slice(unsafe { CStr::from_ptr(json) }.to_bytes())?;
            let graph = unsafe { &mut *session };
            match message {
                Message::Snapshot { sequence, windows } => graph.snapshot(sequence, windows)?,
                Message::Delta {
                    sequence,
                    upsert,
                    removed,
                } => graph.delta(sequence, upsert, &removed)?,
                _ => anyhow::bail!("message is not a window graph update"),
            }
            Ok(())
        }));
    let response = match result {
        Ok(Ok(())) => "{\"ok\":true}".to_owned(),
        Ok(Err(e)) => serde_json::json!({"ok":false,"error":e.to_string()}).to_string(),
        Err(_) => "{\"ok\":false,\"error\":\"native boundary panic\"}".to_owned(),
    };
    CString::new(response).unwrap().into_raw()
}
/// # Safety
/// The pointer must have been returned by frames_session_new and not previously freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frames_session_free(session: *mut WindowGraph) {
    if !session.is_null() {
        drop(unsafe { Box::from_raw(session) });
    }
}
/// # Safety
/// The pointer must have been returned by frames_session_apply and not previously freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn frames_string_free(string: *mut c_char) {
    if !string.is_null() {
        drop(unsafe { CString::from_raw(string) });
    }
}
