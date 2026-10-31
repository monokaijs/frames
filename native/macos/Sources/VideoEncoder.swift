import Foundation
import VideoToolbox
import CoreMedia
import CoreVideo
import Darwin

// A bounded VideoToolbox worker. Each stdout packet is a little-endian u32
// length followed by one complete Annex-B access unit, including parameter sets
// on keyframes. No look-ahead or waiting for the next frame to delimit output.
enum NativeVideoEncoder {
    private final class Output {
        let lock = NSLock()
        func send(_ sample: CMSampleBuffer) throws {
            guard let description = CMSampleBufferGetFormatDescription(sample),
                  let block = CMSampleBufferGetDataBuffer(sample) else {
                throw failure("Missing encoded sample")
            }
            var packet = Data()
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]]
            let keyframe = attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool != true
            if keyframe {
                var count = 0
                try check(CMVideoFormatDescriptionGetH264ParameterSetAtIndex(description, parameterSetIndex: 0, parameterSetPointerOut: nil, parameterSetSizeOut: nil, parameterSetCountOut: &count, nalUnitHeaderLengthOut: nil))
                for index in 0..<count {
                    var bytes: UnsafePointer<UInt8>?
                    var size = 0
                    try check(CMVideoFormatDescriptionGetH264ParameterSetAtIndex(description, parameterSetIndex: index, parameterSetPointerOut: &bytes, parameterSetSizeOut: &size, parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil))
                    guard let bytes else { throw failure("Missing H.264 parameter set") }
                    packet.append(contentsOf: [0, 0, 0, 1])
                    packet.append(bytes, count: size)
                }
            }
            let size = CMBlockBufferGetDataLength(block)
            var encoded = Data(count: size)
            try encoded.withUnsafeMutableBytes { bytes in
                try check(CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: size, destination: bytes.baseAddress!))
            }
            var offset = 0
            while offset + 4 <= encoded.count {
                let size = encoded[offset..<offset + 4].reduce(0) { ($0 << 8) | Int($1) }
                offset += 4
                guard size > 0, size <= encoded.count - offset else { throw failure("Invalid H.264 NAL length") }
                packet.append(contentsOf: [0, 0, 0, 1])
                packet.append(encoded[offset..<offset + size])
                offset += size
            }
            guard offset == encoded.count else { throw failure("Invalid H.264 packet") }
            // Begin the next access unit now, rather than waiting for its
            // picture. The streaming decoder can release this picture at once.
            // AUD is optional before the stream's first SPS/PPS and picture.
            packet.append(contentsOf: [0, 0, 0, 1, 9, 0xf0])
            guard packet.count <= 8 * 1024 * 1024 else { throw failure("Invalid H.264 packet") }
            var sizeLE = UInt32(packet.count).littleEndian
            var framed = withUnsafeBytes(of: &sizeLE) { Data($0) }
            framed.append(packet)
            lock.lock()
            defer { lock.unlock() }
            try FileHandle.standardOutput.write(contentsOf: framed)
        }
    }

    private static func failure(_ message: String) -> NSError {
        NSError(domain: "Frames.VideoEncoder", code: 1, userInfo: [NSLocalizedDescriptionKey: message])
    }
    private static func check(_ status: OSStatus) throws {
        if status != noErr { throw failure("VideoToolbox error \(status)") }
    }

    static func run(_ args: [String]) -> Int32 {
        do {
            guard args.count == 4, let width = Int(args[0]), let height = Int(args[1]),
                  let fps = Int(args[2]), let bitrate = Int(args[3]),
                  (1...8192).contains(width), (1...8192).contains(height),
                  (1...240).contains(fps), (1...100_000_000).contains(bitrate) else {
                throw failure("Expected width height fps bitrate")
            }
            let codedWidth = max(128, (width + 1) / 2 * 2)
            let codedHeight = max(128, (height + 1) / 2 * 2)
            let output = Unmanaged.passRetained(Output())
            defer { output.release() }
            var session: VTCompressionSession?
            let specification: [CFString: Any] = [
                kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true,
                kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true,
            ]
            let attributes: [CFString: Any] = [
                kCVPixelBufferWidthKey: codedWidth, kCVPixelBufferHeightKey: codedHeight,
                kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
                kCVPixelBufferIOSurfacePropertiesKey: [:],
            ]
            try check(VTCompressionSessionCreate(allocator: nil, width: Int32(codedWidth), height: Int32(codedHeight), codecType: kCMVideoCodecType_H264, encoderSpecification: specification as CFDictionary, imageBufferAttributes: attributes as CFDictionary, compressedDataAllocator: nil, outputCallback: { context, _, status, _, sample in
                do {
                    try NativeVideoEncoder.check(status)
                    guard let context, let sample else { throw NativeVideoEncoder.failure("Missing encoder output") }
                    try Unmanaged<Output>.fromOpaque(context).takeUnretainedValue().send(sample)
                } catch {
                    fputs("Frames H.264: \(error.localizedDescription)\n", stderr)
                    exit(1)
                }
            }, refcon: output.toOpaque(), compressionSessionOut: &session))
            guard let session else { throw failure("Missing compression session") }
            defer { VTCompressionSessionInvalidate(session) }
            for (key, value): (CFString, Any) in [
                (kVTCompressionPropertyKey_RealTime, true),
                (kVTCompressionPropertyKey_AllowFrameReordering, false),
                (kVTCompressionPropertyKey_MaxFrameDelayCount, 0),
                (kVTCompressionPropertyKey_AverageBitRate, bitrate),
                (kVTCompressionPropertyKey_ExpectedFrameRate, fps),
                (kVTCompressionPropertyKey_MaxKeyFrameInterval, 60),
            ] {
                let status = VTSessionSetProperty(session, key: key, value: value as CFTypeRef)
                // Apple's low-latency encoder can reject the generic delay
                // property. CompleteFrames below still bounds every submit.
                if key == kVTCompressionPropertyKey_MaxFrameDelayCount && status == kVTPropertyNotSupportedErr { continue }
                guard status == noErr else { throw failure("\(key): VideoToolbox error \(status)") }
            }
            try check(VTCompressionSessionPrepareToEncodeFrames(session))
            guard let pool = VTCompressionSessionGetPixelBufferPool(session) else { throw failure("Missing pixel buffer pool") }
            var index: Int64 = 0
            let input = FileHandle.standardInput
            while true {
                let keepGoing = try autoreleasepool {
                    var raw = Data()
                    let size = width * height * 4
                    while raw.count < size {
                        guard let next = try input.read(upToCount: size - raw.count), !next.isEmpty else {
                            guard raw.isEmpty else { throw failure("Truncated raw frame") }
                            try check(VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid))
                            return false
                        }
                        raw.append(next)
                    }
                    var pixel: CVPixelBuffer?
                    try check(CVPixelBufferPoolCreatePixelBuffer(nil, pool, &pixel))
                    guard let pixel else { throw failure("Missing pixel buffer") }
                    try check(CVPixelBufferLockBaseAddress(pixel, []))
                    guard let base = CVPixelBufferGetBaseAddress(pixel) else { throw failure("Missing pixel data") }
                    let stride = CVPixelBufferGetBytesPerRow(pixel)
                    memset(base, 0, stride * codedHeight)
                    raw.withUnsafeBytes { bytes in
                        for y in 0..<height { memcpy(base.advanced(by: y * stride), bytes.baseAddress!.advanced(by: y * width * 4), width * 4) }
                    }
                    CVPixelBufferUnlockBaseAddress(pixel, [])
                    try check(VTCompressionSessionEncodeFrame(session, imageBuffer: pixel, presentationTimeStamp: CMTime(value: index, timescale: Int32(fps)), duration: CMTime(value: 1, timescale: Int32(fps)), frameProperties: nil, sourceFrameRefcon: nil, infoFlagsOut: nil))
                    try check(VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: CMTime(value: index, timescale: Int32(fps))))
                    return true
                }
                if !keepGoing { return 0 }
                index += 1
            }
        } catch {
            fputs("Frames H.264: \(error.localizedDescription)\n", stderr)
            return 1
        }
    }
}
