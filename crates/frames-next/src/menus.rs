//! The shared window's application menus, shown in the viewer's window:
//! the host reads them (Win32 menu bar; macOS menu bar through
//! Accessibility) on a helper thread every two seconds, sends the tree on
//! a `MENU` stream when it changes, and runs an item when the viewer
//! chooses it (`Event::Menu`). Ids are the host's, valid for that tree.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const POLL: Duration = Duration::from_secs(2);
/// Bounds for a tree: deeper or larger menus are cut (the Windows reader;
/// FramesMac applies its own).
#[cfg(windows)]
pub const MAX_DEPTH: usize = 8;
#[cfg(windows)]
pub const MAX_ITEMS: usize = 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// 0 for separators and submenu titles.
    pub id: u32,
    pub title: String,
    pub enabled: bool,
    pub checked: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub separator: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Item>,
}

/// Host: the window's menus, read off the streaming thread.
pub struct HostMenus {
    latest: Arc<Mutex<(u64, Vec<Item>)>>,
    sent: u64,
    stream: Option<iroh::endpoint::SendStream>,
    window: u64,
    running: Arc<std::sync::atomic::AtomicBool>,
    /// Read the menus now and send them even if unchanged.
    wanted: Arc<std::sync::atomic::AtomicBool>,
}

impl HostMenus {
    /// Starts reading the menus of `window` (HWND or CGWindowID).
    pub fn start(window: u64) -> Self {
        let latest = Arc::new(Mutex::new((0, Vec::new())));
        let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let wanted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let (latest, running, wanted) = (latest.clone(), running.clone(), wanted.clone());
            std::thread::spawn(move || {
                use std::sync::atomic::Ordering::Relaxed;
                let mut last = None;
                while running.load(Relaxed) {
                    let forced = wanted.swap(false, Relaxed);
                    let items = read(window);
                    if forced || last.as_ref() != Some(&items) {
                        let mut slot = latest.lock().unwrap();
                        slot.0 += 1;
                        slot.1 = items.clone();
                        last = Some(items);
                    }
                    let since = std::time::Instant::now();
                    while since.elapsed() < POLL && running.load(Relaxed) && !wanted.load(Relaxed) {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            });
        }
        Self {
            latest,
            sent: 0,
            stream: None,
            window,
            running,
            wanted,
        }
    }

    /// Sends the tree when it changed since the last call (opening the
    /// stream the first time there is a menu).
    pub fn send_changes(
        &mut self,
        handle: &tokio::runtime::Handle,
        session: &frames_net::Session,
    ) -> Result<()> {
        let (generation, items) = {
            let slot = self.latest.lock().unwrap();
            if slot.0 == self.sent {
                return Ok(());
            }
            (slot.0, slot.1.clone())
        };
        self.sent = generation;
        if self.stream.is_none() {
            if items.is_empty() {
                return Ok(());
            }
            let (send, _) = handle.block_on(crate::streams::open(session, crate::streams::MENU))?;
            self.stream = Some(send);
        }
        let mut line = serde_json::to_vec(&items)?;
        line.push(b'\n');
        let send = self.stream.as_mut().expect("opened above");
        handle.block_on(send.write_all(&line))?;
        Ok(())
    }

    /// Reads the menus now and sends them again on the next
    /// [`Self::send_changes`] ("Refresh application menus").
    pub fn refresh(&mut self) {
        self.sent = 0;
        self.wanted
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Runs the item `id` of the tree last sent.
    pub fn invoke(&self, id: u32) {
        let window = self.window;
        // Off the streaming thread: pressing through Accessibility can
        // take a while.
        std::thread::spawn(move || {
            if let Err(error) = invoke(window, id) {
                println!("menu item {id}: {error:#}");
            }
        });
    }
}

impl Drop for HostMenus {
    fn drop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The top-level titles and how many items they hold, for logs.
pub fn summary(items: &[Item]) -> String {
    fn count(items: &[Item]) -> usize {
        items.iter().map(|i| 1 + count(&i.children)).sum()
    }
    let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
    format!("{} ({} items)", titles.join(", "), count(items))
}

/// Viewer: the host's menu trees, from its `MENU` stream.
pub async fn receive(recv: iroh::endpoint::RecvStream, trees: std::sync::mpsc::Sender<Vec<Item>>) {
    let mut lines = tokio::io::BufReader::new(recv);
    let mut line = String::new();
    loop {
        line.clear();
        match tokio::io::AsyncBufReadExt::read_line(&mut lines, &mut line).await {
            Ok(n) if n > 0 && line.len() <= 1024 * 1024 => {}
            _ => return,
        }
        let Ok(items) = serde_json::from_str::<Vec<Item>>(line.trim()) else {
            continue;
        };
        if trees.send(items).is_err() {
            return;
        }
    }
}

#[cfg(windows)]
fn read(window: u64) -> Vec<Item> {
    crate::win::menus::read(window).unwrap_or_default()
}

#[cfg(windows)]
fn invoke(window: u64, id: u32) -> Result<()> {
    crate::win::menus::invoke(window, id)
}

#[cfg(target_os = "macos")]
fn read(window: u64) -> Vec<Item> {
    u32::try_from(window)
        .ok()
        .and_then(frames_platform_macos::capture::app_menus_json)
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

#[cfg(target_os = "macos")]
fn invoke(window: u64, id: u32) -> Result<()> {
    let window = u32::try_from(window)?;
    anyhow::ensure!(
        frames_platform_macos::capture::press_menu(window, id),
        "it is gone or disabled"
    );
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn read(_: u64) -> Vec<Item> {
    Vec::new()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn invoke(_: u64, _: u32) -> Result<()> {
    anyhow::bail!("no application menus here")
}
