import AppKit
import ServiceManagement
import ApplicationServices

@MainActor final class FramesLauncher:NSObject,NSApplicationDelegate{
    let window=NSWindow(contentRect:NSRect(x:200,y:200,width:580,height:340),styleMask:[.titled,.closable,.miniaturizable],backing:.buffered,defer:false)
    let address=NSTextField(string:"127.0.0.1:47990")
    let audio=NSButton(checkboxWithTitle:"Play host audio",target:nil,action:nil)
    let desktop=NSButton(checkboxWithTitle:"Open full desktop view",target:nil,action:nil)
    let unattended=NSButton(checkboxWithTitle:"Allow trusted devices to reconnect unattended",target:nil,action:nil)
    let status=NSTextField(labelWithString:"Pair once on both devices.")
    var host:Process?
    var clients:[Process]=[]
    var discovery:Process?
    func applicationDidFinishLaunching(_ notification:Notification){
        window.title="Frames";window.isReleasedWhenClosed=false
        guard let content=window.contentView else{return}
        let header=NSTextField(labelWithString:"Your remote apps, in windows of their own");header.font = .systemFont(ofSize:20,weight:.semibold);header.frame=NSRect(x:24,y:280,width:530,height:30);content.addSubview(header)
        address.frame=NSRect(x:24,y:225,width:380,height:28);address.placeholderString="Host address (LAN / VPN)";content.addSubview(address)
        button("Connect",#selector(connect),NSRect(x:420,y:223,width:135,height:32),content)
        audio.frame=NSRect(x:24,y:185,width:400,height:26);content.addSubview(audio)
        desktop.frame=NSRect(x:270,y:185,width:285,height:26);content.addSubview(desktop)
        unattended.frame=NSRect(x:24,y:140,width:525,height:26);content.addSubview(unattended)
        button("Share this Mac",#selector(share),NSRect(x:24,y:88,width:160,height:32),content)
        button("Nearby devices",#selector(discover),NSRect(x:194,y:88,width:170,height:32),content)
        button("Stop sessions",#selector(stop),NSRect(x:374,y:88,width:180,height:32),content)
        button("Start sharing at login",#selector(loginItem),NSRect(x:24,y:44,width:210,height:28),content)
        status.frame=NSRect(x:24,y:12,width:535,height:26);content.addSubview(status)
        window.center();window.makeKeyAndOrderFront(nil);NSApp.activate(ignoringOtherApps:true)
    }
    func button(_ title:String,_ action:Selector,_ rect:NSRect,_ parent:NSView){let button=NSButton(title:title,target:self,action:action);button.frame=rect;button.bezelStyle = .rounded;parent.addSubview(button)}
    func process(_ arguments:[String],output:Any?=nil)throws->Process{let process=Process();let resources=URL(fileURLWithPath:CommandLine.arguments[0]).deletingLastPathComponent();process.executableURL=resources.appendingPathComponent("../MacOS/frames").standardizedFileURL;process.arguments=arguments;var environment=ProcessInfo.processInfo.environment;environment["FRAMES_GUI_CHILD"]="1";process.environment=environment;process.standardOutput=output ?? FileHandle.nullDevice;process.standardError=FileHandle.nullDevice;try process.run();return process}
    @objc func connect(){do{var arguments=["connect",address.stringValue];if audio.state == .on{arguments.append("--audio")};if desktop.state == .on{arguments.append(contentsOf:["--view","desktop"])};clients.append(try process(arguments));status.stringValue="Connecting. Compare the pairing code on both devices."}catch{status.stringValue=error.localizedDescription}}
    @objc func share(){
        if host?.isRunning == true{return};host=nil
        guard CGPreflightScreenCaptureAccess() || CGRequestScreenCaptureAccess() else{status.stringValue="Grant Frames Screen Recording in Privacy & Security, then reopen Frames.";return}
        let trusted=AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String:true] as CFDictionary)
        do{var arguments=["host","--pair"];if unattended.state == .on{arguments.append("--unattended")};host=try process(arguments);status.stringValue=trusted ? "Sharing this Mac on port 47990.":"Sharing. Grant \(Wire.controlPermissionLabel) for input and resizing."}catch{status.stringValue=error.localizedDescription}
    }
    @objc func stop(){host?.terminate();host=nil;discovery?.terminate();discovery=nil;for process in clients where process.isRunning{process.terminate()};clients.removeAll();status.stringValue="All sessions stopped."}
    @objc func discover(){
        guard discovery==nil else{return}
        do{
            let pipe=Pipe();let process=try self.process(["discover","--seconds","3"],output:pipe);discovery=process;status.stringValue="Looking for nearby devices..."
            DispatchQueue.global(qos:.userInitiated).async{[weak self] in
                let bytes=pipe.fileHandleForReading.readDataToEndOfFile();process.waitUntilExit()
                DispatchQueue.main.async{guard let self else{return};self.discovery=nil;self.showNearby(bytes)}
            }
        }catch{status.stringValue=error.localizedDescription}
    }
    func showNearby(_ bytes:Data){
        let devices=(try? JSONSerialization.jsonObject(with:bytes)) as? [[String:Any]] ?? []
        var endpoints:[String]=[];var labels:[String]=[]
        for device in devices{
            guard let addresses=device["addresses"] as? [String],let port=device["port"] as? Int else{continue}
            for ip in addresses.prefix(4){let endpoint=ip.contains(":") ? "[\(ip)]:\(port)":"\(ip):\(port)";endpoints.append(endpoint);labels.append("\(device["name"] as? String ?? "Frames") — \(endpoint)")}
        }
        guard !endpoints.isEmpty else{status.stringValue="No nearby hosts found. Enter a LAN or VPN address manually.";return}
        let alert=NSAlert();alert.messageText="Nearby Frames devices";alert.informativeText="Select a host. Verify its pairing code when connecting.";alert.addButton(withTitle:"Connect");alert.addButton(withTitle:"Cancel")
        let choices=NSPopUpButton(frame:NSRect(x:0,y:0,width:440,height:28));choices.addItems(withTitles:labels);alert.accessoryView=choices
        if alert.runModal() == .alertFirstButtonReturn,choices.indexOfSelectedItem>=0{address.stringValue=endpoints[choices.indexOfSelectedItem];connect()}
        else{status.stringValue="Found \(devices.count) nearby hosts."}
    }
    @objc func loginItem(){do{try SMAppService.agent(plistName:"app.frames.agent.plist").register();status.stringValue="Sharing registered for login. Approve it in Login Items if requested."}catch{status.stringValue=error.localizedDescription}}
    func applicationWillTerminate(_ notification:Notification){stop()}
    func applicationShouldTerminateAfterLastWindowClosed(_ sender:NSApplication)->Bool{true}
}
