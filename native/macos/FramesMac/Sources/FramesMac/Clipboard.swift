import AppKit
import CFramesMac
import Foundation

/// Runs `body` on the main thread (AppKit's pasteboard is used from there).
private func onMain<T: Sendable>(_ body: @MainActor () -> T) -> T {
    if Thread.isMainThread { return MainActor.assumeIsolated { body() } }
    return DispatchQueue.main.sync { MainActor.assumeIsolated { body() } }
}

private func copyOut(_ data: Data?, _ out: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>?) -> Int64 {
    guard let data, let out else { return -1 }
    let buffer = malloc(max(data.count, 1))!.assumingMemoryBound(to: UInt8.self)
    data.copyBytes(to: buffer, count: data.count)
    out.pointee = buffer
    return Int64(data.count)
}

@_cdecl("frames_mac_clipboard_change_count")
public func framesMacClipboardChangeCount() -> UInt64 {
    onMain { UInt64(max(NSPasteboard.general.changeCount, 0)) }
}

@_cdecl("frames_mac_clipboard_text")
public func framesMacClipboardText(_ out: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>?) -> Int64 {
    let data = onMain { NSPasteboard.general.string(forType: .string).map { Data($0.utf8) } }
    return copyOut(data, out)
}

@_cdecl("frames_mac_clipboard_png")
public func framesMacClipboardPng(_ out: UnsafeMutablePointer<UnsafeMutablePointer<UInt8>?>?) -> Int64 {
    let data = onMain { NSPasteboard.general.data(forType: .png) }
    return copyOut(data, out)
}

@_cdecl("frames_mac_clipboard_set_text")
public func framesMacClipboardSetText(_ utf8: UnsafePointer<UInt8>?, _ length: Int) -> Int32 {
    guard let utf8, let text = String(bytes: UnsafeBufferPointer(start: utf8, count: length), encoding: .utf8)
    else { return -1 }
    return onMain {
        NSPasteboard.general.clearContents()
        return NSPasteboard.general.setString(text, forType: .string) ? 0 : -2
    }
}

@_cdecl("frames_mac_clipboard_set_png")
public func framesMacClipboardSetPng(_ png: UnsafePointer<UInt8>?, _ length: Int) -> Int32 {
    guard let png else { return -1 }
    let data = Data(bytes: png, count: length)
    return onMain {
        NSPasteboard.general.clearContents()
        return NSPasteboard.general.setData(data, forType: .png) ? 0 : -2
    }
}

@_cdecl("frames_mac_free")
public func framesMacFree(_ pointer: UnsafeMutableRawPointer?) { free(pointer) }
