//! Clipboard sharing in a session: one two-way stream, polled every 250 ms
//! on its own worker (frames-clipboard keeps the sides from echoing). Windows
//! delayed clipboard rendering can block for seconds; it must never hold up
//! video, the viewer's message pump, or input on either end of a session.

use frames_clipboard::{Clipboard, Content, MemoryClipboard, Reader, Sync};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    /// This machine's real clipboard.
    On,
    /// An in-memory clipboard; the viewer "copies" a line every 2 s. For
    /// unattended runs, which must never touch the user's clipboard.
    Qa,
}

pub fn system_clipboard() -> Box<dyn Clipboard + Send> {
    #[cfg(windows)]
    return Box::new(frames_platform_windows::clipboard::WindowsClipboard);
    #[cfg(target_os = "macos")]
    return Box::new(frames_platform_macos::clipboard::MacClipboard);
    #[cfg(not(any(windows, target_os = "macos")))]
    return Box::new(MemoryClipboard::default());
}

pub struct ClipboardLink {
    ticks: mpsc::SyncSender<()>,
    snapshots: mpsc::Receiver<Snapshot>,
    stop: Arc<AtomicBool>,
    next_poll: Instant,
    current: Option<Arc<Content>>,
    pub sent: u64,
    pub received: u64,
}

struct Snapshot {
    sent: u64,
    received: u64,
    current: Option<Arc<Content>>,
}

struct ClipboardWorker {
    sync: Sync,
    clipboard: Box<dyn Clipboard + Send>,
    out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    inbox: std::sync::mpsc::Receiver<Content>,
    qa_copies: Option<(Instant, u64)>,
    snapshot: Snapshot,
    stop: Arc<AtomicBool>,
}

impl ClipboardLink {
    /// Starts the stream's reader and writer on the current runtime.
    pub fn start(
        handle: &tokio::runtime::Handle,
        (mut send, mut recv): (iroh::endpoint::SendStream, iroh::endpoint::RecvStream),
        mode: Mode,
        viewer: bool,
    ) -> Self {
        let (out, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        handle.spawn(async move {
            while let Some(bytes) = outgoing.recv().await {
                if send.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });
        let (inbox_tx, inbox) = std::sync::mpsc::channel();
        handle.spawn(async move {
            let mut reader = Reader::default();
            let mut buffer = vec![0u8; 64 * 1024];
            while let Ok(Some(n)) = recv.read(&mut buffer).await {
                let Some(contents) = reader.push(&buffer[..n]) else {
                    return;
                };
                for content in contents {
                    if inbox_tx.send(content).is_err() {
                        return;
                    }
                }
            }
        });
        let clipboard: Box<dyn Clipboard + Send> = if mode == Mode::Qa {
            Box::new(MemoryClipboard::default())
        } else {
            system_clipboard()
        };
        Self::with_clipboard(out, inbox, clipboard, mode == Mode::Qa && viewer)
    }

    fn with_clipboard(
        out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
        inbox: mpsc::Receiver<Content>,
        clipboard: Box<dyn Clipboard + Send>,
        qa_copies: bool,
    ) -> Self {
        // One pending poll is enough even if a clipboard owner stalls.
        let (ticks, poll) = mpsc::sync_channel(1);
        let (snapshots_tx, snapshots) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let mut worker = ClipboardWorker {
            sync: Sync::default(),
            clipboard,
            out,
            inbox,
            qa_copies: qa_copies.then(|| (Instant::now() + Duration::from_secs(1), 0)),
            snapshot: Snapshot {
                sent: 0,
                received: 0,
                current: None,
            },
            stop: stop.clone(),
        };
        std::thread::spawn(move || {
            while poll.recv().is_ok() && !worker.stop.load(Ordering::Acquire) {
                worker.tick();
                let snapshot = Snapshot {
                    sent: worker.snapshot.sent,
                    received: worker.snapshot.received,
                    current: worker.snapshot.current.clone(),
                };
                let _ = snapshots_tx.try_send(snapshot);
            }
        });
        Self {
            ticks,
            snapshots,
            stop,
            next_poll: Instant::now(),
            current: None,
            sent: 0,
            received: 0,
        }
    }

    /// Schedule a poll and collect counters without calling any clipboard API.
    pub fn tick(&mut self) {
        while let Ok(snapshot) = self.snapshots.try_recv() {
            self.sent = snapshot.sent;
            self.received = snapshot.received;
            self.current = snapshot.current;
        }
        let now = Instant::now();
        if now < self.next_poll {
            return;
        }
        self.next_poll = now + Duration::from_millis(250);
        let _ = self.ticks.try_send(());
    }

    /// Last synchronized content; this diagnostic must not read the OS
    /// clipboard either, including when a session is being disconnected.
    pub fn current(&self) -> Option<Content> {
        self.current.as_deref().cloned()
    }
}

impl Drop for ClipboardLink {
    fn drop(&mut self) {
        // Do not join: an OS call may still be waiting for another app. Once
        // it returns, the worker exits without starting another operation.
        self.stop.store(true, Ordering::Release);
    }
}

impl ClipboardWorker {
    fn tick(&mut self) {
        let now = Instant::now();
        if let Some((due, count)) = &mut self.qa_copies
            && now >= *due
        {
            *count += 1;
            *due = now + Duration::from_secs(2);
            let text = Content::Text(format!("Frames QA clipboard {count}"));
            // The QA viewer's "user" copies something.
            let _ = self.clipboard.write(&text);
        }
        while let Ok(content) = self.inbox.try_recv() {
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            self.snapshot.received += 1;
            if let Err(error) = self.sync.apply(&mut *self.clipboard, content.clone()) {
                eprintln!("clipboard: could not write: {error}");
            } else {
                self.snapshot.current = Some(Arc::new(content));
            }
        }
        if self.stop.load(Ordering::Acquire) {
            return;
        }
        if let Some(content) = self.sync.poll(&*self.clipboard) {
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            let mut bytes = Vec::new();
            content.encode(&mut bytes);
            if self.out.send(bytes).is_ok() {
                self.snapshot.sent += 1;
            }
            self.snapshot.current = Some(Arc::new(content));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    struct SlowClipboard {
        count: Arc<AtomicU64>,
        reading: bool,
        entered: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
        exited: mpsc::Sender<()>,
    }

    impl Clipboard for SlowClipboard {
        fn change_count(&self) -> u64 {
            self.count.load(Ordering::Relaxed)
        }
        fn read(&self) -> Option<Content> {
            if self.reading {
                let _ = self.entered.send(());
                let _ = self.resume.recv_timeout(Duration::from_secs(5));
            }
            Some(Content::Text("slow clipboard".into()))
        }
        fn write(&mut self, _: &Content) -> Result<(), String> {
            if !self.reading {
                let _ = self.entered.send(());
                let _ = self.resume.recv_timeout(Duration::from_secs(5));
            }
            self.count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    impl Drop for SlowClipboard {
        fn drop(&mut self) {
            let _ = self.exited.send(());
        }
    }

    fn blocked_clipboard_does_not_block_session(reading: bool) {
        let (out, mut outgoing) = tokio::sync::mpsc::unbounded_channel();
        let (incoming, inbox) = mpsc::channel();
        let (entered_tx, entered) = mpsc::channel();
        let (resume, resume_rx) = mpsc::channel();
        let (exited_tx, exited) = mpsc::channel();
        let count = Arc::new(AtomicU64::new(0));
        let mut link = ClipboardLink::with_clipboard(
            out,
            inbox,
            Box::new(SlowClipboard {
                count: count.clone(),
                reading,
                entered: entered_tx,
                resume: resume_rx,
                exited: exited_tx,
            }),
            false,
        );
        link.tick();
        // Seed Sync's change counter before making a local or remote change.
        link.snapshots.recv_timeout(Duration::from_secs(2)).unwrap();
        if reading {
            count.store(1, Ordering::Relaxed);
        } else {
            incoming.send(Content::Text("remote copy".into())).unwrap();
        }
        link.next_poll = Instant::now();
        link.tick();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();

        let (done_tx, done) = mpsc::channel();
        let caller = std::thread::spawn(move || {
            for _ in 0..10 {
                link.next_poll = Instant::now();
                link.tick();
                let _ = link.current();
            }
            drop(link);
            done_tx.send(()).unwrap();
        });
        // Input/video/UI calls and disconnect must finish while the OS
        // clipboard call is still blocked. Unblock it even if this fails.
        let responsive = done.recv_timeout(Duration::from_millis(500)).is_ok();
        resume.send(()).unwrap();
        caller.join().unwrap();
        exited.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            responsive,
            "clipboard held up the streaming thread or disconnect"
        );
        assert!(
            outgoing.try_recv().is_err(),
            "no stale copy is sent after disconnect"
        );
    }

    #[test]
    fn delayed_clipboard_read_does_not_freeze_input_video_or_disconnect() {
        blocked_clipboard_does_not_block_session(true);
    }

    #[test]
    fn delayed_clipboard_write_does_not_freeze_input_video_or_disconnect() {
        blocked_clipboard_does_not_block_session(false);
    }

    #[test]
    fn received_clipboard_content_is_applied_without_echo() {
        let (out, mut outgoing) = tokio::sync::mpsc::unbounded_channel();
        let (incoming, inbox) = mpsc::channel();
        let content = Content::Text("remote text".into());
        incoming.send(content.clone()).unwrap();
        let mut link =
            ClipboardLink::with_clipboard(out, inbox, Box::new(MemoryClipboard::default()), false);
        link.tick();
        let snapshot = link.snapshots.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(snapshot.received, 1);
        assert_eq!(snapshot.sent, 0);
        assert_eq!(snapshot.current.as_deref(), Some(&content));
        assert!(outgoing.try_recv().is_err());
    }
}
