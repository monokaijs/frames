import AppKit
import ApplicationServices
import CFramesMac
import CoreGraphics
import CoreImage
import CoreMedia
import CoreVideo
import Foundation
@preconcurrency import ScreenCaptureKit  // SCStream is not Sendable

/// CLI viewers are named `frames` / `frames-next`, while the signed app
/// is `Frames`. Never offer another viewer back to its host: bidirectional
/// connections otherwise recapture themselves and create more windows forever.
func isFramesApplication(owner: String, bundleID: String?) -> Bool {
    if ["frames", "frames-next", "frameshelper"].contains(owner.lowercased()) { return true }
    return ["dev.creaton.frames", "dev.creaton.frames.helper", "app.frames"].contains(bundleID ?? "")
}

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
    private var describedFrame = false
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
    private var pixelRegion = CGRect.zero
    private var cropPool: CVPixelBufferPool?
    private var cropPoolSize = CGSize.zero
    private lazy var cropContext = CIContext(options: [.cacheIntermediates: false])

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
                configuration.width = max(2, Int((frame.width * pointScale).rounded()))
                configuration.height = max(2, Int((frame.height * pointScale).rounded()))
                configuration.minimumFrameInterval = CMTime(value: 1, timescale: fps)
                configuration.queueDepth = 3
                configuration.showsCursor = false
                // Each owned surface has its own stream and viewer window.
                if #available(macOS 14.2, *) { configuration.includeChildWindows = false }
                if ProcessInfo.processInfo.environment["FRAMES_CAPTURE_DEBUG"] != nil {
                    FileHandle.standardError.write("capture \(windowID): window=\(frame), filter=\(filter.contentRect), scale=\(filter.pointPixelScale)\n".data(using: .utf8)!)
                }
                if windowID != 0 {
                    // Match v1: a native window's pixels, without the shadow
                    // or clipping caused by its location on the host display.
                    configuration.ignoreShadowsSingleWindow = true
                    configuration.ignoreGlobalClipSingleWindow = true
                    if #available(macOS 14.2, *) { filter.includeMenuBar = false }
                }
                configuration.pixelFormat = kCVPixelFormatType_32BGRA
                configuration.colorSpaceName = CGColorSpace.sRGB
                let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
                try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
                if windowID != 0 { TitleBarCrop.prepare() }
                // SCK can deliver the only complete frame of a static window
                // before startCapture returns. Its crop must already exist.
                queue.sync {
                    self.stream = stream
                    self.scale = pointScale
                    self.displayOrigin = frame.origin
                    self.configuration = configuration
                    if self.windowID != 0 {
                        self.windowFrame = frame
                        self.contentFrame = self.crop.content(of: frame, window: self.windowID, showHeader: self.showHeader)
                        self.displayOrigin = self.contentFrame.origin
                        self.pixelRegion = windowPixelRegion(frame: frame, content: self.contentFrame, scale: pointScale)
                        self.applied = (
                            CGSize(width: configuration.width, height: configuration.height),
                            self.pixelRegion, configuration.showsCursor
                        )
                    }
                }
                try await stream.startCapture()
                queue.async {
                    if self.windowID != 0 {
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
        let content = crop.content(of: frame, window: windowID, showHeader: showHeader)
        let source = windowPixelRegion(frame: frame, content: content, scale: scale)
        pixelRegion = source
        // Capture the full window at native resolution. Independent-window
        // streams ignore sourceRect; reducing the SCK output size scales the
        // whole window and its chrome instead of cropping it (v1 Host.swift).
        let size = CGSize(
            width: max(2, Int((frame.width * scale).rounded())), height: max(2, Int((frame.height * scale).rounded())))
        if content != contentFrame {
            contentFrame = content
            displayOrigin = content.origin
            reportGeometry()
        }
        guard size.width >= 2, size.height >= 2 else { return }
        let needsUpdate = applied.map { $0.0 != size || $0.2 != showCursor } ?? true
        applied = (size, source, showCursor)
        guard needsUpdate else { return }
        configuration.width = Int(size.width)
        configuration.height = Int(size.height)
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
        if !describedFrame {
            describedFrame = true
            if ProcessInfo.processInfo.environment["FRAMES_CAPTURE_DEBUG"] != nil {
                FileHandle.standardError.write("capture \(windowID) buffer \(CVPixelBufferGetWidth(buffer))x\(CVPixelBufferGetHeight(buffer)), crop \(pixelRegion), applied \(String(describing: applied?.0)), attachments \(String(describing: CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)))\n".data(using: .utf8)!)
            }
        }
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
        if windowID != 0 {
            guard let picture = cropWindow(buffer) else { return }
            callbacks.frame(picture, capturedNanos: captured)
        } else {
            callbacks.frame(buffer, capturedNanos: captured)
        }
    }

    /// Crop verified chrome on the GPU into another IOSurface. The encoder
    /// receives exactly the content pixels; no CPU readback or scaled title bar.
    private func cropWindow(_ buffer: CVPixelBuffer) -> CVPixelBuffer? {
        let width = CVPixelBufferGetWidth(buffer), height = CVPixelBufferGetHeight(buffer)
        guard let applied, Int(applied.0.width) == width, Int(applied.0.height) == height,
            pixelRegion.width >= 2, pixelRegion.height >= 2,
            pixelRegion.minX >= 0, pixelRegion.minY >= 0,
            pixelRegion.maxX <= CGFloat(width), pixelRegion.maxY <= CGFloat(height)
        else { return nil }
        if pixelRegion == CGRect(x: 0, y: 0, width: width, height: height) { return buffer }
        if cropPool == nil || cropPoolSize != pixelRegion.size {
            let attributes: [CFString: Any] = [
                kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
                kCVPixelBufferWidthKey: Int(pixelRegion.width),
                kCVPixelBufferHeightKey: Int(pixelRegion.height),
                kCVPixelBufferMetalCompatibilityKey: true,
                kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
            ]
            cropPool = nil
            guard CVPixelBufferPoolCreate(nil, nil, attributes as CFDictionary, &cropPool) == kCVReturnSuccess else { return nil }
            cropPoolSize = pixelRegion.size
        }
        guard let pool = cropPool else { return nil }
        var result: CVPixelBuffer?
        guard CVPixelBufferPoolCreatePixelBuffer(nil, pool, &result) == kCVReturnSuccess, let result else { return nil }
        let region = CGRect(x: pixelRegion.minX, y: CGFloat(height) - pixelRegion.maxY, width: pixelRegion.width, height: pixelRegion.height)
        let image = CIImage(cvPixelBuffer: buffer).cropped(to: region)
            .transformed(by: CGAffineTransform(translationX: -region.minX, y: -region.minY))
        cropContext.render(image, to: result, bounds: CGRect(origin: .zero, size: region.size), colorSpace: CGColorSpace(name: CGColorSpace.sRGB))
        return result
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
    var excludedApps: [Int32: Bool] = [:]
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
        let excluded = excludedApps[pid] ?? isFramesApplication(
            owner: owner, bundleID: NSRunningApplication(processIdentifier: pid)?.bundleIdentifier)
        excludedApps[pid] = excluded
        if excluded || owner == "Window Server" || owner == "Dock" { continue }
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
            // A modeless panel may also be untitled and inside its owner.
            // Only a verified AX sheet should block input to that owner.
            role = sheetContentBounds(entry.id, frame: entry.bounds, processID: entry.pid) == nil ? "dialog" : "sheet"
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
        let shown = role == "sheet" ? sheetContentBounds(entry.id, frame: entry.bounds, processID: entry.pid) ?? entry.bounds : entry.bounds
        var window: [String: Any] = [
            "id": entry.id, "title": title, "app": entry.owner, "pid": entry.pid,
            "x": shown.origin.x, "y": shown.origin.y,
            "width": shown.width, "height": shown.height,
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
func windowPixelRegion(frame: CGRect, content: CGRect, scale: CGFloat) -> CGRect {
    let left = ((content.minX - frame.minX) * scale).rounded()
    let top = ((content.minY - frame.minY) * scale).rounded()
    let width = Int((content.width * scale).rounded()) & ~1
    let height = Int((content.height * scale).rounded()) & ~1
    return CGRect(x: left, y: top, width: CGFloat(width), height: CGFloat(height))
}

struct TitleBarCrop {
    private var top: CGFloat?

    /// AppKit's standard title bar height (main thread), once known.
    private static let header = HeaderHeight()

    static func prepare() {
        guard header.get() == nil else { return }
        let measure = { @MainActor in
            let frame = NSRect(x: 0, y: 0, width: 800, height: 600)
            let content = NSWindow.contentRect(
                forFrameRect: frame, styleMask: [.titled, .closable, .miniaturizable, .resizable])
            header.set(frame.maxY - content.maxY)
        }
        if Thread.isMainThread {
            MainActor.assumeIsolated { measure() }
        } else {
            DispatchQueue.main.sync { MainActor.assumeIsolated { measure() } }
        }
    }

    mutating func content(of frame: CGRect, window windowID: CGWindowID, showHeader: Bool = false) -> CGRect {
        if let sheet = sheetContentBounds(windowID, frame: frame) { return sheet }
        if showHeader { return frame }
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
