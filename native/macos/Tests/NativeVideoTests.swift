import Foundation
import CoreVideo
import Darwin
import CoreGraphics
import CoreText
import ImageIO

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
            // Real desktop text has a much larger refresh than flat-color
            // frames. Exercise its pixels and expose its actual transport size.
            let text=try makeBuffer(1920,1080,false)
            CVPixelBufferLockBaseAddress(text,[])
            let textContext=CGContext(data:CVPixelBufferGetBaseAddress(text),width:1920,height:1080,bitsPerComponent:8,bytesPerRow:CVPixelBufferGetBytesPerRow(text),space:CGColorSpaceCreateDeviceRGB(),bitmapInfo:CGImageAlphaInfo.premultipliedFirst.rawValue|CGBitmapInfo.byteOrder32Little.rawValue)!
            textContext.setFillColor(CGColor(gray:0.08,alpha:1));textContext.fill(CGRect(x:0,y:0,width:1920,height:1080))
            let font=CTFontCreateWithName("Menlo" as CFString,13,nil)
            for row in 0..<64 {
                let line=NSAttributedString(string:"\(row): let frame = capture.latest(); // Frames screen-content quality 0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ abcdefghijklmnopqrstuvwxyz " + String(repeating:"[window 1920x1080] ",count:5),attributes:[NSAttributedString.Key(kCTFontAttributeName as String):font,NSAttributedString.Key(kCTForegroundColorAttributeName as String):CGColor(gray:0.92,alpha:1)])
                textContext.textPosition=CGPoint(x:12,y:1060-row*16);CTLineDraw(CTLineCreateWithAttributedString(line),textContext)
            }
            CVPixelBufferUnlockBaseAddress(text,[])
            let textEncoder=try NativeVideoEncoder(handle:42,codec:name,fps:60)
            textEncoder.submit(text,region:CGRect(x:0,y:0,width:1920,height:1080))
            let textFrame=try packet(pipe.fileHandleForReading.fileDescriptor,kind:name=="hevc" ? 3:2,generation:99)
            let textInbox=ClientVideoInbox{_ in},textDecoder=NativeVideoDecoders()
            textDecoder.decode(textFrame,into:textInbox)
            var textDecoded=textInbox.take();let textDeadline=Date().addingTimeInterval(5)
            while textDecoded.isEmpty && Date()<textDeadline {Thread.sleep(forTimeInterval:0.001);textDecoded=textInbox.take()}
            precondition(textDecoded.count==1 && textDecoded[0].pixelBuffer != nil)
            let decodedText=textDecoded[0].pixelBuffer!
            CVPixelBufferLockBaseAddress(text,.readOnly);CVPixelBufferLockBaseAddress(decodedText,.readOnly)
            let original=CVPixelBufferGetBaseAddress(text)!.assumingMemoryBound(to:UInt8.self),output=CVPixelBufferGetBaseAddress(decodedText)!.assumingMemoryBound(to:UInt8.self)
            let sourceStride=CVPixelBufferGetBytesPerRow(text),decodedStride=CVPixelBufferGetBytesPerRow(decodedText)
            var error=0.0,edges=0
            for y in 1..<1079 {for x in 1..<1919 {
                let i=y*sourceStride+x*4
                if abs(Int(original[i])-Int(original[i-4]))>40 || abs(Int(original[i])-Int(original[i-sourceStride]))>40 {
                    error+=Double(abs(Int(original[i])-Int(output[y*decodedStride+x*4])));edges+=1
                }
            }}
            let edgeError=error/Double(max(1,edges))
            precondition(textFrame.width==1920 && textFrame.height==1080 && edgeError<35,"native-resolution text was blurred: edge error \(edgeError)")
            if let path=ProcessInfo.processInfo.environment["FRAMES_SCREEN_TEXT_OUTPUT"] {
                try FileManager.default.createDirectory(atPath:path,withIntermediateDirectories:true)
                try textFrame.bytes.write(to:URL(fileURLWithPath:path+"/screen-text-\(name).bin"))
                for (label,pixel) in [("source",text),("decoded",decodedText)] {
                    let context=CGContext(data:CVPixelBufferGetBaseAddress(pixel),width:1920,height:1080,bitsPerComponent:8,bytesPerRow:CVPixelBufferGetBytesPerRow(pixel),space:CGColorSpaceCreateDeviceRGB(),bitmapInfo:CGImageAlphaInfo.premultipliedFirst.rawValue|CGBitmapInfo.byteOrder32Little.rawValue)!
                    let destination=CGImageDestinationCreateWithURL(URL(fileURLWithPath:path+"/screen-text-\(name)-\(label).png") as CFURL,"public.png" as CFString,1,nil)!
                    CGImageDestinationAddImage(destination,context.makeImage()!,nil);precondition(CGImageDestinationFinalize(destination))
                }
            }
            CVPixelBufferUnlockBaseAddress(decodedText,.readOnly);CVPixelBufferUnlockBaseAddress(text,.readOnly)
            print("SCREEN TEXT: \(name) 1920x1080 keyframe=\(textFrame.bytes.count) bytes edge_error=\(edgeError)")
            textEncoder.stop()
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
            // Focus changes rate without changing dimensions or reopening the
            // session. Timestamps must remain increasing across 24 -> 60.
            let focusEncoder=try NativeVideoEncoder(handle:42,codec:name,fps:24)
            let buffer=try makeBuffer(640,360,false)
            focusEncoder.submit(buffer,region:CGRect(x:0,y:0,width:640,height:360))
            _ = try packet(pipe.fileHandleForReading.fileDescriptor,kind:name=="hevc" ? 3:2,generation:9)
            for fps in [24,24,60,24,60,45,60] {
                focusEncoder.configure(bitrate:12_000_000,fps:fps,scale:100)
                focusEncoder.keyframe()
                let frame=try packet(pipe.fileHandleForReading.fileDescriptor,kind:name=="hevc" ? 3:2,generation:9)
                precondition(frame.width==640 && frame.height==360 && frame.bytes[0]==1,"focus change must preserve native resolution and an independently decodable picture")
            }
            // A stationary desktop must be refined when bandwidth recovers,
            // without requiring another ScreenCaptureKit notification.
            focusEncoder.configure(bitrate:16_000_000,fps:60,scale:100)
            let refined=try packet(pipe.fileHandleForReading.fileDescriptor,kind:name=="hevc" ? 3:2,generation:9)
            precondition(refined.bytes[0]==1 && refined.width==640 && refined.height==360,"idle quality recovery needs a fresh independent picture")
            focusEncoder.stop()
            // A slow media admission must coalesce raw captures, not discard
            // encoded reference pictures in the helper's bounded IPC queue.
            let acknowledged=try NativeVideoEncoder(handle:42,codec:name,fps:60,deliveryToken:42)
            acknowledged.submit(try makeBuffer(640,360,false),region:CGRect(x:0,y:0,width:640,height:360))
            let initial=try packet(pipe.fileHandleForReading.fileDescriptor,kind:name=="hevc" ? 3:2,generation:10)
            let green=try makeBuffer(640,360,true)
            for _ in 0..<100 {acknowledged.submit(green,region:CGRect(x:0,y:0,width:640,height:360))}
            acknowledged.delivered(number:initial.number,refresh:false,token:99)
            var waiting=pollfd(fd:pipe.fileHandleForReading.fileDescriptor,events:Int16(POLLIN),revents:0)
            precondition(poll(&waiting,1,20)==0,"raw captures escaped delivery backpressure")
            acknowledged.delivered(number:initial.number,refresh:false,token:42)
            let latest=try packet(pipe.fileHandleForReading.fileDescriptor,kind:name=="hevc" ? 3:2,generation:10)
            precondition(latest.number==initial.number+1,"encoded references were discarded instead of coalescing raw captures")
            acknowledged.delivered(number:initial.number,refresh:true,token:42)
            waiting.revents=0;precondition(poll(&waiting,1,20)==0,"stale delivery acknowledgement restarted capture")
            acknowledged.delivered(number:latest.number,refresh:true,token:42)
            let recovery=try packet(pipe.fileHandleForReading.fileDescriptor,kind:name=="hevc" ? 3:2,generation:10)
            precondition(recovery.number==latest.number+1 && recovery.bytes[0]==1)
            acknowledged.stop()
        }
        print("PASS: native H264/HEVC GPU surfaces, compressed IPC, changed pixels, idle keyframes, 75% scaling, odd dimensions, 1080p/1440p/4K")
    }
}
