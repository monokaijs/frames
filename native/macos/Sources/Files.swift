import AppKit
import UniformTypeIdentifiers
import ApplicationServices

@MainActor final class FileShelf:NSObject,NSWindowDelegate{
    let window:NSWindow
    let content:NSView
    var count=0
    override init(){window=NSWindow(contentRect:NSRect(x:180,y:180,width:550,height:360),styleMask:[.titled,.closable,.resizable],backing:.buffered,defer:false);content=NSView(frame:NSRect(x:0,y:0,width:550,height:360));super.init();window.contentView=content;window.title="Frames received files — drag a file to another app";window.isReleasedWhenClosed=false;window.delegate=self;let cancel=NSButton(title:"Cancel transfers",target:self,action:#selector(cancelTransfers));cancel.frame=NSRect(x:12,y:8,width:160,height:28);content.addSubview(cancel)}
    @objc func cancelTransfers(){Wire.send(["event":"cancel_transfers"])}
    func progress(name:String,done:Double,total:Double){let percent=total>0 ? Int(done/total*100):0;window.title="Frames transfers — \(name) (\(percent)%)";window.orderFront(nil)}
    func add(_ url:URL){let row=PromisedFileRow(url:url,frame:NSRect(x:12,y:content.bounds.height-46-CGFloat(count)*40,width:520,height:36));row.autoresizingMask=[.minYMargin,.width];content.addSubview(row);count+=1;window.orderFront(nil)}
    func windowShouldClose(_ sender:NSWindow)->Bool{window.orderOut(nil);return false}
}
@MainActor final class PromisedFileRow:NSView,NSFilePromiseProviderDelegate,NSDraggingSource{
    let url:URL
    init(url:URL,frame:NSRect){self.url=url;super.init(frame:frame);let label=NSTextField(labelWithString:url.lastPathComponent);label.frame=NSRect(x:40,y:6,width:450,height:24);addSubview(label)}
    required init?(coder:NSCoder){fatalError("not used")}
    override func draw(_ dirtyRect:NSRect){NSWorkspace.shared.icon(forFile:url.path).draw(in:NSRect(x:0,y:2,width:32,height:32))}
    override func mouseDown(with event:NSEvent){}
    override func mouseDragged(with event:NSEvent){let provider=NSFilePromiseProvider(fileType:UTType.data.identifier,delegate:self);let item=NSDraggingItem(pasteboardWriter:provider);item.setDraggingFrame(NSRect(x:0,y:0,width:32,height:32),contents:NSWorkspace.shared.icon(forFile:url.path));beginDraggingSession(with:[item],event:event,source:self)}
    func filePromiseProvider(_ filePromiseProvider:NSFilePromiseProvider,fileNameForType fileType:String)->String{url.lastPathComponent}
    func filePromiseProvider(_ filePromiseProvider:NSFilePromiseProvider,writePromiseTo target:URL,completionHandler:@escaping(Error?)->Void){do{try FileManager.default.copyItem(at:url,to:target);completionHandler(nil)}catch{completionHandler(error)}}
    func draggingSession(_ session:NSDraggingSession,sourceOperationMaskFor context:NSDraggingContext)->NSDragOperation{.copy}
}

@MainActor final class NativeFileDropper:NSView,NSDraggingSource{
    let paths:[String];let point:CGPoint;var panel:NSWindow?
    init(paths:[String],point:CGPoint){self.paths=paths;self.point=point;super.init(frame:NSRect(x:0,y:0,width:3,height:3))}
    required init?(coder:NSCoder){fatalError("not used")}
    func start()throws{
        guard AXIsProcessTrusted(),!paths.isEmpty,paths.allSatisfy({FileManager.default.fileExists(atPath:$0)})else{throw Wire.failure("File drop requires Accessibility and materialized files")}
        let height=CGDisplayBounds(CGMainDisplayID()).height;let window=NSWindow(contentRect:NSRect(x:point.x-1,y:height-point.y-1,width:3,height:3),styleMask:[.borderless],backing:.buffered,defer:false);panel=window;window.level = .floating;window.contentView=self;window.orderFrontRegardless();NSApp.activate(ignoringOtherApps:true)
        CGEvent(mouseEventSource:nil,mouseType:.leftMouseDown,mouseCursorPosition:point,mouseButton:.left)?.post(tap:.cghidEventTap)
    }
    override func mouseDown(with event:NSEvent){
        let items=paths.map{path->NSDraggingItem in let item=NSDraggingItem(pasteboardWriter:URL(fileURLWithPath:path) as NSURL);item.setDraggingFrame(NSRect(x:0,y:0,width:32,height:32),contents:NSWorkspace.shared.icon(forFile:path));return item}
        beginDraggingSession(with:items,event:event,source:self)
        DispatchQueue.main.asyncAfter(deadline:.now()+0.1){[weak self] in guard let self else{return};self.panel?.orderOut(nil);CGEvent(mouseEventSource:nil,mouseType:.leftMouseDragged,mouseCursorPosition:self.point,mouseButton:.left)?.post(tap:.cghidEventTap);DispatchQueue.main.asyncAfter(deadline:.now()+0.15){CGEvent(mouseEventSource:nil,mouseType:.leftMouseUp,mouseCursorPosition:self.point,mouseButton:.left)?.post(tap:.cghidEventTap)}}
    }
    func draggingSession(_ session:NSDraggingSession,sourceOperationMaskFor context:NSDraggingContext)->NSDragOperation{.copy}
    func draggingSession(_ session:NSDraggingSession,endedAt screenPoint:NSPoint,operation:NSDragOperation){panel?.orderOut(nil);panel=nil}
}
