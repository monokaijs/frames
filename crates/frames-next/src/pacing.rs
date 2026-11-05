//! Host: how often to encode for the viewer's window (`Event::Visibility`),
//! as v1: nothing while it is minimized or covered, 24 frames a second
//! while another window is in front, every frame while it is.
//!
//! A frame that arrives too soon is held and sent once its turn comes, so
//! a picture that stops changing still ends on its newest frame.

/// Frames a second while the viewer's window is not in front (v1).
pub const UNFOCUSED_FPS: u64 = 24;

pub struct Visibility {
    shown: bool,
    focused: bool,
    keyframe_due: bool,
}

impl Default for Visibility {
    fn default() -> Self {
        Self {
            shown: true,
            focused: true,
            keyframe_due: false,
        }
    }
}

impl Visibility {
    /// The viewer's window changed; true when that changes the pacing.
    pub fn set(&mut self, shown: bool, focused: bool) -> bool {
        if shown && !self.shown {
            // The picture went unseen for a while: start it afresh.
            self.keyframe_due = true;
        }
        let changed = (shown, focused) != (self.shown, self.focused);
        (self.shown, self.focused) = (shown, focused);
        changed
    }

    /// Whether to force a keyframe now (the window was shown again).
    pub fn take_keyframe(&mut self) -> bool {
        std::mem::take(&mut self.keyframe_due)
    }

    pub fn describe(&self) -> &'static str {
        match (self.shown, self.focused) {
            (false, _) => "paused (the viewer's window is hidden)",
            (true, false) => "24 fps (the viewer's window is not in front)",
            (true, true) => "every frame",
        }
    }

    /// The frame to encode now, if any: `frame` (or an earlier one held in
    /// `held`) when its turn has come; otherwise the newest is held.
    pub fn admit<F>(
        &mut self,
        frame: Option<F>,
        held: &mut Option<F>,
        now_us: u64,
        last_encode_us: u64,
    ) -> Option<F> {
        if frame.is_some() {
            *held = frame;
        }
        if !self.shown {
            return None;
        }
        let interval = if self.focused {
            0
        } else {
            1_000_000 / UNFOCUSED_FPS
        };
        if now_us.saturating_sub(last_encode_us) >= interval {
            held.take()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_windows_get_nothing_and_unfocused_ones_fewer_frames() {
        let mut pacing = Visibility::default();
        let mut held = None;
        assert_eq!(pacing.admit(Some(1), &mut held, 1_000, 990), Some(1));
        assert!(pacing.set(true, false));
        // 10 ms after the last: too soon at 24 fps; held, then sent.
        assert_eq!(pacing.admit(Some(2), &mut held, 10_000, 0), None);
        assert_eq!(pacing.admit(Some(3), &mut held, 20_000, 0), None);
        assert_eq!(pacing.admit(None, &mut held, 42_000, 0), Some(3));
        assert!(pacing.set(false, false));
        assert_eq!(pacing.admit(Some(4), &mut held, 1_000_000, 0), None);
        assert!(!pacing.take_keyframe());
        pacing.set(true, true);
        assert!(pacing.take_keyframe());
        assert_eq!(pacing.admit(None, &mut held, 1_000_001, 1_000_000), Some(4));
    }
}
