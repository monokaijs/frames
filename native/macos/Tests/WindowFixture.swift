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
    var panels:[NSWindow]=[]
    var popover:NSPopover?
    var clicks=0
    let input = NSTextField(frame:NSRect(x:20,y:20,width:340,height:32))
    func applicationDidFinishLaunching(_ notification:Notification) {
        let large=ProcessInfo.processInfo.environment["FRAMES_QA_1080P"] == "1"
        source=FixtureWindow(contentRect:NSRect(x:large ? 0:120,y:large ? 0:160,width:large ? 1920:440,height:large ? 1080:320),styleMask:[.titled,.closable,.resizable,.miniaturizable],backing:.buffered,defer:false)
        source.title="Frames QA source";source.backgroundColor=NSColor(srgbRed:0.85,green:0.05,blue:0.05,alpha:1);source.delegate=self;source.isReleasedWhenClosed=false
        input.delegate=self;source.contentView?.addSubview(input)
        let click=NSButton(frame:NSRect(x:20,y:large ? 200:80,width:160,height:32));click.title="Test remote click";click.target=self;click.action=#selector(clicked);source.contentView?.addSubview(click)
        if ProcessInfo.processInfo.environment["FRAMES_QA_SURFACES"] == "1" {
            let actions:[(String,Selector)]=[("Show sheet",#selector(showSheet)),("Show dialog",#selector(showDialog)),("Open file",#selector(openFile)),("Popover",#selector(showPopover(_:))),("App modal alert",#selector(showAppModal))]
            for (index,action) in actions.enumerated() {
                let button=NSButton(frame:NSRect(x:20+(index%2)*180,y:130+(index/2)*42,width:170,height:32))
                button.title=action.0;button.target=self;button.action=action.1;source.contentView?.addSubview(button)
            }
            input.toolTip="Frames QA tooltip"
            let context=NSMenu();context.autoenablesItems=false
            let select=NSMenuItem(title:"Mark context selection",action:#selector(mark),keyEquivalent:"");select.target=self;context.addItem(select)
            let checked=NSMenuItem(title:"Checked item",action:#selector(mark),keyEquivalent:"");checked.target=self;checked.state = .on;context.addItem(checked)
            let disabled=NSMenuItem(title:"Disabled item",action:nil,keyEquivalent:"");disabled.isEnabled=false;context.addItem(disabled)
            context.addItem(.separator())
            let nested=NSMenuItem(title:"Nested submenu",action:nil,keyEquivalent:"");let submenu=NSMenu()
            let choose=NSMenuItem(title:"Choose nested item",action:#selector(mark),keyEquivalent:"");choose.target=self;submenu.addItem(choose)
            nested.submenu=submenu;context.addItem(nested);source.contentView?.menu=context
            let second=NSWindow(contentRect:NSRect(x:650,y:160,width:360,height:260),styleMask:[.titled,.closable,.resizable],backing:.buffered,defer:false)
            second.title="Frames QA second window";second.isReleasedWhenClosed=false;second.contentView?.menu=context.copy() as? NSMenu;second.orderFront(nil);panels.append(second)
        }
        if let path=ProcessInfo.processInfo.environment["FRAMES_QA_TEXT_SOURCE"],let image=NSImage(contentsOfFile:path),let representation=image.representations.first {
            let scale=NSScreen.main?.backingScaleFactor ?? 1
            let size=NSSize(width:CGFloat(representation.pixelsWide)/scale,height:CGFloat(representation.pixelsHigh)/scale)
            source.setContentSize(size)
            let view=NSImageView(frame:NSRect(origin:.zero,size:size));view.image=image;view.imageScaling = .scaleAxesIndependently
            source.contentView=view
            let marker=NSView(frame:NSRect(x:2,y:2,width:8,height:8));marker.wantsLayer=true;view.addSubview(marker)
            var bright=false
            Timer.scheduledTimer(withTimeInterval:0.1,repeats:true){_ in MainActor.assumeIsolated {bright.toggle();marker.layer?.backgroundColor=(bright ? NSColor.white:NSColor.black).cgColor}}
        }
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
    @objc func showSheet(){let alert=NSAlert();alert.messageText="Frames QA sheet";alert.informativeText="The source window must be blocked; the second window stays usable.";alert.addButton(withTitle:"Done");alert.addButton(withTitle:"Cancel");alert.beginSheetModal(for:source)}
    @objc func showAppModal(){let alert=NSAlert();alert.messageText="Frames QA app-modal alert";alert.informativeText="Both source windows must be blocked until this alert closes.";alert.addButton(withTitle:"Done");_ = alert.runModal()}
    @objc func showDialog(){
        let dialog=NSPanel(contentRect:NSRect(x:250,y:250,width:300,height:160),styleMask:[.titled,.closable],backing:.buffered,defer:false)
        dialog.title="";dialog.isReleasedWhenClosed=false
        dialog.contentView?.addSubview(NSTextField(labelWithString:"Frames QA untitled modeless dialog"))
        source.addChildWindow(dialog,ordered:.above);dialog.makeKeyAndOrderFront(nil);panels.append(dialog)
    }
    @objc func openFile(){let panel=NSOpenPanel();panel.beginSheetModal(for:source){_ in}}
    @objc func showPopover(_ button:NSButton){let popup=NSPopover();let controller=NSViewController();controller.view=NSView(frame:NSRect(x:0,y:0,width:220,height:100));controller.view.addSubview(NSTextField(labelWithString:"Frames QA popover"));popup.contentViewController=controller;popup.behavior = .transient;popover=popup;popup.show(relativeTo:button.bounds,of:button,preferredEdge:.maxY)}
    func controlTextDidChange(_ notification:Notification){source.title="Frames QA source text: "+input.stringValue}
    func windowWillClose(_ notification:Notification){NSApp.terminate(nil)}
}
MainActor.assumeIsolated {
    let app=NSApplication.shared
    app.setActivationPolicy(.regular)
    let fixture=Fixture();app.delegate=fixture
    withExtendedLifetime(fixture){app.run()}
}
