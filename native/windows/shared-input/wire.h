#pragma once
#include <windows.h>
#include <cstdint>

namespace FramesInput {
constexpr uint32_t Abi = 1, Magic = 0x46534931;
constexpr unsigned MaxCursor = 128, CursorBytes = MaxCursor * MaxCursor * 4;
enum Kind : uint32_t { Key=1, Absolute=2, Relative=3, Button=4, Scroll=5, Release=6, Heartbeat=7, Resume=8, Detach=9, Text=10 };
// Local, fixed-width ABI; not a network protocol. All structures include their byte size.
struct Command { uint32_t magic=Magic, size=sizeof(Command), abi=Abi, kind=0;
    uint64_t generation=0, sequence=0; int32_t a=0,b=0,c=0,d=0; uint64_t reserved[2]{}; };
struct Status { uint32_t magic=Magic,size=sizeof(Status),abi=Abi,phase=0,error=0;
    uint32_t width=0,height=0,hot_x=0,hot_y=0,visible=0; int32_t x=0,y=0;
    uint64_t generation=0,image_generation=0;
    uint64_t delivered=0,latency_max_us=0,latency_histogram[32]{};
    uint8_t rgba[CursorBytes]{}; };
struct Start { uint32_t magic=Magic,size=sizeof(Start),abi=Abi,broker_pid=0;
    uint64_t hwnd=0,generation=0; wchar_t pipe[128]{}; uint8_t nonce[32]{}; };
struct Hello { uint32_t magic=Magic,size=sizeof(Hello),abi=Abi,pid=0; uint8_t nonce[32]{}; };
static_assert(sizeof(Command)==64&&sizeof(Status)==65872,"Frames local input ABI");

// Overlapped transfers bound both peers' shutdown latency. Cancellation is always drained.
inline bool transfer(HANDLE pipe, void* bytes, DWORD size, bool write, DWORD timeout=100) {
    OVERLAPPED ov{};ov.hEvent=CreateEventW(nullptr,TRUE,FALSE,nullptr);if(!ov.hEvent)return false;
    const auto deadline=GetTickCount64()+timeout;DWORD offset=0,error=ERROR_SUCCESS;
    while(offset<size){
        auto now=GetTickCount64();if(now>=deadline){error=offset?ERROR_INVALID_DATA:ERROR_TIMEOUT;break;}
        ResetEvent(ov.hEvent);DWORD done=0;
        auto pointer=static_cast<BYTE*>(bytes)+offset;
        BOOL ok=write?WriteFile(pipe,pointer,size-offset,&done,&ov):ReadFile(pipe,pointer,size-offset,&done,&ov);
        if(!ok&&GetLastError()==ERROR_IO_PENDING){
            if(WaitForSingleObject(ov.hEvent,static_cast<DWORD>(deadline-now))==WAIT_OBJECT_0)ok=GetOverlappedResult(pipe,&ov,&done,FALSE);
            else {CancelIoEx(pipe,&ov);ok=GetOverlappedResult(pipe,&ov,&done,TRUE);if(!ok)SetLastError(ERROR_TIMEOUT);}
        }
        if(!ok){error=GetLastError();if(offset&&error==ERROR_TIMEOUT)error=ERROR_INVALID_DATA;break;}
        if(!done){error=ERROR_BROKEN_PIPE;break;}
        offset+=done;
    }
    CloseHandle(ov.hEvent);SetLastError(error);return offset==size;
}
}
