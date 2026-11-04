import AppKit
import Foundation
import ApplicationServices
import ServiceManagement
import ScreenCaptureKit


@MainActor func runFrames(){
guard frames_abi_version() == FRAMES_ABI_VERSION else { fatalError("Frames native ABI mismatch") }
let args = CommandLine.arguments
// The mode is always the first argument. Later arguments carry data a remote
// peer can choose (device names, app names, error text), so they must never
// select a mode: `--approve "--permissions" ...` once skipped the approval
// dialog entirely and exited 0.
let mode = args.count > 1 ? args[1] : ""
let app = NSApplication.shared
app.setActivationPolicy(.accessory)

if mode == "--network-settings"{
    Task { @MainActor in
        Wire.send(["opened":await SettingsNavigation.openLocalNetwork()])
        app.terminate(nil)
    }
    app.run()
}else if mode == "--pause-sharing-for-update"{
    let service=SMAppService.agent(plistName:"dev.creaton.frames.agent.plist")
    do{
        if service.status == .enabled{try service.unregister();print("resume")}else{print("off")}
    }catch{FileHandle.standardError.write(Data(error.localizedDescription.utf8));exit(1)}
}else if mode == "--resume-sharing-after-update"{
    do{try SMAppService.agent(plistName:"dev.creaton.frames.agent.plist").register()}
    catch{FileHandle.standardError.write(Data(error.localizedDescription.utf8));exit(1)}
}else if mode == "--login-item"{
    app.setActivationPolicy(.regular);app.activate(ignoringOtherApps:true)
    let service=SMAppService.agent(plistName:"dev.creaton.frames.agent.plist")
    let enabled=service.status == .enabled || service.status == .requiresApproval
    let alert=NSAlert();alert.messageText="Share this Mac at login"
    alert.informativeText="Background sharing is \(enabled ? "registered" : "off"). Only devices already approved for unattended control can connect. Manage background sharing in System Settings → General → Login Items."
    alert.addButton(withTitle:enabled ? "Disable sharing at login" : "Enable sharing at login")
    alert.addButton(withTitle:"Cancel");alert.addButton(withTitle:"Open System Settings")
    let response=runFramesAlert(alert)
    do{
        if response == .alertFirstButtonReturn{if enabled{try service.unregister()}else{try service.register()}}
        else if response == .alertThirdButtonReturn{SMAppService.openSystemSettingsLoginItems()}
    }catch{
        let failure=NSAlert();failure.messageText="Couldn't update the login item";failure.informativeText=error.localizedDescription;_ = runFramesAlert(failure);exit(1)
    }
}else if mode == "--permissions"{
    let wantsScreen=args.contains("--request") || args.contains("--request-screen")
    let wantsControl=args.contains("--request") || args.contains("--request-control")
    let needsScreen=wantsScreen && !CGPreflightScreenCaptureAccess()
    let needsControl=wantsControl && !AXIsProcessTrusted()
    if needsScreen || needsControl{
        app.setActivationPolicy(.regular);app.activate(ignoringOtherApps:true)
    }
    if needsControl{
        _ = AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String:true] as CFDictionary)
    }
    if needsScreen{
        // Only an explicit setup action may use ScreenCaptureKit's authorization
        // path. Passive checks must never enumerate content or show a prompt.
        Task { @MainActor in
            let authorized:Bool
            do{
                _ = try await SCShareableContent.excludingDesktopWindows(true,onScreenWindowsOnly:true)
                authorized=true
            }catch{
                authorized=false
            }
            Wire.send(Wire.permissions(screen:authorized))
            app.terminate(nil)
        }
        app.run()
    }else{
        if needsControl{RunLoop.current.run(until:Date(timeIntervalSinceNow:1))}
        Wire.send(Wire.permissions())
    }
}else if mode == "--error"{
    app.setActivationPolicy(.regular);let alert=NSAlert();alert.messageText=args.count>2 ? args[2]:"Frames";alert.informativeText=args.count>3 ? args[3]:"Unknown error";alert.alertStyle = .critical;_ = runFramesAlert(alert)
}else if mode == "--approve"{
    app.setActivationPolicy(.regular);app.activate(ignoringOtherApps:true)
    let alert=NSAlert();alert.messageText=args.count>4 ? args[4]:"Approve pairing"
    let name=args.count>2 ? args[2].components(separatedBy:.controlCharacters).joined():"Remote device"
    alert.informativeText="Verification code: \(args.count>3 ? args[3]:"")\n\nDevice: \(name)\n\nApprove only if the other device shows the same code."
    alert.informativeText += "\n\nAccept allows this session only. Accept and trust also saves the device, and it then connects without asking while you share."
    alert.addButton(withTitle:"Decline connection");alert.addButton(withTitle:"Accept and trust");alert.addButton(withTitle:"Accept")
    // Dedicated codes: a normal exit 0 (any other mode, or no answer) must never read as consent.
    let decision=runFramesAlert(alert);exit(decision == .alertSecondButtonReturn ? 10 : decision == .alertThirdButtonReturn ? 11 : 1)
} else if mode == "--launcher"{
    app.setActivationPolicy(.regular);let launcher=FramesLauncher();app.delegate=launcher;withExtendedLifetime(launcher){app.run()}
} else if mode == "--host" {
    let host = NativeHost()
    DispatchQueue.global(qos: .userInteractive).async {
        while let line = readLine() {
            guard let bytes = line.data(using: .utf8), let message = try? JSONSerialization.jsonObject(with: bytes) as? [String: Any] else { continue }
            let id = message["id"] as? NSNumber ?? 0
            Task { @MainActor in
                do {
                    guard (message["version"] as? Int) == 1 else { throw Wire.failure("Incompatible native bridge version") }
                    let result = try await host.request(message["command"] as? String ?? "", message["data"] as? [String: Any] ?? [:])
                    Wire.send(["id": id, "data": result])
                } catch { Wire.send(["id": id, "error": error.localizedDescription]) }
            }
        }
        DispatchQueue.main.async { app.terminate(nil) }
    }
    withExtendedLifetime(host){app.run()}
} else if mode == "--client" {
    // Accessory apps do not own the system menu bar. The client must activate
    // normally so focused remote application menus are visible and actionable.
    app.setActivationPolicy(.regular)
    let client = NativeClient(host: args.last ?? "remote")
    app.delegate = client
    guard let fd = ProcessInfo.processInfo.environment["FRAMES_CLIENT_MEDIA_FD"].flatMap(Int32.init) else {
        fputs("Frames client requires a binary media socket\n", stderr)
        exit(1)
    }
    let decoders = NativeVideoDecoders()
    let inbox = ClientVideoInbox(encoded:{ frame,inbox in decoders.decode(frame,into:inbox) }) { pending in
        DispatchQueue.main.async { client.receiveVideo(pending.take()) }
    }
    DispatchQueue.global(qos: .userInteractive).async { inbox.read(fd) }
    let controls=ClientControlInbox { pending in
        DispatchQueue.main.async { for message in pending.take(){client.update(message)} }
    }
    DispatchQueue.global(qos: .userInteractive).async {
        while let line = readLine() {
            guard let bytes = line.data(using: .utf8), let message = try? JSONSerialization.jsonObject(with: bytes) as? [String: Any] else { continue }
            if !controls.push(message){break}
        }
        Wire.diagnostic("client_stdin_closed")
        Wire.markParentGone()
        DispatchQueue.main.async { app.terminate(nil) }
        // The Frames process is gone. A modal alert running on the main queue
        // would keep the terminate request above from ever running, leaving
        // this process and its remote windows orphaned: exit regardless.
        Thread.sleep(forTimeInterval: 2)
        Wire.diagnostic("client_forced_exit")
        exit(0)
    }
    withExtendedLifetime(client){app.run()}
} else if mode == "--dock" {
    let dock = DockProxy(name: args.count > 2 ? args[2] : "Remote app", icon: args.count > 3 ? args[3] : "")
    app.setActivationPolicy(.regular); app.delegate = dock
    // stdin is a pipe held by the client helper: EOF means the helper is gone
    // (quit, crash, or killed), so this Dock icon must go too.
    DispatchQueue.global(qos: .utility).async {
        while readLine() != nil {}
        exit(0)
    }
    withExtendedLifetime(dock){app.run()}
} else {
    fputs("FramesHelper is launched by the Frames executable.\n", stderr)
}
}
MainActor.assumeIsolated{runFrames()}
