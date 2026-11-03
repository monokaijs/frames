//! Platform-neutral layout and graph rasterization for the debug overlay.
//!
//! Windows draws the panel into a 32-bit DIB: this module fills the
//! background and graphs, GDI draws the text, then [`premultiply`] applies
//! the panel's translucency for `UpdateLayeredWindow`.
use crate::{DebugLevel, DebugOverlay};

pub const FRAME_CAPACITY: usize = 240;
pub const RATE_CAPACITY: usize = 150;
pub const PANEL_ALPHA: u8 = 205;
pub const BACKGROUND: [u8; 3] = [13, 15, 20];
const GRAPH_BACKGROUND: [u8; 3] = [255, 255, 255];
const GOOD: [u8; 3] = [89, 209, 122];
const WARN: [u8; 3] = [255, 199, 77];
const BAD: [u8; 3] = [255, 107, 97];
const LATENCY: [u8; 3] = [102, 191, 255];
const BITRATE: [u8; 3] = [178, 158, 255];
const REFERENCE: [u8; 3] = [120, 124, 132];

pub fn text_color(level: DebugLevel) -> [u8; 3] {
    match level {
        DebugLevel::Normal => [235, 236, 238],
        DebugLevel::Dim => [150, 153, 160],
        DebugLevel::Warn => WARN,
        DebugLevel::Bad => BAD,
    }
}
pub const LABEL: [u8; 3] = [160, 163, 170];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}
impl Rect {
    fn width(&self) -> i32 {
        self.right - self.left
    }
    fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub padding: i32,
    pub line_height: i32,
    pub font_height: i32,
    pub label_height: i32,
    pub frames: Rect,
    pub bitrate: Rect,
}
impl Layout {
    /// `text_width` is the widest line as measured by the platform font.
    pub fn new(lines: usize, text_width: i32, scale: f64, available_width: i32) -> Self {
        let unit = |value: f64| (value * scale).round() as i32;
        let padding = unit(8.0);
        let line_height = unit(15.0);
        let width =
            (text_width.max(unit(260.0)) + padding * 2).min(available_width.max(unit(120.0)));
        let text_bottom = padding + line_height * lines as i32;
        let frames = Rect {
            left: padding,
            top: text_bottom + unit(6.0),
            right: width - padding,
            bottom: text_bottom + unit(6.0) + unit(56.0),
        };
        let bitrate = Rect {
            left: padding,
            top: frames.bottom + unit(6.0),
            right: width - padding,
            bottom: frames.bottom + unit(6.0) + unit(34.0),
        };
        Self {
            width,
            height: bitrate.bottom + padding,
            padding,
            line_height,
            font_height: unit(12.0),
            label_height: unit(10.0),
            frames,
            bitrate,
        }
    }
}

/// Rounded scale for the frame-time graph, in µs.
pub fn frame_scale(overlay: &DebugOverlay) -> u32 {
    let peak = overlay
        .frames
        .iter()
        .map(|[interval, latency]| (*interval).max(*latency))
        .max()
        .unwrap_or(0)
        .max(overlay.frame_budget_us.saturating_mul(2));
    [33_333, 50_000, 100_000, 200_000, 500_000]
        .into_iter()
        .find(|step| *step >= peak)
        .unwrap_or(500_000)
}
pub fn bitrate_scale(overlay: &DebugOverlay) -> f64 {
    (overlay.bitrate_kbps.iter().copied().max().unwrap_or(0) as f64 * 1.15).max(1000.0)
}
/// `(top-right label of each graph, caption of the bitrate graph)`.
pub fn labels(overlay: &DebugOverlay) -> (String, String, String) {
    (
        format!("{} ms", frame_scale(overlay) / 1000),
        format!("{:.1} Mb/s", bitrate_scale(overlay) / 1000.0),
        format!(
            "bitrate · {:.0} s",
            RATE_CAPACITY as f64 * overlay.sample_ms.max(1) as f64 / 1000.0
        ),
    )
}

/// A top-down BGRA canvas.
pub struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: i32,
    height: i32,
}
impl<'a> Canvas<'a> {
    pub fn new(pixels: &'a mut [u8], width: i32, height: i32) -> Self {
        assert_eq!(pixels.len(), (width.max(0) * height.max(0) * 4) as usize);
        Self {
            pixels,
            width,
            height,
        }
    }
    fn blend(&mut self, x: i32, y: i32, color: [u8; 3], alpha: f32) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let index = ((y * self.width + x) * 4) as usize;
        let pixel = &mut self.pixels[index..index + 3];
        // BGRA storage, RGB colors.
        for (channel, value) in pixel.iter_mut().zip([color[2], color[1], color[0]]) {
            *channel = (*channel as f32 * (1.0 - alpha) + value as f32 * alpha).round() as u8;
        }
    }
    pub fn fill(&mut self, rect: Rect, color: [u8; 3], alpha: f32) {
        for y in rect.top.max(0)..rect.bottom.min(self.height) {
            for x in rect.left.max(0)..rect.right.min(self.width) {
                self.blend(x, y, color, alpha);
            }
        }
    }
    fn line(&mut self, from: (f64, f64), to: (f64, f64), color: [u8; 3], thickness: i32) {
        let steps = ((to.0 - from.0).abs().max((to.1 - from.1).abs()) * 2.0)
            .ceil()
            .max(1.0) as i32;
        let half = thickness / 2;
        for step in 0..=steps {
            let t = step as f64 / steps as f64;
            let x = (from.0 + (to.0 - from.0) * t).round() as i32;
            let y = (from.1 + (to.1 - from.1) * t).round() as i32;
            for dy in -half..(thickness - half) {
                for dx in -half..(thickness - half) {
                    self.blend(x + dx, y + dy, color, 1.0);
                }
            }
        }
    }
}

/// Fills the panel background and draws both graphs.
pub fn draw(canvas: &mut Canvas<'_>, layout: &Layout, overlay: &DebugOverlay, scale: f64) {
    let thickness = scale.round().max(1.0) as i32;
    canvas.fill(
        Rect {
            left: 0,
            top: 0,
            right: canvas.width,
            bottom: canvas.height,
        },
        BACKGROUND,
        1.0,
    );
    let graph = layout.frames;
    canvas.fill(graph, GRAPH_BACKGROUND, 0.06);
    let top = frame_scale(overlay) as f64;
    let budget = overlay.frame_budget_us.max(1000) as f64;
    let y_of = |micros: f64| graph.bottom as f64 - (micros.min(top) / top) * graph.height() as f64;
    let step = graph.width() as f64 / FRAME_CAPACITY as f64;
    let frames = &overlay.frames[overlay.frames.len().saturating_sub(FRAME_CAPACITY)..];
    let origin = graph.right as f64 - frames.len() as f64 * step;
    for (index, [interval, _]) in frames.iter().enumerate() {
        if *interval == 0 {
            continue;
        }
        let interval = *interval as f64;
        let color = if interval <= budget * 1.5 {
            GOOD
        } else if interval <= 50_000.0 {
            WARN
        } else {
            BAD
        };
        let left = origin + index as f64 * step;
        canvas.fill(
            Rect {
                left: left.floor() as i32,
                top: y_of(interval).round() as i32,
                right: ((left + step).floor() as i32).max(left.floor() as i32 + 1),
                bottom: graph.bottom,
            },
            color,
            0.9,
        );
    }
    let reference = y_of(budget).round() as i32;
    let dash = 3 * thickness;
    let mut x = graph.left;
    while x < graph.right {
        canvas.fill(
            Rect {
                left: x,
                top: reference,
                right: (x + dash).min(graph.right),
                bottom: reference + thickness,
            },
            REFERENCE,
            1.0,
        );
        x += dash * 2;
    }
    let mut previous: Option<(f64, f64)> = None;
    for (index, [_, latency]) in frames.iter().enumerate() {
        if *latency == 0 {
            previous = None;
            continue;
        }
        let point = (origin + (index as f64 + 0.5) * step, y_of(*latency as f64));
        if let Some(from) = previous {
            canvas.line(from, point, LATENCY, thickness);
        }
        previous = Some(point);
    }

    let graph = layout.bitrate;
    canvas.fill(graph, GRAPH_BACKGROUND, 0.06);
    let samples = &overlay.bitrate_kbps[overlay.bitrate_kbps.len().saturating_sub(RATE_CAPACITY)..];
    let peak = bitrate_scale(overlay);
    let step = graph.width() as f64 / (RATE_CAPACITY - 1) as f64;
    let origin = graph.right as f64 - samples.len().saturating_sub(1) as f64 * step;
    let y_of =
        |kbps: u32| graph.bottom as f64 - (kbps as f64 / peak).min(1.0) * graph.height() as f64;
    if samples.len() > 1 {
        // Area: one column at a time, interpolating between samples.
        for x in (origin.floor() as i32).max(graph.left)..graph.right {
            let position = ((x as f64 - origin) / step).clamp(0.0, (samples.len() - 1) as f64);
            let index = position.floor() as usize;
            let next = (index + 1).min(samples.len() - 1);
            let fraction = position - index as f64;
            let value = samples[index] as f64 * (1.0 - fraction) + samples[next] as f64 * fraction;
            let top = graph.bottom as f64 - (value / peak).min(1.0) * graph.height() as f64;
            canvas.fill(
                Rect {
                    left: x,
                    top: top.round() as i32,
                    right: x + 1,
                    bottom: graph.bottom,
                },
                BITRATE,
                0.35,
            );
        }
        for (index, pair) in samples.windows(2).enumerate() {
            canvas.line(
                (origin + index as f64 * step, y_of(pair[0])),
                (origin + (index + 1) as f64 * step, y_of(pair[1])),
                BITRATE,
                thickness,
            );
        }
    }
}

/// Applies the panel's uniform translucency to a fully drawn canvas.
pub fn premultiply(pixels: &mut [u8], alpha: u8) {
    for pixel in pixels.chunks_exact_mut(4) {
        for channel in &mut pixel[..3] {
            *channel = (*channel as u16 * alpha as u16 / 255) as u8;
        }
        pixel[3] = alpha;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DebugLine;

    fn overlay() -> DebugOverlay {
        let mut frames = vec![[16_667, 20_000]; 200];
        frames[100] = [80_000, 90_000];
        frames[150][1] = 0;
        DebugOverlay {
            lines: vec![
                DebugLine {
                    text: "line".into(),
                    level: DebugLevel::Normal,
                };
                10
            ],
            frames,
            frame_budget_us: 16_666,
            bitrate_kbps: (0..120).map(|i| 8_000 + (i % 7) * 500).collect(),
            sample_ms: 200,
        }
    }
    fn pixel(pixels: &[u8], width: i32, x: i32, y: i32) -> [u8; 4] {
        let index = ((y * width + x) * 4) as usize;
        pixels[index..index + 4].try_into().unwrap()
    }

    #[test]
    fn scales_round_to_readable_steps() {
        let mut overlay = overlay();
        assert_eq!(frame_scale(&overlay), 100_000);
        overlay.frames = vec![[16_000, 21_000]; 10];
        assert_eq!(frame_scale(&overlay), 33_333);
        assert_eq!(labels(&overlay).0, "33 ms");
        assert_eq!(labels(&overlay).2, "bitrate · 30 s");
    }

    #[test]
    fn layout_fits_text_and_graphs_and_respects_the_window() {
        let layout = Layout::new(10, 500, 1.5, 2000);
        assert_eq!(layout.width, 500 + 24);
        assert_eq!(Layout::new(10, 100, 1.5, 2000).width, 390 + 24);
        // 15 px lines round to 23 px at 150%.
        assert_eq!(layout.frames.top, 12 + 23 * 10 + 9);
        assert_eq!(layout.height, layout.bitrate.bottom + 12);
        assert!(Layout::new(10, 900, 1.0, 400).width <= 400);
    }

    #[test]
    fn graphs_draw_slow_frames_and_are_translucent() {
        let overlay = overlay();
        let layout = Layout::new(overlay.lines.len(), 280, 1.0, 1000);
        let mut pixels = vec![0; (layout.width * layout.height * 4) as usize];
        draw(
            &mut Canvas::new(&mut pixels, layout.width, layout.height),
            &layout,
            &overlay,
            1.0,
        );
        if let Ok(path) = std::env::var("FRAMES_DEBUG_RASTER_PNG") {
            let mut rgba = pixels.clone();
            for p in rgba.chunks_exact_mut(4) {
                p.swap(0, 2);
                p[3] = 255;
            }
            image::save_buffer(
                path,
                &rgba,
                layout.width as u32,
                layout.height as u32,
                image::ColorType::Rgba8,
            )
            .unwrap();
        }
        // The 80 ms frame is a red bar reaching near the top of a 100 ms scale.
        let graph = layout.frames;
        let step = graph.width() as f64 / FRAME_CAPACITY as f64;
        let x = (graph.right as f64 - (200.0 - 100.0) * step).floor() as i32;
        let [b, g, r, _] = pixel(&pixels, layout.width, x, graph.bottom - 2);
        assert!(r > 200 && g < 140 && b < 130, "{r} {g} {b}");
        let [b, g, r, _] = pixel(&pixels, layout.width, 1, 1);
        assert_eq!([r, g, b], BACKGROUND);
        premultiply(&mut pixels, PANEL_ALPHA);
        assert!(
            pixels
                .chunks_exact(4)
                .all(|p| p[3] == PANEL_ALPHA && p[..3].iter().all(|c| *c <= PANEL_ALPHA))
        );
    }
}
