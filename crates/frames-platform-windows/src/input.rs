//! Applies v2 input events on the host.
//!
//! [`InputTarget::Desktop`] injects with `SendInput`, mapped onto the captured
//! monitor. [`InputTarget::Window`] posts window messages to one window, which
//! is how QA runs exercise the whole lane without moving the real cursor or
//! typing into the user's applications.

use frames_input::{
    Button, Event, Held, Motion, MotionReceiver, Pointer, PointerFilter, denormalize, keys,
};
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
    /// Real input (`SendInput`) onto wherever this window is on screen now,
    /// for sharing one window; follows the window when it moves. With
    /// `header`, positions cover its whole visible frame, else only its
    /// client area: always the rectangle the capture shows
    /// ([`captured_rect`]).
    WindowArea { hwnd: HWND, header: bool },
}

pub struct InputInjector {
    target: InputTarget,
    filter: PointerFilter,
    /// Relative mouse (games): motion datagrams move the pointer by deltas
    /// and buttons press where the pointer is.
    relative: bool,
    motion: MotionReceiver,
    held: Held,
    /// Last applied normalized position.
    position: (u16, u16),
    /// Pointer datagrams and events applied, for statistics.
    pub applied: u64,
    /// Why input may not have reached the shared window (for the viewer's
    /// title, as v1 said), until taken.
    problem: std::cell::Cell<Option<&'static str>>,
    injection_blocked: std::cell::Cell<bool>,
    rejected: std::cell::Cell<u64>,
}

// The HWND is only used with thread-safe message posting.
unsafe impl Send for InputInjector {}

impl InputInjector {
    /// Positions map onto `target` from now on (the shared window's header
    /// was shown or hidden).
    pub fn set_target(&mut self, target: InputTarget) {
        self.target = target;
    }

    /// What kept input from the shared window since the last call.
    pub fn take_problem(&self) -> Option<&'static str> {
        self.problem.take()
    }

    pub fn new(target: InputTarget) -> Self {
        Self {
            target,
            filter: PointerFilter::default(),
            relative: false,
            motion: MotionReceiver::default(),
            held: Held::default(),
            position: (0, 0),
            applied: 0,
            problem: std::cell::Cell::new(None),
            injection_blocked: std::cell::Cell::new(false),
            rejected: std::cell::Cell::new(0),
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

    /// Relative motion; ignored unless the viewer turned relative mode on.
    /// Real targets get relative `SendInput` moves, which games read as raw
    /// input; a QA window target only counts it.
    pub fn motion(&mut self, motion: &Motion) -> bool {
        if !self.relative {
            return false;
        }
        let Some((dx, dy)) = self.motion.accept(motion) else {
            return false;
        };
        if (dx, dy) != (0, 0) && !matches!(self.target, InputTarget::Window(_)) {
            self.inject(&[mouse(MOUSEEVENTF_MOVE, dx, dy, 0)]);
        }
        self.applied += 1;
        true
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

    /// Releases everything held (the viewer left).
    pub fn release_all(&mut self) {
        self.event(Event::ReleaseAll);
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Button { button, down, x, y } => {
                // In relative mode the viewer's position means nothing: press
                // where the pointer is.
                if !self.relative {
                    self.position = (x, y);
                    self.move_to(x, y);
                }
                // The shared window comes to the front first, so a click on
                // it lands on it even where other windows cover it (v1).
                if down {
                    self.raise();
                }
                self.button(button, down);
            }
            Event::Wheel { dx, dy } => self.wheel(dx, dy),
            Event::Key { usage, down } => {
                if let Some(scan) = keys::usage_to_scan(usage) {
                    // Keys go to the foreground window: the shared one.
                    if down {
                        self.raise();
                    }
                    self.key(scan, down);
                }
            }
            Event::Text { text } => {
                self.raise();
                self.text(&text)
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

    /// Brings a shared window to the front and gives it the keyboard, unless
    /// it (or a window it owns, such as its dialog or menu) has it already.
    fn raise(&self) {
        let InputTarget::WindowArea { hwnd, .. } = self.target else {
            return;
        };
        unsafe {
            // Menus, tooltips and other popups must not take the focus
            // (that would close a menu).
            let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
            if style & WS_POPUP.0 != 0 && style & WS_CAPTION.0 != WS_CAPTION.0 {
                return;
            }
            let foreground = GetForegroundWindow();
            if foreground == hwnd
                || GetWindow(foreground, GW_OWNER).is_ok_and(|owner| owner == hwnd)
                || GetAncestor(foreground, GA_ROOTOWNER) == hwnd
            {
                return;
            }
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
            let _ = SetForegroundWindow(hwnd);
            // Windows can refuse to hand over the foreground (another app
            // holds it, a full-screen game, focus-stealing rules).
            let now = GetForegroundWindow();
            if now != hwnd && GetAncestor(now, GA_ROOTOWNER) != hwnd {
                self.problem.set(Some(
                    "host OS denied keyboard focus; click the remote application first",
                ));
            }
        }
    }

    fn client_point(hwnd: HWND, x: u16, y: u16) -> (i32, i32) {
        use windows::Win32::UI::HiDpi::{
            GetWindowDpiAwarenessContext, SetThreadDpiAwarenessContext,
        };
        let mut rect = RECT::default();
        // Messages carry the window's own client coordinates, which for a
        // DPI-unaware window are smaller than this thread would be told.
        unsafe {
            let previous = SetThreadDpiAwarenessContext(GetWindowDpiAwarenessContext(hwnd));
            let _ = GetClientRect(hwnd, &mut rect);
            if !previous.is_invalid() {
                SetThreadDpiAwarenessContext(previous);
            }
        }
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

    /// The screen rectangle that normalized positions cover.
    fn area(&self) -> RECT {
        match self.target {
            InputTarget::Desktop(rect) => rect,
            InputTarget::WindowArea { hwnd, header } => captured_rect(hwnd, header),
            InputTarget::Window(_) => RECT::default(),
        }
    }

    fn move_to(&self, x: u16, y: u16) {
        match self.target {
            InputTarget::Window(hwnd) => {
                let (px, py) = Self::client_point(hwnd, x, y);
                Self::post(hwnd, WM_MOUSEMOVE, 0, Self::pack(px, py));
            }
            InputTarget::Desktop(_) | InputTarget::WindowArea { .. } => {
                let monitor = self.area();
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
                self.inject(&[mouse(
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
            InputTarget::Desktop(_) | InputTarget::WindowArea { .. } => {
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
                self.inject(&[mouse(flags, 0, 0, data)]);
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
            InputTarget::Desktop(_) | InputTarget::WindowArea { .. } => {
                if dy != 0 {
                    self.inject(&[mouse(MOUSEEVENTF_WHEEL, 0, 0, i32::from(dy))]);
                }
                if dx != 0 {
                    self.inject(&[mouse(MOUSEEVENTF_HWHEEL, 0, 0, i32::from(dx))]);
                }
            }
        }
    }

    /// Characters typed on the viewer (its layout, dead keys, IME), as v1
    /// inserted them: KEYEVENTF_UNICODE (surrogate pairs as two units), or
    /// WM_CHAR for a window by message.
    fn text(&self, text: &str) {
        let units: Vec<u16> = text.encode_utf16().collect();
        match self.target {
            InputTarget::Window(hwnd) => {
                for unit in units {
                    Self::post(hwnd, WM_CHAR, usize::from(unit), 1);
                }
            }
            InputTarget::Desktop(_) | InputTarget::WindowArea { .. } => {
                let inputs: Vec<INPUT> = units
                    .iter()
                    .flat_map(|&unit| {
                        [KEYEVENTF_UNICODE, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP].map(|flags| {
                            INPUT {
                                r#type: INPUT_KEYBOARD,
                                Anonymous: INPUT_0 {
                                    ki: KEYBDINPUT {
                                        wScan: unit,
                                        dwFlags: flags,
                                        ..Default::default()
                                    },
                                },
                            }
                        })
                    })
                    .collect();
                self.inject(&inputs);
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
            InputTarget::Desktop(_) | InputTarget::WindowArea { .. } => {
                let mut flags = KEYEVENTF_SCANCODE;
                if extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                self.inject(&[INPUT {
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

    pub fn rejected(&self) -> u64 {
        self.rejected.get()
    }

    fn inject(&self, inputs: &[INPUT]) {
        if inputs.is_empty() {
            return;
        }
        // Clear the thread's previous error: UIPI can reject SendInput
        // without setting a Win32 error. Never report a stale error as its cause.
        let (sent, error) = unsafe {
            windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));
            let sent = SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
            (sent, windows::Win32::Foundation::GetLastError().0)
        };
        let blocked = sent as usize != inputs.len();
        if blocked && !self.injection_blocked.get() {
            tracing::warn!(
                sent,
                expected = inputs.len(),
                error,
                "Windows rejected remote input"
            );
        }
        self.record_injection(blocked);
    }

    fn record_injection(&self, blocked: bool) {
        if blocked {
            self.rejected.set(self.rejected.get().saturating_add(1));
        }
        if self.injection_blocked.replace(blocked) != blocked {
            self.problem.set(Some(if blocked {
                "Windows rejected remote input; check the host's desktop and application permissions"
            } else {
                "Remote input resumed"
            }));
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

/// The part of a window that window capture shows: DWM's visible frame.
/// `GetWindowRect` also counts the invisible resize borders (about 7 px
/// each side on Windows 10 and 11), which put clicks off by that much.
pub fn visible_bounds(hwnd: HWND) -> RECT {
    use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    let mut rect = RECT::default();
    let visible = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if visible.is_err() || rect.right <= rect.left {
        let _ = unsafe { GetWindowRect(hwnd, &mut rect) };
    }
    rect
}

/// The screen rectangle window capture shows of `hwnd`: its visible frame
/// with `header`, else only its client area (no title bar, borders or menu
/// bar, as v1), cut to even sizes as encoders need. Input maps onto exactly
/// this rectangle, so a click lands where it was made.
pub fn captured_rect(hwnd: HWND, header: bool) -> RECT {
    let frame = visible_bounds(hwnd);
    let mut rect = frame;
    if !header {
        let mut client = RECT::default();
        let mut origin = POINT::default();
        let ok = unsafe {
            GetClientRect(hwnd, &mut client).is_ok() && ClientToScreen(hwnd, &mut origin).as_bool()
        };
        if ok && client.right > client.left && client.bottom > client.top {
            rect = RECT {
                left: (origin.x).max(frame.left),
                top: (origin.y).max(frame.top),
                right: (origin.x + client.right).min(frame.right),
                bottom: (origin.y + client.bottom).min(frame.bottom),
            };
            if rect.right <= rect.left || rect.bottom <= rect.top {
                rect = frame;
            }
        }
    }
    rect.right = rect.left + ((rect.right - rect.left).max(0) & !1);
    rect.bottom = rect.top + ((rect.bottom - rect.top).max(0) & !1);
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_rejection_and_recovery_are_reported_once_per_transition() {
        let input = InputInjector::new(InputTarget::Window(HWND::default()));
        input.record_injection(false);
        assert_eq!(input.take_problem(), None);
        input.record_injection(true);
        assert!(
            input
                .take_problem()
                .unwrap()
                .contains("Windows rejected remote input")
        );
        input.record_injection(true);
        assert_eq!(
            input.take_problem(),
            None,
            "do not spam every rejected pointer move"
        );
        assert_eq!(input.rejected(), 2);
        input.record_injection(false);
        assert_eq!(input.take_problem(), Some("Remote input resumed"));
        input.record_injection(false);
        assert_eq!(input.take_problem(), None);
    }
}
