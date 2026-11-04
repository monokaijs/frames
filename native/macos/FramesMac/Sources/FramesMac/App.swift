import AppKit
import Foundation

/// Headless host mode: Rust's `main` calls this on the process main thread
/// and it blocks inside `NSApplication.run`. Capture, coding, and the Rust
/// runtime run on their own threads. With the Slint launcher active, winit
/// owns NSApplication and this must never be called.
@_cdecl("frames_mac_run_app")
public func framesMacRunApp(_ seconds: Double) {
    precondition(Thread.isMainThread, "frames_mac_run_app must be called on the main thread")
    MainActor.assumeIsolated {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        if seconds > 0 {
            Timer.scheduledTimer(withTimeInterval: seconds, repeats: false) { _ in
                MainActor.assumeIsolated { stopApplication() }
            }
        }
        app.run()
    }
}

@_cdecl("frames_mac_stop_app")
public func framesMacStopApp() {
    DispatchQueue.main.async { MainActor.assumeIsolated { stopApplication() } }
}

@MainActor
private func stopApplication() {
    NSApp.stop(nil)
    // `stop` takes effect after the next event; post one so `run` returns now.
    if let event = NSEvent.otherEvent(
        with: .applicationDefined, location: .zero, modifierFlags: [], timestamp: 0,
        windowNumber: 0, context: nil, subtype: 0, data1: 0, data2: 0)
    {
        NSApp.postEvent(event, atStart: true)
    }
}
