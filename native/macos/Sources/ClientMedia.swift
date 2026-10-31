import Foundation
import Darwin

struct ClientVideoFrame {
    let id: UInt64
    let generation: UInt32
    let width: Int
    let height: Int
    let bytes: Data
    var key: String { "\(id):\(generation)" }
}

// One pending picture per window, one main-queue wakeup for the entire batch.
// Socket reads and allocation never run on AppKit's main actor.
final class ClientVideoInbox: @unchecked Sendable {
    private let lock = NSLock()
    private var frames: [String: ClientVideoFrame] = [:]
    private var scheduled = false
    private let notify: (ClientVideoInbox) -> Void
    init(notify: @escaping (ClientVideoInbox) -> Void) { self.notify = notify }
    func push(_ frame: ClientVideoFrame) {
        lock.lock()
        if frames[frame.key] == nil && frames.count >= 16, let key = frames.keys.first { frames.removeValue(forKey: key) }
        frames[frame.key] = frame
        let wake = !scheduled
        scheduled = true
        lock.unlock()
        if wake { notify(self) }
    }
    func take() -> [ClientVideoFrame] {
        lock.lock(); defer { lock.unlock() }
        let result = Array(frames.values)
        frames.removeAll(keepingCapacity: true)
        scheduled = false
        return result
    }
    static func readExact(_ fd: Int32, count: Int) throws -> Data {
        var data = Data(count: count)
        try data.withUnsafeMutableBytes { raw in
            var offset = 0
            while offset < count {
                let n = Darwin.read(fd, raw.baseAddress!.advanced(by: offset), count - offset)
                if n < 0 && errno == EINTR { continue }
                guard n > 0 else { throw NSError(domain: "Frames.ClientMedia", code: Int(n), userInfo: [NSLocalizedDescriptionKey: "Video socket closed or truncated"]) }
                offset += n
            }
        }
        return data
    }
    static func readFrame(_ fd: Int32) throws -> ClientVideoFrame {
        let header = try readExact(fd, count: 40)
        func value(_ offset: Int, _ count: Int) -> UInt64 {
            (0..<count).reduce(UInt64(0)) { $0 | UInt64(header[offset + $1]) << ($1 * 8) }
        }
        let width = value(20, 4), height = value(24, 4), size = value(32, 8)
        guard header.prefix(4) == Data("FRV1".utf8), value(4, 4) == 1,
              value(28, 4) == 0, width > 0, width <= 8192, height > 0, height <= 8192,
              size == width * height * 4 else {
            throw NSError(domain: "Frames.ClientMedia", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid video header"])
        }
        return ClientVideoFrame(id: value(8, 8), generation: UInt32(value(16, 4)),
                                width: Int(width), height: Int(height), bytes: try readExact(fd, count: Int(size)))
    }
    func read(_ fd: Int32) {
        defer { Darwin.close(fd) }
        do { while true { push(try Self.readFrame(fd)) } }
        catch { fputs("Frames client video: \(error.localizedDescription)\n", stderr) }
    }
}
