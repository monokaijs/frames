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

    init?(width: Int, height: Int, windowed: Bool) {
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
        let makeWindow = { @MainActor () -> (NSWindow?, Double, InputView) in
            let hz = Double(NSScreen.main?.maximumFramesPerSecond ?? 60)
            let scale = NSScreen.main?.backingScaleFactor ?? 2
            let frame = NSRect(x: 80, y: 80, width: CGFloat(width) / scale, height: CGFloat(height) / scale)
            let window = NSWindow(
                contentRect: frame, styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
            window.title = "Frames presenter"
            window.isReleasedWhenClosed = false
            let view = InputView(frame: frame, relay: input)
            view.wantsLayer = true
            view.layer = layer
            window.contentView = view
            window.acceptsMouseMovedEvents = true
            window.makeKeyAndOrderFront(nil)
            window.makeFirstResponder(view)
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
            inputView.setHostCursor(shape.flatMap(cursorImage), hotspot: shape?.hotspot ?? .zero, visible: visible)
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

@MainActor
private func cursorImage(_ shape: CursorShape) -> NSImage? {
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
    let image = NSImage(size: NSSize(width: w, height: h))
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

@_cdecl("frames_mac_presenter_new")
public func framesMacPresenterNew(_ width: UInt32, _ height: UInt32, _ window: Bool) -> UnsafeMutableRawPointer? {
    guard let presenter = Presenter(width: Int(width), height: Int(height), windowed: window) else { return nil }
    return retainedHandle(presenter)
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
    private let relay: InputRelay
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
    private var hostCursorVisible = true
    private static let blank = NSCursor(image: NSImage(size: NSSize(width: 1, height: 1)), hotSpot: .zero)

    func setHostCursor(_ image: NSImage?, hotspot: NSPoint, visible: Bool?) {
        if let image { hostCursor = NSCursor(image: image, hotSpot: hotspot) }
        if let visible { hostCursorVisible = visible }
        window?.invalidateCursorRects(for: self)
    }

    /// The cursor the view shows now (for tests).
    var shownCursor: NSCursor? {
        guard hostCursor != nil || !hostCursorVisible else { return nil }
        return hostCursorVisible ? hostCursor : InputView.blank
    }

    override func resetCursorRects() {
        if let cursor = shownCursor { addCursorRect(bounds, cursor: cursor) }
    }

    init(frame: NSRect, relay: InputRelay) {
        self.relay = relay
        super.init(frame: frame)
    }

    required init?(coder: NSCoder) { nil }

    override var acceptsFirstResponder: Bool { true }
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let tracking { removeTrackingArea(tracking) }
        let area = NSTrackingArea(
            rect: bounds, options: [.mouseMoved, .activeInKeyWindow, .inVisibleRect], owner: self, userInfo: nil)
        addTrackingArea(area)
        tracking = area
    }

    /// Normalized position, origin top left.
    private func position(_ event: NSEvent) -> (Int32, Int32) {
        let point = convert(event.locationInWindow, from: nil)
        let width = max(bounds.width, 1), height = max(bounds.height, 1)
        let x = min(max(point.x / width, 0), 1), y = min(max(1 - point.y / height, 0), 1)
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

    override func keyDown(with event: NSEvent) {
        // Control+Option+F12 (kVK_F12 = 111): release remote input and
        // minimize, as in v1; the shortcut never reaches the host.
        if event.keyCode == 111, event.modifierFlags.contains([.control, .option]) {
            setRelative(false, at: event.timestamp)
            modifiersDown.removeAll()
            relay.send(Int32(FRAMES_MAC_INPUT_FOCUS_LOST), 0, at: event.timestamp)
            window?.miniaturize(nil)
            return
        }
        // Control+Option+F11 (kVK_F11 = 103): relative mouse on or off, as in
        // v1; the shortcut never reaches the host.
        if event.keyCode == 103, event.modifierFlags.contains([.control, .option]) {
            setRelative(!relative, at: event.timestamp)
            return
        }
        relay.send(Int32(FRAMES_MAC_INPUT_KEY), Int32(event.keyCode), 1, at: event.timestamp)
    }

    override func keyUp(with event: NSEvent) {
        relay.send(Int32(FRAMES_MAC_INPUT_KEY), Int32(event.keyCode), 0, at: event.timestamp)
    }

    override func flagsChanged(with event: NSEvent) {
        let code = event.keyCode
        let down = !modifiersDown.contains(code)
        if down { modifiersDown.insert(code) } else { modifiersDown.remove(code) }
        relay.send(Int32(FRAMES_MAC_INPUT_KEY), Int32(code), down ? 1 : 0, at: event.timestamp)
    }

    override func resignFirstResponder() -> Bool {
        setRelative(false)
        modifiersDown.removeAll()
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
            relay.send(Int32(FRAMES_MAC_INPUT_FOCUS_LOST), 0, at: 0)
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
        func key(_ type: NSEvent.EventType) -> NSEvent? {
            NSEvent.keyEvent(
                with: type, location: location, modifierFlags: [], timestamp: now, windowNumber: number,
                context: nil, characters: "a", charactersIgnoringModifiers: "a", isARepeat: false, keyCode: 0)
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
