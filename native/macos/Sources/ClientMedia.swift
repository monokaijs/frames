import Foundation
import Darwin
import CoreVideo

struct ClientVideoFrame {
    let id: UInt64
    let generation: UInt32
    let width: Int
    let height: Int
    let bytes: Data
    var streamGeneration:UInt32 = 0
    var number:UInt64 = 0
    var kind:UInt32 = 1
    var pixelBuffer:CVPixelBuffer? = nil
    var decodedAt:UInt64 = 0
    var key: String { "\(id):\(generation)" }
}

// Only the control reader can wait here. Video/audio sockets and native render
// workers continue independently; AppKit receives one coalesced batch wakeup.
final class ClientControlInbox:@unchecked Sendable {
    private let condition=NSCondition(),capacity:Int
    private var messages:[[String:Any]]=[],scheduled=false,stopped=false
    private let notify:(ClientControlInbox)->Void
    init(capacity:Int=64,notify:@escaping(ClientControlInbox)->Void){self.capacity=max(1,capacity);self.notify=notify}
    @discardableResult func push(_ message:[String:Any])->Bool {
        condition.lock()
        guard !stopped else{condition.unlock();return false}
        if let key=Self.key(message),let index=messages.firstIndex(where:{Self.key($0)==key}){messages[index]=message;condition.unlock();return true}
        while !stopped && messages.count>=capacity{condition.wait()}
        guard !stopped else{condition.unlock();return false}
        messages.append(message);let wake = !scheduled;scheduled=true;condition.unlock()
        if wake{notify(self)};return true
    }
    func take()->[[String:Any]]{condition.lock();defer{condition.unlock()};let result=messages;messages.removeAll(keepingCapacity:true);scheduled=false;condition.broadcast();return result}
    func close(){condition.lock();stopped=true;condition.broadcast();condition.unlock()}
    private static func key(_ message:[String:Any])->String? {
        guard let type=message["type"] as? String else{return nil}
        switch type {
        case "windows","remote_cursor","shared_cursor","shared_session","transfer_progress":return type
        case "devices":
            let error=(message["data"] as? [String:Any])?["error"]
            return error == nil || error is NSNull ? type:nil
        case "status","menu","debug":
            guard let window=message["window"] as? [String:Any],let id=window["id"] as? NSNumber,let generation=window["generation"] as? NSNumber else{return nil}
            return "\(type):\(id):\(generation)"
        default:return nil
        }
    }
}

// One pending picture per window, one main-queue wakeup for the entire batch.
// Socket reads and allocation never run on AppKit's main actor.
final class ClientVideoInbox: @unchecked Sendable {
    private let lock = NSLock()
    private var frames: [String: ClientVideoFrame] = [:]
    private var scheduled = false
    private let notify: (ClientVideoInbox) -> Void
    private let encoded: ((ClientVideoFrame, ClientVideoInbox) -> Void)?
    init(encoded: ((ClientVideoFrame, ClientVideoInbox) -> Void)? = nil, notify: @escaping (ClientVideoInbox) -> Void) { self.notify = notify; self.encoded = encoded }
    func push(_ frame: ClientVideoFrame) {
        lock.lock()
        if frames[frame.key] == nil && frames.count >= 16, let key = frames.keys.first { frames.removeValue(forKey: key) }
        if let old=frames[frame.key], old.streamGeneration>frame.streamGeneration || (old.streamGeneration==frame.streamGeneration && old.number>frame.number){lock.unlock();return}
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
        let header = try readExact(fd, count: 48)
        func value(_ offset: Int, _ count: Int) -> UInt64 {
            (0..<count).reduce(UInt64(0)) { $0 | UInt64(header[offset + $1]) << ($1 * 8) }
        }
        let width = value(20, 4), height = value(24, 4), size = value(32, 8)
        let kind=value(4,4)
        guard header.prefix(4) == Data("FRV2".utf8), (1...3).contains(kind),
              width > 0, width <= 8192, height > 0, height <= 8192,
              kind == 1 ? size == width * height * 4 : (size > 1 && size <= 8*1024*1024) else {
            throw NSError(domain: "Frames.ClientMedia", code: 1, userInfo: [NSLocalizedDescriptionKey: "Invalid video header"])
        }
        return ClientVideoFrame(id: value(8, 8), generation: UInt32(value(16, 4)),
                                width: Int(width), height: Int(height), bytes: try readExact(fd, count: Int(size)), streamGeneration:UInt32(value(28,4)),number:value(40,8),kind:UInt32(kind))
    }
    func read(_ fd: Int32) {
        defer { Darwin.close(fd) }
        do { while true { let frame=try Self.readFrame(fd);if frame.kind == 1 { push(frame) } else if let encoded { encoded(frame,self) } } }
        catch { fputs("Frames client video: \(error.localizedDescription)\n", stderr) }
    }
}
