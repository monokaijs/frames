import AppKit
import CFramesMac
import Foundation
import QuartzCore

// The debug overlay (ABI 6), as v1's: a panel at the picture's top left
// (8 points in), with the stream's state as lines, a frame graph (each
// frame's interval as a bar, its latency as a line, the frame budget
// dashed) and a bitrate graph. frames-next `overlay::Overlay` sends it as
// JSON a few times a second; it is drawn into an image on a layer over
// the video.

/// One redraw of the panel.
struct OverlayContent {
    struct Line {
        let text: String
        let level: String
    }
    var lines: [Line] = []
    /// [interval, latency] in microseconds, oldest first.
    var frames: [(Double, Double)] = []
    var budget: Double = 16_667
    var bitrateKbps: [Double] = []

    init?(json: Data) {
        guard let object = (try? JSONSerialization.jsonObject(with: json)) as? [String: Any] else { return nil }
        lines = (object["lines"] as? [[String: Any]] ?? []).map {
            Line(text: $0["text"] as? String ?? "", level: $0["level"] as? String ?? "normal")
        }
        frames = (object["frames"] as? [[NSNumber]] ?? []).compactMap { pair in
            pair.count == 2 ? (pair[0].doubleValue, pair[1].doubleValue) : nil
        }
        budget = (object["frame_budget_us"] as? NSNumber)?.doubleValue ?? 16_667
        bitrateKbps = (object["bitrate_kbps"] as? [NSNumber] ?? []).map(\.doubleValue)
    }
}

enum OverlayLayout {
    static let width: CGFloat = 400
    static let margin: CGFloat = 8
    static let padding: CGFloat = 8
    static let lineHeight: CGFloat = 14
    static let frameGraph: CGFloat = 56
    static let bitrateGraph: CGFloat = 36
    static let gap: CGFloat = 6
    /// Windows smaller than this show no overlay (as v1).
    static let minimumView = NSSize(width: 360, height: 260)

    static func height(lines: Int) -> CGFloat {
        padding * 2 + CGFloat(lines) * lineHeight + gap + frameGraph + gap + bitrateGraph
    }
}

/// Draws the panel into an image of `size` points at `scale`.
func renderOverlay(_ content: OverlayContent, scale: CGFloat) -> CGImage? {
    let size = NSSize(width: OverlayLayout.width, height: OverlayLayout.height(lines: content.lines.count))
    let pixels = (Int(size.width * scale), Int(size.height * scale))
    guard
        let context = CGContext(
            data: nil, width: pixels.0, height: pixels.1, bitsPerComponent: 8, bytesPerRow: 0,
            space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue)
    else { return nil }
    // Top-left origin, in points.
    context.translateBy(x: 0, y: CGFloat(pixels.1))
    context.scaleBy(x: scale, y: -scale)
    let graphics = NSGraphicsContext(cgContext: context, flipped: true)
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = graphics
    defer { NSGraphicsContext.restoreGraphicsState() }

    let panel = NSBezierPath(roundedRect: NSRect(origin: .zero, size: size), xRadius: 6, yRadius: 6)
    NSColor(white: 0, alpha: 0.62).setFill()
    panel.fill()

    let font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
    func color(_ level: String) -> NSColor {
        switch level {
        case "warn": return NSColor(red: 1, green: 0.8, blue: 0.3, alpha: 1)
        case "bad": return NSColor(red: 1, green: 0.42, blue: 0.42, alpha: 1)
        case "dim": return NSColor(white: 0.6, alpha: 1)
        default: return NSColor(white: 0.93, alpha: 1)
        }
    }
    var y = OverlayLayout.padding
    let x = OverlayLayout.padding
    for line in content.lines {
        NSAttributedString(string: line.text, attributes: [.font: font, .foregroundColor: color(line.level)])
            .draw(at: NSPoint(x: x, y: y))
        y += OverlayLayout.lineHeight
    }
    let graphWidth = size.width - OverlayLayout.padding * 2

    // Frames: intervals as bars, latency as a line, the budget dashed.
    y += OverlayLayout.gap
    let frameRect = NSRect(x: x, y: y, width: graphWidth, height: OverlayLayout.frameGraph)
    NSColor(white: 1, alpha: 0.06).setFill()
    frameRect.fill()
    let intervals = content.frames.map(\.0)
    let latencies = content.frames.map(\.1)
    let top = max(content.budget * 3, min((intervals + latencies).max() ?? 0, 250_000))
    func frameY(_ value: Double) -> CGFloat { frameRect.maxY - CGFloat(min(value / top, 1)) * frameRect.height }
    let slot = graphWidth / 240
    let firstSlot = CGFloat(240 - content.frames.count)
    NSColor(red: 0.45, green: 0.62, blue: 1, alpha: 0.75).setFill()
    for (index, interval) in intervals.enumerated() where interval > 0 {
        let barX = frameRect.minX + (firstSlot + CGFloat(index)) * slot
        NSRect(x: barX, y: frameY(interval), width: max(slot - 0.5, 0.5), height: frameRect.maxY - frameY(interval))
            .fill()
    }
    let latency = NSBezierPath()
    for (index, value) in latencies.enumerated() where value > 0 {
        let point = NSPoint(x: frameRect.minX + (firstSlot + CGFloat(index) + 0.5) * slot, y: frameY(value))
        if latency.isEmpty { latency.move(to: point) } else { latency.line(to: point) }
    }
    NSColor(red: 0.4, green: 0.9, blue: 0.5, alpha: 1).setStroke()
    latency.lineWidth = 1.2
    latency.stroke()
    let budget = NSBezierPath()
    budget.move(to: NSPoint(x: frameRect.minX, y: frameY(content.budget)))
    budget.line(to: NSPoint(x: frameRect.maxX, y: frameY(content.budget)))
    budget.setLineDash([3, 3], count: 2, phase: 0)
    NSColor(red: 1, green: 0.8, blue: 0.3, alpha: 0.8).setStroke()
    budget.stroke()

    // Bitrate: the last 150 samples, scaled to their peak.
    y += OverlayLayout.frameGraph + OverlayLayout.gap
    let rateRect = NSRect(x: x, y: y, width: graphWidth, height: OverlayLayout.bitrateGraph)
    NSColor(white: 1, alpha: 0.06).setFill()
    rateRect.fill()
    let peak = max(content.bitrateKbps.max() ?? 0, 1000)
    let step = graphWidth / 149
    let firstRate = CGFloat(150 - content.bitrateKbps.count)
    let rate = NSBezierPath()
    for (index, kbps) in content.bitrateKbps.enumerated() {
        let point = NSPoint(
            x: rateRect.minX + (firstRate + CGFloat(index)) * step,
            y: rateRect.maxY - CGFloat(kbps / peak) * rateRect.height)
        if rate.isEmpty { rate.move(to: point) } else { rate.line(to: point) }
    }
    NSColor(red: 0.85, green: 0.6, blue: 1, alpha: 1).setStroke()
    rate.lineWidth = 1.2
    rate.stroke()
    NSAttributedString(
        string: String(format: "%.1f Mb/s peak", peak / 1000),
        attributes: [.font: NSFont.monospacedSystemFont(ofSize: 9, weight: .regular), .foregroundColor: color("dim")]
    ).draw(at: NSPoint(x: rateRect.minX + 3, y: rateRect.minY + 1))
    return context.makeImage()
}

extension InputView {
    /// Shows (or with `nil` hides) the overlay over the picture.
    func showOverlay(_ content: OverlayContent?) {
        overlayContent = content
        layoutOverlay()
    }

    /// Places the overlay at the picture's top left, redrawn for the
    /// window's scale; hidden on popups and small windows.
    func layoutOverlay() {
        guard let host = layer else { return }
        let popup = window.map { !$0.styleMask.contains(.titled) } ?? true
        let picture = pictureRect
        guard let content = overlayContent, !popup, bounds.width >= OverlayLayout.minimumView.width,
            bounds.height >= OverlayLayout.minimumView.height,
            picture.height >= OverlayLayout.height(lines: content.lines.count) + OverlayLayout.margin * 2,
            picture.width >= OverlayLayout.width + OverlayLayout.margin * 2,
            let image = renderOverlay(content, scale: window?.backingScaleFactor ?? 2)
        else {
            overlayLayer?.removeFromSuperlayer()
            overlayLayer = nil
            return
        }
        let panel = overlayLayer ?? CALayer()
        if overlayLayer == nil {
            overlayLayer = panel
            host.addSublayer(panel)
        }
        let height = OverlayLayout.height(lines: content.lines.count)
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        panel.contents = image
        panel.contentsScale = window?.backingScaleFactor ?? 2
        // The view is not flipped: the picture's top is its maxY.
        panel.frame = NSRect(
            x: picture.minX + OverlayLayout.margin, y: picture.maxY - OverlayLayout.margin - height,
            width: OverlayLayout.width, height: height)
        CATransaction.commit()
    }
}

@_cdecl("frames_mac_presenter_set_overlay")
public func framesMacPresenterSetOverlay(_ handle: UnsafeMutableRawPointer?, _ utf8: UnsafePointer<UInt8>?, _ length: Int) {
    guard let handle, let view = borrowed(handle, as: Presenter.self).view else { return }
    let content = utf8.flatMap { length > 0 ? OverlayContent(json: Data(bytes: $0, count: length)) : nil }
    let box = OverlayBox(content)
    DispatchQueue.main.async { MainActor.assumeIsolated { view.showOverlay(box.content) } }
}

/// Carries a parsed overlay to the main thread.
private final class OverlayBox: @unchecked Sendable {
    let content: OverlayContent?
    init(_ content: OverlayContent?) { self.content = content }
}
