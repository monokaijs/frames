#include "wire.h"
#include <thread>
#include <vector>
#include <cstdio>
#include <algorithm>
using namespace FramesInput;
int main(){
    wchar_t name[128];swprintf_s(name,L"\\\\.\\pipe\\FramesSharedFraming-%lu-%llu",GetCurrentProcessId(),GetTickCount64());
    HANDLE pipe=CreateNamedPipeW(name,PIPE_ACCESS_DUPLEX|FILE_FLAG_OVERLAPPED,PIPE_TYPE_BYTE|PIPE_WAIT|PIPE_REJECT_REMOTE_CLIENTS,1,4096,4096,0,nullptr);
    if(pipe==INVALID_HANDLE_VALUE)return 1;
    OVERLAPPED connect{};connect.hEvent=CreateEventW(nullptr,TRUE,FALSE,nullptr);ConnectNamedPipe(pipe,&connect);
    HANDLE client=CreateFileW(name,GENERIC_READ|GENERIC_WRITE,0,nullptr,OPEN_EXISTING,0,nullptr);
    if(client==INVALID_HANDLE_VALUE)return 2;DWORD connected=0;GetOverlappedResult(pipe,&connect,&connected,TRUE);CloseHandle(connect.hEvent);
    std::vector<BYTE> source(sizeof(Status)),received(source.size());for(size_t i=0;i<source.size();++i)source[i]=static_cast<BYTE>(i*131);
    std::thread writer([&]{for(size_t offset=0;offset<source.size();){DWORD size=static_cast<DWORD>(std::min<size_t>(997,source.size()-offset)),done=0;if(!WriteFile(client,source.data()+offset,size,&done,nullptr))return;offset+=done;Sleep(1);}});
    bool complete=transfer(pipe,received.data(),static_cast<DWORD>(received.size()),false,2000);writer.join();
    if(!complete||source!=received)return 3;
    BYTE packet[64]{};if(transfer(pipe,packet,sizeof(packet),false,20)||GetLastError()!=ERROR_TIMEOUT)return 4;
    DWORD done=0;WriteFile(client,packet,3,&done,nullptr);
    if(transfer(pipe,packet,sizeof(packet),false,30)||GetLastError()!=ERROR_INVALID_DATA)return 5;
    CloseHandle(client);CloseHandle(pipe);puts("PASS: fragmented byte-pipe records, idle timeout, and partial-record rejection");return 0;
}
