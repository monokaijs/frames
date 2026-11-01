"""Real cross-process input-adapter regression and optional soak test (Windows x64).

Uses generated fixture processes only; never injects system-wide mouse/keyboard events.
"""
import argparse
import ctypes as C
from contextlib import contextmanager
from ctypes import wintypes as W
import json
from pathlib import Path
import queue
import statistics
import subprocess
import sys
import threading
import time

ROOT=Path(__file__).resolve().parent.parent
class Command(C.Structure):
    _fields_=[('magic',W.DWORD),('size',W.DWORD),('abi',W.DWORD),('kind',W.DWORD),('generation',C.c_uint64),('sequence',C.c_uint64),('a',C.c_int32),('b',C.c_int32),('c',C.c_int32),('d',C.c_int32),('reserved',C.c_uint64*2)]
class Status(C.Structure):
    _fields_=[('magic',W.DWORD),('size',W.DWORD),('abi',W.DWORD),('phase',W.DWORD),('error',W.DWORD),('width',W.DWORD),('height',W.DWORD),('hot_x',W.DWORD),('hot_y',W.DWORD),('visible',W.DWORD),('x',C.c_int32),('y',C.c_int32),('generation',C.c_uint64),('image_generation',C.c_uint64),('delivered',C.c_uint64),('latency_max_us',C.c_uint64),('latency_histogram',C.c_uint64*32),('rgba',C.c_ubyte*(128*128*4))]

@contextmanager
def suspend_fixture(child):
    """Suspend only threads whose opened handles still belong to our generated child."""
    kernel=C.WinDLL('kernel32',use_last_error=True)
    class Entry(C.Structure):
        _fields_=[('size',W.DWORD),('usage',W.DWORD),('thread',W.DWORD),('owner',W.DWORD),('priority',W.LONG),('delta',W.LONG),('flags',W.DWORD)]
    kernel.CreateToolhelp32Snapshot.argtypes=[W.DWORD,W.DWORD];kernel.CreateToolhelp32Snapshot.restype=W.HANDLE
    kernel.Thread32First.argtypes=kernel.Thread32Next.argtypes=[W.HANDLE,C.POINTER(Entry)]
    kernel.OpenThread.argtypes=[W.DWORD,W.BOOL,W.DWORD];kernel.OpenThread.restype=W.HANDLE
    kernel.GetProcessIdOfThread.argtypes=[W.HANDLE];kernel.GetProcessIdOfThread.restype=W.DWORD
    kernel.SuspendThread.argtypes=kernel.ResumeThread.argtypes=[W.HANDLE]
    kernel.SuspendThread.restype=kernel.ResumeThread.restype=W.DWORD
    kernel.CloseHandle.argtypes=[W.HANDLE]
    snapshot=kernel.CreateToolhelp32Snapshot(4,0);assert snapshot not in [None,C.c_void_p(-1).value]
    held=[]
    try:
        entry=Entry();entry.size=C.sizeof(entry);available=kernel.Thread32First(snapshot,C.byref(entry))
        while available:
            if entry.owner==child.pid:
                handle=kernel.OpenThread(0x802,False,entry.thread)
                if handle:
                    if kernel.GetProcessIdOfThread(handle)==child.pid and kernel.SuspendThread(handle)!=0xffffffff:held.append(handle)
                    else:kernel.CloseHandle(handle)
            available=kernel.Thread32Next(snapshot,C.byref(entry))
        assert len(held)>=3,'fixture adapter threads were not suspended'
        yield
    finally:
        for handle in held:kernel.ResumeThread(handle);kernel.CloseHandle(handle)
        kernel.CloseHandle(snapshot)

def run(directory:Path,seconds:float,buffered:bool,hidden_raw:bool=False):
    dll=(directory/'FramesInput.dll').resolve(); native=C.CDLL(str(dll),use_last_error=True)
    native.FramesBrokerAttach.argtypes=[W.LPCWSTR,C.c_uint64,C.c_uint64,C.POINTER(W.DWORD)];native.FramesBrokerAttach.restype=C.c_void_p
    native.FramesBrokerSend.argtypes=[C.c_void_p,C.POINTER(Command)];native.FramesBrokerSend.restype=W.BOOL
    native.FramesBrokerPoll.argtypes=[C.c_void_p,C.POINTER(Status)];native.FramesBrokerPoll.restype=W.BOOL
    native.FramesBrokerClose.argtypes=[C.c_void_p]
    user=C.WinDLL('user32',use_last_error=True);user.GetForegroundWindow.restype=W.HWND
    user.PostMessageW.argtypes=[W.HWND,W.UINT,W.WPARAM,W.LPARAM]
    child=subprocess.Popen([str(directory/'shared_input_fixture.exe')]+(['--buffered'] if buffered else [])+(['--hidden-raw'] if hidden_raw else [])+(['--soak'] if seconds>=60 else []),stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,creationflags=0x08000000)
    messages=queue.Queue()
    def reader():
        for line in child.stdout:
            try:messages.put(json.loads(line))
            except ValueError:pass
    threading.Thread(target=reader,daemon=True).start()
    handle=None
    send_lock=threading.Lock()
    def receive(event,timeout=5):
        deadline=time.monotonic()+timeout
        while time.monotonic()<deadline:
            result=messages.get(timeout=max(.01,deadline-time.monotonic()))
            if result['event']==event:return result
        raise AssertionError('fixture did not report '+event)
    def report(command='report'):
        child.stdin.write(command+'\n');child.stdin.flush();deadline=time.monotonic()+8
        while time.monotonic()<deadline:
            try:
                message=messages.get(timeout=.05)
                if message['event']=='report':return message
            except queue.Empty:
                if handle:send(7);poll()
        raise AssertionError('fixture report timed out')
    def send(kind,a=0,b=0,c=0,d=0,flags=0):
        packet=Command();packet.kind=kind;packet.a=a;packet.b=b;packet.c=c;packet.d=d;packet.reserved[1]=flags
        with send_lock:sent=native.FramesBrokerSend(handle,C.byref(packet))
        if not sent:
            error=C.get_last_error();diagnostic=None
            try:
                child.stdin.write('diagnostic\n');child.stdin.flush();diagnostic=messages.get(timeout=1)
            except (OSError,queue.Empty):pass
            raise AssertionError(f'native send failed: Windows {error}, fixture_exit {child.poll()}, elapsed {time.monotonic()-test_started:.3f}s, {diagnostic}')
    def poll():
        value=Status();assert native.FramesBrokerPoll(handle,C.byref(value)),'native status failed';return value
    def physical():
        p=W.POINT();r=W.RECT();user.GetCursorPos(C.byref(p));user.GetClipCursor(C.byref(r))
        return (user.GetForegroundWindow(),p.x,p.y,r.left,r.top,r.right,r.bottom,bool(user.GetAsyncKeyState(ord('W'))&0x8000))
    test_started=time.monotonic()
    try:
        ready=receive('ready');hwnd=ready['hwnd'];before=physical();error=W.DWORD()
        report('reset');time.sleep(.4);baseline=report()
        handle=native.FramesBrokerAttach(str(dll),hwnd,42,C.byref(error));assert handle,f'attach failed: {error.value}'
        assert poll().phase==2
        send(8);send(7);send(1,0x11,1);send(3,7,-3);send(4,1,1,150,100);time.sleep(.08)
        value=report();assert value['w_async']==value['w_queue']==value['w_array']==1,value
        assert value['raw_keys']>=1 and value['raw_mouse']>=2,value
        assert value['errors']==0,value
        user.PostMessageW(hwnd,0x100,ord('A'),1)
        time.sleep(.02);leak=report();assert leak['legacy_keys']==value['legacy_keys'],leak
        effects_before=physical();effects=report('effects');effects_after=physical()
        assert effects['x']==effects['y']==1,effects
        assert effects_after[3:]==effects_before[3:],('host clip/keyboard changed',effects_before,effects_after)
        assert effects_before[1:3]==(1,1) or effects_after[1:3]!=(1,1),('game moved the host cursor',effects_after)
        assert effects_after[0]!=hwnd,('game stole host focus',effects_after)
        send(7);send(1,0x11,0);send(4,1,0,150,100);time.sleep(.05)
        value=report();assert value['w_async']==value['w_queue']==value['w_array']==0,value
        for scan in [0x2a,0x36,0x1d,0xe01d,0x38,0xe038]:send(1,scan,1);send(1,scan,0)
        time.sleep(.05);assert report()['errors']==0
        send(1,0x11,1);time.sleep(.65);value=report();assert value['w_async']==value['w_queue']==value['w_array']==0,('lease did not clear input',value)
        send(8);send(7)
        active_deadline=time.monotonic()+2
        while poll().phase!=1:
            assert time.monotonic()<active_deadline,'resume did not become active'
            time.sleep(.02);send(7)
        # Deliberately fill the status pipe. A slow reader must not block the
        # input worker, invalidate framing, or turn backpressure into detach.
        send(1,0x11,1)
        for _ in range(20):send(7);time.sleep(.1)
        assert report()['w_async']==1,'status backpressure blocked input liveness'
        time.sleep(.65)
        assert report()['w_async']==0,'blocked status writer blocked the lease'
        pause_deadline=time.monotonic()+2
        while poll().phase!=2:
            assert time.monotonic()<pause_deadline,'missing pause after status stall'
            time.sleep(.02)
        send(8);send(7);time.sleep(.04);poll()
        position=report();send(4,5,1,900000,-900000,flags=1);send(4,5,0,900000,-900000,flags=1);time.sleep(.03)
        relative_button=report();assert (relative_button['x'],relative_button['y'])==(position['x'],position['y']),('relative click repositioned the game cursor',position,relative_button)
        send(7);send(1,0x11,1);time.sleep(.05);assert report()['w_async']==1
        # Model a scheduler/process stall: a queued heartbeat must not revive an expired lease.
        with suspend_fixture(child):time.sleep(.65);send(7)
        time.sleep(.1);stalled=report()
        assert stalled['w_async']==stalled['w_raw']==stalled['w_legacy']==0,('late heartbeat revived expired input after process stall',stalled)
        assert poll().phase==2,'process stall did not pause control'
        send(8);send(7);time.sleep(.04)
        kernel=C.WinDLL('kernel32',use_last_error=True)
        kernel.GetProcessTimes.argtypes=[W.HANDLE,C.POINTER(W.FILETIME),C.POINTER(W.FILETIME),C.POINTER(W.FILETIME),C.POINTER(W.FILETIME)]
        class Memory(C.Structure):
            _fields_=[('cb',W.DWORD),('faults',W.DWORD)]+[(name,C.c_size_t) for name in ['peak_working_set','working_set','peak_paged','paged','peak_nonpaged','nonpaged','pagefile','peak_pagefile']]
        psapi=C.WinDLL('psapi');psapi.GetProcessMemoryInfo.argtypes=[W.HANDLE,C.POINTER(Memory),W.DWORD]
        def metrics():
            creation,exit,kernel_time,user_time=(W.FILETIME() for _ in range(4))
            assert kernel.GetProcessTimes(W.HANDLE(int(child._handle)),C.byref(creation),C.byref(exit),C.byref(kernel_time),C.byref(user_time))
            cpu=((kernel_time.dwHighDateTime<<32)|kernel_time.dwLowDateTime)+((user_time.dwHighDateTime<<32)|user_time.dwLowDateTime)
            memory=Memory();memory.cb=C.sizeof(memory);psapi.GetProcessMemoryInfo(W.HANDLE(int(child._handle)),C.byref(memory),memory.cb)
            return cpu,memory.working_set,memory.peak_working_set
        load_baseline=report('reset');start_metrics=metrics();load_started=time.monotonic()
        next_progress=time.monotonic()+60
        durations=[];deadline=time.monotonic()+seconds;next_heartbeat=0;next_input=time.monotonic()
        heartbeat_stop=threading.Event();heartbeat_errors=[];heartbeat_max_gap=[0.0]
        def keep_alive():
            previous=time.monotonic()
            while not heartbeat_stop.is_set():
                now=time.monotonic();heartbeat_max_gap[0]=max(heartbeat_max_gap[0],now-previous);previous=now
                try:send(7)
                except Exception as error:heartbeat_errors.append(str(error));return
                heartbeat_stop.wait(.1)
        heartbeat_thread=threading.Thread(target=keep_alive,daemon=True);heartbeat_thread.start()
        user.IsIconic.argtypes=[W.HWND];user.IsIconic.restype=W.BOOL
        try:
            while time.monotonic()<deadline:
                started=time.perf_counter_ns();send(3,1,-1);durations.append((time.perf_counter_ns()-started)/1000)
                if time.monotonic()>next_heartbeat:
                    assert not heartbeat_errors,heartbeat_errors
                    status=poll();assert status.phase==1,('adapter paused during paced input',{'phase':status.phase,'error':status.error,'minimized':bool(user.IsIconic(hwnd)),'max_broker_heartbeat_gap_ms':heartbeat_max_gap[0]*1000,'elapsed_seconds':time.monotonic()-load_started});next_heartbeat=time.monotonic()+.1
                if time.monotonic()>=next_progress:
                    print(json.dumps({'progress_seconds':round(time.monotonic()-load_started),'events':len(durations),'delivered':status.delivered,'buffered':buffered,'max_broker_heartbeat_gap_ms':round(heartbeat_max_gap[0]*1000,2)}),file=sys.stderr,flush=True)
                    next_progress=time.monotonic()+60
                next_input+=.001
                delay=next_input-time.monotonic()
                if delay>0:time.sleep(delay)
        finally:heartbeat_stop.set();heartbeat_thread.join(timeout=2)
        end_metrics=metrics();load_elapsed=time.monotonic()-load_started;send(7);poll();time.sleep(.05);load_frames=report()
        assert load_frames['raw_mouse']-load_baseline['raw_mouse']==len(durations),('paced input was lost',len(durations),load_baseline,load_frames)
        send(7);time.sleep(.02);delivery=poll();
        # A stopped game message pump exhausts the bounded queue without leaving held keys.
        send(1,0x11,1);send(4,1,1,150,100);time.sleep(.05)
        held=report();assert held['w_raw']==held['w_legacy']==held['left_raw']==held['left_legacy']==1,held
        child.stdin.write('pause\n');child.stdin.flush();receive('paused')
        for i in range(5000):
            send(3,1,0)
            if i%128==0:send(7);poll()
        overflow=poll();overflow_deadline=time.monotonic()+3
        while overflow.phase!=3 and time.monotonic()<overflow_deadline:
            send(7);time.sleep(.05);overflow=poll()
        assert overflow.phase==3 and overflow.error==1816,('overflow did not pause safely',overflow.phase,overflow.error)
        send(6);send(6)
        time.sleep(5.2) # Let a buffered-only loop complete its next read after the deliberate pause.
        value=report();assert value['w_async']==value['w_queue']==value['w_array']==value['w_raw']==value['w_legacy']==value['left_raw']==value['left_legacy']==0,value
        send(8);send(7);time.sleep(.05)
        send(6);native.FramesBrokerClose(handle);handle=None;time.sleep(.2)
        value=report();assert value['registrations']==2,value
        # Physical key state may legitimately change while the host uses the PC.
        assert physical()[3:7]==before[3:7],('host clip changed',before,physical())
        # Resident DLL can be attached again after complete cleanup.
        handle=native.FramesBrokerAttach(str(dll),hwnd,43,C.byref(error));assert handle,('reattach',error.value)
        send(8);send(1,0x11,1);send(4,1,1,150,100);time.sleep(.05);assert report()['w_async']==1
        child.stdin.write('pause\n');child.stdin.flush();receive('paused')
        native.FramesBrokerClose(handle);handle=None;time.sleep(5.3)
        detached=report();assert detached['w_raw']==detached['w_legacy']==detached['left_raw']==detached['left_legacy']==0,('detach lost queued releases while the game was stalled',detached)
        assert detached['registrations']==2,detached
        assert detached['pending']==0,('owned records/messages remained after detach',detached)
        child.stdin.write('quit\n');child.stdin.flush();assert receive('quit')['result']==0
        exit_code=child.wait(timeout=5);assert exit_code==0,{'exit_code':exit_code,'last':value,'stderr':child.stderr.read()}
        durations.sort()
        def delivery_quantile(percent):
            threshold=delivery.delivered*percent/100;total=0
            for index,count in enumerate(delivery.latency_histogram):
                total+=count
                if total>=threshold:return (1<<(index+1))-1
        result={'buffered':buffered,'hidden_raw':hidden_raw,'seconds':seconds,'events':len(durations),'broker_submit_us':{str(p):durations[min(len(durations)-1,int(len(durations)*p/100))] for p in (50,95,99)},'delivery_latency_us_histogram_upper_bound':{str(p):delivery_quantile(p) for p in (50,95,99)},'fixture_initial_working_set_bytes':start_metrics[1],'fixture_working_set_growth_bytes':end_metrics[1]-start_metrics[1],'last_fixture':value,'delivered':delivery.delivered,'delivery_latency_max_us':delivery.latency_max_us,'delivery_histogram_log2_us':list(delivery.latency_histogram),'fixture_cpu_percent_one_core':(end_metrics[0]-start_metrics[0])/1e7/load_elapsed*100,'fixture_working_set_bytes':end_metrics[1],'fixture_peak_working_set_bytes':end_metrics[2],'baseline_frame_mean_ms':baseline.get('frame_mean_ms'),'load_frame_mean_ms':load_frames.get('frame_mean_ms'),'load_frame_max_ms':load_frames.get('frame_max_ms')}
        result['max_broker_heartbeat_gap_ms']=heartbeat_max_gap[0]*1000
        print(json.dumps(result));return result
    finally:
        if handle:native.FramesBrokerClose(handle)
        if child.poll() is None:child.terminate();child.wait(timeout=5)

if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--directory',type=Path,default=ROOT/'target/shared-input/native/Release');parser.add_argument('--seconds',type=float,default=2);parser.add_argument('--buffered',action='store_true');parser.add_argument('--hidden-raw',action='store_true');args=parser.parse_args();run(args.directory,args.seconds,args.buffered,args.hidden_raw)
