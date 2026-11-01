import AppKit
import ScreenCaptureKit
import CoreMedia
import CoreVideo
import ApplicationServices
import Darwin

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
    var contentFrames:[CGWindowID:CGRect]=[:]
    var contentCrops:[CGWindowID:(pid:pid_t,crop:WindowContent.Crop)]=[:]
    var headers:Set<CGWindowID>=[]
    var axBindings:[CGWindowID:(pid:pid_t,frame:CGRect,element:AXUIElement,closeButton:AXUIElement?)]=[:]
    var lastClick:(window:CGWindowID,button:UInt8,point:CGPoint,time:TimeInterval,count:Int64)?
    var clickCounts:[UInt8:Int64]=[:]

    func request(_ command: String, _ data: [String: Any]) async throws -> [String: Any] {
        if ["action","input","menu","drop_files"].contains(command),let error=HostSessionState.controlError(){throw Wire.failure(error)}
        let id = CGWindowID((data["handle"] as? NSNumber)?.uint32Value ?? 0)
        switch command {
        case "preview":
            if let error=HostSessionState.controlError(){throw Wire.failure(error)}
            let content=try await SCShareableContent.excludingDesktopWindows(false,onScreenWindowsOnly:true)
            let filter:SCContentFilter
            let size:CGSize
            if id != 0 && id != desktopCaptureID {
                guard let window=content.windows.first(where:{$0.windowID==id}) else{throw Wire.failure("Window is unavailable")}
                filter=SCContentFilter(desktopIndependentWindow:window);size=window.frame.size
            } else {
                guard let display=content.displays.first(where:{$0.displayID==CGMainDisplayID()}) ?? content.displays.first else{throw Wire.failure("No display is available")}
                filter=SCContentFilter(display:display,excludingWindows:[]);size=CGSize(width:display.width,height:display.height)
            }
            let scale=min(480/max(1,size.width),270/max(1,size.height))
            let configuration=SCStreamConfiguration()
            configuration.width=max(1,Int(size.width*scale));configuration.height=max(1,Int(size.height*scale));configuration.showsCursor=false
            let image=try await SCScreenshotManager.captureImage(contentFilter:filter,configuration:configuration)
            guard let png=NSBitmapImageRep(cgImage:image).representation(using:.png,properties:[:]) else{throw Wire.failure("Preview encoding failed")}
            return ["png":png.base64EncodedString()]
        case "desktop":
            guard CGPreflightScreenCaptureAccess() else{throw Wire.failure("Screen Recording permission is required")}
            let content=try await SCShareableContent.excludingDesktopWindows(false,onScreenWindowsOnly:false)
            guard let display=content.displays.first(where:{$0.displayID==CGMainDisplayID()}) ?? content.displays.first else{throw Wire.failure("No display is available")}
            desktopDisplay=display
            let rect=CGDisplayBounds(display.displayID)
            return ["windows":[["key":["id":desktopCaptureID,"generation":1],"app_id":"app.frames.desktop-view","app_name":"Remote desktop","title":"Remote desktop — primary display","icon_png":NSNull(),"parent":NSNull(),"role":"desktop","bounds":["x":rect.minX,"y":rect.minY,"width":rect.width,"height":rect.height],"scale":Double(display.width)/max(1,Double(rect.width)),"resize":"fixed","minimized":false,"visible":true,"modal":false]]]
        case "windows": return try await enumerate(exclusions: data["exclude"] as? [String] ?? [])
        case "encoder_keyframe": captures[id]?.hardwareEncoder?.keyframe();return [:]
        case "encoder_delivered": captures[id]?.hardwareEncoder?.delivered(number:data["number"] as? UInt64 ?? 0,refresh:data["refresh"] as? Bool ?? false,token:data["delivery_token"] as? UInt64 ?? 0);return [:]
        case "encoder_configure": captures[id]?.hardwareEncoder?.configure(bitrate:data["bitrate"] as? Int ?? 12_000_000,fps:data["fps"] as? Int ?? 60,scale:data["scale"] as? Int ?? 100);return [:]
        case "capture", "capture_encoded":
            let showCursor=data["show_cursor"] as? Bool ?? true
            if captures[id] == nil {
                let capture:WindowCapture
                if id==desktopCaptureID,let display=desktopDisplay{capture=WindowCapture(display:display,cursor:showCursor)}
                else if let window=windows[id]{capture=WindowCapture(window:window,content:contentFrames[id] ?? window.frame,cursor:showCursor)}
                else{throw Wire.failure("Window or display no longer exists")}
                if command == "capture_encoded" { capture.hardwareEncoder = try NativeVideoEncoder(handle:UInt64(id),codec:data["codec"] as? String ?? "h264",fps:data["fps"] as? Int ?? 60,deliveryToken:data["delivery_token"] as? UInt64 ?? 0) }
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
            if command["kind"] as? String == "set_header" {
                guard id != desktopCaptureID,let window=windows[id],let visible=command["visible"] as? Bool else{throw Wire.failure("Window header is unavailable")}
                if visible{headers.insert(id)}else{headers.remove(id)}
                let rect=contentBounds(window)
                contentFrames[id]=rect
                return [:]
            }
            let resizing=command["kind"] as? String == "resize"
            let element=resizing ? windows[id].flatMap(axWindow) : nil
            do { try action(id,command) }
            catch {
                guard command["kind"] as? String == "close" else{throw error}
                // AX can retire an element or finish a close animation just
                // after its reply times out. Retry the same target once.
                axBindings.removeValue(forKey:id)
                try await Task.sleep(nanoseconds:50_000_000)
                try action(id,command)
            }
            if let element { try await synchronizeResize(id,element) }
            return [:]
        case "input":
            let event=data["event"] as? [String:Any] ?? [:]
            let kind=event["kind"] as? String ?? ""
            if id != desktopCaptureID,let window=windows[id] {
                let buttonDown=kind=="button" && event["down"] as? Bool == true
                let keyToInactive=(kind=="key" || kind=="text") && NSWorkspace.shared.frontmostApplication?.processIdentifier != window.owningApplication?.processID
                if buttonDown || keyToInactive {try await focusForInput(id,window:window)}
            }
            try input(id,event); return [:]
        case "release": release(); return [:]
        case "menu": return ["items": try await menu(id)]
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
            let rect=contentBounds(window)
            let point=CGPoint(x:rect.minX+min(1,max(0,x))*rect.width,y:rect.minY+min(1,max(0,y))*rect.height)
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
                    contentFrames[id]=contentBounds(window,element:element)
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
        var nextContent:[CGWindowID:CGRect]=[:]
        let candidates = content.windows.filter { window in
            guard let owner = window.owningApplication, owner.processID != getpid(), !owner.bundleIdentifier.hasPrefix("dev.creaton.frames"), !owner.bundleIdentifier.hasPrefix("app.frames"), window.frame.width > 0, window.frame.height > 0 else { return false }
            if kill(owner.processID,0) != 0 && errno == ESRCH { return false }
            guard window.isOnScreen, !(NSRunningApplication(processIdentifier:owner.processID)?.isHidden ?? false) else { return false }
            // These processes own desktop/menu-bar surfaces, not application windows.
            if ["com.apple.dock","com.apple.notificationcenterui","com.apple.controlcenter","com.apple.WindowManager","com.apple.systemuiserver","com.apple.wallpaper.agent"].contains(owner.bundleIdentifier) { return false }
            return !exclusions.contains { owner.bundleIdentifier.localizedCaseInsensitiveContains($0) || owner.applicationName.localizedCaseInsensitiveContains($0) }
        }
        for window in candidates {
            // Let queued input run between AX queries for different apps.
            await Task.yield()
            guard let owner = window.owningApplication else { continue }
            let id = window.windowID
            // Do not retain a stale SCK surface after its live window leaves.
            guard let info=metadata[id],(info[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == owner.processID else{continue}
            let layer = info[kCGWindowLayer as String] as? Int ?? 0
            if layer < 0 || layer > 30 { continue }
            if layer == 0 && WindowContent.isTitlebarAccessory(frame:window.frame,ownerFrames:content.windows.filter {
                $0.windowID != id && $0.windowLayer == 0 && $0.owningApplication?.processID == owner.processID
            }.map(\.frame)) { continue }
            if (window.title ?? "").isEmpty && layer == 0 { continue }
            let ax = axWindow(window)
            // Off-screen and transparent helper surfaces can still have titles.
            // Floating windows need an AX window; ordinary occluded windows remain eligible.
            if (info[kCGWindowAlpha as String] as? Double ?? 1) <= 0 || (layer > 0 && ax == nil) { continue }
            var settable = DarwinBoolean(false)
            let canResize = ax.map { AXUIElementIsAttributeSettable($0, kAXSizeAttribute as CFString, &settable) == .success && settable.boolValue }
            let minimized = ax.flatMap { attribute($0, kAXMinimizedAttribute) as? Bool } ?? false
            if minimized { continue }
            let modal = ax.flatMap { attribute($0, kAXModalAttribute) as? Bool } ?? false
            let rect = contentBounds(window,element:ax)
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
                "minimized": minimized, "visible": !minimized && !(running?.isHidden ?? false), "modal": modal,"show_header":headers.contains(id)
            ])
            next[id] = window
            nextContent[id]=rect
            if let capture = captures[id], capture.frame != window.frame || capture.contentFrame != rect { capture.scheduleResize(window,content:rect) }
        }
        windows = next
        contentFrames=nextContent
        contentCrops=contentCrops.filter{next[$0.key]?.owningApplication?.processID == $0.value.pid}
        headers=headers.filter{next[$0] != nil}
        axBindings=axBindings.filter{next[$0.key] != nil}
        menuActions=menuActions.filter{next[$0.key] != nil}
        return ["windows": result]
    }

    func attribute(_ element: AXUIElement, _ name: String) -> CFTypeRef? {
        var result: CFTypeRef?; guard AXUIElementCopyAttributeValue(element, name as CFString, &result) == .success else { return nil }; return result
    }

    func contentBounds(_ window:SCWindow,element:AXUIElement?=nil)->CGRect {
        if headers.contains(window.windowID){return window.frame}
        let id=window.windowID
        guard window.windowLayer==0,let pid=window.owningApplication?.processID else{
            contentCrops.removeValue(forKey:id);return window.frame
        }
        var crop=contentCrops[id].flatMap{$0.pid == pid ? $0.crop:nil} ?? WindowContent.Crop()
        defer{contentCrops[id]=(pid,crop)}
        guard let ax=element ?? axWindow(window) else{return crop.bounds(frame:window.frame,titleButtons:nil)}
        if attribute(ax,"AXFullScreen") as? Bool == true{return crop.bounds(frame:window.frame,titleButtons:[])}
        func rect(_ element:AXUIElement)->CGRect? {
            guard let p=attribute(element,kAXPositionAttribute),let s=attribute(element,kAXSizeAttribute),CFGetTypeID(p)==AXValueGetTypeID(),CFGetTypeID(s)==AXValueGetTypeID() else{return nil}
            var point=CGPoint.zero;var size=CGSize.zero
            guard AXValueGetValue(p as! AXValue,.cgPoint,&point),AXValueGetValue(s as! AXValue,.cgSize,&size) else{return nil}
            return CGRect(origin:point,size:size)
        }
        // AX and SCK can describe different moments during a move or resize.
        guard let axFrame=rect(ax),WindowContent.matchingWindow(frame:window.frame,title:"",candidates:[(axFrame,"")]) != nil else{
            return crop.bounds(frame:window.frame,titleButtons:nil)
        }
        var buttons:[CGRect]=[]
        for name in [kAXCloseButtonAttribute,kAXMinimizeButtonAttribute,kAXZoomButtonAttribute] {
            var value:CFTypeRef?
            let result=AXUIElementCopyAttributeValue(ax,name as CFString,&value)
            if result == .attributeUnsupported{continue}
            guard result == .success,let value,CFGetTypeID(value)==AXUIElementGetTypeID() else{
                return crop.bounds(frame:window.frame,titleButtons:nil)
            }
            let button=value as! AXUIElement
            AXUIElementSetMessagingTimeout(button,0.2)
            guard let frame=rect(button) else{return crop.bounds(frame:window.frame,titleButtons:nil)}
            buttons.append(frame)
        }
        return crop.bounds(frame:window.frame,titleButtons:buttons)
    }

    // Public APIs do not expose a universal AX → SCWindow ID conversion.
    // Disable actions when title/frame matching is ambiguous instead of targeting another window.
    func axWindow(_ window: SCWindow) -> AXUIElement? {
        guard AXIsProcessTrusted(), let pid = window.owningApplication?.processID else { return nil }
        let title=window.title ?? ""
        // Captions change while typing and switching tabs without changing the
        // window's identity. Keep the verified binding through those changes.
        if let cached=axBindings[window.windowID],cached.pid==pid,cached.frame==window.frame{return cached.element}
        let application = AXUIElementCreateApplication(pid)
        AXUIElementSetMessagingTimeout(application,0.2)
        var candidates=attribute(application,kAXWindowsAttribute) as? [AXUIElement] ?? []
        if candidates.isEmpty {
            for name in [kAXFocusedWindowAttribute,kAXMainWindowAttribute] {
                if let value=attribute(application,name),CFGetTypeID(value)==AXUIElementGetTypeID() {
                    let element=value as! AXUIElement
                    if !candidates.contains(where:{CFEqual($0,element)}){candidates.append(element)}
                }
            }
        }
        let located = candidates.compactMap { element -> (element:AXUIElement,frame:CGRect,title:String)? in
            AXUIElementSetMessagingTimeout(element,0.2)
            guard let p = attribute(element, kAXPositionAttribute), let s = attribute(element, kAXSizeAttribute), CFGetTypeID(p) == AXValueGetTypeID(), CFGetTypeID(s) == AXValueGetTypeID() else { return nil }
            var point = CGPoint.zero; var size = CGSize.zero
            guard AXValueGetValue(p as! AXValue, .cgPoint, &point), AXValueGetValue(s as! AXValue, .cgSize, &size) else { return nil }
            let title = attribute(element, kAXTitleAttribute) as? String ?? ""
            return (element,CGRect(origin:point,size:size),title)
        }
        let geometry=located.map{($0.frame,$0.title)}
        var index=WindowContent.matchingWindow(frame:window.frame,title:title,candidates:geometry)
        // SCK may retain pre-resize geometry while AX already reports the live
        // window. Query that same WindowServer ID, including its owner PID,
        // rather than falling back to a different window with the same title.
        var liveFrame:CGRect?
        if index == nil,
           let info=(CGWindowListCopyWindowInfo(.optionIncludingWindow,window.windowID) as? [[String:Any]])?.first(where:{
               ($0[kCGWindowNumber as String] as? NSNumber)?.uint32Value == window.windowID &&
               ($0[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == pid
           }),let bounds=info[kCGWindowBounds as String] as? [String:Any] {
            liveFrame=CGRect(dictionaryRepresentation:bounds as CFDictionary)
            if let liveFrame{index=WindowContent.matchingWindow(frame:liveFrame,title:title,candidates:geometry)}
        }
        guard let index else {
            if window.owningApplication?.bundleIdentifier == "dev.frames.qa.fixture" {
                fputs("Frames QA AX mismatch: SCK=\(window.frame) live=\(String(describing:liveFrame)) AX=\(located.map{($0.frame,$0.title)})\n",stderr)
            }
            return nil
        }
        let element=located[index].element
        let close=attribute(element,kAXCloseButtonAttribute)
        let closeButton=close.flatMap { CFGetTypeID($0)==AXUIElementGetTypeID() ? ($0 as! AXUIElement):nil }
        axBindings[window.windowID]=(pid,window.frame,element,closeButton)
        return element
    }

    func windowStillExists(_ id: CGWindowID, owner: pid_t) -> Bool? {
        if kill(owner,0) != 0 && errno == ESRCH { return false }
        guard let info=CGWindowListCopyWindowInfo(.optionIncludingWindow,id) as? [[String:Any]] else{return nil}
        return info.contains {
            ($0[kCGWindowNumber as String] as? NSNumber)?.uint32Value == id &&
            ($0[kCGWindowOwnerPID as String] as? NSNumber)?.int32Value == owner
        }
    }

    func action(_ id: CGWindowID, _ action: [String: Any]) throws {
        if id==desktopCaptureID{
            guard ["focus","restore"].contains(action["kind"] as? String ?? "") else{throw Wire.failure("Desktop surfaces do not support application window commands")};return
        }
        guard let window = windows[id] else { throw Wire.failure("Window no longer exists") }
        let closing=action["kind"] as? String == "close"
        func closed() -> Bool {
            guard closing,let pid=window.owningApplication?.processID else{return false}
            return windowStillExists(id,owner:pid) == false
        }
        if closed(){axBindings.removeValue(forKey:id);return}
        if action["kind"] as? String == "focus",let pid=window.owningApplication?.processID {
            NSRunningApplication(processIdentifier:pid)?.activate(options:[])
        }
        guard let ax=axWindow(window) else {
            if closed(){return}
            throw Wire.failure("Cannot identify this Mac window for control")
        }
        let result: AXError
        switch action["kind"] as? String {
        case "focus":
            if let pid = window.owningApplication?.processID { NSRunningApplication(processIdentifier: pid)?.activate(options: []) }
            result = AXUIElementPerformAction(ax, kAXRaiseAction as CFString)
        case "restore": result = AXUIElementSetAttributeValue(ax, kAXMinimizedAttribute as CFString, kCFBooleanFalse)
        case "close":
            let current=attribute(ax,kAXCloseButtonAttribute)
            let currentButton=current.flatMap { CFGetTypeID($0)==AXUIElementGetTypeID() ? ($0 as! AXUIElement):nil }
            guard let button=currentButton ?? axBindings[id]?.closeButton else {
                if closed(){return}
                throw Wire.failure("Window has no close button")
            }
            AXUIElementSetMessagingTimeout(button,0.2)
            result = AXUIElementPerformAction(button, kAXPressAction as CFString)
        case "resize":
            var settable = DarwinBoolean(false)
            guard AXUIElementIsAttributeSettable(ax, kAXSizeAttribute as CFString, &settable) == .success, settable.boolValue,
                  let width = action["width"] as? Double, let height = action["height"] as? Double, width.isFinite, height.isFinite, width >= 32, height >= 32, width <= 16384, height <= 16384 else { throw Wire.failure("Window is not resizable or size is invalid") }
            let content=contentBounds(window,element:ax)
            var size = CGSize(width: width+window.frame.width-content.width, height: height+window.frame.height-content.height)
            guard let value = AXValueCreate(.cgSize, &size) else { throw Wire.failure("Invalid size") }
            result = AXUIElementSetAttributeValue(ax, kAXSizeAttribute as CFString, value)
        case "menu_item":
            guard let token=action["id"] as? String,let element=menuActions[id]?[token] else { throw Wire.failure("Menu item expired; refresh the application menus") }
            result=AXUIElementPerformAction(element,kAXPressAction as CFString)
        default: throw Wire.failure("Unsupported window command")
        }
        guard result == .success else {
            axBindings.removeValue(forKey:id)
            // The last-window close can terminate an app before AX replies.
            // Accept it only after WindowServer confirms that exact target left.
            if closed(){return}
            throw Wire.failure("Host application rejected window command (\(result.rawValue))")
        }
    }

    func focusForInput(_ id:CGWindowID,window:SCWindow) async throws {
        try action(id,["kind":"focus"])
        guard let pid=window.owningApplication?.processID else{throw Wire.failure("Window owner exited")}
        for _ in 0..<20 {
            if NSWorkspace.shared.frontmostApplication?.processIdentifier==pid{return}
            try await Task.sleep(nanoseconds:10_000_000)
        }
        throw Wire.failure("Mac did not activate the target application")
    }

    func menu(_ id:CGWindowID)async throws -> [[String:Any]] {
        guard AXIsProcessTrusted(),let window=windows[id],let pid=window.owningApplication?.processID else { throw Wire.failure("Accessibility permission required for remote menus") }
        let (items,actions)=try await Task.detached(priority:.utility) { () throws -> ([[String:Any]],[String:AXUIElement]) in
        let application=AXUIElementCreateApplication(pid)
        AXUIElementSetMessagingTimeout(application,0.2)
        func attribute(_ element:AXUIElement,_ name:String)->CFTypeRef?{var result:CFTypeRef?;guard AXUIElementCopyAttributeValue(element,name as CFString,&result) == .success else{return nil};return result}
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
        return (walk(value as! AXUIElement,0),actions)
        }.value
        menuActions[id]=actions;return items
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
        else if let window=windows[id]{bounds=contentFrames[id] ?? window.frame}
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
            if down {
                let now=ProcessInfo.processInfo.systemUptime
                let previous=lastClick
                let repeated=previous.map{$0.window==id && $0.button==button && now-$0.time<=NSEvent.doubleClickInterval && hypot($0.point.x-point.x,$0.point.y-point.y)<=4} ?? false
                let count=repeated ? min(3,(previous?.count ?? 0)+1):1
                lastClick=(id,button,point,now,count);clickCounts[button]=count
            }
            output?.setIntegerValueField(.mouseEventClickState,value:clickCounts[button] ?? 1)
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
    @MainActor private var resizing=false
    let id: CGWindowID; var frame: CGRect; var contentFrame:CGRect;var stream: SCStream?; var configuration: SCStreamConfiguration
    private let geometryLock=NSLock()
    var hardwareEncoder:NativeVideoEncoder?
    private var pixelRegion=CGRect.zero
    private var pixelSize=CGSize.zero
    private var captureNumber:UInt64=0
    let queue = DispatchQueue(label: "app.frames.capture", qos: .userInteractive)
    convenience init(window: SCWindow,content:CGRect,cursor:Bool=true) { self.init(id:window.windowID,frame:window.frame,content:content,filter:SCContentFilter(desktopIndependentWindow:window),cursor:cursor,children:false) }
    convenience init(display: SCDisplay,cursor:Bool=true) { let rect=CGDisplayBounds(display.displayID);self.init(id:desktopCaptureID,frame:rect,content:rect,filter:SCContentFilter(display:display,excludingWindows:[]),cursor:cursor,children:false) }
    init(id:CGWindowID,frame:CGRect,content:CGRect,filter:SCContentFilter,cursor:Bool,children:Bool) {
        self.id=id;self.frame=frame;self.contentFrame=content
        configuration = SCStreamConfiguration()
        configuration.width = Int((frame.width * CGFloat(filter.pointPixelScale)).rounded())
        configuration.height = Int((frame.height * CGFloat(filter.pointPixelScale)).rounded())
        configuration.pixelFormat = kCVPixelFormatType_32BGRA
        configuration.minimumFrameInterval = CMTime(value: 1, timescale: 60)
        configuration.queueDepth = 3; configuration.showsCursor = cursor; configuration.includeChildWindows = children
        if id != desktopCaptureID{configuration.ignoreShadowsSingleWindow=true;configuration.ignoreGlobalClipSingleWindow=true;filter.includeMenuBar=false}
        pixelSize=CGSize(width:configuration.width,height:configuration.height)
        pixelRegion=WindowContent.pixelRegion(frame:frame,content:content,scale:CGFloat(filter.pointPixelScale))
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
        hardwareEncoder?.stop()
        try? stream.removeStreamOutput(self,type:.screen)
        try? await stream.stopCapture()
    }
    @MainActor func scheduleResize(_ window:SCWindow,content:CGRect) {
        guard !resizing else{return}
        resizing=true
        Task { @MainActor in
            defer{resizing=false}
            do{try await resize(window,content:content)}
            catch{Wire.send(["event":"capture_error","handle":id,"error":error.localizedDescription])}
        }
    }
    @MainActor func resize(_ window: SCWindow,content:CGRect) async throws {
        let filter = SCContentFilter(desktopIndependentWindow: window)
        configuration.width = Int((window.frame.width * CGFloat(filter.pointPixelScale)).rounded()); configuration.height = Int((window.frame.height * CGFloat(filter.pointPixelScale)).rounded())
        try await stream?.updateConfiguration(configuration)
        geometryLock.withLock{pixelSize=CGSize(width:configuration.width,height:configuration.height);pixelRegion=WindowContent.pixelRegion(frame:window.frame,content:content,scale:CGFloat(filter.pointPixelScale))}
        frame = window.frame;contentFrame=content
    }
    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        autoreleasepool {
        guard type == .screen, sampleBuffer.isValid, let buffer = CMSampleBufferGetImageBuffer(sampleBuffer) else { return }
        guard let attachments=CMSampleBufferGetSampleAttachmentsArray(sampleBuffer,createIfNecessary:false) as? [[SCStreamFrameInfo:Any]],
              let rawStatus=attachments.first?[.status] as? Int,SCFrameStatus(rawValue:rawStatus) == .complete else{return}
        let fullWidth=CVPixelBufferGetWidth(buffer),fullHeight=CVPixelBufferGetHeight(buffer),stride=CVPixelBufferGetBytesPerRow(buffer)
        geometryLock.lock();let expected=pixelSize,region=pixelRegion;geometryLock.unlock()
        guard fullWidth==Int(expected.width),fullHeight==Int(expected.height) else{return}
        let left=Int(region.minX),top=Int(region.minY),width=Int(region.width),height=Int(region.height)
        guard left>=0,top>=0,width>0,height>0,width<=8192,height<=8192,left+width<=fullWidth,top+height<=fullHeight else{return}
        captureNumber &+= 1
        let time=CMSampleBufferGetPresentationTimeStamp(sampleBuffer)
        let nanos=time.isNumeric ? CMTimeConvertScale(time,timescale:1_000_000_000,method:.default).value:0
        if let hardwareEncoder {
            hardwareEncoder.submit(buffer,region:region,captured:nanos>0 ? UInt64(nanos):DispatchTime.now().uptimeNanoseconds);return
        }
        CVPixelBufferLockBaseAddress(buffer, .readOnly);defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        guard let base=CVPixelBufferGetBaseAddress(buffer) else { return }
        var data = Data(capacity: width*height*4)
        // ScreenCaptureKit ignores sourceRect for independent-window streams.
        // Remove only verified chrome from that window's buffer before encoding.
        for row in 0..<height { data.append(base.advanced(by:(top+row)*stride+left*4).assumingMemoryBound(to: UInt8.self), count: width*4) }
        Wire.sendFrame(handle:UInt64(id),width:UInt32(width),height:UInt32(height),bytes:data,number:captureNumber,captured:nanos>0 ? UInt64(nanos):DispatchTime.now().uptimeNanoseconds)
        }
    }
    func stream(_ stream: SCStream, didStopWithError error: Error) {
        fputs("Frames capture: \(error.localizedDescription)\n", stderr)
        Wire.send(["event":"capture_error","handle":id,"error":error.localizedDescription])
    }
}

let usageToMac: [UInt16: CGKeyCode] = [4:0,5:11,6:8,7:2,8:14,9:3,10:5,11:4,12:34,13:38,14:40,15:37,16:46,17:45,18:31,19:35,20:12,21:15,22:1,23:17,24:32,25:9,26:13,27:7,28:16,29:6,30:18,31:19,32:20,33:21,34:23,35:22,36:26,37:28,38:25,39:29,40:36,41:53,42:51,43:48,44:49,45:27,46:24,47:33,48:30,49:42,51:41,52:39,53:50,54:43,55:47,56:44,57:57,58:122,59:120,60:99,61:118,62:96,63:97,64:98,65:100,66:101,67:109,68:103,69:111,73:114,74:115,75:116,76:117,77:119,78:121,79:124,80:123,81:125,82:126,0xe0:59,0xe1:56,0xe2:58,0xe3:55,0xe4:62,0xe5:60,0xe6:61,0xe7:54]
