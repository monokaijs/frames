//! `Hello` exchange and per-feature version negotiation.
//!
//! Each peer lists the features it implements with a supported version range
//! and the subset it cannot work without. Both sides compute the same
//! [`Negotiated`] result independently: for every feature both support, the
//! highest version inside both ranges. A required feature without overlap is
//! an error that names the feature, instead of v1's "builds must match".

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// A `Hello` larger than this is rejected before parsing.
pub const MAX_HELLO_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRange {
    pub min: u16,
    pub max: u16,
}

impl VersionRange {
    pub fn new(min: u16, max: u16) -> Self {
        Self { min, max }
    }

    pub fn exactly(version: u16) -> Self {
        Self::new(version, version)
    }

    fn overlap(self, other: Self) -> Option<u16> {
        let low = self.min.max(other.min);
        let high = self.max.min(other.max);
        (low <= high).then_some(high)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    /// [`crate::PROTOCOL_MAJOR`] of the sender.
    pub major: u16,
    /// Stable device identity (endpoint public key, hex). Authentication is
    /// done by the transport; this lets the peer look up trust before UI.
    pub device_id: String,
    /// Human-readable device name.
    pub name: String,
    /// Application build, for diagnostics only. Never used for decisions.
    pub build: String,
    pub features: BTreeMap<String, VersionRange>,
    pub required: BTreeSet<String>,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("Hello serializes")
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, HandshakeError> {
        if bytes.len() > MAX_HELLO_BYTES {
            return Err(HandshakeError::Malformed(format!(
                "hello of {} bytes exceeds {MAX_HELLO_BYTES}",
                bytes.len()
            )));
        }
        let hello: Self = serde_json::from_slice(bytes)
            .map_err(|error| HandshakeError::Malformed(error.to_string()))?;
        if let Some((name, range)) = hello.features.iter().find(|(_, r)| r.min > r.max) {
            return Err(HandshakeError::Malformed(format!(
                "feature {name} has an empty range {}..={}",
                range.min, range.max
            )));
        }
        if let Some(name) = hello
            .required
            .iter()
            .find(|name| !hello.features.contains_key(*name))
        {
            return Err(HandshakeError::Malformed(format!(
                "required feature {name} is not offered"
            )));
        }
        Ok(hello)
    }
}

/// The agreed feature versions for one connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Negotiated {
    versions: BTreeMap<String, u16>,
}

impl Negotiated {
    /// Agreed version of `feature`, or `None` when it is off for this peer.
    pub fn version(&self, feature: &str) -> Option<u16> {
        self.versions.get(feature).copied()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, u16)> {
        self.versions
            .iter()
            .map(|(name, version)| (name.as_str(), *version))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandshakeError {
    Malformed(String),
    MajorMismatch {
        local: u16,
        remote: u16,
    },
    /// `side` requires `feature`, and the versions do not overlap or the
    /// other side lacks it.
    MissingRequired {
        side: Side,
        feature: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "malformed hello: {reason}"),
            Self::MajorMismatch { local, remote } => write!(
                f,
                "protocol {remote} on the other device is incompatible with protocol {local}; update both devices"
            ),
            Self::MissingRequired {
                side: Side::Local,
                feature,
            } => write!(
                f,
                "the other device does not support {feature}, which this device requires"
            ),
            Self::MissingRequired {
                side: Side::Remote,
                feature,
            } => write!(
                f,
                "the other device requires {feature}, which this device does not support"
            ),
        }
    }
}

impl std::error::Error for HandshakeError {}

/// Computes the agreed versions. Symmetric: both peers get the same result.
pub fn negotiate(local: &Hello, remote: &Hello) -> Result<Negotiated, HandshakeError> {
    if local.major != remote.major {
        return Err(HandshakeError::MajorMismatch {
            local: local.major,
            remote: remote.major,
        });
    }
    let mut versions = BTreeMap::new();
    for (name, range) in &local.features {
        if let Some(version) = remote
            .features
            .get(name)
            .and_then(|other| range.overlap(*other))
        {
            versions.insert(name.clone(), version);
        }
    }
    for (side, hello) in [(Side::Local, local), (Side::Remote, remote)] {
        if let Some(feature) = hello
            .required
            .iter()
            .find(|name| !versions.contains_key(*name))
        {
            return Err(HandshakeError::MissingRequired {
                side,
                feature: feature.clone(),
            });
        }
    }
    Ok(Negotiated { versions })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(features: &[(&str, u16, u16)], required: &[&str]) -> Hello {
        Hello {
            major: crate::PROTOCOL_MAJOR,
            device_id: "ab".repeat(32),
            name: "Test".into(),
            build: "0.0.0-test".into(),
            features: features
                .iter()
                .map(|(name, min, max)| (name.to_string(), VersionRange::new(*min, *max)))
                .collect(),
            required: required.iter().map(|name| name.to_string()).collect(),
        }
    }

    #[test]
    fn picks_the_highest_common_version_and_is_symmetric() {
        let a = hello(
            &[("media", 1, 3), ("clipboard", 1, 1), ("usb", 1, 2)],
            &["media"],
        );
        let b = hello(
            &[("media", 2, 5), ("clipboard", 2, 2), ("files", 1, 1)],
            &["media"],
        );
        let ab = negotiate(&a, &b).unwrap();
        assert_eq!(ab, negotiate(&b, &a).unwrap());
        assert_eq!(ab.version("media"), Some(3));
        assert_eq!(ab.version("clipboard"), None, "ranges do not overlap");
        assert_eq!(ab.version("usb"), None, "only one side offers it");
        assert_eq!(ab.iter().count(), 1);
    }

    #[test]
    fn missing_required_features_name_the_feature_and_side() {
        let a = hello(&[("media", 1, 1)], &["media"]);
        let b = hello(&[("media", 2, 2)], &[]);
        assert_eq!(
            negotiate(&a, &b),
            Err(HandshakeError::MissingRequired {
                side: Side::Local,
                feature: "media".into()
            })
        );
        assert_eq!(
            negotiate(&b, &a),
            Err(HandshakeError::MissingRequired {
                side: Side::Remote,
                feature: "media".into()
            })
        );
        let mut old = hello(&[], &[]);
        old.major = 1;
        assert!(matches!(
            negotiate(&a, &old),
            Err(HandshakeError::MajorMismatch {
                local: 2,
                remote: 1
            })
        ));
    }

    #[test]
    fn decode_round_trips_and_rejects_inconsistent_hellos() {
        let a = hello(&[("media", 1, 3)], &["media"]);
        assert_eq!(Hello::decode(&a.encode()).unwrap(), a);

        let empty = hello(&[("media", 3, 1)], &[]);
        assert!(Hello::decode(&empty.encode()).is_err());
        let mut unoffered = hello(&[], &[]);
        unoffered.required.insert("media".into());
        assert!(Hello::decode(&unoffered.encode()).is_err());
        assert!(Hello::decode(&vec![b' '; MAX_HELLO_BYTES + 1]).is_err());
        assert!(Hello::decode(b"{\"major\":2}").is_err());
    }
}
