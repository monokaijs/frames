import ApplicationServices
import CoreGraphics
import Foundation

/// AppKit gives sheets a WindowServer surface with a large transparent
/// shadow margin. AX describes the actual sheet. Accept only a centred,
/// uniform inset so an unrelated child cannot crop a normal window.
func verifiedSheetContent(frame: CGRect, candidates: [CGRect]) -> CGRect? {
    candidates.first { candidate in
        let dx = (frame.width - candidate.width) / 2
        let dy = (frame.height - candidate.height) / 2
        return candidate.width >= 2 && candidate.height >= 2 && dx >= 4 && dy >= 4
            && abs(dx - dy) <= 2 && abs(frame.midX - candidate.midX) <= 2
            && abs(frame.midY - candidate.midY) <= 2 && frame.insetBy(dx: -2, dy: -2).contains(candidate)
    }
}

private final class SheetInsets: @unchecked Sendable {
    private struct Insets {
        let pid: Int32
        let left: CGFloat
        let top: CGFloat
        let right: CGFloat
        let bottom: CGFloat
    }
    private let lock = NSLock()
    private var values: [UInt32: Insets] = [:]

    func bounds(id: UInt32, pid: Int32, frame: CGRect) -> CGRect? {
        lock.lock()
        defer { lock.unlock() }
        guard let inset = values[id], inset.pid == pid,
            frame.width > inset.left + inset.right, frame.height > inset.top + inset.bottom else { return nil }
        return CGRect(x: frame.minX + inset.left, y: frame.minY + inset.top,
                      width: frame.width - inset.left - inset.right, height: frame.height - inset.top - inset.bottom)
    }

    func remember(id: UInt32, pid: Int32, frame: CGRect, content: CGRect) {
        lock.lock()
        defer { lock.unlock() }
        if values.count >= 256 { values.removeAll() }
        values[id] = Insets(pid: pid, left: content.minX - frame.minX, top: content.minY - frame.minY,
                            right: frame.maxX - content.maxX, bottom: frame.maxY - content.maxY)
    }
}

private let sheetInsets = SheetInsets()

/// Shared by catalog placement and capture cropping: both must use the same
/// rectangle, including after the sheet moves with its document window.
func sheetContentBounds(_ id: UInt32, frame: CGRect, processID: Int32? = nil) -> CGRect? {
    guard AXIsProcessTrusted() else { return nil }
    let pid: Int32
    if let processID {
        pid = processID
    } else {
        let info = CGWindowListCopyWindowInfo([.optionIncludingWindow], id) as? [[String: Any]] ?? []
        guard let entry = info.first, (entry[kCGWindowName as String] as? String ?? "").isEmpty,
            let owner = entry[kCGWindowOwnerPID as String] as? Int32 else { return nil }
        pid = owner
    }
    if let cached = sheetInsets.bounds(id: id, pid: pid, frame: frame) { return cached }
    let app = AXUIElementCreateApplication(pid)
    AXUIElementSetMessagingTimeout(app, 0.1)
    var raw: CFTypeRef?
    guard AXUIElementCopyAttributeValue(app, kAXWindowsAttribute as CFString, &raw) == .success,
        let windows = raw as? [AXUIElement] else { return nil }
    var candidates: [CGRect] = []
    var remaining = 96
    func collectSheets(_ element: AXUIElement, depth: Int) {
        guard depth < 3, remaining > 0 else { return }
        var rawChildren: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, kAXChildrenAttribute as CFString, &rawChildren) == .success,
            let children = rawChildren as? [AXUIElement] else { return }
        for child in children.prefix(remaining) {
            remaining -= 1
            AXUIElementSetMessagingTimeout(child, 0.1)
            var role: CFTypeRef?
            _ = AXUIElementCopyAttributeValue(child, kAXRoleAttribute as CFString, &role)
            if role as? String == "AXSheet" {
                if let bounds = axFrame(child) { candidates.append(bounds) }
            } else {
                collectSheets(child, depth: depth + 1)
            }
            if remaining <= 0 { break }
        }
    }
    for window in windows.prefix(64) {
        AXUIElementSetMessagingTimeout(window, 0.1)
        var role: CFTypeRef?
        _ = AXUIElementCopyAttributeValue(window, kAXRoleAttribute as CFString, &role)
        if let bounds = axFrame(window) {
            if role as? String == "AXSheet" {
                candidates.append(bounds)
            } else if abs(bounds.minX - frame.minX) < 2 && abs(bounds.minY - frame.minY) < 2
                && abs(bounds.width - frame.width) < 2 && abs(bounds.height - frame.height) < 2 {
                return nil
            }
        }
        var sheets: CFTypeRef?
        if AXUIElementCopyAttributeValue(window, "AXSheets" as CFString, &sheets) == .success,
            let sheets = sheets as? [AXUIElement] {
            for sheet in sheets.prefix(32) {
                AXUIElementSetMessagingTimeout(sheet, 0.1)
                if let bounds = axFrame(sheet) { candidates.append(bounds) }
            }
        }
        // AppKit also exposes attached sheets as AXChildren; AXSheets is
        // not supported by every macOS release or application.
        collectSheets(window, depth: 0)
    }
    // Some sheets have no extra WindowServer shadow surface. They are
    // still sheets; keep the frame unchanged rather than treating them as
    // modeless dialogs. Candidates here have already been verified as AXSheet.
    guard let content = verifiedSheetContent(frame: frame, candidates: candidates)
        ?? candidates.first(where: { $0 == frame }) else { return nil }
    sheetInsets.remember(id: id, pid: pid, frame: frame, content: content)
    return content
}
