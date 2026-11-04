import CFramesMac
import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

private final class DecodeCompletion: @unchecked Sendable {
    let complete = DispatchSemaphore(value: 0)
    let lock = NSLock()
    var output: CVPixelBuffer?
    var status: OSStatus = noErr
}

/// Synchronous hardware VideoToolbox decoder for Annex B access units,
/// producing IOSurface-backed BGRA buffers the Metal presenter can map.
final class Decoder: @unchecked Sendable {
    let codec: CMVideoCodecType
    private var session: VTDecompressionSession?
    private var format: CMVideoFormatDescription?
    private let completion = DecodeCompletion()
    private var completionPointer: UnsafeMutableRawPointer?

    init(codec: UInt32) {
        self.codec = codec == UInt32(FRAMES_MAC_CODEC_HEVC) ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264
    }

    deinit {
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
        }
        if let completionPointer { Unmanaged<DecodeCompletion>.fromOpaque(completionPointer).release() }
    }

    private func nalType(_ unit: Data) -> Int {
        guard let first = unit.first else { return -1 }
        return codec == kCMVideoCodecType_HEVC ? Int((first >> 1) & 63) : Int(first & 31)
    }

    private func isParameterSet(_ unit: Data) -> Bool {
        codec == kCMVideoCodecType_HEVC ? [32, 33, 34].contains(nalType(unit)) : [7, 8].contains(nalType(unit))
    }

    private func configure(_ parameters: [Data]) -> Int32 {
        guard parameters.count >= (codec == kCMVideoCodecType_HEVC ? 3 : 2) else { return Int32(FRAMES_MAC_DECODE_INVALID) }
        var created: CMVideoFormatDescription?
        let data = parameters.map { $0 as NSData }
        let pointers = data.map { $0.bytes.assumingMemoryBound(to: UInt8.self) }
        let sizes = data.map(\.length)
        let status = pointers.withUnsafeBufferPointer { pointers in
            sizes.withUnsafeBufferPointer { sizes in
                codec == kCMVideoCodecType_HEVC
                    ? CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        allocator: nil, parameterSetCount: data.count, parameterSetPointers: pointers.baseAddress!,
                        parameterSetSizes: sizes.baseAddress!, nalUnitHeaderLength: 4, extensions: nil,
                        formatDescriptionOut: &created)
                    : CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        allocator: nil, parameterSetCount: data.count, parameterSetPointers: pointers.baseAddress!,
                        parameterSetSizes: sizes.baseAddress!, nalUnitHeaderLength: 4, formatDescriptionOut: &created)
            }
        }
        guard status == noErr, let created else { return Int32(FRAMES_MAC_DECODE_INVALID) }
        if let session, VTDecompressionSessionCanAcceptFormatDescription(session, formatDescription: created) {
            format = created
            return 0
        }
        if let session {
            VTDecompressionSessionWaitForAsynchronousFrames(session)
            VTDecompressionSessionInvalidate(session)
            self.session = nil
        }
        if completionPointer == nil {
            completionPointer = Unmanaged.passRetained(completion).toOpaque()
        }
        var callback = VTDecompressionOutputCallbackRecord(
            decompressionOutputCallback: { opaque, _, status, _, image, _, _ in
                guard let opaque else { return }
                let completion = Unmanaged<DecodeCompletion>.fromOpaque(opaque).takeUnretainedValue()
                completion.lock.lock()
                completion.output = image
                completion.status = status
                completion.lock.unlock()
                completion.complete.signal()
            }, decompressionOutputRefCon: completionPointer)
        let attributes: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
            kCVPixelBufferMetalCompatibilityKey: true,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
        ]
        let specification: [CFString: Any] = [kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: true]
        var opened: VTDecompressionSession?
        let result = VTDecompressionSessionCreate(
            allocator: nil, formatDescription: created, decoderSpecification: specification as CFDictionary,
            imageBufferAttributes: attributes as CFDictionary, outputCallback: &callback,
            decompressionSessionOut: &opened)
        guard result == noErr, let opened else { return Int32(FRAMES_MAC_DECODE_FAILED) }
        VTSessionSetProperty(opened, key: kVTDecompressionPropertyKey_RealTime, value: kCFBooleanTrue)
        session = opened
        format = created
        return 0
    }

    func decode(_ data: Data, keyframe: Bool) -> (Int32, CVPixelBuffer?) {
        let units = nals(data)
        let coldStart = session == nil
        if keyframe {
            let status = configure(units.filter(isParameterSet))
            guard status == 0 else { return (status, nil) }
        }
        guard let session, let format else { return (Int32(FRAMES_MAC_DECODE_NEED_KEYFRAME), nil) }
        var avcc = Data()
        for unit in units where !isParameterSet(unit) {
            var length = UInt32(unit.count).bigEndian
            withUnsafeBytes(of: &length) { avcc.append(contentsOf: $0) }
            avcc.append(unit)
        }
        guard !avcc.isEmpty else { return (Int32(FRAMES_MAC_DECODE_INVALID), nil) }
        var block: CMBlockBuffer?
        guard
            CMBlockBufferCreateWithMemoryBlock(
                allocator: nil, memoryBlock: nil, blockLength: avcc.count, blockAllocator: nil,
                customBlockSource: nil, offsetToData: 0, dataLength: avcc.count, flags: 0, blockBufferOut: &block)
                == noErr, let block,
            avcc.withUnsafeBytes({
                CMBlockBufferReplaceDataBytes(
                    with: $0.baseAddress!, blockBuffer: block, offsetIntoDestination: 0, dataLength: avcc.count)
            }) == noErr
        else { return (Int32(FRAMES_MAC_DECODE_FAILED), nil) }
        var size = avcc.count
        var sample: CMSampleBuffer?
        guard
            CMSampleBufferCreateReady(
                allocator: nil, dataBuffer: block, formatDescription: format, sampleCount: 1,
                sampleTimingEntryCount: 0, sampleTimingArray: nil, sampleSizeEntryCount: 1, sampleSizeArray: &size,
                sampleBufferOut: &sample) == noErr, let sample
        else { return (Int32(FRAMES_MAC_DECODE_FAILED), nil) }
        // A completion that arrived after an earlier deadline must not be
        // mistaken for this picture's.
        while completion.complete.wait(timeout: .now()) == .success {}
        completion.lock.lock()
        completion.output = nil
        completion.status = noErr
        completion.lock.unlock()
        // The deadline detects a hung decoder: a new session's first picture
        // and keyframes legitimately take longer on a cold or virtual GPU.
        let deadline = coldStart ? 1.0 : (keyframe ? 0.25 : 0.05)
        let status = VTDecompressionSessionDecodeFrame(
            session, sampleBuffer: sample, flags: [._EnableAsynchronousDecompression], frameRefcon: nil,
            infoFlagsOut: nil)
        guard status == noErr else { return (Int32(FRAMES_MAC_DECODE_FAILED), nil) }
        guard completion.complete.wait(timeout: .now() + deadline) == .success else {
            return (Int32(FRAMES_MAC_DECODE_TIMEOUT), nil)
        }
        completion.lock.lock()
        let output = completion.output
        let result = completion.status
        completion.output = nil
        completion.lock.unlock()
        guard result == noErr, let output else { return (Int32(FRAMES_MAC_DECODE_FAILED), nil) }
        return (Int32(FRAMES_MAC_DECODE_OK), output)
    }
}

/// Splits an Annex B stream at 3- and 4-byte start codes.
func nals(_ data: Data) -> [Data] {
    let bytes = [UInt8](data)
    var starts: [(Int, Int)] = []
    var i = 0
    while i + 3 <= bytes.count {
        if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
            starts.append((i, i + 3))
            i += 3
        } else if i + 4 <= bytes.count && bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 0 && bytes[i + 3] == 1 {
            starts.append((i, i + 4))
            i += 4
        } else {
            i += 1
        }
    }
    return starts.enumerated().compactMap { index, start in
        let end = index + 1 < starts.count ? starts[index + 1].0 : bytes.count
        return end > start.1 ? Data(bytes[start.1..<end]) : nil
    }
}

@_cdecl("frames_mac_decoder_new")
public func framesMacDecoderNew(_ codec: UInt32) -> UnsafeMutableRawPointer? {
    retainedHandle(Decoder(codec: codec))
}

@_cdecl("frames_mac_decoder_decode")
public func framesMacDecoderDecode(
    _ handle: UnsafeMutableRawPointer?, _ data: UnsafePointer<UInt8>?, _ length: Int, _ keyframe: Bool,
    _ output: UnsafeMutablePointer<UnsafeMutableRawPointer?>?
) -> Int32 {
    guard let handle, let data, length > 0, length <= 8 * 1024 * 1024 else { return Int32(FRAMES_MAC_DECODE_INVALID) }
    let (status, buffer) = borrowed(handle, as: Decoder.self).decode(Data(bytes: data, count: length), keyframe: keyframe)
    if let buffer, let output {
        output.pointee = Unmanaged.passRetained(buffer).toOpaque()
    }
    return status
}

@_cdecl("frames_mac_decoder_free")
public func framesMacDecoderFree(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    _ = released(handle, as: Decoder.self)
}
