use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Child,
    sync::mpsc,
};
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PcmFormat {
    pub rate: u32,
    pub channels: u16,
}
pub struct Encoder {
    pub child: Child,
    pub pages: mpsc::Receiver<Vec<u8>>,
    pub writer: tokio::task::JoinHandle<()>,
}
pub async fn encode(
    format: PcmFormat,
    mut input: mpsc::Receiver<Vec<u8>>,
) -> anyhow::Result<Encoder> {
    anyhow::ensure!(
        (8000..=192000).contains(&format.rate) && (1..=8).contains(&format.channels),
        "invalid PCM audio format"
    );
    let mut child = crate::codec::command()
        .args([
            "-v",
            "error",
            "-f",
            "f32le",
            "-ar",
            &format.rate.to_string(),
            "-ac",
            &format.channels.to_string(),
            "-i",
            "pipe:0",
            "-vn",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-c:a",
            "libopus",
            "-application",
            "lowdelay",
            "-frame_duration",
            "20",
            "-b:a",
            "128k",
            "-page_duration",
            "20000",
            "-flush_packets",
            "1",
            "-f",
            "opus",
            "pipe:1",
        ])
        .spawn()?;
    crate::process::track(&child)?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let writer = tokio::spawn(async move {
        while let Some(bytes) = input.recv().await {
            if stdin.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let (tx, pages) = mpsc::channel(4);
    tokio::spawn(async move {
        let mut headers = Vec::new();
        loop {
            let mut header = [0; 27];
            if stdout.read_exact(&mut header).await.is_err() || &header[..4] != b"OggS" {
                break;
            }
            let mut segments = vec![0; header[26] as usize];
            if stdout.read_exact(&mut segments).await.is_err() {
                break;
            }
            let count: usize = segments.iter().map(|&b| b as usize).sum();
            let mut payload = vec![0; count];
            if stdout.read_exact(&mut payload).await.is_err() {
                break;
            }
            let mut page = header.to_vec();
            page.extend(segments);
            page.extend(&payload);
            if payload.starts_with(b"OpusHead") || payload.starts_with(b"OpusTags") {
                headers.extend(page);
                continue;
            }
            if !headers.is_empty() {
                headers.extend(page);
                page = std::mem::take(&mut headers);
            }
            if tx.send(page).await.is_err() {
                break;
            }
        }
    });
    Ok(Encoder {
        child,
        pages,
        writer,
    })
}
pub struct Decoder {
    pub child: Child,
    pub input: mpsc::Sender<Vec<u8>>,
    pub pcm: mpsc::Receiver<Vec<u8>>,
}
pub async fn decode(format: PcmFormat) -> anyhow::Result<Decoder> {
    let mut child = crate::codec::command()
        .args([
            "-v",
            "error",
            "-probesize",
            "32",
            "-analyzeduration",
            "0",
            "-f",
            "ogg",
            "-i",
            "pipe:0",
            "-vn",
            "-ar",
            &format.rate.to_string(),
            "-ac",
            &format.channels.to_string(),
            "-f",
            "f32le",
            "-flush_packets",
            "1",
            "pipe:1",
        ])
        .spawn()?;
    crate::process::track(&child)?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (input, mut incoming) = mpsc::channel::<Vec<u8>>(8);
    let (tx, pcm) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(bytes) = incoming.recv().await {
            if stdin.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let count = format.rate as usize * format.channels as usize * 4 / 50;
        loop {
            let mut bytes = vec![0; count];
            if stdout.read_exact(&mut bytes).await.is_err() || tx.send(bytes).await.is_err() {
                break;
            }
        }
    });
    Ok(Decoder { child, input, pcm })
}
