use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub const CHUNK_BYTES: usize = 48 * 1024;
pub const MAX_TRANSFER_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
pub fn transfer_id(name: &str, hash: &str, size: u64) -> String {
    let digest = Sha256::digest(format!("frames-transfer-v1\0{name}\0{hash}\0{size}"));
    let mut bytes: [u8; 16] = digest[..16].try_into().unwrap();
    bytes[6] = (bytes[6] & 15) | 0x80;
    bytes[8] = (bytes[8] & 63) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransferMessage {
    Offer {
        id: String,
        name: String,
        size: u64,
        sha256: String,
    },
    Accept {
        id: String,
        offset: u64,
    },
    Chunk {
        id: String,
        offset: u64,
        data_base64: String,
    },
    Complete {
        id: String,
    },
    Finished {
        id: String,
        name: String,
    },
    Cancel {
        id: String,
        reason: String,
    },
}

pub fn safe_name(name: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 240
            && !name.contains(['/', '\\', ':', '\0'])
            && !name.ends_with(['.', ' ']),
        "unsafe transfer filename"
    );
    anyhow::ensure!(
        Path::new(name)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
            && name != "."
            && name != "..",
        "unsafe transfer path"
    );
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    anyhow::ensure!(
        !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            && !(stem.len() == 4
                && (stem.starts_with("COM") || stem.starts_with("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()),
        "reserved filename"
    );
    Ok(name)
}
pub async fn hash_file(path: &Path) -> anyhow::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; CHUNK_BYTES];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

struct Incoming {
    name: String,
    path: PathBuf,
    size: u64,
    hash: String,
    offset: u64,
    file: tokio::fs::File,
    // Hold the OS lock through checksum verification and publication as well.
    _lease: std::fs::File,
}
pub struct Receiver {
    root: PathBuf,
    incoming: HashMap<String, Incoming>,
    canceled: std::collections::HashSet<String>,
}
impl Receiver {
    pub fn received_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        safe_name(name)?;
        Ok(self.root.join(name))
    }
    pub async fn new(root: PathBuf) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self {
            root: tokio::fs::canonicalize(root).await?,
            incoming: HashMap::new(),
            canceled: std::collections::HashSet::new(),
        })
    }
    pub async fn offer(
        &mut self,
        id: &str,
        name: &str,
        size: u64,
        hash: &str,
    ) -> anyhow::Result<TransferMessage> {
        safe_name(name)?;
        self.canceled.remove(id);
        anyhow::ensure!(
            uuid::Uuid::parse_str(id).is_ok()
                && size <= MAX_TRANSFER_BYTES
                && hash.len() == 64
                && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid transfer offer"
        );
        anyhow::ensure!(
            self.incoming.len() < 8 && !self.incoming.contains_key(id),
            "too many transfers or repeated ID"
        );
        let path = self.root.join(format!(".{id}.partial"));
        if let Ok(meta) = tokio::fs::symlink_metadata(&path).await {
            anyhow::ensure!(
                !meta.file_type().is_symlink() && meta.is_file(),
                "unsafe partial file"
            );
        }
        let lock_path = self.root.join(format!(".{id}.lock"));
        if let Ok(meta) = std::fs::symlink_metadata(&lock_path) {
            anyhow::ensure!(
                !meta.file_type().is_symlink() && meta.is_file(),
                "unsafe transfer lock"
            );
        }
        let lease = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        fs2::FileExt::try_lock_exclusive(&lease)
            .map_err(|_| anyhow::anyhow!("this transfer is already active in another session"))?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .await?;
        let offset = file.metadata().await?.len();
        anyhow::ensure!(offset <= size, "partial file exceeds offered size");
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        self.incoming.insert(
            id.to_string(),
            Incoming {
                name: name.into(),
                path,
                size,
                hash: hash.into(),
                offset,
                file,
                _lease: lease,
            },
        );
        Ok(TransferMessage::Accept {
            id: id.into(),
            offset,
        })
    }
    pub async fn chunk(&mut self, id: &str, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        if self.canceled.contains(id) {
            return Ok(());
        }
        let transfer = self
            .incoming
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("unknown transfer"))?;
        anyhow::ensure!(
            data.len() <= CHUNK_BYTES
                && offset == transfer.offset
                && offset
                    .checked_add(data.len() as u64)
                    .is_some_and(|n| n <= transfer.size),
            "invalid transfer offset/length"
        );
        transfer.file.write_all(data).await?;
        transfer.offset += data.len() as u64;
        Ok(())
    }
    pub async fn finish(&mut self, id: &str) -> anyhow::Result<TransferMessage> {
        let t = self
            .incoming
            .remove(id)
            .ok_or_else(|| anyhow::anyhow!("unknown transfer"))?;
        anyhow::ensure!(t.offset == t.size, "incomplete transfer");
        t.file.sync_all().await?;
        drop(t.file);
        anyhow::ensure!(
            hash_file(&t.path).await?.eq_ignore_ascii_case(&t.hash),
            "transfer checksum mismatch"
        );
        // create_new + copy prevents overwrite and handles simultaneous same-name transfers.
        let mut index = 0u32;
        let (name, mut out) = loop {
            let name = if index == 0 {
                t.name.clone()
            } else {
                format!("{} ({index})", t.name)
            };
            match tokio::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(self.root.join(&name))
                .await
            {
                Ok(f) => break (name, f),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    index += 1;
                    anyhow::ensure!(index < 10000, "too many filename collisions");
                }
                Err(e) => return Err(e.into()),
            }
        };
        let mut source = tokio::fs::File::open(&t.path).await?;
        tokio::io::copy(&mut source, &mut out).await?;
        out.sync_all().await?;
        tokio::fs::remove_file(t.path).await?;
        Ok(TransferMessage::Finished {
            id: id.into(),
            name,
        })
    }
    pub async fn cancel(&mut self, id: &str) -> anyhow::Result<()> {
        if let Some(mut incoming) = self.incoming.remove(id) {
            incoming.file.flush().await?;
            self.canceled.insert(id.to_owned());
        }
        Ok(())
    }
    pub fn is_canceled(&self, id: &str) -> bool {
        self.canceled.contains(id)
    }
    pub fn active_ids(&self) -> Vec<String> {
        self.incoming.keys().cloned().collect()
    }
    pub fn progress(&self, id: &str) -> Option<(String, u64, u64)> {
        self.incoming
            .get(id)
            .map(|p| (p.name.clone(), p.offset, p.size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_sessions_cannot_write_the_same_partial() {
        let root = std::env::temp_dir().join(format!("frames-lock-{}", uuid::Uuid::new_v4()));
        let mut first = Receiver::new(root.clone()).await.unwrap();
        let mut second = Receiver::new(root.clone()).await.unwrap();
        let hash = hex::encode(Sha256::digest(b"abcdef"));
        let id = transfer_id("file.txt", &hash, 6);
        first.offer(&id, "file.txt", 6, &hash).await.unwrap();
        assert!(second.offer(&id, "file.txt", 6, &hash).await.is_err());
        first.chunk(&id, 0, b"abc").await.unwrap();
        first.cancel(&id).await.unwrap();
        assert!(matches!(
            second.offer(&id, "file.txt", 6, &hash).await.unwrap(),
            TransferMessage::Accept { offset: 3, .. }
        ));
        second.chunk(&id, 3, b"def").await.unwrap();
        second.finish(&id).await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn cancel_preserves_a_resumable_verified_prefix() {
        let root = std::env::temp_dir().join(format!("frames-resume-{}", uuid::Uuid::new_v4()));
        let mut receiver = Receiver::new(root.clone()).await.unwrap();
        let data = b"abcdef";
        let hash = hex::encode(Sha256::digest(data));
        let id = transfer_id("file.txt", &hash, 6);
        receiver.offer(&id, "file.txt", 6, &hash).await.unwrap();
        receiver.chunk(&id, 0, b"abc").await.unwrap();
        receiver.cancel(&id).await.unwrap();
        receiver.chunk(&id, 3, b"ignored").await.unwrap();
        let reply = receiver.offer(&id, "file.txt", 6, &hash).await.unwrap();
        assert!(matches!(reply, TransferMessage::Accept { offset: 3, .. }));
        receiver.chunk(&id, 3, b"def").await.unwrap();
        receiver.finish(&id).await.unwrap();
        assert_eq!(tokio::fs::read(root.join("file.txt")).await.unwrap(), data);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn rejects_traversal_streams_and_windows_devices() {
        for name in [
            "../secret",
            "..\\secret",
            "C:evil",
            "NUL.txt",
            "COM1",
            "foo:",
            "foo.",
            "..",
            "",
        ] {
            assert!(safe_name(name).is_err(), "{name}");
        }
        assert!(safe_name("báo cáo.txt").is_ok());
    }
    #[tokio::test]
    async fn transfer_checks_offset_integrity_and_never_overwrites() {
        let root = std::env::temp_dir().join(format!("frames-test-{}", uuid::Uuid::new_v4()));
        let mut rx = Receiver::new(root.clone()).await.unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let data = b"hello";
        let hash = hex::encode(Sha256::digest(data));
        rx.offer(&id, "test.txt", 5, &hash).await.unwrap();
        assert!(rx.chunk(&id, 1, data).await.is_err());
        rx.chunk(&id, 0, data).await.unwrap();
        tokio::fs::write(root.join("test.txt"), b"original")
            .await
            .unwrap();
        let result = rx.finish(&id).await.unwrap();
        assert!(matches!(result,TransferMessage::Finished{name,..} if name=="test.txt (1)"));
        assert_eq!(
            tokio::fs::read(root.join("test.txt")).await.unwrap(),
            b"original"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
