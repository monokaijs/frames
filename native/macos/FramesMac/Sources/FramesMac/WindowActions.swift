import ApplicationServices
import CoreGraphics
import Foundation

/// Asks another application's window to close, as its close button would
/// (ABI 4): the window is found through Accessibility by its owner and
/// frame, and its close button pressed. False when it cannot be found or
/// has no close button, or Accessibility is not allowed.
@_cdecl("frames_mac_close_window")
public func framesMacCloseWindow(_ windowID: UInt32) -> Bool {
    let info = CGWindowListCopyWindowInfo([.optionIncludingWindow], CGWindowID(windowID)) as? [[String: Any]] ?? []
    guard let entry = info.first,
        let pid = entry[kCGWindowOwnerPID as String] as? Int32,
        let boundsInfo = entry[kCGWindowBounds as String] as? NSDictionary,
        let bounds = CGRect(dictionaryRepresentation: boundsInfo)
    else { return false }
    let app = AXUIElementCreateApplication(pid)
    var value: CFTypeRef?
    guard AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &value) == .success,
        let windows = value as? [AXUIElement]
    else { return false }
    for window in windows {
        guard let frame = axFrame(window),
            abs(frame.minX - bounds.minX) < 2, abs(frame.minY - bounds.minY) < 2,
            abs(frame.width - bounds.width) < 2, abs(frame.height - bounds.height) < 2
        else { continue }
        var button: CFTypeRef?
        guard AXUIElementCopyAttributeValue(window, kAXCloseButtonAttribute as CFString, &button) == .success,
            let button, CFGetTypeID(button) == AXUIElementGetTypeID()
        else { return false }
        return AXUIElementPerformAction(unsafeDowncast(button, to: AXUIElement.self), kAXPressAction as CFString)
            == .success
    }
    return false
}

/// A window's frame in global points (top-left origin), as Quartz reports it.
private func axFrame(_ window: AXUIElement) -> CGRect? {
    func value(_ attribute: String) -> AXValue? {
        var raw: CFTypeRef?
        guard AXUIElementCopyAttributeValue(window, attribute as CFString, &raw) == .success, let raw,
            CFGetTypeID(raw) == AXValueGetTypeID()
        else { return nil }
        return unsafeDowncast(raw, to: AXValue.self)
    }
    var position = CGPoint.zero
    var size = CGSize.zero
    guard let point = value(kAXPositionAttribute), AXValueGetValue(point, .cgPoint, &position),
        let extent = value(kAXSizeAttribute), AXValueGetValue(extent, .cgSize, &size)
    else { return nil }
    return CGRect(origin: position, size: size)
}

// MARK: Application menus (ABI 4)

/// The menu bar of the application owning `windowID`, walked in order with
/// a number for each enabled leaf item (as `frames_mac_app_menus` lists
/// them), calling `visit` on each element; stops when it returns true.
private func walkMenus(
    _ windowID: UInt32, limit: Int = 1024,
    leaf: (AXUIElement, UInt32) -> Bool = { _, _ in false }
) -> [[String: Any]]? {
    let info = CGWindowListCopyWindowInfo([.optionIncludingWindow], CGWindowID(windowID)) as? [[String: Any]] ?? []
    guard let pid = info.first?[kCGWindowOwnerPID as String] as? Int32 else { return nil }
    let app = AXUIElementCreateApplication(pid)
    guard let bar = element(app, kAXMenuBarAttribute) else { return nil }
    var next: UInt32 = 0
    var count = 0
    var stop = false
    func items(_ menu: AXUIElement, depth: Int) -> [[String: Any]] {
        var out: [[String: Any]] = []
        for child in children(menu) {
            if stop || count >= limit || depth >= 8 { break }
            count += 1
            let title = string(child, kAXTitleAttribute) ?? ""
            let enabled = bool(child, kAXEnabledAttribute) ?? true
            let submenu = children(child).first
            if title.isEmpty && submenu == nil {
                out.append(["id": 0, "title": "", "enabled": false, "checked": false, "separator": true])
                continue
            }
            var entry: [String: Any] = ["title": title, "enabled": enabled, "checked": !(string(child, kAXMenuItemMarkCharAttribute) ?? "").isEmpty]
            if let submenu {
                entry["id"] = 0
                entry["children"] = items(submenu, depth: depth + 1)
            } else if enabled {
                next += 1
                entry["id"] = next
                if leaf(child, next) {
                    stop = true
                }
            } else {
                entry["id"] = 0
            }
            out.append(entry)
        }
        return out
    }
    var top: [[String: Any]] = []
    // The first menu bar item is the Apple menu: not the application's.
    for item in children(bar).dropFirst() {
        if stop { break }
        guard let title = string(item, kAXTitleAttribute), !title.isEmpty, let menu = children(item).first else { continue }
        top.append(["id": 0, "title": title, "enabled": true, "checked": false, "children": items(menu, depth: 1)])
    }
    return top
}

private func element(_ parent: AXUIElement, _ attribute: String) -> AXUIElement? {
    var raw: CFTypeRef?
    guard AXUIElementCopyAttributeValue(parent, attribute as CFString, &raw) == .success, let raw,
        CFGetTypeID(raw) == AXUIElementGetTypeID()
    else { return nil }
    return unsafeDowncast(raw, to: AXUIElement.self)
}

private func children(_ parent: AXUIElement) -> [AXUIElement] {
    var raw: CFTypeRef?
    guard AXUIElementCopyAttributeValue(parent, kAXChildrenAttribute as CFString, &raw) == .success else { return [] }
    return raw as? [AXUIElement] ?? []
}

private func string(_ parent: AXUIElement, _ attribute: String) -> String? {
    var raw: CFTypeRef?
    guard AXUIElementCopyAttributeValue(parent, attribute as CFString, &raw) == .success else { return nil }
    return raw as? String
}

private func bool(_ parent: AXUIElement, _ attribute: String) -> Bool? {
    var raw: CFTypeRef?
    guard AXUIElementCopyAttributeValue(parent, attribute as CFString, &raw) == .success else { return nil }
    return (raw as? NSNumber)?.boolValue
}

/// The application menus of the app owning `windowID` as UTF-8 JSON (a
/// tree of {"id", "title", "enabled", "checked", "separator", "children"};
/// leaf items numbered in order). Written to `buffer` when it fits; returns
/// its length either way, 0 without Accessibility or menus.
@_cdecl("frames_mac_app_menus")
public func framesMacAppMenus(_ windowID: UInt32, _ buffer: UnsafeMutablePointer<UInt8>?, _ capacity: Int) -> Int {
    guard let tree = walkMenus(windowID), !tree.isEmpty,
        let json = try? JSONSerialization.data(withJSONObject: tree)
    else { return 0 }
    if let buffer, json.count <= capacity {
        json.copyBytes(to: buffer, count: json.count)
    }
    return json.count
}

/// Presses leaf item `id` (as numbered by `frames_mac_app_menus`).
@_cdecl("frames_mac_press_menu")
public func framesMacPressMenu(_ windowID: UInt32, _ id: UInt32) -> Bool {
    var pressed = false
    _ = walkMenus(windowID) { element, number in
        guard number == id else { return false }
        pressed = AXUIElementPerformAction(element, kAXPressAction as CFString) == .success
        return true
    }
    return pressed
}
