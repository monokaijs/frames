import AppKit

// Media diagnostics drawn over a remote window's video. The session formats
// all text; this view only lays it out and draws the frame-time and bitrate
// graphs. It never takes mouse or keyboard input.
@MainActor final class DebugOverlayView: NSView {
    struct Line { let text: String; let level: String }
    struct Snapshot {
        var lines: [Line] = []
        var frames: [(interval: Double, latency: Double)] = []
        var budget: Double = 16_667
        var bitrate: [Double] = []
        var sampleMs: Double = 200
        init() {}
        init(_ data: [String: Any]) {
            lines = (data["lines"] as? [[String: Any]] ?? []).prefix(24).map { Line(text: $0["text"] as? String ?? "", level: $0["level"] as? String ?? "normal") }
            frames = (data["frames"] as? [[NSNumber]] ?? []).suffix(Self.frameCapacity).map { ($0.first?.doubleValue ?? 0, $0.count > 1 ? $0[1].doubleValue : 0) }
            budget = max(1000, (data["frame_budget_us"] as? NSNumber)?.doubleValue ?? 16_667)
            bitrate = (data["bitrate_kbps"] as? [NSNumber] ?? []).suffix(Self.rateCapacity).map(\.doubleValue)
            sampleMs = max(1, (data["sample_ms"] as? NSNumber)?.doubleValue ?? 200)
        }
        static let frameCapacity = 240, rateCapacity = 150
    }
    static let minimumHost = NSSize(width: 360, height: 260)
    private static let font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
    private static let small = NSFont.monospacedSystemFont(ofSize: 9, weight: .regular)
    private static let padding: CGFloat = 8, lineHeight: CGFloat = 14, frameGraph: CGFloat = 56, rateGraph: CGFloat = 34, gap: CGFloat = 6
    var snapshot = Snapshot() { didSet { needsDisplay = true } }
    override var isFlipped: Bool { true }
    override func hitTest(_ point: NSPoint) -> NSView? { nil }
    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layerContentsRedrawPolicy = .onSetNeedsDisplay
        layer?.cornerRadius = 6
        layer?.masksToBounds = true
        isHidden = true
        setAccessibilityElement(false)
    }
    required init?(coder: NSCoder) { fatalError("not used") }

    /// Panel size for the current snapshot, limited to `available` width.
    func preferredSize(available: CGFloat) -> NSSize {
        let attributes: [NSAttributedString.Key: Any] = [.font: Self.font]
        let text = snapshot.lines.map { ($0.text as NSString).size(withAttributes: attributes).width }.max() ?? 200
        let width = min(max(text, 260) + Self.padding * 2, max(120, available))
        let height = Self.padding * 2 + CGFloat(snapshot.lines.count) * Self.lineHeight + Self.gap + Self.frameGraph + Self.gap + Self.rateGraph
        return NSSize(width: ceil(width), height: ceil(height))
    }

    private static func color(_ level: String) -> NSColor {
        switch level {
        case "warn": return NSColor(srgbRed: 1.0, green: 0.78, blue: 0.30, alpha: 1)
        case "bad": return NSColor(srgbRed: 1.0, green: 0.42, blue: 0.38, alpha: 1)
        case "dim": return NSColor(white: 1, alpha: 0.55)
        default: return NSColor(white: 1, alpha: 0.92)
        }
    }

    override func draw(_ dirtyRect: NSRect) {
        NSColor(srgbRed: 0.05, green: 0.06, blue: 0.08, alpha: 0.78).setFill()
        bounds.fill()
        let inner = bounds.insetBy(dx: Self.padding, dy: Self.padding)
        var y = inner.minY
        let paragraph = NSMutableParagraphStyle(); paragraph.lineBreakMode = .byTruncatingTail
        for line in snapshot.lines {
            (line.text as NSString).draw(with: NSRect(x: inner.minX, y: y, width: inner.width, height: Self.lineHeight),
                                         options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine],
                                         attributes: [.font: Self.font, .foregroundColor: Self.color(line.level), .paragraphStyle: paragraph])
            y += Self.lineHeight
        }
        y += Self.gap
        drawFrames(NSRect(x: inner.minX, y: y, width: inner.width, height: Self.frameGraph))
        y += Self.frameGraph + Self.gap
        drawBitrate(NSRect(x: inner.minX, y: y, width: inner.width, height: Self.rateGraph))
    }

    private func label(_ text: String, at point: NSPoint, right: Bool = false, in rect: NSRect) {
        let attributes: [NSAttributedString.Key: Any] = [.font: Self.small, .foregroundColor: NSColor(white: 1, alpha: 0.6)]
        let size = (text as NSString).size(withAttributes: attributes)
        (text as NSString).draw(at: NSPoint(x: right ? rect.maxX - size.width - 2 : point.x, y: point.y), withAttributes: attributes)
    }

    private func drawFrames(_ rect: NSRect) {
        NSColor(white: 1, alpha: 0.06).setFill(); rect.fill()
        let frames = snapshot.frames, budget = snapshot.budget
        let peak = frames.reduce(0.0) { max($0, $1.interval, $1.latency) }
        // Round the scale to a readable step so it does not jitter every snapshot.
        let top = [33_333.0, 50_000, 100_000, 200_000, 500_000].first { $0 >= max(peak, budget * 2) } ?? 500_000
        func yValue(_ micros: Double) -> CGFloat { rect.maxY - CGFloat(min(micros, top) / top) * rect.height }
        let step = rect.width / CGFloat(Snapshot.frameCapacity)
        let origin = rect.maxX - CGFloat(frames.count) * step
        for (index, frame) in frames.enumerated() where frame.interval > 0 {
            let color: NSColor = frame.interval <= budget * 1.5 ? NSColor(srgbRed: 0.35, green: 0.82, blue: 0.48, alpha: 0.9)
                : frame.interval <= 50_000 ? Self.color("warn") : Self.color("bad")
            color.setFill()
            let top = yValue(frame.interval)
            NSRect(x: origin + CGFloat(index) * step, y: top, width: max(1, step - 0.5), height: rect.maxY - top).fill()
        }
        let reference = NSBezierPath(); reference.move(to: NSPoint(x: rect.minX, y: yValue(budget))); reference.line(to: NSPoint(x: rect.maxX, y: yValue(budget)))
        reference.setLineDash([3, 3], count: 2, phase: 0); reference.lineWidth = 1; NSColor(white: 1, alpha: 0.35).setStroke(); reference.stroke()
        let latency = NSBezierPath(); var drawing = false
        for (index, frame) in frames.enumerated() {
            guard frame.latency > 0 else { drawing = false; continue }
            let point = NSPoint(x: origin + (CGFloat(index) + 0.5) * step, y: yValue(frame.latency))
            if drawing { latency.line(to: point) } else { latency.move(to: point); drawing = true }
        }
        latency.lineWidth = 1.25; NSColor(srgbRed: 0.40, green: 0.75, blue: 1.0, alpha: 0.95).setStroke(); latency.stroke()
        label(String(format: "%.0f ms", top / 1000), at: NSPoint(x: rect.minX + 2, y: rect.minY + 1), right: true, in: rect)
    }

    private func drawBitrate(_ rect: NSRect) {
        NSColor(white: 1, alpha: 0.06).setFill(); rect.fill()
        let samples = snapshot.bitrate
        let peak = max(1000, (samples.max() ?? 0) * 1.15)
        let step = rect.width / CGFloat(max(1, Snapshot.rateCapacity - 1))
        let origin = rect.maxX - CGFloat(max(0, samples.count - 1)) * step
        if samples.count > 1 {
            let area = NSBezierPath(); area.move(to: NSPoint(x: origin, y: rect.maxY))
            for (index, value) in samples.enumerated() { area.line(to: NSPoint(x: origin + CGFloat(index) * step, y: rect.maxY - CGFloat(value / peak) * rect.height)) }
            area.line(to: NSPoint(x: rect.maxX, y: rect.maxY)); area.close()
            NSColor(srgbRed: 0.62, green: 0.52, blue: 1.0, alpha: 0.35).setFill(); area.fill()
            let line = NSBezierPath()
            for (index, value) in samples.enumerated() {
                let point = NSPoint(x: origin + CGFloat(index) * step, y: rect.maxY - CGFloat(value / peak) * rect.height)
                if index == 0 { line.move(to: point) } else { line.line(to: point) }
            }
            line.lineWidth = 1.25; NSColor(srgbRed: 0.70, green: 0.62, blue: 1.0, alpha: 1).setStroke(); line.stroke()
        }
        let seconds = Double(Snapshot.rateCapacity) * snapshot.sampleMs / 1000
        label(String(format: "%.1f Mb/s", peak / 1000), at: NSPoint(x: rect.minX + 2, y: rect.minY + 1), right: true, in: rect)
        label(String(format: "bitrate · %.0f s", seconds), at: NSPoint(x: rect.minX + 2, y: rect.minY + 1), in: rect)
    }
}
