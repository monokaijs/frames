import AppKit

@MainActor final class Fixture: NSObject, NSApplicationDelegate, NSWindowDelegate, NSTextFieldDelegate {
    var source: NSWindow!
    var cover: NSWindow!
    let input = NSTextField(frame:NSRect(x:20,y:20,width:340,height:32))
    func applicationDidFinishLaunching(_ notification:Notification) {
        source=NSWindow(contentRect:NSRect(x:120,y:160,width:440,height:320),styleMask:[.titled,.closable,.resizable,.miniaturizable],backing:.buffered,defer:false)
        source.title="Frames QA source";source.backgroundColor=NSColor(srgbRed:0.85,green:0.05,blue:0.05,alpha:1);source.delegate=self;source.isReleasedWhenClosed=false
        input.delegate=self;source.contentView?.addSubview(input)
        let bar=NSMenu();bar.autoenablesItems=false
        let appItem=NSMenuItem(title:"QA",action:nil,keyEquivalent:"");let appMenu=NSMenu();appMenu.addItem(withTitle:"Quit fixture",action:#selector(quit),keyEquivalent:"");appItem.submenu=appMenu;bar.addItem(appItem)
        let file=NSMenuItem(title:"File",action:nil,keyEquivalent:"");let menu=NSMenu();menu.autoenablesItems=false
        let mark=NSMenuItem(title:"Mark QA",action:#selector(mark),keyEquivalent:"");mark.target=self;menu.addItem(mark)
        let disabled=NSMenuItem(title:"Unavailable QA",action:nil,keyEquivalent:"");disabled.isEnabled=false;menu.addItem(disabled);file.submenu=menu;bar.addItem(file);NSApp.mainMenu=bar
        source.makeKeyAndOrderFront(nil);source.makeFirstResponder(input);NSApp.activate(ignoringOtherApps:true)
        cover=NSWindow(contentRect:source.contentRect(forFrameRect:source.frame),styleMask:[.titled],backing:.buffered,defer:false)
        cover.title="Frames QA cover";cover.backgroundColor=NSColor(srgbRed:0.05,green:0.05,blue:0.85,alpha:1);cover.level = .floating;cover.orderFront(nil)
        Timer.scheduledTimer(withTimeInterval:300,repeats:false){_ in NSApp.terminate(nil)}
    }
    @objc func mark(){source.title="Frames QA source marked"}
    @objc func quit(){NSApp.terminate(nil)}
    func controlTextDidChange(_ notification:Notification){source.title="Frames QA source text: "+input.stringValue}
    func windowWillClose(_ notification:Notification){NSApp.terminate(nil)}
}
let app=NSApplication.shared
app.setActivationPolicy(.regular)
let fixture=Fixture();app.delegate=fixture
withExtendedLifetime(fixture){app.run()}
