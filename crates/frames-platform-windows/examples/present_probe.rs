//! Show one elementary HEVC keyframe at 1:1 to isolate decode/present padding.
#[cfg(not(windows))]
fn main() {}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use anyhow::Context as _;
    use frames_codec::ffmpeg::FfmpegDecoder;
    use frames_media::api::{
        AccessUnit, Codec, Decoded, FrameKind, FrameReference, PresentMode, Presenter, VideoDecoder,
    };
    use frames_media::clock::SystemClock;
    use frames_platform_windows::present::D3D11Presenter;
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use windows::{
        Win32::{
            Foundation::*,
            System::LibraryLoader::GetModuleHandleW,
            UI::{HiDpi::*, WindowsAndMessaging::*},
        },
        core::w,
    };
    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }
    let args: Vec<_> = std::env::args().collect();
    let width: u32 = args[2].parse()?;
    let height: u32 = args[3].parse()?;
    let mut decoder =
        FfmpegDecoder::new(Codec::Hevc, width, height, true).context("open decoder")?;
    let Decoded::Frame(frame) = decoder
        .decode(AccessUnit {
            frame: 0,
            kind: FrameKind::Keyframe,
            reference: FrameReference::None,
            mark_long_term: None,
            captured_us: 0,
            data: std::fs::read(&args[1])?.into(),
        })
        .context("decode picture")?
    else {
        anyhow::bail!("no frame");
    };
    println!("decoded visible size {:?}", frame.surface.size());
    if let frames_media::api::Surface::Platform(surface) = &frame.surface {
        let surface = surface
            .as_any()
            .downcast_ref::<frames_codec::d3d11::D3D11Surface>()
            .unwrap();
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { surface.texture.GetDesc(&mut desc) };
        println!(
            "texture {}x{} format {:?} slice {}/{}",
            desc.Width, desc.Height, desc.Format, surface.subresource, desc.ArraySize
        );
    }
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance: HINSTANCE = GetModuleHandleW(None)?.into();
        let class = w!("FramesPixelProbe");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class,
            ..Default::default()
        });
        let mut bounds = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRectExForDpi(&mut bounds, WS_OVERLAPPEDWINDOW, false, WS_EX_TOPMOST, 96)?;
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            class,
            w!("Frames Pixel Probe"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            700,
            50,
            bounds.right - bounds.left,
            bounds.bottom - bounds.top,
            None,
            None,
            Some(instance),
            None,
        )?;
        let mut presenter = D3D11Presenter::new(hwnd, Arc::new(SystemClock::new()));
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(45) {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            if !IsWindow(Some(hwnd)).as_bool() {
                break;
            }
            presenter
                .present(frame.clone(), PresentMode::Immediate)
                .context("present picture")?;
            std::thread::sleep(Duration::from_millis(16));
        }
        let _ = DestroyWindow(hwnd);
    }
    Ok(())
}
