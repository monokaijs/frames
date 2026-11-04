//! macOS menu bar item check: creates the item, builds a menu, fires an item
//! through AppKit target-action, and verifies the command id arrives.
//! cargo run -p frames-native --example macos_tray_probe
#[cfg(target_os = "macos")]
fn main() {
    use frames_native::{Tray, TrayGlyph as G, TrayItem as Item};
    let mut tray = Tray::new("Frames probe").expect("menu bar item");
    let items = vec![
        Item::new(0, "Frames · Ready", G::Network).enabled(false),
        Item::separator(),
        Item::new(1, "Open Frames", G::App),
        Item::new(5, "Start sharing", G::Display).checked(true),
        Item::new(8, "Quit Frames", G::Power),
    ];
    tray.update(items.clone(), false, "Frames · Ready");
    let (image, count, disabled) = tray.probe_state(0);
    assert!(image, "template image must load from display.svg");
    assert_eq!(count, 5, "items including the separator");
    assert_eq!(disabled, Some(false), "a disabled item must stay disabled");
    assert_eq!(tray.probe_state(8).2, Some(true));
    for tag in [1, 8] {
        assert!(tray.probe_perform(tag));
        assert_eq!(
            tray.commands.try_recv().ok(),
            Some(tag),
            "command {tag} must arrive"
        );
    }
    // An identical update must not rebuild the menu (state stays as is).
    tray.update(items, false, "Frames · Ready");
    assert_eq!(tray.probe_state(0).1, 5);
    println!("PASS: menu bar item image, menu contents, disabled state and target-action commands");
}
#[cfg(not(target_os = "macos"))]
fn main() {}
