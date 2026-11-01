//! Explicitly confirmed update shutdown, restricted to this installation.
#[cfg(windows)]
use anyhow::Context;
use anyhow::{Result, ensure};
use std::path::{Path, PathBuf};

fn belongs_to_installation(executable: &Path, target: &Path, bundle: bool) -> bool {
    if bundle {
        [
            "Contents/MacOS/Frames",
            "Contents/MacOS/FramesHelper",
            "Contents/Resources/FramesHelper",
        ]
        .iter()
        .any(|relative| executable == target.join(relative))
    } else {
        executable == target
    }
}

pub struct BackgroundSharing {
    #[cfg(target_os = "macos")]
    resume: Option<PathBuf>,
}

impl BackgroundSharing {
    pub fn pause(target: &Path, bundle: bool) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            ensure!(bundle, "macOS updates require a packaged app");
            let helper = target.join("Contents/MacOS/FramesHelper");
            let output = std::process::Command::new(&helper)
                .arg("--pause-sharing-for-update")
                .output()?;
            ensure!(
                output.status.success(),
                "Could not pause sharing at login: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return Ok(Self {
                resume: (String::from_utf8_lossy(&output.stdout).trim() == "resume")
                    .then_some(helper),
            });
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (target, bundle);
            Ok(Self {})
        }
    }
}

impl Drop for BackgroundSharing {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if let Some(helper) = &self.resume {
            match std::process::Command::new(helper)
                .arg("--resume-sharing-after-update")
                .status()
            {
                Ok(status) if status.success() => {}
                result => tracing::warn!(?result, "could not resume sharing at login after update"),
            }
        }
    }
}

#[cfg(windows)]
pub fn stop_processes(target: &Path, bundle: bool) -> Result<()> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows::{
        Win32::{
            Foundation::{CloseHandle, ERROR_NO_MORE_FILES},
            System::{Diagnostics::ToolHelp::*, Threading::*},
        },
        core::PWSTR,
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        let _snapshot = OwnedHandle::from_raw_handle(snapshot.0);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        Process32FirstW(snapshot, &mut entry)?;
        loop {
            if entry.th32ProcessID != std::process::id()
                && let Ok(query) = OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION,
                    false,
                    entry.th32ProcessID,
                )
            {
                let mut path = vec![0u16; 32768];
                let mut size = path.len() as u32;
                let read = QueryFullProcessImageNameW(
                    query,
                    PROCESS_NAME_WIN32,
                    PWSTR(path.as_mut_ptr()),
                    &mut size,
                );
                let _ = CloseHandle(query);
                if read.is_ok() {
                    let path = PathBuf::from(String::from_utf16_lossy(&path[..size as usize]));
                    if let Ok(path) = path.canonicalize()
                        && belongs_to_installation(&path, target, bundle)
                    {
                        // Re-read the image on the termination handle to prevent
                        // terminating an unrelated process if its PID was reused.
                        let process = OpenProcess(
                            PROCESS_QUERY_LIMITED_INFORMATION
                                | PROCESS_TERMINATE
                                | PROCESS_SYNCHRONIZE,
                            false,
                            entry.th32ProcessID,
                        )
                        .context("Could not disconnect a Frames process for update")?;
                        let _process = OwnedHandle::from_raw_handle(process.0);
                        let mut checked = vec![0u16; 32768];
                        let mut size = checked.len() as u32;
                        QueryFullProcessImageNameW(
                            process,
                            PROCESS_NAME_WIN32,
                            PWSTR(checked.as_mut_ptr()),
                            &mut size,
                        )?;
                        let checked =
                            PathBuf::from(String::from_utf16_lossy(&checked[..size as usize]))
                                .canonicalize()?;
                        if belongs_to_installation(&checked, target, bundle) {
                            TerminateProcess(process, 0)
                                .context("Could not disconnect Frames for update")?;
                            ensure!(
                                WaitForSingleObject(process, 5000)
                                    == windows::Win32::Foundation::WAIT_OBJECT_0,
                                "Frames did not stop for update"
                            );
                        }
                    }
                }
            }
            if let Err(error) = Process32NextW(snapshot, &mut entry) {
                if error.code() == ERROR_NO_MORE_FILES.to_hresult() {
                    break;
                }
                return Err(error.into());
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn stop_processes(target: &Path, bundle: bool) -> Result<()> {
    use std::process::Command;
    let output = Command::new("/bin/ps")
        .args(["-ww", "-axo", "pid=,comm="])
        .output()?;
    ensure!(
        output.status.success(),
        "Could not inspect Frames processes for update"
    );
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        let Some((pid, executable)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(executable) = Path::new(executable.trim()).canonicalize() else {
            continue;
        };
        if belongs_to_installation(&executable, target, bundle) {
            // ps is run again immediately before signalling, guarding PID reuse.
            let current = Command::new("/bin/ps")
                .args(["-ww", "-p", &pid.to_string(), "-o", "comm="])
                .output()?;
            if current.status.success()
                && Path::new(String::from_utf8_lossy(&current.stdout).trim())
                    .canonicalize()
                    .ok()
                    .as_ref()
                    == Some(&executable)
            {
                let result = Command::new("/bin/kill")
                    .args(["-TERM", &pid.to_string()])
                    .output()?;
                ensure!(
                    result.status.success(),
                    "Could not disconnect Frames process {pid}: {}",
                    String::from_utf8_lossy(&result.stderr)
                );
            }
        }
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn stop_processes(_: &Path, _: bool) -> Result<()> {
    anyhow::bail!("Update shutdown is supported on Windows and macOS only")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_matches_the_exact_installation_not_the_process_name() {
        let target = Path::new("install/Frames.exe");
        assert!(belongs_to_installation(target, target, false));
        assert!(!belongs_to_installation(
            Path::new("another/Frames.exe"),
            target,
            false
        ));
        assert!(!belongs_to_installation(
            Path::new("install/apply-helper.exe"),
            target,
            false
        ));
        let app = Path::new("Applications/Frames.app");
        assert!(belongs_to_installation(
            &app.join("Contents/MacOS/Frames"),
            app,
            true
        ));
        assert!(belongs_to_installation(
            &app.join("Contents/MacOS/FramesHelper"),
            app,
            true
        ));
        assert!(!belongs_to_installation(
            Path::new("Applications/Other.app/Contents/MacOS/Frames"),
            app,
            true
        ));
        assert!(!belongs_to_installation(
            &app.join("Contents/MacOS/Other"),
            app,
            true
        ));
    }
}
