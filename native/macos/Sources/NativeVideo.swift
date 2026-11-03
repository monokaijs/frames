import Foundation
import CoreMedia
import CoreVideo
import VideoToolbox
import CoreImage
import Metal

// ScreenCaptureKit surfaces stay in this process. Only complete access units
// cross the Rust bridge. The mailbox retains one newest raw capture.
final class NativeVideoEncoder: @unchecked Sendable {
    let handle: UInt64
    let codec: CMVideoCodecType
    private let queue = DispatchQueue(label: "app.frames.hardware-encoder", qos: .userInteractive)
    private let lock = NSLock()
    private var latest: (CVPixelBuffer, CGRect, UInt64, UInt64)?
    private var version: UInt64 = 0
    private var scheduled = false
    private var lastVersion: UInt64 = 0
    private var force = true
    private var busy = false
    private var stopped = false
    private var session: VTCompressionSession?
    private var pool: CVPixelBufferPool?
    private var context: CIContext
    private var dimensions = CGSize.zero
    private var bitrate = 12_000_000
    private var fps: Int32 = 60
    private var scale = 100
    private var number: UInt64 = 0
    private var nextEncode: UInt64 = 0
    private var delayed = false
    private var consecutiveDrops = 0
    private var restartRequired = false
    private var presentationNanoseconds:UInt64 = 0
    private var dropStarted:UInt64?
    private let deliveryToken:UInt64
    private var pendingDelivery:UInt64?
    init(handle: UInt64, codec: String, fps: Int, deliveryToken:UInt64 = 0) throws {
        guard let device = MTLCreateSystemDefaultDevice() else { throw Wire.failure("Metal unavailable") }
        self.handle = handle; self.codec = codec == "hevc" ? kCMVideoCodecType_HEVC : kCMVideoCodecType_H264
        self.fps = Int32(max(1,min(60,fps)))
        self.deliveryToken=deliveryToken
        context = CIContext(mtlDevice: device, options: [.cacheIntermediates:false])
    }
    func submit(_ buffer: CVPixelBuffer, region: CGRect, captured:UInt64 = DispatchTime.now().uptimeNanoseconds) {
        lock.lock(); version &+= 1
        latest = (buffer, region, captured, version)
        let wake = !scheduled; scheduled = true; lock.unlock()
        if wake { queue.async { [weak self] in self?.drain() } }
    }
    func keyframe() { queue.async { [weak self] in self?.force = true; self?.drain() } }
    func delivered(number:UInt64,refresh:Bool,token:UInt64) {
        queue.async { [weak self] in
            guard let self,!self.stopped,token==self.deliveryToken,self.pendingDelivery==number else{return}
            self.pendingDelivery=nil;self.busy=false;self.force = self.force || refresh;self.drain()
        }
    }
    func configure(bitrate: Int, fps: Int, scale: Int) {
        queue.async { [weak self] in
            guard let self else { return }
            let changed = self.scale != scale
            let rateChanged=self.bitrate != bitrate || self.fps != fps
            let improvePicture = bitrate > self.bitrate + max(64_000,self.bitrate/25)
            self.bitrate = max(128_000,min(100_000_000,bitrate));self.fps = Int32(max(1,min(60,fps)));self.scale = max(50,min(100,scale))
            if let session,rateChanged {
                let status=VTSessionSetProperty(session,key:kVTCompressionPropertyKey_AverageBitRate,value:self.bitrate as CFNumber)
                if status != noErr{self.restartRequired=true;Wire.diagnostic("encoder_restart_required",["reason":"VideoToolbox rejected live bitrate update","status":status])}
                VTSessionSetProperty(session,key:kVTCompressionPropertyKey_ExpectedFrameRate,value:self.fps as CFNumber)
            }
            // An idle desktop needs a fresh independent picture when its
            // quality budget recovers; otherwise the old blurry image remains.
            if changed || improvePicture || self.restartRequired { self.force = true; self.drain() }
        }
    }
    func stop() { queue.async { [self] in stopped = true; if let session { VTCompressionSessionCompleteFrames(session,untilPresentationTimeStamp:.invalid);VTCompressionSessionInvalidate(session) };session = nil;pool = nil;lock.lock();latest = nil;lock.unlock() } }
    private func open(width: Int, height: Int) throws {
        if let session { VTCompressionSessionCompleteFrames(session,untilPresentationTimeStamp:.invalid);VTCompressionSessionInvalidate(session) };session = nil;pool = nil
        let specification: [CFString:Any] = [kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder:true,kVTVideoEncoderSpecification_EnableLowLatencyRateControl:true]
        let status = VTCompressionSessionCreate(allocator:kCFAllocatorDefault,width:Int32(max(256,(width+1) & ~1)),height:Int32(max(256,(height+1) & ~1)),codecType:codec,encoderSpecification:specification as CFDictionary,imageBufferAttributes:nil,compressedDataAllocator:nil,outputCallback:{ opaque, ticket, status, _, sample in
            guard let ticket else { return }
            let frame = Unmanaged<NativeEncodeTicket>.fromOpaque(ticket).takeRetainedValue()
            frame.owner.output(frame, status:status, sample:sample)
        },refcon:nil,compressionSessionOut:&session)
        guard status == noErr, let session else { throw Wire.failure("Hardware VideoToolbox encoder unavailable (\(status))") }
        for (key,value) in [(kVTCompressionPropertyKey_RealTime,kCFBooleanTrue),(kVTCompressionPropertyKey_AllowFrameReordering,kCFBooleanFalse)] {
            guard VTSessionSetProperty(session,key:key,value:value) == noErr else { throw Wire.failure("Low latency encoder configuration rejected") }
        }
        guard VTSessionSetProperty(session,key:kVTCompressionPropertyKey_AverageBitRate,value:bitrate as CFNumber) == noErr else{throw Wire.failure("VideoToolbox cannot apply the bitrate budget")}
        VTSessionSetProperty(session,key:kVTCompressionPropertyKey_ExpectedFrameRate,value:fps as CFNumber)
        // Same periodic safety-net interval as FRAMES_KEYFRAME_INTERVAL in native.c.
        VTSessionSetProperty(session,key:kVTCompressionPropertyKey_MaxKeyFrameInterval,value:60 as CFNumber)
        if codec == kCMVideoCodecType_H264 { VTSessionSetProperty(session,key:kVTCompressionPropertyKey_ProfileLevel,value:kVTProfileLevel_H264_ConstrainedHigh_AutoLevel) }
        VTSessionSetProperty(session,key:kVTCompressionPropertyKey_MaxFrameDelayCount,value:0 as CFNumber)
        VTSessionSetProperty(session,key:kVTCompressionPropertyKey_ColorPrimaries,value:kCVImageBufferColorPrimaries_ITU_R_709_2)
        VTSessionSetProperty(session,key:kVTCompressionPropertyKey_TransferFunction,value:kCVImageBufferTransferFunction_ITU_R_709_2)
        VTSessionSetProperty(session,key:kVTCompressionPropertyKey_YCbCrMatrix,value:kCVImageBufferYCbCrMatrix_ITU_R_709_2)
        guard VTCompressionSessionPrepareToEncodeFrames(session) == noErr else { throw Wire.failure("Cannot prepare VideoToolbox encoder") }
        let attributes: [CFString:Any] = [kCVPixelBufferWidthKey:max(256,(width+1) & ~1),kCVPixelBufferHeightKey:max(256,(height+1) & ~1),kCVPixelBufferPixelFormatTypeKey:kCVPixelFormatType_32BGRA,kCVPixelBufferMetalCompatibilityKey:true,kCVPixelBufferIOSurfacePropertiesKey:[:] as CFDictionary]
        guard CVPixelBufferPoolCreate(nil,[kCVPixelBufferPoolMinimumBufferCountKey:3] as CFDictionary,attributes as CFDictionary,&pool) == kCVReturnSuccess else { throw Wire.failure("Cannot allocate hardware encoder surfaces") }
        dimensions = CGSize(width:width,height:height);force = true;restartRequired = false
    }
    private func drain() {
        lock.lock();scheduled = false;let source = latest;lock.unlock()
        guard !stopped, !busy, let (buffer,region,captured,version) = source, force || version != lastVersion else { return }
        let now=DispatchTime.now().uptimeNanoseconds
        if !force && now<nextEncode {
            if !delayed { delayed=true;queue.asyncAfter(deadline:DispatchTime(uptimeNanoseconds:nextEncode)){[weak self] in self?.delayed=false;self?.drain()} }
            return
        }
        nextEncode=now+1_000_000_000/UInt64(fps)
        let width = max(256,(Int(region.width)*scale/100+1) & ~1),height = max(256,(Int(region.height)*scale/100+1) & ~1)
        do {
            if session == nil || restartRequired || dimensions != CGSize(width:width,height:height) { try open(width:width,height:height) }
            guard let session, let pool else { return }
            var target: CVPixelBuffer?
            guard CVPixelBufferPoolCreatePixelBufferWithAuxAttributes(nil,pool,[kCVPixelBufferPoolAllocationThresholdKey:3] as CFDictionary,&target) == kCVReturnSuccess,let target else { return }
            let crop = CGRect(x:region.minX,y:CGFloat(CVPixelBufferGetHeight(buffer))-region.maxY,width:region.width,height:region.height)
            let image = CIImage(cvPixelBuffer:buffer).cropped(to:crop).transformed(by:CGAffineTransform(translationX:-crop.minX,y:-crop.minY)).transformed(by:CGAffineTransform(scaleX:CGFloat(width)/region.width,y:CGFloat(height)/region.height)).clampedToExtent()
            context.render(image,to:target,bounds:CGRect(x:0,y:0,width:CVPixelBufferGetWidth(target),height:CVPixelBufferGetHeight(target)),colorSpace:CGColorSpaceCreateDeviceRGB())
            let ticket = NativeEncodeTicket(owner:self,width:width,height:height,sourceWidth:Int(region.width),sourceHeight:Int(region.height),captured:captured,number:number,started:now,source:buffer,target:target)
            lastVersion = version;busy = true
            let properties: CFDictionary? = force ? [kVTEncodeFrameOptionKey_ForceKeyFrame:true] as CFDictionary : nil;force = false
            let retained = Unmanaged.passRetained(ticket).toOpaque()
            // Frame identity is not a media clock. Changing 24 -> 60 fps must
            // never reinterpret old frame counts and move timestamps backwards.
            presentationNanoseconds=max(now,presentationNanoseconds+1)
            let status = VTCompressionSessionEncodeFrame(session,imageBuffer:target,presentationTimeStamp:CMTime(value:Int64(presentationNanoseconds),timescale:1_000_000_000),duration:CMTime(value:1,timescale:fps),frameProperties:properties,sourceFrameRefcon:retained,infoFlagsOut:nil)
            if status != noErr { Unmanaged<NativeEncodeTicket>.fromOpaque(retained).release();busy = false;throw Wire.failure("Hardware encode failed (\(status))") }
        } catch { stopped = true;Wire.send(["event":"capture_error","handle":handle,"error":error.localizedDescription]) }
    }
    private func output(_ ticket: NativeEncodeTicket, status: OSStatus, sample: CMSampleBuffer?) {
        // VideoToolbox may explicitly drop a picture under transient hardware
        // pressure. Retire its reference chain and retry the latest surface,
        // with a bounded retry count and pacing instead of a tight reset loop.
        if status == noErr && sample == nil {
            queue.async { [self] in
                busy = false;consecutiveDrops += 1
                let now=DispatchTime.now().uptimeNanoseconds
                if dropStarted == nil {dropStarted=now}
                if now-dropStarted! >= 500_000_000 { stopped = true;Wire.send(["event":"capture_error","handle":handle,"error":"VideoToolbox produced no picture for 500 ms"]) }
                else { force = true;queue.asyncAfter(deadline:.now()+1.0/Double(fps)){[weak self] in self?.drain()} }
            };return
        }
        do {
            guard status == noErr, let sample, CMSampleBufferDataIsReady(sample),let format = CMSampleBufferGetFormatDescription(sample),let block = CMSampleBufferGetDataBuffer(sample) else { throw Wire.failure("Hardware encoder produced no picture") }
            let attachments = CMSampleBufferGetSampleAttachmentsArray(sample,createIfNecessary:false) as? [[CFString:Any]]
            let keyframe = !(attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool ?? false)
            var bytes = Data()
            if keyframe {
                var count = 0, size = 0, length:Int32 = 0;var parameter:UnsafePointer<UInt8>?
                let status = codec == kCMVideoCodecType_HEVC ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,parameterSetIndex:0,parameterSetPointerOut:&parameter,parameterSetSizeOut:&size,parameterSetCountOut:&count,nalUnitHeaderLengthOut:&length) : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,parameterSetIndex:0,parameterSetPointerOut:&parameter,parameterSetSizeOut:&size,parameterSetCountOut:&count,nalUnitHeaderLengthOut:&length)
                guard status == noErr, length == 4 else { throw Wire.failure("Invalid hardware codec parameters") }
                for index in 0..<count {
                    let status = codec == kCMVideoCodecType_HEVC ? CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,parameterSetIndex:index,parameterSetPointerOut:&parameter,parameterSetSizeOut:&size,parameterSetCountOut:nil,nalUnitHeaderLengthOut:nil) : CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,parameterSetIndex:index,parameterSetPointerOut:&parameter,parameterSetSizeOut:&size,parameterSetCountOut:nil,nalUnitHeaderLengthOut:nil)
                    guard status == noErr,let parameter else { throw Wire.failure("Missing codec parameter") }
                    bytes.append(contentsOf:[0,0,0,1]);bytes.append(parameter,count:size)
                }
            }
            let size = CMBlockBufferGetDataLength(block);guard size > 0,size <= 8*1024*1024 else { throw Wire.failure("Encoded frame exceeds limit") }
            var packet = Data(count:size)
            guard packet.withUnsafeMutableBytes({ CMBlockBufferCopyDataBytes(block,atOffset:0,dataLength:size,destination:$0.baseAddress!) }) == noErr else { throw Wire.failure("Cannot copy compressed access unit") }
            var offset = 0
            while offset+4 <= packet.count {
                let length = packet[offset..<offset+4].reduce(0){($0<<8)|Int($1)};offset += 4
                guard length > 0,length <= packet.count-offset else { throw Wire.failure("Invalid encoded NAL length") }
                bytes.append(contentsOf:[0,0,0,1]);bytes.append(packet[offset..<offset+length]);offset += length
            }
            guard offset == packet.count else { throw Wire.failure("Truncated encoded NAL") }
            if deliveryToken != 0 {
                // Publish the acknowledgement state before the packet can be
                // observed on IPC. Keep busy until Rust admits or retires it;
                // raw captures coalesce instead of losing encoded references.
                queue.async { [self] in number &+= 1;pendingDelivery=ticket.number;consecutiveDrops=0;dropStarted=nil }
            }
            Wire.sendEncoded(handle:handle,width:UInt32(ticket.width),height:UInt32(ticket.height),sourceWidth:UInt32(ticket.sourceWidth),sourceHeight:UInt32(ticket.sourceHeight),number:ticket.number,captured:ticket.captured,keyframe:keyframe,bytes:bytes,encodeMicros:UInt32(min(UInt64(UInt32.max),(DispatchTime.now().uptimeNanoseconds-ticket.started)/1000)))
            if deliveryToken == 0 {queue.async { [self] in number &+= 1;busy = false;consecutiveDrops = 0;dropStarted=nil;drain() }}
        } catch { queue.async { [self] in busy = false;stopped = true;Wire.send(["event":"capture_error","handle":handle,"error":error.localizedDescription]) } }
    }
}
private final class NativeEncodeTicket {
    let width:Int,height:Int,sourceWidth:Int,sourceHeight:Int,captured:UInt64,number:UInt64,started:UInt64
    let source:CVPixelBuffer,target:CVPixelBuffer
    let owner:NativeVideoEncoder
    init(owner:NativeVideoEncoder,width:Int,height:Int,sourceWidth:Int,sourceHeight:Int,captured:UInt64,number:UInt64,started:UInt64,source:CVPixelBuffer,target:CVPixelBuffer) { self.owner=owner;self.width=width;self.height=height;self.sourceWidth=sourceWidth;self.sourceHeight=sourceHeight;self.captured=captured;self.number=number;self.started=started;self.source=source;self.target=target }
}

// The socket reader only admits bounded work. Each window initializes and
// decodes on its own serial queue, so one cold/stalled codec cannot block peers.
final class NativeVideoDecoders {
    private var decoders: [String:NativeDecodeWorker] = [:]
    func decode(_ frame: ClientVideoFrame, into inbox: ClientVideoInbox) {
        if let current=decoders[frame.key],frame.streamGeneration<current.generation{return}
        if decoders[frame.key]?.generation != frame.streamGeneration || decoders[frame.key]?.width != frame.width || decoders[frame.key]?.height != frame.height {
            decoders[frame.key]?.stop()
            if decoders.count >= 16 {for worker in decoders.values{worker.stop()};decoders.removeAll()}
            decoders[frame.key]=NativeDecodeWorker(generation:frame.streamGeneration,width:frame.width,height:frame.height)
        }
        decoders[frame.key]?.submit(frame,into:inbox)
    }
}
private final class NativeDecodeWorker:@unchecked Sendable {
    let generation:UInt32,width:Int,height:Int
    private let queue=DispatchQueue(label:"app.frames.hardware-decoder",qos:.userInteractive),lock=NSLock()
    private var pending:[(ClientVideoFrame,ClientVideoInbox)]=[]
    private var busy=false,stopped=false,recovering=false
    private let decoder:NativeVideoDecoder
    init(generation:UInt32,width:Int,height:Int){self.generation=generation;self.width=width;self.height=height;decoder=NativeVideoDecoder(generation:generation,width:width,height:height)}
    func stop(){lock.lock();stopped=true;pending.removeAll();lock.unlock()}
    func submit(_ frame:ClientVideoFrame,into inbox:ClientVideoInbox){
        lock.lock()
        guard !stopped else{lock.unlock();return}
        let keyframe=frame.bytes.first==1
        if recovering && !keyframe{lock.unlock();return}
        if pending.count >= 1 && busy {
            pending.removeAll();let notify = !recovering;recovering=true;lock.unlock()
            if notify{Wire.send(["event":"media_recovery","window":["id":frame.id,"generation":frame.generation],"generation":frame.streamGeneration])};return
        }
        if keyframe{recovering=false}
        pending.append((frame,inbox));let wake = !busy;busy=true;lock.unlock()
        if wake{queue.async{[self] in drain()}}
    }
    private func drain(){
        while true {
            lock.lock();guard !stopped,!pending.isEmpty else{busy=false;lock.unlock();return};let (frame,inbox)=pending.removeFirst();lock.unlock()
            do{
                let decoded=try decoder.decode(frame)
                lock.lock();let deliver = !stopped && !recovering;lock.unlock()
                if deliver,let decoded{inbox.push(decoded)}
            }catch{
                stop();Wire.send(["event":"decoder_failed","window":["id":frame.id,"generation":frame.generation],"generation":frame.streamGeneration,"reason":error.localizedDescription]);return
            }
        }
    }
}
private final class NativeVideoDecoder {
    let generation:UInt32,width:Int,height:Int
    private var session:VTDecompressionSession?
    private var format:CMVideoFormatDescription?
    private var callback:NativeDecodeCallbackState?
    private var callbackPointer:UnsafeMutableRawPointer?
    init(generation:UInt32,width:Int,height:Int) { self.generation=generation;self.width=width;self.height=height }
    deinit {
        let pointer=callbackPointer
        if let session { Thread.detachNewThread {
            VTDecompressionSessionWaitForAsynchronousFrames(session);VTDecompressionSessionInvalidate(session)
            if let pointer { Unmanaged<NativeDecodeCallbackState>.fromOpaque(pointer).release() }
        }} else if let pointer { Unmanaged<NativeDecodeCallbackState>.fromOpaque(pointer).release() }
    }
    func decode(_ frame:ClientVideoFrame) throws -> ClientVideoFrame? {
        guard frame.bytes.count>1,frame.bytes.count<=8*1024*1024,frame.bytes[0]<=1 else { throw Wire.failure("Invalid native access unit") }
        let started = DispatchTime.now().uptimeNanoseconds
        let units = Self.nals(Data(frame.bytes.dropFirst()))
        if session == nil {
            guard frame.bytes[0] == 1 else { return nil }
            let parameters = units.filter { unit in guard let first = unit.first else { return false };let type = frame.kind == 3 ? (first>>1)&63:first&31;return frame.kind == 3 ? [32,33,34].contains(type):[7,8].contains(type) }.map { $0 as NSData }
            let pointers = parameters.map { $0.bytes.assumingMemoryBound(to:UInt8.self) };let lengths = parameters.map(\.length)
            guard parameters.count >= (frame.kind == 3 ? 3:2) else { throw Wire.failure("Missing independent-frame parameters") }
            let status = pointers.withUnsafeBufferPointer { pointers in lengths.withUnsafeBufferPointer { lengths in
                frame.kind == 3 ? CMVideoFormatDescriptionCreateFromHEVCParameterSets(allocator:nil,parameterSetCount:parameters.count,parameterSetPointers:pointers.baseAddress!,parameterSetSizes:lengths.baseAddress!,nalUnitHeaderLength:4,extensions:nil,formatDescriptionOut:&format) : CMVideoFormatDescriptionCreateFromH264ParameterSets(allocator:nil,parameterSetCount:parameters.count,parameterSetPointers:pointers.baseAddress!,parameterSetSizes:lengths.baseAddress!,nalUnitHeaderLength:4,formatDescriptionOut:&format)
            }}
            guard status == noErr,let format else { throw Wire.failure("Cannot configure native decoder (\(status))") }
            let state=NativeDecodeCallbackState();let pointer=Unmanaged.passRetained(state).toOpaque();self.callback=state;self.callbackPointer=pointer
            var callback = VTDecompressionOutputCallbackRecord(decompressionOutputCallback:{ opaque,_,status,_,image,_,_ in
                guard let opaque else { return };let state = Unmanaged<NativeDecodeCallbackState>.fromOpaque(opaque).takeUnretainedValue()
                state.lock.lock();state.output = image;state.status = status;state.lock.unlock();state.complete.signal()
            },decompressionOutputRefCon:pointer)
            let attributes:[CFString:Any] = [kCVPixelBufferPixelFormatTypeKey:kCVPixelFormatType_32BGRA,kCVPixelBufferMetalCompatibilityKey:true,kCVPixelBufferIOSurfacePropertiesKey:[:] as CFDictionary]
            let specification:[CFString:Any] = [kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder:true]
            let opened = VTDecompressionSessionCreate(allocator:nil,formatDescription:format,decoderSpecification:specification as CFDictionary,imageBufferAttributes:attributes as CFDictionary,outputCallback:&callback,decompressionSessionOut:&session)
            guard opened == noErr else { throw Wire.failure("Hardware decoder unavailable (\(opened))") }
            if let session { VTSessionSetProperty(session,key:kVTDecompressionPropertyKey_RealTime,value:kCFBooleanTrue) }
        }
        guard let session,let format else { return nil }
        var avcc = Data()
        for unit in units { var length = UInt32(unit.count).bigEndian;withUnsafeBytes(of:&length){avcc.append(contentsOf:$0)};avcc.append(unit) }
        guard !avcc.isEmpty else { throw Wire.failure("Empty native picture") }
        var block:CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(allocator:nil,memoryBlock:nil,blockLength:avcc.count,blockAllocator:nil,customBlockSource:nil,offsetToData:0,dataLength:avcc.count,flags:0,blockBufferOut:&block) == noErr,let block else { throw Wire.failure("Cannot allocate compressed picture") }
        guard avcc.withUnsafeBytes({CMBlockBufferReplaceDataBytes(with:$0.baseAddress!,blockBuffer:block,offsetIntoDestination:0,dataLength:avcc.count)}) == noErr else { throw Wire.failure("Cannot copy compressed picture") }
        var size=avcc.count;var sample:CMSampleBuffer?
        guard CMSampleBufferCreateReady(allocator:nil,dataBuffer:block,formatDescription:format,sampleCount:1,sampleTimingEntryCount:0,sampleTimingArray:nil,sampleSizeEntryCount:1,sampleSizeArray:&size,sampleBufferOut:&sample) == noErr,let sample else { throw Wire.failure("Cannot create compressed sample") }
        guard let state=callback else{return nil}
        state.lock.lock();state.output=nil;state.status=noErr;state.lock.unlock()
        let status=VTDecompressionSessionDecodeFrame(session,sampleBuffer:sample,flags:[._EnableAsynchronousDecompression],frameRefcon:nil,infoFlagsOut:nil)
        guard status == noErr,state.complete.wait(timeout:.now()+0.05) == .success else { throw Wire.failure("Hardware decoder missed its frame deadline (\(status))") }
        state.lock.lock();let pixel=state.output,statusOut=state.status;state.output=nil;state.lock.unlock()
        guard statusOut == noErr,let pixel else { throw Wire.failure("Hardware decoder returned no surface") }
        Wire.send(["event":"decoded","window":["id":frame.id,"generation":frame.generation],"generation":frame.streamGeneration,"number":frame.number,"micros":(DispatchTime.now().uptimeNanoseconds-started)/1000])
        var decoded=ClientVideoFrame(id:frame.id,generation:frame.generation,width:frame.width,height:frame.height,bytes:Data(),streamGeneration:frame.streamGeneration,number:frame.number,pixelBuffer:pixel)
        decoded.decodedAt=DispatchTime.now().uptimeNanoseconds;return decoded
    }
    private static func nals(_ data:Data)->[Data] {
        let bytes=[UInt8](data);var starts:[(Int,Int)]=[];var i=0
        while i+3<=bytes.count {
            if bytes[i]==0 && bytes[i+1]==0 && bytes[i+2]==1 { starts.append((i,i+3));i+=3 }
            else if i+4<=bytes.count && bytes[i]==0 && bytes[i+1]==0 && bytes[i+2]==0 && bytes[i+3]==1 { starts.append((i,i+4));i+=4 }
            else { i+=1 }
        }
        return starts.enumerated().compactMap { index,start in let end=index+1<starts.count ? starts[index+1].0:bytes.count;return end>start.1 ? Data(bytes[start.1..<end]):nil }
    }
}

private final class NativeDecodeCallbackState:@unchecked Sendable {
    let complete=DispatchSemaphore(value:0),lock=NSLock()
    var output:CVPixelBuffer?,status:OSStatus=noErr
}
