import Foundation
import Darwin

@main struct ClientMediaTests {
    static func main() throws {
        var wakes = 0
        let inbox = ClientVideoInbox { _ in wakes += 1 }
        for value in 0..<1000 {
            inbox.push(ClientVideoFrame(id: 42, generation: 7, width: 1, height: 1, bytes: Data(repeating: UInt8(value % 256), count: 4)))
        }
        precondition(wakes == 1)
        let latest = inbox.take()
        precondition(latest.count == 1 && latest[0].key == "42:7" && latest[0].bytes[0] == UInt8(999 % 256))
        inbox.push(latest[0]); precondition(wakes == 2)
        let pipe = Pipe()
        var header = Data("FRV1".utf8)
        func append<T: FixedWidthInteger>(_ value: T) { var value = value.littleEndian; withUnsafeBytes(of: &value) { header.append(contentsOf: $0) } }
        append(UInt32(1)); append(UInt64(42)); append(UInt32(7)); append(UInt32(2)); append(UInt32(1)); append(UInt32(0)); append(UInt64(8))
        var packet = header; packet.append(Data(repeating: 17, count: 8))
        let sent = DispatchSemaphore(value: 0)
        DispatchQueue.global().async {
            for byte in packet { pipe.fileHandleForWriting.write(Data([byte])) }
            sent.signal()
        }
        let frame = try ClientVideoInbox.readFrame(pipe.fileHandleForReading.fileDescriptor)
        precondition(frame.key == "42:7" && frame.width == 2 && frame.height == 1 && frame.bytes == Data(repeating: 17, count: 8))
        sent.wait()
        // A hostile length must fail before allocating a pixel buffer.
        var malformed = header; malformed[32] = 9
        pipe.fileHandleForWriting.write(malformed)
        do { _ = try ClientVideoInbox.readFrame(pipe.fileHandleForReading.fileDescriptor); fatalError("accepted malformed frame") }
        catch { }
        print("PASS: binary client video, short reads, length validation, latest-frame coalescing")
    }
}
