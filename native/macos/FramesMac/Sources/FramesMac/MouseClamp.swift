import AppKit

/// A visible pointer driven within the picture while normal absolute input
/// continues to go to the host. Tests replace the system calls.
@MainActor
protocol MouseClamp {
    func begin() -> Bool
    func move(to screenPoint: CGPoint)
    func end()
}

struct SystemMouseClamp: MouseClamp {
    func begin() -> Bool {
        CGAssociateMouseAndMouseCursorPosition(0) == .success
    }

    func move(to screenPoint: CGPoint) {
        CGWarpMouseCursorPosition(screenPoint)
    }

    func end() {
        CGAssociateMouseAndMouseCursorPosition(1)
    }
}

/// Keep the pointer inside the last drawable pixel, including on Retina
/// displays. The rectangle is in the view's bottom-left coordinates.
func clampedMousePoint(_ point: NSPoint, in rect: NSRect, scale: CGFloat) -> NSPoint {
    let pixel = 1 / max(scale, 1)
    return NSPoint(
        x: min(max(point.x, rect.minX), max(rect.minX, rect.maxX - pixel)),
        y: min(max(point.y, rect.minY), max(rect.minY, rect.maxY - pixel)))
}
