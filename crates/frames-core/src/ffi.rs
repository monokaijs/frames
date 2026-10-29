//! Owned UTF-8 buffers at the native boundary. No Rust allocation crosses an ABI unowned.
use crate::{NATIVE_ABI_VERSION, model::WindowGraph, protocol::Message};
use std::ffi::{CStr, CString, c_char};

#[unsafe(no_mangle)]
pub extern "C" fn frames_abi_version() -> u32 {
    NATIVE_ABI_VERSION
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_abi_one_accepts_desktop_snapshots_and_rejects_update_gaps() {
        unsafe {
            assert_eq!(frames_abi_version(), 1);
            let session = frames_session_new();
            let message=CString::new(r#"{"type":"snapshot","data":{"sequence":1,"windows":[{"key":{"id":1,"generation":1},"app_id":"fixture","app_name":"Fixture","title":"Desktop","icon_png":null,"parent":null,"role":"desktop","bounds":{"x":0,"y":0,"width":1920,"height":1080},"scale":1,"resize":"fixed","minimized":false,"visible":true,"modal":false}]}}"#).unwrap();
            let result = frames_session_apply(session, message.as_ptr());
            assert_eq!(CStr::from_ptr(result).to_str().unwrap(), r#"{"ok":true}"#);
            frames_string_free(result);
            let gap =
                CString::new(r#"{"type":"delta","data":{"sequence":3,"upsert":[],"removed":[]}}"#)
                    .unwrap();
            let result = frames_session_apply(session, gap.as_ptr());
            let value: serde_json::Value =
                serde_json::from_slice(CStr::from_ptr(result).to_bytes()).unwrap();
            assert_eq!(value["ok"], false);
            assert_eq!((*session).sequence, 1);
            assert_eq!((*session).windows.len(), 1);
            frames_string_free(result);
            frames_session_free(session);
        }
    }
}
