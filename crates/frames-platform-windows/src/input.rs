//! Applies v2 input events on the host.
//!
//! [`InputTarget::Desktop`] injects with `SendInput`, mapped onto the captured
//! monitor. [`InputTarget::Window`] posts window messages to one window, which
//! is how QA runs exercise the whole lane without moving the real cursor or
//! typing into the user's applications.

use frames_input::{Button, Event, Held, Pointer, PointerFilter, denormalize, keys};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

// Message parameter values (WinUser.h).
const MK_LEFT: usize = 0x0001;
const MK_RIGHT: usize = 0x0002;
const MK_MIDDLE: usize = 0x0010;
const XBUTTON_BACK: usize = 0x0001;
const XBUTTON_FORWARD: usize = 0x0002;

#[derive(Clone, Copy, Debug)]
pub enum InputTarget {
    /// The monitor at this rectangle in virtual-screen pixels.
    Desktop(RECT),
    /// A window's client area, by message (no global side effects).
    Window(HWND),
}

pub struct InputInjector {
    target: InputTarget,
    filter: PointerFilter,
    held: Held,
    /// Last applied normalized position.
    position: (u16, u16),
    /// Pointer datagrams and events applied, for statistics.
    pub applied: u64,
}

// The HWND is only used with thread-safe message posting.
unsafe impl Send for InputInjector {}

impl InputInjector {
    pub fn new(target: InputTarget) -> Self {
        Self {
            target,
            filter: PointerFilter::default(),
            held: Held::default(),
            position: (0, 0),
            applied: 0,
        }
    }

    /// Applies a pointer datagram unless a newer one was already applied.
    pub fn pointer(&mut self, pointer: &Pointer) -> bool {
        if !self.filter.accept(pointer) {
            return false;
        }
        self.position = (pointer.x, pointer.y);
        self.move_to(pointer.x, pointer.y);
        self.applied += 1;
        true
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

    /// Releases everything held (the viewer left).
    pub fn release_all(&mut self) {
        self.event(Event::ReleaseAll);
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Button { button, down, x, y } => {
                self.position = (x, y);
                self.move_to(x, y);
                self.button(button, down);
            }
            Event::Wheel { dx, dy } => self.wheel(dx, dy),
            Event::Key { usage, down } => {
                if let Some(scan) = keys::usage_to_scan(usage) {
                    self.key(scan, down);
                }
            }
            Event::ReleaseAll => {}
        }
    }

    fn client_point(hwnd: HWND, x: u16, y: u16) -> (i32, i32) {
        let mut rect = RECT::default();
        let _ = unsafe { GetClientRect(hwnd, &mut rect) };
        (
            denormalize(x, (rect.right - rect.left).max(1) as u32),
            denormalize(y, (rect.bottom - rect.top).max(1) as u32),
        )
    }

    fn post(hwnd: HWND, message: u32, wparam: usize, lparam: isize) {
        let _ = unsafe { PostMessageW(Some(hwnd), message, WPARAM(wparam), LPARAM(lparam)) };
    }

    fn pack(x: i32, y: i32) -> isize {
        ((y as u16 as isize) << 16) | (x as u16 as isize)
    }

    fn move_to(&self, x: u16, y: u16) {
        match self.target {
            InputTarget::Window(hwnd) => {
                let (px, py) = Self::client_point(hwnd, x, y);
                Self::post(hwnd, WM_MOUSEMOVE, 0, Self::pack(px, py));
            }
            InputTarget::Desktop(monitor) => {
                let px = monitor.left + denormalize(x, (monitor.right - monitor.left) as u32);
                let py = monitor.top + denormalize(y, (monitor.bottom - monitor.top) as u32);
                let (vx, vy, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
                    )
                };
                let dx = ((i64::from(px - vx) * 65535) / i64::from(vw - 1)) as i32;
                let dy = ((i64::from(py - vy) * 65535) / i64::from(vh - 1)) as i32;
                send(&[mouse(
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    dx,
                    dy,
                    0,
                )]);
            }
        }
    }

    fn button(&self, button: Button, down: bool) {
        match self.target {
            InputTarget::Window(hwnd) => {
                let (x, y) = Self::client_point(hwnd, self.position.0, self.position.1);
                let (message, wparam) = match (button, down) {
                    (Button::Left, true) => (WM_LBUTTONDOWN, MK_LEFT),
                    (Button::Left, false) => (WM_LBUTTONUP, 0),
                    (Button::Right, true) => (WM_RBUTTONDOWN, MK_RIGHT),
                    (Button::Right, false) => (WM_RBUTTONUP, 0),
                    (Button::Middle, true) => (WM_MBUTTONDOWN, MK_MIDDLE),
                    (Button::Middle, false) => (WM_MBUTTONUP, 0),
                    (Button::Back, true) => (WM_XBUTTONDOWN, XBUTTON_BACK << 16),
                    (Button::Back, false) => (WM_XBUTTONUP, XBUTTON_BACK << 16),
                    (Button::Forward, true) => (WM_XBUTTONDOWN, XBUTTON_FORWARD << 16),
                    (Button::Forward, false) => (WM_XBUTTONUP, XBUTTON_FORWARD << 16),
                };
                Self::post(hwnd, message, wparam, Self::pack(x, y));
            }
            InputTarget::Desktop(_) => {
                let (flags, data) = match (button, down) {
                    (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (Button::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON_BACK as i32),
                    (Button::Back, false) => (MOUSEEVENTF_XUP, XBUTTON_BACK as i32),
                    (Button::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON_FORWARD as i32),
                    (Button::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON_FORWARD as i32),
                };
                send(&[mouse(flags, 0, 0, data)]);
            }
        }
    }

    fn wheel(&self, dx: i16, dy: i16) {
        match self.target {
            InputTarget::Window(hwnd) => {
                let (x, y) = Self::client_point(hwnd, self.position.0, self.position.1);
                let mut point = POINT { x, y };
                let _ = unsafe { ClientToScreen(hwnd, &mut point) };
                for (delta, message) in [(dy, WM_MOUSEWHEEL), (dx, WM_MOUSEHWHEEL)] {
                    if delta != 0 {
                        Self::post(
                            hwnd,
                            message,
                            (delta as u16 as usize) << 16,
                            Self::pack(point.x, point.y),
                        );
                    }
                }
            }
            InputTarget::Desktop(_) => {
                if dy != 0 {
                    send(&[mouse(MOUSEEVENTF_WHEEL, 0, 0, i32::from(dy))]);
                }
                if dx != 0 {
                    send(&[mouse(MOUSEEVENTF_HWHEEL, 0, 0, i32::from(dx))]);
                }
            }
        }
    }

    fn key(&self, scan: u16, down: bool) {
        let extended = scan & 0xff00 == 0xe000;
        let code = scan & 0xff;
        match self.target {
            InputTarget::Window(hwnd) => {
                let mut lparam = 1isize | ((code as isize) << 16);
                if extended {
                    lparam |= 1 << 24;
                }
                if !down {
                    lparam |= (1 << 30) | (1 << 31);
                }
                let vk = unsafe { MapVirtualKeyW(u32::from(scan), MAPVK_VSC_TO_VK_EX) } as usize;
                Self::post(hwnd, if down { WM_KEYDOWN } else { WM_KEYUP }, vk, lparam);
            }
            InputTarget::Desktop(_) => {
                let mut flags = KEYEVENTF_SCANCODE;
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                send(&[INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wScan: code,
                            dwFlags: flags,
                            ..Default::default()
                        },
                    },
                }]);
            }
        }
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        tracing::debug!("SendInput applied {sent} of {} inputs", inputs.len());
    }
}
