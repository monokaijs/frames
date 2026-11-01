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
        var header = Data("FRV2".utf8)
        func append<T: FixedWidthInteger>(_ value: T) { var value = value.littleEndian; withUnsafeBytes(of: &value) { header.append(contentsOf: $0) } }
        append(UInt32(1)); append(UInt64(42)); append(UInt32(7)); append(UInt32(2)); append(UInt32(1)); append(UInt32(0)); append(UInt64(8));append(UInt64(42))
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
        var controlWakes=0
        let controls=ClientControlInbox(capacity:4){_ in controlWakes+=1}
        controls.push(["type":"peer","name":"fixture"])
        for sequence in 0..<1000{controls.push(["type":"windows","sequence":sequence])}
        controls.push(["type":"shutdown"])
        let batch=controls.take()
        precondition(controlWakes==1 && batch.count==3 && batch[1]["sequence"] as? Int==999)
        precondition(batch[0]["type"] as? String=="peer" && batch[2]["type"] as? String=="shutdown")
        controls.close();precondition(!controls.push(["type":"peer"]))
        print("PASS: binary client video, length validation, bounded control and frame coalescing")
    }
}
