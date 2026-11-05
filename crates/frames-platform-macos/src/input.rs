//! Applies v2 input events on a macOS host with Quartz events (`CGEvent`),
//! mapped onto the main display.
//!
//! Posting needs the Accessibility permission (`AXIsProcessTrusted`); without
//! it macOS drops the events, and [`MacInjector::trusted`] says so. QA runs
//! never create an injector, so tests cannot move the real cursor.

use frames_input::{
    Button, Event, Held, Motion, MotionReceiver, Pointer, PointerFilter, denormalize, keys,
};
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

/// The main display's frame in global points (x, y, width, height).
pub(crate) fn main_display_bounds() -> (f64, f64, f64, f64) {
    // SAFETY: plain queries.
    let rect = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    (
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    )
}

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
/// kCGMouseEventDeltaX / DeltaY: what games read as relative motion.
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;

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
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: usize, text: *const u16);
    fn CGEventCreateScrollWheelEvent2(
        source: *mut c_void,
        units: u32,
        wheels: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventCreate(source: *mut c_void) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
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

/// Where relative motion moves the pointer: by the delta, kept on the
/// display (games read the delta fields; the position only has to stay
/// on screen).
fn relative_target(display: CGRect, from: CGPoint, dx: i32, dy: i32) -> CGPoint {
    let clamp = |value: f64, origin: f64, extent: f64| {
        value.clamp(origin, origin + (extent - 1.0).max(0.0))
    };
    CGPoint {
        x: clamp(from.x + f64::from(dx), display.origin.x, display.size.width),
        y: clamp(
            from.y + f64::from(dy),
            display.origin.y,
            display.size.height,
        ),
    }
}

pub struct MacInjector {
    source: *mut c_void,
    filter: PointerFilter,
    /// Relative mouse (games): motion datagrams move the pointer by deltas
    /// and buttons press where the pointer is.
    relative: bool,
    motion: MotionReceiver,
    held: Held,
    buttons: Vec<Button>,
    position: (u16, u16),
    pub applied: u64,
    /// Where positions land, in global points: one shared window's frame,
    /// or the main display when unset.
    area: Option<CGRect>,
}

// The event source is only used from the owning thread.
unsafe impl Send for MacInjector {}

impl MacInjector {
    pub fn new() -> Self {
        Self {
            // SAFETY: returns a retained source or null (events then use none).
            source: unsafe { CGEventSourceCreate(SOURCE_HID_SYSTEM_STATE) },
            filter: PointerFilter::default(),
            relative: false,
            motion: MotionReceiver::default(),
            held: Held::default(),
            buttons: Vec::new(),
            position: (0, 0),
            applied: 0,
            area: None,
        }
    }

    /// Maps positions onto this rectangle (global points, top-left origin),
    /// a shared window's frame, instead of the main display.
    pub fn set_area(&mut self, x: f64, y: f64, width: f64, height: f64) {
        self.area = Some(CGRect {
            origin: CGPoint { x, y },
            size: CGSize { width, height },
        });
    }

    fn bounds(&self) -> CGRect {
        self.area.unwrap_or_else(Self::display)
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

    /// The pointer's current position, in global points.
    fn location() -> CGPoint {
        // SAFETY: an empty event reports the current location; released here.
        unsafe {
            let event = CGEventCreate(std::ptr::null_mut());
            if event.is_null() {
                return CGPoint::default();
            }
            let at = CGEventGetLocation(event);
            CFRelease(event);
            at
        }
    }

    /// Relative motion; ignored unless the viewer turned relative mode on.
    pub fn motion(&mut self, motion: &Motion) -> bool {
        if !self.relative {
            return false;
        }
        let Some((dx, dy)) = self.motion.accept(motion) else {
            return false;
        };
        if dx == 0 && dy == 0 {
            return true;
        }
        let (kind, button) = move_event(&self.buttons);
        let at = relative_target(self.bounds(), Self::location(), dx, dy);
        // SAFETY: creates an event; fields set on the live event, then
        // posted and released by `post`.
        unsafe {
            let event = CGEventCreateMouseEvent(self.source, kind, at, button);
            if !event.is_null() {
                CGEventSetIntegerValueField(event, FIELD_DELTA_X, i64::from(dx));
                CGEventSetIntegerValueField(event, FIELD_DELTA_Y, i64::from(dy));
            }
            Self::post(event);
        }
        self.applied += 1;
        true
    }

    fn move_to(&self, x: u16, y: u16) {
        let (kind, button) = move_event(&self.buttons);
        let at = point(self.bounds(), x, y);
        // SAFETY: creates an event; posted and released by `post`.
        Self::post(unsafe { CGEventCreateMouseEvent(self.source, kind, at, button) });
    }

    pub fn event(&mut self, event: Event) {
        if let Event::Mode { relative } = event {
            self.relative = relative;
            self.motion.restart();
            return;
        }
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
                self.buttons.retain(|&b| b != button);
                if down {
                    self.buttons.push(button);
                }
                let (kind, cg_button, number) = button_event(button, down);
                // In relative mode the viewer's position means nothing: press
                // where the pointer is.
                let at = if self.relative {
                    Self::location()
                } else {
                    self.position = (x, y);
                    point(self.bounds(), x, y)
                };
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
            // Characters typed on the viewer (its layout, dead keys, IME),
            // inserted as text as v1 did; at most 20 UTF-16 units per event.
            Event::Text { text } => {
                let units: Vec<u16> = text.encode_utf16().collect();
                for chunk in units.chunks(20) {
                    for down in [true, false] {
                        // SAFETY: creates an event; posted and released by
                        // `post`; `chunk` outlives the call.
                        unsafe {
                            let event = CGEventCreateKeyboardEvent(self.source, 0, down);
                            if !event.is_null() {
                                CGEventKeyboardSetUnicodeString(event, chunk.len(), chunk.as_ptr());
                            }
                            Self::post(event);
                        }
                    }
                }
            }
            // Window actions are the host session's, not the injector's.
            Event::ReleaseAll
            | Event::Mode { .. }
            | Event::CloseWindow
            | Event::MaxBitrate { .. }
            | Event::Menu { .. }
            | Event::Header { .. }
            | Event::RemoteCursor { .. }
            | Event::RefreshMenus
            | Event::ResizeWindow { .. }
            | Event::Visibility { .. } => {}
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

    #[test]
    fn relative_motion_stays_on_the_display() {
        let display = CGRect {
            origin: CGPoint { x: -100.0, y: 0.0 },
            size: CGSize {
                width: 200.0,
                height: 100.0,
            },
        };
        let from = CGPoint { x: 0.0, y: 50.0 };
        assert_eq!(
            relative_target(display, from, 5, -7),
            CGPoint { x: 5.0, y: 43.0 }
        );
        assert_eq!(
            relative_target(display, from, -1000, 1000),
            CGPoint { x: -100.0, y: 99.0 }
        );
    }
}
