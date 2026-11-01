"""Exercise the real QUIC shared host/client with isolated identities and a generated window."""
import ctypes as C
from ctypes import wintypes as W
import argparse
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import tempfile
import threading
import time
import uuid

ROOT=Path(__file__).resolve().parent.parent
class LossProxy:
    def __init__(self,server):
        self.server=server;self.peer=None;self.drop_until=0;self.stopped=False
        self.socket=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);self.socket.bind(('127.0.0.1',0));self.socket.settimeout(.05)
        self.port=self.socket.getsockname()[1]
        def relay():
            while not self.stopped:
                try:data,address=self.socket.recvfrom(65536)
                except socket.timeout:continue
                except OSError:break
                if address==self.server:destination=self.peer
                else:self.peer=address;destination=self.server
                if destination and time.monotonic()>=self.drop_until:
                    try:self.socket.sendto(data,destination)
                    except OSError:break
        self.thread=threading.Thread(target=relay,daemon=True);self.thread.start()
    def close(self):
        self.stopped=True;self.socket.close();self.thread.join(timeout=1)

def main(network_loss=False,picker=False):
    root=Path(tempfile.mkdtemp(prefix='frames-shared-session-'));env=dict(os.environ,FRAMES_CONFIG_DIR=str(root/'identity'),FRAMES_NO_DISCOVERY='1')
    directory=Path(os.environ.get('FRAMES_TEST_BIN_DIR',str(ROOT/'target/debug')))
    frames=directory/'frames.exe';probe=directory/'examples/shared_probe.exe'
    identity=subprocess.check_output([str(frames),'identity'],env=env,text=True).splitlines()[0].strip()
    (root/'identity/trusted.json').write_text(json.dumps({'peers':{identity:{'name':'Generated shared fixture','unattended':True,'allow_control':True}}}))
    children=[];fixture=None;host=None;client=None;logs=[];proxy=None
    def read_json(process):
        messages=queue.Queue()
        def reader():
            for line in process.stdout:
                try:messages.put(json.loads(line))
                except ValueError:pass
        threading.Thread(target=reader,daemon=True).start();return messages
    try:
        fixture=subprocess.Popen([str(ROOT/'target/shared-input/native/Release/shared_input_fixture.exe')],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,creationflags=0x08000000);children.append(fixture);reports=read_json(fixture)
        ready=reports.get(timeout=5);hwnd=ready['hwnd']
        if picker:
            marker='FramesSelection-'+str(uuid.uuid4())
            user=C.WinDLL('user32',use_last_error=True);user.SetPropW.argtypes=[W.HWND,W.LPCWSTR,W.HANDLE];user.SetPropW.restype=W.BOOL
            assert user.SetPropW(hwnd,marker,W.HANDLE(1)),'could not stamp owned fixture selection'
            sharing=['--one-window','--share-window',f'{hwnd}:{marker}']
        else:sharing=['--shared-game','--shared-window',str(hwnd)]
        sock=socket.socket(socket.AF_INET,socket.SOCK_DGRAM);sock.bind(('127.0.0.1',0));port=sock.getsockname()[1];sock.close()
        log=(root/'host.log').open('w');logs.append(log)
        host=subprocess.Popen([str(frames),'host','--listen',f'127.0.0.1:{port}','--unattended']+sharing,env=env,stdout=subprocess.DEVNULL,stderr=log,creationflags=0x08000000);children.append(host)
        deadline=time.monotonic()+60
        while 'host listening' not in (root/'host.log').read_text(errors='replace'):
            assert host.poll() is None,(root/'host.log').read_text(errors='replace')
            assert time.monotonic()<deadline,'host did not start';time.sleep(.1)
        policy_tests=[]
        for mode in ['--incompatible','--policy']:
            result=subprocess.run([str(probe),f'127.0.0.1:{port}',mode],env=env,capture_output=True,text=True,timeout=18)
            assert result.returncode==0,{'mode':mode,'stdout':result.stdout,'stderr':result.stderr}
            policy_tests.append(json.loads(result.stdout)['stage'])
            time.sleep(.3);fixture.stdin.write('report\n');fixture.stdin.flush();report=reports.get(timeout=5)
            assert report['registrations']==2 and report['w_async']==0,report
        log=(root/'client.log').open('w');logs.append(log)
        client_port=port
        if network_loss:proxy=LossProxy(('127.0.0.1',port));client_port=proxy.port
        client=subprocess.Popen([str(probe),f'127.0.0.1:{client_port}']+(['--network-loss'] if network_loss else []),env=env,stdout=subprocess.PIPE,stderr=log,text=True,creationflags=0x08000000);children.append(client);stages=read_json(client)
        passed=[];deadline=time.monotonic()+45
        while time.monotonic()<deadline:
            try:value=stages.get(timeout=.2)
            except queue.Empty:
                assert client.poll() is None,(root/'client.log').read_text(errors='replace')
                continue
            stage=value['stage'];passed.append(stage)
            if stage in ['key_down','released']:
                time.sleep(.15);fixture.stdin.write('report\n');fixture.stdin.flush()
                report=reports.get(timeout=5)
                expected=1 if stage=='key_down' else 0
                assert report['w_async']==report['w_queue']==report['w_array']==expected,report
                if stage=='key_down' and proxy:proxy.drop_until=time.monotonic()+.9
            if stage=='passed':break
        assert passed and passed[-1]=='passed',passed
        assert client.wait(timeout=10)==0,(root/'client.log').read_text(errors='replace')
        time.sleep(.2);fixture.stdin.write('report\n');fixture.stdin.flush();report=reports.get(timeout=5)
        assert report['registrations']==2 and report['w_async']==0,report
        assert C.windll.user32.IsWindow(C.c_void_p(hwnd)),'viewer close terminated the fixture'
        print(json.dumps({'policy_tests':policy_tests,'picker_identity':picker,'network_outage_ms':900 if network_loss else 0,'stages':passed,'last_fixture':report,'logs':str(root)}))
    except BaseException:
        for name in ['host.log','client.log']:
            path=root/name
            if path.exists():print(f'--- {name} ---\n{path.read_text(errors="replace")}',flush=True)
        raise
    finally:
        for child in reversed(children):
            if child.poll() is None:child.terminate();child.wait(timeout=5)
        for log in logs:log.close()
        if proxy:proxy.close()
        print('Logs:',root)
if __name__=='__main__':
    parser=argparse.ArgumentParser();parser.add_argument('--network-loss',action='store_true');parser.add_argument('--picker',action='store_true');args=parser.parse_args();main(args.network_loss,args.picker)
