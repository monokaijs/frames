//! File transfer from viewer to host: one stream per file.
//!
//! 1. Sender → `{"name", "size", "sha256"}` (one JSON line).
//! 2. Receiver → `{"offset"}`: how much it already has from an interrupted
//!    attempt at the same file (matched by its SHA-256), or 0.
//! 3. Sender → the bytes from `offset`, then finishes the stream.
//! 4. Receiver checks the whole file's SHA-256, moves it to a name that does
//!    not exist yet (never overwriting), and → `{"saved"}` or `{"error"}`.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const MAX_LINE: usize = 4096;

#[derive(Debug, Serialize, Deserialize)]
struct Offer {
    name: String,
    size: u64,
    sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Resume {
    offset: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Done {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    saved: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

async fn write_line<T: Serialize>(send: &mut iroh::endpoint::SendStream, value: &T) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    send.write_all(&line).await?;
    Ok(())
}

async fn read_line<T: for<'a> Deserialize<'a>>(recv: &mut iroh::endpoint::RecvStream) -> Result<T> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        recv.read_exact(&mut byte)
            .await
            .context("the peer closed the transfer")?;
        if byte[0] == b'\n' {
            break;
        }
        line.push(byte[0]);
        ensure!(line.len() <= MAX_LINE, "transfer header too long");
    }
    Ok(serde_json::from_slice(&line)?)
}

pub async fn sha256_of(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Viewer: sends `path`; returns the name the host saved it under.
pub async fn send(session: &frames_net::Session, path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("the file needs a UTF-8 name")?
        .to_string();
    let size = tokio::fs::metadata(path).await?.len();
    let sha256 = sha256_of(path).await?;
    let (mut send, mut recv) = crate::streams::open(session, crate::streams::FILE).await?;
    write_line(&mut send, &Offer { name, size, sha256 }).await?;
    let resume: Resume = read_line(&mut recv).await?;
    ensure!(
        resume.offset <= size,
        "the host claims more than the file holds"
    );
    if resume.offset > 0 {
        println!(
            "resuming {} at {} of {size} bytes",
            path.display(),
            resume.offset
        );
    }
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(resume.offset)).await?;
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buffer).await?;
        if n == 0 {
            break;
        }
        send.write_all(&buffer[..n]).await?;
    }
    send.finish()?;
    let done: Done = read_line(&mut recv).await?;
    match (done.saved, done.error) {
        (Some(saved), _) => Ok(saved),
        (None, Some(error)) => bail!("the host rejected the file: {error}"),
        _ => bail!("the host did not confirm the file"),
    }
}

/// A file name the receiver may create: the last component only, without
/// characters that are unsafe on Windows or macOS.
pub fn safe_name(name: &str) -> Option<String> {
    let last = name.rsplit(['/', '\\']).next()?.trim();
    let cleaned: String = last
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_string();
    (!cleaned.is_empty() && cleaned.len() <= 255).then_some(cleaned)
}

/// `dir/name`, or `dir/name (2).ext`, ... : the first that does not exist.
pub fn unused_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    (2..)
        .map(|n| match extension {
            Some(extension) => dir.join(format!("{stem} ({n}).{extension}")),
            None => dir.join(format!("{stem} ({n})")),
        })
        .find(|path| !path.exists())
        .expect("some name is free")
}

/// Host: receives one file into `dir`.
pub async fn receive(
    (mut send, mut recv): crate::streams::Streams,
    dir: PathBuf,
) -> Result<PathBuf> {
    let offer: Offer = read_line(&mut recv).await?;
    let result = receive_offer(&offer, &mut send, &mut recv, &dir).await;
    let done = match &result {
        Ok(path) => Done {
            saved: Some(
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned(),
            ),
            error: None,
        },
        Err(error) => Done {
            saved: None,
            error: Some(format!("{error:#}")),
        },
    };
    let _ = write_line(&mut send, &done).await;
    let _ = send.finish();
    result
}

async fn receive_offer(
    offer: &Offer,
    send: &mut iroh::endpoint::SendStream,
    recv: &mut iroh::endpoint::RecvStream,
    dir: &Path,
) -> Result<PathBuf> {
    let name = safe_name(&offer.name).context("unusable file name")?;
    ensure!(
        offer.sha256.len() == 64 && offer.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "bad checksum"
    );
    tokio::fs::create_dir_all(dir).await?;
    // A partial file from an interrupted attempt at the same content.
    let part = dir.join(format!(".frames-{}.part", &offer.sha256[..32]));
    let have = tokio::fs::metadata(&part).await.map_or(0, |m| m.len());
    let offset = if have <= offer.size { have } else { 0 };
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(&part)
        .await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    write_line(send, &Resume { offset }).await?;
    let mut remaining = offer.size - offset;
    let mut buffer = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let want = buffer.len().min(remaining as usize);
        let Some(n) = recv.read(&mut buffer[..want]).await? else {
            bail!(
                "the sender stopped after {} of {} bytes",
                offer.size - remaining,
                offer.size
            );
        };
        file.write_all(&buffer[..n]).await?;
        remaining -= n as u64;
    }
    file.flush().await?;
    drop(file);
    let sha256 = sha256_of(&part).await?;
    if sha256 != offer.sha256 {
        let _ = tokio::fs::remove_file(&part).await;
        bail!("checksum mismatch");
    }
    let target = unused_path(dir, &name);
    tokio::fs::rename(&part, &target).await?;
    Ok(target)
}

/// Default receive directory: `~/Downloads/Frames`, as in v1.
pub fn default_receive_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.download_dir().map(Path::to_path_buf))
        .unwrap_or_else(std::env::temp_dir)
        .join("Frames")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_reduced_to_a_safe_last_component() {
        assert_eq!(safe_name("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(safe_name("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(
            safe_name("C:\\Users\\x\\a?b.txt").as_deref(),
            Some("a_b.txt")
        );
        assert_eq!(safe_name(".hidden").as_deref(), Some("hidden"));
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name(""), None);
    }

    #[test]
    fn existing_files_are_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unused_path(dir.path(), "a.txt"), dir.path().join("a.txt"));
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        assert_eq!(
            unused_path(dir.path(), "a.txt"),
            dir.path().join("a (2).txt")
        );
        std::fs::write(dir.path().join("a (2).txt"), b"x").unwrap();
        assert_eq!(
            unused_path(dir.path(), "a.txt"),
            dir.path().join("a (3).txt")
        );
        std::fs::write(dir.path().join("Makefile"), b"x").unwrap();
        assert_eq!(
            unused_path(dir.path(), "Makefile"),
            dir.path().join("Makefile (2)")
        );
    }
}
