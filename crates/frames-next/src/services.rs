//! A host's per-session services fed by the viewer's tagged streams:
//! input events, clipboard, and incoming files.

use crate::streams::{Routes, Streams, route};
use crate::{Options, clip, transfer};
use frames_input::{Event, EventReader};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct HostServices {
    /// Decoded input events, for the streaming thread to apply.
    pub events: std::sync::mpsc::Receiver<Event>,
    /// The clipboard stream, once the viewer opens one.
    pub clipboard: std::sync::mpsc::Receiver<Streams>,
    /// Input events received (applied or not); the macOS host reports it.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub event_count: Arc<AtomicU64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl HostServices {
    /// Starts routing the viewer's streams. Files are accepted only from a
    /// device trusted for control (they are written to this disk).
    pub fn start(session: &frames_net::Session, o: &Options) -> Self {
        let (events_tx, events) = std::sync::mpsc::channel();
        let event_count = Arc::new(AtomicU64::new(0));
        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel::<Streams>();
        let (clipboard_tx, clipboard) = std::sync::mpsc::channel();
        let (file_tx, mut file_rx) = tokio::sync::mpsc::unbounded_channel::<Streams>();
        let routes = Routes {
            input: Some(input_tx),
            clipboard: (o.clipboard != clip::Mode::Off).then_some(clipboard_tx),
            file: o.control.then_some(file_tx),
        };
        let mut tasks = vec![tokio::spawn(route(session.clone(), routes))];
        let count = event_count.clone();
        tasks.push(tokio::spawn(async move {
            while let Some((_send, mut recv)) = input_rx.recv().await {
                let mut decoder = EventReader::default();
                let mut buffer = [0u8; 1024];
                while let Ok(Some(n)) = recv.read(&mut buffer).await {
                    for event in decoder.push(&buffer[..n]) {
                        count.fetch_add(1, Ordering::Relaxed);
                        if events_tx.send(event).is_err() {
                            return;
                        }
                    }
                }
            }
        }));
        let dir = o
            .receive_dir
            .clone()
            .unwrap_or_else(transfer::default_receive_dir);
        tasks.push(tokio::spawn(async move {
            while let Some(streams) = file_rx.recv().await {
                let dir = dir.clone();
                tokio::spawn(async move {
                    match transfer::receive(streams, dir).await {
                        Ok(path) => println!("received {}", path.display()),
                        Err(error) => println!("file transfer failed: {error:#}"),
                    }
                });
            }
        }));
        Self {
            events,
            clipboard,
            event_count,
            tasks,
        }
    }
}

impl Drop for HostServices {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
