import AppKit
import ScreenCaptureKit
import CoreMedia
import CoreVideo
import ApplicationServices

let desktopCaptureID:CGWindowID=CGWindowID.max-1

@MainActor final class NativeHost: NSObject {
    var windows: [CGWindowID: SCWindow] = [:]
    var captures: [CGWindowID: WindowCapture] = [:]
    var held: Set<UInt16> = []
    var heldButtons: Set<UInt8> = []
    var menuActions: [CGWindowID:[String: AXUIElement]] = [:]
    var audio: SystemAudioCapture?
    var dropper: NativeFileDropper?
    var icons: [String: String] = [:]
    var desktopDisplay:SCDisplay?

    func request(_ command: String, _ data: [String: Any]) async throws -> [String: Any] {
        let id = CGWindowID((data["handle"] as? NSNumber)?.uint32Value ?? 0)
        switch command {
        case "desktop":
            guard CGPreflightScreenCaptureAccess() else{throw Wire.failure("Screen Recording permission is required")}
            let content=try await SCShareableContent.excludingDesktopWindows(false,onScreenWindowsOnly:false)
            guard let display=content.displays.first(where:{$0.displayID==CGMainDisplayID()}) ?? content.displays.first else{throw Wire.failure("No display is available")}
            desktopDisplay=display
            let rect=CGDisplayBounds(display.displayID)
            return ["windows":[["key":["id":desktopCaptureID,"generation":1],"app_id":"app.frames.desktop-view","app_name":"Remote desktop","title":"Remote desktop — primary display","icon_png":NSNull(),"parent":NSNull(),"role":"desktop","bounds":["x":rect.minX,"y":rect.minY,"width":rect.width,"height":rect.height],"scale":Double(display.width)/max(1,Double(rect.width)),"resize":"fixed","minimized":false,"visible":true,"modal":false]]]
        case "windows": return try await enumerate(exclusions: data["exclude"] as? [String] ?? [])
        case "capture":
            if captures[id] == nil {
                let capture:WindowCapture
                if id==desktopCaptureID,let display=desktopDisplay{capture=WindowCapture(display:display)}
                else if let window=windows[id]{capture=WindowCapture(window:window)}
                else{throw Wire.failure("Window or display no longer exists")}
                captures[id] = capture
                do { try await capture.start() }
                catch {
                    if captures[id] === capture { captures.removeValue(forKey: id) }
                    await capture.stop()
                    throw error
                }
            }
            return [:]
        case "stop": if let capture = captures.removeValue(forKey: id) { await capture.stop() }; return [:]
        case "action":
            let command=data["action"] as? [String:Any] ?? [:]
            let resizing=command["kind"] as? String == "resize"
            let element=resizing ? windows[id].flatMap(axWindow) : nil
            try action(id,command)
            if let element { try await synchronizeResize(id,element) }
            return [:]
        case "input": try input(id, data["event"] as? [String: Any] ?? [:]); return [:]
        case "release": release(); return [:]
        case "menu": return ["items": try menu(id)]
        case "audio_start":
            if audio == nil {
                let capture=SystemAudioCapture();audio=capture
                do { try await capture.start() }
                catch { if audio === capture { audio=nil };await capture.stop();throw error }
            }
            return ["format":["rate":48000,"channels":2]]
        case "audio_stop": let capture=audio;audio=nil;await capture?.stop();return [:]
        case "drop_files":
            guard let window=windows[id],let paths=data["paths"] as? [String],let x=data["x"] as? Double,let y=data["y"] as? Double,x.isFinite,y.isFinite else{throw Wire.failure("Invalid file drop")}
            let point=CGPoint(x:window.frame.minX+min(1,max(0,x))*window.frame.width,y:window.frame.minY+min(1,max(0,y))*window.frame.height)
            dropper=NativeFileDropper(paths:paths,point:point);try dropper?.start();return [:]
        case "clipboard_image":
            if NSPasteboard.general.types?.contains(NSPasteboard.PasteboardType("org.nspasteboard.ConcealedType")) == true { return [:] }
            if let png = NSPasteboard.general.data(forType: .png) { return ["png":png.base64EncodedString()] }
            if let tiff = NSPasteboard.general.data(forType: .tiff), let image = NSBitmapImageRep(data:tiff), let png=image.representation(using:.png,properties:[:]) { return ["png":png.base64EncodedString()] }; return [:]
        case "set_clipboard_image":
            guard let encoded=data["png"] as? String,let png=Data(base64Encoded:encoded),png.count<=2*1024*1024,NSImage(data:png) != nil else { throw Wire.failure("Invalid clipboard image") }
            NSPasteboard.general.clearContents();NSPasteboard.general.setData(png,forType:.png);return [:]
        case "clipboard": return ["text": NSPasteboard.general.string(forType: .string) as Any? ?? NSNull()]
        case "set_clipboard":
            guard let text = data["text"] as? String, text.utf8.count <= 2 * 1024 * 1024 else { throw Wire.failure("Clipboard text too large") }
            NSPasteboard.general.clearContents(); NSPasteboard.general.setString(text, forType: .string); return [:]
        default: throw Wire.failure("Unsupported native command")
        }
    }

    // AX accepts a resize before ScreenCaptureKit publishes its new bounds.
    // Wait for the actual (possibly constrained) AX size, not the requested size,
    // so the following window snapshot cannot acknowledge pre-resize geometry.
    func synchronizeResize(_ id:CGWindowID,_ element:AXUIElement) async throws {
        for attempt in 0..<12 {
            let content=try await SCShareableContent.excludingDesktopWindows(true,onScreenWindowsOnly:false)
            guard let window=content.windows.first(where:{$0.windowID==id}) else{throw Wire.failure("Window closed during resize")}
            if let value=attribute(element,kAXSizeAttribute),CFGetTypeID(value)==AXValueGetTypeID(){
                var size=CGSize.zero
                if AXValueGetValue(value as! AXValue,.cgSize,&size),abs(window.frame.width-size.width)<1,abs(window.frame.height-size.height)<1{
                    windows[id]=window
                    Wire.diagnostic("resize_geometry_synchronized",["attempts":attempt+1])
                    return
                }
            }
            try await Task.sleep(nanoseconds:20_000_000)
        }
        throw Wire.failure("Host resized the window but capture geometry has not synchronized yet")
    }

    func enumerate(exclusions: [String]) async throws -> [String: Any] {
        guard CGPreflightScreenCaptureAccess() else { throw Wire.failure("Grant Screen Recording to Frames in System Settings → Privacy & Security") }
        let content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: false)
        let layers = CGWindowListCopyWindowInfo([.optionAll, .excludeDesktopElements], kCGNullWindowID) as? [[String: Any]] ?? []
        let metadata = Dictionary(layers.compactMap { item -> (CGWindowID, [String: Any])? in
            guard let number = item[kCGWindowNumber as String] as? NSNumber else { return nil }; return (number.uint32Value, item)
        }, uniquingKeysWith: { first, _ in first })
        var next: [CGWindowID: SCWindow] = [:]
        var result: [[String: Any]] = []
        let candidates = content.windows.filter { window in
            guard let owner = window.owningApplication, owner.processID != getpid(), !owner.bundleIdentifier.hasPrefix("app.frames"), window.frame.width > 0, window.frame.height > 0 else { return false }
            return !exclusions.contains { owner.bundleIdentifier.localizedCaseInsensitiveContains($0) || owner.applicationName.localizedCaseInsensitiveContains($0) }
        }
        for window in candidates {
            guard let owner = window.owningApplication else { continue }
            let id = window.windowID; let info = metadata[id] ?? [:]
            let layer = info[kCGWindowLayer as String] as? Int ?? 0
            if layer < 0 || layer > 30 { continue }
            if (window.title ?? "").isEmpty && layer == 0 { continue }
            let ax = axWindow(window)
            var settable = DarwinBoolean(false)
            let canResize = ax.map { AXUIElementIsAttributeSettable($0, kAXSizeAttribute as CFString, &settable) == .success && settable.boolValue }
            let minimized = ax.flatMap { attribute($0, kAXMinimizedAttribute) as? Bool } ?? false
            let modal = ax.flatMap { attribute($0, kAXModalAttribute) as? Bool } ?? false
            let rect = window.frame
            let filter = SCContentFilter(desktopIndependentWindow: window)
            let parent = layer > 0 ? candidates.first(where: { $0.windowLayer == 0 && $0.owningApplication?.processID == owner.processID })?.windowID : nil
            let running = NSRunningApplication(processIdentifier: owner.processID)
            let iconKey = "\(owner.bundleIdentifier):\(owner.processID)"
            let icon = icons[iconKey] ?? running?.icon.flatMap { image -> String? in
                var rect = NSRect(x: 0, y: 0, width: 64, height: 64)
                guard let cg = image.cgImage(forProposedRect: &rect, context: nil, hints: nil) else { return nil }
                return NSBitmapImageRep(cgImage: cg).representation(using: .png, properties: [:])?.base64EncodedString()
            }
            if let icon { icons[iconKey] = icon }
            result.append([
                "key": ["id": id, "generation": 1], "app_id": owner.bundleIdentifier,
                "app_name": owner.applicationName, "title": window.title ?? owner.applicationName,
                "icon_png": icon as Any? ?? NSNull(), "parent": parent.map { ["id": $0, "generation": 1] } as Any? ?? NSNull(),
                "role": layer > 0 ? "popover" : (modal ? "dialog" : "window"),
                "bounds": ["x": rect.minX, "y": rect.minY, "width": rect.width, "height": rect.height],
                "scale": Double(filter.pointPixelScale), "resize": canResize.map { $0 ? "supported" : "fixed" } ?? "unknown",
                "minimized": minimized, "visible": !minimized && !(running?.isHidden ?? false), "modal": modal
            ])
            next[id] = window
            if let capture = captures[id], capture.frame != rect { try await capture.resize(window) }
        }
        windows = next
        menuActions=menuActions.filter{next[$0.key] != nil}
        return ["windows": result]
    }

    func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
        var result: CFTypeRef?; guard AXUIElementCopyAttributeValue(element, name as CFString, &result) == .success else { return nil }; return result
    }

    // Public APIs do not expose a universal AX → SCWindow ID conversion.
    // Disable actions when title/frame matching is ambiguous instead of targeting another window.
    func axWindow(_ window: SCWindow) -> AXUIElement? {
        guard AXIsProcessTrusted(), let pid = window.owningApplication?.processID else { return nil }
        let application = AXUIElementCreateApplication(pid)
        guard let candidates = attribute(application, kAXWindowsAttribute) as? [AXUIElement] else { return nil }
        let scored = candidates.compactMap { element -> (AXUIElement, Double)? in
            guard let p = attribute(element, kAXPositionAttribute), let s = attribute(element, kAXSizeAttribute), CFGetTypeID(p) == AXValueGetTypeID(), CFGetTypeID(s) == AXValueGetTypeID() else { return nil }
            var point = CGPoint.zero; var size = CGSize.zero
            guard AXValueGetValue(p as! AXValue, .cgPoint, &point), AXValueGetValue(s as! AXValue, .cgSize, &size) else { return nil }
            let title = attribute(element, kAXTitleAttribute) as? String ?? ""
            let r = window.frame
            let score = abs(point.x-r.minX)+abs(point.y-r.minY)+abs(size.width-r.width)+abs(size.height-r.height)+(title == (window.title ?? "") ? 0 : 100)
            return (element, Double(score))
        }.sorted { $0.1 < $1.1 }
        guard let first = scored.first, first.1 < 8, scored.count == 1 || scored[1].1 > first.1 + 1 else { return nil }
        return first.0
    }

    func action(_ id: CGWindowID, _ action: [String: Any]) throws {
        if id==desktopCaptureID{
            guard ["focus","restore"].contains(action["kind"] as? String ?? "") else{throw Wire.failure("Desktop surfaces do not support application window commands")};return
        }
        guard let window = windows[id], let ax = axWindow(window) else { throw Wire.failure("Window control unavailable: grant Accessibility or use the captured controls") }
        let result: AXError
        switch action["kind"] as? String {
        case "focus":
            if let pid = window.owningApplication?.processID { NSRunningApplication(processIdentifier: pid)?.activate(options: []) }
            result = AXUIElementPerformAction(ax, kAXRaiseAction as CFString)
        case "restore": result = AXUIElementSetAttributeValue(ax, kAXMinimizedAttribute as CFString, kCFBooleanFalse)
        case "close":
            guard let button = attribute(ax, kAXCloseButtonAttribute), CFGetTypeID(button) == AXUIElementGetTypeID() else { throw Wire.failure("Window has no close button") }
            result = AXUIElementPerformAction(button as! AXUIElement, kAXPressAction as CFString)
        case "resize":
            var settable = DarwinBoolean(false)
            guard AXUIElementIsAttributeSettable(ax, kAXSizeAttribute as CFString, &settable) == .success, settable.boolValue,
                  let width = action["width"] as? Double, let height = action["height"] as? Double, width.isFinite, height.isFinite, width >= 32, height >= 32, width <= 16384, height <= 16384 else { throw Wire.failure("Window is not resizable or size is invalid") }
            var size = CGSize(width: width, height: height)
            guard let value = AXValueCreate(.cgSize, &size) else { throw Wire.failure("Invalid size") }
            result = AXUIElementSetAttributeValue(ax, kAXSizeAttribute as CFString, value)
        case "menu_item":
            guard let token=action["id"] as? String,let element=menuActions[id]?[token] else { throw Wire.failure("Menu item expired; refresh the application menus") }
            result=AXUIElementPerformAction(element,kAXPressAction as CFString)
        default: throw Wire.failure("Unsupported window command")
        }
        guard result == .success else { throw Wire.failure("Host application rejected window command (\(result.rawValue))") }
    }

    func menu(_ id:CGWindowID)throws -> [[String:Any]] {
        guard AXIsProcessTrusted(),let window=windows[id],let pid=window.owningApplication?.processID else { throw Wire.failure("Accessibility permission required for remote menus") }
        NSRunningApplication(processIdentifier:pid)?.activate(options:[])
        let application=AXUIElementCreateApplication(pid)
        guard let value=attribute(application,kAXMenuBarAttribute),CFGetTypeID(value)==AXUIElementGetTypeID() else { throw Wire.failure("Application does not expose its menu bar") }
        var actions:[String:AXUIElement]=[:]
        var count=0
        func walk(_ root:AXUIElement,_ depth:Int)->[[String:Any]] {
            guard depth<8,let children=attribute(root,kAXChildrenAttribute) as? [AXUIElement] else{return []}
            return children.prefix(200).compactMap{element in
                count+=1;guard count<=1024 else{return nil}
                let title=attribute(element,kAXTitleAttribute) as? String ?? ""
                let children=walk(element,depth+1)
                if title.isEmpty{return children.isEmpty ? nil : ["id":"","title":"Menu","enabled":true,"checked":false,"children":children]}
                let token=UUID().uuidString;actions[token]=element
                return ["id":token,"title":title,"enabled":attribute(element,kAXEnabledAttribute) as? Bool ?? true,"checked":!(attribute(element,kAXMenuItemMarkCharAttribute) as? String ?? "").isEmpty,"children":children]
            }
        }
        let items=walk(value as! AXUIElement,0);menuActions[id]=actions;return items
    }

    func flags() -> CGEventFlags {
        var flags: CGEventFlags = []
        if !held.isDisjoint(with: [0xe0,0xe4]) { flags.insert(.maskControl) }
        if !held.isDisjoint(with: [0xe1,0xe5]) { flags.insert(.maskShift) }
        if !held.isDisjoint(with: [0xe2,0xe6]) { flags.insert(.maskAlternate) }
        if !held.isDisjoint(with: [0xe3,0xe7]) { flags.insert(.maskCommand) }
        return flags
    }

    func input(_ id: CGWindowID, _ event: [String: Any]) throws {
        guard AXIsProcessTrusted() else { throw Wire.failure("Accessibility permission required") }
        let bounds:CGRect
        if id==desktopCaptureID,let display=desktopDisplay{bounds=CGDisplayBounds(display.displayID)}
        else if let window=windows[id]{bounds=window.frame}
        else{throw Wire.failure("Window no longer exists")}
        let source = CGEventSource(stateID: .hidSystemState)
        let x = event["x"] as? Double ?? 0, y = event["y"] as? Double ?? 0
        guard x.isFinite, y.isFinite else { throw Wire.failure("Invalid pointer position") }
        let point = CGPoint(x: bounds.minX + min(1,max(0,x))*bounds.width, y: bounds.minY + min(1,max(0,y))*bounds.height)
        var output: CGEvent?
        switch event["kind"] as? String {
        case "pointer":
            let type: CGEventType = heldButtons.contains(0) ? .leftMouseDragged : (heldButtons.contains(1) ? .rightMouseDragged : (heldButtons.contains(2) ? .otherMouseDragged : .mouseMoved))
            output = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: point, mouseButton: heldButtons.contains(1) ? .right : (heldButtons.contains(2) ? .center : .left))
        case "button":
            let button = UInt8(event["button"] as? Int ?? 0); let down = event["down"] as? Bool ?? false
            if down { heldButtons.insert(button) } else { heldButtons.remove(button) }
            let type: CGEventType = button == 0 ? (down ? .leftMouseDown : .leftMouseUp) : button == 1 ? (down ? .rightMouseDown : .rightMouseUp) : (down ? .otherMouseDown : .otherMouseUp)
            output = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: point, mouseButton: CGMouseButton(rawValue: UInt32(button)) ?? .left)
        case "scroll": output = CGEvent(scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2, wheel1: Int32(-(event["dy"] as? Double ?? 0)*20), wheel2: Int32((event["dx"] as? Double ?? 0)*20), wheel3: 0)
        case "key":
            let usage = UInt16(event["usage"] as? Int ?? 0), down = event["down"] as? Bool ?? false
            guard let code = usageToMac[usage] else { return }
            if down { held.insert(usage) } else { held.remove(usage) }
            output = CGEvent(keyboardEventSource: source, virtualKey: code, keyDown: down)
        case "text":
            guard let text = event["text"] as? String, text.utf8.count <= 65536 else { throw Wire.failure("Invalid text input") }
            let units = Array(text.utf16)
            guard !units.isEmpty else{return}
            for down in [true,false] { let key = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: down); units.withUnsafeBufferPointer { key?.keyboardSetUnicodeString(stringLength: units.count, unicodeString: $0.baseAddress!) }; key?.post(tap: .cghidEventTap) }
            return
        default: return
        }
        output?.flags = flags(); output?.post(tap: .cghidEventTap)
    }
    func release() {
        for usage in held { if let code = usageToMac[usage] { CGEvent(keyboardEventSource: nil, virtualKey: code, keyDown: false)?.post(tap: .cghidEventTap) } }
        let point = CGEvent(source: nil)?.location ?? .zero
        for button in heldButtons { CGEvent(mouseEventSource: nil, mouseType: button == 0 ? .leftMouseUp : (button == 1 ? .rightMouseUp : .otherMouseUp), mouseCursorPosition: point, mouseButton: CGMouseButton(rawValue:UInt32(button)) ?? .left)?.post(tap: .cghidEventTap) }
        held.removeAll(); heldButtons.removeAll()
    }
}

final class WindowCapture: NSObject, SCStreamOutput, SCStreamDelegate {
    let id: CGWindowID; var frame: CGRect; var stream: SCStream?; var configuration: SCStreamConfiguration
    let queue = DispatchQueue(label: "app.frames.capture", qos: .userInteractive)
    convenience init(window: SCWindow) { self.init(id:window.windowID,frame:window.frame,filter:SCContentFilter(desktopIndependentWindow:window),cursor:false,children:true) }
    convenience init(display: SCDisplay) { self.init(id:desktopCaptureID,frame:CGDisplayBounds(display.displayID),filter:SCContentFilter(display:display,excludingWindows:[]),cursor:true,children:false) }
    init(id:CGWindowID,frame:CGRect,filter:SCContentFilter,cursor:Bool,children:Bool) {
        self.id=id;self.frame=frame
        configuration = SCStreamConfiguration()
        configuration.width = Int(frame.width * CGFloat(filter.pointPixelScale))
        configuration.height = Int(frame.height * CGFloat(filter.pointPixelScale))
        configuration.pixelFormat = kCVPixelFormatType_32BGRA
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 60)
        configuration.queueDepth = 3; configuration.showsCursor = cursor; configuration.includeChildWindows = children
        super.init(); stream = SCStream(filter: filter, configuration: configuration, delegate: self)
    }
    func start() async throws {
        guard let stream else { throw Wire.failure("Window capture was stopped") }
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        try await stream.startCapture()
        if self.stream == nil { try? await stream.stopCapture();throw Wire.failure("Window capture was stopped") }
    }
    func stop() async {
        guard let stream else { return }
        self.stream=nil
        try? stream.removeStreamOutput(self,type:.screen)
        try? await stream.stopCapture()
    }
    func resize(_ window: SCWindow) async throws {
        let filter = SCContentFilter(desktopIndependentWindow: window)
        configuration.width = Int(window.frame.width * CGFloat(filter.pointPixelScale)); configuration.height = Int(window.frame.height * CGFloat(filter.pointPixelScale))
        try await stream?.updateConfiguration(configuration); frame = window.frame
    }
    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, sampleBuffer.isValid, let buffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        CVPixelBufferLockBaseAddress(buffer, .readOnly); defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base = CVPixelBufferGetBaseAddress(buffer) else { return }
        let width = CVPixelBufferGetWidth(buffer), height = CVPixelBufferGetHeight(buffer), stride = CVPixelBufferGetBytesPerRow(buffer)
        guard width > 0, height > 0, width <= 8192, height <= 8192 else { return }
        var data = Data(capacity: width*height*4)
        for row in 0..<height { data.append(base.advanced(by: row*stride).assumingMemoryBound(to: UInt8.self), count: width*4) }
        Wire.send(["event":"frame","handle":id,"width":width,"height":height,"bgra":data.base64EncodedString()])
    }
    func stream(_ stream: SCStream, didStopWithError error: Error) { fputs("Frames capture: \(error.localizedDescription)\n", stderr) }
}

let usageToMac: [UInt16: CGKeyCode] = [4:0,5:11,6:8,7:2,8:14,9:3,10:5,11:4,12:34,13:38,14:40,15:37,16:46,17:45,18:31,19:35,20:12,21:15,22:1,23:17,24:32,25:9,26:13,27:7,28:16,29:6,30:18,31:19,32:20,33:21,34:23,35:22,36:26,37:28,38:25,39:29,40:36,41:53,42:51,43:48,44:49,45:27,46:24,47:33,48:30,49:42,51:41,52:39,53:50,54:43,55:47,56:44,57:57,58:122,59:120,60:99,61:118,62:96,63:97,64:98,65:100,66:101,67:109,68:103,69:111,73:114,74:115,75:116,76:117,77:119,78:121,79:124,80:123,81:125,82:126,0xe0:59,0xe1:56,0xe2:58,0xe3:55,0xe4:62,0xe5:60,0xe6:61,0xe7:54]
