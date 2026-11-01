//! Private helper media framing. Control messages stay on their own pipe.
use std::io::{self, Read, Write};
pub const ENCODED_METADATA_BYTES: usize = 37;

pub fn write_video(
    writer: &mut impl Write,
    window: frames_core::model::WindowId,
    frame: &frames_core::media::RawFrame,
) -> io::Result<()> {
    if !frame.validate() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid video frame",
        ));
    }
    let mut header = [0u8; 48];
    header[..4].copy_from_slice(b"FRV2");
    header[4..8].copy_from_slice(&1u32.to_le_bytes());
    header[8..16].copy_from_slice(&window.id.to_le_bytes());
    header[16..20].copy_from_slice(&window.generation.to_le_bytes());
    header[20..24].copy_from_slice(&frame.width.to_le_bytes());
    header[24..28].copy_from_slice(&frame.height.to_le_bytes());
    header[32..40].copy_from_slice(&(frame.bgra.len() as u64).to_le_bytes());
    header[28..32].copy_from_slice(&frame.media_generation.to_le_bytes());
    header[40..48].copy_from_slice(&frame.frame_number.to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(&frame.bgra)
}

#[derive(Debug)]
pub struct Packet {
    pub kind: u32,
    pub handle: u64,
    pub width: u32,
    pub height: u32,
    pub number: u64,
    pub captured_ns: u64,
    pub bytes: Vec<u8>,
}

pub fn read_packet(reader: &mut impl Read) -> io::Result<Packet> {
    let mut header = [0u8; 32];
    reader.read_exact(&mut header)?;
    if &header[..4] != b"FRM1" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid media header",
        ));
    }
    let kind = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let handle = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let width = u32::from_le_bytes(header[16..20].try_into().unwrap());
    let height = u32::from_le_bytes(header[20..24].try_into().unwrap());
    let size = u64::from_le_bytes(header[24..32].try_into().unwrap());
    let valid = match kind {
        1 | 4 => {
            width > 0
                && height > 0
                && width <= 8192
                && height <= 8192
                && size == u64::from(width) * u64::from(height) * 4 + if kind == 4 { 16 } else { 0 }
        }
        3 => {
            width > 0
                && width <= 8192
                && height > 0
                && height <= 8192
                && size > ENCODED_METADATA_BYTES as u64
                && size <= (frames_core::media::MAX_FRAME_BYTES + ENCODED_METADATA_BYTES) as u64
        }
        2 => width == 0 && height == 0 && size > 0 && size <= 4 * 1024 * 1024,
        _ => false,
    };
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid media dimensions or length",
        ));
    }
    let (number, captured_ns) = if kind == 4 {
        let mut metadata = [0u8; 16];
        reader.read_exact(&mut metadata)?;
        (
            u64::from_le_bytes(metadata[..8].try_into().unwrap()),
            u64::from_le_bytes(metadata[8..].try_into().unwrap()),
        )
    } else {
        (0, 0)
    };
    let mut bytes = vec![0; size as usize - if kind == 4 { 16 } else { 0 }];
    reader.read_exact(&mut bytes)?;
    Ok(Packet {
        kind,
        handle,
        width,
        height,
        number,
        captured_ns,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn client_video_uses_binary_identity_and_pixels() {
        let window = frames_core::model::WindowId {
            id: 42,
            generation: 7,
        };
        let mut bytes = Vec::new();
        let mut frame = frames_core::media::RawFrame {
            media_generation: 0,
            #[cfg(windows)]
            gpu: None,
            captured_at: std::time::Instant::now(),
            frame_number: 0,
            width: 2,
            height: 1,
            bgra: vec![9; 8].into(),
        };
        write_video(&mut bytes, window, &frame).unwrap();
        assert_eq!(&bytes[..4], b"FRV2");
        assert_eq!(u64::from_le_bytes(bytes[8..16].try_into().unwrap()), 42);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 7);
        assert_eq!(u64::from_le_bytes(bytes[32..40].try_into().unwrap()), 8);
        assert_eq!(&bytes[48..], &[9; 8]);
        frame.width = 3;
        assert!(write_video(&mut bytes, window, &frame).is_err());
    }
    fn packet(kind: u32, w: u32, h: u32, size: u64) -> Vec<u8> {
        let mut b = b"FRM1".to_vec();
        b.extend(kind.to_le_bytes());
        b.extend(42u64.to_le_bytes());
        b.extend(w.to_le_bytes());
        b.extend(h.to_le_bytes());
        b.extend(size.to_le_bytes());
        b
    }
    #[test]
    fn reads_framed_video_and_audio_without_base64() {
        let mut b = packet(1, 2, 1, 8);
        b.extend([1, 2, 3, 4, 5, 6, 7, 8]);
        b.extend(packet(2, 0, 0, 4));
        b.extend([9, 10, 11, 12]);
        let mut r = std::io::Cursor::new(b);
        let video = read_packet(&mut r).unwrap();
        assert_eq!(
            (video.kind, video.handle, video.width, video.height),
            (1, 42, 2, 1)
        );
        assert_eq!(video.bytes, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(read_packet(&mut r).unwrap().bytes, [9, 10, 11, 12]);
    }
    #[test]
    fn fallback_capture_preserves_source_identity_and_timestamp_without_copying_the_pixels() {
        let mut bytes = packet(4, 2, 1, 24);
        bytes.extend(17u64.to_le_bytes());
        bytes.extend(123456u64.to_le_bytes());
        bytes.extend([37; 8]);
        let p = read_packet(&mut std::io::Cursor::new(bytes)).unwrap();
        assert_eq!((p.number, p.captured_ns), (17, 123456));
        assert_eq!(p.bytes, [37; 8]);
        assert!(read_packet(&mut std::io::Cursor::new(packet(4, 2, 1, 8))).is_err());
    }
    #[test]
    fn rejects_bad_lengths_and_dimensions_before_allocating() {
        for b in [
            packet(1, 2, 1, 9),
            packet(1, 8193, 1, 32772),
            packet(1, 0, 1, 0),
            packet(2, 0, 0, u64::MAX),
            packet(3, 0, 0, 1),
        ] {
            assert_eq!(
                read_packet(&mut std::io::Cursor::new(b))
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
        }
    }
    #[test]
    fn rejects_truncated_payload() {
        let mut b = packet(1, 2, 1, 8);
        b.extend([1, 2]);
        assert_eq!(
            read_packet(&mut std::io::Cursor::new(b))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}

#[cfg(target_os = "macos")]
pub fn write_encoded(
    writer: &mut impl Write,
    window: frames_core::model::WindowId,
    generation: u32,
    codec: frames_core::codec::Codec,
    width: u32,
    height: u32,
    packet: &frames_core::codec::EncodedPacket,
) -> io::Result<()> {
    let kind = match codec {
        frames_core::codec::Codec::H264 => 2u32,
        frames_core::codec::Codec::Hevc => 3,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported native codec",
            ));
        }
    };
    let mut header = [0u8; 48];
    header[..4].copy_from_slice(b"FRV2");
    header[4..8].copy_from_slice(&kind.to_le_bytes());
    header[8..16].copy_from_slice(&window.id.to_le_bytes());
    header[16..20].copy_from_slice(&window.generation.to_le_bytes());
    header[20..24].copy_from_slice(&width.to_le_bytes());
    header[24..28].copy_from_slice(&height.to_le_bytes());
    header[28..32].copy_from_slice(&generation.to_le_bytes());
    header[32..40].copy_from_slice(&(packet.bytes.len() as u64).to_le_bytes());
    header[40..48].copy_from_slice(&packet.number.to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(&packet.bytes)
}
