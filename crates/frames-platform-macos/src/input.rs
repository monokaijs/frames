//! Applies v2 input events on a macOS host with Quartz events (`CGEvent`),
//! mapped onto the main display.
//!
//! Posting needs the Accessibility permission (`AXIsProcessTrusted`); without
//! it macOS drops the events, and [`MacInjector::trusted`] says so. QA runs
//! never create an injector, so tests cannot move the real cursor.

use frames_input::{Button, Event, Held, Pointer, PointerFilter, denormalize, keys};
use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

type CGEventRef = *mut c_void;

// CGEventTypes.h
const MOUSE_MOVED: u32 = 5;
const LEFT_DOWN: u32 = 1;
const LEFT_UP: u32 = 2;
const RIGHT_DOWN: u32 = 3;
const RIGHT_UP: u32 = 4;
const LEFT_DRAGGED: u32 = 6;
const RIGHT_DRAGGED: u32 = 7;
const OTHER_DOWN: u32 = 25;
const OTHER_UP: u32 = 26;
const OTHER_DRAGGED: u32 = 27;
const HID_EVENT_TAP: u32 = 0;
const SCROLL_UNIT_PIXEL: u32 = 0;
const SOURCE_HID_SYSTEM_STATE: i32 = 1;
/// kCGMouseEventButtonNumber, for back and forward.
const FIELD_BUTTON_NUMBER: u32 = 3;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGEventSourceCreate(state: i32) -> *mut c_void;
    fn CGEventCreateMouseEvent(
        source: *mut c_void,
        kind: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(source: *mut c_void, key: u16, down: bool) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        source: *mut c_void,
        units: u32,
        wheels: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: CGEventRef);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

/// A Quartz mouse event: its type, the CGMouseButton, and the button number.
fn button_event(button: Button, down: bool) -> (u32, u32, i64) {
    match (button, down) {
        (Button::Left, true) => (LEFT_DOWN, 0, 0),
        (Button::Left, false) => (LEFT_UP, 0, 0),
        (Button::Right, true) => (RIGHT_DOWN, 1, 1),
        (Button::Right, false) => (RIGHT_UP, 1, 1),
        (Button::Middle, true) => (OTHER_DOWN, 2, 2),
        (Button::Middle, false) => (OTHER_UP, 2, 2),
        (Button::Back, true) => (OTHER_DOWN, 2, 3),
        (Button::Back, false) => (OTHER_UP, 2, 3),
        (Button::Forward, true) => (OTHER_DOWN, 2, 4),
        (Button::Forward, false) => (OTHER_UP, 2, 4),
    }
}

/// While a button is held, motion is a drag of that button.
fn move_event(held: &[Button]) -> (u32, u32) {
    match held.last() {
        Some(Button::Left) => (LEFT_DRAGGED, 0),
        Some(Button::Right) => (RIGHT_DRAGGED, 1),
        Some(_) => (OTHER_DRAGGED, 2),
        None => (MOUSE_MOVED, 0),
    }
}

/// A normalized position on `display`, in global points (origin top left of
/// the main display, as Quartz events use).
fn point(display: CGRect, x: u16, y: u16) -> CGPoint {
    let to = |value: u16, origin: f64, extent: f64| {
        let pixels = denormalize(value, extent.max(1.0) as u32);
        origin + f64::from(pixels)
    };
    CGPoint {
        x: to(x, display.origin.x, display.size.width),
        y: to(y, display.origin.y, display.size.height),
    }
}

pub struct MacInjector {
    source: *mut c_void,
    filter: PointerFilter,
    held: Held,
    buttons: Vec<Button>,
    position: (u16, u16),
    pub applied: u64,
}

// The event source is only used from the owning thread.
unsafe impl Send for MacInjector {}

impl MacInjector {
    pub fn new() -> Self {
        Self {
            // SAFETY: returns a retained source or null (events then use none).
            source: unsafe { CGEventSourceCreate(SOURCE_HID_SYSTEM_STATE) },
            filter: PointerFilter::default(),
            held: Held::default(),
            buttons: Vec::new(),
            position: (0, 0),
            applied: 0,
        }
    }

    /// Whether macOS lets this process post input (Accessibility).
    pub fn trusted() -> bool {
        // SAFETY: no arguments.
        unsafe { AXIsProcessTrusted() }
    }

    fn display() -> CGRect {
        // SAFETY: plain queries.
        unsafe { CGDisplayBounds(CGMainDisplayID()) }
    }

    fn post(event: CGEventRef) {
        if event.is_null() {
            return;
        }
        // SAFETY: a freshly created event, released after posting.
        unsafe {
            CGEventPost(HID_EVENT_TAP, event);
            CFRelease(event);
        }
    }

    pub fn pointer(&mut self, pointer: &Pointer) -> bool {
        if !self.filter.accept(pointer) {
            return false;
        }
        self.position = (pointer.x, pointer.y);
        self.move_to(pointer.x, pointer.y);
        self.applied += 1;
        true
    }

    fn move_to(&self, x: u16, y: u16) {
        let (kind, button) = move_event(&self.buttons);
        let at = point(Self::display(), x, y);
        // SAFETY: creates an event; posted and released by `post`.
        Self::post(unsafe { CGEventCreateMouseEvent(self.source, kind, at, button) });
    }

    pub fn event(&mut self, event: Event) {
        if event == Event::ReleaseAll {
            let (x, y) = self.position;
            for release in self.held.release_all(x, y) {
                self.apply(release);
            }
            return;
        }
        self.held.track(&event);
        self.apply(event);
        self.applied += 1;
    }

    pub fn release_all(&mut self) {
        self.event(Event::ReleaseAll);
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Button { button, down, x, y } => {
                self.position = (x, y);
                self.buttons.retain(|&b| b != button);
                if down {
                    self.buttons.push(button);
                }
                let (kind, cg_button, number) = button_event(button, down);
                let at = point(Self::display(), x, y);
                // SAFETY: creates an event; posted and released by `post`.
                let created = unsafe { CGEventCreateMouseEvent(self.source, kind, at, cg_button) };
                if !created.is_null() && number > 2 {
                    // SAFETY: live event.
                    unsafe { CGEventSetIntegerValueField(created, FIELD_BUTTON_NUMBER, number) };
                }
                Self::post(created);
            }
            Event::Wheel { dx, dy } => {
                // 120 units per notch on the wire; macOS pixel deltas are
                // about 10 points per notch.
                let (x, y) = (i32::from(dx) / 12, i32::from(dy) / 12);
                // SAFETY: creates an event; posted and released by `post`.
                Self::post(unsafe {
                    CGEventCreateScrollWheelEvent2(self.source, SCROLL_UNIT_PIXEL, 2, y, x, 0)
                });
            }
            Event::Key { usage, down } => {
                if let Some(code) = keys::usage_to_mac(usage) {
                    // SAFETY: creates an event; posted and released by `post`.
                    Self::post(unsafe { CGEventCreateKeyboardEvent(self.source, code, down) });
                }
            }
            Event::ReleaseAll => {}
        }
    }
}

impl Default for MacInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MacInjector {
    fn drop(&mut self) {
        self.release_all();
        if !self.source.is_null() {
            // SAFETY: the source this injector created.
            unsafe { CFRelease(self.source) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_map_to_quartz_events_and_held_buttons_drag() {
        assert_eq!(button_event(Button::Left, true), (LEFT_DOWN, 0, 0));
        assert_eq!(button_event(Button::Right, false), (RIGHT_UP, 1, 1));
        assert_eq!(button_event(Button::Back, true), (OTHER_DOWN, 2, 3));
        assert_eq!(move_event(&[]), (MOUSE_MOVED, 0));
        assert_eq!(move_event(&[Button::Left]), (LEFT_DRAGGED, 0));
        assert_eq!(
            move_event(&[Button::Left, Button::Right]),
            (RIGHT_DRAGGED, 1)
        );
    }

    #[test]
    fn positions_land_on_the_display_edges() {
        let display = CGRect {
            origin: CGPoint { x: 0.0, y: 0.0 },
            size: CGSize {
                width: 1512.0,
                height: 982.0,
            },
        };
        assert_eq!(point(display, 0, 0), CGPoint { x: 0.0, y: 0.0 });
        assert_eq!(
            point(display, 65535, 65535),
            CGPoint {
                x: 1511.0,
                y: 981.0
            }
        );
        let second = CGRect {
            origin: CGPoint { x: -1920.0, y: 0.0 },
            ..display
        };
        assert_eq!(point(second, 0, 0).x, -1920.0);
    }
}
