import CFramesMac
import CoreVideo
import Foundation

/// IOSurface-backed BGRA pixel buffers, the format the encoder, decoder, and
/// presenter exchange without copies.
func makeBGRABuffer(width: Int, height: Int) -> CVPixelBuffer? {
    let attributes: [CFString: Any] = [
        kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
        kCVPixelBufferWidthKey: width,
        kCVPixelBufferHeightKey: height,
        kCVPixelBufferMetalCompatibilityKey: true,
        kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
    ]
    var buffer: CVPixelBuffer?
    guard
        CVPixelBufferCreate(nil, width, height, kCVPixelFormatType_32BGRA, attributes as CFDictionary, &buffer)
            == kCVReturnSuccess
    else { return nil }
    return buffer
}

/// Fills `buffer` with a desktop-like generated picture for `index`: banded
/// background with fine detail, a moving bar, and the index as 8x8-pixel bit
/// blocks. Deterministic, so a receiver can regenerate the source to check
/// decoded pixels.
func fillSynthetic(_ buffer: CVPixelBuffer, index: UInt64) {
    CVPixelBufferLockBaseAddress(buffer, [])
    defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
    guard let base = CVPixelBufferGetBaseAddress(buffer) else { return }
    let width = CVPixelBufferGetWidth(buffer)
    let height = CVPixelBufferGetHeight(buffer)
    let stride = CVPixelBufferGetBytesPerRow(buffer)
    let bar = Int((index * 12) % UInt64(max(1, width - 200)))
    for row in 0..<height {
        let line = base.advanced(by: row * stride)
        let shade = UInt32(40 + (row / 6) % 160)
        // Thin "text" lines every 18 rows give the encoder real detail.
        var background: UInt32 = row % 18 < 2 ? 0xFF20_2020 : (0xFF00_0000 | shade << 16 | shade << 8 | 200)
        memset_pattern4(line, &background, width * 4)
        var highlight: UInt32 = 0xFFF0_C040
        memset_pattern4(line.advanced(by: bar * 4), &highlight, min(200, width - bar) * 4)
    }
    // 32 bits of the index as white/black 8x8 blocks in the top-left corner.
    for bit in 0..<32 {
        var color: UInt32 = (index >> UInt64(bit)) & 1 == 1 ? 0xFFFF_FFFF : 0xFF00_0000
        for row in 0..<8 where row < height {
            let x = bit * 8
            guard x + 8 <= width else { break }
            memset_pattern4(base.advanced(by: row * stride + x * 4), &color, 8 * 4)
        }
    }
}

@_cdecl("frames_mac_synthetic_frame")
public func framesMacSyntheticFrame(_ width: UInt32, _ height: UInt32, _ index: UInt64) -> UnsafeMutableRawPointer? {
    guard let buffer = makeBGRABuffer(width: Int(width), height: Int(height)) else { return nil }
    fillSynthetic(buffer, index: index)
    return Unmanaged.passRetained(buffer).toOpaque()
}

/// Mean absolute difference per B, G, R channel between two BGRA buffers.
func pixelDifference(_ a: CVPixelBuffer, _ b: CVPixelBuffer) -> Double {
    guard CVPixelBufferGetPixelFormatType(a) == kCVPixelFormatType_32BGRA,
        CVPixelBufferGetPixelFormatType(b) == kCVPixelFormatType_32BGRA,
        CVPixelBufferGetWidth(a) == CVPixelBufferGetWidth(b),
        CVPixelBufferGetHeight(a) == CVPixelBufferGetHeight(b)
    else { return -1 }
    CVPixelBufferLockBaseAddress(a, .readOnly)
    CVPixelBufferLockBaseAddress(b, .readOnly)
    defer {
        CVPixelBufferUnlockBaseAddress(a, .readOnly)
        CVPixelBufferUnlockBaseAddress(b, .readOnly)
    }
    guard let pa = CVPixelBufferGetBaseAddress(a)?.assumingMemoryBound(to: UInt8.self),
        let pb = CVPixelBufferGetBaseAddress(b)?.assumingMemoryBound(to: UInt8.self)
    else { return -1 }
    let width = CVPixelBufferGetWidth(a)
    let height = CVPixelBufferGetHeight(a)
    let sa = CVPixelBufferGetBytesPerRow(a)
    let sb = CVPixelBufferGetBytesPerRow(b)
    var total: UInt64 = 0
    for row in 0..<height {
        let ra = pa + row * sa
        let rb = pb + row * sb
        for x in 0..<width {
            let i = x * 4
            total += UInt64(abs(Int(ra[i]) - Int(rb[i])))
            total += UInt64(abs(Int(ra[i + 1]) - Int(rb[i + 1])))
            total += UInt64(abs(Int(ra[i + 2]) - Int(rb[i + 2])))
        }
    }
    return Double(total) / Double(width * height * 3)
}

@_cdecl("frames_mac_pixel_buffer_difference")
public func framesMacPixelBufferDifference(_ a: UnsafeMutableRawPointer?, _ b: UnsafeMutableRawPointer?) -> Double {
    guard let a, let b else { return -1 }
    return pixelDifference(borrowedPixelBuffer(a), borrowedPixelBuffer(b))
}
