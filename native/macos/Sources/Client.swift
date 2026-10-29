import AppKit
import Metal
import QuartzCore

@MainActor final class NativeClient: NSObject, NSApplicationDelegate, NSMenuDelegate {
    var host: String
    var windows: [String: RemoteProxy] = [:]
    var docks: [String: Process] = [:]
    var session = frames_session_new()
    var sequence = 0
    var fileShelf: FileShelf?
    weak var activeProxy:RemoteProxy?
    var viewMode="windows"
    var adbEnabled=false
    var menuTracking=0
    var needsMenu=false
    var focusAfterSwitch=false
    init(host: String) { self.host = host; super.init() }
    deinit { if let session { frames_session_free(session) } }
    static func key(_ object: [String: Any]) -> String { "\(object["id"] ?? 0):\(object["generation"] ?? 0)" }
    func applicationDidFinishLaunching(_ notification:Notification){Wire.diagnostic("client_started",["pid":ProcessInfo.processInfo.processIdentifier]);installMenu()}
    // A host can temporarily have no shared windows during graph reconciliation.
    // Keep the session alive until an explicit disconnect or a transport failure.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender:NSApplication)->Bool{false}
    func applicationShouldTerminate(_ sender:NSApplication)->NSApplication.TerminateReply{Wire.diagnostic("client_terminate_requested");return .terminateNow}
    func applicationDidResignActive(_ notification:Notification){for proxy in windows.values{proxy.view.releaseInput()}}
    func applicationDidHide(_ notification:Notification){for proxy in windows.values{proxy.view.releaseInput()}}
    func activate(_ proxy:RemoteProxy){activeProxy=proxy;installMenu()}
    func menuWillOpen(_ menu:NSMenu){menuTracking+=1}
    func menuDidClose(_ menu:NSMenu){menuTracking=max(0,menuTracking-1);if menuTracking==0 && needsMenu{DispatchQueue.main.async{[weak self] in self?.installMenu()}}}
    func installMenu(){
        guard menuTracking==0 else{needsMenu=true;return};needsMenu=false
        let bar=NSMenu();bar.autoenablesItems=false
        let local=NSMenu();local.autoenablesItems=false;local.delegate=self
        let appItem=NSMenuItem(title:"Frames",action:nil,keyEquivalent:"");appItem.submenu=local;bar.addItem(appItem)
        func add(_ title:String,_ action:Selector,_ state:Bool?=nil){let item=NSMenuItem(title:title,action:action,keyEquivalent:"");item.target=self;if let state{item.state=state ? .on:.off};local.addItem(item)}
        add("Per-window view",#selector(windowsView),viewMode=="windows");add("Full desktop view",#selector(desktopView),viewMode=="desktop")
        local.addItem(.separator())
        if let proxy=activeProxy{
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
        let adb=NSMenuItem(title:"Android ADB bridge",action:#selector(toggleAdb),keyEquivalent:"");adb.target=self;adb.state=adbEnabled ? .on:.off;devices.addItem(adb)
        let support=NSMenuItem(title:"Device forwarding support...",action:#selector(deviceSupport),keyEquivalent:"");support.target=self;devices.addItem(support)
        let deviceItem=NSMenuItem(title:"Devices",action:nil,keyEquivalent:"");deviceItem.submenu=devices;bar.addItem(deviceItem)
        NSApp.mainMenu=bar
        Wire.diagnostic("menu_installed", ["top_level_items":bar.items.count,"remote_top_level_items":activeProxy?.remoteMenuItems.count ?? 0,"activation_policy":NSApp.activationPolicy().rawValue])
    }
    @objc func windowsView(){Wire.send(["event":"release"]);Wire.send(["event":"view_mode","mode":"windows"])}
    @objc func desktopView(){Wire.send(["event":"release"]);Wire.send(["event":"view_mode","mode":"desktop"])}
    @objc func disconnect(){Wire.diagnostic("client_disconnect_requested");Wire.send(["event":"release"]);Wire.send(["event":"disconnect"])}
    @objc func toggleAdb(){Wire.send(["event":"adb_bridge","enabled":!adbEnabled])}
    @objc func deviceSupport(){let alert=NSAlert();alert.messageText="Device forwarding";alert.informativeText="Android ADB: start adb on this computer, then enable Android ADB bridge. On the host use adb -H 127.0.0.1 -P 15037. Raw USB, fastboot, DFU, and iOS passthrough require separate driver support and are not enabled.";alert.runModal()}
    func error(_ title:String,_ message:String){let alert=NSAlert();alert.messageText=title;alert.informativeText=message;alert.runModal()}
    func update(_ message: [String: Any]) {
        switch message["type"] as? String {
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
            for key in Array(windows.keys) where !keys.contains(key) { windows.removeValue(forKey: key)?.destroy() }
            let sorted = list.sorted { ($0["parent"] is NSNull) && !($1["parent"] is NSNull) }
            for remote in sorted {
                guard let id = remote["key"] as? [String: Any] else { continue }; let key = Self.key(id)
                if let proxy = windows[key] { proxy.remote = remote; proxy.refreshTitle(); continue }
                let proxy = RemoteProxy(remote: remote, host: host);proxy.client=self; windows[key] = proxy
                if let owner = remote["parent"] as? [String: Any], let parent = windows[Self.key(owner)] {
                    parent.window.addChildWindow(proxy.window, ordered: .above)
                    let pr = parent.remote["bounds"] as? [String: Double] ?? [:], cr = remote["bounds"] as? [String: Double] ?? [:]
                    let body=parent.window.convertToScreen(parent.view.convert(parent.view.bounds,to:nil))
                    proxy.window.setFrameOrigin(NSPoint(x:body.minX+(cr["x"] ?? 0)-(pr["x"] ?? 0),y:body.maxY-((cr["y"] ?? 0)-(pr["y"] ?? 0))-proxy.view.bounds.height))
                }
                proxy.window.orderFront(nil); proxy.visibility()
                let appID = remote["app_id"] as? String ?? "unknown"
                if docks[appID] == nil { startDock(appID, remote) }
            }
            let alive = Set(list.compactMap { $0["app_id"] as? String })
            for id in Array(docks.keys) where !alive.contains(id) { docks.removeValue(forKey: id)?.terminate() }
            if focusAfterSwitch,let proxy=windows.values.first{focusAfterSwitch=false;NSApp.activate(ignoringOtherApps:true);proxy.window.makeKeyAndOrderFront(nil)}
        case "frame":
            guard let id = message["window"] as? [String: Any], let proxy = windows[Self.key(id)], let encoded = message["bgra"] as? String, let bytes = Data(base64Encoded: encoded), let width = message["width"] as? Int, let height = message["height"] as? Int else { return }
            guard width > 0, height > 0, width <= 8192, height <= 8192, bytes.count == width*height*4 else { return }
            proxy.view.display(bytes, width: width, height: height); proxy.status = ""; proxy.refreshTitle()
        case "status": if let id = message["window"] as? [String: Any], let proxy = windows[Self.key(id)] { proxy.status = message["status"] as? String ?? ""; proxy.refreshTitle() }
        case "ack": if let id = message["window"] as? [String: Any], let proxy = windows[Self.key(id)] {
            if let actual = message["actual"] as? [String: Any] { proxy.remote = actual }
            proxy.commandError = message["error"] as? String
            proxy.refreshTitle()
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
                    else if event == "activate", let proxy = proxies.first { NSApp.activate(ignoringOtherApps: true); proxy.window.makeKeyAndOrderFront(nil) }
                }
            }
        }
        do { try process.run(); docks[id] = process } catch { fputs("Dock proxy: \(error)\n", stderr) }
    }
    func applicationWillTerminate(_ notification: Notification) { Wire.diagnostic("client_terminated",["window_count":windows.count]);for process in docks.values { process.terminate() }; Wire.send(["event":"release"]) }
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

@MainActor final class RemoteProxy: NSObject, NSWindowDelegate {
    weak var client:NativeClient?
    var remoteMenuItems:[[String:Any]]=[]
    var remote: [String: Any]
    let window: NSWindow
    let view: RemoteView
    let preference: String
    var mode: String
    var fill: Bool
    var swap: Bool
    var physical: Bool
    var status = "Connecting stream…"
    var commandError:String?
    var resizeWork: DispatchWorkItem?
    var lastSize = CGSize.zero
    var destroying = false
    var id: [String: Any] { remote["key"] as? [String: Any] ?? [:] }
    var resizable: Bool { remote["resize"] as? String == "supported" }
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
        let transient = ["menu","tooltip","popover"].contains(remote["role"] as? String ?? "") && !(remote["parent"] is NSNull)
        window = NSWindow(contentRect:rect,styleMask: transient ? [.borderless] : [.titled,.closable,.miniaturizable,.resizable],backing:.buffered,defer:false)
        view = RemoteView(frame:NSRect(origin:.zero,size:rect.size))
        super.init(); view.proxy = self; window.contentView = view; window.delegate = self; window.isReleasedWhenClosed = false; window.acceptsMouseMovedEvents = true; window.title = remote["title"] as? String ?? "Remote window"
        window.collectionBehavior = [.managed, .fullScreenPrimary]
        if remote["role"] as? String == "desktop"{window.center()}
        refreshTitle()
    }
    func send(_ event: String, _ fields: [String: Any] = [:]) { var value = fields;value["event"] = event;value["window"] = id;Wire.send(value) }
    func visibility() { send("visibility",["visible":window.isVisible && !window.isMiniaturized && window.occlusionState.contains(.visible),"focused":window.isKeyWindow]) }
    func refreshTitle() { let detail=commandError ?? status;window.title = "\(remote["title"] ?? "Remote window") — \(mode == "resize_remote" && resizable ? "Resize remote" : "Stretch locally")\(physical ? " / Physical keys":"")\(detail.isEmpty ? "" : " — "+detail)" }
    func save() { UserDefaults.standard.set(mode,forKey:preference+":mode");UserDefaults.standard.set(fill,forKey:preference+":fill");UserDefaults.standard.set(swap,forKey:preference+":swap");UserDefaults.standard.set(physical,forKey:preference+":physical");refreshTitle();view.redraw() }
    @objc func setResize() { if resizable { mode="resize_remote";save() } }
    @objc func setStretch() { mode="stretch_local";save() }
    @objc func toggleFill() { fill.toggle();save() }
    @objc func toggleSwap() { view.releaseInput();swap.toggle();save() }
    @objc func togglePhysical() { view.releaseInput();physical.toggle();save() }
    @objc func remoteMenu(){send("action",["action":["kind":"menu"]])}
    @objc func fetchFile(){let alert=NSAlert();alert.messageText="Fetch remote file";alert.informativeText="Enter a path relative to the remote user's home folder.";let input=NSTextField(frame:NSRect(x:0,y:0,width:400,height:24));input.stringValue="Downloads/";alert.accessoryView=input;alert.addButton(withTitle:"Download");alert.addButton(withTitle:"Cancel");if alert.runModal() == .alertFirstButtonReturn{send("download_file",["path":input.stringValue])}}
    @objc func pressMenu(_ item:NSMenuItem){if let id=item.representedObject as? String{send("action",["action":["kind":"menu_item","id":id]])}}
    func makeMenu(_ items:[[String:Any]])->NSMenu{
        func build(_ items:[[String:Any]],_ depth:Int)->NSMenu{let menu=NSMenu();menu.autoenablesItems=false;menu.delegate=client;guard depth<8 else{return menu};for value in items.prefix(200){let title=(value["title"] as? String ?? "").components(separatedBy:"\t")[0].replacingOccurrences(of:"&",with:"");if title.isEmpty{menu.addItem(.separator());continue};let item=NSMenuItem(title:title,action:#selector(pressMenu(_:)),keyEquivalent:"");item.target=self;item.representedObject=value["id"];item.isEnabled=value["enabled"] as? Bool ?? true;item.state=(value["checked"] as? Bool ?? false) ? .on:.off;if let children=value["children"] as? [[String:Any]],!children.isEmpty{item.submenu=build(children,depth+1)};menu.addItem(item)};return menu}
        return build(items,0)
    }
    func windowShouldClose(_ sender: NSWindow) -> Bool { if destroying { return true };if remote["role"] as? String == "desktop"{client?.disconnect()}else{send("action",["action":["kind":"close"]])};return false }
    func destroy() { destroying=true;resizeWork?.cancel();window.close() }
    func windowDidBecomeKey(_ notification: Notification) { client?.activate(self);send("action",["action":["kind":"focus"]]);if remote["role"] as? String != "desktop"{remoteMenu()};visibility() }
    func windowDidResignKey(_ notification: Notification) { view.releaseInput();visibility() }
    func windowDidMiniaturize(_ notification: Notification) { visibility() }
    func windowDidDeminiaturize(_ notification: Notification) { visibility() }
    func windowDidChangeOcclusionState(_ notification: Notification) { visibility() }
    func windowDidResize(_ notification: Notification) {
        view.redraw();guard resizable,mode=="resize_remote",let size=window.contentView?.bounds.size,size != lastSize else{return};lastSize=size;resizeWork?.cancel()
        let work=DispatchWorkItem{[weak self] in self?.send("action",["action":["kind":"resize","width":size.width,"height":size.height]])};resizeWork=work;DispatchQueue.main.asyncAfter(deadline:.now()+0.05,execute:work)
    }
}

@MainActor final class RemoteView: NSView {
    weak var proxy: RemoteProxy?
    var renderer: MetalPicture?
    var picture = CGSize(width:1,height:1)
    var marked = NSAttributedString(string:"")
    var forwarded: Set<UInt16> = []
    var modifiers = KeyboardModifiers()
    var optionPointer = false
    override var acceptsFirstResponder: Bool { true }
    override init(frame:NSRect) { super.init(frame:frame);wantsLayer=true;renderer=try? MetalPicture(view:self);registerForDraggedTypes([.fileURL]);addTrackingArea(NSTrackingArea(rect:.zero,options:[.activeAlways,.inVisibleRect,.mouseMoved],owner:self,userInfo:nil)) }
    required init?(coder:NSCoder) { fatalError("not used") }
    func display(_ bytes:Data,width:Int,height:Int) { picture=CGSize(width:width,height:height);renderer?.upload(bytes,width:width,height:height);redraw() }
    func redraw() { renderer?.draw(size:convertToBacking(bounds).size,fill:proxy?.fill ?? false) }
    override func layout() { super.layout();redraw() }
    func position(_ event:NSEvent)->[String:Any] {
        let p=convert(event.locationInWindow,from:nil);var rect=bounds
        if !(proxy?.fill ?? false) { let aspect=picture.width/picture.height;if rect.width/rect.height>aspect{let width=rect.height*aspect;rect.origin.x=(rect.width-width)/2;rect.size.width=width}else{let height=rect.width/aspect;rect.origin.y=(rect.height-height)/2;rect.size.height=height} }
        return ["x":min(1,max(0,(p.x-rect.minX)/max(1,rect.width))),"y":min(1,max(0,1-(p.y-rect.minY)/max(1,rect.height)))]
    }
    func pointer(_ event:NSEvent,kind:String,button:Int?=nil,down:Bool?=nil) { if down == true{optionPointer=true;synchronizeModifiers()};var value=position(event);value["kind"]=kind;if let button{value["button"]=button};if let down{value["down"]=down};if kind=="button"{Wire.diagnostic("pointer_button",["window":proxy?.id ?? [:],"x":value["x"] ?? 0,"y":value["y"] ?? 0,"view_width":bounds.width,"view_height":bounds.height,"picture_width":picture.width,"picture_height":picture.height,"down":down ?? false])};proxy?.send("input",["input":value]) }
    override func mouseMoved(with event:NSEvent){pointer(event,kind:"pointer")}
    override func mouseDragged(with event:NSEvent){pointer(event,kind:"pointer")}
    override func rightMouseDragged(with event:NSEvent){pointer(event,kind:"pointer")}
    override func otherMouseDragged(with event:NSEvent){pointer(event,kind:"pointer")}
    override func mouseDown(with event:NSEvent){window?.makeFirstResponder(self);pointer(event,kind:"button",button:0,down:true)}
    override func mouseUp(with event:NSEvent){pointer(event,kind:"button",button:0,down:false)}
    override func rightMouseDown(with event:NSEvent){pointer(event,kind:"button",button:1,down:true)}
    override func rightMouseUp(with event:NSEvent){pointer(event,kind:"button",button:1,down:false)}
    override func otherMouseDown(with event:NSEvent){if event.buttonNumber==2{pointer(event,kind:"button",button:2,down:true)}}
    override func otherMouseUp(with event:NSEvent){if event.buttonNumber==2{pointer(event,kind:"button",button:2,down:false)}}
    override func scrollWheel(with event:NSEvent){optionPointer=true;synchronizeModifiers();var value=position(event);value["kind"]="scroll";value["dx"]=event.scrollingDeltaX/20;value["dy"] = -event.scrollingDeltaY/20;proxy?.send("input",["input":value])}
    func key(_ usage:UInt16,down:Bool){var mapped=usage;if proxy?.swap == true && proxy?.physical != true{mapped=[UInt16(0xe0):0xe3,0xe3:0xe0,0xe4:0xe7,0xe7:0xe4][usage] ?? usage};proxy?.send("input",["input":["kind":"key","usage":mapped,"down":down]])}
    func synchronizeModifiers(suspend:Bool=false){for(usage,down)in modifiers.synchronize(physical:proxy?.physical == true,optionShortcut:!forwarded.isEmpty || optionPointer,suspend:suspend){key(usage,down:down)}}
    func releaseInput(){proxy?.send("release");forwarded.removeAll();modifiers.reset();optionPointer=false;marked=NSAttributedString(string:"");inputContext?.discardMarkedText()}
    override func keyDown(with event:NSEvent){
        optionPointer=false
        if event.modifierFlags.contains(.command),[48,49].contains(event.keyCode){releaseInput();return} // Local app switching / Spotlight.
        guard let usage=usageToMac.first(where:{$0.value==event.keyCode})?.key else{return}
        if event.keyCode==111,event.modifierFlags.contains([.control,.option]){releaseInput();window?.miniaturize(nil);return}
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
    override func draggingEntered(_ sender:NSDraggingInfo)->NSDragOperation{.copy}
    override func performDragOperation(_ sender:NSDraggingInfo)->Bool{guard let urls=sender.draggingPasteboard.readObjects(forClasses:[NSURL.self],options:[.urlReadingFileURLsOnly:true]) as? [URL],!urls.isEmpty else{return false};let point=convert(sender.draggingLocation,from:nil);proxy?.send("send_files",["paths":urls.map(\.path),"x":min(1,max(0,point.x/max(1,bounds.width))),"y":min(1,max(0,1-point.y/max(1,bounds.height)))]);return true}
}

#if compiler(>=6.0)
extension RemoteView: @preconcurrency NSTextInputClient {}
#else
extension RemoteView: NSTextInputClient {}
#endif

@MainActor final class MetalPicture {
    let device:MTLDevice;let queue:MTLCommandQueue;let layer:CAMetalLayer;let pipeline:MTLRenderPipelineState
    var texture:MTLTexture?
    let diagnosticID = UUID().uuidString
    var diagnosticPending = true
    init(view:NSView)throws{
        guard let device=MTLCreateSystemDefaultDevice(),let queue=device.makeCommandQueue()else{throw Wire.failure("Metal unavailable")};self.device=device;self.queue=queue
        layer=CAMetalLayer();layer.device=device;layer.pixelFormat = .bgra8Unorm;layer.framebufferOnly=true;view.layer=layer
        let source="""
        #include <metal_stdlib>
        using namespace metal;
        struct V { float4 p [[position]]; float2 uv; };
        vertex V vertexMain(uint id [[vertex_id]], constant float2 &scale [[buffer(0)]]) {
          float2 p[4]={float2(-1,-1),float2(1,-1),float2(-1,1),float2(1,1)};
          V v; v.p=float4(p[id]*scale,0,1); v.uv=float2((p[id].x+1)*0.5,(1-p[id].y)*0.5);return v;
        }
        fragment float4 fragmentMain(V v [[stage_in]], texture2d<float> picture [[texture(0)]]) {
          constexpr sampler s(filter::linear);return picture.sample(s,v.uv);
        }
        """
        let library=try device.makeLibrary(source:source,options:nil);let descriptor=MTLRenderPipelineDescriptor();descriptor.vertexFunction=library.makeFunction(name:"vertexMain");descriptor.fragmentFunction=library.makeFunction(name:"fragmentMain");descriptor.colorAttachments[0].pixelFormat = .bgra8Unorm;pipeline=try device.makeRenderPipelineState(descriptor:descriptor)
    }
    func upload(_ data:Data,width:Int,height:Int){
        if texture?.width != width||texture?.height != height{let desc=MTLTextureDescriptor.texture2DDescriptor(pixelFormat:.bgra8Unorm,width:width,height:height,mipmapped:false);desc.usage = .shaderRead;desc.storageMode = .shared;texture=device.makeTexture(descriptor:desc);diagnosticPending=true}
        data.withUnsafeBytes{bytes in if let base=bytes.baseAddress{texture?.replace(region:MTLRegionMake2D(0,0,width,height),mipmapLevel:0,withBytes:base,bytesPerRow:width*4)}}
    }
    func draw(size:CGSize,fill:Bool){
        guard size.width>0,size.height>0,let texture else{return};layer.drawableSize=size
        guard let drawable=layer.nextDrawable(),let buffer=queue.makeCommandBuffer()else{return};let pass=MTLRenderPassDescriptor();pass.colorAttachments[0].texture=drawable.texture;pass.colorAttachments[0].loadAction = .clear;pass.colorAttachments[0].storeAction = .store;pass.colorAttachments[0].clearColor=MTLClearColorMake(0,0,0,1)
        guard let encoder=buffer.makeRenderCommandEncoder(descriptor:pass)else{return};var scale=SIMD2<Float>(1,1)
        if !fill{let aspect=Float(texture.width)/Float(texture.height),screen=Float(size.width/size.height);if screen>aspect{scale.x=aspect/screen}else{scale.y=screen/aspect}}
        encoder.setRenderPipelineState(pipeline);encoder.setVertexBytes(&scale,length:MemoryLayout<SIMD2<Float>>.size,index:0);encoder.setFragmentTexture(texture,index:0);encoder.drawPrimitives(type:.triangleStrip,vertexStart:0,vertexCount:4);encoder.endEncoding()
        if Wire.diagnosticsEnabled && diagnosticPending {
            diagnosticPending=false
            let id=diagnosticID,width=texture.width,height=texture.height
            buffer.addCompletedHandler { completed in
                Wire.diagnostic("metal_present_completed", ["renderer":id,"width":width,"height":height,"success":completed.status == .completed,"error":completed.error?.localizedDescription ?? ""])
            }
        }
        buffer.present(drawable);buffer.commit()
    }
}

@MainActor final class DockProxy:NSObject,NSApplicationDelegate{
    init(name:String,icon:String){super.init();ProcessInfo.processInfo.processName=name;if let data=Data(base64Encoded:icon),let image=NSImage(data:data){NSApp.applicationIconImage=image}}
    func applicationDidBecomeActive(_ notification:Notification){Wire.send(["event":"activate"])}
    func applicationShouldHandleReopen(_ sender:NSApplication,hasVisibleWindows flag:Bool)->Bool{Wire.send(["event":"activate"]);return false}
    func applicationShouldTerminate(_ sender:NSApplication)->NSApplication.TerminateReply{Wire.send(["event":"quit"]);return .terminateCancel}
}
