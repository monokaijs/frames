import AppKit
import CFramesMac
import Foundation

// The viewer's connection menus (ABI 5), as v1 had them: the menu bar's
// Frames menu, the shared application's menus and a Devices menu, also
// popped up from each window's title-bar chevron. They are built from the
// session's state (frames-next `control::Snapshot`, as JSON) and send what
// the user chose back as a `control::Command` (JSON).

/// Makes this process a regular app with a Dock icon and the viewer's menu
/// bar. Call once a viewer window exists.
@_cdecl("frames_mac_viewer_app")
public func framesMacViewerApp() {
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            let app = NSApplication.shared
            app.setActivationPolicy(.regular)
            if app.delegate == nil { app.delegate = viewerAppDelegate }
            installViewerMenu()
            app.activate(ignoringOtherApps: true)
            if !observingKeyWindows {
                observingKeyWindows = true
                // The front viewer window's options and host application
                // menus join the menu bar.
                NotificationCenter.default.addObserver(
                    forName: NSWindow.didBecomeKeyNotification, object: nil, queue: .main
                ) { _ in MainActor.assumeIsolated { installViewerMenu() } }
            }
        }
    }
}

@_cdecl("frames_mac_viewer_set_state")
public func framesMacViewerSetState(_ utf8: UnsafePointer<UInt8>?, _ length: Int) {
    guard let utf8 else { return }
    let data = Data(bytes: utf8, count: length)
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            viewerState = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
            refreshViewerMenus()
            let transfers = viewerState["transfers"] as? [String] ?? []
            let received = viewerState["received"] as? [String] ?? []
            if !transfers.isEmpty || !received.isEmpty || shelfOpened {
                shelfOpened = true
                FileShelf.shared.update(transfers: transfers, received: received)
            }
        }
    }
}

@_cdecl("frames_mac_viewer_set_command_handler")
public func framesMacViewerSetCommandHandler(_ context: UnsafeMutableRawPointer?, _ callback: FramesMacCommandCallback?) {
    commands.set(context: context, callback: callback)
}

/// Sends a `control::Command` (as a JSON object) to the Rust viewer.
func sendViewerCommand(_ command: [String: Any]) {
    commands.send(command)
}

/// Where menu choices go (the Rust viewer's handler).
final class CommandSink: @unchecked Sendable {
    private let lock = NSLock()
    private var context: UnsafeMutableRawPointer?
    private var callback: FramesMacCommandCallback?

    func set(context: UnsafeMutableRawPointer?, callback: FramesMacCommandCallback?) {
        lock.lock()
        self.context = context
        self.callback = callback
        lock.unlock()
    }

    func send(_ command: [String: Any]) {
        guard let json = try? JSONSerialization.data(withJSONObject: command) else { return }
        lock.lock()
        defer { lock.unlock() }
        json.withUnsafeBytes { bytes in
            callback?(context, bytes.baseAddress?.assumingMemoryBound(to: UInt8.self), bytes.count)
        }
    }
}

private let commands = CommandSink()

@MainActor var viewerState: [String: Any] = [:]
@MainActor private var observingKeyWindows = false
/// The received files shelf exists (a fetch started this session).
@MainActor private var shelfOpened = false
/// Menus open now: the menu bar is rebuilt once they close.
@MainActor private var menusOpen = 0
@MainActor private var menuStale = false

/// The state changed: rebuild the menu bar (after open menus close).
@MainActor
func refreshViewerMenus() {
    guard NSApp.mainMenu != nil, NSApp.activationPolicy() == .regular else { return }
    installViewerMenu()
}

@MainActor
private func installViewerMenu() {
    if menusOpen > 0 {
        menuStale = true
        return
    }
    menuStale = false
    NSApp.mainMenu = menuBar()
}

@MainActor
private let menuTarget = MenuTarget()

// MARK: Building the menus

@MainActor
private func menuBar() -> NSMenu {
    let bar = NSMenu()
    bar.autoenablesItems = false
    let appItem = NSMenuItem()
    appItem.submenu = framesMenu(for: frontView())
    bar.addItem(appItem)
    // The shared application's own menus (its window is in front).
    for item in hostMenuItems(frontView()) { bar.addItem(item) }
    let devicesItem = NSMenuItem()
    devicesItem.submenu = devicesMenu()
    bar.addItem(devicesItem)
    return bar
}

/// The title-bar chevron's menu: the Frames menu, then the application's
/// menus and Devices as submenus.
@MainActor
private func connectionMenu(for view: InputView) -> NSMenu {
    let menu = framesMenu(for: view)
    let hosts = hostMenuItems(view)
    if !hosts.isEmpty {
        menu.insertItem(.separator(), at: menu.items.count - 1)
    }
    for item in hosts { menu.insertItem(item, at: menu.items.count - 1) }
    let devicesItem = NSMenuItem(title: "Devices", action: nil, keyEquivalent: "")
    devicesItem.image = menuSymbol("desktopcomputer")
    devicesItem.submenu = devicesMenu()
    menu.insertItem(devicesItem, at: menu.items.count - 1)
    menu.insertItem(.separator(), at: menu.items.count - 1)
    return showingMenuImages(menu)
}

private func number(_ value: Any?) -> UInt64? { (value as? NSNumber)?.uint64Value }
private func flag(_ value: Any?) -> Bool { (value as? NSNumber)?.boolValue ?? false }

@MainActor
private func windows() -> [[String: Any]] { viewerState["windows"] as? [[String: Any]] ?? [] }

@MainActor
private func entry(for view: InputView?) -> [String: Any]? {
    guard let view else { return nil }
    return windows().first { number($0["id"]) == view.menuID }
}

@MainActor
func framesMenu(for view: InputView?) -> NSMenu {
    let menu = NSMenu(title: "Frames")
    menu.autoenablesItems = false
    menu.delegate = menuTarget
    let quality = Int(number(viewerState["quality"]) ?? 0)
    let qualityMenu = NSMenu()
    qualityMenu.autoenablesItems = false
    for (index, title) in ["High (sharp text, native resolution)", "Medium (balanced)", "Low (lower bandwidth)"]
        .enumerated()
    {
        qualityMenu.addItem(
            item(title, ["command": "quality", "quality": index], checked: quality == index))
    }
    submenu(menu, "Image quality", qualityMenu)
    let list = windows().filter { !flag($0["popup"]) }
    let windowMenu = NSMenu()
    windowMenu.autoenablesItems = false
    for window in list {
        guard let id = number(window["id"]) else { continue }
        let title = window["title"] as? String ?? "Remote window"
        let app = window["app"] as? String ?? ""
        let shown = flag(window["shown"])
        windowMenu.addItem(
            item(
                app.isEmpty || title.contains(app) ? title : "\(title) · \(app)",
                ["command": "show", "id": id, "shown": !shown], checked: shown))
    }
    if list.isEmpty {
        // A host that cannot share says why (a locked Mac).
        windowMenu.addItem(disabled(viewerState["empty_reason"] as? String ?? "No remote windows available"))
    }
    submenu(menu, "Windows", windowMenu)
    menu.addItem(item("Show all windows", ["command": "show_all", "shown": true], enabled: !list.isEmpty))
    menu.addItem(
        item(
            "Hide all windows", ["command": "show_all", "shown": false],
            enabled: list.contains { flag($0["shown"]) }))
    let current = entry(for: view)
    if let current, let id = number(current["id"]), !flag(current["desktop"]) {
        menu.addItem(item("Ignore / hide this window", ["command": "show", "id": id, "shown": false]))
    }
    menu.addItem(.separator())
    let view = viewerState["view"] as? String ?? "windows"
    menu.addItem(item("Per-window view", ["command": "view", "view": "windows"], checked: view == "windows"))
    menu.addItem(item("Full desktop view", ["command": "view", "view": "desktop"], checked: view == "desktop", enabled: !flag(viewerState["only_app"])))
    let cursor = flag(viewerState["remote_cursor"])
    menu.addItem(item("Show remote cursor", ["command": "remote_cursor", "show": !cursor], checked: cursor))
    menu.addItem(
        item("Show debug overlay (Control+Option+F10)", ["command": "stats"], checked: flag(viewerState["stats"])))
    // One window with isolated input (games), as v1's shared sessions:
    // relative mouse and the game's compatibility instead of window options.
    if let compatibility = viewerState["compatibility"] as? String, let current, let id = number(current["id"]) {
        menu.addItem(.separator())
        menu.addItem(
            item(
                "Toggle relative mouse (Control+Option+F11)",
                ["command": "window", "id": id, "action": ["action": "toggle_relative"]]))
        let info = NSMenuItem(
            title: "Window compatibility", action: #selector(MenuTarget.showCompatibility(_:)), keyEquivalent: "")
        info.target = menuTarget
        info.image = menuSymbol("info.circle")
        info.representedObject = compatibility
        menu.addItem(info)
    } else if let current, let id = number(current["id"]) {
        menu.addItem(.separator())
        let prefs = current["prefs"] as? [String: Any] ?? [:]
        let desktop = flag(current["desktop"])
        let resizable = flag(current["resizable"]) && !desktop
        let resizeRemote = flag(prefs["resize_remote"]) && resizable
        let physical = flag(prefs["physical"])
        let platform = viewerState["host_platform"] as? String ?? ""
        let swap = !physical && ((prefs["swap"] as? NSNumber)?.boolValue ?? (!platform.isEmpty && platform != "macos"))
        func action(_ name: String) -> [String: Any] {
            ["command": "window", "id": id, "action": ["action": name]]
        }
        menu.addItem(
            item(
                "Show remote window header", action("toggle_header"), checked: flag(prefs["header"]),
                enabled: !desktop))
        menu.addItem(item("Resize remote app", action("resize_remote"), checked: resizeRemote, enabled: resizable))
        menu.addItem(item("Stretch locally", action("stretch_local"), checked: !resizeRemote))
        menu.addItem(item("Toggle aspect fit / fill", action("toggle_fill"), checked: flag(prefs["fill"])))
        menu.addItem(item("Toggle Control / Command swap", action("toggle_swap"), checked: swap))
        menu.addItem(item("Toggle physical keys (terminal)", action("toggle_physical"), checked: physical))
        menu.addItem(item("Refresh application menus", action("refresh_menus"), enabled: !desktop))
        let fetch = NSMenuItem(title: "Fetch remote file...", action: #selector(MenuTarget.fetchFile(_:)), keyEquivalent: "")
        fetch.target = menuTarget
        fetch.image = menuSymbol("folder")
        fetch.representedObject = NSNumber(value: id)
        menu.addItem(fetch)
    }
    menu.addItem(.separator())
    let send = NSMenuItem(title: "Send File…", action: #selector(MenuTarget.sendFile(_:)), keyEquivalent: "o")
    send.target = menuTarget
    send.image = menuSymbol("paperplane")
    send.isEnabled = frontView() != nil
    menu.addItem(send)
    menu.addItem(.separator())
    let disconnect = item("Disconnect", ["command": "disconnect"])
    disconnect.keyEquivalent = "q"
    disconnect.keyEquivalentModifierMask = [.command]
    menu.addItem(disconnect)
    return showingMenuImages(menu)
}

@MainActor
func devicesMenu() -> NSMenu {
    let devices = NSMenu(title: "Devices")
    devices.autoenablesItems = false
    devices.delegate = menuTarget
    let gamepads = NSMenu()
    gamepads.autoenablesItems = false
    let pads = viewerState["gamepads"] as? [[String: Any]] ?? []
    if pads.isEmpty { gamepads.addItem(disabled("No mapped gamepads found")) }
    for pad in pads.prefix(32) {
        guard let id = pad["id"] as? String else { continue }
        let forwarded = flag(pad["forwarded"])
        let status = (pad["status"] as? String).map { " — \($0)" } ?? ""
        gamepads.addItem(
            item(
                (pad["name"] as? String ?? "Gamepad") + status, ["command": "gamepad", "id": id, "on": !forwarded],
                checked: forwarded))
    }
    if let error = viewerState["gamepad_error"] as? String { gamepads.addItem(disabled(error)) }
    let setup = NSMenuItem(
        title: "Set up gamepad driver on Windows host...", action: #selector(MenuTarget.gamepadSetup(_:)),
        keyEquivalent: "")
    setup.target = menuTarget
    setup.image = menuSymbol("gearshape")
    gamepads.addItem(setup)
    submenu(devices, "Gamepads", gamepads)
    let usb = NSMenu()
    usb.autoenablesItems = false
    let usbDevices = viewerState["usb"] as? [[String: Any]] ?? []
    if usbDevices.isEmpty {
        usb.addItem(disabled("USB passthrough is Windows → Windows in this release"))
    }
    for device in usbDevices.prefix(32) {
        guard let id = device["id"] as? String else { continue }
        let forwarded = flag(device["forwarded"])
        let status = (device["status"] as? String).map { " (\($0))" } ?? ""
        usb.addItem(
            item(
                "\(id) — \(device["name"] as? String ?? "USB device")\(status)",
                ["command": "usb", "id": id, "on": !forwarded], checked: forwarded))
    }
    submenu(devices, "USB devices", usb)
    devices.addItem(item("Refresh devices", ["command": "refresh_devices"]))
    let adb = flag(viewerState["adb"])
    devices.addItem(item("Android ADB bridge", ["command": "adb", "on": !adb], checked: adb))
    let support = NSMenuItem(
        title: "Device forwarding support...", action: #selector(MenuTarget.deviceSupport(_:)), keyEquivalent: "")
    support.target = menuTarget
    support.image = menuSymbol("info.circle")
    devices.addItem(support)
    return showingMenuImages(devices)
}

/// The front window's host application menus, as top-level items.
@MainActor
private func hostMenuItems(_ view: InputView?) -> [NSMenuItem] {
    var items: [NSMenuItem] = []
    for top in view?.hostMenus ?? [] {
        guard let title = top["title"] as? String, let children = top["children"] as? [[String: Any]] else { continue }
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        item.submenu = hostMenu(title, children)
        items.append(item)
    }
    return items
}

@MainActor
private func hostMenu(_ title: String, _ items: [[String: Any]]) -> NSMenu {
    let menu = NSMenu(title: title)
    menu.autoenablesItems = false
    menu.delegate = menuTarget
    for entry in items.prefix(200) {
        if entry["separator"] as? Bool == true {
            menu.addItem(.separator())
            continue
        }
        let title = (entry["title"] as? String ?? "").components(separatedBy: "\t")[0].replacingOccurrences(
            of: "&", with: "")
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
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
private func item(_ title: String, _ command: [String: Any], checked: Bool = false, enabled: Bool = true) -> NSMenuItem {
    let item = NSMenuItem(title: title, action: #selector(MenuTarget.send(_:)), keyEquivalent: "")
    item.target = menuTarget
    item.representedObject = command as NSDictionary
    item.state = checked ? .on : .off
    item.isEnabled = enabled
    item.image = commandSymbol(command).flatMap(menuSymbol)
    return item
}

@MainActor
private func disabled(_ title: String) -> NSMenuItem {
    let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
    item.isEnabled = false
    return item
}

@MainActor
private func submenu(_ menu: NSMenu, _ title: String, _ child: NSMenu) {
    let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
    item.submenu = child
    let symbols = ["Image quality": "display", "Windows": "macwindow.on.rectangle", "Gamepads": "gamecontroller", "USB devices": "cable.connector"]
    item.image = symbols[title].flatMap(menuSymbol)
    menu.addItem(item)
}

@MainActor
private func showingMenuImages(_ menu: NSMenu) -> NSMenu {
    // Use the runtime setter so this also builds with SDKs predating the
    // property. #available alone cannot hide a symbol absent from an SDK.
    let selector = NSSelectorFromString("setPreferredImageVisibility:")
    typealias SetVisibility = @convention(c) (AnyObject, Selector, Int) -> Void
    for item in menu.items {
        if item.image != nil, item.responds(to: selector) {
            let set = unsafeBitCast(item.method(for: selector), to: SetVisibility.self)
            set(item, selector, 1) // NSMenuItemImageVisibilityVisible
        }
        if let submenu = item.submenu { _ = showingMenuImages(submenu) }
    }
    return menu
}

@MainActor
private func menuSymbol(_ name: String) -> NSImage? {
    let image = NSImage(systemSymbolName: name, accessibilityDescription: nil)
    image?.size = NSSize(width: 16, height: 16)
    image?.isTemplate = true
    return image
}

private func commandSymbol(_ command: [String: Any]) -> String? {
    switch command["command"] as? String {
    case "quality": return "display"
    case "show", "show_all": return flag(command["shown"]) ? "eye" : "eye.slash"
    case "view": return command["view"] as? String == "desktop" ? "desktopcomputer" : "macwindow.on.rectangle"
    case "remote_cursor": return "cursorarrow"
    case "stats": return "chart.bar"
    case "disconnect": return "power"
    case "gamepad": return "gamecontroller"
    case "usb": return "cable.connector"
    case "adb": return "network"
    case "refresh_devices": return "arrow.clockwise"
    case "window":
        switch (command["action"] as? [String: Any])?["action"] as? String {
        case "toggle_swap", "toggle_physical": return "keyboard"
        case "refresh_menus": return "arrow.clockwise"
        case "toggle_relative": return "cursorarrow"
        case "toggle_header": return "macwindow"
        default: return "arrow.up.left.and.arrow.down.right"
        }
    default: return nil
    }
}

/// The front viewer window's input view, if a viewer window is in front.
@MainActor
private func frontView() -> InputView? {
    (NSApplication.shared.keyWindow ?? NSApplication.shared.mainWindow)?.contentView as? InputView
}

// MARK: The Dock

/// One process has one Dock tile, so v1's tile per remote application (a
/// helper process each) becomes this tile's menu: each remote application
/// with its windows (choose one to bring it forward, or show it again when
/// hidden) and "Quit <application>", which closes that application's
/// windows on the host as their close buttons would. Clicking the tile with
/// every window hidden shows them all.
@MainActor
final class ViewerAppDelegate: NSObject, NSApplicationDelegate {
    func applicationDockMenu(_ sender: NSApplication) -> NSMenu? {
        let menu = NSMenu()
        menu.autoenablesItems = false
        var apps: [String] = []
        var byApp: [String: [[String: Any]]] = [:]
        for window in windows() where !flag(window["popup"]) {
            let app = window["app"] as? String ?? ""
            let name = app.isEmpty ? "Remote windows" : app
            if byApp[name] == nil { apps.append(name) }
            byApp[name, default: []].append(window)
        }
        for app in apps {
            let entries = byApp[app] ?? []
            let child = NSMenu()
            child.autoenablesItems = false
            for window in entries {
                guard let id = number(window["id"]) else { continue }
                let shown = flag(window["shown"])
                let entry = NSMenuItem(
                    title: window["title"] as? String ?? app, action: #selector(bringForward(_:)), keyEquivalent: "")
                entry.target = self
                entry.representedObject = NSNumber(value: id)
                entry.state = shown ? .on : .off
                child.addItem(entry)
            }
            child.addItem(.separator())
            if entries.contains(where: { flag($0["desktop"]) }) {
                child.addItem(item("Disconnect", ["command": "disconnect"]))
            } else {
                let quit = NSMenuItem(title: "Quit \(app)", action: #selector(quitApp(_:)), keyEquivalent: "")
                quit.target = self
                quit.representedObject = app
                child.addItem(quit)
            }
            let top = NSMenuItem(title: app, action: nil, keyEquivalent: "")
            top.submenu = child
            menu.addItem(top)
        }
        if apps.isEmpty {
            menu.addItem(disabled(viewerState["empty_reason"] as? String ?? "No remote windows available"))
        }
        return menu
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        if !hasVisibleWindows { sendViewerCommand(["command": "show_all", "shown": true]) }
        return true
    }

    @objc func bringForward(_ sender: NSMenuItem) {
        guard let id = (sender.representedObject as? NSNumber)?.uint64Value else { return }
        if let view = viewerViews().first(where: { $0.menuID == id }), let window = view.window {
            if window.isMiniaturized { window.deminiaturize(nil) }
            NSApp.activate(ignoringOtherApps: true)
            window.makeKeyAndOrderFront(nil)
        } else {
            sendViewerCommand(["command": "show", "id": id, "shown": true])
        }
    }

    /// Asks the host to close each of the application's windows, as their
    /// close buttons would (it may ask to save).
    @objc func quitApp(_ sender: NSMenuItem) {
        guard let app = sender.representedObject as? String else { return }
        let ids = Set(
            windows().filter { ($0["app"] as? String ?? "") == app && !flag($0["popup"]) }.compactMap { number($0["id"]) })
        for view in viewerViews() where ids.contains(view.menuID) {
            view.window?.performClose(nil)
        }
    }
}

@MainActor private let viewerAppDelegate = ViewerAppDelegate()

/// Every open viewer window's view.
@MainActor
private func viewerViews() -> [InputView] {
    NSApplication.shared.windows.compactMap { $0.contentView as? InputView }
}

// MARK: The title-bar chevron

/// Adds the "Connection menu" chevron at the right of the window's title bar.
@MainActor
func addConnectionButton(_ window: NSWindow, view: InputView) {
    let image =
        NSImage(systemSymbolName: "chevron.down", accessibilityDescription: "Connection menu") ?? NSImage()
    let button = NSButton(image: image, target: view, action: #selector(InputView.openConnectionMenu(_:)))
    button.isBordered = false
    button.toolTip = "Connection menu"
    button.frame = NSRect(x: 0, y: 0, width: 28, height: 22)
    let container = NSView(frame: NSRect(x: 0, y: 0, width: 36, height: 22))
    button.frame.origin = NSPoint(x: 4, y: 0)
    container.addSubview(button)
    let accessory = NSTitlebarAccessoryViewController()
    accessory.view = container
    accessory.layoutAttribute = .right
    window.addTitlebarAccessoryViewController(accessory)
}

extension InputView {
    @objc func openConnectionMenu(_ button: NSButton) {
        releaseKeys()
        let menu = connectionMenu(for: self)
        let y = button.isFlipped ? button.bounds.maxY + 4 : button.bounds.minY - 4
        menu.popUp(positioning: nil, at: NSPoint(x: 0, y: y), in: button)
    }
}

// MARK: Actions

@MainActor
final class MenuTarget: NSObject, NSMenuDelegate {
    func menuWillOpen(_ menu: NSMenu) {
        if menusOpen == 0 { frontView()?.releaseKeys() }
        menusOpen += 1
    }

    func menuDidClose(_ menu: NSMenu) {
        menusOpen = max(0, menusOpen - 1)
        if menusOpen == 0 && menuStale {
            DispatchQueue.main.async { MainActor.assumeIsolated { installViewerMenu() } }
        }
    }

    @objc func send(_ sender: NSMenuItem) {
        guard let command = sender.representedObject as? [String: Any] else { return }
        commands.send(command)
    }

    /// An item of the shared application's menus: run on the host.
    @objc func hostItem(_ sender: NSMenuItem) {
        frontView()?.relay.send(Int32(FRAMES_MAC_INPUT_MENU), Int32(sender.tag), at: ProcessInfo.processInfo.systemUptime)
    }

    @objc func sendFile(_ sender: NSMenuItem) {
        guard let view = frontView(), let window = view.window else { return }
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = true
        panel.message = "Choose files to send to the remote computer's Downloads/Frames folder."
        // A sheet: video keeps playing while it is open.
        panel.beginSheetModal(for: window) { [weak view] response in
            guard response == .OK else { return }
            MainActor.assumeIsolated { view?.drops?.add(panel.urls.map(\.path)) }
        }
    }

    @objc func fetchFile(_ sender: NSMenuItem) {
        guard let id = (sender.representedObject as? NSNumber)?.uint64Value else { return }
        let alert = NSAlert()
        alert.messageText = "Fetch remote file"
        alert.informativeText = "Enter a path relative to the remote user's home folder."
        let input = NSTextField(frame: NSRect(x: 0, y: 0, width: 400, height: 24))
        input.stringValue = "Downloads/"
        alert.accessoryView = input
        alert.addButton(withTitle: "Download")
        alert.addButton(withTitle: "Cancel")
        sheet(alert) { response in
            let path = input.stringValue.trimmingCharacters(in: .whitespaces)
            guard response == .alertFirstButtonReturn, !path.isEmpty else { return }
            commands.send(["command": "window", "id": id, "action": ["action": "fetch", "path": path]])
        }
        alert.window.initialFirstResponder = input
    }

    @objc func showCompatibility(_ sender: NSMenuItem) {
        let alert = NSAlert()
        alert.messageText = "Window compatibility"
        alert.informativeText = sender.representedObject as? String ?? "Untested"
        sheet(alert) { _ in }
    }

    @objc func deviceSupport(_ sender: NSMenuItem) {
        let alert = NSAlert()
        alert.messageText = "Device forwarding"
        alert.informativeText =
            "Gamepads: pair a controller locally over USB or Bluetooth, then choose it under Devices → Gamepads. Receiving requires a Windows host with ViGEmBus. Standard controls and rumble are supported. Generic USB passthrough is available between Windows computers. The Android ADB bridge remains available on both platforms."
        sheet(alert) { _ in }
    }

    @objc func gamepadSetup(_ sender: NSMenuItem) {
        let alert = NSAlert()
        alert.messageText = "Gamepad driver on the Windows host"
        alert.informativeText =
            "Gamepads reach a Windows host as Xbox 360 controllers through ViGEmBus. Install it on the host (github.com/nefarius/ViGEmBus releases), then choose a controller under Devices → Gamepads."
        sheet(alert) { _ in }
    }

    /// Shows `alert` as a sheet on the front window (video keeps playing),
    /// or as a panel when no viewer window is open.
    private func sheet(_ alert: NSAlert, _ done: @escaping @MainActor (NSApplication.ModalResponse) -> Void) {
        if let window = frontView()?.window {
            alert.beginSheetModal(for: window) { response in MainActor.assumeIsolated { done(response) } }
        } else {
            done(alert.runModal())
        }
    }
}
