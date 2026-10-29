#![allow(unsafe_op_in_unsafe_fn)]
use super::wide;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::Gdi::*,
        System::LibraryLoader::GetModuleHandleW,
        UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
    },
    core::*,
};
struct State {
    address: HWND,
    audio: HWND,
    desktop: HWND,
    unattended: HWND,
    status: HWND,
    host: Option<Child>,
    clients: Vec<Child>,
    discovery_tx: std::sync::mpsc::Sender<std::result::Result<Vec<String>, String>>,
    discovery_rx: std::sync::mpsc::Receiver<std::result::Result<Vec<String>, String>>,
    discovering: bool,
}
unsafe fn label(hwnd: HWND, text: &str) {
    let _ = SetWindowTextW(hwnd, PCWSTR(wide(text).as_ptr()));
}
fn spawn(args: &[&str]) -> anyhow::Result<Child> {
    Ok(Command::new(std::env::current_exe()?)
        .args(args)
        .env("FRAMES_GUI_CHILD", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()?)
}
unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let pointer = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if message == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    if !pointer.is_null() {
        let state = &mut *pointer;
        match message {
            WM_TIMER => {
                if let Ok(result) = state.discovery_rx.try_recv() {
                    state.discovering = false;
                    match result {
                        Ok(addresses) if !addresses.is_empty() => {
                            SendMessageW(state.address, CB_RESETCONTENT, None, None);
                            for address in &addresses {
                                SendMessageW(
                                    state.address,
                                    CB_ADDSTRING,
                                    None,
                                    Some(LPARAM(wide(address).as_ptr() as isize)),
                                );
                            }
                            label(state.address, &addresses[0]);
                            label(
                                state.status,
                                "Nearby hosts are available in the address list. Select one, then Connect.",
                            );
                        }
                        Ok(_) => label(
                            state.status,
                            "No nearby hosts found. Enter a LAN or VPN address manually.",
                        ),
                        Err(error) => label(state.status, &error),
                    }
                }
                if state
                    .host
                    .as_mut()
                    .is_some_and(|host| matches!(host.try_wait(), Ok(Some(_))))
                {
                    state.host = None;
                    label(
                        state.status,
                        "Sharing stopped. Use Share this computer to restart it.",
                    );
                }
                state
                    .clients
                    .retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_))));
                return LRESULT(0);
            }
            WM_COMMAND => {
                let action = w.0 & 0xffff;
                let result = (|| -> anyhow::Result<()> {
                    match action {
                        1 => {
                            let mut address = [0u16; 256];
                            let size = GetWindowTextW(state.address, &mut address);
                            let address = String::from_utf16_lossy(&address[..size as usize]);
                            let mut args = vec!["connect", address.as_str()];
                            if SendMessageW(state.audio, BM_GETCHECK, None, None).0 != 0 {
                                args.push("--audio");
                            }
                            if SendMessageW(state.desktop, BM_GETCHECK, None, None).0 != 0 {
                                args.extend(["--view", "desktop"]);
                            }
                            state.clients.push(spawn(&args)?);
                            label(
                                state.status,
                                "Connecting. Compare and approve the pairing code on both devices.",
                            );
                        }
                        2 => {
                            if state.host.is_none() {
                                let mut args = vec!["host", "--pair"];
                                if SendMessageW(state.unattended, BM_GETCHECK, None, None).0 != 0 {
                                    args.push("--unattended");
                                }
                                state.host = Some(spawn(&args)?);
                                label(
                                    state.status,
                                    "Sharing on port 47990. New devices require pairing approval.",
                                );
                            }
                        }
                        3 => {
                            if let Some(mut child) = state.host.take() {
                                let _ = child.kill();
                                let _ = child.wait();
                            }
                            for mut child in state.clients.drain(..) {
                                let _ = child.kill();
                                let _ = child.wait();
                            }
                            label(state.status, "All sessions stopped.");
                        }
                        4 => {
                            if state.discovering {
                                return Ok(());
                            }
                            state.discovering = true;
                            label(state.status, "Looking for nearby hosts...");
                            let sender = state.discovery_tx.clone();
                            std::thread::spawn(move || {
                                let result = (|| -> anyhow::Result<Vec<String>> {
                                    let runtime = tokio::runtime::Builder::new_current_thread()
                                        .enable_all()
                                        .build()?;
                                    let devices =
                                        runtime.block_on(frames_core::discovery::browse(3))?;
                                    let mut addresses = std::collections::BTreeSet::new();
                                    for device in devices {
                                        let Some(port) = device["port"]
                                            .as_u64()
                                            .and_then(|p| u16::try_from(p).ok())
                                        else {
                                            continue;
                                        };
                                        if let Some(ips) = device["addresses"].as_array() {
                                            for ip in ips {
                                                if let Some(ip) = ip.as_str().and_then(|s| {
                                                    s.parse::<std::net::IpAddr>().ok()
                                                }) {
                                                    addresses.insert(
                                                        std::net::SocketAddr::new(ip, port)
                                                            .to_string(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    Ok(addresses.into_iter().collect())
                                })();
                                let _ = sender.send(result.map_err(|e| e.to_string()));
                            });
                        }
                        _ => {}
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    label(state.status, &error.to_string());
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                if let Some(mut child) = state.host.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                for mut child in state.clients.drain(..) {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            _ => {}
        }
    }
    DefWindowProcW(hwnd, message, w, l)
}
pub fn run() -> anyhow::Result<()> {
    unsafe {
        let _ = windows::Win32::System::Console::FreeConsole();
        use windows::Win32::UI::HiDpi::*;
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None)?;
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance.into(),
            lpszClassName: w!("FramesLauncher"),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as *mut _),
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW,
            w!("FramesLauncher"),
            w!("Frames"),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            620,
            390,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        let dpi = GetDpiForWindow(hwnd).max(96);
        let scale = |value: i32| (value * dpi as i32 + 48) / 96;
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            scale(620),
            scale(390),
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
        let mut metrics = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some(&mut metrics as *mut _ as *mut _),
            0,
            dpi,
        )?;
        let font = CreateFontIndirectW(&metrics.lfMessageFont);
        let control = |class: PCWSTR,
                       text: &str,
                       style: WINDOW_STYLE,
                       x,
                       y,
                       width,
                       height,
                       id|
         -> anyhow::Result<HWND> {
            let control = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                PCWSTR(wide(text).as_ptr()),
                WS_CHILD | WS_VISIBLE | style,
                scale(x),
                scale(y),
                scale(width),
                scale(height),
                Some(hwnd),
                Some(HMENU(id as *mut _)),
                Some(instance.into()),
                None,
            )?;
            SendMessageW(
                control,
                WM_SETFONT,
                Some(WPARAM(font.0 as usize)),
                Some(LPARAM(0)),
            );
            Ok(control)
        };
        control(
            w!("STATIC"),
            "FRAMES — your remote apps, in windows of their own",
            WINDOW_STYLE(0),
            24,
            22,
            560,
            26,
            0usize,
        )?;
        control(
            w!("STATIC"),
            "Host address (LAN / VPN)",
            WINDOW_STYLE(0),
            24,
            65,
            400,
            22,
            0,
        )?;
        let address = control(
            w!("COMBOBOX"),
            "127.0.0.1:47990",
            WS_VSCROLL | WS_TABSTOP | WINDOW_STYLE(CBS_DROPDOWN as u32 | CBS_AUTOHSCROLL as u32),
            24,
            92,
            400,
            180,
            0,
        )?;
        label(address, "127.0.0.1:47990");
        control(w!("BUTTON"), "Connect", WS_TABSTOP, 438, 90, 140, 30, 1)?;
        let audio = control(
            w!("BUTTON"),
            "Play host audio",
            WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            24,
            128,
            280,
            24,
            0,
        )?;
        let unattended = control(
            w!("BUTTON"),
            "Allow trusted devices to reconnect unattended",
            WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            24,
            195,
            550,
            24,
            0,
        )?;
        let desktop = control(
            w!("BUTTON"),
            "Open full desktop view",
            WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            24,
            160,
            400,
            24,
            0,
        )?;
        control(
            w!("BUTTON"),
            "Share this computer",
            WS_TABSTOP,
            24,
            235,
            190,
            32,
            2,
        )?;
        control(
            w!("BUTTON"),
            "Nearby devices",
            WS_TABSTOP,
            225,
            235,
            170,
            32,
            4,
        )?;
        control(
            w!("BUTTON"),
            "Stop sessions",
            WS_TABSTOP,
            406,
            235,
            172,
            32,
            3,
        )?;
        let status = control(
            w!("STATIC"),
            "Pair once on both devices. Window controls are in the remote window's system menu.",
            WINDOW_STYLE(0),
            24,
            290,
            550,
            46,
            0,
        )?;
        let (discovery_tx, discovery_rx) = std::sync::mpsc::channel();
        let mut state = State {
            address,
            audio,
            desktop,
            unattended,
            status,
            host: None,
            clients: vec![],
            discovery_tx,
            discovery_rx,
            discovering: false,
        };
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut state as *mut _ as isize);
        SetTimer(Some(hwnd), 1, 100, None);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetFocus(Some(address));
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            if !IsDialogMessageW(hwnd, &message).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        let _ = DeleteObject(font.into());
        Ok(())
    }
}
