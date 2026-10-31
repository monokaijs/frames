import AppKit

@MainActor final class FixtureWindow: NSWindow {
    override func constrainFrameRect(_ frameRect: NSRect, to screen: NSScreen?) -> NSRect {
        // Exercise a complete 1080p content surface even when the menu bar and
        // Dock reduce the physical display's available work area.
        if frameRect.width >= 1920 && frameRect.height >= 1080 { return frameRect }
        return super.constrainFrameRect(frameRect, to: screen)
    }
}

@MainActor final class Fixture: NSObject, NSApplicationDelegate, NSWindowDelegate, NSTextFieldDelegate {
    var source: NSWindow!
    var cover: NSWindow!
    var visibilityFixtures:[NSWindow]=[]
    var clicks=0
    let input = NSTextField(frame:NSRect(x:20,y:20,width:340,height:32))
    func applicationDidFinishLaunching(_ notification:Notification) {
        let large=ProcessInfo.processInfo.environment["FRAMES_QA_1080P"] == "1"
        source=FixtureWindow(contentRect:NSRect(x:large ? 0:120,y:large ? 0:160,width:large ? 1920:440,height:large ? 1080:320),styleMask:[.titled,.closable,.resizable,.miniaturizable],backing:.buffered,defer:false)
        source.title="Frames QA source";source.backgroundColor=NSColor(srgbRed:0.85,green:0.05,blue:0.05,alpha:1);source.delegate=self;source.isReleasedWhenClosed=false
        input.delegate=self;source.contentView?.addSubview(input)
        let click=NSButton(frame:NSRect(x:20,y:large ? 200:80,width:160,height:32));click.title="Test remote click";click.target=self;click.action=#selector(clicked);source.contentView?.addSubview(click)
        let bar=NSMenu();bar.autoenablesItems=false
        let appItem=NSMenuItem(title:"QA",action:nil,keyEquivalent:"");let appMenu=NSMenu();appMenu.addItem(withTitle:"Quit fixture",action:#selector(quit),keyEquivalent:"");appItem.submenu=appMenu;bar.addItem(appItem)
        let file=NSMenuItem(title:"File",action:nil,keyEquivalent:"");let menu=NSMenu();menu.autoenablesItems=false
        let mark=NSMenuItem(title:"Mark QA",action:#selector(mark),keyEquivalent:"");mark.target=self;menu.addItem(mark)
        let disabled=NSMenuItem(title:"Unavailable QA",action:nil,keyEquivalent:"");disabled.isEnabled=false;menu.addItem(disabled);file.submenu=menu;bar.addItem(file);NSApp.mainMenu=bar
        source.makeKeyAndOrderFront(nil);source.makeFirstResponder(input);NSApp.activate(ignoringOtherApps:true)
        if let path=ProcessInfo.processInfo.environment["FRAMES_QA_PULSE_LOG"] {
            _ = FileManager.default.createFile(atPath:path,contents:nil)
            let output=FileHandle(forWritingAtPath:path)
            Timer.scheduledTimer(withTimeInterval:0.5,repeats:true){[weak self] _ in
                MainActor.assumeIsolated {
                    guard let self else{return}
                    let time=Date().timeIntervalSince1970
                    self.clicks+=1
                    let green = !self.clicks.isMultiple(of:2)
                    self.source.backgroundColor=green ? NSColor(srgbRed:0.05,green:0.85,blue:0.05,alpha:1):NSColor(srgbRed:0.85,green:0.05,blue:0.05,alpha:1)
                    if let data=try? JSONSerialization.data(withJSONObject:["unix_us":Int64(time*1_000_000),"green":green,"sequence":self.clicks]) {
                        output?.write(data);output?.write(Data([10]))
                    }
                }
            }
        }
        cover=NSWindow(contentRect:source.contentRect(forFrameRect:source.frame),styleMask:[.titled],backing:.buffered,defer:false)
        cover.title="Frames QA cover";cover.backgroundColor=NSColor(srgbRed:0.05,green:0.05,blue:0.85,alpha:1);cover.level = .floating
        if ProcessInfo.processInfo.environment["FRAMES_QA_NO_COVER"] != "1" {cover.orderFront(nil)}
        if ProcessInfo.processInfo.environment["FRAMES_QA_VISIBILITY_CHECK"] == "1" {
            for title in ["Frames QA hidden helper","Frames QA minimized","Frames QA transparent helper"] {
                let window=NSWindow(contentRect:NSRect(x:650,y:160,width:240,height:180),styleMask:[.titled,.miniaturizable],backing:.buffered,defer:false)
                window.title=title;window.isReleasedWhenClosed=false
                if title.contains("transparent") {window.alphaValue=0;window.orderFront(nil)}
                else if title.contains("minimized") {window.orderFront(nil);window.miniaturize(nil)}
                else {window.orderOut(nil)}
                visibilityFixtures.append(window)
            }
        }
        Timer.scheduledTimer(withTimeInterval:300,repeats:false){_ in NSApp.terminate(nil)}
    }
    @objc func mark(){source.title="Frames QA source marked";cover.orderOut(nil)}
    @objc func clicked(){clicks+=1;source.backgroundColor=clicks.isMultiple(of:2) ? NSColor(srgbRed:0.85,green:0.05,blue:0.05,alpha:1):NSColor(srgbRed:0.05,green:0.85,blue:0.05,alpha:1);source.title="Frames QA source clicked";source.makeFirstResponder(input)}
    @objc func quit(){NSApp.terminate(nil)}
    func controlTextDidChange(_ notification:Notification){source.title="Frames QA source text: "+input.stringValue}
    func windowWillClose(_ notification:Notification){NSApp.terminate(nil)}
}
MainActor.assumeIsolated {
    let app=NSApplication.shared
    app.setActivationPolicy(.regular)
    let fixture=Fixture();app.delegate=fixture
    withExtendedLifetime(fixture){app.run()}
}
