import AppKit

@main struct RemoteHeaderTests {
    @MainActor static func main() {
        _ = NSApplication.shared
        let output=Pipe()
        let original=dup(STDOUT_FILENO)
        dup2(output.fileHandleForWriting.fileDescriptor,STDOUT_FILENO)
        let remote:[String:Any]=["key":["id":42,"generation":1],"role":"window","bounds":["width":440.0,"height":320.0],"resize":"supported"]
        let proxy=RemoteProxy(remote:remote,host:"test")
        let style=proxy.window.styleMask,frame=proxy.window.frame
        precondition(!proxy.showHeader)
        proxy.toggleHeader()
        precondition(proxy.window.styleMask==style && proxy.window.frame==frame)
        precondition(!proxy.showHeader) // The host acknowledgement owns this state.
        let client=NativeClient(host:"test")
        client.windows[NativeClient.key(proxy.id)]=proxy
        client.activeProxy=proxy
        proxy.client=client
        var actual=remote
        actual["show_header"]=true
        client.update(["type":"ack","window":proxy.id,"actual":actual])
        precondition(proxy.showHeader)
        let item=NSApp.mainMenu?.items.first?.submenu?.items.first{$0.action == #selector(RemoteProxy.toggleHeader)}
        precondition(item?.state == .on)
        proxy.toggleHeader()
        precondition(proxy.window.styleMask==style && proxy.window.frame==frame)
        actual["role"]="desktop"
        proxy.remote=actual
        proxy.toggleHeader() // Desktop capture is always complete.
        dup2(original,STDOUT_FILENO)
        close(original)
        output.fileHandleForWriting.closeFile()
        let lines=String(data:output.fileHandleForReading.readDataToEndOfFile(),encoding:.utf8)!.split(separator:"\n")
        let events=lines.map{try! JSONSerialization.jsonObject(with:Data($0.utf8)) as! [String:Any]}
        precondition(events.count==4)
        for (index,visible) in [(1,true),(3,false)] {
            precondition(events[index-1]["event"] as? String == "release")
            precondition(events[index]["event"] as? String == "action")
            let action=events[index]["action"] as! [String:Any]
            precondition(action["kind"] as? String == "set_header" && action["visible"] as? Bool == visible)
            precondition((events[index]["window"] as! [String:Int])["id"]==42)
        }
        precondition(proxy.menuButton?.toolTip=="Connection menu")
        client.update(["type":"quality","preset":"low"])
        let connectionMenu=client.connectionMenu(for:proxy)
        let quality=connectionMenu.items.first{$0.title=="Image quality"}?.submenu
        precondition(quality?.items.count==3)
        precondition(quality?.items.first{($0.representedObject as? String)=="low"}?.state == .on)
        precondition(connectionMenu.items.contains{$0.title=="Ignore / hide this window"})
        precondition(connectionMenu.items.contains{$0.title=="Windows"})
        proxy.setShown(false)
        precondition(proxy.hidden && !proxy.window.isVisible)
        client.showAllWindows()
        precondition(!proxy.hidden && proxy.window.isVisible)
        proxy.destroy()
        print("PASS: header toggles request remote capture changes, follow host state, preserve local chrome, and reject desktop toggles")
    }
}
