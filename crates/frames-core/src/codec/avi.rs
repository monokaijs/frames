//! Read length-delimited FFmpeg AVI packets without waiting for another picture.
//! This is a private single-video-stream pipe, never a network container.
use crate::media::MAX_FRAME_BYTES;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};

pub async fn header(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<()> {
    let mut header = [0; 12];
    reader.read_exact(&mut header).await?;
    if &header[..4] != b"RIFF" || &header[8..] != b"AVI " {
        return Err(invalid("invalid encoder AVI header"));
    }
    Ok(())
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub async fn packet(reader: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    loop {
        let mut header = [0; 8];
        reader.read_exact(&mut header).await?;
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        if &header[..4] == b"RIFF" || &header[..4] == b"LIST" {
            let mut kind = [0; 4];
            reader.read_exact(&mut kind).await?;
            if (&header[..4] == b"RIFF" && &kind == b"AVIX")
                || (&header[..4] == b"LIST" && matches!(&kind, b"movi" | b"rec "))
            {
                continue;
            }
            if !(4..=MAX_FRAME_BYTES).contains(&size) {
                return Err(invalid("invalid AVI list length"));
            }
            skip(reader, size - 4 + (size & 1)).await?;
        } else if matches!(&header[..4], b"00dc" | b"00db") {
            // AVI can contain empty timing chunks; they are not pictures.
            if size == 0 {
                continue;
            }
            if size > MAX_FRAME_BYTES {
                return Err(invalid("invalid AVI picture length"));
            }
            let mut bytes = vec![0; size];
            reader.read_exact(&mut bytes).await?;
            if size & 1 != 0 {
                reader.read_u8().await?;
            }
            if !bytes.starts_with(&[0, 0, 1]) && !bytes.starts_with(&[0, 0, 0, 1]) {
                return Err(invalid("encoder picture is not Annex B"));
            }
            return Ok(bytes);
        } else {
            if size > MAX_FRAME_BYTES {
                return Err(invalid("invalid AVI metadata length"));
            }
            skip(reader, size + (size & 1)).await?;
        }
    }
}
async fn skip(reader: &mut (impl AsyncRead + Unpin), mut count: usize) -> io::Result<()> {
    let mut scratch = [0; 4096];
    while count > 0 {
        let n = count.min(scratch.len());
        reader.read_exact(&mut scratch[..n]).await?;
        count -= n;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    #[tokio::test]
    async fn short_reads_deliver_an_odd_sized_picture_without_eof_or_another_picture() {
        let (mut writer, mut reader) = tokio::io::duplex(8);
        let task = tokio::spawn(async move {
            writer.write_all(b"RIFF\xff\xff\xff\xffAVI LIST\x04\x00\x00\x00hdrlLIST\xff\xff\xff\xffmovi00dc\x05\x00\x00\x00\x00\x00\x01\x65\x01\x00").await.unwrap();
            writer
        });
        header(&mut reader).await.unwrap();
        assert_eq!(packet(&mut reader).await.unwrap(), [0, 0, 1, 0x65, 1]);
        let _keep_open = task.await.unwrap();
    }
    #[tokio::test]
    async fn invalid_sizes_are_rejected_before_allocation() {
        let mut reader = &b"00dc\xff\xff\xff\xff"[..];
        assert_eq!(
            packet(&mut reader).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
