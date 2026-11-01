import AppKit

// Approval/error helpers run in a separate process from the Rust launcher.
// App-modal alone cannot establish ordering relative to that other process.
@discardableResult
@MainActor func runFramesAlert(_ alert: NSAlert) -> NSApplication.ModalResponse {
    let owner = NSApp.keyWindow ?? NSApp.mainWindow
    if let owner, owner !== alert.window {
        owner.addChildWindow(alert.window, ordered: .above)
    }
    defer {
        if let owner, owner !== alert.window {
            owner.removeChildWindow(alert.window)
        }
    }
    alert.window.level = .modalPanel
    alert.window.center()
    NSApp.activate(ignoringOtherApps: true)
    alert.window.makeKeyAndOrderFront(nil)
    alert.window.orderFrontRegardless()
    return alert.runModal()
}
