import CFramesMac
import CoreMedia
import CoreVideo
import Foundation
@preconcurrency import ScreenCaptureKit  // SCStream is not Sendable (macOS 27 SDK)

/// ScreenCaptureKit capture delivering IOSurface-backed NV12 buffers straight to
/// Rust. Falls back to a synthetic IOSurface pool when capture is not permitted,
/// so the zero-copy handoff can be proven either way.
final class Capture: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    private let callbacks: Callbacks
    private let fps: Int32
    private let queue = DispatchQueue(label: "app.frames.capture", qos: .userInteractive)
    // Mutated only on `queue` (and once before capture starts).
    private var stream: SCStream?
    private var timer: DispatchSourceTimer?
    private var pool: CVPixelBufferPool?
    private var tick = 0

    init(fps: UInt32, callbacks: Callbacks) {
        self.fps = Int32(max(1, min(fps, 240)))
        self.callbacks = callbacks
    }

    func start(synthetic: Bool) {
        if synthetic {
            queue.async { self.startSynthetic(reason: "requested") }
            return
        }
        Task {
            do {
                let content = try await SCShareableContent.excludingDesktopWindows(
                    false, onScreenWindowsOnly: true)
                guard let display = content.displays.first else {
                    throw NSError(domain: "Frames", code: 1, userInfo: [NSLocalizedDescriptionKey: "no display"])
                }
                let filter = SCContentFilter(display: display, excludingWindows: [])
                let configuration = SCStreamConfiguration()
                configuration.width = CGDisplayPixelsWide(display.displayID)
                configuration.height = CGDisplayPixelsHigh(display.displayID)
                configuration.minimumFrameInterval = CMTime(value: 1, timescale: fps)
                configuration.queueDepth = 3
                configuration.showsCursor = false
                // NV12 goes into VideoToolbox without a colour conversion pass.
                configuration.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
                try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
                try await stream.startCapture()
                queue.async { self.stream = stream }
                callbacks.event(Int32(FRAMES_MAC_EVENT_CAPTURE_STARTED), "real \(configuration.width)x\(configuration.height)")
            } catch {
                callbacks.event(Int32(FRAMES_MAC_EVENT_CAPTURE_FAILED), "\(error)")
                queue.async { self.startSynthetic(reason: "capture failed") }
            }
        }
    }

    func stop() {
        queue.sync {
            timer?.cancel()
            timer = nil
        }
        let stream = queue.sync { self.stream }
        if let stream {
            let done = DispatchSemaphore(value: 0)
            stream.stopCapture { _ in done.signal() }
            _ = done.wait(timeout: .now() + 2)
        }
    }

    // MARK: ScreenCaptureKit

    func stream(_ stream: SCStream, didOutputSampleBuffer sample: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, let buffer = sample.imageBuffer else { return }
        // Only `.complete` frames carry new pixels; idle/blank ones are skipped.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]],
            let raw = attachments.first?[.status] as? Int,
            SCFrameStatus(rawValue: raw) != .complete
        {
            return
        }
        let pts = sample.presentationTimeStamp
        let captured = pts.isValid ? UInt64(max(0, pts.seconds) * 1_000_000_000) : uptimeNanos()
        callbacks.frame(buffer, capturedNanos: captured)
    }

    func stream(_ stream: SCStream, didStopWithError error: any Error) {
        callbacks.event(Int32(FRAMES_MAC_EVENT_CAPTURE_FAILED), "stopped: \(error)")
    }

    // MARK: Synthetic IOSurface source

    private func startSynthetic(reason: String) {
        let width = 1920, height = 1080
        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
        ]
        CVPixelBufferPoolCreate(nil, nil, attributes as CFDictionary, &pool)
        let timer = DispatchSource.makeTimerSource(flags: .strict, queue: queue)
        timer.schedule(deadline: .now(), repeating: .nanoseconds(Int(1_000_000_000 / Int64(fps))), leeway: .microseconds(200))
        timer.setEventHandler { [weak self] in self?.emitSynthetic() }
        self.timer = timer
        timer.resume()
        callbacks.event(Int32(FRAMES_MAC_EVENT_CAPTURE_STARTED), "synthetic \(width)x\(height) (\(reason))")
    }

    private func emitSynthetic() {
        guard let pool else { return }
        var created: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(nil, pool, &created) == kCVReturnSuccess, let buffer = created else { return }
        let captured = uptimeNanos()
        CVPixelBufferLockBaseAddress(buffer, [])
        let width = CVPixelBufferGetWidthOfPlane(buffer, 0)
        let height = CVPixelBufferGetHeightOfPlane(buffer, 0)
        if let luma = CVPixelBufferGetBaseAddressOfPlane(buffer, 0)?.assumingMemoryBound(to: UInt8.self) {
            let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
            let bar = (tick * 16) % width
            for row in 0..<height {
                let line = luma + row * stride
                // Static "desktop" plus a moving 160-px bar: realistic small deltas.
                memset(line, Int32(16 + (row / 8) % 200), width)
                memset(line + bar, 235, min(160, width - bar))
            }
        }
        if let chroma = CVPixelBufferGetBaseAddressOfPlane(buffer, 1) {
            memset(chroma, 128, CVPixelBufferGetBytesPerRowOfPlane(buffer, 1) * CVPixelBufferGetHeightOfPlane(buffer, 1))
        }
        CVPixelBufferUnlockBaseAddress(buffer, [])
        tick += 1
        callbacks.frame(buffer, capturedNanos: captured)
    }
}

@_cdecl("frames_mac_capture_start")
public func framesMacCaptureStart(_ fps: UInt32, _ synthetic: Bool, _ callbacks: FramesMacCallbacks) -> UnsafeMutableRawPointer? {
    let capture = Capture(fps: fps, callbacks: Callbacks(raw: callbacks))
    capture.start(synthetic: synthetic)
    return retainedHandle(capture)
}

@_cdecl("frames_mac_capture_stop")
public func framesMacCaptureStop(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    released(handle, as: Capture.self).stop()
}
