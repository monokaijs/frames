//! Windows Graphics Capture as a [`CaptureSource`].
//!
//! Frames arrive on a free-threaded pool created on the caller's device (the
//! NVIDIA device when NVENC encodes, so capture is zero-copy into the
//! encoder). Each frame is copied into one of a small ring of owned textures,
//! so the pool buffer returns to Windows at once and the encoder's
//! registration cache sees a stable set of textures.

use frames_codec::d3d11::D3D11Surface;
use frames_media::api::{
    CaptureCaps, CaptureConfig, CaptureEvent, CaptureSource, CursorShape, CursorUpdate, MediaClock,
    MediaError, Surface, VideoFrame,
};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::Duration;
use windows::Foundation::TimeSpan;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTOPRIMARY, MonitorFromPoint};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::{CURSOR_SHOWING, CURSORINFO, GetCursorInfo, HCURSOR};
use windows::core::Interface;

const RING: usize = 4;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
}

pub struct WgcCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    direct: IDirect3DDevice,
    _item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    arrived: Receiver<()>,
    clock: Arc<dyn MediaClock>,
    size: SizeInt32,
    ring: Vec<ID3D11Texture2D>,
    next_slot: usize,
    next_id: u64,
    config: CaptureConfig,
    cursor: Option<(isize, i32, i32, bool)>,
}

// WinRT capture objects created free-threaded; the source is used by one
// thread at a time.
unsafe impl Send for WgcCapture {}

fn backend(error: windows::core::Error) -> MediaError {
    MediaError::Backend(error.to_string())
}

impl WgcCapture {
    /// Captures one window.
    pub fn window(
        hwnd: HWND,
        device: &ID3D11Device,
        clock: Arc<dyn MediaClock>,
    ) -> Result<Self, MediaError> {
        Self::start(device, clock, |interop| unsafe {
            interop.CreateForWindow(hwnd)
        })
    }

    /// Captures the primary monitor.
    pub fn primary_monitor(
        device: &ID3D11Device,
        clock: Arc<dyn MediaClock>,
    ) -> Result<Self, MediaError> {
        Self::start(device, clock, |interop| unsafe {
            interop.CreateForMonitor(MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY))
        })
    }

    fn start(
        device: &ID3D11Device,
        clock: Arc<dyn MediaClock>,
        item: impl FnOnce(&IGraphicsCaptureItemInterop) -> windows::core::Result<GraphicsCaptureItem>,
    ) -> Result<Self, MediaError> {
        unsafe {
            // Already-initialized apartments are fine.
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        if !GraphicsCaptureSession::IsSupported().map_err(backend)? {
            return Err(MediaError::Unsupported("Windows Graphics Capture"));
        }
        let context = unsafe { device.GetImmediateContext() }.map_err(backend)?;
        let dxgi: IDXGIDevice = device.cast().map_err(backend)?;
        let direct: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
            .and_then(|d| d.cast())
            .map_err(backend)?;
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(backend)?;
        let item = item(&interop).map_err(backend)?;
        let size = item.Size().map_err(backend)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )
        .map_err(backend)?;
        let session = pool.CreateCaptureSession(&item).map_err(backend)?;
        let (sender, arrived) = sync_channel(1);
        pool.FrameArrived(&windows::Foundation::TypedEventHandler::new(move |_, _| {
            let _ = sender.try_send(());
            Ok(())
        }))
        .map_err(backend)?;
        let config = CaptureConfig {
            fps: 60,
            separate_cursor: true,
        };
        let mut capture = Self {
            device: device.clone(),
            context,
            direct,
            _item: item,
            pool,
            session,
            arrived,
            clock,
            size,
            ring: Vec::new(),
            next_slot: 0,
            next_id: 0,
            config,
            cursor: None,
        };
        capture.configure(config)?;
        let _ = capture.session.SetIsBorderRequired(false);
        capture.session.StartCapture().map_err(backend)?;
        Ok(capture)
    }

    fn ensure_ring(&mut self, width: u32, height: u32) -> Result<(), MediaError> {
        let current = self.ring.first().map(|t| {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { t.GetDesc(&mut desc) };
            (desc.Width, desc.Height)
        });
        if current == Some((width, height)) {
            return Ok(());
        }
        self.ring.clear();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        for _ in 0..RING {
            let mut texture = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .map_err(backend)?;
            self.ring.push(texture.expect("created"));
        }
        Ok(())
    }

    /// Converts a capture's QPC-based `SystemRelativeTime` (100 ns units) to
    /// the media clock.
    fn captured_us(&self, relative_100ns: i64) -> u64 {
        let now = self.clock.now_us();
        let (mut counter, mut frequency) = (0i64, 0i64);
        unsafe {
            if QueryPerformanceFrequency(&mut frequency) == 0
                || QueryPerformanceCounter(&mut counter) == 0
                || frequency <= 0
            {
                return now;
            }
        }
        let now_100ns = counter as i128 * 10_000_000 / frequency as i128;
        let age_us = (now_100ns - relative_100ns as i128) / 10;
        // WGC stamps a frame with its composition time. Content that did not
        // change for a while (the first frame of a static window) can carry
        // an old stamp; it is current content, not a late frame, and must not
        // be dropped downstream as stale.
        if (0..100_000).contains(&age_us) {
            now.saturating_sub(age_us as u64)
        } else {
            now
        }
    }
}

impl CaptureSource for WgcCapture {
    fn caps(&self) -> CaptureCaps {
        CaptureCaps {
            max_fps: 240,
            separate_cursor: true,
            dirty_rects: false,
        }
    }

    fn configure(&mut self, config: CaptureConfig) -> Result<(), MediaError> {
        if config.fps == 0 || config.fps > 240 {
            return Err(MediaError::Unsupported("frame rate"));
        }
        // Windows 11 24H2+: without it WGC delivers at most ~60 Hz.
        if let Err(error) = self.session.SetMinUpdateInterval(TimeSpan {
            Duration: 10_000_000 / i64::from(config.fps),
        }) {
            tracing::debug!(%error, "MinUpdateInterval unsupported on this Windows build");
        }
        self.session
            .SetIsCursorCaptureEnabled(!config.separate_cursor)
            .map_err(backend)?;
        self.config = config;
        Ok(())
    }

    fn next(&mut self, deadline_us: u64) -> Result<CaptureEvent, MediaError> {
        loop {
            match self.pool.TryGetNextFrame() {
                Ok(mut frame) => {
                    // Only the newest frame matters.
                    while let Ok(newer) = self.pool.TryGetNextFrame() {
                        let _ = frame.Close();
                        frame = newer;
                    }
                    let content = frame.ContentSize().map_err(backend)?;
                    if content.Width <= 0 || content.Height <= 0 {
                        let _ = frame.Close();
                        continue;
                    }
                    if (content.Width, content.Height) != (self.size.Width, self.size.Height) {
                        let _ = frame.Close();
                        self.size = content;
                        self.pool
                            .Recreate(
                                &self.direct,
                                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                                2,
                                content,
                            )
                            .map_err(backend)?;
                        return Ok(CaptureEvent::Resized {
                            width: content.Width as u32,
                            height: content.Height as u32,
                        });
                    }
                    let captured_us = frame
                        .SystemRelativeTime()
                        .map(|t| self.captured_us(t.Duration))
                        .unwrap_or_else(|_| self.clock.now_us());
                    let access: IDirect3DDxgiInterfaceAccess =
                        frame.Surface().and_then(|s| s.cast()).map_err(backend)?;
                    let source: ID3D11Texture2D =
                        unsafe { access.GetInterface() }.map_err(backend)?;
                    // Encoders want even dimensions.
                    let (width, height) = (content.Width as u32 & !1, content.Height as u32 & !1);
                    self.ensure_ring(width, height)?;
                    let target = self.ring[self.next_slot].clone();
                    self.next_slot = (self.next_slot + 1) % RING;
                    unsafe {
                        self.context.CopySubresourceRegion(
                            &target,
                            0,
                            0,
                            0,
                            0,
                            &source,
                            0,
                            Some(&D3D11_BOX {
                                left: 0,
                                top: 0,
                                front: 0,
                                right: width,
                                bottom: height,
                                back: 1,
                            }),
                        );
                        self.context.Flush();
                    }
                    let _ = frame.Close();
                    let id = self.next_id;
                    self.next_id += 1;
                    return Ok(CaptureEvent::Frame(VideoFrame {
                        id,
                        captured_us,
                        surface: Surface::Platform(Arc::new(D3D11Surface {
                            texture: target,
                            subresource: 0,
                            width,
                            height,
                            matrix_709: true,
                            full_range: true,
                            keepalive: None,
                        })),
                        dirty: None,
                    }));
                }
                Err(_) => {
                    let now = self.clock.now_us();
                    // Windows rounds short channel waits up to the system
                    // timer tick (15.6 ms by default): never block for less
                    // than a couple of milliseconds.
                    if now + 2_000 >= deadline_us {
                        return Ok(CaptureEvent::Timeout);
                    }
                    let wait = Duration::from_micros((deadline_us - now).min(100_000));
                    if self.arrived.recv_timeout(wait).is_err() {
                        return Ok(CaptureEvent::Timeout);
                    }
                }
            }
        }
    }

    fn cursor(&mut self) -> Option<CursorUpdate> {
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        unsafe { GetCursorInfo(&mut info) }.ok()?;
        let visible = info.flags.0 & CURSOR_SHOWING.0 != 0;
        let handle = info.hCursor.0 as isize;
        let state = (handle, info.ptScreenPos.x, info.ptScreenPos.y, visible);
        if self.cursor == Some(state) {
            return None;
        }
        let shape_changed = self.cursor.is_none_or(|(old, ..)| old != handle);
        self.cursor = Some(state);
        Some(CursorUpdate {
            visible,
            x: info.ptScreenPos.x,
            y: info.ptScreenPos.y,
            shape: if shape_changed && visible {
                cursor_shape(info.hCursor)
            } else {
                None
            },
        })
    }
}

/// Renders a cursor into premultiplied BGRA at its own size (so the hotspot,
/// which is in cursor pixels, stays exact).
fn cursor_shape(cursor: HCURSOR) -> Option<CursorShape> {
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::WindowsAndMessaging::{
        DI_NORMAL, DrawIconEx, GetIconInfo, HICON, ICONINFO,
    };
    unsafe {
        let mut icon = ICONINFO::default();
        GetIconInfo(HICON(cursor.0), &mut icon).ok()?;
        let size_of = |bitmap: HBITMAP| {
            let mut info = BITMAP::default();
            let read = GetObjectW(
                bitmap.into(),
                std::mem::size_of::<BITMAP>() as i32,
                Some((&mut info as *mut BITMAP).cast()),
            );
            (read != 0).then_some((info.bmWidth, info.bmHeight))
        };
        // Monochrome cursors have no colour bitmap; their mask stacks the AND
        // and XOR masks, so it is twice the cursor's height.
        let size = if icon.hbmColor.is_invalid() {
            size_of(icon.hbmMask).map(|(w, h)| (w, h / 2))
        } else {
            size_of(icon.hbmColor)
        };
        let _ = DeleteObject(icon.hbmColor.into());
        let _ = DeleteObject(icon.hbmMask.into());
        let (width, height) =
            size.filter(|&(w, h)| (1..=256).contains(&w) && (1..=256).contains(&h))?;
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(dc, bitmap.into());
        let drawn =
            DrawIconEx(dc, 0, 0, HICON(cursor.0), width, height, 0, None, DI_NORMAL).is_ok();
        let mut bgra =
            std::slice::from_raw_parts(bits as *const u8, (width * height * 4) as usize).to_vec();
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        ReleaseDC(None, screen);
        if !drawn {
            return None;
        }
        // Monochrome cursors draw without alpha: treat any colour as opaque.
        if bgra.chunks(4).all(|p| p[3] == 0) {
            for p in bgra.chunks_mut(4) {
                p[3] = if p[0] | p[1] | p[2] != 0 { 255 } else { 0 };
            }
        }
        Some(CursorShape {
            width: width as u32,
            height: height as u32,
            hotspot_x: icon.xHotspot,
            hotspot_y: icon.yHotspot,
            bgra: bgra.into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::{IDC_ARROW, IDC_IBEAM, LoadCursorW};

    #[test]
    fn system_cursors_render_at_their_own_size_with_the_hotspot_inside() {
        for id in [IDC_ARROW, IDC_IBEAM] {
            let cursor = unsafe { LoadCursorW(None, id) }.unwrap();
            let shape = cursor_shape(cursor).expect("rendered");
            assert!(shape.width >= 16 && shape.width <= 256, "{shape:?}");
            assert!(shape.hotspot_x < shape.width && shape.hotspot_y < shape.height);
            assert_eq!(shape.bgra.len() as u32, shape.width * shape.height * 4);
            assert!(shape.bgra.chunks(4).any(|p| p[3] > 0), "some opaque pixels");
        }
    }
}
