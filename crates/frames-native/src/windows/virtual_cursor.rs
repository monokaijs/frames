//! A layered child of the viewer, so the virtual pointer never covers unrelated local apps.
use super::wide;
use anyhow::{Context, Result};
use base64::Engine;
use frames_core::shared_game::Cursor;
use windows::{
    Win32::{Foundation::*, Graphics::Gdi::*, UI::WindowsAndMessaging::*},
    core::*,
};

pub(super) struct VirtualCursor {
    window: HWND,
    parent: HWND,
    image: Option<image::RgbaImage>,
    cursor: Cursor,
}
impl VirtualCursor {
    pub unsafe fn new(parent: HWND) -> Result<Self> {
        let window = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
                w!("Static"),
                PCWSTR(wide("").as_ptr()),
                WS_CHILD,
                0,
                0,
                1,
                1,
                Some(parent),
                None,
                None,
                None,
            )
            .context("create layered viewer child")?
        };
        Ok(Self {
            window,
            parent,
            image: None,
            cursor: Cursor::default(),
        })
    }
    pub unsafe fn update(
        &mut self,
        cursor: &Cursor,
        content: RECT,
        scale: (f64, f64),
    ) -> Result<()> {
        if let Some(png) = &cursor.png_base64 {
            anyhow::ensure!(png.len() <= 128 * 1024, "cursor payload too large");
            let bytes = base64::engine::general_purpose::STANDARD.decode(png)?;
            self.image = Some(super::decode_png(&bytes, 128, 512 * 1024)?.to_rgba8());
        }
        self.cursor = cursor.clone();
        unsafe { self.render(content, scale) }
    }
    pub unsafe fn reposition(&mut self, content: RECT, scale: (f64, f64)) -> Result<()> {
        unsafe { self.render(content, scale) }
    }
    unsafe fn render(&self, content: RECT, scale: (f64, f64)) -> Result<()> {
        if !self.cursor.visible || self.image.is_none() {
            unsafe {
                let _ = ShowWindow(self.window, SW_HIDE);
            }
            return Ok(());
        }
        let image = self.image.as_ref().unwrap();
        let width = (image.width() as f64 * scale.0).round().clamp(1., 256.) as u32;
        let height = (image.height() as f64 * scale.1).round().clamp(1., 256.) as u32;
        let resized =
            image::imageops::resize(image, width, height, image::imageops::FilterType::Triangle);
        unsafe {
            let dc = CreateCompatibleDC(None);
            anyhow::ensure!(!dc.is_invalid(), "cannot allocate virtual cursor context");
            let mut bits = std::ptr::null_mut();
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let bitmap = match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
                Ok(bitmap) => bitmap,
                Err(error) => {
                    let _ = DeleteDC(dc);
                    return Err(error.into());
                }
            };
            let pixels =
                std::slice::from_raw_parts_mut(bits.cast::<u8>(), (width * height * 4) as usize);
            for (source, destination) in resized
                .as_raw()
                .chunks_exact(4)
                .zip(pixels.chunks_exact_mut(4))
            {
                let a = u16::from(source[3]);
                destination.copy_from_slice(&[
                    (u16::from(source[2]) * a / 255) as u8,
                    (u16::from(source[1]) * a / 255) as u8,
                    (u16::from(source[0]) * a / 255) as u8,
                    source[3],
                ]);
            }
            let old = SelectObject(dc, HGDIOBJ(bitmap.0));
            let mut position = POINT {
                x: content.left
                    + ((content.right - content.left) as f64 * self.cursor.x
                        - self.cursor.hotspot_x as f64 * scale.0)
                        .round() as i32,
                y: content.top
                    + ((content.bottom - content.top) as f64 * self.cursor.y
                        - self.cursor.hotspot_y as f64 * scale.1)
                        .round() as i32,
            };
            let _ = ClientToScreen(self.parent, &mut position);
            let result = UpdateLayeredWindow(
                self.window,
                None,
                Some(&position),
                Some(&SIZE {
                    cx: width as i32,
                    cy: height as i32,
                }),
                Some(dc),
                Some(&POINT::default()),
                COLORREF(0),
                Some(&BLENDFUNCTION {
                    BlendOp: AC_SRC_OVER as u8,
                    BlendFlags: 0,
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                }),
                ULW_ALPHA,
            );
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
            let _ = DeleteDC(dc);
            result.context("update layered viewer cursor")?;
            let _ = ShowWindow(self.window, SW_SHOWNOACTIVATE);
            Ok(())
        }
    }
}
impl Drop for VirtualCursor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_cursor_is_a_nonactivating_viewer_child_and_preserves_image_deltas() {
        unsafe {
            let foreground = GetForegroundWindow();
            let parent = CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("Static"),
                w!("Shared cursor fixture"),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                400,
                300,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let result = (|| -> Result<()> {
                let mut overlay = VirtualCursor::new(parent)?;
                let image = image::RgbaImage::from_pixel(8, 8, image::Rgba([255, 0, 0, 255]));
                let mut png = std::io::Cursor::new(Vec::new());
                image.write_to(&mut png, image::ImageFormat::Png)?;
                let mut cursor = Cursor {
                    x: 0.5,
                    y: 0.5,
                    visible: true,
                    hotspot_x: 1,
                    hotspot_y: 1,
                    png_base64: Some(
                        base64::engine::general_purpose::STANDARD.encode(png.into_inner()),
                    ),
                };
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: 320,
                    bottom: 240,
                };
                overlay.update(&cursor, rect, (1., 1.))?;
                assert_eq!(GetParent(overlay.window)?, parent);
                assert_ne!(
                    GetWindowLongPtrW(overlay.window, GWL_EXSTYLE) as u32 & WS_EX_NOACTIVATE.0,
                    0
                );
                let mut bounds = RECT::default();
                GetWindowRect(overlay.window, &mut bounds)?;
                assert_eq!(
                    (bounds.right - bounds.left, bounds.bottom - bounds.top),
                    (8, 8)
                );
                cursor.png_base64 = None;
                cursor.visible = false;
                overlay.update(&cursor, rect, (1., 1.))?;
                assert!(overlay.image.is_some());
                assert_eq!(
                    GetWindowLongPtrW(overlay.window, GWL_STYLE) as u32 & WS_VISIBLE.0,
                    0
                );
                Ok(())
            })();
            let _ = DestroyWindow(parent);
            result.unwrap();
            assert_eq!(GetForegroundWindow(), foreground);
        }
    }
}
