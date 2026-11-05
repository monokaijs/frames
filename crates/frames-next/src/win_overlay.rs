//! The Windows viewer's debug overlay (Ctrl+Alt+F10), as v1's panel: a layered, click-through child of the viewer (like the
//! virtual cursor), so it never touches the D3D presentation path. Graphs and
//! translucency come from `debug_raster`; GDI only draws the text.
use crate::overlay::Overlay as DebugOverlay;
use crate::overlay_raster as raster;
use anyhow::{Context, Result};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        UI::{HiDpi::GetDpiForWindow, WindowsAndMessaging::*},
    },
    core::*,
};

pub struct DebugPanel {
    window: HWND,
    parent: HWND,
}

fn color([r, g, b]: [u8; 3]) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}
fn text(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

impl DebugPanel {
    pub unsafe fn new(parent: HWND) -> Result<Self> {
        let window = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
                w!("Static"),
                w!(""),
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
            .context("create debug overlay child")?
        };
        Ok(Self { window, parent })
    }
    pub unsafe fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.window, SW_HIDE);
        }
    }
    /// Draws `overlay` at the top-left of the video `content` rectangle.
    /// Returns false (and hides) when the content is too small to hold it.
    pub unsafe fn show(&self, overlay: &DebugOverlay, content: RECT) -> Result<bool> {
        unsafe {
            let scale = GetDpiForWindow(self.parent).max(96) as f64 / 96.0;
            let margin = (8.0 * scale).round() as i32;
            let available = content.right - content.left - margin * 2;
            let dc = CreateCompatibleDC(None);
            anyhow::ensure!(!dc.is_invalid(), "cannot allocate debug overlay context");
            let probe = raster::Layout::new(0, 0, scale, available);
            let face = w!("Consolas");
            let font = CreateFontW(
                -probe.font_height,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                (FIXED_PITCH.0 | FF_MODERN.0) as u32,
                face,
            );
            let label_font = CreateFontW(
                -probe.label_height,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                (FIXED_PITCH.0 | FF_MODERN.0) as u32,
                face,
            );
            let previous_font = SelectObject(dc, font.into());
            let lines: Vec<(Vec<u16>, [u8; 3])> = overlay
                .lines
                .iter()
                .take(24)
                .map(|line| (text(&line.text), raster::text_color(line.level)))
                .collect();
            let widest = lines
                .iter()
                .map(|(units, _)| {
                    let mut size = SIZE::default();
                    let _ = GetTextExtentPoint32W(dc, units, &mut size);
                    size.cx
                })
                .max()
                .unwrap_or(0);
            let layout = raster::Layout::new(lines.len(), widest, scale, available);
            let fits = layout.width > 0
                && available >= (120.0 * scale) as i32
                && layout.height + margin * 2 <= content.bottom - content.top;
            let result = if fits {
                self.paint(dc, &layout, overlay, &lines, label_font, scale)
                    .map(|()| true)
            } else {
                Ok(false)
            };
            SelectObject(dc, previous_font);
            let _ = DeleteObject(font.into());
            let _ = DeleteObject(label_font.into());
            let _ = DeleteDC(dc);
            match result {
                Ok(true) => {
                    let _ = SetWindowPos(
                        self.window,
                        Some(HWND_TOP),
                        content.left + margin,
                        content.top + margin,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW,
                    );
                    Ok(true)
                }
                other => {
                    self.hide();
                    other
                }
            }
        }
    }
    unsafe fn paint(
        &self,
        dc: HDC,
        layout: &raster::Layout,
        overlay: &DebugOverlay,
        lines: &[(Vec<u16>, [u8; 3])],
        label_font: HFONT,
        scale: f64,
    ) -> Result<()> {
        unsafe {
            let (width, height) = (layout.width, layout.height);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            let previous = SelectObject(dc, bitmap.into());
            let pixels =
                std::slice::from_raw_parts_mut(bits.cast::<u8>(), (width * height * 4) as usize);
            raster::draw(
                &mut raster::Canvas::new(pixels, width, height),
                layout,
                overlay,
                scale,
            );
            // GDI must finish before the CPU touches the DIB again.
            let _ = GdiFlush();
            let _ = SetBkMode(dc, TRANSPARENT);
            let inset = (layout.line_height - layout.font_height) / 2;
            for (index, (units, rgb)) in lines.iter().enumerate() {
                SetTextColor(dc, color(*rgb));
                let _ = TextOutW(
                    dc,
                    layout.padding,
                    layout.padding + index as i32 * layout.line_height + inset,
                    units,
                );
            }
            let previous_font = SelectObject(dc, label_font.into());
            SetTextColor(dc, color(raster::LABEL));
            let (frame_top, rate_peak, rate_caption) = raster::labels(overlay);
            let gap = (2.0 * scale).round() as i32;
            for (label, rect) in [(frame_top, layout.frames), (rate_peak, layout.bitrate)] {
                let units = text(&label);
                let mut size = SIZE::default();
                let _ = GetTextExtentPoint32W(dc, &units, &mut size);
                let _ = TextOutW(dc, rect.right - size.cx - gap, rect.top + gap / 2, &units);
            }
            let _ = TextOutW(
                dc,
                layout.bitrate.left + gap,
                layout.bitrate.top + gap / 2,
                &text(&rate_caption),
            );
            SelectObject(dc, previous_font);
            let _ = GdiFlush();
            raster::premultiply(pixels, raster::PANEL_ALPHA);
            let result = UpdateLayeredWindow(
                self.window,
                None,
                None,
                Some(&SIZE {
                    cx: width,
                    cy: height,
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
            SelectObject(dc, previous);
            let _ = DeleteObject(bitmap.into());
            result.context("update debug overlay")?;
            Ok(())
        }
    }
}
impl Drop for DebugPanel {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.window);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::{Level as DebugLevel, Line as DebugLine};
    #[test]
    fn panel_is_a_hidden_until_shown_click_through_child_that_skips_small_windows() {
        unsafe {
            let foreground = GetForegroundWindow();
            let parent = CreateWindowExW(
                WS_EX_NOACTIVATE,
                w!("Static"),
                w!("Debug overlay fixture"),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                800,
                600,
                None,
                None,
                None,
                None,
            )
            .unwrap();
            let result = (|| -> Result<()> {
                let panel = DebugPanel::new(parent)?;
                assert_eq!(GetParent(panel.window)?, parent);
                let style = GetWindowLongPtrW(panel.window, GWL_EXSTYLE) as u32;
                assert_ne!(style & WS_EX_TRANSPARENT.0, 0);
                assert_ne!(style & WS_EX_NOACTIVATE.0, 0);
                let overlay = DebugOverlay {
                    lines: vec![
                        DebugLine {
                            text: "FPS      59.8 shown · 60.0 decoded · 60.1 recv".into(),
                            level: DebugLevel::Normal,
                        };
                        11
                    ],
                    frames: vec![[16_667, 21_000]; 240],
                    frame_budget_us: 16_666,
                    bitrate_kbps: vec![12_000; 150],
                    sample_ms: 200,
                };
                let content = RECT {
                    left: 0,
                    top: 0,
                    right: 780,
                    bottom: 560,
                };
                assert!(panel.show(&overlay, content)?);
                assert_ne!(
                    GetWindowLongPtrW(panel.window, GWL_STYLE) as u32 & WS_VISIBLE.0,
                    0
                );
                let small = RECT {
                    left: 0,
                    top: 0,
                    right: 300,
                    bottom: 120,
                };
                assert!(!panel.show(&overlay, small)?);
                assert_eq!(
                    GetWindowLongPtrW(panel.window, GWL_STYLE) as u32 & WS_VISIBLE.0,
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
