import CFramesMac
import CoreGraphics
import CoreMedia
import Foundation
@preconcurrency import ScreenCaptureKit  // SCStream is not Sendable

/// Host audio from ScreenCaptureKit (ABI 7), as v1's SystemAudioCapture:
/// everything this Mac plays except Frames itself, or one application's
/// (sharing selected windows of one app). PCM goes to Rust as interleaved
/// f32 at 48 kHz, stereo. The stream's video is a 2x2 picture at 1 fps,
/// which SCStream requires and nothing uses.
final class AudioCapture: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    private let pid: pid_t
    private let callback: FramesMacAudioCallback
    private let context: UnsafeMutableRawPointer?
    private let queue = DispatchQueue(label: "app.frames.audio", qos: .userInteractive)
    // Used only on `queue`.
    private var stream: SCStream?
    private var interleaved: [Float] = []

    init(pid: pid_t, callback: @escaping FramesMacAudioCallback, context: UnsafeMutableRawPointer?) {
        self.pid = pid
        self.callback = callback
        self.context = context
    }

    /// Starts capturing; the error text when it cannot.
    func start() -> String? {
        guard CGPreflightScreenCaptureAccess() else {
            return "host audio needs the Screen Recording permission (System Settings → Privacy & Security → Screen Recording)"
        }
        final class Outcome: @unchecked Sendable { var error: String? }
        let outcome = Outcome()
        let done = DispatchSemaphore(value: 0)
        Task {
            do {
                let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: false)
                guard let display = content.displays.first else {
                    throw NSError(domain: "Frames", code: 1, userInfo: [NSLocalizedDescriptionKey: "no display"])
                }
                let filter: SCContentFilter
                if self.pid == 0 {
                    filter = SCContentFilter(display: display, excludingApplications: [], exceptingWindows: [])
                } else {
                    guard let app = content.applications.first(where: { $0.processID == self.pid }) else {
                        throw NSError(
                            domain: "Frames", code: 2,
                            userInfo: [NSLocalizedDescriptionKey: "the shared application is not running"])
                    }
                    filter = SCContentFilter(display: display, including: [app], exceptingWindows: [])
                }
                let configuration = SCStreamConfiguration()
                configuration.capturesAudio = true
                configuration.excludesCurrentProcessAudio = true
                configuration.sampleRate = 48_000
                configuration.channelCount = 2
                configuration.width = 2
                configuration.height = 2
                configuration.minimumFrameInterval = CMTime(value: 1, timescale: 1)
                configuration.showsCursor = false
                let stream = SCStream(filter: filter, configuration: configuration, delegate: self)
                try stream.addStreamOutput(self, type: .audio, sampleHandlerQueue: self.queue)
                // Screen output too, so ScreenCaptureKit does not log every
                // dropped picture; the pictures are ignored.
                try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: self.queue)
                try await stream.startCapture()
                self.queue.sync { self.stream = stream }
            } catch {
                outcome.error = error.localizedDescription
            }
            done.signal()
        }
        if done.wait(timeout: .now() + 10) == .timedOut {
            return "ScreenCaptureKit did not start audio capture within 10 s"
        }
        return outcome.error
    }

    func stop() {
        let stream: SCStream? = queue.sync {
            let stream = self.stream
            self.stream = nil
            return stream
        }
        if let stream {
            let done = DispatchSemaphore(value: 0)
            stream.stopCapture { _ in done.signal() }
            _ = done.wait(timeout: .now() + 2)
        }
        // Samples already queued are delivered before Rust frees the sink.
        queue.sync {}
    }

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .audio, sampleBuffer.isValid,
            let description = CMSampleBufferGetFormatDescription(sampleBuffer),
            let format = CMAudioFormatDescriptionGetStreamBasicDescription(description)?.pointee,
            format.mFormatID == kAudioFormatLinearPCM,
            format.mFormatFlags & kAudioFormatFlagIsFloat != 0, format.mBitsPerChannel == 32
        else { return }
        let frames = CMSampleBufferGetNumSamples(sampleBuffer)
        let channels = Int(format.mChannelsPerFrame)
        guard frames > 0, channels > 0 else { return }
        let planar = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0
        try? sampleBuffer.withAudioBufferList { list, _ in
            interleaved.removeAll(keepingCapacity: true)
            interleaved.reserveCapacity(frames * 2)
            // Always stereo: mono is doubled, extra channels are dropped.
            let sample: (Int, Int) -> Float
            if planar {
                let buffers = list.map { $0.mData?.assumingMemoryBound(to: Float.self) }
                sample = { frame, channel in buffers[min(channel, buffers.count - 1)]?[frame] ?? 0 }
            } else {
                let data = list.first?.mData?.assumingMemoryBound(to: Float.self)
                sample = { frame, channel in data?[frame * channels + min(channel, channels - 1)] ?? 0 }
            }
            for frame in 0..<frames {
                interleaved.append(sample(frame, 0))
                interleaved.append(sample(frame, 1))
            }
            interleaved.withUnsafeBufferPointer { samples in
                callback(context, samples.baseAddress, frames, 2)
            }
        }
    }

    func stream(_ stream: SCStream, didStopWithError error: any Error) {
        // No samples: the capture ended (Rust stops reading).
        callback(context, nil, 0, 0)
    }
}

@_cdecl("frames_mac_audio_start")
public func framesMacAudioStart(
    _ pid: Int32, _ callback: FramesMacAudioCallback?, _ context: UnsafeMutableRawPointer?,
    _ error: UnsafeMutablePointer<CChar>?, _ errorCapacity: Int
) -> UnsafeMutableRawPointer? {
    guard let callback else { return nil }
    let capture = AudioCapture(pid: pid, callback: callback, context: context)
    if let failure = capture.start() {
        if let error, errorCapacity > 0 {
            let bytes = Array(failure.utf8.prefix(errorCapacity - 1))
            for (index, byte) in bytes.enumerated() { error[index] = CChar(bitPattern: byte) }
            error[bytes.count] = 0
        }
        return nil
    }
    return retainedHandle(capture)
}

@_cdecl("frames_mac_audio_stop")
public func framesMacAudioStop(_ handle: UnsafeMutableRawPointer?) {
    guard let handle else { return }
    released(handle, as: AudioCapture.self).stop()
}
