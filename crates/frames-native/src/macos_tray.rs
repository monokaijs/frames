//! macOS menu bar item with the same model and command ids as the Windows
//! notification-area icon, so the launcher drives both with one code path.
//!
//! AppKit objects are main-thread only: the launcher creates and updates the
//! tray from its Slint event loop, which runs on the main thread.
use crate::tray_model::{TrayGlyph, TrayItem};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained, runtime::NSObject, sel,
};
use objc2_app_kit::{
    NSControlStateValueOff, NSControlStateValueOn, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSSize, NSString};
use std::sync::mpsc;

/// Monitor glyph from the launcher's icon set, drawn as a template image so
/// macOS tints it for light, dark and highlighted menu bars.
const ICON: &[u8] = include_bytes!("../../frames/ui/icons/display.svg");

struct Ivars {
    commands: mpsc::Sender<u32>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and Target has no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "FramesTrayTarget"]
    #[ivars = Ivars]
    struct Target;

    impl Target {
        #[unsafe(method(itemClicked:))]
        fn item_clicked(&self, sender: &NSMenuItem) {
            let _ = self.ivars().commands.send(sender.tag() as u32);
        }
    }
);

impl Target {
    fn new(mtm: MainThreadMarker, commands: mpsc::Sender<u32>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars { commands });
        unsafe { msg_send![super(this), init] }
    }
}

pub struct Tray {
    mtm: MainThreadMarker,
    item: Retained<NSStatusItem>,
    target: Retained<Target>,
    pub commands: mpsc::Receiver<u32>,
    last: Option<(Vec<TrayItem>, String)>,
}

impl Tray {
    pub fn new(title: &str) -> anyhow::Result<Self> {
        let mtm = MainThreadMarker::new().ok_or_else(|| {
            anyhow::anyhow!("the menu bar item must be created on the main thread")
        })?;
        let (sender, commands) = mpsc::channel();
        let target = Target::new(mtm, sender);
        let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
        if let Some(button) = item.button(mtm) {
            let data = NSData::with_bytes(ICON);
            match NSImage::initWithData(NSImage::alloc(), &data) {
                Some(image) => {
                    image.setTemplate(true);
                    image.setSize(NSSize::new(18.0, 18.0));
                    button.setImage(Some(&image));
                }
                None => button.setTitle(&NSString::from_str(title)),
            }
            button.setToolTip(Some(&NSString::from_str(title)));
        }
        Ok(Self {
            mtm,
            item,
            target,
            commands,
            last: None,
        })
    }

    /// Rebuilds the menu only when its contents change. `active` has no
    /// separate macOS presentation; the first item already reports sessions.
    pub fn update(&mut self, items: Vec<TrayItem>, _active: bool, tip: impl Into<String>) {
        let tip = tip.into();
        if self
            .last
            .as_ref()
            .is_some_and(|(old, old_tip)| *old == items && *old_tip == tip)
        {
            return;
        }
        let menu = self.menu(&items);
        self.item.setMenu(Some(&menu));
        if let Some(button) = self.item.button(self.mtm) {
            button.setToolTip(Some(&NSString::from_str(&tip)));
        }
        self.last = Some((items, tip));
    }

    fn menu(&self, items: &[TrayItem]) -> Retained<NSMenu> {
        let menu = NSMenu::new(self.mtm);
        // Items carry their own enabled state; AppKit must not override it.
        menu.setAutoenablesItems(false);
        for entry in items {
            if entry.title.is_empty() {
                menu.addItem(&NSMenuItem::separatorItem(self.mtm));
                continue;
            }
            let item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    NSMenuItem::alloc(self.mtm),
                    &NSString::from_str(&entry.title),
                    Some(sel!(itemClicked:)),
                    &NSString::from_str(""),
                )
            };
            item.setTag(entry.id as isize);
            unsafe { item.setTarget(Some(&self.target)) };
            item.setEnabled(entry.enabled);
            let symbol = match entry.glyph {
                TrayGlyph::App => "app",
                TrayGlyph::Window => "macwindow",
                TrayGlyph::Show => "eye",
                TrayGlyph::Hide => "eye.slash",
                TrayGlyph::Settings => "gearshape",
                TrayGlyph::Display => "desktopcomputer",
                TrayGlyph::Cursor => "cursorarrow",
                TrayGlyph::Keyboard => "keyboard",
                TrayGlyph::Refresh => "arrow.clockwise",
                TrayGlyph::Folder => "folder",
                TrayGlyph::Devices => "desktopcomputer",
                TrayGlyph::Network => "network",
                TrayGlyph::Power => "power",
                TrayGlyph::Info => "info.circle",
            };
            if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str(symbol),
                None,
            ) {
                image.setTemplate(true);
                image.setSize(NSSize::new(16.0, 16.0));
                item.setImage(Some(&image));
                // macOS 27 hides menu images in automatic mode. Older
                // systems do not implement this selector.
                unsafe {
                    let supported: bool =
                        msg_send![&*item, respondsToSelector: sel!(setPreferredImageVisibility:)];
                    if supported {
                        let _: () = msg_send![&*item, setPreferredImageVisibility: 1isize];
                    }
                }
            }
            item.setState(if entry.checked {
                NSControlStateValueOn
            } else {
                NSControlStateValueOff
            });
            if !entry.children.is_empty() {
                item.setSubmenu(Some(&self.menu(&entry.children)));
            }
            menu.addItem(&item);
        }
        menu
    }
}

#[doc(hidden)]
impl Tray {
    /// Probe support: (has image, item count, enabled per tag) of the live menu.
    pub fn probe_state(&self, tag: u32) -> (bool, isize, Option<bool>) {
        let image = self
            .item
            .button(self.mtm)
            .and_then(|button| button.image())
            .is_some();
        let menu = self.item.menu(self.mtm);
        let count = menu.as_ref().map_or(0, |menu| menu.numberOfItems());
        let enabled = menu
            .and_then(|menu| menu.itemWithTag(tag as isize))
            .map(|item| item.isEnabled());
        (image, count, enabled)
    }
    /// Probe support: fires a menu item through AppKit's target-action path.
    pub fn probe_perform(&self, tag: u32) -> bool {
        let Some(menu) = self.item.menu(self.mtm) else {
            return false;
        };
        let index = menu.indexOfItemWithTag(tag as isize);
        if index < 0 {
            return false;
        }
        menu.performActionForItemAtIndex(index);
        true
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        NSStatusBar::systemStatusBar().removeStatusItem(&self.item);
    }
}
