import AppKit
import ApplicationServices
import CoreGraphics
import Foundation

// Host: a file the viewer dropped on its window, saved here, is dropped
// into the shared application at the same place (ABI 6), as v1's
// NativeFileDropper did: a tiny window under the point starts a real drag
// session with the file, and synthetic mouse events (Accessibility) carry
// it to the point and release it there, so the application receives an
// ordinary Finder-style drop.

/// Waits for a drop's outcome from the main thread.
private final class DropOutcome: @unchecked Sendable {
    let done = DispatchSemaphore(value: 0)
    var accepted = false
}

@MainActor
private final class FileDropper: NSView, NSDraggingSource {
    let path: String
    /// Global point, top-left origin (Quartz).
    let point: CGPoint
    let outcome: DropOutcome
    var panel: NSWindow?
    private var finished = false

    init(path: String, point: CGPoint, outcome: DropOutcome) {
        self.path = path
        self.point = point
        self.outcome = outcome
        super.init(frame: NSRect(x: 0, y: 0, width: 3, height: 3))
    }

    required init?(coder: NSCoder) { nil }

    func start() {
        // Quartz (top-left) to Cocoa (bottom-left) screen coordinates.
        let height = CGDisplayBounds(CGMainDisplayID()).height
        let window = NSWindow(
            contentRect: NSRect(x: point.x - 1, y: height - point.y - 1, width: 3, height: 3),
            styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.level = .floating
        window.contentView = self
        panel = window
        window.orderFrontRegardless()
        NSApp.activate(ignoringOtherApps: true)
        CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: point, mouseButton: .left)?
            .post(tap: .cghidEventTap)
        // No drag began (the press went elsewhere): give up.
        DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
            MainActor.assumeIsolated { self?.finish(accepted: false) }
        }
    }

    override func mouseDown(with event: NSEvent) {
        let item = NSDraggingItem(pasteboardWriter: URL(fileURLWithPath: path) as NSURL)
        item.setDraggingFrame(NSRect(x: 0, y: 0, width: 32, height: 32), contents: NSWorkspace.shared.icon(forFile: path))
        beginDraggingSession(with: [item], event: event, source: self)
        let point = self.point
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [weak self] in
            MainActor.assumeIsolated { self?.panel?.orderOut(nil) }
            CGEvent(mouseEventSource: nil, mouseType: .leftMouseDragged, mouseCursorPosition: point, mouseButton: .left)?
                .post(tap: .cghidEventTap)
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) {
                CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: point, mouseButton: .left)?
                    .post(tap: .cghidEventTap)
            }
        }
    }

    func draggingSession(_ session: NSDraggingSession, sourceOperationMaskFor context: NSDraggingContext)
        -> NSDragOperation
    { .copy }

    func draggingSession(_ session: NSDraggingSession, endedAt screenPoint: NSPoint, operation: NSDragOperation) {
        finish(accepted: !operation.isEmpty)
    }

    private func finish(accepted: Bool) {
        guard !finished else { return }
        finished = true
        panel?.orderOut(nil)
        panel = nil
        outcome.accepted = accepted
        outcome.done.signal()
        activeDropper = nil
    }
}

/// The drop in progress (kept alive until it ends).
@MainActor private var activeDropper: FileDropper?

@_cdecl("frames_mac_drop_file")
public func framesMacDropFile(_ windowID: UInt32, _ path: UnsafePointer<UInt8>?, _ length: Int, _ x: Double, _ y: Double)
    -> Bool
{
    guard let path, !Thread.isMainThread, AXIsProcessTrusted() else { return false }
    let file = String(decoding: UnsafeBufferPointer(start: path, count: length), as: UTF8.self)
    guard FileManager.default.fileExists(atPath: file) else { return false }
    // The application comes forward first, so the drop lands in it.
    if windowID != 0 { _ = framesMacActivateWindow(windowID, true) }
    let outcome = DropOutcome()
    let point = CGPoint(x: x, y: y)
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            guard activeDropper == nil else {
                outcome.done.signal()
                return
            }
            let dropper = FileDropper(path: file, point: point, outcome: outcome)
            activeDropper = dropper
            dropper.start()
        }
    }
    guard outcome.done.wait(timeout: .now() + 5) == .success else { return false }
    return outcome.accepted
}

// MARK: Session state (ABI 6)

/// v1's checks (SessionState.swift): no session, locked, or another user
/// in front.
@_cdecl("frames_mac_session_state")
public func framesMacSessionState() -> Int32 {
    guard let session = CGSessionCopyCurrentDictionary() as? [String: Any] else { return 3 }
    if session["CGSSessionScreenIsLocked"] as? Bool == true { return 1 }
    if session[kCGSessionOnConsoleKey as String] as? Bool == false { return 2 }
    return 0
}
