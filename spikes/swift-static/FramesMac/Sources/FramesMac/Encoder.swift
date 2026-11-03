import CFramesMac
import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

/// Minimal low-latency VideoToolbox encoder: infinite GOP, no reordering,
/// real-time, optional long-term references (LTR) for keyframe-free recovery.
final class Encoder: @unchecked Sendable {
    let session: VTCompressionSession
    private let callbacks: Callbacks
    private let lock = NSLock()
    private var acknowledged: [NSNumber] = []  // guarded by lock
    let configuration: String

    init?(width: Int32, height: Int32, fps: UInt32, bitrate: UInt32, codec: UInt32, flags: UInt32, callbacks: Callbacks) {
        self.callbacks = callbacks
        let codecType = codec == UInt32(FRAMES_MAC_CODEC_HEVC) ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264
        let specification: [CFString: Any] = [
            kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true,
            kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true,
        ]
        var created: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: nil, width: width, height: height, codecType: codecType,
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
        // 0 = no limit: keyframes only when forced (infinite GOP).
        set(kVTCompressionPropertyKey_MaxKeyFrameInterval, 0 as CFNumber, "MaxKeyFrameInterval=0")
        set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, 0 as CFNumber, "MaxKeyFrameIntervalDuration=0")
        set(kVTCompressionPropertyKey_AverageBitRate, bitrate as CFNumber, "AverageBitRate")
        set(kVTCompressionPropertyKey_ExpectedFrameRate, fps as CFNumber, "ExpectedFrameRate")
        set(kVTCompressionPropertyKey_MaxFrameDelayCount, 0 as CFNumber, "MaxFrameDelayCount=0")
        if codecType == kCMVideoCodecType_H264 {
            set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel, "Profile")
        }
        if flags & UInt32(FRAMES_MAC_ENCODER_LTR) != 0 {
            set(kVTCompressionPropertyKey_EnableLTR, kCFBooleanTrue, "EnableLTR")
        }
        if flags & UInt32(FRAMES_MAC_ENCODER_SLICES) != 0 && codecType == kCMVideoCodecType_H264 {
            // Cap slice size near one datagram so each slice is independently decodable.
            set(kVTCompressionPropertyKey_MaxH264SliceBytes, 1100 as CFNumber, "MaxH264SliceBytes=1100")
        }
        report.append("Prepare=\(VTCompressionSessionPrepareToEncodeFrames(session))")
        configuration = report.joined(separator: " ")
    }

    func encode(_ buffer: CVPixelBuffer, pts: UInt64, options: UInt32) -> OSStatus {
        var properties: [CFString: Any] = [:]
        if options & UInt32(FRAMES_MAC_ENCODE_FORCE_KEYFRAME) != 0 {
            properties[kVTEncodeFrameOptionKey_ForceKeyFrame] = true
        }
        if options & UInt32(FRAMES_MAC_ENCODE_FORCE_LTR_REFRESH) != 0 {
            properties[kVTEncodeFrameOptionKey_ForceLTRRefresh] = true
        }
        lock.lock()
        if !acknowledged.isEmpty {
            properties[kVTEncodeFrameOptionKey_AcknowledgedLTRTokens] = acknowledged
            acknowledged.removeAll()
        }
        lock.unlock()
        let submitted = uptimeNanos()
        let callbacks = self.callbacks
        return VTCompressionSessionEncodeFrame(
            session, imageBuffer: buffer,
            presentationTimeStamp: CMTime(value: Int64(pts), timescale: 1_000_000_000),
            duration: .invalid, frameProperties: properties.isEmpty ? nil : properties as CFDictionary,
            infoFlagsOut: nil
        ) { status, flags, sample in
            guard status == noErr, !flags.contains(.frameDropped), let sample, let data = sample.dataBuffer else {
                callbacks.encoded(nil, keyframe: false, ltrToken: -1, submitted: submitted)
                return
            }
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]]
            let first = attachments?.first
            let keyframe = !((first?[kCMSampleAttachmentKey_NotSync] as? Bool) ?? false)
            let token = (first?[kVTSampleAttachmentKey_RequireLTRAcknowledgementToken] as? NSNumber)?.int64Value ?? -1
            var length = 0
            var pointer: UnsafeMutablePointer<CChar>?
            if CMBlockBufferGetDataPointer(data, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length, dataPointerOut: &pointer) == noErr,
                let pointer, CMBlockBufferIsRangeContiguous(data, atOffset: 0, length: length)
            {
                callbacks.encoded(UnsafeRawBufferPointer(start: pointer, count: length), keyframe: keyframe, ltrToken: token, submitted: submitted)
            } else {
                var copy = Data(count: length)
                copy.withUnsafeMutableBytes { bytes in
                    _ = CMBlockBufferCopyDataBytes(data, atOffset: 0, dataLength: length, destination: bytes.baseAddress!)
                }
                copy.withUnsafeBytes { callbacks.encoded($0, keyframe: keyframe, ltrToken: token, submitted: submitted) }
            }
        }
    }

    func acknowledge(_ token: Int64) {
        lock.lock()
        acknowledged.append(NSNumber(value: token))
        lock.unlock()
    }

    func flush() { VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid) }

    deinit { VTCompressionSessionInvalidate(session) }
}

@_cdecl("frames_mac_encoder_new")
public func framesMacEncoderNew(
    _ width: UInt32, _ height: UInt32, _ fps: UInt32, _ bitrate: UInt32, _ codec: UInt32, _ flags: UInt32,
    _ callbacks: FramesMacCallbacks
) -> UnsafeMutableRawPointer? {
    let callbacks = Callbacks(raw: callbacks)
    guard let encoder = Encoder(width: Int32(width), height: Int32(height), fps: fps, bitrate: bitrate, codec: codec, flags: flags, callbacks: callbacks) else {
        return nil
    }
    callbacks.event(Int32(FRAMES_MAC_EVENT_LOG), "encoder: \(encoder.configuration)")
    return retainedHandle(encoder)
}

@_cdecl("frames_mac_encoder_encode")
public func framesMacEncoderEncode(_ handle: UnsafeMutableRawPointer?, _ buffer: UnsafeMutableRawPointer?, _ pts: UInt64, _ options: UInt32) -> Int32 {
    guard let handle, let buffer else { return -1 }
    let pixelBuffer = Unmanaged<CVPixelBuffer>.fromOpaque(buffer).takeUnretainedValue()
    return borrowed(handle, as: Encoder.self).encode(pixelBuffer, pts: pts, options: options)
}

@_cdecl("frames_mac_encoder_acknowledge_ltr")
public func framesMacEncoderAcknowledgeLtr(_ handle: UnsafeMutableRawPointer?, _ token: Int64) {
    guard let handle else { return }
    borrowed(handle, as: Encoder.self).acknowledge(token)
}

@_cdecl("frames_mac_encoder_flush")
public func framesMacEncoderFlush(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    borrowed(handle, as: Encoder.self).flush()
}

@_cdecl("frames_mac_encoder_free")
public func framesMacEncoderFree(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    _ = released(handle, as: Encoder.self)
}

@_cdecl("frames_mac_probe")
public func framesMacProbe(_ codec: UInt32, _ output: UnsafeMutablePointer<CChar>?, _ capacity: Int) -> Int {
    let codecType = codec == UInt32(FRAMES_MAC_CODEC_HEVC) ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264
    let specification: [CFString: Any] = [
        kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: true,
        kVTVideoEncoderSpecification_EnableLowLatencyRateControl: true,
    ]
    var created: VTCompressionSession?
    var object: [String: Any] = ["codec": codec == UInt32(FRAMES_MAC_CODEC_HEVC) ? "hevc" : "h264"]
    let status = VTCompressionSessionCreate(
        allocator: nil, width: 1920, height: 1080, codecType: codecType,
        encoderSpecification: specification as CFDictionary, imageBufferAttributes: nil,
        compressedDataAllocator: nil, outputCallback: nil, refcon: nil, compressionSessionOut: &created)
    object["create_status"] = Int(status)
    if let session = created {
        var supported: CFDictionary?
        VTSessionCopySupportedPropertyDictionary(session, supportedPropertyDictionaryOut: &supported)
        let keys = ((supported as? [String: Any])?.keys).map { Array($0).sorted() } ?? []
        object["supported_property_count"] = keys.count
        let interesting = [
            kVTCompressionPropertyKey_EnableLTR, kVTCompressionPropertyKey_MaxH264SliceBytes,
            kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ReferenceBufferCount,
            kVTCompressionPropertyKey_MaxFrameDelayCount, kVTCompressionPropertyKey_BaseLayerFrameRateFraction,
            kVTCompressionPropertyKey_SupportsBaseFrameQP, kVTCompressionPropertyKey_ConstantBitRate,
        ].map { $0 as String }
        object["interesting"] = Dictionary(uniqueKeysWithValues: interesting.map { ($0, keys.contains($0)) })
        object["supported"] = keys
        VTCompressionSessionInvalidate(session)
    }
    guard let json = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]) else { return 0 }
    if let output, capacity > 0 {
        let count = min(json.count, capacity - 1)
        json.withUnsafeBytes { bytes in
            output.withMemoryRebound(to: UInt8.self, capacity: capacity) { $0.update(from: bytes.bindMemory(to: UInt8.self).baseAddress!, count: count) }
        }
        output[count] = 0
    }
    return json.count + 1
}
