//! Windows host, one shared window with isolated input (`--isolated`): the
//! viewer's input goes into that window's process only
//! (`frames_platform_windows::shared_input`), never to the host's own
//! mouse and keyboard, so the person at the host keeps using them.
//!
//! The adapter pauses input when its lease runs out (heartbeats every
//! 100 ms keep it), the game is minimized, or input overflows; the
//! viewer's next press or key resumes it.

use anyhow::Result;
use frames_input::cursor::CursorMessage;
use frames_input::{Button, Event, Motion, MotionReceiver, Pointer, PointerFilter, keys};
use frames_platform_windows::shared_input::SharedInput;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::HWND;

const HEARTBEAT: Duration = Duration::from_millis(100);

pub struct Isolated {
    input: SharedInput,
    pointer: PointerFilter,
    motion: MotionReceiver,
    active: bool,
    next_beat: Instant,
    image_generation: Option<u64>,
    cursor_visible: Option<bool>,
    /// Pointer moves and events given to the adapter, and what was last
    /// reported (once a second, for diagnostics).
    moves: u64,
    events: u64,
    reported: (u64, u64),
    next_report: Instant,
}

impl Isolated {
    /// Attaches to `hwnd`'s process; `config` holds the extracted adapter.
    pub fn attach(hwnd: HWND, config: &std::path::Path) -> Result<Self> {
        let input = SharedInput::attach(hwnd, config)?;
        Ok(Self {
            input,
            pointer: PointerFilter::default(),
            motion: MotionReceiver::default(),
            active: false,
            next_beat: Instant::now(),
            image_generation: None,
            cursor_visible: None,
            moves: 0,
            events: 0,
            reported: (0, 0),
            next_report: Instant::now() + Duration::from_secs(1),
        })
    }

    /// Resumes a paused adapter before input that should take effect.
    fn resume_if_paused(&mut self) -> Result<()> {
        if !self.active {
            self.input.resume()?;
            self.active = true;
        }
        Ok(())
    }

    pub fn pointer(&mut self, pointer: &Pointer) -> Result<()> {
        if self.pointer.accept(pointer) && self.active {
            self.input.move_to(pointer.x, pointer.y)?;
            self.moves += 1;
        }
        Ok(())
    }

    pub fn motion(&mut self, motion: &Motion) -> Result<()> {
        if let Some((dx, dy)) = self.motion.accept(motion)
            && self.active
        {
            self.input.motion(dx, dy)?;
        }
        Ok(())
    }

    pub fn event(&mut self, event: Event) -> Result<()> {
        self.events += 1;
        match event {
            Event::Button { button, down, x, y } => {
                if down {
                    self.resume_if_paused()?;
                }
                let code = match button {
                    Button::Left => 1,
                    Button::Right => 2,
                    Button::Middle => 3,
                    Button::Back => 4,
                    Button::Forward => 5,
                };
                self.input.button(code, down, x, y)
            }
            Event::Wheel { dx, dy } => self.input.scroll(i32::from(dx), i32::from(dy)),
            Event::Key { usage, down } => {
                if down {
                    self.resume_if_paused()?;
                }
                match keys::usage_to_scan(usage) {
                    Some(scan) => self.input.key(scan, down),
                    None => Ok(()),
                }
            }
            Event::ReleaseAll => {
                self.active = false;
                self.input.release()
            }
            Event::Mode { relative } => {
                self.input.set_relative(relative);
                self.motion.restart();
                Ok(())
            }
            Event::CloseWindow
            | Event::MaxBitrate { .. }
            | Event::Menu { .. }
            | Event::Header { .. }
            | Event::RemoteCursor { .. }
            | Event::RefreshMenus
            | Event::ResizeWindow { .. }
            // One-window sessions use physical keys (as v1).
            | Event::Text { .. }
            | Event::Visibility { .. } => Ok(()),
        }
    }

    /// Keeps the lease, follows the adapter's state, and hands the game's
    /// cursor to `send` when it changes.
    pub fn tick(&mut self, mut send: impl FnMut(CursorMessage)) -> Result<()> {
        if Instant::now() < self.next_beat {
            return Ok(());
        }
        self.next_beat = Instant::now() + HEARTBEAT;
        if Instant::now() >= self.next_report {
            self.next_report = Instant::now() + Duration::from_secs(1);
            if (self.moves, self.events) != self.reported {
                println!(
                    "isolated input: {} moves, {} events given to the window ({})",
                    self.moves,
                    self.events,
                    if self.active { "active" } else { "paused" }
                );
                self.reported = (self.moves, self.events);
            }
        }
        self.input.heartbeat()?;
        let status = self.input.status()?;
        if self.active && !status.active {
            println!(
                "isolated input paused{}; the viewer's next click or key resumes it",
                if status.error != 0 {
                    format!(" (Windows error {})", status.error)
                } else {
                    String::new()
                }
            );
        }
        self.active = status.active;
        if let Some(cursor) = status.cursor {
            if self.image_generation != Some(status.image_generation) {
                self.image_generation = Some(status.image_generation);
                send(CursorMessage::Shape {
                    width: cursor.width as u16,
                    height: cursor.height as u16,
                    hotspot_x: cursor.hot_x as u16,
                    hotspot_y: cursor.hot_y as u16,
                    // The viewer takes straight-alpha BGRA.
                    bgra: cursor
                        .rgba
                        .chunks_exact(4)
                        .flat_map(|p| [p[2], p[1], p[0], p[3]])
                        .collect(),
                });
            }
            if self.cursor_visible != Some(cursor.visible) {
                self.cursor_visible = Some(cursor.visible);
                send(CursorMessage::Visible(cursor.visible));
            }
        }
        Ok(())
    }
}
