//! Host cursor shape and visibility, sent to the viewer on a reliable stream.
//!
//! The viewer shows the host's cursor shape with its own OS cursor, so the
//! pointer moves with zero video latency; the video never contains it.
//!
//! Record: 4-byte little-endian length `n`, then `n` bytes: a kind byte and
//! its fields. Shapes are premultiplied BGRA, top row first.

/// Largest record accepted (a 256x256 cursor and its header).
pub const MAX_RECORD: usize = 16 + 256 * 256 * 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorMessage {
    Shape {
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        bgra: Vec<u8>,
    },
    /// Shown or hidden (games and video players hide it).
    Visible(bool),
}

const KIND_SHAPE: u8 = 1;
const KIND_VISIBLE: u8 = 2;

impl CursorMessage {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend([0; 4]);
        match self {
            Self::Shape {
                width,
                height,
                hotspot_x,
                hotspot_y,
                bgra,
            } => {
                out.push(KIND_SHAPE);
                for value in [width, height, hotspot_x, hotspot_y] {
                    out.extend(value.to_le_bytes());
                }
                out.extend(bgra);
            }
            Self::Visible(visible) => out.extend([KIND_VISIBLE, u8::from(*visible)]),
        }
        let length = (out.len() - start - 4) as u32;
        out[start..start + 4].copy_from_slice(&length.to_le_bytes());
    }
}

/// Splits the cursor stream into messages; malformed records are skipped and
/// an oversized length ends the stream (`None`).
#[derive(Debug, Default)]
pub struct CursorReader {
    buffer: Vec<u8>,
}

impl CursorReader {
    pub fn push(&mut self, bytes: &[u8]) -> Option<Vec<CursorMessage>> {
        self.buffer.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut at = 0;
        while self.buffer.len() >= at + 4 {
            let length = u32::from_le_bytes(self.buffer[at..at + 4].try_into().unwrap()) as usize;
            if length > MAX_RECORD {
                return None;
            }
            let end = at + 4 + length;
            if end > self.buffer.len() {
                break;
            }
            if let Some(message) = decode(&self.buffer[at + 4..end]) {
                out.push(message);
            }
            at = end;
        }
        self.buffer.drain(..at);
        Some(out)
    }
}

fn decode(record: &[u8]) -> Option<CursorMessage> {
    let (&kind, body) = record.split_first()?;
    match kind {
        KIND_SHAPE if body.len() >= 8 => {
            let field = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
            let (width, height) = (field(0), field(2));
            let (hotspot_x, hotspot_y) = (field(4), field(6));
            let pixels = &body[8..];
            let valid = width > 0
                && height > 0
                && width <= 256
                && height <= 256
                && hotspot_x < width
                && hotspot_y < height
                && pixels.len() == usize::from(width) * usize::from(height) * 4;
            valid.then(|| CursorMessage::Shape {
                width,
                height,
                hotspot_x,
                hotspot_y,
                bgra: pixels.to_vec(),
            })
        }
        KIND_VISIBLE if body.len() == 1 && body[0] <= 1 => {
            Some(CursorMessage::Visible(body[0] == 1))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_across_splits_and_bad_shapes_are_dropped() {
        let shape = CursorMessage::Shape {
            width: 2,
            height: 3,
            hotspot_x: 1,
            hotspot_y: 2,
            bgra: (0..24).collect(),
        };
        let mut bytes = Vec::new();
        shape.encode(&mut bytes);
        CursorMessage::Visible(false).encode(&mut bytes);
        // Hotspot outside the shape: skipped, the stream continues.
        CursorMessage::Shape {
            width: 1,
            height: 1,
            hotspot_x: 1,
            hotspot_y: 0,
            bgra: vec![0; 4],
        }
        .encode(&mut bytes);
        CursorMessage::Visible(true).encode(&mut bytes);
        let expected = [
            shape,
            CursorMessage::Visible(false),
            CursorMessage::Visible(true),
        ];
        for split in 0..bytes.len() {
            let mut reader = CursorReader::default();
            let mut got = reader.push(&bytes[..split]).unwrap();
            got.extend(reader.push(&bytes[split..]).unwrap());
            assert_eq!(got, expected, "split at {split}");
        }
    }

    #[test]
    fn an_oversized_length_ends_the_stream() {
        let mut reader = CursorReader::default();
        assert_eq!(reader.push(&u32::MAX.to_le_bytes()), None);
    }
}
