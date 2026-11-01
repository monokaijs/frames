"""Crash, minimization, window-destruction, and resident-DLL recovery using owned fixtures."""
import ctypes as C
from ctypes import wintypes as W
import json
from pathlib import Path
import queue
import subprocess
import sys
import threading
import time
from test_shared_input import Command, Status, ROOT

DIRECTORY=ROOT/'target/shared-input/native/Release'

def library():
    native=C.CDLL(str(DIRECTORY/'FramesInput.dll'),use_last_error=True)
    native.FramesBrokerAttach.argtypes=[W.LPCWSTR,C.c_uint64,C.c_uint64,C.POINTER(W.DWORD)];native.FramesBrokerAttach.restype=C.c_void_p
    native.FramesBrokerSend.argtypes=[C.c_void_p,C.POINTER(Command)];native.FramesBrokerSend.restype=W.BOOL
    native.FramesBrokerPoll.argtypes=[C.c_void_p,C.POINTER(Status)];native.FramesBrokerPoll.restype=W.BOOL
    native.FramesBrokerClose.argtypes=[C.c_void_p]
    return native

def broker_child(hwnd):
    native=library();error=W.DWORD()
    handle=native.FramesBrokerAttach(str(DIRECTORY/'FramesInput.dll'),hwnd,52,C.byref(error))
    assert handle, error.value
    def send(kind,a=0,b=0):
        command=Command();command.kind=kind;command.a=a;command.b=b
        assert native.FramesBrokerSend(handle,C.byref(command)),C.get_last_error()
    try:
        send(8);send(1,0x11,1)
        print('attached',flush=True)
        while True:
            send(7);value=Status()
            if not native.FramesBrokerPoll(handle,C.byref(value)):break
            time.sleep(.1)
    finally:native.FramesBrokerClose(handle)

def main():
    fixture=subprocess.Popen([str(DIRECTORY/'shared_input_fixture.exe')],stdin=subprocess.PIPE,stdout=subprocess.PIPE,text=True,creationflags=0x08000000)
    messages=queue.Queue()
    def reader():
        for line in fixture.stdout:messages.put(json.loads(line))
    threading.Thread(target=reader,daemon=True).start()
    broker=None;handle=None;native=library()
    def report(command='report'):
        fixture.stdin.write(command+'\n');fixture.stdin.flush()
        return messages.get(timeout=5)
    def attach(generation):
        error=W.DWORD();value=native.FramesBrokerAttach(str(DIRECTORY/'FramesInput.dll'),hwnd,generation,C.byref(error))
        assert value,error.value
        return value
    def send(kind,a=0,b=0):
        command=Command();command.kind=kind;command.a=a;command.b=b
        assert native.FramesBrokerSend(handle,C.byref(command)),C.get_last_error()
    def state():
        send(7);time.sleep(.04);value=Status()
        assert native.FramesBrokerPoll(handle,C.byref(value))
        return value
    try:
        hwnd=messages.get(timeout=5)['hwnd']
        broker=subprocess.Popen([sys.executable,__file__,'--broker-child',str(hwnd)],stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,creationflags=0x08000000)
        assert broker.stdout.readline().strip()=='attached',broker.stderr.read()
        time.sleep(.1);assert report()['w_async']==1
        broker.kill();broker.wait(timeout=5);time.sleep(.7)
        value=report();assert value['registrations']==2 and value['w_async']==0,value
        # No restart is necessary after broker death: the same resident DLL accepts a new generation.
        handle=attach(53);send(8);send(1,0x11,1);time.sleep(.1)
        assert report()['w_async']==1
        report('minimize');time.sleep(.2)
        value=state();assert value.phase==2,('minimize',value.phase)
        assert report()['w_async']==0
        report('restore');time.sleep(.1)
        assert state().phase==2,'heartbeat resumed input without consent'
        send(8);send(1,0x11,1);time.sleep(.1);assert report()['w_async']==1
        native.FramesBrokerClose(handle);handle=None;time.sleep(.3)
        assert report()['registrations']==2
        handle=attach(54);send(8);send(1,0x11,1);time.sleep(.1)
        report('destroy');time.sleep(.3)
        assert not C.windll.user32.IsWindow(C.c_void_p(hwnd))
        value=Status();assert not native.FramesBrokerPoll(handle,C.byref(value)),'destroyed window remained attached'
        native.FramesBrokerClose(handle);handle=None
        print(json.dumps({'broker_crash':True,'resident_reattach':True,'minimize_pause':True,'explicit_resume':True,'window_destruction':True}))
    finally:
        if handle:native.FramesBrokerClose(handle)
        for child in [broker,fixture]:
            if child and child.poll() is None:child.terminate();child.wait(timeout=5)

if __name__=='__main__':
    if len(sys.argv)>1 and sys.argv[1]=='--broker-child':broker_child(int(sys.argv[2]))
    else:main()
