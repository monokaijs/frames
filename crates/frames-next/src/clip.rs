//! Clipboard sharing in a session: one two-way stream, polled every 250 ms
//! on the streaming thread (frames-clipboard keeps the sides from echoing).

use frames_clipboard::{Clipboard, Content, MemoryClipboard, Reader, Sync};
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
    sync: Sync,
    clipboard: Box<dyn Clipboard + Send>,
    out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    inbox: std::sync::mpsc::Receiver<Content>,
    next_poll: Instant,
    qa_copies: Option<(Instant, u64)>,
    pub sent: u64,
    pub received: u64,
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
        let qa = mode == Mode::Qa;
        Self {
            sync: Sync::default(),
            clipboard: if qa {
                Box::new(MemoryClipboard::default())
            } else {
                system_clipboard()
            },
            out,
            inbox,
            next_poll: Instant::now(),
            qa_copies: (qa && viewer).then(|| (Instant::now() + Duration::from_secs(1), 0)),
            sent: 0,
            received: 0,
        }
    }

    /// Call from the streaming loop; does work at most every 250 ms.
    pub fn tick(&mut self) {
        let now = Instant::now();
        if now < self.next_poll {
            return;
        }
        self.next_poll = now + Duration::from_millis(250);
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
            self.received += 1;
            if let Err(error) = self.sync.apply(&mut *self.clipboard, content) {
                eprintln!("clipboard: could not write: {error}");
            }
        }
        if let Some(content) = self.sync.poll(&*self.clipboard) {
            let mut bytes = Vec::new();
            content.encode(&mut bytes);
            if self.out.send(bytes).is_ok() {
                self.sent += 1;
            }
        }
    }

    /// What this side's clipboard holds now (QA reports).
    pub fn current(&self) -> Option<Content> {
        self.clipboard.read()
    }
}
