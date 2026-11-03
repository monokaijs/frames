//! Channel identifiers for frames on reliable streams.
//!
//! Core channels have fixed ids below [`ChannelId::FIRST_SERVICE`]. Each
//! service registers its own id at or above it; ids are only meaningful once
//! both peers negotiated the feature that owns them.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(pub u16);

impl ChannelId {
    /// Handshake, session lifecycle, and feature negotiation.
    pub const CONTROL: Self = Self(0);
    /// Discrete input (keys, buttons, text). Pointer motion uses datagrams.
    pub const INPUT: Self = Self(1);
    /// Media stream lifecycle: start, ready, stop, quality, recovery requests
    /// that need reliable delivery.
    pub const MEDIA_CONTROL: Self = Self(2);
    /// Window catalog snapshots and deltas.
    pub const WINDOWS: Self = Self(3);
    /// First id available to services (transfer, clipboard, devices, ...).
    pub const FIRST_SERVICE: Self = Self(0x100);

    pub fn is_core(self) -> bool {
        self < Self::FIRST_SERVICE
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::CONTROL => f.write_str("control"),
            Self::INPUT => f.write_str("input"),
            Self::MEDIA_CONTROL => f.write_str("media-control"),
            Self::WINDOWS => f.write_str("windows"),
            Self(id) => write!(f, "channel {id:#06x}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_and_service_ranges_do_not_overlap() {
        for core in [
            ChannelId::CONTROL,
            ChannelId::INPUT,
            ChannelId::MEDIA_CONTROL,
            ChannelId::WINDOWS,
        ] {
            assert!(core.is_core());
        }
        assert!(!ChannelId::FIRST_SERVICE.is_core());
        assert_eq!(ChannelId(0x123).to_string(), "channel 0x0123");
    }
}
