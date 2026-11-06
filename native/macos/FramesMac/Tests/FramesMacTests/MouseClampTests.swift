import AppKit
import CFramesMac
import XCTest
@testable import FramesMac

@MainActor
private final class FakeMouseClamp: MouseClamp {
    var allowed = true
    var held = false
    var points: [CGPoint] = []
    func begin() -> Bool { held = allowed; return allowed }
    func move(to point: CGPoint) { points.append(point) }
    func end() { held = false }
}

final class MouseClampTests: XCTestCase {
    func testClampUsesPictureEdgesAndDisplayScale() {
        let picture = NSRect(x: 50, y: 10, width: 100, height: 80)
        XCTAssertEqual(clampedMousePoint(NSPoint(x: -100, y: -200), in: picture, scale: 2), picture.origin)
        XCTAssertEqual(
            clampedMousePoint(NSPoint(x: 300, y: 200), in: picture, scale: 2), NSPoint(x: 149.5, y: 89.5))
        XCTAssertEqual(
            clampedMousePoint(NSPoint(x: 100, y: 40), in: picture, scale: 1), NSPoint(x: 100, y: 40))
    }

    @MainActor
    func testClampKeepsAbsoluteMotionAndClicksAndReleasesOnFocusLoss() throws {
        final class Events: @unchecked Sendable { var values: [(UInt32, Int32, Int32, Int32, Int32)] = [] }
        let events = Events()
        let relay = InputRelay()
        relay.set(context: Unmanaged.passUnretained(events).toOpaque()) { context, kind, a, b, c, d, _ in
            Unmanaged<Events>.fromOpaque(context!).takeUnretainedValue().values.append((kind, a, b, c, d))
        }
        defer { relay.set(context: nil, callback: nil) }
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 200, height: 100), relay: relay)
        let window = NSWindow(contentRect: view.bounds, styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = view
        defer { view.setMouseClamped(false); window.close() }
        let clamp = FakeMouseClamp()
        view.mouseClamp = clamp
        view.setMouseClamped(true)
        XCTAssertTrue(clamp.held)
        XCTAssertFalse(view.relative)
        let motion = try XCTUnwrap(CGEvent(
            mouseEventSource: nil, mouseType: .mouseMoved, mouseCursorPosition: .zero, mouseButton: .left))
        motion.setIntegerValueField(.mouseEventDeltaX, value: 1000)
        motion.setIntegerValueField(.mouseEventDeltaY, value: 1000)
        view.mouseMoved(with: try XCTUnwrap(NSEvent(cgEvent: motion)))
        let point = try XCTUnwrap(view.mouseClampPoint)
        XCTAssertLessThan(point.x, view.pictureRect.maxX)
        XCTAssertEqual(point.y, view.pictureRect.minY)
        let click = try XCTUnwrap(NSEvent.mouseEvent(
            with: .leftMouseDown, location: .zero, modifierFlags: [], timestamp: 1, windowNumber: window.windowNumber,
            context: nil, eventNumber: 0, clickCount: 1, pressure: 1))
        view.mouseDown(with: click)
        let move = try XCTUnwrap(events.values.first { $0.0 == UInt32(FRAMES_MAC_INPUT_MOVE) })
        let button = try XCTUnwrap(events.values.first { $0.0 == UInt32(FRAMES_MAC_INPUT_BUTTON) })
        XCTAssertEqual(button.3, move.1)
        XCTAssertEqual(button.4, move.2)
        XCTAssertFalse(events.values.contains { $0.0 == UInt32(FRAMES_MAC_INPUT_MODE) })
        NotificationCenter.default.post(name: NSWindow.didResignKeyNotification, object: window)
        XCTAssertFalse(clamp.held)
        XCTAssertFalse(view.mouseClamped)
    }

    @MainActor
    func testClampShortcutIgnoresRepeatsAndNeverForwardsEitherHalf() throws {
        final class Events: @unchecked Sendable { var kinds: [UInt32] = [] }
        let events = Events()
        let relay = InputRelay()
        relay.set(context: Unmanaged.passUnretained(events).toOpaque()) { context, kind, _, _, _, _, _ in
            Unmanaged<Events>.fromOpaque(context!).takeUnretainedValue().kinds.append(kind)
        }
        defer { relay.set(context: nil, callback: nil) }
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 100, height: 100), relay: relay)
        let window = NSWindow(contentRect: view.bounds, styleMask: [.titled], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = view
        defer { view.setMouseClamped(false); window.close() }
        let clamp = FakeMouseClamp()
        view.mouseClamp = clamp
        func key(_ type: NSEvent.EventType, _ code: UInt16 = 101, repeat repeated: Bool = false) throws -> NSEvent {
            try XCTUnwrap(NSEvent.keyEvent(
                with: type, location: .zero, modifierFlags: type == .keyDown ? [.control, .option] : [],
                timestamp: 1, windowNumber: window.windowNumber, context: nil, characters: "",
                charactersIgnoringModifiers: "", isARepeat: repeated, keyCode: code))
        }
        view.keyDown(with: try key(.keyDown))
        XCTAssertTrue(view.mouseClamped)
        XCTAssertEqual(framesMenu(for: view).item(withTitle: "Clamp mouse to view (Control+Option+F9)")?.state, .on)
        view.keyDown(with: try key(.keyDown, repeat: true))
        XCTAssertTrue(view.mouseClamped)
        view.keyUp(with: try key(.keyUp))
        view.keyDown(with: try key(.keyDown))
        XCTAssertFalse(view.mouseClamped)
        view.keyUp(with: try key(.keyUp))
        XCTAssertFalse(events.kinds.contains(UInt32(FRAMES_MAC_INPUT_KEY)))
        view.setMouseClamped(true)
        view.keyDown(with: try key(.keyDown, 111))
        XCTAssertFalse(clamp.held, "F12 releases both pointer modes")
        clamp.allowed = false
        view.setMouseClamped(true)
        XCTAssertFalse(view.mouseClamped, "failed confinement must leave normal input usable")
    }
}
