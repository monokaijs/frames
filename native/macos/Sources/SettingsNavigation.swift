import AppKit
import ApplicationServices

/// Local Network has no working System Settings URL anchor on macOS 27.
/// Use the existing control grant to press only its category navigation button.
/// Never request control access here or interact with permission switches.
@MainActor enum SettingsNavigation {
    static func openLocalNetwork() async -> Bool {
        let url = URL(string: "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension")!
        guard NSWorkspace.shared.open(url), AXIsProcessTrusted() else { return false }
        let privacy = Bundle(path: "/System/Library/ExtensionKit/Extensions/SecurityPrivacyExtension.appex")
        let title = privacy?.localizedString(forKey: "LOCAL_NETWORK", value: "Local Network", table: "Localizable") ?? "Local Network"
        let deadline = Date().addingTimeInterval(4)
        while Date() < deadline {
            if let settings = NSRunningApplication.runningApplications(withBundleIdentifier: "com.apple.systempreferences").first {
                let application = AXUIElementCreateApplication(settings.processIdentifier)
                AXUIElementSetMessagingTimeout(application, 0.3)
                let windows = attribute(application, kAXWindowsAttribute) as? [AXUIElement] ?? []
                var pending: [(AXUIElement, Int)] = windows.map { ($0, 0) }
                var visited = 0
                while let (element, depth) = pending.popLast(), visited < 500, Date() < deadline {
                    visited += 1
                    if attribute(element, kAXRoleAttribute) as? String == kAXButtonRole,
                       let identifier = attribute(element, kAXIdentifierAttribute) as? String {
                        let navigationID = title + "_Navigator"
                        // Settings may append the current app count to the ID.
                        if identifier == navigationID || identifier.hasPrefix(navigationID + ",") {
                            return AXUIElementPerformAction(element, kAXPressAction as CFString) == .success
                        }
                    }
                    if depth < 16, let children = attribute(element, kAXChildrenAttribute) as? [AXUIElement] {
                        pending.append(contentsOf: children.map { ($0, depth + 1) })
                    }
                }
            }
            try? await Task.sleep(nanoseconds: 100_000_000)
        }
        return false
    }

    private static func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
        var value: CFTypeRef?
        guard AXUIElementCopyAttributeValue(element, name as CFString, &value) == .success else { return nil }
        return value
    }
}
