//! Platform-neutral tray menu model shared by the Windows notification-area
//! icon and the macOS menu bar item.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TrayGlyph {
    App,
    Window,
    Show,
    Hide,
    Settings,
    Display,
    Cursor,
    Keyboard,
    Refresh,
    Folder,
    Devices,
    Network,
    Power,
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrayItem {
    pub id: u32,
    pub title: String,
    pub glyph: TrayGlyph,
    pub checked: bool,
    pub enabled: bool,
    pub children: Vec<TrayItem>,
}
impl TrayItem {
    pub fn new(id: u32, title: impl Into<String>, glyph: TrayGlyph) -> Self {
        Self {
            id,
            title: title.into(),
            glyph,
            checked: false,
            enabled: true,
            children: vec![],
        }
    }
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn children(mut self, children: Vec<Self>) -> Self {
        self.children = children;
        self
    }
    pub fn separator() -> Self {
        Self::new(0, "", TrayGlyph::App)
    }
}
