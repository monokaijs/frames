//! Session processes keep their prompts owned by the launcher that started them.
use windows::{
    Win32::{
        Foundation::*,
        UI::{Input::KeyboardAndMouse::GetActiveWindow, WindowsAndMessaging::*},
    },
    core::PCWSTR,
};

fn owner() -> Option<HWND> {
    struct Search {
        process: u32,
        launcher: Option<u32>,
        own: Option<HWND>,
        parent: Option<HWND>,
    }
    unsafe extern "system" fn visit(window: HWND, context: LPARAM) -> windows_core::BOOL {
        unsafe {
            let search = &mut *(context.0 as *mut Search);
            if !IsWindowVisible(window).as_bool() || GetWindow(window, GW_OWNER).is_ok() {
                return TRUE;
            }
            let mut process = 0;
            GetWindowThreadProcessId(window, Some(&mut process));
            if process == search.process && search.own.is_none() {
                search.own = Some(window);
            } else if Some(process) == search.launcher && search.parent.is_none() {
                search.parent = Some(window);
            }
            TRUE
        }
    }
    let mut search = Search {
        process: std::process::id(),
        launcher: std::env::var("FRAMES_LAUNCHER_PID")
            .ok()
            .and_then(|v| v.parse().ok()),
        own: None,
        parent: None,
    };
    unsafe {
        let active = GetActiveWindow();
        if !active.is_invalid() {
            return Some(active);
        }
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut _ as isize));
    }
    search.own.or(search.parent)
}

pub fn message(title: &str, text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let title = super::wide(title);
    let text = super::wide(text);
    unsafe {
        MessageBoxW(
            owner(),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            style | MB_SETFOREGROUND,
        )
    }
}

pub fn connection(name: &str, text: &str) -> anyhow::Result<crate::ConnectionDecision> {
    use windows::Win32::UI::Controls::*;
    let title = super::wide("Frames — Connection request");
    let heading = super::wide(&format!("Connect with {name}?"));
    let content = super::wide(text);
    let labels = [
        super::wide("Decline connection\nDo not allow this session."),
        super::wide(
            "Accept and trust\nAllow this session and save the device for future connections.",
        ),
        super::wide("Accept\nAllow this session only. You can trust the device later."),
    ];
    let buttons: Vec<_> = labels
        .iter()
        .enumerate()
        .map(|(i, label)| TASKDIALOG_BUTTON {
            nButtonID: 100 + i as i32,
            pszButtonText: PCWSTR(label.as_ptr()),
        })
        .collect();
    let config = TASKDIALOGCONFIG {
        cbSize: std::mem::size_of::<TASKDIALOGCONFIG>() as u32,
        hwndParent: owner().unwrap_or_default(),
        dwFlags: TDF_ALLOW_DIALOG_CANCELLATION | TDF_USE_COMMAND_LINKS,
        pszWindowTitle: PCWSTR(title.as_ptr()),
        pszMainInstruction: PCWSTR(heading.as_ptr()),
        pszContent: PCWSTR(content.as_ptr()),
        cButtons: buttons.len() as u32,
        pButtons: buttons.as_ptr(),
        nDefaultButton: 100,
        ..Default::default()
    };
    let mut selected = 0;
    unsafe {
        TaskDialogIndirect(&config, Some(&mut selected), None, None)?;
    }
    Ok(match selected {
        101 => crate::ConnectionDecision::Trust,
        102 => crate::ConnectionDecision::Accept,
        _ => crate::ConnectionDecision::Decline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a visible launcher identified by FRAMES_LAUNCHER_PID"]
    fn session_dialog_resolves_launcher_owner() {
        let launcher: u32 = std::env::var("FRAMES_LAUNCHER_PID")
            .unwrap()
            .parse()
            .unwrap();
        let window = owner().expect("session dialog must have a launcher owner");
        let mut process = 0;
        unsafe {
            GetWindowThreadProcessId(window, Some(&mut process));
        }
        assert_eq!(process, launcher);
    }
}
