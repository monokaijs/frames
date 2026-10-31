pub mod audio;
pub mod clipboard;
pub mod codec;
pub mod device_bridge;
pub mod discovery;
pub mod ffi;
pub mod input;
pub mod media;
pub mod model;
pub mod protocol;
pub mod scheduler;
pub mod transfer;
pub mod transport;
pub mod trust;
mod worker;

pub const PROTOCOL_VERSION: u16 = 5;
pub const NATIVE_ABI_VERSION: u32 = 1;
pub const ALPN: &[u8] = b"frames/5";
