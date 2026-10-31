import AppKit
import Foundation
import ApplicationServices
import ServiceManagement
import ScreenCaptureKit

enum Wire {
    static let lock = NSLock()
    static let mediaLock = NSLock()
    static let mediaOutput:FileHandle? = ProcessInfo.processInfo.environment["FRAMES_MEDIA_FD"].flatMap(Int32.init).map { FileHandle(fileDescriptor:$0,closeOnDealloc:false) }
    static let controlPermissionLabel = ProcessInfo.processInfo.operatingSystemVersion.majorVersion >= 27 ? "Device Control and Data Access (Accessibility)" : "Accessibility"
    static let diagnosticsEnabled = ProcessInfo.processInfo.environment["FRAMES_NATIVE_DIAGNOSTICS"] == "1"
    // Metadata only: no titles, pixels, clipboard contents, or input text.
    static func diagnostic(_ event: String, _ fields: [String: Any] = [:]) {
        guard diagnosticsEnabled else { return }
        var object = fields
        object["native_event"] = event
        guard let bytes = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]) else { return }
        lock.lock(); defer { lock.unlock() }
        FileHandle.standardError.write(bytes); FileHandle.standardError.write(Data([10]))
    }
    static func send(_ object: [String: Any]) {
        guard JSONSerialization.isValidJSONObject(object), let bytes = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]) else { return }
        lock.lock(); defer { lock.unlock() }
        FileHandle.standardOutput.write(bytes); FileHandle.standardOutput.write(Data([10]))
    }
    static func sendFrame(handle:UInt64,width:UInt32,height:UInt32,bytes:Data) {
        sendMedia(kind:1,handle:handle,width:width,height:height,bytes:bytes)
    }
    static func sendAudio(_ bytes:Data) {
        sendMedia(kind:2,handle:0,width:0,height:0,bytes:bytes)
    }
    private static func sendMedia(kind:UInt32,handle:UInt64,width:UInt32,height:UInt32,bytes:Data) {
        guard let mediaOutput else{fputs("Frames host requires a binary media socket\n",stderr);exit(1)}
        var header=Data("FRM1".utf8)
        func append<T:FixedWidthInteger>(_ value:T){var little=value.littleEndian;withUnsafeBytes(of:&little){header.append(contentsOf:$0)}}
        append(kind);append(handle);append(width);append(height);append(UInt64(bytes.count))
        mediaLock.lock();defer{mediaLock.unlock()}
        mediaOutput.write(header);mediaOutput.write(bytes)
    }
    static func permissions(screen: Bool? = nil) -> [String: Any] {
        ["screen_capture":screen ?? CGPreflightScreenCaptureAccess(),"accessibility":AXIsProcessTrusted(),"sharing_at_login":SMAppService.agent(plistName:"dev.creaton.frames.agent.plist").status == .enabled,"control_permission_name":controlPermissionLabel,"bundle_id":Bundle.main.bundleIdentifier ?? "unbundled","executable":CommandLine.arguments[0]]
    }
    static func failure(_ text: String) -> NSError { NSError(domain: "Frames", code: 1, userInfo: [NSLocalizedDescriptionKey: text]) }
}

