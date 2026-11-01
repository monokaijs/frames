use anyhow::{Context, Result};
use frames_core::{media::RawFrame, model::WindowId};
use image::{DynamicImage, ImageFormat, imageops::FilterType};
use std::{io::Cursor, sync::Arc, time::Duration};

pub const MAX_PREVIEW_BYTES: usize = 512 * 1024;

pub fn encode_frame(frame: &RawFrame) -> Result<Vec<u8>> {
    let frame = frame.cpu_copy()?;
    anyhow::ensure!(frame.validate(), "invalid preview frame");
    // Resize directly from the source so caching never retains a full desktop.
    let mut rgba = frame.bgra.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    let pixels = image::RgbaImage::from_raw(frame.width, frame.height, rgba)
        .context("invalid preview pixels")?;
    encode(&DynamicImage::ImageRgba8(pixels).thumbnail(480, 270))
}

fn encode(image: &DynamicImage) -> Result<Vec<u8>> {
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Png)?;
    let bytes = output.into_inner();
    anyhow::ensure!(bytes.len() <= MAX_PREVIEW_BYTES, "preview too large");
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    anyhow::ensure!(bytes.len() <= MAX_PREVIEW_BYTES, "preview too large");
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(480);
    limits.max_image_height = Some(270);
    limits.max_alloc = Some(4 * 1024 * 1024);
    reader.limits(limits);
    Ok(reader.decode()?)
}

pub fn save(device_id: &str, bytes: &[u8], trusted: bool) -> Result<()> {
    let image = decode(bytes)?;
    // Destroy small details before blurring; untrusted cards never load the
    // original pixels, even after a previously trusted device is revoked.
    let blurred = image
        .resize_exact(24, 14, FilterType::Triangle)
        .resize_exact(image.width(), image.height(), FilterType::Triangle)
        .blur(8.0);
    crate::connection_history::write_atomic(
        crate::connection_history::preview_path(device_id, true)?,
        &encode(&blurred)?,
    )?;
    if trusted {
        crate::connection_history::write_atomic(
            crate::connection_history::preview_path(device_id, false)?,
            bytes,
        )?;
    }
    Ok(())
}

pub async fn capture(
    desktop: Arc<dyn frames_native::Desktop>,
    window: Option<WindowId>,
) -> Result<Vec<u8>> {
    let source = desktop.clone();
    if let Some(bytes) = tokio::task::spawn_blocking(move || source.preview_image(window)).await?? {
        decode(&bytes)?;
        return Ok(bytes);
    }
    let mut capture = tokio::task::spawn_blocking(move || -> Result<_> {
        desktop.check_health()?;
        desktop.preview_capture(window)
    })
    .await??;
    let frame = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(frame) = capture.frames.borrow_and_update().clone() {
                return Ok::<_, anyhow::Error>(frame);
            }
            capture
                .frames
                .changed()
                .await
                .context("preview capture ended")?;
        }
    })
    .await??;
    drop(capture);
    tokio::task::spawn_blocking(move || encode_frame(&frame)).await?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thumbnails_are_bounded_and_oversized_inputs_are_rejected() {
        let frame = RawFrame {
            width: 800,
            height: 600,
            bgra: vec![255; 800 * 600 * 4].into(),
            captured_at: std::time::Instant::now(),
            frame_number: 1,
            media_generation: 1,
            #[cfg(windows)]
            gpu: None,
        };
        let thumbnail = decode(&encode_frame(&frame).unwrap()).unwrap();
        assert_eq!((thumbnail.width(), thumbnail.height()), (360, 270));
        assert!(decode(&vec![0; MAX_PREVIEW_BYTES + 1]).is_err());
        assert!(decode(b"not a png").is_err());
    }
}
