"""Verify process-only audio, including descendants, using quiet generated tones."""
import array
import ctypes as C
import json
import math
from pathlib import Path
import subprocess
import time

ROOT=Path(__file__).resolve().parent.parent
DIRECTORY=ROOT/'target/shared-input/native/Release'

def run(descendant):
    library=C.CDLL(str(DIRECTORY/'FramesInput.dll'))
    library.FramesAudioStart.argtypes=[C.c_uint32,C.POINTER(C.c_int32)];library.FramesAudioStart.restype=C.c_void_p
    library.FramesAudioRead.argtypes=[C.c_void_p,C.c_void_p,C.c_uint32];library.FramesAudioRead.restype=C.c_uint32
    library.FramesAudioClose.argtypes=[C.c_void_p]
    children=[];handle=None
    try:
        for frequency in [880,440]:
            args=[str(DIRECTORY/'shared_audio_fixture.exe'),str(frequency),'4']
            if frequency==880 and descendant:args+=['--spawn-child']
            child=subprocess.Popen(args,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,creationflags=0x08000000);children.append(child)
            ready=json.loads(child.stdout.readline())
            if frequency==880:pid=ready['pid']
        error=C.c_int32();handle=library.FramesAudioStart(pid,C.byref(error))
        assert handle, f'Process loopback unavailable: HRESULT {error.value&0xffffffff:08x}'
        data=bytearray();buffer=C.create_string_buffer(38400);deadline=time.monotonic()+1.2
        while time.monotonic()<deadline:
            count=library.FramesAudioRead(handle,buffer,len(buffer));assert count!=0xffffffff,'capture failed'
            if count:data+=buffer.raw[:count]
        pcm=array.array('f');pcm.frombytes(data);samples=pcm[::2]
        assert len(samples)>12000, len(samples)
        def magnitude(frequency):
            re=sum(value*math.cos(i*frequency*math.tau/48000) for i,value in enumerate(samples))
            im=sum(value*math.sin(i*frequency*math.tau/48000) for i,value in enumerate(samples))
            return math.hypot(re,im)*2/len(samples)
        target,outside=magnitude(880),magnitude(440)
        assert target>.001 and outside<target/20, {'target':target,'outside':outside}
        result={'descendant':descendant,'frames':len(samples),'target_880_amplitude':target,'outside_440_amplitude':outside}
        print(json.dumps(result));return result
    finally:
        if handle:library.FramesAudioClose(handle)
        for child in children:
            try:child.wait(timeout=6)
            except subprocess.TimeoutExpired:child.terminate();child.wait(timeout=5)

if __name__=='__main__':run(False);run(True)
