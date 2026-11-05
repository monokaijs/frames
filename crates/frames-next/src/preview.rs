//! Preview images for the launcher's device cards: a viewer asks a host it
//! is trusted by for a small picture of its screen (a hello with `preview`
//! set), and the host answers with one PNG on a `PREVIEW` stream. Only
//! trusted devices get one; nobody is asked (`crate::admit`).

use anyhow::{Context, Result};
use std::time::Duration;

/// Thumbnails are at most this wide (and keep the screen's shape).
const MAX_WIDTH: u32 = 480;
/// The most a preview may weigh on the wire.
const MAX_BYTES: usize = 2 * 1024 * 1024;

/// Packed BGRA rows of `width` x `height`, scaled to at most `MAX_WIDTH`
/// wide, as a PNG.
pub fn thumbnail(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    anyhow::ensure!(
        width > 0 && height > 0 && bgra.len() >= (width * height * 4) as usize,
        "no picture"
    );
    let rgba: Vec<u8> = bgra
        .chunks_exact(4)
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect();
    let image = image::RgbaImage::from_raw(width, height, rgba).context("picture size")?;
    let scale = (f64::from(MAX_WIDTH) / f64::from(width)).min(1.0);
    let (w, h) = (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    );
    let small = image::imageops::resize(&image, w, h, image::imageops::FilterType::Triangle);
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(small)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)?;
    Ok(png)
}

/// Host: sends a preview of this screen to a trusted viewer.
pub async fn serve(session: frames_net::Session) -> Result<()> {
    let png = tokio::task::spawn_blocking(|| {
        let (bgra, width, height) = screenshot()?;
        thumbnail(&bgra, width, height)
    })
    .await??;
    let (mut send, _recv) = crate::streams::open(&session, crate::streams::PREVIEW).await?;
    send.write_all(&png).await?;
    send.finish()?;
    // Let the viewer read it before the session goes.
    let _ = tokio::time::timeout(Duration::from_secs(5), session.closed()).await;
    Ok(())
}

/// Viewer: reads the preview from a host's `PREVIEW` stream.
pub async fn receive(session: &frames_net::Session) -> Result<Vec<u8>> {
    let (_send, mut recv) = session.accept_channel().await?;
    let mut kind = [0u8; 1];
    recv.read_exact(&mut kind).await?;
    anyhow::ensure!(kind[0] == crate::streams::PREVIEW, "not a preview");
    Ok(recv.read_to_end(MAX_BYTES).await?)
}

#[cfg(windows)]
fn screenshot() -> Result<(Vec<u8>, u32, u32)> {
    crate::windows_host::screenshot()
}

#[cfg(target_os = "macos")]
fn screenshot() -> Result<(Vec<u8>, u32, u32)> {
    crate::mac_host::screenshot()
}

#[cfg(not(any(windows, target_os = "macos")))]
fn screenshot() -> Result<(Vec<u8>, u32, u32)> {
    anyhow::bail!("no screen capture on this platform")
}

#[cfg(test)]
mod tests {
    #[test]
    fn thumbnails_are_small_pngs_of_the_same_shape() {
        let (width, height) = (1920u32, 1080u32);
        let bgra = vec![40u8; (width * height * 4) as usize];
        let png = super::thumbnail(&bgra, width, height).unwrap();
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (480, 270));
        assert!(super::thumbnail(&[], 0, 0).is_err());
    }
}
