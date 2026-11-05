import AppKit
import ApplicationServices
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
    // Window capture (ABI 5), on `queue`: the title bar is cropped unless
    // `showHeader`; the picture follows the window's size.
    private var showHeader = false
    private var showCursor = false
    private var configuration: SCStreamConfiguration?
    private var geometryTimer: DispatchSourceTimer?
    /// The applied picture: its size in pixels, the source rect in window
    /// points, and whether the cursor is drawn.
    private var applied: (CGSize, CGRect, Bool)?
    /// The window's frame and the captured part of it (global points).
    private var windowFrame = CGRect.zero
    private var contentFrame = CGRect.zero
    private var crop = TitleBarCrop()

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
                if windowID != 0 {
                    configuration.sourceRect = CGRect(origin: .zero, size: frame.size)
                }
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
                    self.configuration = configuration
                    if self.windowID != 0 {
                        self.windowFrame = frame
                        self.contentFrame = frame
                        self.applied = (
                            CGSize(width: configuration.width, height: configuration.height),
                            configuration.sourceRect, false
                        )
                        self.startGeometry()
                    } else {
                        self.applyOptions()
                    }
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
            geometryTimer?.cancel()
            geometryTimer = nil
            return self.stream
        }
        if let stream {
            let done = DispatchSemaphore(value: 0)
            stream.stopCapture { _ in done.signal() }
            _ = done.wait(timeout: .now() + 2)
        }
    }

    // MARK: Window geometry (ABI 5)

    func setOptions(showHeader: Bool, showCursor: Bool) {
        queue.async {
            self.showHeader = showHeader
            self.showCursor = showCursor
            if self.windowID == 0 { self.applyOptions() } else { self.refreshGeometry() }
        }
    }

    /// Display capture: only the cursor option applies.
    private func applyOptions() {
        guard let stream, let configuration, configuration.showsCursor != showCursor else { return }
        configuration.showsCursor = showCursor
        stream.updateConfiguration(configuration) { _ in }
    }

    private func startGeometry() {
        TitleBarCrop.prepare()
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now() + .milliseconds(100), repeating: .milliseconds(250))
        timer.setEventHandler { [weak self] in self?.refreshGeometry() }
        geometryTimer = timer
        timer.resume()
        reportGeometry()
    }

    /// Follows the window's size and the title bar crop, and reports the
    /// captured rect (input maps to it).
    private func refreshGeometry() {
        guard let stream, let configuration, let frame = windowBounds(windowID) else { return }
        windowFrame = frame
        let content = showHeader ? frame : crop.content(of: frame, window: windowID)
        let source = CGRect(
            x: content.minX - frame.minX, y: content.minY - frame.minY, width: content.width, height: content.height)
        // Even sizes: the encoders work on 4:2:0 pictures.
        let size = CGSize(
            width: (Int(content.width * scale) + 1) & ~1, height: (Int(content.height * scale) + 1) & ~1)
        if content != contentFrame {
            contentFrame = content
            displayOrigin = content.origin
            reportGeometry()
        }
        guard size.width >= 2, size.height >= 2 else { return }
        if let (oldSize, oldSource, oldCursor) = applied, oldSize == size, oldSource == source, oldCursor == showCursor {
            return
        }
        applied = (size, source, showCursor)
        configuration.width = Int(size.width)
        configuration.height = Int(size.height)
        configuration.sourceRect = source
        configuration.showsCursor = showCursor
        stream.updateConfiguration(configuration) { [callbacks] error in
            if let error { callbacks.event(Int32(FRAMES_MAC_EVENT_LOG), "capture update failed: \(error)") }
        }
    }

    private func reportGeometry() {
        let c = contentFrame
        callbacks.event(Int32(FRAMES_MAC_EVENT_GEOMETRY), "\(c.minX) \(c.minY) \(c.width) \(c.height)")
    }

    /// Resizes the window so the captured part is `width` x `height` pixels.
    func resizeWindow(width: Int, height: Int) -> Bool {
        let (frame, content, scale) = queue.sync { (windowFrame, contentFrame, self.scale) }
        guard windowID != 0, width >= 32, height >= 32, scale > 0, frame.width > 0 else { return false }
        var size = CGSize(
            width: CGFloat(width) / scale + frame.width - content.width,
            height: CGFloat(height) / scale + frame.height - content.height)
        guard let window = axWindow(windowID) else { return false }
        var settable = DarwinBoolean(false)
        guard AXUIElementIsAttributeSettable(window, kAXSizeAttribute as CFString, &settable) == .success,
            settable.boolValue, let value = AXValueCreate(.cgSize, &size)
        else { return false }
        let done = AXUIElementSetAttributeValue(window, kAXSizeAttribute as CFString, value) == .success
        queue.async { self.refreshGeometry() }
        return done
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
/// of {"id": CGWindowID, "title": String, "app": owner name, "x", "y",
/// "width", "height" (global points, top-left origin)} in front-to-back
/// order (ABI 4). Menus, tooltips, popovers and sheets (ABI 5) also carry
/// "role" and "parent" (the CGWindowID of the application's window they
/// belong to); without a parent they are left out, as v1 did. Writes it
/// into `buffer` when it fits and returns its length either way.
@_cdecl("frames_mac_windows")
public func framesMacWindows(_ buffer: UnsafeMutablePointer<UInt8>?, _ capacity: Int) -> Int {
    let options: CGWindowListOption = [.optionOnScreenOnly, .excludeDesktopElements]
    let info = CGWindowListCopyWindowInfo(options, kCGNullWindowID) as? [[String: Any]] ?? []
    let ownPid = ProcessInfo.processInfo.processIdentifier
    let menuLevel = Int(CGWindowLevelForKey(.popUpMenuWindow))
    let helpLevel = Int(CGWindowLevelForKey(.helpWindow))
    struct Entry {
        let id: UInt32
        let pid: Int32
        let layer: Int
        let bounds: CGRect
        let name: String
        let owner: String
    }
    var entries: [Entry] = []
    for entry in info {
        guard let layer = entry[kCGWindowLayer as String] as? Int,
            let id = entry[kCGWindowNumber as String] as? UInt32,
            let pid = entry[kCGWindowOwnerPID as String] as? Int32, pid != ownPid,
            let boundsInfo = entry[kCGWindowBounds as String] as? NSDictionary,
            let bounds = CGRect(dictionaryRepresentation: boundsInfo),
            (entry[kCGWindowAlpha as String] as? Double ?? 1) > 0,
            layer == 0 || (layer > 0 && layer <= 30) || layer == menuLevel || layer == helpLevel
        else { continue }
        let owner = entry[kCGWindowOwnerName as String] as? String ?? ""
        if owner == "Frames" || owner == "Window Server" || owner == "Dock" { continue }
        entries.append(
            Entry(id: id, pid: pid, layer: layer, bounds: bounds, name: entry[kCGWindowName as String] as? String ?? "",
                owner: owner))
    }
    // An application's ordinary windows, front to back: a popup belongs to
    // the front one it overlaps (else the front one).
    let owners = entries.filter { $0.layer == 0 && $0.bounds.width >= 80 && $0.bounds.height >= 60 }
    func parent(of entry: Entry) -> UInt32? {
        let mine = owners.filter { $0.pid == entry.pid && $0.id != entry.id }
        return (mine.first { $0.bounds.intersects(entry.bounds) } ?? mine.first)?.id
    }
    var windows: [[String: Any]] = []
    for entry in entries {
        var role = "window"
        var parentID: UInt32?
        if entry.layer == menuLevel {
            role = "menu"
        } else if entry.layer == helpLevel {
            role = "tooltip"
        } else if entry.layer > 0 {
            role = "popover"
        } else if entry.name.isEmpty,
            let owner = owners.first(where: {
                $0.pid == entry.pid && $0.id != entry.id && $0.bounds.contains(entry.bounds)
                    && $0.bounds.width > entry.bounds.width
            })
        {
            // An untitled window inside another of its application's: a
            // sheet or a panel attached to it.
            role = "sheet"
            parentID = owner.id
        }
        if role == "window" {
            guard entry.bounds.width >= 80, entry.bounds.height >= 60 else { continue }
        } else {
            if parentID == nil { parentID = parent(of: entry) }
            guard parentID != nil, entry.bounds.width >= 4, entry.bounds.height >= 4 else { continue }
        }
        // Window names need Screen Recording permission; the app name is
        // what Frames shows without it.
        let title =
            entry.name.isEmpty
            ? entry.owner : (entry.owner.isEmpty || entry.name.contains(entry.owner) ? entry.name : "\(entry.name) — \(entry.owner)")
        if title.isEmpty { continue }
        var window: [String: Any] = [
            "id": entry.id, "title": title, "app": entry.owner, "pid": entry.pid,
            "x": entry.bounds.origin.x, "y": entry.bounds.origin.y,
            "width": entry.bounds.width, "height": entry.bounds.height,
        ]
        if let parentID {
            window["role"] = role
            window["parent"] = parentID
        }
        windows.append(window)
    }
    guard let json = try? JSONSerialization.data(withJSONObject: windows) else { return 0 }
    if let buffer, json.count <= capacity {
        json.copyBytes(to: buffer, count: json.count)
    }
    return json.count
}

@_cdecl("frames_mac_capture_set_options")
public func framesMacCaptureSetOptions(_ handle: UnsafeMutableRawPointer?, _ showHeader: Bool, _ showCursor: Bool) {
    guard let handle else { return }
    borrowed(handle, as: Capture.self).setOptions(showHeader: showHeader, showCursor: showCursor)
}

@_cdecl("frames_mac_capture_resize_window")
public func framesMacCaptureResizeWindow(_ handle: UnsafeMutableRawPointer?, _ width: UInt32, _ height: UInt32) -> Bool {
    guard let handle else { return false }
    return borrowed(handle, as: Capture.self).resizeWindow(width: Int(width), height: Int(height))
}

/// A window's frame in global points (top-left origin), from Quartz.
func windowBounds(_ windowID: CGWindowID) -> CGRect? {
    let info = CGWindowListCopyWindowInfo([.optionIncludingWindow], windowID) as? [[String: Any]] ?? []
    guard let entry = info.first, let boundsInfo = entry[kCGWindowBounds as String] as? NSDictionary else {
        return nil
    }
    return CGRect(dictionaryRepresentation: boundsInfo)
}

/// The standard title bar cropped from captured windows (v1's rule): only
/// when Accessibility finds the window's title buttons inside AppKit's
/// standard title bar. Custom chrome, toolbars and full screen windows stay
/// whole. Insets are kept across Accessibility failures (moves, resizes).
struct TitleBarCrop {
    private var top: CGFloat?

    /// AppKit's standard title bar height (main thread), once known.
    private static let header = HeaderHeight()

    static func prepare() {
        guard header.get() == nil else { return }
        DispatchQueue.main.async {
            MainActor.assumeIsolated {
                let frame = NSRect(x: 0, y: 0, width: 800, height: 600)
                let content = NSWindow.contentRect(
                    forFrameRect: frame, styleMask: [.titled, .closable, .miniaturizable, .resizable])
                header.set(frame.maxY - content.maxY)
            }
        }
    }

    mutating func content(of frame: CGRect, window windowID: CGWindowID) -> CGRect {
        if let measured = measure(frame: frame, window: windowID) {
            top = measured
        }
        guard let top, top > 0, frame.height > top else { return frame }
        return CGRect(x: frame.minX, y: frame.minY + top, width: frame.width, height: frame.height - top)
    }

    /// The title bar to crop now; nil when Accessibility cannot tell (the
    /// last answer stands).
    private func measure(frame: CGRect, window windowID: CGWindowID) -> CGFloat? {
        guard let top = Self.header.get(), top > 0, top < frame.height, let ax = axWindow(windowID) else { return nil }
        AXUIElementSetMessagingTimeout(ax, 0.2)
        var fullScreen: CFTypeRef?
        if AXUIElementCopyAttributeValue(ax, "AXFullScreen" as CFString, &fullScreen) == .success,
            (fullScreen as? NSNumber)?.boolValue == true
        {
            return 0
        }
        let header = CGRect(x: frame.minX, y: frame.minY, width: frame.width, height: top).insetBy(dx: -1, dy: -1)
        var buttons: [CGRect] = []
        for name in [kAXCloseButtonAttribute, kAXMinimizeButtonAttribute, kAXZoomButtonAttribute] {
            var value: CFTypeRef?
            let result = AXUIElementCopyAttributeValue(ax, name as CFString, &value)
            if result == .attributeUnsupported || result == .noValue { continue }
            guard result == .success, let value, CFGetTypeID(value) == AXUIElementGetTypeID(),
                let button = axFrame(unsafeDowncast(value, to: AXUIElement.self))
            else { return nil }
            buttons.append(button)
        }
        guard !buttons.isEmpty, buttons.allSatisfy({ !$0.isEmpty && header.contains($0) }) else { return 0 }
        return top
    }
}

/// A lock-protected value set once on the main thread.
private final class HeaderHeight: @unchecked Sendable {
    private let lock = NSLock()
    private var value: CGFloat?
    func get() -> CGFloat? {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
    func set(_ new: CGFloat) {
        lock.lock()
        value = new
        lock.unlock()
    }
}

@_cdecl("frames_mac_capture_stop")
public func framesMacCaptureStop(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    released(handle, as: Capture.self).stop()
}
