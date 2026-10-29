import AppKit
import Foundation
import ApplicationServices

enum Wire {
    static let lock = NSLock()
    static func send(_ object: [String: Any]) {
        guard JSONSerialization.isValidJSONObject(object), let bytes = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]) else { return }
        lock.lock(); defer { lock.unlock() }
        FileHandle.standardOutput.write(bytes); FileHandle.standardOutput.write(Data([10]))
    }
    static func failure(_ text: String) -> NSError { NSError(domain: "Frames", code: 1, userInfo: [NSLocalizedDescriptionKey: text]) }
}

@MainActor func runFrames(){
guard frames_abi_version() == 1 else { fatalError("Frames native ABI mismatch") }
let args = CommandLine.arguments
let app = NSApplication.shared
app.setActivationPolicy(.accessory)

if args.contains("--permissions"){
    if args.contains("--request"){
        app.setActivationPolicy(.regular);app.activate(ignoringOtherApps:true)
        if !CGPreflightScreenCaptureAccess(){_ = CGRequestScreenCaptureAccess()}
        _ = AXIsProcessTrustedWithOptions([kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String:true] as CFDictionary)
        RunLoop.current.run(until:Date(timeIntervalSinceNow:1))
    }
    Wire.send(["screen_capture":CGPreflightScreenCaptureAccess(),"accessibility":AXIsProcessTrusted(),"bundle_id":Bundle.main.bundleIdentifier ?? "unbundled","executable":CommandLine.arguments[0]])
}else if args.contains("--error"){
    app.setActivationPolicy(.regular);app.activate(ignoringOtherApps:true);let alert=NSAlert();alert.messageText=args.count>2 ? args[2]:"Frames";alert.informativeText=args.count>3 ? args[3]:"Unknown error";alert.alertStyle = .critical;alert.runModal()
}else if args.contains("--approve"){
    app.setActivationPolicy(.regular);app.activate(ignoringOtherApps:true)
    let alert=NSAlert();alert.messageText=args.count>4 ? args[4]:"Approve pairing"
    let name=args.count>2 ? args[2].components(separatedBy:.controlCharacters).joined():"Remote device"
    alert.informativeText="Verification code: \(args.count>3 ? args[3]:"")\n\nDevice: \(name)\n\nApprove only if the other device shows the same code."
    alert.addButton(withTitle:"Reject");alert.addButton(withTitle:"Approve");exit(alert.runModal() == .alertSecondButtonReturn ? 0:1)
} else if args.contains("--launcher"){
    app.setActivationPolicy(.regular);let launcher=FramesLauncher();app.delegate=launcher;withExtendedLifetime(launcher){app.run()}
} else if args.contains("--host") {
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
} else if args.contains("--client") {
    let client = NativeClient(host: args.last ?? "remote")
    app.delegate = client
    DispatchQueue.global(qos: .userInteractive).async {
        while let line = readLine() {
            guard let bytes = line.data(using: .utf8), let message = try? JSONSerialization.jsonObject(with: bytes) as? [String: Any] else { continue }
            DispatchQueue.main.async { client.update(message) }
        }
        DispatchQueue.main.async { app.terminate(nil) }
    }
    withExtendedLifetime(client){app.run()}
} else if args.contains("--dock") {
    let dock = DockProxy(name: args.count > 2 ? args[2] : "Remote app", icon: args.count > 3 ? args[3] : "")
    app.setActivationPolicy(.regular); app.delegate = dock
    withExtendedLifetime(dock){app.run()}
} else {
    fputs("FramesHelper is launched by the Frames executable.\n", stderr)
}
}
MainActor.assumeIsolated{runFrames()}
