mod capture;
mod client;
mod drag;
mod files;
mod host;
pub mod launcher;
mod menus;
mod render;
mod secure;
mod session_control;
pub use client::run_client;
pub use host::Host;
fn decode_png(
    bytes: &[u8],
    max_dimension: u32,
    max_alloc: u64,
) -> anyhow::Result<image::DynamicImage> {
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(max_dimension);
    limits.max_image_height = Some(max_dimension);
    limits.max_alloc = Some(max_alloc);
    reader.limits(limits);
    Ok(reader.decode()?)
}
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}
