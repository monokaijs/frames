import Foundation

@main struct BridgeOutputTests {
    static func main() {
        let pipe=Pipe()
        setenv("FRAMES_MEDIA_FD",String(pipe.fileHandleForWriting.fileDescriptor),1)
        _ = Wire.mediaOutput
        let started=DispatchSemaphore(value:0)
        let finished=DispatchSemaphore(value:0)
        DispatchQueue.global().async {
            started.signal()
            Wire.sendFrame(handle:42,width:1024,height:1024,bytes:Data(repeating:1,count:4*1024*1024))
            finished.signal()
        }
        started.wait()
        Thread.sleep(forTimeInterval:0.1)
        // The media pipe is deliberately full and is never read in this test.
        let start=ProcessInfo.processInfo.systemUptime
        Wire.send(["id":1,"data":["control_responsive":true]])
        precondition(ProcessInfo.processInfo.systemUptime-start<0.1)
        fputs("PASS: control reply is independent of a blocked media pipe\n",stderr)
        let drained=DispatchSemaphore(value:0)
        DispatchQueue.global().async {
            while !pipe.fileHandleForReading.availableData.isEmpty {}
            drained.signal()
        }
        finished.wait()
        pipe.fileHandleForWriting.closeFile()
        drained.wait()
    }
}
