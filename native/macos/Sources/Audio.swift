import Foundation
import ScreenCaptureKit
import CoreMedia
import AudioToolbox

final class SystemAudioCapture:NSObject,SCStreamOutput,SCStreamDelegate{
    var stream:SCStream?
    var stopped=false
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
        stopped=true;guard let stream else{return};self.stream=nil
        try? stream.removeStreamOutput(self,type:.audio);try? await stream.stopCapture()
    }
    func stream(_ stream:SCStream,didOutputSampleBuffer sampleBuffer:CMSampleBuffer,of type:SCStreamOutputType){
        guard type == .audio,sampleBuffer.isValid,let description=CMSampleBufferGetFormatDescription(sampleBuffer),let format=CMAudioFormatDescriptionGetStreamBasicDescription(description) else{return}
        let channels=Int(format.pointee.mChannelsPerFrame),frames=CMSampleBufferGetNumSamples(sampleBuffer)
        guard channels==2,format.pointee.mSampleRate==48000,format.pointee.mBitsPerChannel==32,frames>0 else{return}
        let size=MemoryLayout<AudioBufferList>.size+MemoryLayout<AudioBuffer>.size*channels
        let storage=UnsafeMutableRawPointer.allocate(byteCount:size,alignment:16);defer{storage.deallocate()}
        let list=storage.assumingMemoryBound(to:AudioBufferList.self);var retained:CMBlockBuffer?
        let status=CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(sampleBuffer,bufferListSizeNeededOut:nil,bufferListOut:list,bufferListSize:size,blockBufferAllocator:kCFAllocatorDefault,blockBufferMemoryAllocator:kCFAllocatorDefault,flags:0,blockBufferOut:&retained)
        guard status==noErr else{return};let buffers=UnsafeMutableAudioBufferListPointer(list);var output=Data(capacity:frames*channels*4)
        for frame in 0..<frames{for channel in 0..<channels{let index=buffers.count==1 ? 0:channel;let offset=buffers.count==1 ? frame*channels+channel:frame;guard let data=buffers[index].mData,offset*4+4<=Int(buffers[index].mDataByteSize)else{return};var sample=data.assumingMemoryBound(to:Float32.self)[offset];withUnsafeBytes(of:&sample){output.append(contentsOf:$0)}}}
        Wire.send(["event":"audio","pcm":output.base64EncodedString()])
    }
    func stream(_ stream:SCStream,didStopWithError error:Error){fputs("Frames system audio: \(error)\n",stderr)}
}
