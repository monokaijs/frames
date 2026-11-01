//! Session-scoped device forwarding. Report queues preserve button transitions.
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

pub const MAX_GAMEPADS: usize = 4;
pub const MAX_USB_DEVICES: usize = 8;
pub const REPORT_TTL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub gamepad_host: bool,
    pub usb_host: bool,
    pub gamepad_error: Option<String>,
    pub usb_error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GamepadState {
    pub buttons: u16,
    pub left_x: i16,
    pub left_y: i16,
    pub right_x: i16,
    pub right_y: i16,
    pub left_trigger: u8,
    pub right_trigger: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub physical_id: Option<String>,
    pub id: String,
    pub name: String,
    pub forwarded: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceList {
    pub usb_local_error: Option<String>,
    pub capabilities: Capabilities,
    pub gamepads: Vec<DeviceInfo>,
    pub usb: Vec<DeviceInfo>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum DeviceAction {
    Refresh,
    Gamepad { id: String, enabled: bool },
    Usb { id: String, enabled: bool },
    Setup { target: String },
    Focus { active: bool },
    Release,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceMessage {
    Unavailable {
        error: String,
    },
    Refresh,
    Setup {
        target: String,
    },
    Capabilities(Capabilities),
    GamepadAttach {
        slot: u8,
        generation: u64,
        name: String,
    },
    GamepadDetach {
        slot: u8,
        generation: u64,
    },
    GamepadStatus {
        slot: u8,
        generation: u64,
        attached: bool,
        error: Option<String>,
    },
    GamepadState {
        slot: u8,
        generation: u64,
        sequence: u64,
        elapsed_ms: u64,
        state: GamepadState,
    },
    Rumble {
        slot: u8,
        generation: u64,
        elapsed_ms: u64,
        low: u16,
        high: u16,
    },
    Clock {
        elapsed_ms: u64,
    },
    Release,
    UsbAttach {
        id: String,
        generation: u64,
        name: String,
    },
    UsbDetach {
        id: String,
        generation: u64,
    },
    UsbStatus {
        id: String,
        generation: u64,
        attached: bool,
        error: Option<String>,
    },
}

impl DeviceMessage {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Unavailable { error } => {
                anyhow::ensure!(error.len() <= 2048, "Invalid device error length")
            }
            Self::Setup { target } => anyhow::ensure!(
                ["gamepad", "usb-host"].contains(&target.as_str()),
                "Invalid host driver setup target"
            ),
            Self::GamepadAttach {
                slot,
                generation,
                name,
            } => {
                anyhow::ensure!(
                    (*slot as usize) < MAX_GAMEPADS && *generation != 0 && name.len() <= 256,
                    "invalid gamepad attachment"
                );
            }
            Self::GamepadDetach { slot, generation }
            | Self::GamepadStatus {
                slot, generation, ..
            }
            | Self::GamepadState {
                slot, generation, ..
            }
            | Self::Rumble {
                slot, generation, ..
            } => {
                anyhow::ensure!(
                    (*slot as usize) < MAX_GAMEPADS && *generation != 0,
                    "invalid gamepad slot or generation"
                );
            }
            Self::UsbAttach {
                id,
                generation,
                name,
            } => {
                anyhow::ensure!(
                    valid_bus_id(id) && *generation != 0 && name.len() <= 256,
                    "invalid USB attachment"
                );
            }
            Self::UsbDetach { id, generation } | Self::UsbStatus { id, generation, .. } => {
                anyhow::ensure!(
                    valid_bus_id(id) && *generation != 0,
                    "invalid USB attachment"
                );
            }
            Self::Clock { elapsed_ms } => anyhow::ensure!(
                *elapsed_ms <= 90 * 24 * 60 * 60 * 1000,
                "invalid device clock"
            ),
            _ => {}
        }
        Ok(())
    }
    fn coalesces(&self, previous: &Self) -> bool {
        match (self, previous) {
            (
                Self::GamepadState {
                    slot,
                    generation,
                    state,
                    ..
                },
                Self::GamepadState {
                    slot: old_slot,
                    generation: old_generation,
                    state: old,
                    ..
                },
            ) => slot == old_slot && generation == old_generation && state.buttons == old.buttons,
            (
                Self::Rumble {
                    slot, generation, ..
                },
                Self::Rumble {
                    slot: old_slot,
                    generation: old_generation,
                    ..
                },
            ) => slot == old_slot && generation == old_generation,
            (Self::Clock { .. }, Self::Clock { .. }) => true,
            _ => false,
        }
    }
}

pub fn valid_bus_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() < 32
        && id
            .bytes()
            .all(|c| c.is_ascii_digit() || c == b'-' || c == b'.')
        && id.bytes().any(|c| c == b'-')
}

/// Exact PnP identity when available; VID/PID fallback conservatively reserves
/// identical models when an XInput driver does not expose its physical path.
pub fn same_physical(a: Option<&str>, b: Option<&str>) -> bool {
    let (Some(a), Some(b)) = (a, b) else {
        return true;
    };
    fn matches_model(model: &str, identity: &str) -> bool {
        let Some(model) = model.strip_prefix("vidpid:") else {
            return false;
        };
        let Some((vendor, product)) = model.split_once(':') else {
            return false;
        };
        identity
            .to_ascii_lowercase()
            .contains(&format!("vid_{vendor}&pid_{product}"))
    }
    a.eq_ignore_ascii_case(b) || matches_model(a, b) || matches_model(b, a)
}

#[derive(Default)]
struct Queue {
    messages: VecDeque<DeviceMessage>,
    closed: bool,
}
struct Shared {
    queue: Mutex<Queue>,
    wake: Notify,
}
#[derive(Clone)]
pub struct Sender(Arc<Shared>);
pub struct Receiver(Arc<Shared>);
pub fn channel() -> (Sender, Receiver) {
    let shared = Arc::new(Shared {
        queue: Mutex::new(Queue::default()),
        wake: Notify::new(),
    });
    (Sender(shared.clone()), Receiver(shared))
}
impl Sender {
    pub fn send(&self, message: DeviceMessage) -> anyhow::Result<()> {
        message.validate()?;
        let mut queue = self.0.queue.lock().unwrap();
        anyhow::ensure!(!queue.closed, "device stream stopped");
        if matches!(message, DeviceMessage::Release) {
            queue.messages.retain(|m| {
                !matches!(
                    m,
                    DeviceMessage::GamepadState { .. } | DeviceMessage::Rumble { .. }
                )
            });
        }
        if let DeviceMessage::Rumble {
            slot, generation, ..
        } = &message
        {
            queue.messages.retain(|old| !matches!(old,DeviceMessage::Rumble { slot:old_slot,generation:old_generation,.. } if old_slot==slot && old_generation==generation));
        }
        if let DeviceMessage::GamepadState {
            slot, generation, ..
        } = &message
            && let Some(index) = queue.messages.iter().rposition(|old| {
                matches!(old, DeviceMessage::GamepadState { slot: old_slot, generation: old_generation, .. }
                    if old_slot == slot && old_generation == generation)
            })
            && message.coalesces(&queue.messages[index])
        {
            // Interleaved reports from other pads must not prevent analog
            // coalescing. Only replace the latest report for this pad, so a
            // preceding button edge always survives.
            queue.messages.remove(index);
        }
        if queue
            .messages
            .back()
            .is_some_and(|old| message.coalesces(old))
        {
            queue.messages.pop_back();
        }
        if queue.messages.len() >= 128 {
            queue.messages.clear();
            queue.messages.push_back(DeviceMessage::Release);
            self.0.wake.notify_one();
            anyhow::bail!("device queue overloaded; forwarding released");
        }
        queue.messages.push_back(message);
        drop(queue);
        self.0.wake.notify_one();
        Ok(())
    }
}
impl Receiver {
    pub async fn recv(&mut self) -> Option<DeviceMessage> {
        loop {
            let wake = self.0.wake.notified();
            {
                let mut queue = self.0.queue.lock().unwrap();
                if let Some(message) = queue.messages.pop_front() {
                    return Some(message);
                }
                if queue.closed {
                    return None;
                }
            }
            wake.await;
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.0.queue.lock().unwrap().closed = true;
        self.0.wake.notify_waiters();
    }
}

/// Uses the earliest observed epoch; delayed clock packets cannot refresh old input.
#[derive(Default)]
pub struct InputClock {
    epoch: Option<Instant>,
}
impl InputClock {
    pub fn observe(&mut self, elapsed_ms: u64, now: Instant) {
        if elapsed_ms > 90 * 24 * 60 * 60 * 1000 {
            return;
        }
        if let Some(epoch) = now.checked_sub(Duration::from_millis(elapsed_ms)) {
            self.epoch = Some(self.epoch.map_or(epoch, |old| old.min(epoch)));
        }
    }
    pub fn fresh(&self, elapsed_ms: u64, now: Instant) -> bool {
        self.epoch
            .and_then(|e| e.checked_add(Duration::from_millis(elapsed_ms)))
            .is_some_and(|sent| {
                sent <= now + Duration::from_millis(5)
                    && now.saturating_duration_since(sent) < REPORT_TTL
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn report(buttons: u16, x: i16) -> DeviceMessage {
        DeviceMessage::GamepadState {
            slot: 0,
            generation: 1,
            sequence: 1,
            elapsed_ms: 0,
            state: GamepadState {
                buttons,
                left_x: x,
                ..Default::default()
            },
        }
    }
    #[tokio::test]
    async fn analog_updates_coalesce_but_short_taps_survive() {
        let (tx, mut rx) = channel();
        tx.send(report(0, 1)).unwrap();
        tx.send(report(0, 2)).unwrap();
        tx.send(report(0x1000, 3)).unwrap();
        tx.send(report(0, 4)).unwrap();
        for (buttons, left_x) in [(0, 2), (0x1000, 3), (0, 4)] {
            assert!(
                matches!(rx.recv().await, Some(DeviceMessage::GamepadState { state, .. }) if state.buttons == buttons && state.left_x == left_x)
            );
        }
    }
    #[test]
    fn delayed_clock_does_not_resurrect_old_reports() {
        let now = Instant::now();
        let mut clock = InputClock::default();
        clock.observe(0, now);
        clock.observe(0, now + Duration::from_secs(1));
        assert!(!clock.fresh(1, now + Duration::from_secs(1)));
        assert!(clock.fresh(1000, now + Duration::from_secs(1)));
        assert!(!valid_bus_id("1-2;calc"));
        assert!(!valid_bus_id("../x"));
        assert!(valid_bus_id("1-2.3"));
    }
    #[tokio::test]
    async fn overflow_releases_instead_of_replaying_held_buttons() {
        let (tx, mut rx) = channel();
        for n in 0..128 {
            tx.send(report((n % 2) as u16, 0)).unwrap();
        }
        assert!(tx.send(report(0, 0)).is_err());
        assert!(matches!(rx.recv().await, Some(DeviceMessage::Release)));
    }
    #[tokio::test]
    async fn interleaved_pads_coalesce_without_losing_each_others_button_edges() {
        let (tx, mut rx) = channel();
        tx.send(report(0, 1)).unwrap();
        let mut other = report(0x1000, 2);
        if let DeviceMessage::GamepadState { slot, .. } = &mut other {
            *slot = 1;
        }
        tx.send(other).unwrap();
        tx.send(report(0, 3)).unwrap();
        tx.send(report(0x1000, 4)).unwrap();
        tx.send(report(0, 5)).unwrap();
        for (expected_slot, buttons, left_x) in
            [(1, 0x1000, 2), (0, 0, 3), (0, 0x1000, 4), (0, 0, 5)]
        {
            assert!(
                matches!(rx.recv().await, Some(DeviceMessage::GamepadState {slot,state,..})
                if slot == expected_slot && state.buttons == buttons && state.left_x == left_x)
            );
        }
    }
    #[tokio::test]
    async fn rumble_coalesces_across_pads_and_release_discards_pending_effects() {
        let (tx, mut rx) = channel();
        for n in 0..400 {
            tx.send(DeviceMessage::Rumble {
                slot: (n % 4) as u8,
                generation: 1,
                elapsed_ms: 0,
                low: n as u16,
                high: 0,
            })
            .unwrap();
        }
        for expected in 396..400 {
            assert!(
                matches!(rx.recv().await, Some(DeviceMessage::Rumble {low,..}) if low == expected)
            );
        }
        tx.send(report(0x1000, 0)).unwrap();
        tx.send(DeviceMessage::Rumble {
            slot: 0,
            generation: 1,
            elapsed_ms: 0,
            low: 100,
            high: 100,
        })
        .unwrap();
        tx.send(DeviceMessage::Release).unwrap();
        assert!(matches!(rx.recv().await, Some(DeviceMessage::Release)));
    }
    #[test]
    fn physical_ownership_matches_paths_and_conservative_model_fallbacks() {
        let usb = "USB\\VID_054C&PID_0CE6\\1234";
        assert!(same_physical(Some(usb), Some(&usb.to_ascii_lowercase())));
        assert!(same_physical(Some("vidpid:054c:0ce6"), Some(usb)));
        assert!(!same_physical(Some("vidpid:045e:02ea"), Some(usb)));
        assert!(same_physical(None, Some(usb)));
    }
}
