//! One launcher per user. A second launch (Start menu, a second click, `open
//! -n`) asks the running launcher to show its window and exits instead of
//! opening another launcher with its own sessions, tray icon and menu bar item.
//!
//! The lock is an OS file lock, so it is released however the owner ends,
//! including a crash. Sessions, the sharing agent and the CLI never take it.
use anyhow::Result;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    path::PathBuf,
};

pub enum Launch {
    /// This process is the launcher; keep the value alive while it runs.
    Primary(Instance),
    /// Another launcher is running and has been asked to show its window.
    Secondary,
}

pub struct Instance {
    _lock: File,
    show_request: PathBuf,
}

impl Instance {
    /// True once per request from a later launch.
    pub fn take_show_request(&self) -> bool {
        fs::remove_file(&self.show_request).is_ok()
    }
}

pub fn acquire() -> Result<Launch> {
    acquire_in(frames_core::trust::config_dir()?)
}

fn acquire_in(directory: PathBuf) -> Result<Launch> {
    fs::create_dir_all(&directory)?;
    let show_request = directory.join("launcher.show");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(directory.join("launcher.lock"))?;
    match lock.try_lock_exclusive() {
        Ok(()) => {
            // A request left by a launch that raced a previous owner's exit.
            let _ = fs::remove_file(&show_request);
            Ok(Launch::Primary(Instance {
                _lock: lock,
                show_request,
            }))
        }
        Err(error) if error.kind() == fs2::lock_contended_error().kind() => {
            fs::write(&show_request, std::process::id().to_string())?;
            Ok(Launch::Secondary)
        }
        // Locking unsupported (unusual filesystems): never block the launcher.
        Err(error) => {
            tracing::warn!("cannot enforce a single launcher: {error}");
            Ok(Launch::Primary(Instance {
                _lock: lock,
                show_request,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_launch_signals_the_first_and_the_lock_frees_on_exit() {
        let directory = tempfile::tempdir().unwrap();
        let Launch::Primary(first) = acquire_in(directory.path().into()).unwrap() else {
            panic!("the first launch must own the launcher");
        };
        assert!(!first.take_show_request());
        assert!(matches!(
            acquire_in(directory.path().into()).unwrap(),
            Launch::Secondary
        ));
        assert!(first.take_show_request(), "the second launch asks to show");
        assert!(!first.take_show_request(), "each request is taken once");
        drop(first);
        assert!(matches!(
            acquire_in(directory.path().into()).unwrap(),
            Launch::Primary(_)
        ));
    }
}
