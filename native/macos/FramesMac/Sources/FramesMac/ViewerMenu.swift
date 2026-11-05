import AppKit
import CFramesMac
import Foundation

/// Commands of the viewer's Frames menu, sent to the front viewer window's
/// Rust session as FRAMES_MAC_INPUT_COMMAND (a = one of these).
enum ViewerCommand: Int32 {
    case qualityHigh = 1
    case qualityMedium = 2
    case qualityLow = 3
    case toggleStatistics = 4
    case releaseInput = 5
    case disconnect = 6
}

/// Makes this process a regular app with a Dock icon and the viewer's menu
/// bar (ABI 4): Frames > Quit, and a Connection menu acting on the front
/// viewer window. Call once a viewer window exists.
@_cdecl("frames_mac_viewer_app")
public func framesMacViewerApp() {
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            let app = NSApplication.shared
            app.setActivationPolicy(.regular)
            app.mainMenu = viewerMenu()
            app.activate(ignoringOtherApps: true)
            if !observingKeyWindows {
                observingKeyWindows = true
                // The front viewer window's host application menus join
                // the menu bar.
                NotificationCenter.default.addObserver(
                    forName: NSWindow.didBecomeKeyNotification, object: nil, queue: .main
                ) { _ in MainActor.assumeIsolated { NSApp.mainMenu = viewerMenu() } }
            }
        }
    }
}

@MainActor
private var observingKeyWindows = false

/// Rebuilds the menu bar when `view`'s window is in front (its host menus
/// changed).
@MainActor
func refreshViewerMenu(for view: InputView) {
    if frontView() === view {
        NSApp.mainMenu = viewerMenu()
    }
}

@MainActor
private let menuTarget = MenuTarget()

@MainActor
private func viewerMenu() -> NSMenu {
    let bar = NSMenu()
    let appItem = NSMenuItem()
    let appMenu = NSMenu(title: "Frames")
    appMenu.addItem(item("Disconnect and Quit", .disconnect, key: "q", modifiers: [.command]))
    appItem.submenu = appMenu
    bar.addItem(appItem)

    let connectionItem = NSMenuItem()
    let connection = NSMenu(title: "Connection")
    connection.addItem(item("High Quality", .qualityHigh))
    connection.addItem(item("Medium Quality", .qualityMedium))
    connection.addItem(item("Low Quality", .qualityLow))
    connection.addItem(.separator())
    let send = NSMenuItem(title: "Send File…", action: #selector(MenuTarget.sendFile(_:)), keyEquivalent: "o")
    send.target = menuTarget
    connection.addItem(send)
    connection.addItem(item("Show Statistics", .toggleStatistics, key: String(UnicodeScalar(NSF10FunctionKey)!), modifiers: [.control, .option]))
    connection.addItem(item("Release Input", .releaseInput, key: String(UnicodeScalar(NSF12FunctionKey)!), modifiers: [.control, .option]))
    connection.addItem(.separator())
    connection.addItem(item("Disconnect", .disconnect))
    connectionItem.submenu = connection
    bar.addItem(connectionItem)
    // The shared application's own menus (its window is in front).
    for top in frontView()?.hostMenus ?? [] {
        guard let title = top["title"] as? String, let children = top["children"] as? [[String: Any]] else { continue }
        let item = NSMenuItem()
        item.submenu = hostMenu(title, children)
        bar.addItem(item)
    }
    return bar
}

@MainActor
private func hostMenu(_ title: String, _ items: [[String: Any]]) -> NSMenu {
    let menu = NSMenu(title: title)
    menu.autoenablesItems = false
    for entry in items {
        if entry["separator"] as? Bool == true {
            menu.addItem(.separator())
            continue
        }
        let item = NSMenuItem(title: entry["title"] as? String ?? "", action: nil, keyEquivalent: "")
        item.isEnabled = entry["enabled"] as? Bool ?? true
        item.state = entry["checked"] as? Bool == true ? .on : .off
        if let children = entry["children"] as? [[String: Any]], !children.isEmpty {
            item.submenu = hostMenu(item.title, children)
        } else if let id = entry["id"] as? Int, id > 0 {
            item.tag = id
            item.action = #selector(MenuTarget.hostItem(_:))
            item.target = menuTarget
        }
        menu.addItem(item)
    }
    return menu
}

@MainActor
private func item(
    _ title: String, _ command: ViewerCommand, key: String = "", modifiers: NSEvent.ModifierFlags = []
) -> NSMenuItem {
    let item = NSMenuItem(title: title, action: #selector(MenuTarget.perform(_:)), keyEquivalent: key)
    item.keyEquivalentModifierMask = modifiers
    item.tag = Int(command.rawValue)
    item.target = menuTarget
    return item
}

/// The front viewer window's input view, if a viewer window is in front.
@MainActor
private func frontView() -> InputView? {
    (NSApp.keyWindow ?? NSApp.mainWindow)?.contentView as? InputView
}

@MainActor
final class MenuTarget: NSObject {
    @objc func perform(_ sender: NSMenuItem) {
        guard let command = ViewerCommand(rawValue: Int32(sender.tag)) else { return }
        if command == .disconnect {
            // Every window of this viewer, not just the front one.
            for window in NSApp.windows {
                (window.contentView as? InputView)?.command(command)
            }
            return
        }
        frontView()?.command(command)
    }

    /// An item of the shared application's menus: run on the host.
    @objc func hostItem(_ sender: NSMenuItem) {
        frontView()?.relay.send(Int32(FRAMES_MAC_INPUT_MENU), Int32(sender.tag), at: ProcessInfo.processInfo.systemUptime)
    }

    @objc func sendFile(_ sender: NSMenuItem) {
        guard let view = frontView() else { return }
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = true
        panel.message = "Choose files to send to the remote computer's Downloads/Frames folder."
        guard panel.runModal() == .OK else { return }
        view.drops?.add(panel.urls.map(\.path))
    }
}
