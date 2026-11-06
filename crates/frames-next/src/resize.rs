//! A shared window that changes size, followed end to end so the viewer's
//! window keeps the picture's shape (no bars above and below it).
//!
//! The host waits for the size to settle (a drag resizes many times a
//! second), reconfigures its encoder at the new size with a keyframe, and
//! tells the viewer on a `RESIZE` stream (one JSON line per change). The
//! viewer recreates its decoder and resizes its window to the new shape at
//! its current scale.

use anyhow::Result;
use std::time::{Duration, Instant};

/// How long a new size must hold before the encoder follows it.
const SETTLE: Duration = Duration::from_millis(150);

/// What to do with a captured frame.
pub enum Step<F> {
    /// Encode it, first reconfiguring to this size when it is new.
    Encode(F, Option<(u32, u32)>),
    /// The size is still changing; the frame is held for later.
    Wait,
}

/// Host: tracks the encoder's size against captured frames.
pub struct Follower<F> {
    current: (u32, u32),
    /// The newest frame at a size not yet followed, and since when frames
    /// have had that size.
    held: Option<(F, (u32, u32), Instant)>,
}

impl<F> Follower<F> {
    pub fn new(size: (u32, u32)) -> Self {
        Self {
            current: size,
            held: None,
        }
    }

    /// `frame` of `size` just arrived (`None`: no new frame, but a held one
    /// may have settled by now).
    pub fn step(&mut self, frame: Option<(F, (u32, u32))>, now: Instant) -> Step<F> {
        match frame {
            Some((frame, size)) if size == self.current => {
                self.held = None;
                Step::Encode(frame, None)
            }
            Some((frame, size)) => {
                let since = match &self.held {
                    Some((_, held, since)) if *held == size => *since,
                    _ => now,
                };
                self.held = Some((frame, size, since));
                self.settle(now)
            }
            None => self.settle(now),
        }
    }

    fn settle(&mut self, now: Instant) -> Step<F> {
        match self.held.take() {
            Some((frame, size, since)) if now.duration_since(since) >= SETTLE => {
                self.current = size;
                Step::Encode(frame, Some(size))
            }
            held => {
                self.held = held;
                Step::Wait
            }
        }
    }
}

/// Host: tells the viewer the stream's new size, opening the stream the
/// first time.
pub fn announce(
    handle: &tokio::runtime::Handle,
    session: &frames_net::Session,
    stream: &mut Option<iroh::endpoint::SendStream>,
    (width, height): (u32, u32),
) -> Result<()> {
    if stream.is_none() {
        let (send, _) = handle.block_on(crate::streams::open(session, crate::streams::RESIZE))?;
        *stream = Some(send);
    }
    let mut line = serde_json::to_vec(&serde_json::json!({"width": width, "height": height}))?;
    line.push(b'\n');
    let send = stream.as_mut().expect("opened above");
    handle.block_on(send.write_all(&line))?;
    Ok(())
}

/// A window picture's outline where its corners are transparent (a macOS
/// sheet's or menu's rounded corners), measured from the captured alpha:
/// how many pixels of each row are transparent at the left and right, from
/// the top and from the bottom, with equal rows in one band. A picture whose
/// edges are all opaque has none (a plain rectangle).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Shape {
    /// Bands from the top row down: `(rows, left, right)`.
    pub top: Vec<(u32, u32, u32)>,
    /// Bands from the bottom row up.
    pub bottom: Vec<(u32, u32, u32)>,
}

impl Shape {
    /// Measures `bgra` (packed, `width` x `height`). Transparent is below
    /// half the picture's own opaque level (the most opaque alpha of its
    /// middle row), so a translucent body still counts as inside. `None`
    /// when nothing is transparent at the edges or the alpha says nothing
    /// (an opaque or unknown picture stays a rectangle).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn measure(bgra: &[u8], width: u32, height: u32) -> Option<Shape> {
        let (w, h) = (width as usize, height as usize);
        if w == 0 || h == 0 || bgra.len() != w * h * 4 {
            return None;
        }
        let alpha = |x: usize, y: usize| bgra[(y * w + x) * 4 + 3];
        let opaque = (0..w).map(|x| alpha(x, h / 2)).max().unwrap_or(0);
        if opaque < 32 {
            return None;
        }
        let threshold = opaque / 2;
        let row = |y: usize| -> (u32, u32) {
            match (0..w).position(|x| alpha(x, y) >= threshold) {
                None => (width, 0),
                Some(left) => {
                    let right = (0..w)
                        .rev()
                        .position(|x| alpha(x, y) >= threshold)
                        .unwrap_or(0);
                    (left as u32, right as u32)
                }
            }
        };
        let bands = |rows: &mut dyn Iterator<Item = usize>| {
            let mut bands: Vec<(u32, u32, u32)> = Vec::new();
            for y in rows {
                let (left, right) = row(y);
                if (left, right) == (0, 0) {
                    break;
                }
                match bands.last_mut() {
                    Some(band) if (band.1, band.2) == (left, right) => band.0 += 1,
                    _ => bands.push((1, left, right)),
                }
            }
            bands
        };
        let top = bands(&mut (0..h));
        let covered: u32 = top.iter().map(|b| b.0).sum();
        if covered as usize >= h {
            return None;
        }
        let bottom = bands(&mut (0..h).rev());
        let shape = Shape { top, bottom };
        (shape != Shape::default() && shape.fits(width, height)).then_some(shape)
    }

    /// Whether the shape fits a `width` x `height` picture (a viewer checks
    /// what a host sent before using it).
    pub fn fits(&self, width: u32, height: u32) -> bool {
        let rows = |bands: &[(u32, u32, u32)]| -> Option<u64> {
            bands.iter().try_fold(0u64, |sum, &(rows, left, right)| {
                (rows > 0 && u64::from(left) + u64::from(right) <= u64::from(width))
                    .then(|| sum + u64::from(rows))
            })
        };
        match (rows(&self.top), rows(&self.bottom)) {
            (Some(top), Some(bottom)) => {
                top + bottom <= u64::from(height)
                    && self.top.len() + self.bottom.len() <= height as usize
            }
            _ => false,
        }
    }

    /// The parts to cut away, as `(x0, y0, x1, y1)` in a client area of
    /// `client` pixels showing a `stream`-sized picture: a left and a right
    /// rectangle per band.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn cuts(&self, stream: (u32, u32), client: (u32, u32)) -> Vec<(i32, i32, i32, i32)> {
        let sx = f64::from(client.0) / f64::from(stream.0.max(1));
        let sy = f64::from(client.1) / f64::from(stream.1.max(1));
        let (cw, ch) = (client.0 as i32, client.1 as i32);
        let x = |v: u32| (f64::from(v) * sx).round() as i32;
        let y = |v: u32| (f64::from(v) * sy).round() as i32;
        let mut cuts = Vec::new();
        let mut band = |y0: i32, y1: i32, left: u32, right: u32| {
            if y1 > y0 {
                if left > 0 {
                    cuts.push((0, y0, x(left).min(cw), y1));
                }
                if right > 0 {
                    cuts.push(((cw - x(right)).max(0), y0, cw, y1));
                }
            }
        };
        let mut from = 0;
        for &(rows, left, right) in &self.top {
            band(y(from), y(from + rows), left, right);
            from += rows;
        }
        let mut from = 0;
        for &(rows, left, right) in &self.bottom {
            band(ch - y(from + rows), ch - y(from), left, right);
            from += rows;
        }
        cuts
    }
}

/// Host: sends the picture's shape, or `None` (a rectangle again), as its
/// own line: a size line would make the viewer reopen its decoder, and
/// viewers without shapes skip lines without a size.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn announce_shape(
    handle: &tokio::runtime::Handle,
    session: &frames_net::Session,
    stream: &mut Option<iroh::endpoint::SendStream>,
    shape: Option<&Shape>,
) -> Result<()> {
    if stream.is_none() {
        let (send, _) = handle.block_on(crate::streams::open(session, crate::streams::RESIZE))?;
        *stream = Some(send);
    }
    let mut line = serde_json::to_vec(&serde_json::json!({ "shape": shape }))?;
    line.push(b'\n');
    let send = stream.as_mut().expect("opened above");
    handle.block_on(send.write_all(&line))?;
    Ok(())
}

/// Viewer: the host's size changes, from its `RESIZE` stream, and its
/// picture's shape when it sends one.
pub async fn receive(
    recv: iroh::endpoint::RecvStream,
    sizes: std::sync::mpsc::Sender<(u32, u32)>,
    shapes: std::sync::mpsc::Sender<Option<Shape>>,
) {
    let mut lines = tokio::io::BufReader::new(recv);
    let mut line = String::new();
    loop {
        line.clear();
        match tokio::io::AsyncBufReadExt::read_line(&mut lines, &mut line).await {
            Ok(n) if n > 0 => {}
            _ => return,
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        if let Some(shape) = value.get("shape") {
            // Checked against the picture by the window before it is used.
            if let Ok(shape) = serde_json::from_value::<Option<Shape>>(shape.clone()) {
                let _ = shapes.send(shape);
            }
            continue;
        }
        let (Some(width), Some(height)) = (value["width"].as_u64(), value["height"].as_u64())
        else {
            continue;
        };
        if sizes.send((width as u32, height as u32)).is_err() {
            return;
        }
    }
}

/// Viewer (Windows; FramesMac does this itself): the window's new client size for a stream that changed from
/// `old` to `new`, keeping its current scale, within `limit` (the screen).
#[cfg(any(windows, test))]
pub fn window_size(
    client: (u32, u32),
    old: (u32, u32),
    new: (u32, u32),
    limit: (u32, u32),
) -> (u32, u32) {
    let scale = f64::from(client.0.max(1)) / f64::from(old.0.max(1));
    let fit = (f64::from(limit.0) / f64::from(new.0.max(1)))
        .min(f64::from(limit.1) / f64::from(new.1.max(1)));
    let scale = scale.min(fit);
    (
        (f64::from(new.0) * scale).round().max(1.0) as u32,
        (f64::from(new.1) * scale).round().max(1.0) as u32,
    )
}

/// Viewer: the last picture shown, kept to draw again when the window's
/// drawing area changes (a menu row added, a move to another screen). A
/// still picture (a sheet, a dialog) sends no new frame that would redraw
/// it, so without this it stays blank or stretched.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Redraw<F> {
    last: Option<(F, (u32, u32))>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl<F: Clone> Redraw<F> {
    pub fn new() -> Self {
        Self { last: None }
    }

    /// `frame` was just shown in a drawing area of `size`.
    pub fn shown(&mut self, frame: F, size: (u32, u32)) {
        self.last = Some((frame, size));
    }

    /// Show the last picture again on the next check (its window was just
    /// shown or moved, which a still picture would not otherwise redraw).
    pub fn invalidate(&mut self) {
        if let Some((_, size)) = &mut self.last {
            *size = (0, 0);
        }
    }

    /// The picture to show again: the drawing area is `size` now and no
    /// newer frame came.
    pub fn due(&self, size: (u32, u32)) -> Option<F> {
        match &self.last {
            Some((frame, shown_at)) if *shown_at != size && size.0 > 0 && size.1 > 0 => {
                Some(frame.clone())
            }
            _ => None,
        }
    }
}

/// What a window's picture (`stream`, in pixels) leaves out at the top of
/// its catalog frame (`frame`: x, y, width, height), in catalog units, and
/// the picture's pixels per catalog unit (from the uncropped width; a
/// Retina host's are 2). A frame the catalog already lists cropped (a
/// sheet's content), or an unknown one, crops nothing.
#[cfg_attr(not(windows), allow(dead_code))]
fn cropped_top(frame: (f64, f64, f64, f64), stream: (u32, u32)) -> (f64, f64) {
    if frame.2 <= 0.0 || stream.0 == 0 {
        return (0.0, 1.0);
    }
    let density = f64::from(stream.0) / frame.2;
    ((frame.3 - f64::from(stream.1) / density).max(0.0), density)
}

/// Viewer: where a host popup or owned dialog goes on this screen.
/// `window` and `parent` are their frames (x, y, width, height) in the
/// host's catalog units, and `stream` and `parent_stream` their pictures
/// in pixels. A picture can leave the title bar out (Mac hosts do by
/// default) and have more pixels than catalog units (Retina hosts), so the
/// offset is taken between the two pictures' tops. `view` is where the
/// parent's viewer shows its picture: screen x, y and screen pixels per
/// picture pixel. Returns the window's client area's top left and size.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn popup_place(
    window: (f64, f64, f64, f64),
    parent: (f64, f64, f64, f64),
    parent_stream: (u32, u32),
    view: (i32, i32, f64),
    stream: (u32, u32),
) -> ((i32, i32), (u32, u32)) {
    let (x, y, scale) = view;
    let (parent_crop, density) = cropped_top(parent, parent_stream);
    let (own_crop, _) = cropped_top(window, stream);
    (
        (
            x + ((window.0 - parent.0) * density * scale).round() as i32,
            y + ((window.1 + own_crop - parent.1 - parent_crop) * density * scale).round() as i32,
        ),
        (
            ((f64::from(stream.0) * scale).round() as u32).max(1),
            ((f64::from(stream.1) * scale).round() as u32).max(1),
        ),
    )
}

/// Viewer: a popup follows its parent's picture. It waits (hidden) until
/// the parent's viewer is placed, then moves with it when the parent's
/// window moves or rescales; nothing is done while nothing changed.
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub struct Follow {
    placed: Option<((i32, i32), (u32, u32))>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl Follow {
    /// The placement to apply now, given where the parent puts it (`None`
    /// while the parent's viewer is not there yet).
    pub fn update(
        &mut self,
        now: Option<((i32, i32), (u32, u32))>,
    ) -> Option<((i32, i32), (u32, u32))> {
        match now {
            Some(place) if self.placed != Some(place) => {
                self.placed = Some(place);
                Some(place)
            }
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn placed(&self) -> bool {
        self.placed.is_some()
    }
}

#[cfg(test)]
mod tests {

    /// A picture whose corners are transparent, with a translucent body.
    fn picture(width: u32, height: u32, radius: u32, body: u8) -> Vec<u8> {
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let dx = f64::from(radius.saturating_sub(x.min(width - 1 - x)));
                let dy = f64::from(radius.saturating_sub(y.min(height - 1 - y)));
                let inside =
                    dx == 0.0 || dy == 0.0 || dx * dx + dy * dy <= f64::from(radius * radius);
                bgra[((y * width + x) * 4 + 3) as usize] = if inside { body } else { 8 };
            }
        }
        bgra
    }

    #[test]
    fn a_rounded_picture_gives_its_corner_bands_and_an_opaque_one_none() {
        // A 26-pixel radius on a 260x154 sheet whose body is translucent.
        let shape = Shape::measure(&picture(260, 154, 26, 200), 260, 154).unwrap();
        let rows: u32 = shape.top.iter().map(|b| b.0).sum();
        assert!(rows > 0 && rows <= 26, "{rows}");
        assert!(
            shape.top[0].1 > 0 && shape.top[0].1 == shape.top[0].2,
            "{:?}",
            shape.top[0]
        );
        assert_eq!(shape.top, shape.bottom, "symmetric picture");
        assert!(
            shape.top.windows(2).all(|w| w[0].1 >= w[1].1),
            "insets shrink downwards"
        );
        assert!(shape.fits(260, 154));
        // A large radius is measured whole (no row cap).
        let big = Shape::measure(&picture(400, 400, 120, 255), 400, 400).unwrap();
        assert!(big.top.iter().map(|b| b.0).sum::<u32>() > 100);
        // Opaque, all transparent, or not matching its size: a rectangle.
        assert_eq!(Shape::measure(&picture(100, 60, 0, 255), 100, 60), None);
        assert_eq!(Shape::measure(&vec![0u8; 100 * 60 * 4], 100, 60), None);
        assert_eq!(Shape::measure(&[0u8; 12], 100, 60), None);
    }

    /// Each corner as it is: 8 top left, 64 top right, a square bottom.
    #[test]
    fn an_asymmetric_outline_is_kept_as_measured() {
        let (width, height) = (300u32, 200u32);
        let mut bgra = vec![255u8; (width * height * 4) as usize];
        let corner = |bgra: &mut Vec<u8>, radius: u32, right: bool| {
            for y in 0..radius {
                for x in 0..radius {
                    let (dx, dy) = (f64::from(radius - x), f64::from(radius - y));
                    if dx * dx + dy * dy > f64::from(radius * radius) {
                        let px = if right { width - 1 - x } else { x };
                        bgra[((y * width + px) * 4 + 3) as usize] = 8;
                    }
                }
            }
        };
        corner(&mut bgra, 8, false);
        corner(&mut bgra, 64, true);
        let shape = Shape::measure(&bgra, width, height).unwrap();
        assert!(shape.bottom.is_empty(), "square bottom: {:?}", shape.bottom);
        let rows: u32 = shape.top.iter().map(|b| b.0).sum();
        assert!(
            (50..=64).contains(&rows),
            "the larger corner's rows: {rows}"
        );
        let first = shape.top[0];
        assert!(
            first.1 <= 8 && first.2 > 30,
            "left small, right large: {first:?}"
        );
        // Below the small corner the left is straight while the right curves on.
        assert!(shape.top.iter().skip_while(|b| b.1 > 0).any(|b| b.2 > 0));
    }

    #[test]
    fn a_shape_from_a_host_is_checked_and_scaled_to_the_window() {
        let shape = Shape {
            top: vec![(2, 6, 6), (4, 2, 3)],
            bottom: vec![(3, 4, 4)],
        };
        assert!(shape.fits(100, 50));
        assert!(!shape.fits(10, 50), "insets wider than the picture");
        assert!(!shape.fits(100, 8), "more rows than the picture");
        assert!(
            !Shape {
                top: vec![(0, 1, 1)],
                bottom: vec![]
            }
            .fits(100, 50),
            "empty band"
        );
        // Shown at twice its size.
        assert_eq!(
            shape.cuts((100, 50), (200, 100)),
            vec![
                (0, 0, 12, 4),
                (188, 0, 200, 4),
                (0, 4, 4, 12),
                (194, 4, 200, 12),
                (0, 94, 8, 100),
                (192, 94, 200, 100),
            ]
        );
        // On the wire it is optional: null clears it.
        let line = serde_json::json!({ "shape": shape });
        let back: Option<Shape> = serde_json::from_value(line["shape"].clone()).unwrap();
        assert_eq!(back.as_ref(), Some(&shape));
        let cleared: Option<Shape> = serde_json::from_value(serde_json::Value::Null).unwrap();
        assert_eq!(cleared, None);
    }

    #[test]
    fn a_popup_is_placed_below_its_parents_cropped_title_bar() {
        // A Mac window 440x352 points whose picture leaves out the 32-point
        // title bar (440x320), and a sheet whose top is 22 points below the
        // content's top.
        let parent = (100.0, 100.0, 440.0, 352.0);
        let sheet = (190.0, 154.0, 260.0, 154.0);
        assert_eq!(
            popup_place(sheet, parent, (440, 320), (400, 300, 1.0), (260, 154)),
            ((490, 322), (260, 154)),
            "from the picture's top, not the window's"
        );
        // A Retina host: twice the pixels, shown at half scale.
        assert_eq!(
            popup_place(sheet, parent, (880, 640), (400, 300, 0.5), (520, 308)),
            ((490, 322), (260, 154))
        );
        // The viewer chose to show the title bar: nothing is cropped.
        assert_eq!(
            popup_place(sheet, parent, (440, 352), (400, 300, 1.0), (260, 154)),
            ((490, 354), (260, 154))
        );
    }

    #[test]
    fn an_owned_dialogs_own_title_bar_crop_counts_too() {
        // A dialog frame 300x232 points whose picture also leaves its
        // 32-point title bar out (300x200): its picture's top is 72 points
        // below the parent's frame top, 40 below the parent picture's.
        let parent = (100.0, 100.0, 440.0, 352.0);
        let dialog = (150.0, 140.0, 300.0, 232.0);
        assert_eq!(
            popup_place(dialog, parent, (440, 320), (400, 300, 1.0), (300, 200)),
            ((450, 340), (300, 200))
        );
        // Retina at half scale: the same place on screen.
        assert_eq!(
            popup_place(dialog, parent, (880, 640), (400, 300, 0.5), (600, 400)),
            ((450, 340), (300, 200))
        );
        // Unknown frame size: no crop assumed for it.
        assert_eq!(
            popup_place(
                (150.0, 140.0, 0.0, 0.0),
                parent,
                (440, 320),
                (400, 300, 1.0),
                (300, 200)
            ),
            ((450, 308), (300, 200))
        );
    }

    #[test]
    fn a_popup_waits_for_its_parent_then_follows_it() {
        // A sheet 90 points right and 22 down of its window, whose picture
        // (uncropped) shows at (400, 300) at half scale.
        let place = |view| {
            popup_place(
                (190.0, 122.0, 260.0, 154.0),
                (100.0, 100.0, 440.0, 320.0),
                (440, 320),
                view,
                (260, 154),
            )
        };
        assert_eq!(place((400, 300, 0.5)), ((445, 311), (130, 77)));
        let mut follow = Follow::default();
        assert_eq!(follow.update(None), None, "parent not there yet");
        assert!(!follow.placed());
        assert_eq!(
            follow.update(Some(place((400, 300, 0.5)))),
            Some(((445, 311), (130, 77)))
        );
        assert_eq!(
            follow.update(Some(place((400, 300, 0.5)))),
            None,
            "unchanged"
        );
        // The parent's window moved, then was shown at full scale.
        assert_eq!(
            follow.update(Some(place((500, 320, 0.5)))),
            Some(((545, 331), (130, 77)))
        );
        assert_eq!(
            follow.update(Some(place((500, 320, 1.0)))),
            Some(((590, 342), (260, 154)))
        );
        // The parent's viewer went away for a moment: stay where it was.
        assert_eq!(follow.update(None), None);
        assert!(follow.placed());
    }

    #[test]
    fn a_still_picture_is_drawn_again_when_its_window_changes() {
        let mut redraw = Redraw::new();
        assert_eq!(redraw.due((260, 154)), None, "nothing shown yet");
        redraw.shown("sheet", (260, 154));
        assert_eq!(redraw.due((260, 154)), None, "same area: nothing to do");
        // A menu row took 20 pixels: draw it again in the new area, once.
        assert_eq!(redraw.due((260, 134)), Some("sheet"));
        redraw.shown("sheet", (260, 134));
        assert_eq!(redraw.due((260, 134)), None);
        assert_eq!(redraw.due((0, 0)), None, "minimized");
        // Shown again after it was placed: once more at the same size.
        redraw.invalidate();
        assert_eq!(redraw.due((260, 134)), Some("sheet"));
    }
    use super::*;

    #[test]
    fn a_drag_is_followed_once_it_settles_and_frames_wait_meanwhile() {
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let mut follower = Follower::new((800, 600));
        assert!(matches!(
            follower.step(Some((1, (800, 600))), ms(0)),
            Step::Encode(1, None)
        ));
        // Dragging: sizes change faster than they settle.
        assert!(matches!(
            follower.step(Some((2, (810, 600))), ms(10)),
            Step::Wait
        ));
        assert!(matches!(
            follower.step(Some((3, (830, 610))), ms(100)),
            Step::Wait
        ));
        assert!(matches!(
            follower.step(Some((4, (830, 610))), ms(200)),
            Step::Wait
        ));
        // Still since 100 ms: the held frame goes out at the new size even
        // without another frame (a still window sends none).
        assert!(matches!(
            follower.step(None, ms(260)),
            Step::Encode(4, Some((830, 610)))
        ));
        assert!(matches!(
            follower.step(Some((5, (830, 610))), ms(270)),
            Step::Encode(5, None)
        ));
        // Back to the old size before settling: nothing to reconfigure.
        assert!(matches!(
            follower.step(Some((6, (900, 610))), ms(300)),
            Step::Wait
        ));
        assert!(matches!(
            follower.step(Some((7, (830, 610))), ms(320)),
            Step::Encode(7, None)
        ));
        assert!(matches!(follower.step(None, ms(600)), Step::Wait));
    }

    #[test]
    fn the_viewer_window_keeps_its_scale_within_the_screen() {
        // Half scale, the host window grows taller.
        assert_eq!(
            window_size((400, 300), (800, 600), (800, 800), (1920, 1080)),
            (400, 400)
        );
        // Too tall for the screen at that scale: fitted.
        assert_eq!(
            window_size((800, 600), (800, 600), (800, 2400), (1920, 1080)),
            (360, 1080)
        );
    }
}
