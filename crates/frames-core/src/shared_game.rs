//! Shared-mode policy and reviewed compatibility evidence. A catalog is never an allowlist.
#[cfg(test)]
use crate::input::InputEvent;
use crate::{model::WindowId, protocol::Message};
use serde::{Deserialize, Serialize};

pub const ADAPTER_REVISION: &str = "frames-protoinput-1";
pub const UPSTREAM_REVISION: &str = "e18e06bb345b86540e7464f7355c649bfdd17f38";
pub const CATALOG_JSON: &str = include_str!("../../../assets/game-compatibility.json");
pub const LEASE_MS: u64 = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub revision: u16,
    pub physical_keyboard: bool,
    pub relative_mouse: bool,
    pub relative_buttons: bool,
    pub virtual_cursor: bool,
    pub generation_lease: bool,
}
impl Capabilities {
    pub const V1: Self = Self {
        revision: 1,
        physical_keyboard: true,
        relative_mouse: true,
        relative_buttons: true,
        virtual_cursor: true,
        generation_lease: true,
    };
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    pub revision: u32,
}
impl Default for Profile {
    fn default() -> Self {
        Self {
            id: "generic".into(),
            revision: 1,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compatibility {
    VerifiedBuild,
    Partial,
    KnownIssues,
    #[default]
    Untested,
}
impl Compatibility {
    pub fn label(self) -> &'static str {
        match self {
            Self::VerifiedBuild => "Verified build",
            Self::Partial => "Partial",
            Self::KnownIssues => "Known issues",
            Self::Untested => "Untested",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Results {
    pub keyboard: bool,
    pub relative_mouse: bool,
    pub absolute_mouse: bool,
    pub background: bool,
    pub capture: bool,
    pub audio: bool,
    pub recovery: bool,
}
impl Results {
    pub fn passed(&self) -> bool {
        self.keyboard
            && self.relative_mouse
            && self.absolute_mouse
            && self.background
            && self.capture
            && self.audio
            && self.recovery
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub title: String,
    pub edition: String,
    pub store: String,
    pub build: String,
    pub executable_sha256: String,
    pub adapter_revision: String,
    pub profile: Profile,
    pub frames_version: String,
    pub tested_at: String,
    pub windows_version: String,
    pub client_platform: String,
    pub status: Compatibility,
    pub results: Results,
    pub settings: Vec<String>,
    pub limitations: Vec<String>,
    pub report: String,
    pub simultaneous_use_minutes: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub schema: u32,
    pub adapter_revision: String,
    pub upstream_revision: String,
    pub profiles: Vec<Profile>,
    pub records: Vec<Record>,
}
impl Catalog {
    pub fn bundled() -> anyhow::Result<Self> {
        Self::parse(CATALOG_JSON)
    }
    pub fn parse(json: &str) -> anyhow::Result<Self> {
        let catalog: Self = serde_json::from_str(json)?;
        anyhow::ensure!(
            catalog.schema == 1
                && catalog.adapter_revision == ADAPTER_REVISION
                && catalog.upstream_revision == UPSTREAM_REVISION,
            "incompatible game catalog"
        );
        // v1 has no reviewed per-game overrides. Metadata cannot enable unreviewed hooks.
        anyhow::ensure!(
            catalog.profiles == [Profile::default()],
            "unsupported game profile"
        );
        let mut identities = std::collections::BTreeSet::new();
        for r in &catalog.records {
            anyhow::ensure!(
                valid_hash(&r.executable_sha256)
                    && !r.title.trim().is_empty()
                    && !r.build.trim().is_empty()
                    && !r.report.trim().is_empty()
                    && !r.windows_version.trim().is_empty()
                    && !r.client_platform.trim().is_empty()
                    && !r.frames_version.trim().is_empty()
                    && !r.edition.trim().is_empty()
                    && !r.store.trim().is_empty()
                    && !r.adapter_revision.trim().is_empty()
                    && valid_date(&r.tested_at),
                "incomplete compatibility evidence"
            );
            anyhow::ensure!(
                r.status != Compatibility::Untested,
                "untested is computed, not a test record"
            );
            anyhow::ensure!(
                r.status != Compatibility::VerifiedBuild
                    || (r.results.passed() && r.simultaneous_use_minutes >= 30),
                "verification requires every acceptance result"
            );
            anyhow::ensure!(
                identities.insert((
                    &r.executable_sha256,
                    &r.adapter_revision,
                    &r.profile.id,
                    r.profile.revision
                )),
                "duplicate compatibility configuration"
            );
            anyhow::ensure!(
                !r.profile.id.trim().is_empty()
                    && r.profile.id.len() <= 64
                    && r.profile.revision > 0,
                "invalid historical profile"
            );
        }
        Ok(catalog)
    }
    pub fn lookup(&self, hash: &str) -> CompatibilityInfo {
        let profile = Profile::default();
        let record = self.records.iter().find(|r| {
            r.executable_sha256 == hash
                && r.adapter_revision == ADAPTER_REVISION
                && r.profile == profile
        });
        CompatibilityInfo {
            executable_sha256: valid_hash(hash).then(|| hash.to_owned()),
            status: record.map_or(Compatibility::Untested, |r| r.status),
            profile,
            record: record.cloned(),
        }
    }
}
fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_date(s: &str) -> bool {
    if s.len() != 10
        || s.as_bytes()[4] != b'-'
        || s.as_bytes()[7] != b'-'
        || !s
            .bytes()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return false;
    }
    let (Ok(year), Ok(month), Ok(day)) = (
        s[..4].parse::<u32>(),
        s[5..7].parse::<u32>(),
        s[8..].parse::<u32>(),
    ) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    };
    year > 0 && day > 0 && day <= days
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CompatibilityInfo {
    #[serde(default)]
    pub executable_sha256: Option<String>,
    pub status: Compatibility,
    pub profile: Profile,
    pub record: Option<Record>,
}
impl CompatibilityInfo {
    pub fn description(&self) -> String {
        if let Some(r) = &self.record {
            format!(
                "{}\n{} · {} · {}\nGame build: {}\nTested: {}\nFrames {} · Windows {} · {} viewer\nAdapter {} · profile {} revision {}\nSettings: {}\nLimitations: {}\nReport: {}\n\nVerification applies to this recorded configuration.",
                self.status.label(),
                r.title,
                r.edition,
                r.store,
                r.build,
                r.tested_at,
                r.frames_version,
                r.windows_version,
                r.client_platform,
                r.adapter_revision,
                r.profile.id,
                r.profile.revision,
                r.settings.join("; "),
                r.limitations.join("; "),
                r.report
            )
        } else {
            format!(
                "Untested\nNo verification matches this executable build and adapter/profile revision. You can try the generic profile. Successful attachment does not verify a game.\n\nExecutable SHA-256: {}\nAdapter: {}\nProfile: {} revision {}",
                self.executable_sha256.as_deref().unwrap_or("unavailable"),
                ADAPTER_REVISION,
                self.profile.id,
                self.profile.revision
            )
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Attaching,
    Active,
    Paused,
    Error,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedSession {
    pub window: WindowId,
    pub generation: u64,
    pub phase: Phase,
    pub compatibility: CompatibilityInfo,
    pub detail: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Cursor {
    pub x: f64,
    pub y: f64,
    pub visible: bool,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    /// Bounded PNG, sent only when the cursor image changes.
    pub png_base64: Option<String>,
}
impl Cursor {
    pub fn valid(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && (0.0..=1.0).contains(&self.x)
            && (0.0..=1.0).contains(&self.y)
            && self.hotspot_x < 128
            && self.hotspot_y < 128
            && self
                .png_base64
                .as_ref()
                .is_none_or(|png| png.len() <= 128 * 1024)
    }
}
impl SharedSession {
    /// Authoritative host admission across all transport lanes. No client setting can widen this.
    pub fn permits(&self, message: &Message) -> bool {
        match message {
            Message::SharedInput {
                window,
                generation,
                event,
                ..
            } => *window == self.window && *generation == self.generation && event.valid(),
            Message::SharedHeartbeat { generation } | Message::SharedResume { generation, .. } => {
                *generation == self.generation
            }
            Message::Visibility { window, .. }
            | Message::StreamReady { window, .. }
            | Message::Keyframe { window, .. }
            | Message::Feedback { window, .. } => {
                *window == self.window || *window == crate::media::AUDIO_WINDOW
            }
            Message::ReleaseInput
            | Message::InputClock { .. }
            | Message::SelectCodec { .. }
            | Message::SetQuality { .. }
            | Message::AudioPreference { .. }
            | Message::ClockProbe { .. }
            | Message::Ping { .. }
            | Message::Pong { .. }
            | Message::Goodbye => true,
            Message::SetViewMode {
                mode: crate::model::ViewMode::Windows,
            } => true,
            Message::SetRemoteCursor { .. } => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn empty_catalog() -> Catalog {
        let mut c = Catalog::bundled().unwrap();
        c.records.clear();
        c
    }
    fn record() -> Record {
        Record {
            title: "Fixture".into(),
            edition: "test".into(),
            store: "test".into(),
            build: "1".into(),
            executable_sha256: "a".repeat(64),
            adapter_revision: ADAPTER_REVISION.into(),
            profile: Profile::default(),
            frames_version: "0.1.4".into(),
            tested_at: "2026-11-01".into(),
            windows_version: "26200".into(),
            client_platform: "windows".into(),
            status: Compatibility::VerifiedBuild,
            results: Results {
                keyboard: true,
                relative_mouse: true,
                absolute_mouse: true,
                background: true,
                capture: true,
                audio: true,
                recovery: true,
            },
            settings: vec![],
            limitations: vec![],
            report: "fixture-report".into(),
            simultaneous_use_minutes: 30,
        }
    }
    #[test]
    fn catalog_is_evidence_and_matches_exact_build_and_adapter() {
        let mut c = empty_catalog();
        assert_eq!(c.lookup("unknown.exe").status, Compatibility::Untested);
        c.records.push(record());
        assert_eq!(
            c.lookup(&"a".repeat(64)).status,
            Compatibility::VerifiedBuild
        );
        assert_eq!(c.lookup(&"b".repeat(64)).status, Compatibility::Untested);
        c.records[0].adapter_revision = "old".into();
        assert_eq!(c.lookup(&"a".repeat(64)).status, Compatibility::Untested);
    }
    #[test]
    fn incomplete_evidence_and_duplicate_records_are_rejected() {
        let mut c = empty_catalog();
        let mut r = record();
        r.results.recovery = false;
        c.records.push(r);
        assert!(Catalog::parse(&serde_json::to_string(&c).unwrap()).is_err());
        c.records = vec![record()];
        c.records[0].simultaneous_use_minutes = 29;
        assert!(Catalog::parse(&serde_json::to_string(&c).unwrap()).is_err());
        c.records = vec![record(), record()];
        assert!(Catalog::parse(&serde_json::to_string(&c).unwrap()).is_err());
    }
    #[test]
    fn cursor_bounds_and_historical_profiles() {
        assert!(Cursor::default().valid());
        assert!(
            !Cursor {
                x: f64::NAN,
                ..Default::default()
            }
            .valid()
        );
        assert!(
            !Cursor {
                hotspot_x: 128,
                ..Default::default()
            }
            .valid()
        );
        let mut c = empty_catalog();
        let mut r = record();
        r.profile = Profile {
            id: "historical".into(),
            revision: 3,
        };
        c.records.push(r);
        let c = Catalog::parse(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(c.lookup(&"a".repeat(64)).status, Compatibility::Untested);
        assert_eq!(c.records.len(), 1);
        assert!(valid_date("2024-02-29"));
        for date in [
            "2026-02-29",
            "2026-13-01",
            "2026-04-31",
            "0000-01-01",
            "é026-01-01",
        ] {
            assert!(!valid_date(date));
        }
    }
    #[test]
    fn shared_policy_rejects_other_generations_and_global_channels() {
        let s = SharedSession {
            window: WindowId {
                id: 1,
                generation: 1,
            },
            generation: 42,
            phase: Phase::Active,
            compatibility: Default::default(),
            detail: String::new(),
        };
        for generation in [41, 43] {
            assert!(!s.permits(&Message::SharedHeartbeat { generation }));
        }
        assert!(s.permits(&Message::SharedHeartbeat { generation: 42 }));
        assert!(!s.permits(&Message::Input {
            window: s.window,
            event: InputEvent::Key {
                usage: 4,
                down: true
            },
            elapsed_ms: 0
        }));
        assert!(!s.permits(&Message::SetViewMode {
            mode: crate::model::ViewMode::Desktop
        }));
        assert!(!s.permits(&Message::AdbBridge { enabled: true }));
        assert!(!s.permits(&Message::DownloadFile {
            path: "secret".into()
        }));
        assert!(!s.permits(&Message::Command {
            request: 1,
            window: s.window,
            action: crate::model::WindowAction::Focus
        }));
    }
}
