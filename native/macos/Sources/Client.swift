import AppKit
import Metal
import QuartzCore
import CoreVideo
import ImageIO

@MainActor final class NativeClient: NSObject, NSApplicationDelegate, NSMenuDelegate {
    var host: String
    var windows: [String: RemoteProxy] = [:]
    var docks: [String: Process] = [:]
    var session = frames_session_new()
    var sequence = 0
    var fileShelf: FileShelf?
    weak var activeProxy:RemoteProxy?
    var sharedSession:[String:Any]?
    var compatibilityText="Untested"
    var virtualCursor:[String:Any]=[:]
    var viewMode="windows"
    var showRemoteCursor=true
    var adbEnabled=false
    var quality="high"
    var deviceData:[String:Any]=[:]
    var menuTracking=0
    var needsMenu=false
    var focusAfterSwitch=false
    var pendingVideo: [String: ClientVideoFrame] = [:]
    var surfaceOrder:[String]=[]
    var outsideClickMonitor:Any?
    var layingOutGroups=false
    var nextSurfaceSerial=0
    var dismissingMenuApps=Set<String>()
    func receiveVideo(_ frames: [ClientVideoFrame]) {
        for frame in frames {
            if pendingVideo[frame.key] == nil && pendingVideo.count >= 16, let key = pendingVideo.keys.first { pendingVideo.removeValue(forKey: key) }
            pendingVideo[frame.key] = frame
        }
        presentVideo()
    }
    func presentVideo() {
        for key in Array(pendingVideo.keys) {
            guard let proxy = windows[key], let frame = pendingVideo.removeValue(forKey: key) else { continue }
            proxy.view.display(frame)
            if !proxy.status.isEmpty { proxy.status = ""; proxy.refreshTitle() }
        }
    }
    init(host: String) { self.host = host; super.init() }
    deinit { if let session { frames_session_free(session) } }
    static func key(_ object: [String: Any]) -> String { "\(object["id"] ?? 0):\(object["generation"] ?? 0)" }
    func applicationDidFinishLaunching(_ notification:Notification){let timer=Timer(timeInterval:0.1,repeats:true){[weak self] _ in Task { @MainActor in if self?.sharedSession != nil{Wire.send(["event":"shared_alive"])} }};RunLoop.main.add(timer,forMode:.common);Wire.diagnostic("client_started",["pid":ProcessInfo.processInfo.processIdentifier]);installMenu()}
    // A host can temporarily have no shared windows during graph reconciliation.
    // Keep the session alive until an explicit disconnect or a transport failure.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender:NSApplication)->Bool{false}
    func applicationShouldTerminate(_ sender:NSApplication)->NSApplication.TerminateReply{Wire.diagnostic("client_terminate_requested");return .terminateNow}
    func applicationDidResignActive(_ notification:Notification){dismissRemoteMenus();for proxy in windows.values{proxy.view.releaseInput()}}
    func applicationDidBecomeActive(_ notification:Notification){dismissingMenuApps.removeAll();Wire.send(["event":"devices","data":["action":"focus","active":true]])}
    func applicationDidHide(_ notification:Notification){for proxy in windows.values{proxy.view.releaseInput()}}
    func activate(_ proxy:RemoteProxy){activeProxy=proxy;installMenu()}
    func modalBlocker(for proxy:RemoteProxy)->RemoteProxy? {
        let modals=windows.values.filter{$0.remote["modal"] as? Bool == true}.sorted{$0.surfaceSerial>$1.surfaceSerial}
        func descends(_ child:RemoteProxy,from ancestor:RemoteProxy)->Bool {
            var current=child,seen=Set<String>()
            while let id=current.remote["parent"] as? [String:Any],seen.insert(Self.key(id)).inserted,let parent=windows[Self.key(id)] {
                if parent === ancestor{return true};current=parent
            }
            return false
        }
        func direct(_ target:RemoteProxy)->RemoteProxy? {
            for candidate in modals where candidate !== target {
                if descends(candidate,from:target){return candidate}
                if !(candidate.remote["parent"] is [String:Any]),let app=candidate.remote["app_id"] as? String,
                   target.remote["app_id"] as? String == app,!descends(target,from:candidate),
                   target.remote["modal"] as? Bool != true || candidate.surfaceSerial>target.surfaceSerial {return candidate}
            }
            return nil
        }
        // Follow nested modality to the final dialog so focus forwarding cannot
        // oscillate between an outer alert and its own sheet.
        var target=proxy,seen:Set<String>=[Self.key(proxy.id)]
        while let next=direct(target),seen.insert(Self.key(next.id)).inserted{target=next}
        return target === proxy ? nil:target
    }
    func dismissRemoteMenus() {
        for key in surfaceOrder.reversed() {
            guard let proxy=windows[key],proxy.remote["role"] as? String == "menu" else{continue}
            let app=proxy.remote["app_id"] as? String ?? key
            guard dismissingMenuApps.insert(app).inserted else{continue}
            // Cancel each native menu session once. Repeated Escape events can
            // reach the underlying dialog after the entire menu chain closes.
            proxy.send("input",["input":["kind":"key","usage":41,"down":true]])
            proxy.send("input",["input":["kind":"key","usage":41,"down":false]])
        }
    }
    func installOutsideClickMonitor() {
        guard outsideClickMonitor==nil else{return}
        outsideClickMonitor=NSEvent.addLocalMonitorForEvents(matching:[.leftMouseDown,.rightMouseDown,.otherMouseDown]){[weak self] event in
            MainActor.assumeIsolated {
                guard let self else{return}
                if let proxy=self.windows.values.first(where:{$0.window === event.window}) {
                    if !proxy.view.bounds.contains(proxy.view.convert(event.locationInWindow,from:nil)){self.dismissRemoteMenus()}
                }else{self.dismissRemoteMenus()}
            }
            return event
        }
    }
    func popupTarget(at screenPoint:CGPoint)->RemoteProxy? {
        for key in surfaceOrder.reversed() {
            if let proxy=windows[key],proxy.isPopup,proxy.remote["role"] as? String != "tooltip",
               proxy.window.isVisible,!proxy.hidden,proxy.window.frame.contains(screenPoint) {return proxy}
        }
        return nil
    }
    func survivingOwner(of proxy:RemoteProxy,keys:Set<String>)->RemoteProxy? {
        var current=proxy,seen=Set<String>()
        while let id=current.remote["parent"] as? [String:Any],seen.insert(Self.key(id)).inserted,let parent=windows[Self.key(id)] {
            if keys.contains(Self.key(id)){return parent};current=parent
        }
        return nil
    }
    func layoutWindowGroups() {
        guard !layingOutGroups else{return};layingOutGroups=true;defer{layingOutGroups=false}
        for key in surfaceOrder {
            guard let proxy=windows[key],let id=proxy.remote["parent"] as? [String:Any],let parent=windows[Self.key(id)],parent !== proxy else{continue}
            if proxy.window.parent !== parent.window {
                proxy.window.parent?.removeChildWindow(proxy.window)
                parent.window.addChildWindow(proxy.window,ordered:.above)
                proxy.attachmentBounds=nil
            }
            let body=parent.window.convertToScreen(parent.view.convert(parent.view.contentRect(),to:nil))
            let source=WindowSurfaces.bounds(proxy.remote),ownerBounds=WindowSurfaces.bounds(parent.remote)
            let rect=WindowSurfaces.attachedRect(child:source,parent:ownerBounds,displayed:body)
            proxy.applyingGeometry=true
            let frame=proxy.window.frameRect(forContentRect:rect)
            // Titled dialogs remain movable locally. AppKit moves child windows
            // with their owner; recompute their frame only when source geometry
            // or the displayed parent scale changes. Menus stay at their anchor.
            let changed=proxy.attachmentBounds != source || proxy.attachmentParentBounds != ownerBounds || proxy.attachmentDisplaySize != body.size
            if (proxy.isPopup || changed) && proxy.window.frame != frame{proxy.window.setFrame(frame,display:proxy.window.isVisible)}
            proxy.attachmentBounds=source;proxy.attachmentParentBounds=ownerBounds;proxy.attachmentDisplaySize=body.size
            proxy.applyingGeometry=false
            let visible = !proxy.hidden && !parent.hidden && parent.window.isVisible && !parent.window.isMiniaturized
            if visible {if !proxy.window.isVisible{proxy.window.orderFront(nil)}}else{proxy.window.orderOut(nil)}
            proxy.visibility()
        }
    }
    func menuWillOpen(_ menu:NSMenu){if menuTracking==0{dismissRemoteMenus()};menuTracking+=1}
    func menuDidClose(_ menu:NSMenu){menuTracking=max(0,menuTracking-1);if menuTracking==0 && needsMenu{DispatchQueue.main.async{[weak self] in self?.installMenu()}}}
    func installMenu(){
        guard menuTracking==0 else{needsMenu=true;return};needsMenu=false
        let bar=NSMenu();bar.autoenablesItems=false
        let local=NSMenu();local.autoenablesItems=false;local.delegate=self
        let appItem=NSMenuItem(title:"Frames",action:nil,keyEquivalent:"");appItem.submenu=local;bar.addItem(appItem)
        func add(_ title:String,_ action:Selector,_ state:Bool?=nil){let item=NSMenuItem(title:title,action:action,keyEquivalent:"");item.target=self;if let state{item.state=state ? .on:.off};local.addItem(item)}
        let qualityMenu=NSMenu();qualityMenu.autoenablesItems=false
        for (preset,title) in [("high","High (sharp text, native resolution)"),("medium","Medium (balanced)"),("low","Low (lower bandwidth)")] {
            let item=NSMenuItem(title:title,action:#selector(selectQuality(_:)),keyEquivalent:"");item.target=self;item.representedObject=preset;item.state=quality==preset ? .on:.off;qualityMenu.addItem(item)
        }
        let qualityItem=NSMenuItem(title:"Image quality",action:nil,keyEquivalent:"");qualityItem.submenu=qualityMenu;local.addItem(qualityItem)
        let windowMenu=NSMenu();windowMenu.autoenablesItems=false
        for proxy in windows.values.sorted(by:{$0.window.title<$1.window.title}) {
            let entry=NSMenuItem(title:proxy.remote["title"] as? String ?? "Remote window",action:#selector(toggleWindow(_:)),keyEquivalent:"");entry.target=self;entry.representedObject=Self.key(proxy.id);entry.state=proxy.hidden ? .off:.on;windowMenu.addItem(entry)
        }
        let windowItem=NSMenuItem(title:"Windows",action:nil,keyEquivalent:"");windowItem.submenu=windowMenu;local.addItem(windowItem)
        add("Show all windows",#selector(showAllWindows));add("Hide all windows",#selector(hideAllWindows))
        if let proxy=activeProxy {
            let hide=NSMenuItem(title:"Ignore / hide this window",action:#selector(RemoteProxy.hideWindow),keyEquivalent:"");hide.target=proxy;local.addItem(hide)
        }
        local.addItem(.separator())
        if sharedSession != nil {
            add("Toggle relative mouse (Control+Option+F11)",#selector(toggleRelative))
            add("Show virtual cursor",#selector(toggleRemoteCursor),showRemoteCursor)
            add("Window compatibility",#selector(showCompatibility));local.addItem(.separator());add("Disconnect",#selector(disconnect));NSApp.mainMenu=bar;return
        }
        add("Per-window view",#selector(windowsView),viewMode=="windows");add("Full desktop view",#selector(desktopView),viewMode=="desktop")
        add("Show remote cursor",#selector(toggleRemoteCursor),showRemoteCursor)
        local.addItem(.separator())
        if let proxy=activeProxy{
            let header=NSMenuItem(title:"Show remote window header",action:#selector(RemoteProxy.toggleHeader),keyEquivalent:"");header.target=proxy;header.state=proxy.showHeader ? .on:.off;header.isEnabled=proxy.remote["role"] as? String != "desktop";local.addItem(header)
            for(title,action)in[("Resize remote app",#selector(RemoteProxy.setResize)),("Stretch locally",#selector(RemoteProxy.setStretch)),("Toggle aspect fit / fill",#selector(RemoteProxy.toggleFill)),("Toggle Control / Command swap",#selector(RemoteProxy.toggleSwap)),("Toggle physical keys (terminal)",#selector(RemoteProxy.togglePhysical)),("Refresh application menus",#selector(RemoteProxy.remoteMenu)),("Fetch remote file...",#selector(RemoteProxy.fetchFile))]{
                let item=NSMenuItem(title:title,action:action,keyEquivalent:"");item.target=proxy;item.isEnabled=action != #selector(RemoteProxy.setResize) || proxy.resizable
                let checked = action == #selector(RemoteProxy.setResize) ? proxy.mode == "resize_remote" && proxy.resizable : action == #selector(RemoteProxy.setStretch) ? proxy.mode == "stretch_local" || !proxy.resizable : action == #selector(RemoteProxy.toggleFill) ? proxy.fill : action == #selector(RemoteProxy.toggleSwap) ? proxy.swap : action == #selector(RemoteProxy.togglePhysical) ? proxy.physical : false
                item.state=checked ? .on:.off;local.addItem(item)
            }
            let remote=proxy.makeMenu(proxy.remoteMenuItems)
            for item in Array(remote.items){remote.removeItem(item);bar.addItem(item)}
        }
        local.addItem(.separator());add("Disconnect",#selector(disconnect))
        let devices=NSMenu();devices.autoenablesItems=false;devices.delegate=self
        appendDeviceMenus(devices)
        let adb=NSMenuItem(title:"Android ADB bridge",action:#selector(toggleAdb),keyEquivalent:"");adb.target=self;adb.state=adbEnabled ? .on:.off;devices.addItem(adb)
        let support=NSMenuItem(title:"Device forwarding support...",action:#selector(deviceSupport),keyEquivalent:"");support.target=self;devices.addItem(support)
        let deviceItem=NSMenuItem(title:"Devices",action:nil,keyEquivalent:"");deviceItem.submenu=devices;bar.addItem(deviceItem)
        NSApp.mainMenu=bar
        Wire.diagnostic("menu_installed", ["top_level_items":bar.items.count,"remote_top_level_items":activeProxy?.remoteMenuItems.count ?? 0,"activation_policy":NSApp.activationPolicy().rawValue])
    }
    @objc func selectQuality(_ item:NSMenuItem){if let preset=item.representedObject as? String{Wire.send(["event":"quality","preset":preset])}}
    @objc func toggleWindow(_ item:NSMenuItem){if let key=item.representedObject as? String,let proxy=windows[key]{proxy.setShown(proxy.hidden)}}
    @objc func showAllWindows(){for proxy in windows.values{proxy.setShown(true)}}
    @objc func hideAllWindows(){for proxy in windows.values{proxy.setShown(false)}}
    func connectionMenu(for proxy:RemoteProxy)->NSMenu {
        activate(proxy);let menu=(NSApp.mainMenu?.items.first?.submenu?.copy() as? NSMenu) ?? NSMenu();menu.delegate=self
        for item in NSApp.mainMenu?.items.dropFirst() ?? [] {menu.addItem(item.copy() as! NSMenuItem)}
        return menu
    }
    @objc func toggleRelative(){activeProxy?.view.toggleRelative()}
    @objc func showCompatibility(){error("Window compatibility",compatibilityText)}
    func configureShared(_ proxy:RemoteProxy){
        guard let sharedSession else{return};proxy.physical=true;proxy.swap=false;proxy.mode="stretch_local";proxy.sharedStatus=sharedSession["detail"] as? String
        if sharedSession["phase"] as? String != "active"{proxy.view.releaseInput()}
        proxy.window.invalidateCursorRects(for:proxy.view);proxy.view.updateVirtualCursor(virtualCursor);proxy.refreshTitle()
    }
    @objc func windowsView(){Wire.send(["event":"release"]);Wire.send(["event":"view_mode","mode":"windows"])}
    @objc func desktopView(){Wire.send(["event":"release"]);Wire.send(["event":"view_mode","mode":"desktop"])}
    @objc func disconnect(){for proxy in windows.values{proxy.view.releaseInput()};Wire.diagnostic("client_disconnect_requested");Wire.send(["event":"release"]);Wire.send(["event":"disconnect"])}
    @objc func toggleAdb(){Wire.send(["event":"adb_bridge","enabled":!adbEnabled])}
    @objc func deviceAction(_ item:NSMenuItem){if let action=item.representedObject as? [String:Any]{Wire.send(["event":"devices","data":action])}}
    func appendDeviceMenus(_ devices:NSMenu){
        func item(_ menu:NSMenu,_ title:String,_ action:[String:Any]?=nil,_ checked:Bool=false){let entry=NSMenuItem(title:title,action:action == nil ? nil:#selector(deviceAction(_:)),keyEquivalent:"");entry.target=self;entry.representedObject=action;entry.isEnabled=action != nil;entry.state=checked ? .on:.off;menu.addItem(entry)}
        let gamepads=NSMenu();gamepads.autoenablesItems=false;gamepads.delegate=self
        let pads=deviceData["gamepads"] as? [[String:Any]] ?? []
        if pads.isEmpty{item(gamepads,"No mapped gamepads found")}
        for pad in pads.prefix(32){guard let id=pad["id"] as? String else{continue};let active=pad["forwarded"] as? Bool ?? false;let suffix=(pad["status"] as? String).map{" — \($0)"} ?? "";item(gamepads,(pad["name"] as? String ?? "Gamepad")+suffix,["action":"gamepad","id":id,"enabled":!active],active)}
        if let caps=deviceData["capabilities"] as? [String:Any],let error=caps["gamepad_error"] as? String{item(gamepads,error)}
        item(gamepads,"Set up gamepad driver on Windows host...",["action":"setup","target":"gamepad"])
        let padItem=NSMenuItem(title:"Gamepads",action:nil,keyEquivalent:"");padItem.submenu=gamepads;devices.addItem(padItem)
        let usb=NSMenu();usb.autoenablesItems=false;item(usb,"USB passthrough is Windows → Windows in this release")
        let usbItem=NSMenuItem(title:"USB devices",action:nil,keyEquivalent:"");usbItem.submenu=usb;devices.addItem(usbItem)
        item(devices,"Refresh devices",["action":"refresh"])
    }
    @objc func toggleRemoteCursor(){Wire.send(["event":"remote_cursor","visible":!showRemoteCursor])}
    @objc func deviceSupport(){let alert=NSAlert();alert.messageText="Device forwarding";alert.informativeText="Gamepads: pair a controller locally over USB or Bluetooth, then choose it under Devices → Gamepads. Receiving requires a Windows host with ViGEmBus. Standard controls and rumble are supported. Generic USB passthrough is available between Windows computers. The Android ADB bridge remains available on both platforms.";runFramesAlert(alert)}
    func error(_ title:String,_ message:String){let alert=NSAlert();alert.messageText=title;alert.informativeText=message;runFramesAlert(alert)}
    func update(_ message: [String: Any]) {
        switch message["type"] as? String {
        case "shared_session":sharedSession=message["data"] as? [String:Any];compatibilityText=message["compatibility_text"] as? String ?? "Untested";for proxy in windows.values{configureShared(proxy)};installMenu()
        case "shared_cursor":
            var cursor=message["data"] as? [String:Any] ?? [:]
            if !(cursor["png_base64"] is String){cursor["png_base64"]=virtualCursor["png_base64"]}
            virtualCursor=cursor;for proxy in windows.values{proxy.view.updateVirtualCursor(cursor)}
        case "devices":deviceData=message["data"] as? [String:Any] ?? [:];if let error=deviceData["error"] as? String{self.error("Device forwarding",error)};installMenu()
        case "quality":quality=message["preset"] as? String ?? "high";installMenu()
        case "remote_cursor":showRemoteCursor=message["visible"] as? Bool ?? true;installMenu()
        case "view_mode":viewMode=message["mode"] as? String ?? "windows";if let error=message["error"] as? String{self.error("Frames view mode",error)}else{focusAfterSwitch=true};installMenu()
        case "adb_bridge":adbEnabled=message["enabled"] as? Bool ?? false;if let error=message["error"] as? String{self.error("Android ADB bridge",error)};installMenu()
        case "peer": host = "\(message["platform"] ?? "remote"):\(message["device_id"] ?? "")"
        case "received_file":if let path=message["path"] as? String{if fileShelf==nil{fileShelf=FileShelf()};fileShelf?.add(URL(fileURLWithPath:path))}
        case "transfer_progress":if fileShelf==nil{fileShelf=FileShelf()};fileShelf?.progress(name:message["name"] as? String ?? "File",done:(message["done"] as? NSNumber)?.doubleValue ?? 0,total:(message["total"] as? NSNumber)?.doubleValue ?? 0)
        case "menu": if let id=message["window"] as? [String:Any],let proxy=windows[Self.key(id)],let items=message["items"] as? [[String:Any]]{proxy.remoteMenuItems=items;if activeProxy === proxy{installMenu()}}
        case "windows":
            guard let list = message["windows"] as? [[String: Any]] else { return }
            sequence += 1
            let graph: [String: Any] = ["type":"snapshot","data":["sequence":sequence,"windows":list]]
            if let bytes = try? JSONSerialization.data(withJSONObject: graph), let string = String(data: bytes, encoding: .utf8) {
                let result = string.withCString { frames_session_apply(session, $0) }; if let result { frames_string_free(result) }
            }
            let keys = Set(list.compactMap { ($0["key"] as? [String: Any]).map(Self.key) })
            Wire.diagnostic("window_snapshot", ["count":keys.count,"view_mode":viewMode])
            var restoreFocus:RemoteProxy?
            if let closing=windows.values.first(where:{$0.window.isKeyWindow && !keys.contains(Self.key($0.id))}) {
                restoreFocus=survivingOwner(of:closing,keys:keys)
            }
            for key in Array(windows.keys) where !keys.contains(key) {
                if let proxy=windows.removeValue(forKey:key) {
                    proxy.destroy()
                }
            }
            let sorted = WindowSurfaces.ordered(list)
            let menuApps=Set(list.filter{$0["role"] as? String == "menu"}.compactMap{$0["app_id"] as? String})
            dismissingMenuApps.formIntersection(menuApps)
            surfaceOrder=sorted.compactMap{($0["key"] as? [String:Any]).map(Self.key)}
            installOutsideClickMonitor()
            for remote in sorted {
                guard let id = remote["key"] as? [String: Any] else { continue }; let key = Self.key(id)
                if let proxy = windows[key] {
                    if remote["modal"] as? Bool == true && proxy.remote["modal"] as? Bool != true {nextSurfaceSerial+=1;proxy.surfaceSerial=nextSurfaceSerial}
                    proxy.remote = remote;proxy.applySurfaceStyle();proxy.refreshTitle()
                    if !(remote["parent"] is [String:Any]) {proxy.window.parent?.removeChildWindow(proxy.window)}
                    if activeProxy === proxy{installMenu()};continue
                }
                let proxy = RemoteProxy(remote: remote, host: host);proxy.client=self;configureShared(proxy); windows[key] = proxy
                nextSurfaceSerial+=1;proxy.surfaceSerial=nextSurfaceSerial
                proxy.window.orderFront(nil); proxy.visibility()
                let appID = remote["app_id"] as? String ?? "unknown"
                if !proxy.isPopup && docks[appID] == nil { startDock(appID, remote) }
            }
            layoutWindowGroups()
            if NSApp.isActive,let focused=windows.values.first(where:{$0.window.isKeyWindow}),let blocker=modalBlocker(for:focused) {
                focused.view.releaseInput();restoreFocus=blocker
            }
            if let proxy=restoreFocus,let blocker=modalBlocker(for:proxy){restoreFocus=blocker}
            if let proxy=restoreFocus,windows[Self.key(proxy.id)] === proxy,NSApp.isActive,!proxy.hidden{proxy.window.makeKeyAndOrderFront(nil)}
            let alive = Set(list.compactMap { $0["app_id"] as? String })
            for id in Array(docks.keys) where !alive.contains(id) { docks.removeValue(forKey: id)?.terminate() }
            presentVideo()
            if focusAfterSwitch,let proxy=windows.values.first(where:{!$0.hidden}){focusAfterSwitch=false;NSApp.activate(ignoringOtherApps:true);proxy.window.makeKeyAndOrderFront(nil)}
        case "status": if let id = message["window"] as? [String: Any], let proxy = windows[Self.key(id)] { if proxy.shared{proxy.sharedNotice=message["status"] as? String}else{proxy.status=message["status"] as? String ?? ""}; proxy.refreshTitle() }
        case "ack": if let id = message["window"] as? [String: Any], let proxy = windows[Self.key(id)] {
            if let actual = message["actual"] as? [String: Any] { proxy.remote = actual }
            proxy.commandError = message["error"] as? String
            proxy.refreshTitle()
            if activeProxy === proxy{installMenu()}
        }
        default: break
        }
    }
    func startDock(_ id: String, _ remote: [String: Any]) {
        let process = Process(); process.executableURL = URL(fileURLWithPath: CommandLine.arguments[0])
        process.arguments = ["--dock",remote["app_name"] as? String ?? "Remote app",remote["icon_png"] as? String ?? ""]
        let pipe = Pipe(); process.standardOutput = pipe
        let reader = DockEventReader()
        pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData; if data.isEmpty { handle.readabilityHandler = nil; return }
            let events = reader.append(data)
            DispatchQueue.main.async {
                guard let self else { return }
                for event in events {
                    let proxies = self.windows.values.filter { ($0.remote["app_id"] as? String) == id }
                    if event == "quit" { for proxy in proxies { if proxy.remote["role"] as? String == "desktop"{self.disconnect()}else{proxy.send("action",["action":["kind":"close"]])} } }
                    else if event == "activate", let proxy = proxies.first { NSApp.activate(ignoringOtherApps: true); proxy.setShown(true) }
                }
            }
        }
        do { try process.run(); docks[id] = process } catch { fputs("Dock proxy: \(error)\n", stderr) }
    }
    func applicationWillTerminate(_ notification: Notification) {if let monitor=outsideClickMonitor{NSEvent.removeMonitor(monitor);outsideClickMonitor=nil};for proxy in windows.values{proxy.view.releaseInput()}; Wire.diagnostic("client_terminated",["window_count":windows.count]);for process in docks.values { process.terminate() }; Wire.send(["event":"release"]) }
}

final class DockEventReader: @unchecked Sendable {
    private let lock=NSLock()
    private var pending=Data()
    func append(_ data:Data)->[String]{
        lock.lock();defer{lock.unlock()};pending.append(data)
        var result:[String]=[]
        while let end=pending.firstIndex(of:10){
            let line=Data(pending[..<end]);pending.removeSubrange(...end)
            if let object=try? JSONSerialization.jsonObject(with:line) as? [String:Any],let event=object["event"] as? String{result.append(event)}
        }
        if pending.count>4096{pending.removeAll()}
        return result
    }
}

@MainActor final class ProxyWindow: NSWindow {
    var acceptsFocus=true
    override var canBecomeKey:Bool{acceptsFocus}
    override var canBecomeMain:Bool{acceptsFocus}
}

@MainActor final class RemoteProxy: NSObject, NSWindowDelegate {
    weak var client:NativeClient?
    var remoteMenuItems:[[String:Any]]=[]
    var remote: [String: Any]
    let window: ProxyWindow
    let view: RemoteView
    let preference: String
    var mode: String
    var fill: Bool
    var swap: Bool
    var physical: Bool
    var sharedStatus:String?
    var sharedNotice:String?
    var shared:Bool{client?.sharedSession != nil}
    var status = "Connecting stream…"
    var commandError:String?
    var resizeWork: DispatchWorkItem?
    var lastSize = CGSize.zero
    var destroying = false
    var hidden = false
    var applyingGeometry=false
    var surfaceSerial=0
    var attachmentBounds:CGRect?
    var attachmentParentBounds:CGRect?
    var attachmentDisplaySize:CGSize?
    var menuButton:NSButton?
    var showHeader:Bool{remote["show_header"] as? Bool ?? false}
    var id: [String: Any] { remote["key"] as? [String: Any] ?? [:] }
    var resizable: Bool { !shared && remote["resize"] as? String == "supported" }
    var isPopup:Bool {WindowSurfaces.isPopup(remote["role"] as? String ?? "") && remote["parent"] is [String:Any]}
    init(remote: [String: Any], host: String) {
        self.remote = remote
        preference = "\(host):\(remote["app_id"] ?? "")"
        mode = UserDefaults.standard.string(forKey: preference+":mode") ?? "resize_remote"
        fill = UserDefaults.standard.bool(forKey: preference+":fill")
        swap = UserDefaults.standard.object(forKey: preference+":swap") as? Bool ?? host.hasPrefix("windows:")
        physical = UserDefaults.standard.bool(forKey: preference+":physical")
        let bounds = remote["bounds"] as? [String: Double] ?? [:]
        let sourceWidth=bounds["width"] ?? 800,sourceHeight=bounds["height"] ?? 600
        let area=NSScreen.main?.visibleFrame ?? NSRect(x:0,y:0,width:1440,height:1000)
        var width=min(1400,sourceWidth,max(320,Double(area.width)-60)),height=min(900,sourceHeight,max(240,Double(area.height)-80))
        if remote["role"] as? String == "desktop"{let fit=min(width/sourceWidth,height/sourceHeight);width=sourceWidth*fit;height=sourceHeight*fit}
        let rect = NSRect(x: 150, y: 150, width:CGFloat(width),height:CGFloat(height))
        let transient = WindowSurfaces.isPopup(remote["role"] as? String ?? "") && remote["parent"] is [String:Any]
        window = ProxyWindow(contentRect:rect,styleMask: transient ? [.borderless] : [.titled,.closable,.miniaturizable,.resizable],backing:.buffered,defer:false)
        view = RemoteView(frame:NSRect(origin:.zero,size:rect.size))
        super.init(); window.acceptsFocus = !transient;view.proxy = self; window.contentView = view; window.delegate = self; window.isReleasedWhenClosed = false; window.acceptsMouseMovedEvents = true; window.title = remote["title"] as? String ?? "Remote window"
        window.collectionBehavior = [.managed, .fullScreenPrimary]
        if remote["role"] as? String == "desktop"{window.center()}
        if !transient {
            let accessory=NSTitlebarAccessoryViewController();accessory.layoutAttribute = .right
            let button=NSButton(image:NSImage(systemSymbolName:"chevron.down",accessibilityDescription:"Connection menu")!,target:self,action:#selector(openConnectionMenu(_:)));button.bezelStyle = .inline;button.isBordered=false;button.toolTip="Connection menu";button.setAccessibilityLabel("Connection menu");button.frame=NSRect(x:0,y:0,width:34,height:24)
            accessory.view=button;window.addTitlebarAccessoryViewController(accessory);menuButton=button
        }
        refreshTitle()
        applySurfaceStyle()
    }
    func applySurfaceStyle() {
        window.acceptsFocus = !isPopup
        window.ignoresMouseEvents = remote["role"] as? String == "tooltip"
        window.hidesOnDeactivate=isPopup
        if isPopup {if window.styleMask != [.borderless]{window.styleMask = [.borderless]};window.collectionBehavior = [.fullScreenAuxiliary];mode="stretch_local";fill=true}
        else if window.styleMask == [.borderless] {window.styleMask = [.titled,.closable,.miniaturizable,.resizable];window.collectionBehavior = [.managed,.fullScreenPrimary]}
    }
    @objc func openConnectionMenu(_ button:NSButton){client?.dismissRemoteMenus();view.releaseInput();client?.connectionMenu(for:self).popUp(positioning:nil,at:NSPoint(x:0,y:button.bounds.minY),in:button)}
    @objc func hideWindow(){setShown(false)}
    func setShown(_ shown:Bool) {
        hidden = !shown
        if shown {NSApp.activate(ignoringOtherApps:true);window.makeKeyAndOrderFront(nil)}
        else {view.releaseInput();resizeWork?.cancel();window.orderOut(nil);view.redraw()}
        client?.layoutWindowGroups();visibility();client?.installMenu()
    }
    func send(_ event: String, _ fields: [String: Any] = [:]) {
        if (event=="input" || event=="action"),let blocker=client?.modalBlocker(for:self) {
            if event=="action",(fields["action"] as? [String:Any])?["kind"] as? String == "focus" {blocker.send(event,fields)}
            if NSApp.isActive{blocker.hidden=false;client?.layoutWindowGroups();blocker.window.makeKeyAndOrderFront(nil)}
            return
        }
        var value = fields;value["event"] = event;value["window"] = id;Wire.send(value)
    }
    func visibility() { send("visibility",["visible":!hidden && window.isVisible && !window.isMiniaturized && window.occlusionState.contains(.visible),"focused":window.isKeyWindow]) }
    func refreshTitle() { let detail=commandError ?? (shared ? [sharedStatus,sharedNotice].compactMap{$0}.joined(separator:" / ") : status);let title = "\(remote["title"] ?? "Remote window") — \(mode == "resize_remote" && resizable ? "Resize remote" : "Stretch locally")\(physical ? " / Physical keys":"")\(detail.isEmpty ? "" : " — "+detail)";if window.title != title { window.title=title } }
    func save() { if shared{refreshTitle();view.redraw();return};UserDefaults.standard.set(mode,forKey:preference+":mode");UserDefaults.standard.set(fill,forKey:preference+":fill");UserDefaults.standard.set(swap,forKey:preference+":swap");UserDefaults.standard.set(physical,forKey:preference+":physical");refreshTitle();view.redraw() }
    @objc func setResize() { if resizable { mode="resize_remote";save() } }
    @objc func setStretch() { mode="stretch_local";save() }
    @objc func toggleFill() { fill.toggle();save() }
    @objc func toggleSwap() { guard !shared else{return};view.releaseInput();swap.toggle();save() }
    @objc func togglePhysical() { guard !shared else{return};view.releaseInput();physical.toggle();save() }
    @objc func toggleHeader() {
        guard !shared,remote["role"] as? String != "desktop" else{return}
        view.releaseInput()
        resizeWork?.cancel()
        send("action",["action":["kind":"set_header","visible":!showHeader]])
    }
    @objc func remoteMenu(){guard !shared else{return};send("action",["action":["kind":"menu"]])}
    @objc func fetchFile(){guard !shared else{return};let alert=NSAlert();alert.messageText="Fetch remote file";alert.informativeText="Enter a path relative to the remote user's home folder.";let input=NSTextField(frame:NSRect(x:0,y:0,width:400,height:24));input.stringValue="Downloads/";alert.accessoryView=input;alert.addButton(withTitle:"Download");alert.addButton(withTitle:"Cancel");if runFramesAlert(alert) == .alertFirstButtonReturn{send("download_file",["path":input.stringValue])}}
    @objc func pressMenu(_ item:NSMenuItem){guard !shared else{return};if let id=item.representedObject as? String{send("action",["action":["kind":"menu_item","id":id]])}}
    func makeMenu(_ items:[[String:Any]])->NSMenu{
        func build(_ items:[[String:Any]],_ depth:Int)->NSMenu{let menu=NSMenu();menu.autoenablesItems=false;menu.delegate=client;guard depth<8 else{return menu};for value in items.prefix(200){let title=(value["title"] as? String ?? "").components(separatedBy:"\t")[0].replacingOccurrences(of:"&",with:"");if title.isEmpty{menu.addItem(.separator());continue};let item=NSMenuItem(title:title,action:#selector(pressMenu(_:)),keyEquivalent:"");item.target=self;item.representedObject=value["id"];item.isEnabled=value["enabled"] as? Bool ?? true;item.state=(value["checked"] as? Bool ?? false) ? .on:.off;if let children=value["children"] as? [[String:Any]],!children.isEmpty{item.submenu=build(children,depth+1)};menu.addItem(item)};return menu}
        return build(items,0)
    }
    func windowShouldClose(_ sender: NSWindow) -> Bool { if destroying { return true };if shared || remote["role"] as? String == "desktop"{view.releaseInput();client?.disconnect()}else{send("action",["action":["kind":"close"]])};return false }
    func destroy() {if shared || window.isKeyWindow{view.releaseInput()};destroying=true;resizeWork?.cancel();window.parent?.removeChildWindow(window);window.close() }
    func windowDidBecomeKey(_ notification: Notification) { client?.activate(self);if shared{send("resume")}else{send("action",["action":["kind":"focus"]]);if remote["role"] as? String != "desktop" && !isPopup{remoteMenu()}};visibility() }
    func windowDidResignKey(_ notification: Notification) { view.releaseInput();visibility() }
    func windowDidMiniaturize(_ notification: Notification) {client?.layoutWindowGroups();visibility() }
    func windowDidDeminiaturize(_ notification: Notification) {client?.layoutWindowGroups();visibility() }
    func windowDidMove(_ notification:Notification) {if !applyingGeometry{client?.layoutWindowGroups()}}
    func windowDidChangeOcclusionState(_ notification: Notification) { visibility() }
    func windowDidResize(_ notification: Notification) {
        view.redraw();if !applyingGeometry{client?.layoutWindowGroups()};guard !applyingGeometry,resizable,mode=="resize_remote",let size=window.contentView?.bounds.size,size != lastSize else{return};lastSize=size;resizeWork?.cancel()
        let work=DispatchWorkItem{[weak self] in self?.send("action",["action":["kind":"resize","width":size.width,"height":size.height]])};resizeWork=work;DispatchQueue.main.asyncAfter(deadline:.now()+0.05,execute:work)
    }
}

@MainActor final class VirtualCursorView:NSImageView {override func hitTest(_ point:NSPoint)->NSView?{nil}}
@MainActor final class RemoteView: NSView {
    weak var proxy: RemoteProxy?
    var renderer: MetalPicture?
    var picture = CGSize(width:1,height:1)
    var marked = NSAttributedString(string:"")
    var forwarded: Set<UInt16> = []
    var modifiers = KeyboardModifiers()
    var relative=false
    let cursorOverlay=VirtualCursorView(frame:.zero)
    var cursorState:[String:Any]=[:]
    static let emptyCursor=NSCursor(image:NSImage(size:NSSize(width:1,height:1)),hotSpot:.zero)
    var optionPointer = false
    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event:NSEvent?)->Bool{proxy?.isPopup == true}
    override init(frame:NSRect) { super.init(frame:frame);wantsLayer=true;cursorOverlay.imageScaling = .scaleAxesIndependently;addSubview(cursorOverlay);registerForDraggedTypes([.fileURL]);addTrackingArea(NSTrackingArea(rect:.zero,options:[.activeAlways,.inVisibleRect,.mouseMoved],owner:self,userInfo:nil)) }
    required init?(coder:NSCoder) { fatalError("not used") }
    func display(_ frame:ClientVideoFrame) { if renderer==nil{renderer=try? MetalPicture(view:self)};let changed=picture != CGSize(width:frame.width,height:frame.height);picture=CGSize(width:frame.width,height:frame.height);renderer?.upload(frame);redraw();if changed{proxy?.client?.layoutWindowGroups()} }
    func display(_ bytes:Data,width:Int,height:Int) { if renderer==nil{renderer=try? MetalPicture(view:self)};picture=CGSize(width:width,height:height);renderer?.upload(bytes,width:width,height:height);redraw() }
    func redraw() { renderer?.draw(size:convertToBacking(bounds).size,fill:proxy?.fill ?? false,visible:window?.isVisible == true && window?.isMiniaturized != true) }
    override func layout() { super.layout();redraw();positionVirtualCursor() }
    override func resetCursorRects(){if proxy?.shared == true{addCursorRect(bounds,cursor:Self.emptyCursor)}else{super.resetCursorRects()}}
    func contentRect()->NSRect{
        var rect=bounds
        if !(proxy?.fill ?? false){let aspect=picture.width/max(1,picture.height);if rect.width/max(1,rect.height)>aspect{let width=rect.height*aspect;rect.origin.x=(rect.width-width)/2;rect.size.width=width}else{let height=rect.width/aspect;rect.origin.y=(rect.height-height)/2;rect.size.height=height}}
        return rect
    }
    func updateVirtualCursor(_ state:[String:Any]){
        cursorState=state
        if let encoded=state["png_base64"] as? String,encoded.utf8.count<=131072,let data=Data(base64Encoded:encoded),let source=CGImageSourceCreateWithData(data as CFData,nil),let properties=CGImageSourceCopyPropertiesAtIndex(source,0,nil) as? [CFString:Any],let width=properties[kCGImagePropertyPixelWidth] as? NSNumber,let height=properties[kCGImagePropertyPixelHeight] as? NSNumber,width.intValue<=128,height.intValue<=128{cursorOverlay.image=NSImage(data:data)}
        positionVirtualCursor()
    }
    func positionVirtualCursor(){
        guard cursorState["visible"] as? Bool == true,let image=cursorOverlay.image,let proxy else{cursorOverlay.isHidden=true;return}
        let rect=contentRect(),remoteBounds=proxy.remote["bounds"] as? [String:Double] ?? [:],scale=(proxy.remote["scale"] as? NSNumber)?.doubleValue ?? 1
        let sx=rect.width/CGFloat(max(1,(remoteBounds["width"] ?? 1)*scale)),sy=rect.height/CGFloat(max(1,(remoteBounds["height"] ?? 1)*scale))
        let width=image.size.width*sx,height=image.size.height*sy,x=(cursorState["x"] as? NSNumber)?.doubleValue ?? 0,y=(cursorState["y"] as? NSNumber)?.doubleValue ?? 0,hx=(cursorState["hotspot_x"] as? NSNumber)?.doubleValue ?? 0,hy=(cursorState["hotspot_y"] as? NSNumber)?.doubleValue ?? 0
        cursorOverlay.frame=NSRect(x:rect.minX+rect.width*CGFloat(x)-CGFloat(hx)*sx,y:rect.maxY-rect.height*CGFloat(y)+CGFloat(hy)*sy-height,width:width,height:height);cursorOverlay.isHidden=false
    }
    func toggleRelative(){
        guard proxy?.shared == true else{return}
        if relative{releaseInput();return}
        releaseInput()
        guard let window else{return};window.makeFirstResponder(self)
        let center=window.convertPoint(toScreen:convert(NSPoint(x:bounds.midX,y:bounds.midY),to:nil)),height=NSScreen.screens.first?.frame.height ?? 0
        CGWarpMouseCursorPosition(CGPoint(x:center.x,y:height-center.y))
        if CGAssociateMouseAndMouseCursorPosition(0) == .success{relative=true;NSCursor.hide();proxy?.send("resume")}
    }
    func releaseRelative(){if relative{relative=false;CGAssociateMouseAndMouseCursorPosition(1);NSCursor.unhide()}}
    func relativeMotion(_ event:NSEvent){
        let dx=event.cgEvent?.getIntegerValueField(.mouseEventDeltaX) ?? Int64(event.deltaX.rounded()),dy=event.cgEvent?.getIntegerValueField(.mouseEventDeltaY) ?? Int64(event.deltaY.rounded())
        proxy?.send("input",["input":["kind":"relative_motion","dx":max(-1000000,min(1000000,dx)),"dy":max(-1000000,min(1000000,dy))]])
    }
    func position(_ event:NSEvent)->[String:Any] {
        position(point:convert(event.locationInWindow,from:nil))
    }
    func position(point p:CGPoint)->[String:Any] {
        let rect=contentRect()
        return ["x":min(1,max(0,(p.x-rect.minX)/max(1,rect.width))),"y":min(1,max(0,1-(p.y-rect.minY)/max(1,rect.height)))]
    }
    func pointerDestination(_ event:NSEvent)->(RemoteProxy?,[String:Any]) {
        // AppKit keeps delivering a drag to the original mouse-down window,
        // even after a native remote submenu opens beneath the pointer.
        if proxy?.shared != true,let window {
            let screen=window.convertPoint(toScreen:event.locationInWindow)
            if let target=proxy?.client?.popupTarget(at:screen) {
                let local=target.view.convert(target.window.convertPoint(fromScreen:screen),from:nil)
                return (target,target.view.position(point:local))
            }
        }
        return (proxy,position(event))
    }
    func pointer(_ event:NSEvent,kind:String,button:Int?=nil,down:Bool?=nil) {
        if relative && kind=="pointer"{relativeMotion(event);return}
        if down == true{if let app=proxy?.remote["app_id"] as? String{proxy?.client?.dismissingMenuApps.remove(app)};if proxy?.shared == true{proxy?.send("resume")};optionPointer=true;synchronizeModifiers()}
        let (target,position)=pointerDestination(event)
        var value=position;value["kind"]=relative && kind=="button" ? "relative_button":kind
        if relative && kind=="button"{value.removeValue(forKey:"x");value.removeValue(forKey:"y")}
        if let button{value["button"]=button};if let down{value["down"]=down}
        if kind=="button"{Wire.diagnostic("pointer_button",["window":target?.id ?? [:],"x":value["x"] ?? 0,"y":value["y"] ?? 0,"view_width":bounds.width,"view_height":bounds.height,"picture_width":picture.width,"picture_height":picture.height,"down":down ?? false])}
        target?.send("input",["input":value])
    }
    override func mouseMoved(with event:NSEvent){pointer(event,kind:"pointer")}
    override func mouseDragged(with event:NSEvent){pointer(event,kind:"pointer")}
    override func rightMouseDragged(with event:NSEvent){pointer(event,kind:"pointer")}
    override func otherMouseDragged(with event:NSEvent){pointer(event,kind:"pointer")}
    override func mouseDown(with event:NSEvent){window?.makeFirstResponder(self);pointer(event,kind:"button",button:0,down:true)}
    override func mouseUp(with event:NSEvent){pointer(event,kind:"button",button:0,down:false)}
    override func rightMouseDown(with event:NSEvent){pointer(event,kind:"button",button:1,down:true)}
    override func rightMouseUp(with event:NSEvent){pointer(event,kind:"button",button:1,down:false)}
    override func otherMouseDown(with event:NSEvent){if (2...4).contains(event.buttonNumber){pointer(event,kind:"button",button:event.buttonNumber,down:true)}}
    override func otherMouseUp(with event:NSEvent){if (2...4).contains(event.buttonNumber){pointer(event,kind:"button",button:event.buttonNumber,down:false)}}
    override func scrollWheel(with event:NSEvent){optionPointer=true;synchronizeModifiers();let(target,position)=pointerDestination(event);var value=position;value["kind"]="scroll";value["dx"]=event.scrollingDeltaX/20;value["dy"] = -event.scrollingDeltaY/20;target?.send("input",["input":value])}
    func key(_ usage:UInt16,down:Bool){var mapped=usage;if proxy?.swap == true && proxy?.physical != true{mapped=[UInt16(0xe0):0xe3,0xe3:0xe0,0xe4:0xe7,0xe7:0xe4][usage] ?? usage};proxy?.send("input",["input":["kind":"key","usage":mapped,"down":down]])}
    func synchronizeModifiers(suspend:Bool=false){for(usage,down)in modifiers.synchronize(physical:proxy?.physical == true,optionShortcut:!forwarded.isEmpty || optionPointer,suspend:suspend){key(usage,down:down)}}
    func releaseInput(){releaseRelative();proxy?.send("release");forwarded.removeAll();modifiers.reset();optionPointer=false;marked=NSAttributedString(string:"");inputContext?.discardMarkedText()}
    override func keyDown(with event:NSEvent){
        optionPointer=false
        if event.modifierFlags.contains(.command),[48,49].contains(event.keyCode){releaseInput();return} // Local app switching / Spotlight.
        guard let usage=usageToMac.first(where:{$0.value==event.keyCode})?.key else{return}
        if proxy?.shared == true,event.keyCode==103,event.modifierFlags.contains([.control,.option]){toggleRelative();return}
        if event.keyCode==111,event.modifierFlags.contains([.control,.option]){releaseInput();if proxy?.shared != true{window?.miniaturize(nil)};return}
        let printable=(4...39).contains(usage)||(44...56).contains(usage)
        if proxy?.physical == true || event.modifierFlags.contains(.command) || event.modifierFlags.contains(.control) || !printable {
            forwarded.insert(usage)
            synchronizeModifiers()
            key(usage,down:true)
        } else {
            synchronizeModifiers(suspend:true)
            interpretKeyEvents([event])
        }
    }
    override func keyUp(with event:NSEvent){if let usage=usageToMac.first(where:{$0.value==event.keyCode})?.key,forwarded.remove(usage) != nil{key(usage,down:false)};synchronizeModifiers()}
    override func flagsChanged(with event:NSEvent){
        guard let usage=usageToMac.first(where:{$0.value==event.keyCode})?.key else{return}
        if usage==57{if proxy?.physical == true{key(usage,down:true);key(usage,down:false)};return}
        let hadCommand = !modifiers.held.isDisjoint(with:[0xe3,0xe7])
        modifiers.changed(usage,flags:event.modifierFlags.rawValue)
        if modifiers.held.isDisjoint(with:[0xe2,0xe6]){optionPointer=false}
        // AppKit can consume keyUp for Command shortcuts. Release their keys
        // when the final Command key goes up, before remapping modifiers.
        if hadCommand && modifiers.held.isDisjoint(with:[0xe3,0xe7]){for keyUsage in forwarded.sorted(){key(keyUsage,down:false)};forwarded.removeAll()}
        synchronizeModifiers()
    }
    func insertText(_ string:Any,replacementRange:NSRange){let text=(string as? NSAttributedString)?.string ?? string as? String ?? "";synchronizeModifiers(suspend:true);proxy?.send("input",["input":["kind":"text","text":text]]);synchronizeModifiers();marked=NSAttributedString(string:"")}
    func setMarkedText(_ string:Any,selectedRange:NSRange,replacementRange:NSRange){marked=(string as? NSAttributedString) ?? NSAttributedString(string:string as? String ?? "")}
    func unmarkText(){marked=NSAttributedString(string:"")}
    func selectedRange()->NSRange{NSRange(location:0,length:0)}
    func markedRange()->NSRange{marked.length>0 ? NSRange(location:0,length:marked.length) : NSRange(location:NSNotFound,length:0)}
    func hasMarkedText()->Bool{marked.length>0}
    func attributedSubstring(forProposedRange range:NSRange,actualRange:NSRangePointer?)->NSAttributedString?{nil}
    func validAttributesForMarkedText()->[NSAttributedString.Key]{[]}
    func firstRect(forCharacterRange range:NSRange,actualRange:NSRangePointer?)->NSRect{window?.convertToScreen(convert(bounds,to:nil)) ?? .zero}
    func characterIndex(for point:NSPoint)->Int{0}
    override func doCommand(by selector:Selector){}
    override func draggingEntered(_ sender:NSDraggingInfo)->NSDragOperation{proxy?.shared == true ? []:.copy}
    override func performDragOperation(_ sender:NSDraggingInfo)->Bool{guard proxy?.shared != true,let urls=sender.draggingPasteboard.readObjects(forClasses:[NSURL.self],options:[.urlReadingFileURLsOnly:true]) as? [URL],!urls.isEmpty else{return false};let point=convert(sender.draggingLocation,from:nil);proxy?.send("send_files",["paths":urls.map(\.path),"x":min(1,max(0,point.x/max(1,bounds.width))),"y":min(1,max(0,1-point.y/max(1,bounds.height)))]);return true}
}

#if compiler(>=6.0)
extension RemoteView: @preconcurrency NSTextInputClient {}
#else
extension RemoteView: NSTextInputClient {}
#endif

@MainActor final class MetalPicture {
    private let presenter:MetalPresenter
    init(view:NSView)throws { presenter=try MetalPresenter(hardware:true);view.layer=presenter.layer;presenter.start() }
    deinit { presenter.stop() }
    func upload(_ data:Data,width:Int,height:Int) { upload(ClientVideoFrame(id:0,generation:0,width:width,height:height,bytes:data)) }
    func upload(_ frame:ClientVideoFrame) { presenter.upload(frame) }
    func draw(size:CGSize,fill:Bool,visible:Bool=true) { presenter.layout(size:size,fill:fill,visible:visible) }
}

private final class MetalPresenter:NSObject,CAMetalDisplayLinkDelegate,@unchecked Sendable {
    let layer:CAMetalLayer
    private let device:MTLDevice,queue:MTLCommandQueue
    private var pipeline:MTLRenderPipelineState?
    private var initializationError:String?,initializationReported=false
    private let lock=NSLock(),inFlight=DispatchSemaphore(value:1)
    private var pending:ClientVideoFrame?,current:ClientVideoFrame?
    private var size=CGSize.zero,fill=false,visible=true,dirty=false,stopped=false
    private var loop:CFRunLoop?,link:CAMetalDisplayLink?
    private var texture:MTLTexture?,mapped:CVMetalTexture?,cache:CVMetalTextureCache?
    private var diagnosticPending=true
    private var presentedCount=0
    private var lastPresentedNs:UInt64=0
    private let diagnosticID=UUID().uuidString
    init(hardware:Bool)throws {
        guard let device=MTLCreateSystemDefaultDevice(),let queue=device.makeCommandQueue() else{throw Wire.failure("Metal unavailable")}
        self.device=device;self.queue=queue
        layer=CAMetalLayer();layer.device=device;layer.pixelFormat = .bgra8Unorm;layer.framebufferOnly=true;layer.maximumDrawableCount=2;layer.allowsNextDrawableTimeout=true
        super.init()
        guard CVMetalTextureCacheCreate(nil,nil,device,nil,&cache) == kCVReturnSuccess else{throw Wire.failure("Cannot create Metal surface cache")}
    }
    private func preparePipeline() throws {
        let source="""
        #include <metal_stdlib>
        using namespace metal;
        struct V { float4 p [[position]];float2 uv; };
        vertex V vertexMain(uint id [[vertex_id]],constant float4 &s [[buffer(0)]]) {
          float2 p[4]={float2(-1,-1),float2(1,-1),float2(-1,1),float2(1,1)};
          V v;v.p=float4(p[id]*s.xy,0,1);v.uv=float2((p[id].x+1)*0.5,(1-p[id].y)*0.5)*s.zw;return v;
        }
        fragment float4 fragmentMain(V v [[stage_in]],texture2d<float> picture [[texture(0)]]) {
          constexpr sampler s(filter::linear);return picture.sample(s,v.uv);
        }
        """
        let library=try device.makeLibrary(source:source,options:nil),descriptor=MTLRenderPipelineDescriptor()
        descriptor.vertexFunction=library.makeFunction(name:"vertexMain");descriptor.fragmentFunction=library.makeFunction(name:"fragmentMain");descriptor.colorAttachments[0].pixelFormat = .bgra8Unorm
        pipeline=try device.makeRenderPipelineState(descriptor:descriptor)
    }
    func start() {
        Thread.detachNewThread { [self] in
            do {try preparePipeline()}catch{lock.lock();initializationError=error.localizedDescription;let frame=pending;lock.unlock();if let frame{reportInitializationFailure(frame)};return}
            lock.lock();loop=CFRunLoopGetCurrent();let shouldStop=stopped;lock.unlock();if shouldStop{return}
            // Pausing the display link on an idle desktop must not let its
            // otherwise empty run loop exit before the next decoded picture.
            let keepAlive=Port();RunLoop.current.add(keepAlive,forMode:.common)
            defer{keepAlive.invalidate()}
            let link=CAMetalDisplayLink(metalLayer:layer);self.link=link;link.delegate=self;link.preferredFrameLatency=1
            link.preferredFrameRateRange=CAFrameRateRange(minimum:30,maximum:120,preferred:60)
            link.add(to:.current,forMode:.common);CFRunLoopRun();link.invalidate();self.link=nil
        }
    }
    func stop(){lock.lock();stopped=true;let loop=loop;lock.unlock();if let loop{CFRunLoopPerformBlock(loop,CFRunLoopMode.commonModes.rawValue){CFRunLoopStop(loop)};CFRunLoopWakeUp(loop)}}
    private func wake(){lock.lock();let loop=loop;lock.unlock();if let loop{CFRunLoopPerformBlock(loop,CFRunLoopMode.commonModes.rawValue){[weak self] in guard let self else{return};self.lock.lock();let size=self.size;self.lock.unlock();if size.width>0 && size.height>0{self.layer.drawableSize=size};self.link?.isPaused=false};CFRunLoopWakeUp(loop)}}
    private func reportInitializationFailure(_ frame:ClientVideoFrame){
        lock.lock();let reason=initializationError,report = !initializationReported;initializationReported=true;lock.unlock()
        if report,let reason{Wire.send(["event":"decoder_failed","window":["id":frame.id,"generation":frame.generation],"generation":frame.streamGeneration,"reason":reason])}
    }
    func upload(_ frame:ClientVideoFrame){lock.lock();let failed=initializationError != nil;pending=frame;dirty=true;lock.unlock();if failed{reportInitializationFailure(frame)}else{wake()}}
    func layout(size:CGSize,fill:Bool,visible:Bool){lock.lock();if self.size != size || self.fill != fill || self.visible != visible{self.size=size;self.fill=fill;self.visible=visible;dirty=true};lock.unlock();wake()}
    func metalDisplayLink(_ link:CAMetalDisplayLink,needsUpdate update:CAMetalDisplayLink.Update){
        lock.lock()
        guard !stopped,visible,dirty,size.width>0,size.height>0 else{lock.unlock();link.isPaused=true;return}
        guard inFlight.wait(timeout:.now()) == .success else{lock.unlock();return}
        let next=pending,size=size,fill=fill;pending=nil;dirty=false;lock.unlock()
        if let next{current=next;do{try uploadTexture(next)}catch{inFlight.signal();Wire.send(["event":"decoder_failed","window":["id":next.id,"generation":next.generation],"generation":next.streamGeneration,"reason":error.localizedDescription]);return}}
        guard let current,let texture,let pipeline,let buffer=queue.makeCommandBuffer() else{inFlight.signal();return}
        let pass=MTLRenderPassDescriptor();pass.colorAttachments[0].texture=update.drawable.texture;pass.colorAttachments[0].loadAction = .clear;pass.colorAttachments[0].storeAction = .store;pass.colorAttachments[0].clearColor=MTLClearColorMake(0,0,0,1)
        guard let encoder=buffer.makeRenderCommandEncoder(descriptor:pass)else{inFlight.signal();return}
        var parameters=SIMD4<Float>(1,1,Float(current.width)/Float(texture.width),Float(current.height)/Float(texture.height))
        if !fill{let aspect=Float(current.width)/Float(current.height),screen=Float(size.width/size.height);if screen>aspect{parameters.x=aspect/screen}else{parameters.y=screen/aspect}}
        encoder.setRenderPipelineState(pipeline);encoder.setVertexBytes(&parameters,length:MemoryLayout<SIMD4<Float>>.size,index:0);encoder.setFragmentTexture(texture,index:0);encoder.drawPrimitives(type:.triangleStrip,vertexStart:0,vertexCount:4);encoder.endEncoding()
        let retainedSurface=current.pixelBuffer,retainedMapping=mapped,signal=inFlight
        let diagnostic=diagnosticPending;diagnosticPending=false;let diagnosticID=diagnosticID
        buffer.addCompletedHandler{completed in
            withExtendedLifetime((retainedSurface,retainedMapping)){}
            if Wire.diagnosticsEnabled && diagnostic{Wire.diagnostic("metal_present_completed",["renderer":diagnosticID,"width":current.width,"height":current.height,"success":completed.status == .completed,"error":completed.error?.localizedDescription ?? ""])}
            signal.signal()
        }
        presentedCount += 1
        let reportTiming=Wire.diagnosticsEnabled && presentedCount.isMultiple(of:60)
        let budgetMs=(update.targetTimestamp-CACurrentMediaTime())*1000,targetMs=(update.targetPresentationTimestamp-CACurrentMediaTime())*1000
        if current.id != 0{update.drawable.addPresentedHandler{[weak self] _ in
            Wire.send(["event":"presented","window":["id":current.id,"generation":current.generation],"generation":current.streamGeneration,"number":current.number])
            if let self,Wire.diagnosticsEnabled{
                let now=DispatchTime.now().uptimeNanoseconds;self.lock.lock();let previous=self.lastPresentedNs;self.lastPresentedNs=now;self.lock.unlock()
                if previous>0 && now>previous+50_000_000{Wire.diagnostic("presentation_gap",["gap_us":(now-previous)/1000,"unix_us":Int64(Date().timeIntervalSince1970*1_000_000),"number":current.number,"generation":current.streamGeneration])}
            }
            if reportTiming && current.decodedAt>0{Wire.diagnostic("metal_frame_timing",["number":current.number,"decode_present_us":(DispatchTime.now().uptimeNanoseconds-current.decodedAt)/1000,"delegate_budget_ms":budgetMs,"target_delay_ms":targetMs])}
        }}
        // CAMetalDisplayLink supplies and schedules this drawable; explicit atTime
        // presentation raises CAMetalDrawableInvalidOperation.
        buffer.present(update.drawable);buffer.commit()
    }
    private func uploadTexture(_ frame:ClientVideoFrame)throws{
        if let pixel=frame.pixelBuffer {
            guard let cache else{throw Wire.failure("Missing Metal surface cache")}
            var result:CVMetalTexture?
            guard CVMetalTextureCacheCreateTextureFromImage(nil,cache,pixel,nil,.bgra8Unorm,CVPixelBufferGetWidth(pixel),CVPixelBufferGetHeight(pixel),0,&result) == kCVReturnSuccess,let result,let texture=CVMetalTextureGetTexture(result) else{throw Wire.failure("Decoded surface is not Metal compatible")}
            mapped=result;self.texture=texture
        }else{
            guard frame.bytes.count==frame.width*frame.height*4 else{throw Wire.failure("Invalid CPU video picture")}
            if texture?.width != frame.width || texture?.height != frame.height || mapped != nil{
                let descriptor=MTLTextureDescriptor.texture2DDescriptor(pixelFormat:.bgra8Unorm,width:frame.width,height:frame.height,mipmapped:false);descriptor.usage = .shaderRead;descriptor.storageMode = .shared
                texture=device.makeTexture(descriptor:descriptor);mapped=nil;diagnosticPending=true
            }
            frame.bytes.withUnsafeBytes{bytes in if let base=bytes.baseAddress{texture?.replace(region:MTLRegionMake2D(0,0,frame.width,frame.height),mipmapLevel:0,withBytes:base,bytesPerRow:frame.width*4)}}
        }
    }
}

@MainActor final class DockProxy:NSObject,NSApplicationDelegate{
    init(name:String,icon:String){super.init();ProcessInfo.processInfo.processName=name;if let data=Data(base64Encoded:icon),let image=NSImage(data:data){NSApp.applicationIconImage=image}}
    func applicationDidBecomeActive(_ notification:Notification){Wire.send(["event":"activate"])}
    func applicationShouldHandleReopen(_ sender:NSApplication,hasVisibleWindows flag:Bool)->Bool{Wire.send(["event":"activate"]);return false}
    func applicationShouldTerminate(_ sender:NSApplication)->NSApplication.TerminateReply{Wire.send(["event":"quit"]);return .terminateCancel}
}
