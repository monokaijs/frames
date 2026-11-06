use super::*;

#[test]
fn relative_mouse_sends_gameplay_key_presses_and_releases_without_text() {
    PHYSICAL.with(|physical| physical.set(false));
    LOCAL_SHORTCUT_KEYS.with(|keys| keys.borrow_mut().clear());
    let hwnd = HWND::default();
    set_relative(hwnd, true, false); // No pointer grab in unattended tests.
    take_input();
    // W, A, S, D, Space: actual scan-code messages, like the viewer receives.
    for (vk, scan, usage) in [
        (0x57, 0x11, 0x1a),
        (0x41, 0x1e, 4),
        (0x53, 0x1f, 0x16),
        (0x44, 0x20, 7),
        (0x20, 0x39, 0x2c),
    ] {
        for (message, down) in [(WM_KEYDOWN, true), (WM_KEYUP, false)] {
            let input = viewer_input(hwnd, message, WPARAM(vk), LPARAM(scan << 16));
            assert!(matches!(input, Some(Captured::Event {
                event: Event::Key { usage: actual, down: pressed }, ..
            }) if actual == usage && pressed == down));
        }
        assert!(
            viewer_input(hwnd, WM_CHAR, WPARAM(vk), LPARAM(0)).is_none(),
            "gameplay must not also type text"
        );
    }
    set_relative(hwnd, false, false);
    assert!(
        matches!(viewer_input(hwnd, WM_CHAR, WPARAM(0x77), LPARAM(0)), Some(Captured::Event {
        event: Event::Text { text }, ..
    }) if text == "w"),
        "normal typing resumes after leaving game mode"
    );
    take_input();
}

#[test]
fn mode_changes_release_held_game_keys_and_shortcut_modifiers() {
    let hwnd = HWND::default();
    take_input();
    set_relative(hwnd, true, false);
    set_relative(hwnd, false, false);
    let events: Vec<_> = take_input()
        .into_iter()
        .filter_map(|input| match input {
            Captured::Event { event, .. } => Some(event),
            _ => None,
        })
        .collect();
    assert_eq!(
        events,
        [
            Event::ReleaseAll,
            Event::Mode { relative: true },
            Event::ReleaseAll,
            Event::Mode { relative: false },
        ]
    );
}

#[test]
fn local_shortcut_repeats_and_releases_stay_local_after_modifiers_lift() {
    PHYSICAL.with(|physical| physical.set(true));
    for key in [VK_F9, VK_F10, VK_F11, VK_F12] {
        LOCAL_SHORTCUT_KEYS.with(|keys| keys.borrow_mut().push(usize::from(key.0)));
        for message in [WM_KEYDOWN, WM_KEYUP] {
            assert!(
                viewer_input(
                    HWND::default(),
                    message,
                    WPARAM(usize::from(key.0)),
                    LPARAM(0)
                )
                .is_none()
            );
        }
        assert!(LOCAL_SHORTCUT_KEYS.with(|keys| keys.borrow().is_empty()));
    }
    PHYSICAL.with(|physical| physical.set(false));
}
