import AppKit
import Foundation
import ApplicationServices
import ServiceManagement
import ScreenCaptureKit

enum Wire {
    static let lock = NSLock()
    static let mediaLock = NSLock()
    static let mediaOutput:FileHandle? = ProcessInfo.processInfo.environment["FRAMES_MEDIA_FD"].flatMap(Int32.init).map { FileHandle(fileDescriptor:$0,closeOnDealloc:false) }
    static let encodedOutput:FileHandle? = ProcessInfo.processInfo.environment["FRAMES_ENCODED_FD"].flatMap(Int32.init).map { FileHandle(fileDescriptor:$0,closeOnDealloc:false) }
    static let audioOutput:FileHandle? = ProcessInfo.processInfo.environment["FRAMES_AUDIO_FD"].flatMap(Int32.init).map { FileHandle(fileDescriptor:$0,closeOnDealloc:false) }
    static let encodedLock=NSLock(),audioLock=NSLock()
    static func sendEncoded(handle:UInt64,width:UInt32,height:UInt32,sourceWidth:UInt32,sourceHeight:UInt32,number:UInt64,captured:UInt64,keyframe:Bool,bytes:Data,encodeMicros:UInt32=0) {
        var payload=Data([keyframe ? 1:0])
        func append<T:FixedWidthInteger>(_ value:T){var little=value.littleEndian;withUnsafeBytes(of:&little){payload.append(contentsOf:$0)}}
        append(sourceWidth);append(sourceHeight);append(number);append(captured);append(encodeMicros);append(DispatchTime.now().uptimeNanoseconds);payload.append(bytes)
        sendMedia(kind:3,handle:handle,width:width,height:height,bytes:payload)
    }
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
    /// Set once the Frames parent process is gone (its stdin pipe closed).
    /// Writing to its pipes then raises SIGPIPE and kills this process before
    /// it can clean up (dock proxies, released input), so writes are dropped.
    nonisolated(unsafe) private static var parentGone = false
    static func markParentGone() { lock.lock(); parentGone = true; lock.unlock() }
    static var isParentGone: Bool { lock.lock(); defer { lock.unlock() }; return parentGone }
    static func send(_ object: [String: Any]) {
        guard JSONSerialization.isValidJSONObject(object), let bytes = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]) else { return }
        lock.lock(); defer { lock.unlock() }
        if parentGone { return }
        FileHandle.standardOutput.write(bytes); FileHandle.standardOutput.write(Data([10]))
    }
    static func sendFrame(handle:UInt64,width:UInt32,height:UInt32,bytes:Data,number:UInt64=0,captured:UInt64=DispatchTime.now().uptimeNanoseconds) {
        var metadata=Data();var number=number.littleEndian,captured=captured.littleEndian
        withUnsafeBytes(of:&number){metadata.append(contentsOf:$0)};withUnsafeBytes(of:&captured){metadata.append(contentsOf:$0)}
        sendMedia(kind:4,handle:handle,width:width,height:height,bytes:bytes,metadata:metadata)
    }
    static func sendAudio(_ bytes:Data) {
        sendMedia(kind:2,handle:0,width:0,height:0,bytes:bytes)
    }
    private static func sendMedia(kind:UInt32,handle:UInt64,width:UInt32,height:UInt32,bytes:Data,metadata:Data=Data()) {
        if isParentGone { return }
        let chosen = kind == 3 ? encodedOutput : (kind == 2 ? (audioOutput ?? mediaOutput):mediaOutput)
        guard let mediaOutput = chosen else{fputs("Frames host requires a binary media socket\n",stderr);exit(1)}
        var header=Data("FRM1".utf8)
        func append<T:FixedWidthInteger>(_ value:T){var little=value.littleEndian;withUnsafeBytes(of:&little){header.append(contentsOf:$0)}}
        append(kind);append(handle);append(width);append(height);append(UInt64(bytes.count+metadata.count))
        let lock = kind == 3 ? encodedLock:(kind == 2 && audioOutput != nil ? audioLock:mediaLock)
        lock.lock();defer{lock.unlock()}
        mediaOutput.write(header);if !metadata.isEmpty{mediaOutput.write(metadata)};mediaOutput.write(bytes)
    }
    static func permissions(screen: Bool? = nil) -> [String: Any] {
        ["screen_capture":screen ?? CGPreflightScreenCaptureAccess(),"accessibility":AXIsProcessTrusted(),"sharing_at_login":SMAppService.agent(plistName:"dev.creaton.frames.agent.plist").status == .enabled,"control_permission_name":controlPermissionLabel,"bundle_id":Bundle.main.bundleIdentifier ?? "unbundled","executable":CommandLine.arguments[0]]
    }
    static func failure(_ text: String) -> NSError { NSError(domain: "Frames", code: 1, userInfo: [NSLocalizedDescriptionKey: text]) }
}

