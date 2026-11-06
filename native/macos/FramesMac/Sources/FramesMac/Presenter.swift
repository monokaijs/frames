import AppKit
import CFramesMac
import CoreVideo
import Foundation
import Metal
import QuartzCore

/// Metal presenter driven by the caller's thread: the engine's presenter
/// thread is the render thread. `vsync == false` turns off display sync on
/// the layer, so a frame goes to the glass as soon as it is drawn (it may
/// tear); `true` waits for the next refresh.
final class Presenter: @unchecked Sendable {
    let layer: CAMetalLayer
    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let pipeline: MTLRenderPipelineState
    private var cache: CVMetalTextureCache?
    private var window: NSWindow?
    let windowed: Bool
    let refreshHz: Double
    /// Host time (ns) the newest drawable reached the glass, when known.
    let presentedAt = PresentedTime()
    /// Where the window's input goes (set by the Rust viewer).
    let input = InputRelay()
    private var inputView: InputView?
    /// The window's view (windowed presenters).
    var view: InputView? { inputView }
    /// Set once the user closes the window (ABI 4).
    let closed = CloseWatcher()
    /// Files dropped on the window, for the host (ABI 4).
    let drops = DroppedFiles()

    init?(width: Int, height: Int, windowed: Bool, popup: Bool = false) {
        guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue() else { return nil }
        self.device = device
        self.queue = queue
        let source = """
            #include <metal_stdlib>
            using namespace metal;
            struct V { float4 p [[position]]; float2 uv; };
            vertex V vertexMain(uint id [[vertex_id]]) {
              float2 p[4] = {float2(-1,-1), float2(1,-1), float2(-1,1), float2(1,1)};
              V v; v.p = float4(p[id], 0, 1); v.uv = float2((p[id].x + 1) * 0.5, (1 - p[id].y) * 0.5); return v;
            }
            fragment float4 fragmentMain(V v [[stage_in]], texture2d<float> picture [[texture(0)]]) {
              constexpr sampler s(filter::linear); return picture.sample(s, v.uv);
            }
            """
        guard let library = try? device.makeLibrary(source: source, options: nil) else { return nil }
        let descriptor = MTLRenderPipelineDescriptor()
        descriptor.vertexFunction = library.makeFunction(name: "vertexMain")
        descriptor.fragmentFunction = library.makeFunction(name: "fragmentMain")
        descriptor.colorAttachments[0].pixelFormat = .bgra8Unorm
        guard let pipeline = try? device.makeRenderPipelineState(descriptor: descriptor) else { return nil }
        self.pipeline = pipeline
        guard CVMetalTextureCacheCreate(nil, nil, device, nil, &cache) == kCVReturnSuccess else { return nil }
        let layer = CAMetalLayer()
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.maximumDrawableCount = 2
        layer.allowsNextDrawableTimeout = true
        layer.drawableSize = CGSize(width: width, height: height)
        // Letterboxed ("fit"): the picture keeps its shape inside the view.
        layer.contentsGravity = .resizeAspect
        layer.backgroundColor = CGColor(gray: 0, alpha: 1)
        // This layer is created on the render worker, which has no run
        // loop to commit Core Animation's implicit transaction. Commit its
        // setup before handing it to AppKit or presented drawables stay black.
        CATransaction.flush()
        self.layer = layer
        self.windowed = windowed
        // Offscreen presenters never touch the main thread (it may not be
        // running a run loop, e.g. under a test harness).
        guard windowed else {
            let mode = CGDisplayCopyDisplayMode(CGMainDisplayID())
            let rate = mode?.refreshRate ?? 0
            refreshHz = rate > 0 ? rate : 60
            return
        }
        var refresh = 60.0
        let input = self.input
        let closed = self.closed
        let drops = self.drops
        let makeWindow = { @MainActor () -> (NSWindow?, Double, InputView) in
            let hz = Double(NSScreen.main?.maximumFramesPerSecond ?? 60)
            let scale = NSScreen.main?.backingScaleFactor ?? 2
            // Pixels to points, shrunk to fit the screen when larger.
            var size = NSSize(width: CGFloat(width) / scale, height: CGFloat(height) / scale)
            if let screen = NSScreen.main?.visibleFrame.size {
                let fit = min(1, screen.width * 0.9 / size.width, screen.height * 0.9 / size.height)
                size = NSSize(width: size.width * fit, height: size.height * fit)
            }
            let frame = NSRect(x: 80, y: 80, width: size.width, height: size.height)
            let window = NSWindow(
                contentRect: frame, styleMask: popup ? [.borderless] : [.titled, .closable, .resizable],
                backing: .buffered, defer: false)
            window.title = "Frames presenter"
            window.isReleasedWhenClosed = false
            window.delegate = closed
            // Resizing keeps the picture's shape: no bars around it.
            window.contentAspectRatio = NSSize(width: CGFloat(width), height: CGFloat(height))
            let view = InputView(frame: frame, relay: input)
            view.drops = drops
            view.registerForDraggedTypes([.fileURL])
            view.wantsLayer = true
            view.layer = layer
            window.contentView = view
            closed.view = view
            window.acceptsMouseMovedEvents = true
            if popup {
                // Shown by attach(), over its parent; never key.
                window.hasShadow = true
            } else {
                addConnectionButton(window, view: view)
                window.makeKeyAndOrderFront(nil)
                window.makeFirstResponder(view)
            }
            return (window, hz, view)
        }
        let made: (NSWindow?, Double, InputView)
        if Thread.isMainThread {
            made = MainActor.assumeIsolated { makeWindow() }
        } else {
            made = DispatchQueue.main.sync { MainActor.assumeIsolated { makeWindow() } }
        }
        (window, refresh, inputView) = (made.0, made.1, made.2)
        refreshHz = refresh
    }

    /// Shows the host's cursor over the view: straight-alpha BGRA pixels
    /// (built into an image on the main thread; images are not Sendable).
    func setCursor(_ shape: CursorShape?, visible: Bool?) {
        guard let inputView else { return }
        let apply = { @MainActor in
            inputView.setHostCursor(shape, visible: visible)
        }
        if Thread.isMainThread {
            MainActor.assumeIsolated { apply() }
        } else {
            DispatchQueue.main.async { MainActor.assumeIsolated { apply() } }
        }
    }

    /// The stream's new size (ABI 4): the drawable follows it, and the
    /// window takes its shape at the window's current scale, within the
    /// screen.
    func resize(width: Int, height: Int) {
        layer.drawableSize = CGSize(width: width, height: height)
        guard let window else { return }
        let view = inputView
        let closed = self.closed
        let apply = { @MainActor in
            view?.pictureChanged()
            // "Resize remote app": the window keeps the size the user gave
            // it (the host followed it); the picture fits inside.
            guard view?.keepAspect ?? true else { return }
            let current = window.contentLayoutRect.size
            let old = window.contentAspectRatio
            let scale = old.width > 0 ? current.width / old.width : 0.5
            var size = NSSize(width: CGFloat(width) * scale, height: CGFloat(height) * scale)
            if let screen = (window.screen ?? NSScreen.main)?.visibleFrame.size {
                let fit = min(1, screen.width / size.width, screen.height / size.height)
                size = NSSize(width: size.width * fit, height: size.height * fit)
            }
            window.contentAspectRatio = NSSize(width: CGFloat(width), height: CGFloat(height))
            closed.programmatic = true
            window.setContentSize(size)
            closed.programmatic = false
        }
        if Thread.isMainThread {
            MainActor.assumeIsolated { apply() }
        } else {
            DispatchQueue.main.async { MainActor.assumeIsolated { apply() } }
        }
    }

    /// Shows this popup over its parent's picture (main thread), retrying
    /// while the parent's window is not open yet.
    func attach(parent: UInt64, frame: CGRect, parentSize: CGSize, role: UInt32, attempts: Int = 40) {
        guard let window else { return }
        let apply = { @MainActor in
            guard
                let parentView = NSApplication.shared.windows.compactMap({ $0.contentView as? InputView })
                    .first(where: { $0.menuID == parent && $0.window?.styleMask.contains(.titled) == true }),
                let parentWindow = parentView.window, parentSize.width > 0
            else {
                if attempts > 0 {
                    DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) {
                        self.attach(parent: parent, frame: frame, parentSize: parentSize, role: role, attempts: attempts - 1)
                    }
                }
                return
            }
            let picture = parentWindow.convertToScreen(parentView.convert(parentView.pictureRect, to: nil))
            let scale = picture.width / parentSize.width
            // The parent's title bar may be cropped: its picture starts
            // below the top of its frame.
            let crop = max(0, parentSize.height - picture.height / max(scale, 0.0001))
            let shown = NSRect(
                x: picture.minX + frame.minX * scale, y: picture.maxY - (frame.minY - crop + frame.height) * scale,
                width: frame.width * scale, height: frame.height * scale)
            window.setFrame(shown, display: false)
            switch Int(role) {
            case Int(FRAMES_MAC_POPUP_MENU): window.level = .popUpMenu
            case Int(FRAMES_MAC_POPUP_TOOLTIP):
                window.level = .popUpMenu
                window.ignoresMouseEvents = true
            case Int(FRAMES_MAC_POPUP_SHEET): window.level = parentWindow.level
            default: window.level = .floating
            }
            if window.parent !== parentWindow {
                window.parent?.removeChildWindow(window)
                parentWindow.addChildWindow(window, ordered: .above)
            }
            window.orderFront(nil)
        }
        if Thread.isMainThread {
            MainActor.assumeIsolated { apply() }
        } else {
            DispatchQueue.main.async { MainActor.assumeIsolated { apply() } }
        }
    }

    func setTitle(_ title: String) {
        guard let window else { return }
        let apply = { @MainActor in window.title = title }
        if Thread.isMainThread {
            MainActor.assumeIsolated { apply() }
        } else {
            DispatchQueue.main.async { MainActor.assumeIsolated { apply() } }
        }
    }

    func setRelative(_ relative: Bool, lockPointer: Bool) {
        guard let inputView else { return }
        let apply = { @MainActor in inputView.setRelative(relative, lockPointer: lockPointer) }
        if Thread.isMainThread {
            MainActor.assumeIsolated { apply() }
        } else {
            DispatchQueue.main.async { MainActor.assumeIsolated { apply() } }
        }
    }

    /// Generated events for unattended tests, through `InputView`'s handlers.
    func postSyntheticInput(step: UInt64) {
        guard let inputView else { return }
        let post = { @MainActor in inputView.synthetic(step: step) }
        if Thread.isMainThread {
            MainActor.assumeIsolated { post() }
        } else {
            DispatchQueue.main.async { MainActor.assumeIsolated { post() } }
        }
    }

    func close() {
        input.setText(context: nil, callback: nil)
        input.set(context: nil, callback: nil)
        guard let window else { return }
        self.window = nil
        // The pointer is given back before the window goes (the callback is
        // already removed, so the host is not told twice).
        let view = inputView
        let close = { @MainActor in
            view?.setRelative(false)
            window.close()
        }
        if Thread.isMainThread {
            MainActor.assumeIsolated { close() }
        } else {
            DispatchQueue.main.async { MainActor.assumeIsolated { close() } }
        }
    }

    /// Draws `buffer` and returns when it is on the glass (windowed) or
    /// scheduled (offscreen), with that time.
    func present(_ buffer: CVPixelBuffer, vsync: Bool) -> (Int32, UInt64) {
        layer.displaySyncEnabled = vsync
        guard let cache else { return (-1, 0) }
        var mapped: CVMetalTexture?
        guard
            CVMetalTextureCacheCreateTextureFromImage(
                nil, cache, buffer, nil, .bgra8Unorm, CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer), 0,
                &mapped) == kCVReturnSuccess, let mapped, let texture = CVMetalTextureGetTexture(mapped)
        else { return (-2, 0) }
        let waitStart = uptimeNanos()
        guard let drawable = layer.nextDrawable(), let commands = queue.makeCommandBuffer() else { return (-3, 0) }
        let waited = uptimeNanos() - waitStart
        if waited > 20_000_000, ProcessInfo.processInfo.environment["FRAMES_PRESENTER_DEBUG"] != nil {
            FileHandle.standardError.write("nextDrawable waited \(waited / 1_000_000) ms\n".data(using: .utf8)!)
        }
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .dontCare
        pass.colorAttachments[0].storeAction = .store
        guard let encoder = commands.makeRenderCommandEncoder(descriptor: pass) else { return (-4, 0) }
        encoder.setRenderPipelineState(pipeline)
        encoder.setFragmentTexture(texture, index: 0)
        encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        encoder.endEncoding()
        // The mapped texture must outlive the GPU's use of it.
        let keepAlive = Retained(mapped)
        commands.addCompletedHandler { _ in withExtendedLifetime(keepAlive) {} }
        if windowed {
            // Record when the previous frame actually reached the glass, for
            // diagnostics; never block the render thread on it.
            let presentedAt = self.presentedAt
            drawable.addPresentedHandler { drawable in
                // presentedTime is CACurrentMediaTime seconds (host time).
                if drawable.presentedTime > 0 { presentedAt.set(UInt64(drawable.presentedTime * 1e9)) }
            }
        }
        commands.present(drawable)
        commands.commit()
        commands.waitUntilScheduled()
        return (0, uptimeNanos())
    }
}

/// Keeps a CoreFoundation object alive across a Sendable completion handler;
/// it is only retained and released there, never used.
private final class Retained: @unchecked Sendable {
    let value: AnyObject
    init(_ value: AnyObject) { self.value = value }
}

/// A cursor as it crosses threads: plain values only.
struct CursorShape: Sendable {
    let bgra: Data
    let width: Int
    let height: Int
    let hotspot: NSPoint
}

/// The cursor as an image `scale` points per host pixel, so it covers as
/// much of the picture as it does on the host.
@MainActor
private func cursorImage(_ shape: CursorShape, scale: CGFloat) -> NSImage? {
    let (w, h) = (shape.width, shape.height)
    guard
        let bitmap = NSBitmapImageRep(
            bitmapDataPlanes: nil, pixelsWide: w, pixelsHigh: h, bitsPerSample: 8, samplesPerPixel: 4,
            hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: w * 4, bitsPerPixel: 32),
        let pixels = bitmap.bitmapData, shape.bgra.count == w * h * 4
    else { return nil }
    shape.bgra.withUnsafeBytes { (bgra: UnsafeRawBufferPointer) in
        for i in 0..<(w * h) {
            pixels[i * 4] = bgra[i * 4 + 2]
            pixels[i * 4 + 1] = bgra[i * 4 + 1]
            pixels[i * 4 + 2] = bgra[i * 4]
            pixels[i * 4 + 3] = bgra[i * 4 + 3]
        }
    }
    let image = NSImage(size: NSSize(width: CGFloat(w) * scale, height: CGFloat(h) * scale))
    image.addRepresentation(bitmap)
    return image
}

final class PresentedTime: @unchecked Sendable {
    private let lock = NSLock()
    private var value: UInt64 = 0
    func set(_ time: UInt64) {
        lock.lock()
        value = time
        lock.unlock()
    }
    func get() -> UInt64 {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

/// Notices the user closing a presenter window; read from any thread.
/// With `intercept`, the first close only records a request (the viewer
/// asks the host window to close; its going away ends the session) and a
/// second one closes the window.
final class CloseWatcher: NSObject, NSWindowDelegate, @unchecked Sendable {
    /// The window's view, told when the user resizes the window.
    @MainActor weak var view: InputView?
    /// Set while the presenter itself resizes the window.
    @MainActor var programmatic = false
    private let lock = NSLock()
    private var value = false
    private var intercept = false
    private var requested = false
    private var asked = false

    @MainActor func windowShouldClose(_ sender: NSWindow) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if intercept && !asked {
            asked = true
            requested = true
            return false
        }
        return true
    }

    @MainActor func windowDidResize(_ notification: Notification) {
        view?.pictureChanged()
        if !programmatic { view?.userResized() }
    }

    @MainActor func windowWillClose(_ notification: Notification) {
        view?.focusOwner()
        lock.lock()
        value = true
        lock.unlock()
    }

    func setIntercept(_ on: Bool) {
        lock.lock()
        intercept = on
        lock.unlock()
    }

    /// Whether a close was requested since the last call.
    func takeRequest() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        let was = requested
        requested = false
        return was
    }

    var isClosed: Bool {
        lock.lock()
        defer { lock.unlock() }
        return value
    }
}

@_cdecl("frames_mac_presenter_resize")
public func framesMacPresenterResize(_ handle: UnsafeMutableRawPointer?, _ width: UInt32, _ height: UInt32) {
    guard let handle else { return }
    borrowed(handle, as: Presenter.self).resize(width: Int(width), height: Int(height))
}

/// Files dropped on a presenter window, until taken: paths, or with the
/// point they were dropped at over the picture (ABI 6) {"path", "x", "y"}
/// (0-65535, origin top left), for the host to drop them there too.
final class DroppedFiles: @unchecked Sendable {
    private let lock = NSLock()
    private var paths: [Any] = []

    func add(_ new: [String], at point: (Int32, Int32)? = nil) {
        lock.lock()
        if let point {
            paths.append(contentsOf: new.map { ["path": $0, "x": point.0, "y": point.1] as [String: Any] })
        } else {
            paths.append(contentsOf: new)
        }
        lock.unlock()
    }

    func put(back: [Any]) {
        lock.lock()
        paths.insert(contentsOf: back, at: 0)
        lock.unlock()
    }

    func take() -> [Any] {
        lock.lock()
        defer { lock.unlock() }
        let taken = paths
        paths = []
        return taken
    }
}

/// Files dropped on the window since the last call, as a UTF-8 JSON array
/// of paths (ABI 4). Written to `buffer` when it fits in `capacity`; the
/// length is returned either way, and the files are kept until they fit.
@_cdecl("frames_mac_presenter_take_dropped")
public func framesMacPresenterTakeDropped(
    _ handle: UnsafeMutableRawPointer?, _ buffer: UnsafeMutablePointer<UInt8>?, _ capacity: Int
) -> Int {
    guard let handle else { return 0 }
    let drops = borrowed(handle, as: Presenter.self).drops
    let paths = drops.take()
    guard !paths.isEmpty, let json = try? JSONSerialization.data(withJSONObject: paths) else { return 0 }
    guard let buffer, json.count <= capacity else {
        drops.put(back: paths)
        return json.count
    }
    json.copyBytes(to: buffer, count: json.count)
    return json.count
}

/// The shared application's menus for this window's menu bar (ABI 4):
/// UTF-8 JSON as `frames_mac_app_menus` makes it; items are chosen as
/// FRAMES_MAC_INPUT_MENU (a = id).
@_cdecl("frames_mac_presenter_set_menus")
public func framesMacPresenterSetMenus(_ handle: UnsafeMutableRawPointer?, _ utf8: UnsafePointer<UInt8>?, _ length: Int) {
    guard let handle, let utf8 else { return }
    let data = Data(bytes: utf8, count: length)
    let presenter = borrowed(handle, as: Presenter.self)
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            guard let view = presenter.view else { return }
            view.hostMenus = (try? JSONSerialization.jsonObject(with: data)) as? [[String: Any]] ?? []
            refreshViewerMenus()
        }
    }
}

@_cdecl("frames_mac_presenter_set_menu_id")
public func framesMacPresenterSetMenuId(_ handle: UnsafeMutableRawPointer?, _ id: UInt64) {
    guard let handle else { return }
    let presenter = borrowed(handle, as: Presenter.self)
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            presenter.view?.menuID = id
            refreshViewerMenus()
        }
    }
}

@_cdecl("frames_mac_presenter_set_view_options")
public func framesMacPresenterSetViewOptions(
    _ handle: UnsafeMutableRawPointer?, _ fill: Bool, _ keepAspect: Bool, _ physicalKeys: Bool
) {
    guard let handle else { return }
    let presenter = borrowed(handle, as: Presenter.self)
    DispatchQueue.main.async {
        MainActor.assumeIsolated {
            guard let view = presenter.view else { return }
            view.fill = fill
            view.keepAspect = keepAspect
            view.setPhysicalKeys(physicalKeys)
        }
    }
}

@_cdecl("frames_mac_presenter_set_text_input")
public func framesMacPresenterSetTextInput(
    _ handle: UnsafeMutableRawPointer?, _ context: UnsafeMutableRawPointer?, _ callback: FramesMacTextCallback?
) {
    guard let handle else { return }
    borrowed(handle, as: Presenter.self).input.setText(context: context, callback: callback)
}

@_cdecl("frames_mac_presenter_intercept_close")
public func framesMacPresenterInterceptClose(_ handle: UnsafeMutableRawPointer?, _ on: Bool) {
    guard let handle else { return }
    borrowed(handle, as: Presenter.self).closed.setIntercept(on)
}

@_cdecl("frames_mac_presenter_take_close_request")
public func framesMacPresenterTakeCloseRequest(_ handle: UnsafeMutableRawPointer?) -> Bool {
    guard let handle else { return false }
    return borrowed(handle, as: Presenter.self).closed.takeRequest()
}

@_cdecl("frames_mac_presenter_closed")
public func framesMacPresenterClosed(_ handle: UnsafeMutableRawPointer?) -> Bool {
    guard let handle else { return false }
    return borrowed(handle, as: Presenter.self).closed.isClosed
}

@_cdecl("frames_mac_presenter_new")
public func framesMacPresenterNew(_ width: UInt32, _ height: UInt32, _ window: Bool) -> UnsafeMutableRawPointer? {
    guard let presenter = Presenter(width: Int(width), height: Int(height), windowed: window) else { return nil }
    return retainedHandle(presenter)
}

@_cdecl("frames_mac_presenter_new_popup")
public func framesMacPresenterNewPopup(_ width: UInt32, _ height: UInt32) -> UnsafeMutableRawPointer? {
    guard let presenter = Presenter(width: Int(width), height: Int(height), windowed: true, popup: true) else {
        return nil
    }
    return retainedHandle(presenter)
}

@_cdecl("frames_mac_presenter_attach")
public func framesMacPresenterAttach(
    _ handle: UnsafeMutableRawPointer?, _ parent: UInt64, _ x: Double, _ y: Double, _ width: Double, _ height: Double,
    _ parentWidth: Double, _ parentHeight: Double, _ role: UInt32
) {
    guard let handle else { return }
    borrowed(handle, as: Presenter.self).attach(
        parent: parent, frame: CGRect(x: x, y: y, width: width, height: height),
        parentSize: CGSize(width: parentWidth, height: parentHeight), role: role)
}

@_cdecl("frames_mac_presenter_present")
public func framesMacPresenterPresent(
    _ handle: UnsafeMutableRawPointer?, _ buffer: UnsafeMutableRawPointer?, _ vsync: Bool,
    _ presentedNs: UnsafeMutablePointer<UInt64>?
) -> Int32 {
    guard let handle, let buffer else { return -1 }
    let (status, time) = borrowed(handle, as: Presenter.self).present(borrowedPixelBuffer(buffer), vsync: vsync)
    presentedNs?.pointee = time
    return status
}

@_cdecl("frames_mac_presenter_last_on_glass_ns")
public func framesMacPresenterLastOnGlassNs(_ handle: UnsafeMutableRawPointer?) -> UInt64 {
    guard let handle else { return 0 }
    return borrowed(handle, as: Presenter.self).presentedAt.get()
}

@_cdecl("frames_mac_presenter_refresh_hz")
public func framesMacPresenterRefreshHz(_ handle: UnsafeMutableRawPointer?) -> Double {
    guard let handle else { return 0 }
    return borrowed(handle, as: Presenter.self).refreshHz
}

@_cdecl("frames_mac_presenter_free")
public func framesMacPresenterFree(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    released(handle, as: Presenter.self).close()
}


/// The Rust viewer's input callback, behind a lock so removing it waits for
/// a call in progress (the context can then be freed safely).
final class InputRelay: @unchecked Sendable {
    private let lock = NSLock()
    private var context: UnsafeMutableRawPointer?
    private var callback: FramesMacInputCallback?
    private var textContext: UnsafeMutableRawPointer?
    private var textCallback: FramesMacTextCallback?

    func setText(context: UnsafeMutableRawPointer?, callback: FramesMacTextCallback?) {
        lock.lock()
        textContext = context
        textCallback = callback
        lock.unlock()
    }

    func sendText(_ text: String, at seconds: TimeInterval) {
        lock.lock()
        defer { lock.unlock() }
        guard let textCallback, !text.isEmpty else { return }
        let nanos = seconds > 0 ? UInt64(seconds * 1_000_000_000) : uptimeNanos()
        var text = text
        text.withUTF8 { bytes in textCallback(textContext, bytes.baseAddress, bytes.count, nanos) }
    }

    func set(context: UnsafeMutableRawPointer?, callback: FramesMacInputCallback?) {
        lock.lock()
        self.context = context
        self.callback = callback
        lock.unlock()
    }

    func send(_ kind: Int32, _ a: Int32, _ b: Int32 = 0, _ c: Int32 = 0, _ d: Int32 = 0, at seconds: TimeInterval) {
        lock.lock()
        defer { lock.unlock() }
        guard let callback else { return }
        // NSEvent timestamps are seconds of system uptime.
        let nanos = seconds > 0 ? UInt64(seconds * 1_000_000_000) : uptimeNanos()
        callback(context, UInt32(kind), a, b, c, d, nanos)
    }
}

/// Holds the system pointer still and hidden while relative mouse is on.
@MainActor
protocol PointerLock {
    func lock() -> Bool
    func unlock()
}

/// The real lock: the pointer stops following the mouse (events still carry
/// its motion as deltas) and is hidden.
struct SystemPointerLock: PointerLock {
    func lock() -> Bool {
        guard CGAssociateMouseAndMouseCursorPosition(0) == .success else { return false }
        NSCursor.hide()
        return true
    }

    func unlock() {
        CGAssociateMouseAndMouseCursorPosition(1)
        NSCursor.unhide()
    }
}

/// Leaves the system pointer alone (unattended tests).
struct NoPointerLock: PointerLock {
    func lock() -> Bool { true }
    func unlock() {}
}

/// Lets a notification observer reach the view without capturing it.
private final class WeakInputView: @unchecked Sendable {
    weak var view: InputView?
}

/// The presenter window's content: forwards mouse, wheel, and key events.
final class InputView: NSView {
    let relay: InputRelay
    /// What Control+Option+F11 locks; tests replace it.
    var pointerLock: any PointerLock = SystemPointerLock()
    private var heldLock: (any PointerLock)?
    /// Relative mouse (games): motion goes as deltas, not positions.
    private(set) var relative = false
    /// Sub-point motion not sent yet.
    private var remainder = CGPoint.zero
    private var tracking: NSTrackingArea?
    /// Modifier keys currently down, by key code (flagsChanged reports one
    /// key at a time).
    private var modifiersDown: Set<UInt16> = []
    /// The host's cursor, shown over the video by the view's cursor rect.
    private var hostCursor: NSCursor?
    private var hostShape: CursorShape?
    private var cursorScale: CGFloat = 0
    private var hostCursorVisible = true
    private static let blank = NSCursor(image: NSImage(size: NSSize(width: 1, height: 1)), hotSpot: .zero)
    /// The window's entry in the connection menus (ABI 5).
    var menuID: UInt64 = 0
    weak var hideWindowButton: NSButton?
    weak var connectionMenuButton: NSButton?
    /// Every key goes as a key (terminals); otherwise printable keys go as
    /// text composed here.
    private(set) var physicalKeys = false
    /// Keys whose press went as text (their release is not sent).
    private var textKeys: Set<UInt16> = []
    /// Option keys held here, and those the host was told about.
    private var heldOptions: Set<UInt16> = []
    private var sentOptions: Set<UInt16> = []
    /// The key being interpreted, and an input method's composition.
    private var typing: NSEvent?
    private var marked = NSMutableAttributedString()
    private var lastPoint = NSPoint.zero

    private func clearTyping() {
        textKeys.removeAll()
        heldOptions.removeAll()
        sentOptions.removeAll()
        marked = NSMutableAttributedString()
        inputContext?.discardMarkedText()
    }
    /// Stretch the picture over the view (otherwise it is letterboxed).
    var fill = false {
        didSet {
            layer?.contentsGravity = fill ? .resize : .resizeAspect
            pictureChanged()
        }
    }
    /// The window keeps the picture's shape ("Stretch locally").
    var keepAspect = true {
        didSet {
            guard keepAspect != oldValue, let window else { return }
            if keepAspect {
                let size = pictureSize
                if size.width > 0, size.height > 0 { window.contentAspectRatio = size }
            } else {
                // Clears the aspect ratio (the two are exclusive).
                window.contentResizeIncrements = NSSize(width: 1, height: 1)
            }
        }
    }

    /// The stream's size in pixels.
    var pictureSize: NSSize { (layer as? CAMetalLayer)?.drawableSize ?? bounds.size }

    /// Where the picture is drawn in the view: all of it with `fill`, else
    /// the largest rect of the picture's shape, centred.
    var pictureRect: NSRect {
        let size = pictureSize
        guard !fill, size.width > 0, size.height > 0, bounds.width > 0, bounds.height > 0 else { return bounds }
        let scale = min(bounds.width / size.width, bounds.height / size.height)
        let (w, h) = (size.width * scale, size.height * scale)
        return NSRect(x: bounds.minX + (bounds.width - w) / 2, y: bounds.minY + (bounds.height - h) / 2, width: w, height: h)
    }

    func setHostCursor(_ shape: CursorShape?, visible: Bool?) {
        if let shape {
            hostShape = shape
            cursorScale = 0
        }
        if let visible { hostCursorVisible = visible }
        refreshCursor()
    }

    /// Builds the host cursor at the picture's current scale.
    private func refreshCursor() {
        if let shape = hostShape {
            let picture = pictureSize
            var scale = picture.width > 0 ? pictureRect.width / picture.width : 1
            // Never smaller than a third of its size on the host's screen.
            let backing = window?.backingScaleFactor ?? 2
            scale = max(scale, 1 / (backing * 3))
            if abs(scale - cursorScale) > 0.001, let image = cursorImage(shape, scale: scale) {
                cursorScale = scale
                hostCursor = NSCursor(
                    image: image, hotSpot: NSPoint(x: shape.hotspot.x * scale, y: shape.hotspot.y * scale))
            }
        }
        window?.invalidateCursorRects(for: self)
    }

    /// The view or the picture changed size.
    func pictureChanged() {
        refreshCursor()
        layoutOverlay()
    }

    /// Tells the host when the window is minimized, covered or shown
    /// again, and when it comes to the front or leaves it, so it sends no
    /// frames, fewer frames or every frame (v1). Popups always count as
    /// focused: they show while their window is in use.
    func reportVisibility() {
        guard let window else { return }
        let shown = window.isVisible && !window.isMiniaturized && window.occlusionState.contains(.visible)
        let focused = !window.styleMask.contains(.titled) || window.isKeyWindow
        guard (shown, focused) != reportedVisibility else { return }
        reportedVisibility = (shown, focused)
        relay.send(Int32(FRAMES_MAC_INPUT_VISIBILITY), shown ? 1 : 0, focused ? 1 : 0, at: 0)
    }

    /// The window that blocks this one now (a remote modal dialog or sheet
    /// shown here), as v1 redirected input to it.
    func modalBlocker() -> InputView? {
        let windows = viewerState["windows"] as? [[String: Any]] ?? []
        guard
            let blocker = windows.first(where: {
                ($0["parent"] as? NSNumber)?.uint64Value == menuID && ($0["modal"] as? NSNumber)?.boolValue == true
                    && ($0["shown"] as? NSNumber)?.boolValue == true
            }), let id = (blocker["id"] as? NSNumber)?.uint64Value
        else { return nil }
        return NSApplication.shared.windows.compactMap { $0.contentView as? InputView }.first {
            $0.menuID == id && $0.window?.isVisible == true
        }
    }

    /// Input for a blocked window goes nowhere; its blocker comes forward
    /// instead (it may be nested: the innermost blocker). True if blocked.
    func redirectToBlocker() -> Bool {
        guard var blocker = modalBlocker() else { return false }
        var depth = 0
        while let inner = blocker.modalBlocker(), depth < 8 {
            blocker = inner
            depth += 1
        }
        if blocker.window?.styleMask.contains(.titled) == true {
            blocker.window?.makeKeyAndOrderFront(nil)
        } else {
            blocker.window?.orderFront(nil)
        }
        NSSound.beep()
        return true
    }

    /// This window is closing while in front: its owner (the window a
    /// dialog belongs to) comes forward, as v1.
    func focusOwner() {
        guard window?.isKeyWindow == true else { return }
        let windows = viewerState["windows"] as? [[String: Any]] ?? []
        guard
            let entry = windows.first(where: { ($0["id"] as? NSNumber)?.uint64Value == menuID }),
            let owner = (entry["parent"] as? NSNumber)?.uint64Value
        else { return }
        let view = NSApplication.shared.windows.compactMap { $0.contentView as? InputView }.first {
            $0.menuID == owner && $0 !== self && $0.window?.isVisible == true
        }
        view?.window?.makeKeyAndOrderFront(nil)
    }

    /// The user resized the window: its size in pixels, for "Resize remote app".
    func userResized() {
        let scale = window?.backingScaleFactor ?? 2
        let (w, h) = (Int32((bounds.width * scale).rounded()), Int32((bounds.height * scale).rounded()))
        relay.send(Int32(FRAMES_MAC_INPUT_SIZE), w, h, at: ProcessInfo.processInfo.systemUptime)
    }

    /// The cursor the view shows now (for tests).
    var shownCursor: NSCursor? {
        guard hostCursor != nil || !hostCursorVisible else { return nil }
        return hostCursorVisible ? hostCursor : InputView.blank
    }

    override func resetCursorRects() {
        if let cursor = shownCursor { addCursorRect(pictureRect, cursor: cursor) }
    }

    /// Where dropped files go.
    var drops: DroppedFiles?
    /// The shared application's menus (from the host), for the menu bar.
    var hostMenus: [[String: Any]] = []
    /// The debug overlay's last content and its layer (`Overlay.swift`).
    var overlayContent: OverlayContent?
    var overlayLayer: CALayer?
    /// What the host was last told about this window's visibility.
    private var reportedVisibility: (shown: Bool, focused: Bool) = (true, true)

    init(frame: NSRect, relay: InputRelay) {
        self.relay = relay
        super.init(frame: frame)
    }

    override func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation {
        fileURLs(sender).isEmpty ? [] : .copy
    }

    override func performDragOperation(_ sender: any NSDraggingInfo) -> Bool {
        let files = fileURLs(sender)
        guard !files.isEmpty else { return false }
        // The host drops them where they were dropped here (v1).
        drops?.add(files.map(\.path), at: normalized(convert(sender.draggingLocation, from: nil)))
        return true
    }

    private func fileURLs(_ sender: any NSDraggingInfo) -> [URL] {
        let options: [NSPasteboard.ReadingOptionKey: Any] = [.urlReadingFileURLsOnly: true]
        return sender.draggingPasteboard.readObjects(forClasses: [NSURL.self], options: options) as? [URL] ?? []
    }

    required init?(coder: NSCoder) { nil }

    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let tracking { removeTrackingArea(tracking) }
        // A popup never becomes key; it follows the pointer anyway.
        let popup = window.map { !$0.styleMask.contains(.titled) } ?? false
        let area = NSTrackingArea(
            rect: bounds, options: [.mouseMoved, popup ? .activeAlways : .activeInKeyWindow, .inVisibleRect],
            owner: self, userInfo: nil)
        addTrackingArea(area)
        tracking = area
    }

    /// Normalized position over the picture (where it is drawn), origin top
    /// left; outside it, clamped to its edge.
    private func position(_ event: NSEvent) -> (Int32, Int32) {
        normalized(convert(event.locationInWindow, from: nil))
    }

    func normalized(_ point: NSPoint) -> (Int32, Int32) {
        let rect = pictureRect
        let width = max(rect.width, 1), height = max(rect.height, 1)
        let x = min(max((point.x - rect.minX) / width, 0), 1)
        let y = min(max(1 - (point.y - rect.minY) / height, 0), 1)
        return (Int32((x * 65535).rounded()), Int32((y * 65535).rounded()))
    }

    /// Turns relative mouse on or off and tells the host.
    func setRelative(_ on: Bool, lockPointer: Bool = true, at seconds: TimeInterval = 0) {
        guard on != relative else { return }
        if on {
            let lock: any PointerLock = lockPointer ? pointerLock : NoPointerLock()
            guard lock.lock() else { return }
            heldLock = lock
            remainder = .zero
        } else {
            heldLock?.unlock()
            heldLock = nil
        }
        relative = on
        relay.send(Int32(FRAMES_MAC_INPUT_MODE), on ? 1 : 0, at: seconds)
    }

    private func motion(dx: CGFloat, dy: CGFloat, at seconds: TimeInterval) {
        let x = dx + remainder.x, y = dy + remainder.y
        let (sx, sy) = (x.rounded(.towardZero), y.rounded(.towardZero))
        remainder = CGPoint(x: x - sx, y: y - sy)
        if sx != 0 || sy != 0 {
            relay.send(Int32(FRAMES_MAC_INPUT_MOTION), Int32(sx), Int32(sy), at: seconds)
        }
    }

    private func move(_ event: NSEvent) {
        if relative {
            // NSEvent deltas are points, positive right and down.
            motion(dx: event.deltaX, dy: event.deltaY, at: event.timestamp)
            return
        }
        let (x, y) = position(event)
        relay.send(Int32(FRAMES_MAC_INPUT_MOVE), x, y, at: event.timestamp)
    }

    private func button(_ event: NSEvent, _ button: Int32, down: Bool) {
        if down && redirectToBlocker() { return }
        if down { sendHeldOptions(at: event.timestamp) }
        lastPoint = convert(event.locationInWindow, from: nil)
        let (x, y) = position(event)
        relay.send(Int32(FRAMES_MAC_INPUT_BUTTON), button, down ? 1 : 0, x, y, at: event.timestamp)
    }

    private func otherButton(_ event: NSEvent) -> Int32 {
        switch event.buttonNumber {
        case 3: return 3
        case 4: return 4
        default: return 2
        }
    }

    override func mouseMoved(with event: NSEvent) { move(event) }
    override func mouseDragged(with event: NSEvent) { move(event) }
    override func rightMouseDragged(with event: NSEvent) { move(event) }
    override func otherMouseDragged(with event: NSEvent) { move(event) }
    override func mouseDown(with event: NSEvent) { button(event, 0, down: true) }
    override func mouseUp(with event: NSEvent) { button(event, 0, down: false) }
    override func rightMouseDown(with event: NSEvent) { button(event, 1, down: true) }
    override func rightMouseUp(with event: NSEvent) { button(event, 1, down: false) }
    override func otherMouseDown(with event: NSEvent) { button(event, otherButton(event), down: true) }
    override func otherMouseUp(with event: NSEvent) { button(event, otherButton(event), down: false) }

    override func scrollWheel(with event: NSEvent) {
        // Lines (wheel mice) are notches; trackpad deltas are points, about a
        // tenth of a notch each.
        let scale: CGFloat = event.hasPreciseScrollingDeltas ? 12 : 120
        let dx = Int32((event.scrollingDeltaX * scale).rounded())
        let dy = Int32((event.scrollingDeltaY * scale).rounded())
        if dx != 0 || dy != 0 {
            relay.send(Int32(FRAMES_MAC_INPUT_WHEEL), dx, dy, at: event.timestamp)
        }
    }

    /// Gives the pointer and keys back (a menu opens, for example).
    func releaseKeys() {
        setRelative(false)
        modifiersDown.removeAll()
        clearTyping()
        relay.send(Int32(FRAMES_MAC_INPUT_FOCUS_LOST), 0, at: 0)
    }

    /// Gives the pointer and keys back and minimizes the window.
    func releaseInput(at timestamp: TimeInterval) {
        setRelative(false, at: timestamp)
        modifiersDown.removeAll()
        relay.send(Int32(FRAMES_MAC_INPUT_FOCUS_LOST), 0, at: timestamp)
        window?.miniaturize(nil)
    }

    override func keyDown(with event: NSEvent) {
        // Control+Option+F12 (kVK_F12 = 111): release remote input and
        // minimize, as in v1; the shortcut never reaches the host.
        if event.keyCode == 111, event.modifierFlags.contains([.control, .option]) {
            releaseInput(at: event.timestamp)
            return
        }
        // Control+Option+F10 (kVK_F10 = 109): live statistics on or off, as
        // v1's debug overlay shortcut; never reaches the host.
        if event.keyCode == 109, event.modifierFlags.contains([.control, .option]) {
            relay.send(Int32(FRAMES_MAC_INPUT_STATS), 0, at: event.timestamp)
            return
        }
        // Control+Option+F11 (kVK_F11 = 103): relative mouse on or off, as in
        // v1; the shortcut never reaches the host.
        if event.keyCode == 103, event.modifierFlags.contains([.control, .option]) {
            setRelative(!relative, at: event.timestamp)
            return
        }
        if redirectToBlocker() { return }
        if !physicalKeys && !relative && wantsText(event) {
            // Composed here (layout, dead keys, input methods); the text
            // goes to the host, the key's release does not.
            textKeys.insert(event.keyCode)
            typing = event
            interpretKeyEvents([event])
            typing = nil
            return
        }
        sendKey(event.keyCode, down: true, at: event.timestamp)
    }

    override func keyUp(with event: NSEvent) {
        if textKeys.remove(event.keyCode) != nil { return }
        sendKey(event.keyCode, down: false, at: event.timestamp)
    }

    override func flagsChanged(with event: NSEvent) {
        let code = event.keyCode
        let down = !modifiersDown.contains(code)
        if down { modifiersDown.insert(code) } else { modifiersDown.remove(code) }
        if !physicalKeys {
            // Caps Lock is already in the typed text.
            if code == 57 { return }
            // Option composes characters here; the host gets it only with a
            // key that is not text (Option+arrow) or a click.
            if code == 58 || code == 61 {
                if down {
                    heldOptions.insert(code)
                } else {
                    heldOptions.remove(code)
                    if sentOptions.remove(code) != nil {
                        relay.send(Int32(FRAMES_MAC_INPUT_KEY), Int32(code), 0, at: event.timestamp)
                    }
                }
                return
            }
        }
        relay.send(Int32(FRAMES_MAC_INPUT_KEY), Int32(code), down ? 1 : 0, at: event.timestamp)
    }

    /// A key as a key, Option first when it is held but not yet sent.
    func sendKey(_ code: UInt16, down: Bool, at timestamp: TimeInterval) {
        if down { sendHeldOptions(at: timestamp) }
        relay.send(Int32(FRAMES_MAC_INPUT_KEY), Int32(code), down ? 1 : 0, at: timestamp)
    }

    func sendHeldOptions(at timestamp: TimeInterval) {
        for code in heldOptions.subtracting(sentOptions) {
            sentOptions.insert(code)
            relay.send(Int32(FRAMES_MAC_INPUT_KEY), Int32(code), 1, at: timestamp)
        }
    }

    /// Whether `event` types text: no Command or Control, and printable
    /// characters (or none, a dead key), or an input method is composing.
    private func wantsText(_ event: NSEvent) -> Bool {
        let flags = event.modifierFlags
        if flags.contains(.command) || flags.contains(.control) { return false }
        if hasMarkedText() { return true }
        guard let characters = event.characters else { return false }
        if characters.isEmpty { return true }
        return characters.unicodeScalars.allSatisfy { scalar in
            scalar.value >= 0x20 && scalar.value != 0x7F && !(0xF700...0xF8FF).contains(scalar.value)
        }
    }

    func setPhysicalKeys(_ on: Bool) {
        guard on != physicalKeys else { return }
        releaseKeys()
        physicalKeys = on
    }

    override func resignFirstResponder() -> Bool {
        setRelative(false)
        modifiersDown.removeAll()
        clearTyping()
        relay.send(Int32(FRAMES_MAC_INPUT_FOCUS_LOST), 0, at: 0)
        return super.resignFirstResponder()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        // Leaving the window gives the pointer back and releases input.
        let weak = WeakInputView()
        weak.view = self
        NotificationCenter.default.addObserver(
            forName: NSWindow.didResignKeyNotification, object: window, queue: .main
        ) { [relay] _ in
            MainActor.assumeIsolated { weak.view?.setRelative(false) }
            relay.send(Int32(FRAMES_MAC_INPUT_FOCUS_LOST), 1, at: 0)
        }
        // The host raises its window and reads its menus (v1's focus).
        NotificationCenter.default.addObserver(
            forName: NSWindow.didBecomeKeyNotification, object: window, queue: .main
        ) { [relay] _ in
            relay.send(Int32(FRAMES_MAC_INPUT_FOCUS_GAINED), 0, at: 0)
            MainActor.assumeIsolated { weak.view?.reportVisibility() }
        }
        // Minimized, covered or shown again; in front or not (the host
        // paces its frames by it).
        for name in [
            NSWindow.didChangeOcclusionStateNotification, NSWindow.didMiniaturizeNotification,
            NSWindow.didDeminiaturizeNotification, NSWindow.didResignKeyNotification,
        ] {
            NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { _ in
                MainActor.assumeIsolated { weak.view?.reportVisibility() }
            }
        }
    }

    /// One step of generated input: a circling pointer every step, and a
    /// click, a wheel notch, and an "A" key press every 125th.
    func synthetic(step: UInt64) {
        let angle = Double(step) * 0.05
        let point = NSPoint(
            x: bounds.midX + bounds.width * 0.3 * cos(angle), y: bounds.midY + bounds.height * 0.3 * sin(angle))
        let location = convert(point, to: nil)
        let number = window?.windowNumber ?? 0
        let now = ProcessInfo.processInfo.systemUptime
        func mouse(_ type: NSEvent.EventType) -> NSEvent? {
            NSEvent.mouseEvent(
                with: type, location: location, modifierFlags: [], timestamp: now, windowNumber: number,
                context: nil, eventNumber: 0, clickCount: 1, pressure: type == .leftMouseDown ? 1 : 0)
        }
        // Alternates a plain and an accented character (text input checks).
        let typed = step % 250 == 0 ? "é" : "a"
        func key(_ type: NSEvent.EventType) -> NSEvent? {
            NSEvent.keyEvent(
                with: type, location: location, modifierFlags: [], timestamp: now, windowNumber: number,
                context: nil, characters: typed, charactersIgnoringModifiers: typed, isARepeat: false, keyCode: 0)
        }
        if relative {
            // Generated events carry no deltas: send the motion directly,
            // 3 right and 1 up per step.
            motion(dx: 3, dy: -1, at: now)
        } else if let event = mouse(.mouseMoved) {
            mouseMoved(with: event)
        }
        guard step % 125 == 0 else { return }
        if let event = mouse(.leftMouseDown) { mouseDown(with: event) }
        if let event = mouse(.leftMouseUp) { mouseUp(with: event) }
        relay.send(Int32(FRAMES_MAC_INPUT_WHEEL), 0, 120, at: now)
        if let event = key(.keyDown) { keyDown(with: event) }
        if let event = key(.keyUp) { keyUp(with: event) }
    }
}

// MARK: Text input (ABI 5)

extension InputView: @preconcurrency NSTextInputClient {
    func insertText(_ string: Any, replacementRange: NSRange) {
        let text = (string as? NSAttributedString)?.string ?? (string as? String) ?? ""
        marked = NSMutableAttributedString()
        relay.sendText(text, at: typing?.timestamp ?? 0)
    }

    override func doCommand(by selector: Selector) {
        // The key means no text here (Return, an arrow in a composition's
        // absence): it goes as a key after all.
        guard let typing else { return }
        textKeys.remove(typing.keyCode)
        sendKey(typing.keyCode, down: true, at: typing.timestamp)
    }

    func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange: NSRange) {
        if let attributed = string as? NSAttributedString {
            marked = NSMutableAttributedString(attributedString: attributed)
        } else {
            marked = NSMutableAttributedString(string: string as? String ?? "")
        }
    }

    func unmarkText() {
        marked = NSMutableAttributedString()
    }

    func selectedRange() -> NSRange { NSRange(location: NSNotFound, length: 0) }

    func markedRange() -> NSRange {
        marked.length > 0 ? NSRange(location: 0, length: marked.length) : NSRange(location: NSNotFound, length: 0)
    }

    func hasMarkedText() -> Bool { marked.length > 0 }

    func attributedSubstring(forProposedRange range: NSRange, actualRange: NSRangePointer?) -> NSAttributedString? {
        nil
    }

    func validAttributesForMarkedText() -> [NSAttributedString.Key] { [] }

    /// Candidate windows open where the user last clicked.
    func firstRect(forCharacterRange range: NSRange, actualRange: NSRangePointer?) -> NSRect {
        let local = NSRect(x: lastPoint.x, y: lastPoint.y - 20, width: 1, height: 20)
        guard let window else { return local }
        return window.convertToScreen(convert(local, to: nil))
    }

    func characterIndex(for point: NSPoint) -> Int { NSNotFound }
}

@_cdecl("frames_mac_presenter_set_input")
public func framesMacPresenterSetInput(
    _ handle: UnsafeMutableRawPointer?, _ context: UnsafeMutableRawPointer?, _ callback: FramesMacInputCallback?
) {
    guard let handle else { return }
    borrowed(handle, as: Presenter.self).input.set(context: context, callback: callback)
}

@_cdecl("frames_mac_presenter_post_synthetic_input")
public func framesMacPresenterPostSyntheticInput(_ handle: UnsafeMutableRawPointer?, _ step: UInt64) {
    guard let handle else { return }
    borrowed(handle, as: Presenter.self).postSyntheticInput(step: step)
}

@_cdecl("frames_mac_presenter_set_relative")
public func framesMacPresenterSetRelative(_ handle: UnsafeMutableRawPointer?, _ relative: Bool, _ lockPointer: Bool) {
    guard let handle else { return }
    borrowed(handle, as: Presenter.self).setRelative(relative, lockPointer: lockPointer)
}

@_cdecl("frames_mac_presenter_set_cursor")
public func framesMacPresenterSetCursor(
    _ handle: UnsafeMutableRawPointer?, _ bgra: UnsafePointer<UInt8>?, _ width: UInt32, _ height: UInt32,
    _ hotspotX: UInt32, _ hotspotY: UInt32
) {
    guard let handle, let bgra, width > 0, height > 0, width <= 256, height <= 256 else { return }
    let (w, h) = (Int(width), Int(height))
    let shape = CursorShape(
        bgra: Data(bytes: bgra, count: w * h * 4), width: w, height: h,
        hotspot: NSPoint(x: Int(hotspotX), y: Int(hotspotY)))
    borrowed(handle, as: Presenter.self).setCursor(shape, visible: nil)
}

@_cdecl("frames_mac_presenter_set_cursor_visible")
public func framesMacPresenterSetCursorVisible(_ handle: UnsafeMutableRawPointer?, _ visible: Bool) {
    guard let handle else { return }
    borrowed(handle, as: Presenter.self).setCursor(nil, visible: visible)
}

@_cdecl("frames_mac_presenter_set_title")
public func framesMacPresenterSetTitle(_ handle: UnsafeMutableRawPointer?, _ utf8: UnsafePointer<UInt8>?, _ length: Int) {
    guard let handle, let utf8 else { return }
    let title = String(decoding: UnsafeBufferPointer(start: utf8, count: length), as: UTF8.self)
    borrowed(handle, as: Presenter.self).setTitle(title)
}
