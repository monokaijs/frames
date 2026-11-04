//! Clipboard sharing for v2 sessions (docs/V2_PARITY.md).
//!
//! Each side polls its clipboard's change counter. When it moves and the
//! content is not what the other side just sent, the content goes to the
//! peer on the session's clipboard stream, where it is written to that
//! clipboard. Remembering the last content in both directions keeps the two
//! clipboards from echoing a change back and forth.
//!
//! Record on the stream: 4-byte little-endian length `n`, then `n` bytes: a
//! kind byte and the content. Unknown kinds are skipped by length.

/// Largest text shared (UTF-8 bytes), as in v1.
pub const MAX_TEXT: usize = 2 * 1024 * 1024;
/// Largest PNG shared.
pub const MAX_PNG: usize = 16 * 1024 * 1024;

const KIND_TEXT: u8 = 1;
const KIND_PNG: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Png(Vec<u8>),
}

impl Content {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let (kind, bytes) = match self {
            Self::Text(text) => (KIND_TEXT, text.as_bytes()),
            Self::Png(png) => (KIND_PNG, png.as_slice()),
        };
        out.extend(((bytes.len() + 1) as u32).to_le_bytes());
        out.push(kind);
        out.extend_from_slice(bytes);
    }

    fn fits(&self) -> bool {
        match self {
            Self::Text(text) => text.len() <= MAX_TEXT,
            Self::Png(png) => png.len() <= MAX_PNG,
        }
    }
}

/// Splits the clipboard stream into contents. Returns `None` when a record
/// claims more than any content may hold (the stream is then closed).
#[derive(Debug, Default)]
pub struct Reader {
    buffer: Vec<u8>,
}

impl Reader {
    pub fn push(&mut self, bytes: &[u8]) -> Option<Vec<Content>> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        while self.buffer.len() >= at + 4 {
            let length = u32::from_le_bytes(self.buffer[at..at + 4].try_into().unwrap()) as usize;
            if length > MAX_PNG + 1 {
                return None;
            }
            let end = at + 4 + length;
            if end > self.buffer.len() {
                break;
            }
            let record = &self.buffer[at + 4..end];
            match record.split_first() {
                Some((&KIND_TEXT, text)) => {
                    if let Ok(text) = std::str::from_utf8(text)
                        && text.len() <= MAX_TEXT
                    {
                        out.push(Content::Text(text.to_string()));
                    }
                }
                Some((&KIND_PNG, png)) => out.push(Content::Png(png.to_vec())),
                _ => {}
            }
            at = end;
        }
        self.buffer.drain(..at);
        Some(out)
    }
}

/// A clipboard the sync can read and write.
pub trait Clipboard {
    /// Increases whenever the clipboard changes (any application).
    fn change_count(&self) -> u64;
    /// The current content, preferring text.
    fn read(&self) -> Option<Content>;
    fn write(&mut self, content: &Content) -> Result<(), String>;
}

/// An in-memory clipboard, for tests and QA runs (never the user's).
#[derive(Debug, Default)]
pub struct MemoryClipboard {
    content: Option<Content>,
    count: u64,
}

impl MemoryClipboard {
    /// What another application on this machine would do: change it.
    pub fn set_locally(&mut self, content: Content) {
        self.content = Some(content);
        self.count += 1;
    }

    pub fn content(&self) -> Option<&Content> {
        self.content.as_ref()
    }
}

impl Clipboard for MemoryClipboard {
    fn change_count(&self) -> u64 {
        self.count
    }
    fn read(&self) -> Option<Content> {
        self.content.clone()
    }
    fn write(&mut self, content: &Content) -> Result<(), String> {
        self.set_locally(content.clone());
        Ok(())
    }
}

/// One side's clipboard sharing.
#[derive(Debug, Default)]
pub struct Sync {
    seen_count: Option<u64>,
    /// The last content exchanged in either direction.
    last: Option<Content>,
}

impl Sync {
    /// Call regularly: a local change to send to the peer, if any. The
    /// first call only records the current state (what was on the
    /// clipboard before the session is not pushed to the peer).
    pub fn poll(&mut self, clipboard: &dyn Clipboard) -> Option<Content> {
        let count = clipboard.change_count();
        let first = self.seen_count.is_none();
        if self.seen_count == Some(count) {
            return None;
        }
        self.seen_count = Some(count);
        if first {
            return None;
        }
        let content = clipboard.read()?;
        if !content.fits() || self.last.as_ref() == Some(&content) {
            return None;
        }
        self.last = Some(content.clone());
        Some(content)
    }

    /// The peer's clipboard changed: write it here, without sending it back.
    pub fn apply(&mut self, clipboard: &mut dyn Clipboard, content: Content) -> Result<(), String> {
        if self.last.as_ref() == Some(&content) {
            return Ok(());
        }
        clipboard.write(&content)?;
        self.last = Some(content);
        self.seen_count = Some(clipboard.change_count());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_across_splits() {
        let contents = [
            Content::Text("héllo".into()),
            Content::Png(vec![0x89, b'P', b'N', b'G']),
            Content::Text(String::new()),
        ];
        let mut bytes = Vec::new();
        for content in &contents {
            content.encode(&mut bytes);
        }
        for split in 0..bytes.len() {
            let mut reader = Reader::default();
            let mut got = reader.push(&bytes[..split]).unwrap();
            got.extend(reader.push(&bytes[split..]).unwrap());
            assert_eq!(got, contents, "split at {split}");
        }
        assert!(Reader::default().push(&u32::MAX.to_le_bytes()).is_none());
    }

    #[test]
    fn changes_flow_both_ways_without_echo_or_pushing_the_old_clipboard() {
        let (mut host, mut viewer) = (MemoryClipboard::default(), MemoryClipboard::default());
        host.set_locally(Content::Text("before the session".into()));
        let (mut host_sync, mut viewer_sync) = (Sync::default(), Sync::default());
        assert_eq!(host_sync.poll(&host), None, "the old clipboard stays local");
        assert_eq!(viewer_sync.poll(&viewer), None);

        viewer.set_locally(Content::Text("copied on the viewer".into()));
        let sent = viewer_sync.poll(&viewer).expect("a local copy is sent");
        host_sync.apply(&mut host, sent.clone()).unwrap();
        assert_eq!(host.content(), Some(&sent));
        assert_eq!(host_sync.poll(&host), None, "not echoed back");
        assert_eq!(viewer_sync.poll(&viewer), None);

        host.set_locally(Content::Png(vec![1, 2, 3]));
        let back = host_sync.poll(&host).expect("the host's copy is sent");
        viewer_sync.apply(&mut viewer, back.clone()).unwrap();
        assert_eq!(viewer.content(), Some(&back));
        assert_eq!(viewer_sync.poll(&viewer), None);

        // Copying the same thing again changes the counter, not the content.
        host.set_locally(Content::Png(vec![1, 2, 3]));
        assert_eq!(host_sync.poll(&host), None);
    }

    #[test]
    fn oversized_text_is_not_sent() {
        let mut clipboard = MemoryClipboard::default();
        let mut sync = Sync::default();
        sync.poll(&clipboard);
        clipboard.set_locally(Content::Text("x".repeat(MAX_TEXT + 1)));
        assert_eq!(sync.poll(&clipboard), None);
    }
}
