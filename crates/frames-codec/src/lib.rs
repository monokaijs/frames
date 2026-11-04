//! Codec backends for the v2 media engine. Each implements the
//! `frames_media::api` encoder or decoder traits.
//!
//! - [`nvenc`] (Windows): NVENC driven directly with D3D11 texture input,
//!   slice output, reference invalidation, and intra refresh.
//! - [`ffmpeg`] (feature `ffmpeg`): decoding through the v1 FFmpeg runtime,
//!   D3D11VA on Windows.
//! - [`d3d11`] (Windows): the texture surface type shared with capture and
//!   presentation, and adapter selection.

#[cfg(windows)]
pub mod d3d11;
#[cfg(feature = "ffmpeg")]
pub mod ffmpeg;
#[cfg(windows)]
pub mod nvenc;
