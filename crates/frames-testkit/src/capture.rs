//! Synthetic capture source: one frame per `1/fps` of virtual time.
//!
//! Frames carry no real pixels: the CPU image holds the frame id (8 bytes),
//! which the synthetic encoder needs. Sizes are reported as configured.

use crate::clock::VirtualClock;
use bytes::Bytes;
use frames_media::api::{
    CaptureCaps, CaptureConfig, CaptureEvent, CaptureSource, CpuImage, CursorUpdate, MediaClock,
    MediaError, PixelFormat, Surface, VideoFrame,
};

pub struct SyntheticCapture {
    clock: VirtualClock,
    width: u32,
    height: u32,
    fps: u32,
    next_id: u64,
    next_due_us: u64,
}

impl SyntheticCapture {
    pub fn new(clock: &VirtualClock, width: u32, height: u32, fps: u32) -> Self {
        Self {
            clock: clock.clone(),
            width,
            height,
            fps: fps.max(1),
            next_id: 0,
            next_due_us: clock.now_us(),
        }
    }

    /// When the next frame is due, for virtual-time drivers.
    pub fn next_due_us(&self) -> u64 {
        self.next_due_us
    }

    fn interval_us(&self) -> u64 {
        1_000_000 / u64::from(self.fps)
    }
}

impl CaptureSource for SyntheticCapture {
    fn caps(&self) -> CaptureCaps {
        CaptureCaps {
            max_fps: 240,
            separate_cursor: true,
            dirty_rects: false,
        }
    }

    fn configure(&mut self, config: CaptureConfig) -> Result<(), MediaError> {
        if config.fps == 0 || config.fps > self.caps().max_fps {
            return Err(MediaError::Unsupported("frame rate"));
        }
        self.fps = config.fps;
        Ok(())
    }

    /// Returns a frame if one is due by now. Virtual time never blocks: when
    /// the next frame is later than `deadline_us` (or than now) the result
    /// is `Timeout` and the driver advances the clock to `next_due_us()`.
    fn next(&mut self, deadline_us: u64) -> Result<CaptureEvent, MediaError> {
        let now = self.clock.now_us();
        if self.next_due_us > now.min(deadline_us) {
            return Ok(CaptureEvent::Timeout);
        }
        let captured_us = self.next_due_us;
        let id = self.next_id;
        self.next_id += 1;
        // Frames missed while the driver was busy are dropped, as real
        // capture does: the next one is due one interval after now.
        self.next_due_us = (captured_us + self.interval_us()).max(now);
        Ok(CaptureEvent::Frame(VideoFrame {
            id,
            captured_us,
            surface: Surface::Cpu(CpuImage {
                width: self.width,
                height: self.height,
                stride: 0,
                format: PixelFormat::Bgra8,
                data: Bytes::copy_from_slice(&id.to_le_bytes()),
            }),
            dirty: None,
        }))
    }

    fn cursor(&mut self) -> Option<CursorUpdate> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_frames_on_the_virtual_frame_clock() {
        let clock = VirtualClock::new(0);
        let mut capture = SyntheticCapture::new(&clock, 1920, 1080, 60);
        let CaptureEvent::Frame(first) = capture.next(u64::MAX).unwrap() else {
            panic!("first frame is due immediately");
        };
        assert_eq!((first.id, first.captured_us), (0, 0));
        assert!(matches!(
            capture.next(u64::MAX).unwrap(),
            CaptureEvent::Timeout
        ));
        clock.advance_to(capture.next_due_us());
        let CaptureEvent::Frame(second) = capture.next(u64::MAX).unwrap() else {
            panic!("second frame");
        };
        assert_eq!((second.id, second.captured_us), (1, 16_666));
        assert_eq!(second.surface.size(), (1920, 1080));
        assert!(
            capture
                .configure(CaptureConfig {
                    fps: 0,
                    separate_cursor: true
                })
                .is_err()
        );
        capture
            .configure(CaptureConfig {
                fps: 120,
                separate_cursor: true,
            })
            .unwrap();
        clock.advance_to(capture.next_due_us());
        assert!(matches!(
            capture.next(u64::MAX).unwrap(),
            CaptureEvent::Frame(_)
        ));
        assert_eq!(
            capture.next_due_us(),
            2 * 16_666 + 8_333,
            "the 120 fps interval applies after the frame already due at 60 fps"
        );
    }
}
