use crate::{Capture, CaptureStream};
use frames_core::media::RawFrame;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::watch;
use windows::{
    Graphics::{
        Capture::*,
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
    },
    Win32::{
        Foundation::{HMODULE, HWND, RECT},
        Graphics::{
            Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            Direct3D11::*,
            Dxgi::{Common::DXGI_FORMAT_B8G8R8A8_UNORM, IDXGIDevice},
            Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint},
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
            RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize,
        },
    },
    core::Interface,
};
pub const DESKTOP_HANDLE: u64 = u64::MAX - 1;

// Select only the client area from this window's own WGC texture, never from a
// display texture. All rectangles are physical pixels, even on mixed-DPI hosts.
fn client_region(frame: RECT, client: RECT, width: u32, height: u32) -> Option<D3D11_BOX> {
    if frame.right - frame.left != width as i32 || frame.bottom - frame.top != height as i32 {
        return None; // Resize/DPI transition: wait for matching capture geometry.
    }
    let left = (client.left - frame.left).max(0) as u32;
    let top = (client.top - frame.top).max(0) as u32;
    let right = (client.right - frame.left).max(0) as u32;
    let bottom = (client.bottom - frame.top).max(0) as u32;
    (left < right && top < bottom && right <= width && bottom <= height).then_some(D3D11_BOX {
        left,
        top,
        right,
        bottom,
        front: 0,
        back: 1,
    })
}

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            RoUninitialize();
        }
    }
}

fn pin_capture_runtime() -> anyhow::Result<()> {
    // WGC can still execute teardown callbacks after the final COM reference is
    // released. Windows 11 26100.9549 faulted inside GraphicsCapture.dll_unloaded.
    // Keep the system module resident until process exit, while still closing
    // sessions, frame pools, textures, and COM apartments normally.
    static PIN: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    PIN.get_or_init(|| unsafe {
        use windows::Win32::System::LibraryLoader::*;
        let result = (|| -> windows::core::Result<()> {
            let library = LoadLibraryExW(
                windows::core::w!("GraphicsCapture.dll"),
                None,
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )?;
            let mut pinned = HMODULE::default();
            let result = GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_PIN,
                windows::core::w!("GraphicsCapture.dll"),
                &mut pinned,
            );
            let _ = windows::Win32::Foundation::FreeLibrary(library);
            result
        })();
        result.map_err(|error| error.to_string())
    })
    .as_ref()
    .copied()
    .map_err(|error| anyhow::anyhow!(error.clone()))
}

struct Guard {
    stop: Arc<AtomicBool>,
}
impl Capture for Guard {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start(handle: u64) -> anyhow::Result<CaptureStream> {
    let (sender, frames) = watch::channel(None);
    let stop = Arc::new(AtomicBool::new(false));
    let guard = Guard { stop: stop.clone() };
    let flag = stop.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("frames-wgc".into())
        .spawn(move || {
            let result = unsafe { capture_thread(handle, sender, flag.clone(), ready_tx) };
            if let Err(error) = result {
                tracing::warn!(%error,handle,"window capture ended");
            }
        })?;
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))?
        .map_err(anyhow::Error::msg)?;
    Ok(CaptureStream {
        frames,
        guard: Box::new(guard),
    })
}

unsafe fn capture_thread(
    handle: u64,
    sender: watch::Sender<Option<RawFrame>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> anyhow::Result<()> {
    let _dpi = unsafe { super::host::PhysicalDpi::enter() };
    let setup = (|| -> anyhow::Result<_> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
        }
        let apartment = Apartment;
        pin_capture_runtime()?;
        anyhow::ensure!(
            GraphicsCaptureSession::IsSupported()?,
            "Windows Graphics Capture unavailable"
        );
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.unwrap();
        let context = context.unwrap();
        let dxgi: IDXGIDevice = device.cast()?;
        let direct: IDirect3DDevice =
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()? };
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = if handle == DESKTOP_HANDLE {
            unsafe {
                interop.CreateForMonitor(MonitorFromPoint(
                    windows::Win32::Foundation::POINT::default(),
                    MONITOR_DEFAULTTOPRIMARY,
                ))?
            }
        } else {
            unsafe { interop.CreateForWindow(HWND(handle as *mut _))? }
        };
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        let _ = session.SetIsCursorCaptureEnabled(handle == DESKTOP_HANDLE);
        Ok((apartment, device, context, direct, pool, session))
    })();
    let (_apartment, device, context, direct, pool, session) = match setup {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready.send(Err(e.to_string()));
            return Err(e);
        }
    };
    // Readback uses a staging texture. It never reads or crops the desktop.
    let mut texture = None::<ID3D11Texture2D>;
    let mut dims = (0u32, 0u32);
    session.StartCapture()?;
    let _ = ready.send(Ok(()));
    while !stop.load(Ordering::Acquire) {
        let frame = match pool.TryGetNextFrame() {
            Ok(frame) => frame,
            Err(_) => {
                std::thread::sleep(std::time::Duration::from_millis(2));
                continue;
            }
        };
        let result = (|| -> windows::core::Result<()> {
            let content = frame.ContentSize()?;
            if content.Width <= 0 || content.Height <= 0 {
                return Ok(());
            }
            let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let source: ID3D11Texture2D = unsafe { access.GetInterface()? };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe {
                source.GetDesc(&mut desc);
            }
            let width = (content.Width as u32).min(desc.Width);
            let height = (content.Height as u32).min(desc.Height);
            let region = if handle == DESKTOP_HANDLE {
                Some(D3D11_BOX {
                    left: 0,
                    top: 0,
                    right: width,
                    bottom: height,
                    front: 0,
                    back: 1,
                })
            } else {
                let hwnd = HWND(handle as *mut _);
                unsafe {
                    super::host::frame_bounds(hwnd)
                        .ok()
                        .zip(super::host::capture_bounds(hwnd).ok())
                        .and_then(|(frame, client)| {
                            client_region(
                                frame,
                                client,
                                content.Width as u32,
                                content.Height as u32,
                            )
                        })
                        .filter(|r| r.right <= desc.Width && r.bottom <= desc.Height)
                }
            };
            let changed =
                content.Width as u32 != desc.Width || content.Height as u32 != desc.Height;
            let Some(region) = region else {
                frame.Close()?;
                if changed {
                    pool.Recreate(
                        &direct,
                        DirectXPixelFormat::B8G8R8A8UIntNormalized,
                        2,
                        content,
                    )?;
                }
                return Ok(());
            };
            let (width, height) = (region.right - region.left, region.bottom - region.top);
            if dims != (width, height) {
                desc.Width = width;
                desc.Height = height;
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                desc.MiscFlags = 0;
                desc.Format = DXGI_FORMAT_B8G8R8A8_UNORM;
                unsafe {
                    device.CreateTexture2D(&desc, None, Some(&mut texture))?;
                }
                dims = (desc.Width, desc.Height);
            }
            let target = texture.as_ref().unwrap();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                context.CopySubresourceRegion(target, 0, 0, 0, 0, &source, 0, Some(&region));
                context.Map(target, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            }
            let mut bgra = vec![0; width as usize * height as usize * 4];
            for row in 0..height as usize {
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                        bgra.as_mut_ptr().add(row * width as usize * 4),
                        width as usize * 4,
                    );
                }
            }
            unsafe {
                context.Unmap(target, 0);
            }
            frame.Close()?;
            sender.send_replace(Some(RawFrame {
                width,
                height,
                bgra,
            }));
            if changed {
                pool.Recreate(
                    &direct,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    content,
                )?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            tracing::debug!(error=%e,"capture callback");
        }
    }
    session.Close()?;
    pool.Close()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn client_area_removes_chrome_and_preserves_negative_screen_origins() {
        let frame = RECT {
            left: -900,
            top: -100,
            right: -300,
            bottom: 400,
        };
        let client = RECT {
            left: -894,
            top: -48,
            right: -306,
            bottom: 394,
        };
        let region = client_region(frame, client, 600, 500).unwrap();
        assert_eq!(
            (region.left, region.top, region.right, region.bottom),
            (6, 52, 594, 494)
        );
        assert!(client_region(frame, client, 601, 500).is_none());
        assert!(
            client_region(
                frame,
                RECT {
                    right: frame.right + 1,
                    ..client
                },
                600,
                500
            )
            .is_none()
        );
    }
    #[test]
    #[ignore = "captures the primary display locally; no pixels are persisted or transmitted"]
    fn full_desktop_uses_a_display_capture_surface() {
        let mut capture = start(DESKTOP_HANDLE).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let valid =
            loop {
                if capture.frames.borrow().as_ref().is_some_and(|frame| {
                    frame.validate() && frame.width > 100 && frame.height > 100
                }) {
                    break true;
                }
                if std::time::Instant::now() > end {
                    break false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
        capture.guard.stop();
        assert!(valid, "no valid display frame arrived");
    }
    unsafe extern "system" fn fixture_proc(
        hwnd: windows::Win32::Foundation::HWND,
        message: u32,
        wparam: windows::Win32::Foundation::WPARAM,
        lparam: windows::Win32::Foundation::LPARAM,
    ) -> windows::Win32::Foundation::LRESULT {
        unsafe {
            windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, message, wparam, lparam)
        }
    }
    #[test]
    #[ignore = "requires an interactive Windows desktop; opens two temporary fixture windows"]
    fn captures_the_window_while_another_window_covers_it() {
        use windows::{
            Win32::{
                Foundation::{HWND, LPARAM, WPARAM},
                Graphics::Gdi::*,
                System::LibraryLoader::GetModuleHandleW,
                UI::WindowsAndMessaging::*,
            },
            core::w,
        };
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let thread = std::thread::spawn(move || unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            let red = CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x0000ff));
            let blue = CreateSolidBrush(windows::Win32::Foundation::COLORREF(0xff0000));
            for (name, brush) in [
                (w!("FramesFixtureSource"), red),
                (w!("FramesFixtureCover"), blue),
            ] {
                RegisterClassW(&WNDCLASSW {
                    lpfnWndProc: Some(fixture_proc),
                    hInstance: instance.into(),
                    lpszClassName: name,
                    hbrBackground: brush,
                    ..Default::default()
                });
            }
            let menu = CreateMenu().unwrap();
            AppendMenuW(menu, MF_STRING, 101, w!("Fixture menu")).unwrap();
            let source = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("FramesFixtureSource"),
                w!("Frames direct capture fixture"),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                400,
                300,
                None,
                Some(menu),
                Some(instance.into()),
                None,
            )
            .unwrap();
            let cover = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("FramesFixtureCover"),
                w!("Frames occlusion fixture"),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                400,
                300,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let _ = ShowWindow(source, SW_SHOWNOACTIVATE);
            let _ = UpdateWindow(source);
            let _ = ShowWindow(cover, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                cover,
                Some(HWND_TOPMOST),
                100,
                100,
                400,
                300,
                SWP_NOACTIVATE,
            );
            let _ = UpdateWindow(cover);
            let mut client = windows::Win32::Foundation::RECT::default();
            GetClientRect(source, &mut client).unwrap();
            ready_tx
                .send((source.0 as u64, client.right as u32, client.bottom as u32))
                .unwrap();
            let mut message = MSG::default();
            while stop_rx.try_recv().is_err() {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let _ = DestroyWindow(source);
            let _ = DestroyWindow(cover);
            let _ = DeleteObject(red.into());
            let _ = DeleteObject(blue.into());
            let _ = (HWND::default(), WPARAM(0), LPARAM(0));
        });
        let (handle, width, height) = ready_rx.recv().unwrap();
        let mut stream = start(handle).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let frame = loop {
            if let Some(frame) = stream.frames.borrow().clone() {
                break Some(frame);
            }
            if std::time::Instant::now() > end {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        stream.guard.stop();
        let _ = stop_tx.send(());
        thread.join().unwrap();
        let frame = frame.expect("no frame captured");
        assert!(frame.validate());
        assert_eq!(
            (frame.width, frame.height),
            (width, height),
            "stream includes non-client chrome"
        );
        for (x, y) in [
            (8, 8),
            (width - 9, 8),
            (8, height - 9),
            (width - 9, height - 9),
        ] {
            let offset = ((y * width + x) * 4) as usize;
            let pixel = &frame.bgra[offset..offset + 4];
            assert!(
                pixel[2] > 200 && pixel[0] < 40,
                "non-client frame/menu leaked at {x},{y}: {pixel:?}"
            );
        }
        let center = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
        let pixel = &frame.bgra[center..center + 4];
        assert!(
            pixel[2] > 200 && pixel[0] < 40,
            "captured the covering window instead of source: {pixel:?}"
        );
    }
}
