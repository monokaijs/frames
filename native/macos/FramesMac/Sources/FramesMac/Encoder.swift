import CFramesMac
import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

/// Low-latency hardware VideoToolbox encoder: real time, no reordering,
/// keyframes only when forced (infinite GOP), and optional long-term
/// references for keyframe-free recovery. Output is Annex B with parameter
/// sets before every keyframe, as the v1 helper and Windows decoders expect.
///
/// VideoToolbox decides which frames are long-term candidates and tags them
/// with a token. A frame is safe to predict from once its token is
/// acknowledged; ForceLTRRefresh then predicts from an acknowledged one.
final class Encoder: @unchecked Sendable {
    let session: VTCompressionSession
    let codec: CMVideoCodecType
    private let callbacks: Callbacks
    let configuration: String

    init?(width: Int32, height: Int32, fps: UInt32, bitrate: UInt32, codec: UInt32, flags: UInt32, callbacks: Callbacks) {
        self.callbacks = callbacks
        self.codec = codec == UInt32(FRAMES_MAC_CODEC_HEVC) ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264
        let specification: [CFString: Any] = [
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true,
            kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true,
        ]
        var created: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: nil, width: width, height: height, codecType: self.codec,
            encoderSpecification: specification as CFDictionary, imageBufferAttributes: nil,
            compressedDataAllocator: nil, outputCallback: nil, refcon: nil, compressionSessionOut: &created)
        guard status == noErr, let session = created else {
            callbacks.event(Int32(FRAMES_MAC_EVENT_LOG), "VTCompressionSessionCreate failed \(status)")
            return nil
        }
        self.session = session
        var report: [String] = []
        func set(_ key: CFString, _ value: CFTypeRef, _ name: String) {
            let result = VTSessionSetProperty(session, key: key, value: value)
            report.append("\(name)=\(result == noErr ? "ok" : "err \(result)")")
        }
        set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue, "RealTime")
        set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse, "AllowFrameReordering=false")
        // 0 = no limit: keyframes only when forced.
        set(kVTCompressionPropertyKey_MaxKeyFrameInterval, 0 as CFNumber, "MaxKeyFrameInterval=0")
        set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, 0 as CFNumber, "MaxKeyFrameIntervalDuration=0")
        set(kVTCompressionPropertyKey_AverageBitRate, bitrate as CFNumber, "AverageBitRate")
        set(kVTCompressionPropertyKey_ExpectedFrameRate, fps as CFNumber, "ExpectedFrameRate")
        if self.codec == kCMVideoCodecType_H264 {
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel, "Profile")
        }
        set(kVTCompressionPropertyKey_ColorPrimaries, kCVImageBufferColorPrimaries_ITU_R_709_2, "Primaries")
        set(kVTCompressionPropertyKey_TransferFunction, kCVImageBufferTransferFunction_ITU_R_709_2, "Transfer")
        set(kVTCompressionPropertyKey_YCbCrMatrix, kCVImageBufferYCbCrMatrix_ITU_R_709_2, "Matrix")
        if flags & UInt32(FRAMES_MAC_ENCODER_LTR) != 0 {
            set(kVTCompressionPropertyKey_EnableLTR, kCFBooleanTrue, "EnableLTR")
        }
        report.append("Prepare=\(VTCompressionSessionPrepareToEncodeFrames(session))")
        configuration = report.joined(separator: " ")
    }

    func setBitrate(_ bitrate: UInt32, fps: UInt32) -> OSStatus {
        let status = VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AverageBitRate, value: bitrate as CFNumber)
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_ExpectedFrameRate, value: fps as CFNumber)
        return status
    }

    func encode(_ buffer: CVPixelBuffer, pts: UInt64, options: UInt32, acknowledged: [Int64]) -> OSStatus {
        var properties: [CFString: Any] = [:]
        if options & UInt32(FRAMES_MAC_ENCODE_FORCE_KEYFRAME) != 0 {
            properties[kVTEncodeFrameOptionKey_ForceKeyFrame] = true
        }
        if options & UInt32(FRAMES_MAC_ENCODE_FORCE_LTR_REFRESH) != 0 {
            properties[kVTEncodeFrameOptionKey_ForceLTRRefresh] = true
        }
        if !acknowledged.isEmpty {
            properties[kVTEncodeFrameOptionKey_AcknowledgedLTRTokens] = acknowledged.map { NSNumber(value: $0) }
        }
        let submitted = uptimeNanos()
        let callbacks = self.callbacks
        let codec = self.codec
        return VTCompressionSessionEncodeFrame(
            session, imageBuffer: buffer,
            presentationTimeStamp: CMTime(value: Int64(pts), timescale: 1_000_000_000),
            duration: .invalid, frameProperties: properties.isEmpty ? nil : properties as CFDictionary,
            infoFlagsOut: nil
        ) { status, flags, sample in
            guard status == noErr, !flags.contains(.frameDropped), let sample else {
                callbacks.encoded(nil, keyframe: false, ltrToken: -1, submitted: submitted, status: status == noErr ? -1 : status)
                return
            }
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]]
            let first = attachments?.first
            let keyframe = !((first?[kCMSampleAttachmentKey_NotSync] as? Bool) ?? false)
            let token = (first?[kVTSampleAttachmentKey_RequireLTRAcknowledgementToken] as? NSNumber)?.int64Value ?? -1
            guard let bytes = annexB(sample, codec: codec, keyframe: keyframe) else {
                callbacks.encoded(nil, keyframe: false, ltrToken: -1, submitted: submitted, status: -2)
                return
            }
            bytes.withUnsafeBytes {
                callbacks.encoded($0, keyframe: keyframe, ltrToken: token, submitted: submitted, status: 0)
            }
        }
    }

    deinit {
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        VTCompressionSessionInvalidate(session)
    }
}

/// Converts a VideoToolbox sample (4-byte length-prefixed NAL units) to
/// Annex B, prepending the parameter sets to keyframes.
private func annexB(_ sample: CMSampleBuffer, codec: CMVideoCodecType, keyframe: Bool) -> Data? {
    guard let block = CMSampleBufferGetDataBuffer(sample) else { return nil }
    var bytes = Data()
    if keyframe {
        guard let format = CMSampleBufferGetFormatDescription(sample) else { return nil }
        var count = 0
        var length: Int32 = 0
        let hevc = codec == kCMVideoCodecType_HEVC
        func parameter(_ index: Int, count: UnsafeMutablePointer<Int>?, length: UnsafeMutablePointer<Int32>?)
            -> (UnsafePointer<UInt8>, Int)?
        {
            var pointer: UnsafePointer<UInt8>?
            var size = 0
            let status =
                hevc
                ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                    format, parameterSetIndex: index, parameterSetPointerOut: &pointer, parameterSetSizeOut: &size,
                    parameterSetCountOut: count, nalUnitHeaderLengthOut: length)
                : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format, parameterSetIndex: index, parameterSetPointerOut: &pointer, parameterSetSizeOut: &size,
                    parameterSetCountOut: count, nalUnitHeaderLengthOut: length)
            guard status == noErr, let pointer else { return nil }
            return (pointer, size)
        }
        guard parameter(0, count: &count, length: &length) != nil, length == 4 else { return nil }
        for index in 0..<count {
            guard let (pointer, size) = parameter(index, count: nil, length: nil) else { return nil }
            bytes.append(contentsOf: [0, 0, 0, 1])
            bytes.append(pointer, count: size)
        }
    }
    let size = CMBlockBufferGetDataLength(block)
    guard size > 0, size <= 8 * 1024 * 1024 else { return nil }
    var packet = Data(count: size)
    let copied = packet.withUnsafeMutableBytes {
        CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: size, destination: $0.baseAddress!)
    }
    guard copied == noErr else { return nil }
    var offset = 0
    while offset + 4 <= packet.count {
        let length = packet[offset..<offset + 4].reduce(0) { ($0 << 8) | Int($1) }
        offset += 4
        guard length > 0, length <= packet.count - offset else { return nil }
        bytes.append(contentsOf: [0, 0, 0, 1])
        bytes.append(packet[offset..<offset + length])
        offset += length
    }
    return offset == packet.count ? bytes : nil
}

@_cdecl("frames_mac_encoder_new")
public func framesMacEncoderNew(
    _ width: UInt32, _ height: UInt32, _ fps: UInt32, _ bitrate: UInt32, _ codec: UInt32, _ flags: UInt32,
    _ callbacks: FramesMacCallbacks
) -> UnsafeMutableRawPointer? {
    let callbacks = Callbacks(raw: callbacks)
    guard
        let encoder = Encoder(
            width: Int32(width), height: Int32(height), fps: fps, bitrate: bitrate, codec: codec, flags: flags,
            callbacks: callbacks)
    else { return nil }
    callbacks.event(Int32(FRAMES_MAC_EVENT_LOG), "encoder: \(encoder.configuration)")
    return retainedHandle(encoder)
}

@_cdecl("frames_mac_encoder_encode")
public func framesMacEncoderEncode(
    _ handle: UnsafeMutableRawPointer?, _ buffer: UnsafeMutableRawPointer?, _ pts: UInt64, _ options: UInt32,
    _ acknowledged: UnsafePointer<Int64>?, _ acknowledgedCount: Int
) -> Int32 {
    guard let handle, let buffer else { return -1 }
    let tokens = acknowledged.map { Array(UnsafeBufferPointer(start: $0, count: acknowledgedCount)) } ?? []
    return borrowed(handle, as: Encoder.self).encode(
        borrowedPixelBuffer(buffer), pts: pts, options: options, acknowledged: tokens)
}

@_cdecl("frames_mac_encoder_set_bitrate")
public func framesMacEncoderSetBitrate(_ handle: UnsafeMutableRawPointer?, _ bitrate: UInt32, _ fps: UInt32) -> Int32 {
    guard let handle else { return -1 }
    return borrowed(handle, as: Encoder.self).setBitrate(bitrate, fps: fps)
}

@_cdecl("frames_mac_encoder_free")
public func framesMacEncoderFree(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    _ = released(handle, as: Encoder.self)
}
