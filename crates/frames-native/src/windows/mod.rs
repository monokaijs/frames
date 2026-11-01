mod caption;
mod capture;
mod client;
mod devices;
pub(crate) mod dialogs;
mod drag;
mod files;
mod host;
pub mod launcher;
mod menus;
#[cfg(frames_shared_input)]
pub(crate) mod process_audio;
mod render;
mod secure;
mod session_control;
#[cfg(frames_shared_input)]
pub(crate) mod shared_game;
pub mod tray;
mod virtual_cursor;
pub use client::run_client;
pub use host::Host;
#[cfg(feature = "media-diagnostics")]
pub fn diagnostic_capture_window(title: &str) -> anyhow::Result<crate::CaptureStream> {
    use windows::Win32::{Foundation::*, UI::WindowsAndMessaging::*};
    use windows_core::BOOL;
    struct Search {
        title: String,
        matches: Vec<HWND>,
    }
    unsafe extern "system" fn find(window: HWND, context: LPARAM) -> BOOL {
        unsafe {
            let search = &mut *(context.0 as *mut Search);
            if !IsWindowVisible(window).as_bool() || IsIconic(window).as_bool() {
                return TRUE;
            }
            let mut text = vec![0; GetWindowTextLengthW(window).max(0) as usize + 1];
            let length = GetWindowTextW(window, &mut text);
            if length > 0
                && String::from_utf16_lossy(&text[..length as usize]).contains(&search.title)
            {
                search.matches.push(window);
            }
            TRUE
        }
    }
    let mut search = Search {
        title: title.into(),
        matches: vec![],
    };
    unsafe {
        EnumWindows(Some(find), LPARAM(&mut search as *mut _ as isize))?;
    }
    anyhow::ensure!(
        search.matches.len() == 1,
        "expected one visible comparison window matching {title:?}, found {}",
        search.matches.len()
    );
    capture::start(search.matches[0].0 as u64, false)
}
fn decode_png(
    bytes: &[u8],
    max_dimension: u32,
    max_alloc: u64,
) -> anyhow::Result<image::DynamicImage> {
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max_dimension);
    limits.max_image_height = Some(max_dimension);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    Ok(reader.decode()?)
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
