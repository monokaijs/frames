import Foundation
import CoreVideo
import Darwin

@main struct NativeVideoTests {
    static func exact(_ fd:Int32,_ length:Int)throws->Data{
        var result=Data(count:length)
        try result.withUnsafeMutableBytes{raw in var offset=0;while offset<length{
            var event=pollfd(fd:fd,events:Int16(POLLIN),revents:0)
            guard poll(&event,1,5000)>0 else{throw Wire.failure("Native encoder timed out")}
            let read=Darwin.read(fd,raw.baseAddress!.advanced(by:offset),length-offset)
            guard read>0 else{throw Wire.failure("Truncated native packet")};offset+=read
        }};return result
    }
    static func packet(_ fd:Int32,kind:UInt32,generation:UInt32)throws->ClientVideoFrame{
        let header=try exact(fd,32)
        func value(_ bytes:Data,_ start:Int,_ length:Int)->UInt64{(0..<length).reduce(0){$0|UInt64(bytes[start+$1])<<($1*8)}}
        guard header.prefix(4)==Data("FRM1".utf8),value(header,4,4)==3 else{throw Wire.failure("Invalid native encoded header")}
        let payload=try exact(fd,Int(value(header,24,8)))
        var bytes=Data([payload[0]]);bytes.append(payload.dropFirst(37))
        return ClientVideoFrame(id:42,generation:1,width:Int(value(header,16,4)),height:Int(value(header,20,4)),bytes:bytes,streamGeneration:generation,number:value(payload,9,8),kind:kind)
    }
    static func main()throws{
        let pipe=Pipe();setenv("FRAMES_ENCODED_FD",String(pipe.fileHandleForWriting.fileDescriptor),1)
        func makeBuffer(_ width:Int,_ height:Int,_ green:Bool)throws->CVPixelBuffer{
            var pixel:CVPixelBuffer?
            let attributes:[CFString:Any]=[kCVPixelBufferIOSurfacePropertiesKey:[:] as CFDictionary,kCVPixelBufferMetalCompatibilityKey:true]
            precondition(CVPixelBufferCreate(nil,width,height,kCVPixelFormatType_32BGRA,attributes as CFDictionary,&pixel)==kCVReturnSuccess)
            let buffer=pixel!;CVPixelBufferLockBaseAddress(buffer,[])
            let base=CVPixelBufferGetBaseAddress(buffer)!.assumingMemoryBound(to:UInt8.self),stride=CVPixelBufferGetBytesPerRow(buffer)
            for y in 0..<height{for x in 0..<width{let p=base.advanced(by:y*stride+x*4);p[0]=20;p[1]=green ? 220:30;p[2]=green ? 30:220;p[3]=255}}
            CVPixelBufferUnlockBaseAddress(buffer,[]);return buffer
        }
        for (index,name) in ["h264","hevc"].enumerated(){
            for (width,height) in [(640,360),(1920,1080),(2560,1440),(3840,2160),(129,97)] {
                let encoder=try NativeVideoEncoder(handle:42,codec:name,fps:width>=2560 ? 30:60)
                encoder.submit(try makeBuffer(width,height,false),region:CGRect(x:0,y:0,width:width,height:height))
                let decoders=NativeVideoDecoders(),inbox=ClientVideoInbox{_ in}
                for iteration in 0..<4{
                    if iteration==1{encoder.keyframe()}
                    if iteration==2{encoder.submit(try makeBuffer(width,height,true),region:CGRect(x:0,y:0,width:width,height:height))}
                    if iteration==3{encoder.configure(bitrate:8_000_000,fps:45,scale:75)}
                    let frame=try packet(pipe.fileHandleForReading.fileDescriptor,kind:name=="hevc" ? 3:2,generation:UInt32(index+1))
                    let scale=iteration==3 ? 75:100
                    precondition(frame.width==max(256,(width*scale/100+1) & ~1) && frame.height==max(256,(height*scale/100+1) & ~1))
                    if iteration<2 || iteration==3 { precondition(frame.bytes[0]==1,"recovery/configuration must be independent") }
                    decoders.decode(frame,into:inbox)
                    var decoded=inbox.take();let deadline=Date().addingTimeInterval(5)
                    while decoded.isEmpty && Date()<deadline {Thread.sleep(forTimeInterval:0.001);decoded=inbox.take()}
                    precondition(decoded.count==1 && decoded[0].pixelBuffer != nil)
                    let pixel=decoded[0].pixelBuffer!;CVPixelBufferLockBaseAddress(pixel,.readOnly)
                    let center=CVPixelBufferGetBaseAddress(pixel)!.assumingMemoryBound(to:UInt8.self).advanced(by:CVPixelBufferGetHeight(pixel)/2*CVPixelBufferGetBytesPerRow(pixel)+CVPixelBufferGetWidth(pixel)/2*4)
                    let channel=iteration>=2 ? 1:2
                    precondition(center[channel]>150 && center[channel==1 ? 2:1]<100,"incorrect or stale GPU pixels")
                    CVPixelBufferUnlockBaseAddress(pixel,.readOnly)
                }
                encoder.stop()
            }
        }
        print("PASS: native H264/HEVC GPU surfaces, compressed IPC, changed pixels, idle keyframes, 75% scaling, odd dimensions, 1080p/1440p/4K")
    }
}
