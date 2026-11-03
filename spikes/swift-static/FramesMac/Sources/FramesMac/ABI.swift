import CFramesMac
import CoreGraphics
import CoreVideo
import Foundation

/// Host monotonic clock in nanoseconds. Equals mach_absolute_time scaled to ns,
/// the same clock CoreMedia's host time clock (and ScreenCaptureKit PTS) uses.
func uptimeNanos() -> UInt64 { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }

/// The Rust-provided callback table. It replaces the `Wire` stdout singleton:
/// every session gets its own context, and nothing is serialized to JSON.
/// C function pointers and the opaque context are thread-agnostic by contract
/// (the Rust side is Send + Sync), hence `@unchecked Sendable`.
struct Callbacks: @unchecked Sendable {
    let raw: FramesMacCallbacks

    func frame(_ buffer: CVPixelBuffer, capturedNanos: UInt64) {
        guard let onFrame = raw.on_frame else { return }
        // +1 retain transferred to Rust, which releases it (zero copy).
        let pointer = Unmanaged.passRetained(buffer).toOpaque()
        onFrame(
            raw.context, pointer, UInt32(CVPixelBufferGetWidth(buffer)),
            UInt32(CVPixelBufferGetHeight(buffer)), capturedNanos, uptimeNanos())
    }

    func encoded(
        _ bytes: UnsafeRawBufferPointer?, keyframe: Bool, ltrToken: Int64, submitted: UInt64
    ) {
        raw.on_encoded?(
            raw.context, bytes?.baseAddress?.assumingMemoryBound(to: UInt8.self),
            bytes?.count ?? 0, keyframe, ltrToken, submitted, uptimeNanos())
    }

    func event(_ kind: Int32, _ text: String) {
        var text = text
        text.withUTF8 { bytes in
            raw.on_event?(raw.context, UInt32(kind), bytes.baseAddress, bytes.count)
        }
    }
}

@_cdecl("frames_mac_abi_version")
public func framesMacAbiVersion() -> UInt32 { UInt32(FRAMES_MAC_ABI_VERSION) }

@_cdecl("frames_mac_screen_capture_preflight")
public func framesMacScreenCapturePreflight() -> Bool { CGPreflightScreenCaptureAccess() }

func retainedHandle(_ object: AnyObject) -> UnsafeMutableRawPointer {
    Unmanaged.passRetained(object).toOpaque()
}

func borrowed<T: AnyObject>(_ pointer: UnsafeMutableRawPointer, as: T.Type) -> T {
    Unmanaged<T>.fromOpaque(pointer).takeUnretainedValue()
}

func released<T: AnyObject>(_ pointer: UnsafeMutableRawPointer, as: T.Type) -> T {
    Unmanaged<T>.fromOpaque(pointer).takeRetainedValue()
}
