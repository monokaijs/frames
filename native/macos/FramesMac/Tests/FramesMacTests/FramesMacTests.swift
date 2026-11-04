import AppKit
import CFramesMac
import CoreVideo
import XCTest

@testable import FramesMac

/// Encoder output collected from the C callback.
private final class Sink: @unchecked Sendable {
    struct Unit {
        let bytes: Data
        let keyframe: Bool
        let token: Int64
        let status: Int32
    }
    let lock = NSLock()
    let ready = DispatchSemaphore(value: 0)
    var units: [Unit] = []

    func next() -> Unit? {
        guard ready.wait(timeout: .now() + 2) == .success else { return nil }
        lock.lock()
        defer { lock.unlock() }
        return units.removeFirst()
    }
}


/// Long-term reference behaviour needs Apple-silicon VideoToolbox; virtual
/// Macs (CI runners) hand out no tokens and time out decoding.
private func skipOnVirtualMac() throws {
    var size = 0
    sysctlbyname("hw.model", nil, &size, nil, 0)
    var model = [CChar](repeating: 0, count: max(size, 1))
    sysctlbyname("hw.model", &model, &size, nil, 0)
    let name = String(cString: model)
    if name.contains("VirtualMac") {
        throw XCTSkip("VideoToolbox long-term references need a physical Mac (\(name))")
    }
}

private func callbacks(_ sink: Sink) -> FramesMacCallbacks {
    FramesMacCallbacks(
        context: Unmanaged.passUnretained(sink).toOpaque(),
        on_frame: nil,
        on_encoded: { context, data, length, flags, token, _, _, status in
            let sink = Unmanaged<Sink>.fromOpaque(context!).takeUnretainedValue()
            let bytes = data.map { Data(bytes: $0, count: length) } ?? Data()
            sink.lock.lock()
            sink.units.append(
                .init(bytes: bytes, keyframe: flags & UInt32(FRAMES_MAC_ENCODED_KEYFRAME) != 0, token: token, status: status))
            sink.lock.unlock()
            sink.ready.signal()
        },
        on_cursor: nil,
        on_event: nil)
}

final class FramesMacTests: XCTestCase {
    func testAbiVersionMatchesHeader() {
        XCTAssertEqual(framesMacAbiVersion(), UInt32(FRAMES_MAC_ABI_VERSION))
    }

    func testSyntheticFramesAreDeterministic() throws {
        let a = try XCTUnwrap(makeBGRABuffer(width: 640, height: 360))
        let b = try XCTUnwrap(makeBGRABuffer(width: 640, height: 360))
        fillSynthetic(a, index: 7)
        fillSynthetic(b, index: 7)
        XCTAssertEqual(pixelDifference(a, b), 0)
        fillSynthetic(b, index: 8)
        XCTAssertGreaterThan(pixelDifference(a, b), 0)
    }

    func testHardwareRoundTripWithInfiniteGopAndLongTermRecovery() throws {
        try skipOnVirtualMac()
        for codec in [UInt32(FRAMES_MAC_CODEC_H264), UInt32(FRAMES_MAC_CODEC_HEVC)] {
            let sink = Sink()
            let encoder = try XCTUnwrap(
                Encoder(
                    width: 1280, height: 720, fps: 60, bitrate: 8_000_000, codec: codec,
                    flags: UInt32(FRAMES_MAC_ENCODER_LTR), callbacks: Callbacks(raw: callbacks(sink))))
            let decoder = Decoder(codec: codec)
            var tokens: [Int: Int64] = [:]
            var keyframes = 0
            var lastError = 0.0
            let started = Date()
            for index in 0..<90 {
                let source = try XCTUnwrap(makeBGRABuffer(width: 1280, height: 720))
                fillSynthetic(source, index: UInt64(index))
                var options: UInt32 = 0
                var acknowledged: [Int64] = []
                // The receiver confirms holding frame 30 two frames later (a
                // feedback delay); only that token is acknowledged. Frames
                // 40-44 are lost, and frame 45 predicts from the acknowledged
                // long-term frame.
                if index < 40, let token = tokens[index - 2] {
                    acknowledged = [token]
                }
                if index == 45 {
                    options = UInt32(FRAMES_MAC_ENCODE_FORCE_LTR_REFRESH)
                }
                XCTAssertEqual(
                    encoder.encode(source, pts: UInt64(index) * 16_666_667, options: options, acknowledged: acknowledged),
                    noErr)
                let unit = try XCTUnwrap(sink.next(), "encoder output for frame \(index)")
                XCTAssertEqual(unit.status, 0)
                if unit.keyframe { keyframes += 1 }
                if (40..<45).contains(index) { continue }
                if unit.token >= 0 { tokens[index] = unit.token }
                let (status, decoded) = decoder.decode(unit.bytes, keyframe: unit.keyframe)
                XCTAssertEqual(status, Int32(FRAMES_MAC_DECODE_OK), "frame \(index)")
                let decodedBuffer = try XCTUnwrap(decoded)
                if index == 45 {
                    XCTAssertFalse(unit.keyframe, "long-term recovery is a predicted frame")
                }
                // Pixel checks are slow in debug builds; sample them.
                if (45...48).contains(index) || index % 20 == 0 || index == 89 {
                    lastError = pixelDifference(source, decodedBuffer)
                    XCTAssertLessThan(lastError, 12, "frame \(index) decodes to the source")
                }
            }
            XCTAssertEqual(keyframes, 1, "no periodic or recovery keyframes")
            XCTAssertNotNil(tokens[30], "VideoToolbox offered long-term references")
            XCTAssertLessThan(lastError, 12)
            _ = started
        }
    }

    /// Only frames the engine marks as long-term (every 30 frames) are
    /// acknowledged, as the receiver confirms them; VideoToolbox must keep
    /// offering tokens and recover from the acknowledged one.
    func testSparseAcknowledgementStillRecoversWithoutKeyframe() throws {
        try skipOnVirtualMac()
        for codec in [UInt32(FRAMES_MAC_CODEC_H264), UInt32(FRAMES_MAC_CODEC_HEVC)] {
            let sink = Sink()
            let encoder = try XCTUnwrap(
                Encoder(
                    width: 1280, height: 720, fps: 60, bitrate: 8_000_000, codec: codec,
                    flags: UInt32(FRAMES_MAC_ENCODER_LTR), callbacks: Callbacks(raw: callbacks(sink))))
            let decoder = Decoder(codec: codec)
            var tokens: [Int: Int64] = [:]
            var keyframes = 0
            var pendingMark = true
            var marks: [Int] = []
            for index in 0..<120 {
                let source = try XCTUnwrap(makeBGRABuffer(width: 1280, height: 720))
                fillSynthetic(source, index: UInt64(index))
                var acknowledged: [Int64] = []
                // A mark requested every 30 frames lands on the next frame
                // carrying a token and is confirmed 3 frames later.
                if index % 30 == 0 { pendingMark = true }
                if let marked = marks.last, index == marked + 3, let token = tokens[marked] { acknowledged = [token] }
                let refresh = index == 75  // frames 70-74 lost
                XCTAssertEqual(
                    encoder.encode(
                        source, pts: UInt64(index) * 16_666_667,
                        options: refresh ? UInt32(FRAMES_MAC_ENCODE_FORCE_LTR_REFRESH) : 0, acknowledged: acknowledged),
                    noErr)
                let unit = try XCTUnwrap(sink.next())
                if unit.keyframe { keyframes += 1 }
                if unit.token >= 0 {
                    tokens[index] = unit.token
                    if pendingMark, !(70..<75).contains(index) {
                        marks.append(index)
                        pendingMark = false
                    }
                }
                if (70..<75).contains(index) { continue }
                let (status, decoded) = decoder.decode(unit.bytes, keyframe: unit.keyframe)
                XCTAssertEqual(status, Int32(FRAMES_MAC_DECODE_OK), "codec \(codec) frame \(index)")
                if refresh { XCTAssertFalse(unit.keyframe, "codec \(codec): recovery is a predicted frame") }
                if (75...77).contains(index) || index == 119, let decoded {
                    XCTAssertLessThan(pixelDifference(source, decoded), 12, "codec \(codec) frame \(index)")
                }
            }
            XCTAssertEqual(keyframes, 1, "codec \(codec)")
            XCTAssertEqual(marks.count, 4, "codec \(codec): marks \(marks)")
            for (request, marked) in zip([0, 30, 60, 90], marks) {
                XCTAssertLessThanOrEqual(marked - request, 4, "codec \(codec): a token arrives soon after a mark request")
            }
        }
    }

    func testOffscreenPresenterDrawsDecodedFrames() throws {
        let presenter = try XCTUnwrap(Presenter(width: 640, height: 360, windowed: false))
        let source = try XCTUnwrap(makeBGRABuffer(width: 640, height: 360))
        fillSynthetic(source, index: 1)
        let (status, time) = presenter.present(source, vsync: false)
        XCTAssertEqual(status, 0)
        XCTAssertGreaterThan(time, 0)
    }

    @MainActor
    func testInputViewShowsTheHostCursorAndHidesIt() {
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 200, height: 100), relay: InputRelay())
        XCTAssertNil(view.shownCursor, "the system cursor until the host sends one")
        let image = NSImage(size: NSSize(width: 16, height: 16))
        view.setHostCursor(image, hotspot: NSPoint(x: 2, y: 3), visible: nil)
        XCTAssertEqual(view.shownCursor?.hotSpot, NSPoint(x: 2, y: 3))
        view.setHostCursor(nil, hotspot: .zero, visible: false)
        XCTAssertNotNil(view.shownCursor)
        XCTAssertNotEqual(view.shownCursor?.hotSpot, NSPoint(x: 2, y: 3), "hidden: a blank cursor")
        view.setHostCursor(nil, hotspot: .zero, visible: true)
        XCTAssertEqual(view.shownCursor?.hotSpot, NSPoint(x: 2, y: 3), "shown again")
    }

    @MainActor
    func testInputViewReportsNormalizedPositionsTopLeft() {
        final class Box: @unchecked Sendable { var events: [(UInt32, Int32, Int32)] = [] }
        let box = Box()
        let relay = InputRelay()
        let callback: FramesMacInputCallback = { context, kind, a, b, _, _, _ in
            Unmanaged<Box>.fromOpaque(context!).takeUnretainedValue().events.append((kind, a, b))
        }
        relay.set(context: Unmanaged.passUnretained(box).toOpaque(), callback: callback)
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 100), styleMask: [.titled], backing: .buffered,
            defer: true)
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 200, height: 100), relay: relay)
        window.contentView = view
        // Window coordinates: origin bottom left.
        let event = NSEvent.mouseEvent(
            with: .mouseMoved, location: NSPoint(x: 200, y: 100), modifierFlags: [], timestamp: 1,
            windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 0, pressure: 0)!
        view.mouseMoved(with: event)
        relay.set(context: nil, callback: nil)
        XCTAssertEqual(box.events.count, 1)
        XCTAssertEqual(box.events.first?.0, UInt32(FRAMES_MAC_INPUT_MOVE))
        XCTAssertEqual(box.events.first?.1, 65535, "right edge")
        XCTAssertEqual(box.events.first?.2, 0, "top edge")
    }

    @MainActor
    func testControlOptionF12ReleasesInputInsteadOfSendingTheKey() {
        final class Box: @unchecked Sendable { var kinds: [UInt32] = [] }
        let box = Box()
        let relay = InputRelay()
        let callback: FramesMacInputCallback = { context, kind, _, _, _, _, _ in
            Unmanaged<Box>.fromOpaque(context!).takeUnretainedValue().kinds.append(kind)
        }
        relay.set(context: Unmanaged.passUnretained(box).toOpaque(), callback: callback)
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 100, height: 100), relay: relay)
        func key(_ code: UInt16, _ flags: NSEvent.ModifierFlags) -> NSEvent {
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: flags, timestamp: 1, windowNumber: 0,
                context: nil, characters: "", charactersIgnoringModifiers: "", isARepeat: false, keyCode: code)!
        }
        view.keyDown(with: key(111, [.control, .option]))
        view.keyDown(with: key(111, []))
        relay.set(context: nil, callback: nil)
        XCTAssertEqual(box.kinds, [UInt32(FRAMES_MAC_INPUT_FOCUS_LOST), UInt32(FRAMES_MAC_INPUT_KEY)])
    }

    @MainActor
    func testControlOptionF11TogglesRelativeMouseWithoutSendingTheKey() {
        final class Box: @unchecked Sendable { var events: [(UInt32, Int32, Int32)] = [] }
        @MainActor final class FakeLock: PointerLock {
            var locked = false
            func lock() -> Bool {
                locked = true
                return true
            }
            func unlock() { locked = false }
        }
        let box = Box()
        let relay = InputRelay()
        let callback: FramesMacInputCallback = { context, kind, a, b, _, _, _ in
            Unmanaged<Box>.fromOpaque(context!).takeUnretainedValue().events.append((kind, a, b))
        }
        relay.set(context: Unmanaged.passUnretained(box).toOpaque(), callback: callback)
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 100, height: 100), relay: relay)
        let lock = FakeLock()
        view.pointerLock = lock
        func key(_ code: UInt16) -> NSEvent {
            NSEvent.keyEvent(
                with: .keyDown, location: .zero, modifierFlags: [.control, .option], timestamp: 1, windowNumber: 0,
                context: nil, characters: "", charactersIgnoringModifiers: "", isARepeat: false, keyCode: code)!
        }
        // A mouse event with deltas, built but never posted.
        func moved(_ dx: Int64, _ dy: Int64) -> NSEvent {
            let event = CGEvent(
                mouseEventSource: nil, mouseType: .mouseMoved, mouseCursorPosition: .zero, mouseButton: .left)!
            event.setIntegerValueField(.mouseEventDeltaX, value: dx)
            event.setIntegerValueField(.mouseEventDeltaY, value: dy)
            return NSEvent(cgEvent: event)!
        }
        view.keyDown(with: key(103))
        XCTAssertTrue(view.relative)
        XCTAssertTrue(lock.locked, "the pointer is held while relative")
        view.mouseMoved(with: moved(5, -3))
        view.mouseMoved(with: moved(0, 0))
        view.keyDown(with: key(103))
        XCTAssertFalse(view.relative)
        XCTAssertFalse(lock.locked, "given back")
        view.keyDown(with: key(103))
        view.keyDown(with: key(111))
        XCTAssertFalse(lock.locked, "Control+Option+F12 also gives the pointer back")
        relay.set(context: nil, callback: nil)
        let mode = UInt32(FRAMES_MAC_INPUT_MODE), motion = UInt32(FRAMES_MAC_INPUT_MOTION)
        XCTAssertEqual(box.events.map(\.0), [mode, motion, mode, mode, mode, UInt32(FRAMES_MAC_INPUT_FOCUS_LOST)])
        XCTAssertEqual(box.events[0].1, 1, "on")
        XCTAssertEqual(box.events[1].1, 5)
        XCTAssertEqual(box.events[1].2, -3, "up is negative")
        XCTAssertEqual(box.events[2].1, 0, "off")
        XCTAssertFalse(box.events.contains { $0.0 == UInt32(FRAMES_MAC_INPUT_KEY) }, "F11 never reaches the host")
    }
}
