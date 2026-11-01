use crate::{CaptureStream, Desktop, NativeEncodedCapture};
use anyhow::{Context, Result};
use frames_core::{codec::CodecCapability, input::InputEvent, model::*};
use std::{collections::HashSet, sync::Arc};

#[derive(Clone)]
pub struct SharingWindow {
    pub window: RemoteWindow,
    pub selection: Option<WindowSelection>,
}
#[derive(Clone)]
pub struct WindowSelection(Arc<Selection>);
struct Selection {
    id: u64,
    marker: String,
}
impl WindowSelection {
    pub fn argument(&self) -> String {
        format!("{}:{}", self.0.id, self.0.marker)
    }
}
impl Drop for Selection {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            use windows::{
                Win32::{Foundation::HWND, UI::WindowsAndMessaging::*},
                core::PCWSTR,
            };
            let name: Vec<u16> = self.marker.encode_utf16().chain(Some(0)).collect();
            let hwnd = HWND(self.id as *mut _);
            if GetPropW(hwnd, PCWSTR(name.as_ptr())).0 as usize == 1 {
                let _ = RemovePropW(hwnd, PCWSTR(name.as_ptr()));
            }
        }
    }
}
pub fn sharing_windows(source: &dyn Desktop) -> Result<Vec<SharingWindow>> {
    source
        .windows()?
        .into_iter()
        .filter(|w| w.visible && matches!(w.role, WindowRole::Window | WindowRole::Dialog))
        .map(|window| {
            let id = source.local_window_handle(window.key)?;
            let marker = format!("FramesSelection-{}", uuid::Uuid::new_v4());
            #[cfg(windows)]
            let available = unsafe {
                use windows::{
                    Win32::{
                        Foundation::{HANDLE, HWND},
                        UI::WindowsAndMessaging::SetPropW,
                    },
                    core::PCWSTR,
                };
                let name: Vec<u16> = marker.encode_utf16().chain(Some(0)).collect();
                SetPropW(
                    HWND(id as *mut _),
                    PCWSTR(name.as_ptr()),
                    Some(HANDLE(std::ptr::without_provenance_mut(1))),
                )
                .is_ok()
            };
            #[cfg(not(windows))]
            let available = true;
            let selection = available.then(|| WindowSelection(Arc::new(Selection { id, marker })));
            Ok(SharingWindow { window, selection })
        })
        .collect()
}
pub fn parse_selection(argument: &str) -> Result<(u64, String)> {
    let (id, marker) = argument
        .split_once(':')
        .context("invalid window selection")?;
    let id = id.parse::<u64>()?;
    anyhow::ensure!(
        id != 0
            && marker
                .strip_prefix("FramesSelection-")
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok()),
        "invalid window selection"
    );
    Ok((id, marker.into()))
}
pub(crate) fn valid_marker(id: u64, marker: &str) -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::{
            Win32::{Foundation::HWND, UI::WindowsAndMessaging::*},
            core::PCWSTR,
        };
        let name: Vec<u16> = marker.encode_utf16().chain(Some(0)).collect();
        IsWindow(Some(HWND(id as *mut _))).as_bool()
            && GetPropW(HWND(id as *mut _), PCWSTR(name.as_ptr())).0 as usize == 1
    }
    #[cfg(not(windows))]
    {
        let _ = (id, marker);
        true
    }
}
struct SelectedDesktop {
    source: Arc<dyn Desktop>,
    windows: Vec<(WindowId, u64, String)>,
}
pub fn selected_desktop(
    exclusions: Vec<String>,
    arguments: Vec<String>,
) -> Result<Arc<dyn Desktop>> {
    anyhow::ensure!(
        !arguments.is_empty() && arguments.len() <= 32,
        "Choose between one and 32 windows"
    );
    let source = crate::desktop(exclusions)?;
    let available = source.windows()?;
    let mut seen = HashSet::new();
    let mut windows = Vec::new();
    for arg in arguments {
        let (id, marker) = parse_selection(&arg)?;
        anyhow::ensure!(
            seen.insert(id) && valid_marker(id, &marker),
            "A selected window closed; choose it again"
        );
        let key = available
            .iter()
            .find(|w| {
                source
                    .local_window_handle(w.key)
                    .is_ok_and(|handle| handle == id)
            })
            .context("A selected window closed; choose it again")?
            .key;
        windows.push((key, id, marker));
    }
    Ok(Arc::new(SelectedDesktop { source, windows }))
}
impl SelectedDesktop {
    fn admits(&self, key: WindowId) -> bool {
        self.windows
            .iter()
            .any(|(key_id, id, marker)| *key_id == key && valid_marker(*id, marker))
    }
    fn require(&self, key: WindowId) -> Result<()> {
        anyhow::ensure!(self.admits(key), "This window is not selected for sharing");
        Ok(())
    }
    fn preview_key(&self, key: Option<WindowId>) -> Result<WindowId> {
        let key = key
            .or_else(|| {
                self.windows
                    .iter()
                    .find(|(id, _, _)| self.admits(*id))
                    .map(|(id, _, _)| *id)
            })
            .context("All selected windows closed")?;
        self.require(key)?;
        Ok(key)
    }
}
impl Desktop for SelectedDesktop {
    fn windows(&self) -> Result<Vec<RemoteWindow>> {
        Ok(self
            .source
            .windows()?
            .into_iter()
            .filter(|w| self.admits(w.key))
            .collect())
    }
    fn allow_desktop(&self) -> bool {
        false
    }
    fn allow_clipboard(&self) -> bool {
        false
    }
    fn allow_file_transfer(&self) -> bool {
        false
    }
    fn allow_device_bridge(&self) -> bool {
        false
    }
    fn check_health(&self) -> Result<()> {
        self.source.check_health()?;
        anyhow::ensure!(
            self.windows.iter().any(|(id, _, _)| self.admits(*id)),
            "All selected windows closed"
        );
        Ok(())
    }
    fn action(&self, key: WindowId, action: &WindowAction) -> Result<()> {
        self.require(key)?;
        self.source.action(key, action)
    }
    fn input(&self, key: WindowId, event: &InputEvent) -> Result<()> {
        self.require(key)?;
        self.source.input(key, event)
    }
    fn release_input(&self) {
        self.source.release_input()
    }
    fn capture(&self, key: WindowId, cursor: bool) -> Result<CaptureStream> {
        self.require(key)?;
        let capture = self.source.capture(key, cursor)?;
        self.require(key)?;
        Ok(capture)
    }
    fn capture_for_encoding(
        &self,
        key: WindowId,
        cursor: bool,
        hardware: bool,
    ) -> Result<CaptureStream> {
        self.require(key)?;
        let capture = self.source.capture_for_encoding(key, cursor, hardware)?;
        self.require(key)?;
        Ok(capture)
    }
    fn encoded_capture(
        &self,
        key: WindowId,
        cursor: bool,
        cap: &CodecCapability,
        fps: u32,
    ) -> Result<Option<NativeEncodedCapture>> {
        self.require(key)?;
        let capture = self.source.encoded_capture(key, cursor, cap, fps)?;
        self.require(key)?;
        Ok(capture)
    }
    fn preview_image(&self, key: Option<WindowId>) -> Result<Option<Vec<u8>>> {
        let key = self.preview_key(key)?;
        let image = self.source.preview_image(Some(key))?;
        self.require(key)?;
        Ok(image)
    }
    fn preview_capture(&self, key: Option<WindowId>) -> Result<CaptureStream> {
        self.capture(self.preview_key(key)?, false)
    }
    fn clipboard(&self) -> Result<Option<String>> {
        Ok(None)
    }
    fn set_clipboard(&self, _: &str) -> Result<()> {
        anyhow::bail!("Clipboard is unavailable for selected-window sharing")
    }
    fn menu(&self, key: WindowId) -> Result<Vec<MenuItem>> {
        self.require(key)?;
        self.source.menu(key)
    }
    fn audio(&self) -> Result<crate::audio::AudioCapture> {
        #[cfg(all(windows, frames_shared_input))]
        {
            use windows::Win32::{
                Foundation::HWND, UI::WindowsAndMessaging::GetWindowThreadProcessId,
            };
            let mut processes = HashSet::new();
            for (key, id, _) in &self.windows {
                if self.admits(*key) {
                    let mut pid = 0;
                    unsafe {
                        GetWindowThreadProcessId(HWND(*id as *mut _), Some(&mut pid));
                    }
                    processes.insert(pid);
                }
            }
            if processes.len() == 1 {
                return crate::windows::process_audio::start(*processes.iter().next().unwrap());
            }
        }
        anyhow::bail!("Selected-window audio is unavailable for this combination of applications")
    }
}

#[cfg(all(test, windows, frames_shared_input))]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Command, Stdio},
        time::Duration,
    };
    struct Fixture(std::process::Child, u64);
    impl Fixture {
        fn start() -> Result<Self> {
            let executable = std::path::Path::new(env!("FRAMES_INPUT_ARTIFACT"))
                .with_file_name("shared_input_fixture.exe");
            let mut child = Command::new(executable)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()?;
            let mut line = String::new();
            BufReader::new(child.stdout.take().unwrap()).read_line(&mut line)?;
            let ready: serde_json::Value = serde_json::from_str(&line)?;
            Ok(Self(child, ready["hwnd"].as_u64().unwrap()))
        }
        fn destroy(&mut self) -> Result<()> {
            self.0.stdin.as_mut().unwrap().write_all(b"destroy\n")?;
            self.0.stdin.as_mut().unwrap().flush()?;
            std::thread::sleep(Duration::from_millis(150));
            Ok(())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    #[ignore = "opens two owned nonactivating Windows fixtures"]
    fn selected_windows_reject_desktop_other_windows_and_closed_identities() -> Result<()> {
        let mut first = Fixture::start()?;
        let mut second = Fixture::start()?;
        let source = crate::desktop(vec![])?;
        let choices = sharing_windows(source.as_ref())?;
        let ticket = |handle| {
            choices
                .iter()
                .find(|c| source.local_window_handle(c.window.key).ok() == Some(handle))
                .and_then(|c| c.selection.clone())
                .context("fixture was not selectable")
        };
        let first_ticket = ticket(first.1)?;
        let second_ticket = ticket(second.1)?;
        assert_eq!(parse_selection(&first_ticket.argument())?.0, first.1);
        let (first_id, first_marker) = parse_selection(&first_ticket.argument())?;
        assert!(selected_desktop(vec![], vec![format!("{}:{first_marker}", second.1)]).is_err());
        assert!(
            selected_desktop(
                vec![],
                vec![first_ticket.argument(), first_ticket.argument()]
            )
            .is_err()
        );
        let selected = selected_desktop(
            vec![],
            vec![first_ticket.argument(), second_ticket.argument()],
        )?;
        assert_eq!(selected.windows()?.len(), 2);
        assert!(!selected.allow_desktop());
        assert!(selected.desktop_windows().is_err());
        assert!(
            selected.audio().is_err(),
            "two unrelated selected processes must not fall back to desktop audio"
        );
        assert!(
            !selected.allow_clipboard()
                && !selected.allow_file_transfer()
                && !selected.allow_device_bridge()
        );
        let outside = WindowId {
            id: u64::MAX,
            generation: 1,
        };
        assert!(selected.capture(outside, false).is_err());
        assert!(selected.preview_image(Some(outside)).is_err());
        assert!(
            selected
                .input(outside, &InputEvent::Pointer { x: 0.5, y: 0.5 })
                .is_err()
        );
        assert!(selected.action(outside, &WindowAction::Close).is_err());
        let key = selected.windows()?[0].key;
        let capture = selected.capture(key, false)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(frame) = capture.frames.borrow().clone() {
                assert!(frame.validate());
                break;
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "selected preview did not capture its owned fixture"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(capture);
        second.destroy()?;
        assert_eq!(selected.windows()?.len(), 1);
        assert!(selected.check_health().is_ok());
        assert!(selected_desktop(vec![], vec![second_ticket.argument()]).is_err());
        first.destroy()?;
        assert!(selected.windows()?.is_empty());
        assert!(selected.check_health().is_err());
        assert!(!valid_marker(first_id, &first_marker));
        Ok(())
    }
    #[test]
    fn invalid_local_selection_arguments_are_rejected() {
        for argument in [
            "game.exe",
            "0:FramesSelection-00000000-0000-0000-0000-000000000000",
            "1:other-property",
            "1:FramesSelection-not-a-uuid",
        ] {
            assert!(parse_selection(argument).is_err());
        }
    }
    #[test]
    #[ignore = "attaches the private adapter to an owned nonactivating Windows fixture"]
    fn one_window_picker_ticket_attaches_and_cannot_be_reused_after_stop() -> Result<()> {
        let fixture = Fixture::start()?;
        let source = crate::desktop(vec![])?;
        let choices = sharing_windows(source.as_ref())?;
        let ticket = choices
            .iter()
            .find(|c| source.local_window_handle(c.window.key).ok() == Some(fixture.1))
            .and_then(|c| c.selection.clone())
            .context("fixture was not selectable")?;
        let argument = ticket.argument();
        let host = crate::shared_selected_desktop(vec![], argument.clone())?;
        let prepared = host.prepare_control()?.unwrap();
        host.begin_control()?;
        assert_eq!(host.windows()?.len(), 1);
        host.shared_resume(prepared.generation)?;
        host.shared_input(
            prepared.window,
            prepared.generation,
            &InputEvent::Key {
                usage: 0x1a,
                down: true,
            },
        )?;
        host.shared_release(prepared.generation);
        host.end_control();
        drop(host);
        assert!(crate::shared_selected_desktop(vec![], argument).is_err());
        Ok(())
    }
}
