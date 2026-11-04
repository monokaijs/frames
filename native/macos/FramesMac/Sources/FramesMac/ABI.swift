import CFramesMac
import CoreGraphics
import CoreVideo
import Foundation

/// Host monotonic clock in nanoseconds: mach_absolute_time scaled to ns, the
/// clock CoreMedia host time and ScreenCaptureKit timestamps use.
func uptimeNanos() -> UInt64 { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) }

/// The Rust-provided callback table, one per object. It replaces the v1
/// `Wire` stdout singleton; nothing is serialized. The C function pointers and
/// the opaque context are thread-agnostic by contract (the Rust side is
/// Send + Sync), hence `@unchecked Sendable`.
struct Callbacks: @unchecked Sendable {
    let raw: FramesMacCallbacks

    func frame(_ buffer: CVPixelBuffer, capturedNanos: UInt64) {
        guard let onFrame = raw.on_frame else { return }
        // The +1 retain transfers to Rust, which releases it (zero copy).
        let pointer = Unmanaged.passRetained(buffer).toOpaque()
        onFrame(
            raw.context, pointer, UInt32(CVPixelBufferGetWidth(buffer)),
            UInt32(CVPixelBufferGetHeight(buffer)), capturedNanos, uptimeNanos())
    }

    func encoded(
        _ bytes: UnsafeRawBufferPointer?, keyframe: Bool, ltrToken: Int64, submitted: UInt64,
        status: Int32
    ) {
        raw.on_encoded?(
            raw.context, bytes?.baseAddress?.assumingMemoryBound(to: UInt8.self),
            bytes?.count ?? 0, keyframe ? UInt32(FRAMES_MAC_ENCODED_KEYFRAME) : 0, ltrToken,
            submitted, uptimeNanos(), status)
    }

    func cursor(x: Double, y: Double, visible: Bool) {
        raw.on_cursor?(raw.context, x, y, visible)
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

func borrowedPixelBuffer(_ pointer: UnsafeMutableRawPointer) -> CVPixelBuffer {
    Unmanaged<CVPixelBuffer>.fromOpaque(pointer).takeUnretainedValue()
}
