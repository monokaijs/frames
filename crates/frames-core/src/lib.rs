pub mod audio;
pub mod clipboard;
pub mod codec;
pub mod device_bridge;
pub mod discovery;
pub mod ffi;
pub mod input;
pub mod media;
pub mod model;
mod process;
pub mod protocol;
pub mod scheduler;
pub mod transfer;
pub mod transport;
pub mod trust;

pub const PROTOCOL_VERSION: u16 = 2;
pub const ALPN: &[u8] = b"frames/2";
