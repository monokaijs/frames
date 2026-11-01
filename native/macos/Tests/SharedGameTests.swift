import AppKit

@main struct SharedGameTests {
    @MainActor static func main() {
        _ = NSApplication.shared
        let output=Pipe(),original=dup(STDOUT_FILENO)
        dup2(output.fileHandleForWriting.fileDescriptor,STDOUT_FILENO)
        let remote:[String:Any]=["key":["id":42,"generation":1],"title":"Generated game","role":"window","bounds":["width":440.0,"height":320.0],"scale":1.0,"resize":"supported"]
        let proxy=RemoteProxy(remote:remote,host:"shared-fixture")
        let client=NativeClient(host:"shared-fixture")
        proxy.client=client;client.windows[NativeClient.key(proxy.id)]=proxy;client.activeProxy=proxy
        client.update(["type":"shared_session","data":["phase":"active","detail":"Untested"],"compatibility_text":"Fixture evidence only"])
        precondition(proxy.shared && proxy.physical && !proxy.swap && !proxy.resizable)
        precondition(proxy.mode == "stretch_local" && proxy.window.title.contains("Untested"))
        proxy.setResize()
        proxy.togglePhysical();proxy.toggleSwap();proxy.toggleHeader();proxy.remoteMenu();proxy.fetchFile()
        precondition(proxy.mode == "stretch_local")
        precondition(proxy.physical && !proxy.swap)
        proxy.view.forwarded.insert(0x1a)
        client.update(["type":"shared_session","data":["phase":"paused","detail":"Input paused"],"compatibility_text":"Untested"])
        precondition(proxy.view.forwarded.isEmpty && !proxy.view.relative)
        let image=NSBitmapImageRep(bitmapDataPlanes:nil,pixelsWide:8,pixelsHigh:8,bitsPerSample:8,samplesPerPixel:4,hasAlpha:true,isPlanar:false,colorSpaceName:.deviceRGB,bytesPerRow:0,bitsPerPixel:0)!
        let png=image.representation(using:.png,properties:[:])!.base64EncodedString()
        client.update(["type":"shared_cursor","data":["x":0.5,"y":0.5,"visible":true,"hotspot_x":1,"hotspot_y":1,"png_base64":png]])
        precondition(proxy.view.cursorOverlay.image != nil && !proxy.view.cursorOverlay.isHidden)
        client.update(["type":"shared_cursor","data":["x":0.25,"y":0.25,"visible":false,"hotspot_x":1,"hotspot_y":1]])
        precondition(proxy.view.cursorOverlay.image != nil && proxy.view.cursorOverlay.isHidden)
        proxy.windowDidResignKey(Notification(name:NSWindow.didResignKeyNotification,object:proxy.window))
        proxy.destroy()
        dup2(original,STDOUT_FILENO);close(original);output.fileHandleForWriting.closeFile()
        let lines=String(data:output.fileHandleForReading.readDataToEndOfFile(),encoding:.utf8)!.split(separator:"\n")
        let events=lines.map{try! JSONSerialization.jsonObject(with:Data($0.utf8)) as! [String:Any]}
        precondition(events.contains{$0["event"] as? String == "release"})
        precondition(!events.contains{$0["event"] as? String == "action"})
        print("PASS: shared policy forces physical keys and local sizing, pauses held input, and preserves bounded cursor images")
    }
}
