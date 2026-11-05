import CFramesMac
import CoreGraphics
import CoreMedia
import CoreVideo
import Foundation
@preconcurrency import ScreenCaptureKit  // SCStream is not Sendable

/// ScreenCaptureKit capture of the main display, or of one window (ABI 4),
/// delivering IOSurface-backed BGRA buffers straight to Rust (zero copy).
/// The cursor is excluded from the pixels and reported separately so the
/// client can draw it locally. Falls back to the generated source when
/// capture is not permitted (display capture only).
final class Capture: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    private let callbacks: Callbacks
    private let fps: Int32
    /// The window to capture (CGWindowID); 0 captures the main display.
    private let windowID: CGWindowID
    private let queue = DispatchQueue(label: "app.frames.capture", qos: .userInteractive)
    // Mutated only on `queue`.
    private var stream: SCStream?
    private var timer: DispatchSourceTimer?
    private var pool: CVPixelBufferPool?
    private var tick: UInt64 = 0
    private var scale = 1.0
    private var displayOrigin = CGPoint.zero
    private var lastCursor: (Double, Double, Bool)?

    init(fps: UInt32, windowID: CGWindowID = 0, callbacks: Callbacks) {
        self.fps = Int32(max(1, min(fps, 240)))
        self.windowID = windowID
        self.callbacks = callbacks
    }

    func start(synthetic: Bool) {
        if windowID == 0 && (synthetic || !CGPreflightScreenCaptureAccess()) {
            queue.async { self.startSynthetic(reason: synthetic ? "requested" : "no Screen Recording permission") }
            return
        }
        Task {
            do {
                let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
                let filter: SCContentFilter
                let frame: CGRect
                if windowID != 0 {
                    guard let window = content.windows.first(where: { $0.windowID == self.windowID }) else {
                        throw NSError(domain: "Frames", code: 2, userInfo: [NSLocalizedDescriptionKey: "no such window"])
                    }
                    filter = SCContentFilter(desktopIndependentWindow: window)
                    frame = window.frame
                } else {
                    guard let display = content.displays.first else {
                        throw NSError(domain: "Frames", code: 1, userInfo: [NSLocalizedDescriptionKey: "no display"])
                    }
                    filter = SCContentFilter(display: display, excludingWindows: [])
                    frame = display.frame
                }
                let configuration = SCStreamConfiguration()
                let pointScale = Double(filter.pointPixelScale)
                // Even sizes: the encoders work on 4:2:0 pictures.
                configuration.width = (Int(frame.width * pointScale) + 1) & ~1
                configuration.height = (Int(frame.height * pointScale) + 1) & ~1
                configuration.minimumFrameInterval = CMTime(value: 1, timescale: fps)
                configuration.queueDepth = 3
                configuration.showsCursor = false
                configuration.pixelFormat = kCVPixelFormatType_32BGRA
                configuration.colorSpaceName = CGColorSpace.sRGB
                let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
                try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
                try await stream.startCapture()
                let origin = frame.origin
                queue.async {
                    self.stream = stream
                    self.scale = pointScale
                    self.displayOrigin = origin
                }
                callbacks.event(
                    Int32(FRAMES_MAC_EVENT_CAPTURE_STARTED), "real \(configuration.width)x\(configuration.height)")
            } catch {
                callbacks.event(Int32(FRAMES_MAC_EVENT_CAPTURE_FAILED), "\(error)")
                // A window that cannot be captured stays failed: the host ends
                // that session instead of showing something else.
                if self.windowID == 0 {
                    queue.async { self.startSynthetic(reason: "capture failed") }
                }
            }
        }
    }

    func stop() {
        let stream: SCStream? = queue.sync {
            timer?.cancel()
            timer = nil
            return self.stream
        }
        if let stream {
            let done = DispatchSemaphore(value: 0)
            stream.stopCapture { _ in done.signal() }
            _ = done.wait(timeout: .now() + 2)
        }
    }

    private func reportCursor() {
        guard let location = CGEvent(source: nil)?.location else { return }
        // Quartz global points, top-left origin, to captured pixels.
        let x = (location.x - displayOrigin.x) * scale
        let y = (location.y - displayOrigin.y) * scale
        let cursor = (x, y, true)
        if lastCursor.map({ $0 != cursor }) ?? true {
            lastCursor = cursor
            callbacks.cursor(x: x, y: y, visible: true)
        }
    }

    // MARK: ScreenCaptureKit

    func stream(_ stream: SCStream, didOutputSampleBuffer sample: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, let buffer = sample.imageBuffer else { return }
        reportCursor()
        // Only `.complete` frames carry new pixels; idle frames are skipped.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
            as? [[SCStreamFrameInfo: Any]],
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

    // MARK: Generated source

    private func startSynthetic(reason: String) {
        let width = 1920
        let height = 1080
        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height,
            kCVPixelBufferMetalCompatibilityKey: true,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
        ]
        CVPixelBufferPoolCreate(nil, nil, attributes as CFDictionary, &pool)
        let timer = DispatchSource.makeTimerSource(flags: .strict, queue: queue)
        timer.schedule(
            deadline: .now(), repeating: .nanoseconds(Int(1_000_000_000 / Int64(fps))), leeway: .microseconds(200))
        timer.setEventHandler { [weak self] in self?.emitSynthetic() }
        self.timer = timer
        timer.resume()
        callbacks.event(Int32(FRAMES_MAC_EVENT_CAPTURE_STARTED), "synthetic \(width)x\(height) (\(reason))")
    }

    private func emitSynthetic() {
        guard let pool else { return }
        var created: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(nil, pool, &created) == kCVReturnSuccess, let buffer = created
        else { return }
        let captured = uptimeNanos()
        fillSynthetic(buffer, index: tick)
        tick += 1
        callbacks.frame(buffer, capturedNanos: captured)
    }
}

@_cdecl("frames_mac_capture_start")
public func framesMacCaptureStart(_ fps: UInt32, _ synthetic: Bool, _ callbacks: FramesMacCallbacks)
    -> UnsafeMutableRawPointer?
{
    let capture = Capture(fps: fps, callbacks: Callbacks(raw: callbacks))
    capture.start(synthetic: synthetic)
    return retainedHandle(capture)
}

@_cdecl("frames_mac_capture_start_window")
public func framesMacCaptureStartWindow(_ windowID: UInt32, _ fps: UInt32, _ callbacks: FramesMacCallbacks)
    -> UnsafeMutableRawPointer?
{
    let capture = Capture(fps: fps, windowID: windowID, callbacks: Callbacks(raw: callbacks))
    capture.start(synthetic: false)
    return retainedHandle(capture)
}

/// The application windows other processes show on screen, as a JSON array
/// of {"id": CGWindowID, "title": String, "x", "y", "width", "height" (global
/// points, top-left origin)} in front-to-back order. Writes it
/// into `buffer` when it fits and returns its length either way.
@_cdecl("frames_mac_windows")
public func framesMacWindows(_ buffer: UnsafeMutablePointer<UInt8>?, _ capacity: Int) -> Int {
    let options: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
    let info = CGWindowListCopyWindowInfo(options, kCGNullWindowID) as? [[String: Any]] ?? []
    let ownPid = ProcessInfo.processInfo.processIdentifier
    var windows: [[String: Any]] = []
    for entry in info {
        guard (entry[kCGWindowLayer as String] as? Int) == 0,
            let id = entry[kCGWindowNumber as String] as? UInt32,
            let pid = entry[kCGWindowOwnerPID as String] as? Int32, pid != ownPid,
            let boundsInfo = entry[kCGWindowBounds as String] as? NSDictionary,
            let bounds = CGRect(dictionaryRepresentation: boundsInfo),
            bounds.width >= 80, bounds.height >= 60,
            (entry[kCGWindowAlpha as String] as? Double ?? 1) > 0
        else { continue }
        let owner = entry[kCGWindowOwnerName as String] as? String ?? ""
        // Window names need Screen Recording permission; the app name is
        // what Frames shows without it.
        let name = entry[kCGWindowName as String] as? String ?? ""
        let title = name.isEmpty ? owner : (owner.isEmpty || name.contains(owner) ? name : "\(name) — \(owner)")
        if title.isEmpty || owner == "Frames" { continue }
        windows.append([
            "id": id, "title": title, "x": bounds.origin.x, "y": bounds.origin.y,
            "width": bounds.width, "height": bounds.height,
        ])
    }
    guard let json = try? JSONSerialization.data(withJSONObject: windows) else { return 0 }
    if let buffer, json.count <= capacity {
        json.copyBytes(to: buffer, count: json.count)
    }
    return json.count
}

@_cdecl("frames_mac_capture_stop")
public func framesMacCaptureStop(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    released(handle, as: Capture.self).stop()
}
