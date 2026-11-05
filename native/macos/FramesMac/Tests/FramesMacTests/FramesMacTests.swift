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
        let shape = CursorShape(bgra: Data(count: 16 * 16 * 4), width: 16, height: 16, hotspot: NSPoint(x: 2, y: 3))
        view.setHostCursor(shape, visible: nil)
        // No picture size yet: one point per host pixel.
        XCTAssertEqual(view.shownCursor?.hotSpot, NSPoint(x: 2, y: 3))
        view.setHostCursor(nil, visible: false)
        XCTAssertNotNil(view.shownCursor)
        XCTAssertNotEqual(view.shownCursor?.hotSpot, NSPoint(x: 2, y: 3), "hidden: a blank cursor")
        view.setHostCursor(nil, visible: true)
        XCTAssertEqual(view.shownCursor?.hotSpot, NSPoint(x: 2, y: 3), "shown again")
    }

    @MainActor
    func testHostCursorFollowsThePictureScale() {
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 200, height: 100), relay: InputRelay())
        let layer = CAMetalLayer()
        layer.drawableSize = CGSize(width: 400, height: 200)
        view.wantsLayer = true
        view.layer = layer
        let shape = CursorShape(bgra: Data(count: 32 * 32 * 4), width: 32, height: 32, hotspot: NSPoint(x: 10, y: 4))
        view.setHostCursor(shape, visible: nil)
        // 400 host pixels in 200 points: half a point each.
        XCTAssertEqual(view.shownCursor?.image.size, NSSize(width: 16, height: 16))
        XCTAssertEqual(view.shownCursor?.hotSpot, NSPoint(x: 5, y: 2))
    }

    @MainActor
    func testLetterboxedPicturesMapInputToThePictureOnly() {
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 200, height: 100), relay: InputRelay())
        let layer = CAMetalLayer()
        layer.drawableSize = CGSize(width: 100, height: 100)
        view.wantsLayer = true
        view.layer = layer
        // A square picture in a 200x100 view: drawn from x 50 to 150.
        XCTAssertEqual(view.pictureRect, NSRect(x: 50, y: 0, width: 100, height: 100))
        XCTAssertEqual(view.normalized(NSPoint(x: 50, y: 100)).0, 0)
        XCTAssertEqual(view.normalized(NSPoint(x: 100, y: 50)).0, 32768)
        XCTAssertEqual(view.normalized(NSPoint(x: 150, y: 0)).0, 65535)
        XCTAssertEqual(view.normalized(NSPoint(x: 10, y: 0)).0, 0, "the bars clamp to the edge")
        view.fill = true
        XCTAssertEqual(view.pictureRect, view.bounds, "filled: the whole view")
        XCTAssertEqual(view.normalized(NSPoint(x: 200, y: 0)).0, 65535)
    }

    @MainActor
    func testPrintableKeysGoAsTextUnlessPhysicalOrWithCommand() {
        final class Box: @unchecked Sendable {
            var keys: [(Int32, Int32)] = []
            var texts: [String] = []
        }
        let box = Box()
        let relay = InputRelay()
        let context = Unmanaged.passUnretained(box).toOpaque()
        relay.set(context: context) { context, kind, a, b, _, _, _ in
            if kind == UInt32(FRAMES_MAC_INPUT_KEY) {
                Unmanaged<Box>.fromOpaque(context!).takeUnretainedValue().keys.append((a, b))
            }
        }
        relay.setText(context: context) { context, utf8, length, _ in
            let text = String(decoding: UnsafeBufferPointer(start: utf8, count: length), as: UTF8.self)
            Unmanaged<Box>.fromOpaque(context!).takeUnretainedValue().texts.append(text)
        }
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 100, height: 100), relay: relay)
        func key(_ type: NSEvent.EventType, _ code: UInt16, _ text: String, _ flags: NSEvent.ModifierFlags = []) -> NSEvent {
            NSEvent.keyEvent(
                with: type, location: .zero, modifierFlags: flags, timestamp: 1, windowNumber: 0, context: nil,
                characters: text, charactersIgnoringModifiers: text, isARepeat: false, keyCode: code)!
        }
        view.insertText("é", replacementRange: NSRange(location: NSNotFound, length: 0))
        XCTAssertEqual(box.texts, ["é"], "committed text goes as text")
        view.keyUp(with: key(.keyUp, 0, "a"))
        XCTAssertEqual(box.keys.count, 1, "a release without a press still goes (as a key)")
        box.keys.removeAll()
        view.keyDown(with: key(.keyDown, 0, "a", [.command]))
        view.keyUp(with: key(.keyUp, 0, "a", [.command]))
        XCTAssertEqual(box.keys.map(\.1), [1, 0], "Command+A is a shortcut: keys")
        box.keys.removeAll()
        view.keyDown(with: key(.keyDown, 123, "\u{F702}"))
        XCTAssertEqual(box.keys.map(\.0), [123], "arrows are keys")
        box.keys.removeAll()
        view.setPhysicalKeys(true)
        view.keyDown(with: key(.keyDown, 0, "a"))
        view.keyUp(with: key(.keyUp, 0, "a"))
        XCTAssertEqual(box.keys.map(\.1), [1, 0], "physical keys: every key is a key")
        relay.set(context: nil, callback: nil)
        relay.setText(context: nil, callback: nil)
    }

    @MainActor
    func testConnectionMenuShowsV1Items() {
        viewerState = [
            "quality": 1, "view": "windows", "remote_cursor": false, "stats": false, "adb": true,
            "host_platform": "windows",
            "windows": [
                ["id": 7, "title": "Notepad", "app": "notepad", "shown": true, "desktop": false, "resizable": true,
                 "prefs": ["resize_remote": true, "fill": false, "header": false, "physical": false]],
            ],
            "gamepads": [], "usb": [],
        ]
        let view = InputView(frame: NSRect(x: 0, y: 0, width: 100, height: 100), relay: InputRelay())
        view.menuID = 7
        let titles = framesMenu(for: view).items.map(\.title)
        for title in [
            "Image quality", "Windows", "Show all windows", "Hide all windows", "Ignore / hide this window",
            "Per-window view", "Full desktop view", "Show remote cursor", "Show debug overlay (Control+Option+F10)",
            "Show remote window header", "Resize remote app", "Stretch locally", "Toggle aspect fit / fill",
            "Toggle Control / Command swap", "Toggle physical keys (terminal)", "Refresh application menus",
            "Fetch remote file...", "Disconnect",
        ] {
            XCTAssertTrue(titles.contains(title), title)
        }
        let menu = framesMenu(for: view)
        XCTAssertEqual(menu.item(withTitle: "Resize remote app")?.state, .on)
        XCTAssertEqual(menu.item(withTitle: "Toggle Control / Command swap")?.state, .on, "Windows host: swapped")
        XCTAssertEqual(menu.item(withTitle: "Image quality")?.submenu?.items[1].state, .on)
        let devices = devicesMenu().items.map(\.title)
        XCTAssertEqual(devices, ["Gamepads", "USB devices", "Refresh devices", "Android ADB bridge", "Device forwarding support..."])
        XCTAssertEqual(devicesMenu().item(withTitle: "Android ADB bridge")?.state, .on)
        viewerState = [:]
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
                context: nil, characters: "\u{F70F}", charactersIgnoringModifiers: "\u{F70F}", isARepeat: false, keyCode: code)!
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

    func testDebugOverlayDrawsV1sPanelAtTheWindowScale() throws {
        let json = """
            {"lines":[{"text":"HEVC 1920×1080 · 60 fps","level":"normal"},{"text":"Latency  p50 9.0 ms","level":"warn"}],
             "frames":[[16667,9000],[16667,12000]],"frame_budget_us":16666,"bitrate_kbps":[12000,14000],"sample_ms":200}
            """
        let content = try XCTUnwrap(OverlayContent(json: Data(json.utf8)))
        XCTAssertEqual(content.lines.count, 2)
        XCTAssertEqual(content.lines[1].level, "warn")
        XCTAssertEqual(content.frames.count, 2)
        let image = try XCTUnwrap(renderOverlay(content, scale: 2))
        // To look at it: FRAMES_OVERLAY_PNG=/path/overlay.png swift test.
        if let path = ProcessInfo.processInfo.environment["FRAMES_OVERLAY_PNG"] {
            let rep = NSBitmapImageRep(cgImage: image)
            try rep.representation(using: .png, properties: [:])?.write(to: URL(fileURLWithPath: path))
        }
        XCTAssertEqual(image.width, Int(OverlayLayout.width * 2))
        XCTAssertEqual(image.height, Int(OverlayLayout.height(lines: 2) * 2))
        XCTAssertNil(OverlayContent(json: Data("[]".utf8)), "not an overlay")
    }

    func testDroppedFilesCarryWhereTheyWereDropped() throws {
        let drops = DroppedFiles()
        drops.add(["/tmp/a.txt"], at: (100, 65535))
        drops.add(["/tmp/b.txt"])
        let taken = drops.take()
        XCTAssertEqual((taken[0] as? [String: Any])?["path"] as? String, "/tmp/a.txt")
        XCTAssertEqual((taken[0] as? [String: Any])?["y"] as? Int32, 65535)
        XCTAssertEqual(taken[1] as? String, "/tmp/b.txt")
        XCTAssertTrue(drops.take().isEmpty)
    }

    @MainActor
    func testInputGoesToTheModalDialogBlockingTheWindow() {
        final class Box: @unchecked Sendable { var kinds: [UInt32] = [] }
        let box = Box()
        let relay = InputRelay()
        relay.set(context: Unmanaged.passUnretained(box).toOpaque()) { context, kind, _, _, _, _, _ in
            Unmanaged<Box>.fromOpaque(context!).takeUnretainedValue().kinds.append(kind)
        }
        let owner = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 400, height: 300), styleMask: [.titled], backing: .buffered,
            defer: false)
        let ownerView = InputView(frame: owner.contentLayoutRect, relay: relay)
        ownerView.menuID = 10
        owner.contentView = ownerView
        let dialog = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 200, height: 100), styleMask: [.titled], backing: .buffered,
            defer: false)
        let dialogView = InputView(frame: dialog.contentLayoutRect, relay: InputRelay())
        dialogView.menuID = 11
        dialog.contentView = dialogView
        dialog.orderFront(nil)
        defer {
            dialog.orderOut(nil)
            viewerState = [:]
            relay.set(context: nil, callback: nil)
        }
        viewerState = [
            "windows": [
                ["id": 10, "shown": true],
                ["id": 11, "shown": true, "parent": 10, "modal": true],
            ]
        ]
        XCTAssertTrue(ownerView.modalBlocker() === dialogView)
        let click = NSEvent.mouseEvent(
            with: .leftMouseDown, location: NSPoint(x: 10, y: 10), modifierFlags: [], timestamp: 0,
            windowNumber: owner.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1)!
        ownerView.mouseDown(with: click)
        XCTAssertFalse(box.kinds.contains(UInt32(FRAMES_MAC_INPUT_BUTTON)), "the blocked window sends no click")
        viewerState = ["windows": [["id": 10, "shown": true], ["id": 11, "shown": true, "parent": 10]]]
        XCTAssertNil(ownerView.modalBlocker(), "a modeless dialog blocks nothing")
    }
}
