//! Cursor pixels follow the same source-to-picture transform as the video.
//! Sizes are physical pixels: applying the viewer's DPI again would double
//! scale a cursor already rendered at the host's DPI.

pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub bgra: Vec<u8>,
}

impl CursorImage {
    pub fn display_size(&self, source: (u32, u32), picture: (u32, u32)) -> (u16, u16) {
        let scale = |extent: u16, source: u32, picture: u32| {
            (f64::from(extent) * f64::from(picture) / f64::from(source.max(1)))
                .round()
                .clamp(1.0, 256.0) as u16
        };
        (
            scale(self.width, source.0, picture.0),
            scale(self.height, source.1, picture.1),
        )
    }

    /// Bilinear filtering in premultiplied alpha avoids dark edges around a
    /// reduced pointer. Scale the hotspot with exactly the same transform.
    pub fn resized(&self, width: u16, height: u16) -> Self {
        let (width, height) = (width.clamp(1, 256), height.clamp(1, 256));
        let hotspot = |value: u16, before: u16, after: u16| {
            (f64::from(value) * f64::from(after) / f64::from(before.max(1)))
                .round()
                .clamp(0.0, f64::from(after - 1)) as u16
        };
        let bgra = if (width, height) == (self.width, self.height) {
            self.bgra.clone()
        } else {
            let sample = |x: usize, y: usize, channel: usize| {
                f64::from(self.bgra[(y * usize::from(self.width) + x) * 4 + channel])
            };
            let coordinate = |at: u16, before: u16, after: u16| {
                let value = ((f64::from(at) + 0.5) * f64::from(before) / f64::from(after) - 0.5)
                    .clamp(0.0, f64::from(before - 1));
                let first = value.floor() as usize;
                (
                    first,
                    (first + 1).min(usize::from(before - 1)),
                    value.fract(),
                )
            };
            let mut pixels = vec![0; usize::from(width) * usize::from(height) * 4];
            for y in 0..height {
                let (top, bottom, fy) = coordinate(y, self.height, height);
                for x in 0..width {
                    let (left, right, fx) = coordinate(x, self.width, width);
                    for channel in 0..4 {
                        let a = sample(left, top, channel) * (1.0 - fx)
                            + sample(right, top, channel) * fx;
                        let b = sample(left, bottom, channel) * (1.0 - fx)
                            + sample(right, bottom, channel) * fx;
                        pixels[(usize::from(y) * usize::from(width) + usize::from(x)) * 4
                            + channel] = (a * (1.0 - fy) + b * fy).round() as u8;
                    }
                }
            }
            pixels
        };
        Self {
            width,
            height,
            hotspot_x: hotspot(self.hotspot_x, self.width, width),
            hotspot_y: hotspot(self.hotspot_y, self.height, height),
            bgra,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor() -> CursorImage {
        CursorImage {
            width: 96,
            height: 96,
            hotspot_x: 12,
            hotspot_y: 18,
            bgra: vec![255; 96 * 96 * 4],
        }
    }

    #[test]
    fn high_dpi_host_cursor_shrinks_with_the_remote_desktop() {
        let image = cursor();
        let size = image.display_size((3840, 2160), (1280, 720));
        assert_eq!(size, (32, 32));
        let resized = image.resized(size.0, size.1);
        assert_eq!((resized.hotspot_x, resized.hotspot_y), (4, 6));
        assert_eq!(resized.bgra, vec![255; 32 * 32 * 4]);
        // Restoring the window must rescale the original, not the small copy.
        assert_eq!(image.display_size((3840, 2160), (3840, 2160)), (96, 96));
    }

    #[test]
    fn fit_uses_the_picture_size_and_fill_can_scale_axes_independently() {
        let image = cursor();
        assert_eq!(image.display_size((1920, 1080), (960, 540)), (48, 48));
        assert_eq!(image.display_size((1920, 1080), (960, 1080)), (48, 96));
        assert_eq!(image.display_size((1920, 1080), (3840, 2160)), (192, 192));
        assert_eq!(image.display_size((1920, 1080), (1, 1)), (1, 1));
    }

    #[test]
    fn scaling_preserves_alpha_and_keeps_the_hotspot_inside() {
        let image = CursorImage {
            width: 2,
            height: 2,
            hotspot_x: 1,
            hotspot_y: 1,
            bgra: vec![0, 0, 0, 0, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 0, 0],
        };
        let small = image.resized(1, 1);
        assert_eq!(small.bgra, [0, 0, 128, 128]);
        assert_eq!((small.hotspot_x, small.hotspot_y), (0, 0));
        assert_eq!(image.resized(2, 2).bgra, image.bgra);
    }
}
