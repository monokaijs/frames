use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
pub struct WindowId {
    pub id: u64,
    pub generation: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowRole {
    Desktop,
    Window,
    Dialog,
    Menu,
    Tooltip,
    Popover,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewMode {
    #[default]
    Windows,
    Desktop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResizeCapability {
    Supported,
    Fixed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteWindow {
    pub key: WindowId,
    pub app_id: String,
    pub app_name: String,
    pub title: String,
    pub icon_png: Option<String>,
    pub parent: Option<WindowId>,
    pub role: WindowRole,
    pub bounds: Bounds,
    pub scale: f64,
    pub resize: ResizeCapability,
    pub minimized: bool,
    pub visible: bool,
    pub modal: bool,
}

impl RemoteWindow {
    pub fn validate(&self) -> bool {
        let b = self.bounds;
        b.x.is_finite()
            && b.y.is_finite()
            && b.width.is_finite()
            && b.height.is_finite()
            && b.width > 0.0
            && b.height > 0.0
            && b.width <= 16384.0
            && b.height <= 16384.0
            && self.scale.is_finite()
            && self.scale > 0.0
            && self.scale <= 8.0
            && self.parent != Some(self.key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SizingMode {
    #[default]
    ResizeRemote,
    StretchLocal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WindowAction {
    Focus,
    Close,
    Restore,
    Resize { width: f64, height: f64 },
    Menu,
    MenuItem { id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MenuItem {
    pub id: String,
    pub title: String,
    pub enabled: bool,
    pub checked: bool,
    pub children: Vec<MenuItem>,
}

pub fn validate_menu(items: &[MenuItem]) -> anyhow::Result<()> {
    fn walk(items: &[MenuItem], depth: usize, count: &mut usize) -> anyhow::Result<()> {
        anyhow::ensure!(depth <= 8, "menu nesting exceeds limit");
        for item in items {
            *count += 1;
            anyhow::ensure!(
                *count <= 1024 && item.title.len() <= 4096 && item.id.len() <= 128,
                "menu exceeds size limits"
            );
            if !item.children.is_empty() {
                walk(&item.children, depth + 1, count)?;
            }
        }
        Ok(())
    }
    walk(items, 0, &mut 0)
}

#[derive(Default)]
pub struct WindowGraph {
    pub sequence: u64,
    pub windows: BTreeMap<WindowId, RemoteWindow>,
}

impl WindowGraph {
    pub fn snapshot(&mut self, sequence: u64, windows: Vec<RemoteWindow>) -> anyhow::Result<()> {
        let candidate: BTreeMap<_, _> = windows.into_iter().map(|w| (w.key, w)).collect();
        validate_graph(&candidate)?;
        self.windows = candidate;
        self.sequence = sequence;
        Ok(())
    }
    pub fn delta(
        &mut self,
        sequence: u64,
        upsert: Vec<RemoteWindow>,
        removed: &[WindowId],
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            sequence == self.sequence + 1,
            "window update gap: request a snapshot"
        );
        let mut next = self.windows.clone();
        for key in removed {
            next.remove(key);
        }
        for window in upsert {
            next.insert(window.key, window);
        }
        validate_graph(&next)?;
        self.windows = next;
        self.sequence = sequence;
        Ok(())
    }
}

fn validate_graph(windows: &BTreeMap<WindowId, RemoteWindow>) -> anyhow::Result<()> {
    anyhow::ensure!(windows.len() <= 4096, "too many remote windows");
    for w in windows.values() {
        anyhow::ensure!(w.validate(), "invalid window geometry or identity");
        let mut parent = w.parent;
        let mut depth = 0;
        while let Some(key) = parent {
            depth += 1;
            anyhow::ensure!(depth <= 32 && key != w.key, "window ownership cycle");
            parent = windows.get(&key).and_then(|p| p.parent);
        }
    }
    Ok(())
}

/// Local handles never go on the wire. A recycled handle receives a new generation.
#[derive(Default)]
pub struct WindowRegistry {
    live: HashMap<u64, (WindowId, u64)>,
    generations: HashMap<u64, u32>,
    next_id: u64,
}

impl WindowRegistry {
    pub fn reconcile(&mut self, handles: &[(u64, u64)]) -> HashMap<u64, WindowId> {
        self.live
            .retain(|h, (_, owner)| handles.iter().any(|(n, p)| n == h && p == owner));
        for &(handle, owner) in handles {
            if !self.live.contains_key(&handle) {
                let generation = self.generations.entry(handle).or_default();
                *generation = generation.saturating_add(1);
                self.next_id += 1;
                self.live.insert(
                    handle,
                    (
                        WindowId {
                            id: self.next_id,
                            generation: *generation,
                        },
                        owner,
                    ),
                );
            }
        }
        self.live.iter().map(|(&h, &(key, _))| (h, key)).collect()
    }
    pub fn handle(&self, key: WindowId) -> Option<u64> {
        self.live
            .iter()
            .find_map(|(&h, &(k, _))| (k == key).then_some(h))
    }
}

#[derive(Default, Debug, Clone)]
pub struct Subscription {
    pub visible: bool,
    pub focused: bool,
    pub needs_keyframe: bool,
}

impl Subscription {
    pub fn update(&mut self, visible: bool, focused: bool) -> bool {
        let resumed = visible && !self.visible;
        self.visible = visible;
        self.focused = focused && visible;
        self.needs_keyframe |= resumed;
        resumed
    }
    pub fn fps(&self) -> u32 {
        if !self.visible {
            0
        } else if self.focused {
            60
        } else {
            24
        }
    }
}

pub fn effective_sizing(requested: SizingMode, capability: ResizeCapability) -> SizingMode {
    if capability != ResizeCapability::Supported {
        SizingMode::StretchLocal
    } else {
        requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recycled_handle_cannot_target_old_window() {
        let mut r = WindowRegistry::default();
        let first = r.reconcile(&[(100, 1)])[&100];
        r.reconcile(&[]);
        let next = r.reconcile(&[(100, 2)])[&100];
        assert_ne!(first, next);
        assert_eq!(r.handle(first), None);
        assert_eq!(r.handle(next), Some(100));
    }
    #[test]
    fn hide_stops_work_and_resume_requests_keyframe() {
        let mut s = Subscription::default();
        assert!(s.update(true, true));
        assert_eq!(s.fps(), 60);
        s.needs_keyframe = false;
        assert!(!s.update(false, false));
        assert_eq!(s.fps(), 0);
        assert!(s.update(true, false));
        assert!(s.needs_keyframe);
        assert_eq!(s.fps(), 24);
    }
    #[test]
    fn fixed_and_unknown_windows_cannot_remote_resize() {
        for cap in [ResizeCapability::Fixed, ResizeCapability::Unknown] {
            assert_eq!(
                effective_sizing(SizingMode::ResizeRemote, cap),
                SizingMode::StretchLocal
            );
        }
    }
    #[test]
    fn update_gaps_do_not_mutate_graph() {
        let mut graph = WindowGraph::default();
        assert!(graph.delta(2, vec![], &[]).is_err());
        assert_eq!(graph.sequence, 0);
    }
}
