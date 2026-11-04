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
        let makeWindow = { @MainActor () -> (NSWindow?, Double) in
            let hz = Double(NSScreen.main?.maximumFramesPerSecond ?? 60)
            let scale = NSScreen.main?.backingScaleFactor ?? 2
            let frame = NSRect(x: 80, y: 80, width: CGFloat(width) / scale, height: CGFloat(height) / scale)
            let window = NSWindow(
                contentRect: frame, styleMask: [.titled, .closable, .resizable], backing: .buffered, defer: false)
            window.title = "Frames presenter"
            window.isReleasedWhenClosed = false
            let view = NSView(frame: frame)
            view.wantsLayer = true
            view.layer = layer
            window.contentView = view
            window.makeKeyAndOrderFront(nil)
            return (window, hz)
        }
        if Thread.isMainThread {
            (window, refresh) = MainActor.assumeIsolated { makeWindow() }
        } else {
            (window, refresh) = DispatchQueue.main.sync { MainActor.assumeIsolated { makeWindow() } }
        }
        refreshHz = refresh
    }

    func close() {
        guard let window else { return }
        self.window = nil
        let close = { @MainActor in window.close() }
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
