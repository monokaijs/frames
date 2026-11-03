//! Length-prefixed frames on a reliable stream.
//!
//! Wire layout, little endian: `channel: u16`, `length: u32`, `payload`.
//! The decoder is incremental (feed it whatever the stream returns) and
//! bounded: a length above the configured limit is an error before any
//! payload is buffered, so a peer cannot make us allocate on its behalf.

use crate::channel::ChannelId;
use std::fmt;

pub const HEADER_BYTES: usize = 6;
/// Default per-frame limit. Bulk services (file transfer) send many frames
/// rather than one large one.
pub const DEFAULT_MAX_FRAME: u32 = 1 << 20;

#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub channel: ChannelId,
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnvelopeError {
    /// The declared payload length exceeds the limit.
    TooLarge { length: u32, limit: u32 },
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { length, limit } => {
                write!(f, "frame of {length} bytes exceeds the {limit}-byte limit")
            }
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// Appends one frame to `out`.
pub fn encode(
    channel: ChannelId,
    payload: &[u8],
    limit: u32,
    out: &mut Vec<u8>,
) -> Result<(), EnvelopeError> {
    let length = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    if length > limit {
        return Err(EnvelopeError::TooLarge { length, limit });
    }
    out.reserve(HEADER_BYTES + payload.len());
    out.extend_from_slice(&channel.0.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

/// Incremental decoder for one stream.
pub struct Decoder {
    buffer: Vec<u8>,
    limit: u32,
    failure: Option<EnvelopeError>,
}

impl Decoder {
    pub fn new(limit: u32) -> Self {
        Self {
            buffer: Vec::new(),
            limit,
            failure: None,
        }
    }

    /// Appends bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) {
        if self.failure.is_none() {
            self.buffer.extend_from_slice(bytes);
        }
    }

    /// Returns the next complete frame, `Ok(None)` if more bytes are needed.
    /// After an error the stream is unusable and every call fails.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, EnvelopeError> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        if self.buffer.len() < HEADER_BYTES {
            return Ok(None);
        }
        let channel = ChannelId(u16::from_le_bytes([self.buffer[0], self.buffer[1]]));
        let length = u32::from_le_bytes([
            self.buffer[2],
            self.buffer[3],
            self.buffer[4],
            self.buffer[5],
        ]);
        if length > self.limit {
            let failure = EnvelopeError::TooLarge {
                length,
                limit: self.limit,
            };
            self.failure = Some(failure);
            self.buffer = Vec::new();
            return Err(failure);
        }
        let end = HEADER_BYTES + length as usize;
        if self.buffer.len() < end {
            return Ok(None);
        }
        let payload = self.buffer[HEADER_BYTES..end].to_vec();
        self.buffer.drain(..end);
        Ok(Some(Frame { channel, payload }))
    }

    /// Bytes received but not yet returned as a frame.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_survive_arbitrary_stream_splits() {
        let mut stream = Vec::new();
        encode(ChannelId::CONTROL, b"hello", DEFAULT_MAX_FRAME, &mut stream).unwrap();
        encode(ChannelId(0x101), b"", DEFAULT_MAX_FRAME, &mut stream).unwrap();
        encode(ChannelId::INPUT, &[7; 300], DEFAULT_MAX_FRAME, &mut stream).unwrap();
        for split in 1..stream.len() {
            let mut decoder = Decoder::new(DEFAULT_MAX_FRAME);
            let mut frames = Vec::new();
            for chunk in stream.chunks(split) {
                decoder.push(chunk);
                while let Some(frame) = decoder.next_frame().unwrap() {
                    frames.push(frame);
                }
            }
            assert_eq!(frames.len(), 3, "split {split}");
            assert_eq!(frames[0].payload, b"hello");
            assert_eq!(frames[1].channel, ChannelId(0x101));
            assert!(frames[1].payload.is_empty());
            assert_eq!(frames[2].payload, vec![7; 300]);
            assert_eq!(decoder.buffered(), 0);
        }
    }

    #[test]
    fn oversized_frames_fail_before_buffering() {
        assert_eq!(
            encode(ChannelId::CONTROL, &[0; 11], 10, &mut Vec::new()),
            Err(EnvelopeError::TooLarge {
                length: 11,
                limit: 10
            })
        );
        let mut decoder = Decoder::new(10);
        decoder.push(&[0, 0, 0xFF, 0xFF, 0xFF, 0x7F]);
        assert!(decoder.next_frame().is_err());
        decoder.push(&[0; 64]);
        assert_eq!(decoder.buffered(), 0, "a failed stream buffers nothing");
        assert!(decoder.next_frame().is_err(), "and keeps failing");
    }
}
