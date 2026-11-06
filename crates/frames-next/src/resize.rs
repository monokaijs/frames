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

/// Viewer: the host's size changes, from its `RESIZE` stream.
pub async fn receive(recv: iroh::endpoint::RecvStream, sizes: std::sync::mpsc::Sender<(u32, u32)>) {
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
