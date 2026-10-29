use sha2::{Digest, Sha256};
#[derive(Debug, Clone)]
pub enum Content {
    Text(String),
    Png(Vec<u8>),
}
impl Content {
    fn digest(&self) -> Vec<u8> {
        let mut hash = Sha256::new();
        match self {
            Self::Text(text) => {
                hash.update(b"text\0");
                hash.update(text.as_bytes());
            }
            Self::Png(bytes) => {
                hash.update(b"png\0");
                hash.update(bytes);
            }
        }
        hash.finalize().to_vec()
    }
}
/// Lamport order makes simultaneous copies converge without bouncing changes.
#[derive(Default, Clone)]
pub struct ClipboardState {
    counter: u64,
    stamp: (u64, String),
    content: Vec<u8>,
}
impl ClipboardState {
    pub fn local(&mut self, origin: &str, content: &Content) -> Option<u64> {
        let digest = content.digest();
        if digest == self.content {
            return None;
        }
        self.counter = self.counter.saturating_add(1);
        self.stamp = (self.counter, origin.into());
        self.content = digest;
        Some(self.counter)
    }
    pub fn remote(&mut self, origin: &str, revision: u64, content: &Content) -> bool {
        self.counter = self.counter.max(revision);
        let stamp = (revision, origin.to_owned());
        if stamp <= self.stamp {
            return false;
        }
        self.stamp = stamp;
        let digest = content.digest();
        let changed = digest != self.content;
        self.content = digest;
        changed
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simultaneous_copies_converge_without_echo() {
        let mut a = ClipboardState::default();
        let mut b = ClipboardState::default();
        let x = Content::Text("a".into());
        let y = Content::Text("b".into());
        let ax = a.local("a", &x).unwrap();
        let by = b.local("b", &y).unwrap();
        assert!(a.remote("b", by, &y));
        assert!(!b.remote("a", ax, &x));
        assert!(a.local("a", &y).is_none());
        assert!(b.local("b", &y).is_none());
    }
    #[test]
    fn repeated_image_after_text_is_a_new_copy() {
        let mut state = ClipboardState::default();
        let image = Content::Png(vec![1, 2, 3]);
        assert!(state.local("a", &image).is_some());
        state.local("a", &Content::Text("text".into()));
        assert!(state.local("a", &image).is_some());
    }
}
