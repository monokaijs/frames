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
}
