import Foundation
import ScreenCaptureKit
import CoreMedia
import AudioToolbox

final class SystemAudioCapture:NSObject,SCStreamOutput,SCStreamDelegate{
    var stream:SCStream?
    var stopped=false
    private let delivery=NativeAudioDelivery()
    let queue=DispatchQueue(label:"app.frames.system-audio",qos:.userInteractive)
    func start()async throws{
        let content=try await SCShareableContent.excludingDesktopWindows(true,onScreenWindowsOnly:true)
        guard !stopped else { throw Wire.failure("Audio capture was stopped") }
        guard let display=content.displays.first else{throw Wire.failure("No display for system audio capture")}
        let filter=SCContentFilter(display:display,excludingWindows:[])
        let config=SCStreamConfiguration();config.width=2;config.height=2;config.minimumFrameInterval=CMTime(value:1,timescale:1);config.capturesAudio=true;config.excludesCurrentProcessAudio=true;config.sampleRate=48000;config.channelCount=2
        let stream=SCStream(filter:filter,configuration:config,delegate:self);self.stream=stream
        try stream.addStreamOutput(self,type:.audio,sampleHandlerQueue:queue);try await stream.startCapture()
        if stopped { try? await stream.stopCapture();throw Wire.failure("Audio capture was stopped") }
    }
    func stop()async{
        stopped=true;delivery.stop();guard let stream else{return};self.stream=nil
        try? stream.removeStreamOutput(self,type:.audio);try? await stream.stopCapture()
    }
    func stream(_ stream:SCStream,didOutputSampleBuffer sampleBuffer:CMSampleBuffer,of type:SCStreamOutputType){
        if type == .audio && sampleBuffer.isValid { delivery.push(sampleBuffer) }
    }

    func stream(_ stream:SCStream,didStopWithError error:Error){fputs("Frames system audio: \(error)\n",stderr)}
}

// Retaining a sample into this fixed ring is the only capture-callback work.
// Layout conversion, allocation and socket writes run on the delivery thread.
private final class NativeAudioDelivery:@unchecked Sendable {
    private let lock=NSLock(),ready=DispatchSemaphore(value:0)
    private var ring:[CMSampleBuffer?]=Array(repeating:nil,count:4)
    private var read=0,write=0,count=0,stopped=false
    init(){Thread.detachNewThread{[self] in run()}}
    func stop(){lock.lock();stopped=true;lock.unlock();ready.signal()}
    func push(_ sample:CMSampleBuffer){
        guard lock.try() else{return}
        guard !stopped,count<ring.count else{lock.unlock();return}
        ring[write]=sample;write=(write+1)%ring.count;count+=1;lock.unlock();ready.signal()
    }
    private func run(){
        let size=MemoryLayout<AudioBufferList>.size+MemoryLayout<AudioBuffer>.size*2
        let storage=UnsafeMutableRawPointer.allocate(byteCount:size,alignment:16);defer{storage.deallocate()}
        while true{
            ready.wait();lock.lock();if stopped{lock.unlock();return}
            let sample=ring[read];if count>0{ring[read]=nil;read=(read+1)%ring.count;count-=1};lock.unlock()
            guard let sample,let description=CMSampleBufferGetFormatDescription(sample),let format=CMAudioFormatDescriptionGetStreamBasicDescription(description) else{continue}
            let channels=Int(format.pointee.mChannelsPerFrame),frames=CMSampleBufferGetNumSamples(sample)
            guard channels==2,format.pointee.mSampleRate==48000,format.pointee.mBitsPerChannel==32,frames>0 else{continue}
            let list=storage.assumingMemoryBound(to:AudioBufferList.self);var retained:CMBlockBuffer?
            guard CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sample,bufferListSizeNeededOut:nil,bufferListOut:list,bufferListSize:size,blockBufferAllocator:kCFAllocatorDefault,blockBufferMemoryAllocator:kCFAllocatorDefault,flags:0,blockBufferOut:&retained)==noErr else{continue}
            let buffers=UnsafeMutableAudioBufferListPointer(list);var output=Data(capacity:frames*channels*4)
            for frame in 0..<frames{for channel in 0..<channels{let index=buffers.count==1 ? 0:channel,offset=buffers.count==1 ? frame*channels+channel:frame
                guard index<buffers.count,let data=buffers[index].mData,offset*4+4<=Int(buffers[index].mDataByteSize)else{return}
                var sample=data.assumingMemoryBound(to:Float32.self)[offset];withUnsafeBytes(of:&sample){output.append(contentsOf:$0)}
            }}
            Wire.sendAudio(output)
        }
    }
}
