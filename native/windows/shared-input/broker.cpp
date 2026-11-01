// Local broker and ordinary same-user, same-integrity runtime injection helper.
#include "wire.h"
#include <sddl.h>
#include <tlhelp32.h>
#include <bcrypt.h>
#include <string>
#include <vector>
#include <cstring>
#include <memory>
using namespace FramesInput;
namespace {
struct Handle {HANDLE value=INVALID_HANDLE_VALUE;Handle()=default;explicit Handle(HANDLE h):value(h){}~Handle(){if(value&&value!=INVALID_HANDLE_VALUE)CloseHandle(value);}operator HANDLE()const{return value;}bool valid()const{return value&&value!=INVALID_HANDLE_VALUE;}};
struct Broker {Handle pipe,process;uint64_t generation=0,sequence=0;Status latest;};
bool token_info(HANDLE process,TOKEN_INFORMATION_CLASS cls,std::vector<BYTE>& buffer) {
    Handle token; if(!OpenProcessToken(process,TOKEN_QUERY,&token.value))return false;
    DWORD size=0;GetTokenInformation(token,cls,nullptr,0,&size);if(!size)return false;buffer.resize(size);
    return GetTokenInformation(token,cls,buffer.data(),size,&size)!=FALSE;
}
bool same_identity(HANDLE process) {
    std::vector<BYTE> a,b,ia,ib;
    if(!token_info(GetCurrentProcess(),TokenUser,a)||!token_info(process,TokenUser,b)
        ||!EqualSid(reinterpret_cast<TOKEN_USER*>(a.data())->User.Sid,reinterpret_cast<TOKEN_USER*>(b.data())->User.Sid)
        ||!token_info(GetCurrentProcess(),TokenIntegrityLevel,ia)||!token_info(process,TokenIntegrityLevel,ib))return false;
    auto sa=reinterpret_cast<TOKEN_MANDATORY_LABEL*>(ia.data())->Label.Sid,sb=reinterpret_cast<TOKEN_MANDATORY_LABEL*>(ib.data())->Label.Sid;
    DWORD ra=*GetSidSubAuthority(sa,*GetSidSubAuthorityCount(sa)-1),rb=*GetSidSubAuthority(sb,*GetSidSubAuthorityCount(sb)-1);
    DWORD current_session=0,target_session=1;ProcessIdToSessionId(GetCurrentProcessId(),&current_session);ProcessIdToSessionId(GetProcessId(process),&target_session);
    return ra==rb&&current_session==target_session;
}
uintptr_t remote_module(DWORD pid,const wchar_t* name) {
    Handle snapshot(CreateToolhelp32Snapshot(TH32CS_SNAPMODULE|TH32CS_SNAPMODULE32,pid));if(!snapshot.valid())return 0;
    MODULEENTRY32W entry{};entry.dwSize=sizeof(entry);
    if(Module32FirstW(snapshot,&entry))do{if(!_wcsicmp(entry.szModule,name)||!_wcsicmp(entry.szExePath,name))return reinterpret_cast<uintptr_t>(entry.modBaseAddr);}while(Module32NextW(snapshot,&entry));return 0;
}
bool remote_call(HANDLE process,uintptr_t function,const void* data,size_t size,DWORD* exit_code=nullptr) {
    auto memory=VirtualAllocEx(process,nullptr,size,MEM_RESERVE|MEM_COMMIT,PAGE_READWRITE);if(!memory)return false;
    SIZE_T written=0;bool ok=WriteProcessMemory(process,memory,data,size,&written)&&written==size;
    Handle thread;if(ok)thread.value=CreateRemoteThread(process,nullptr,0,reinterpret_cast<LPTHREAD_START_ROUTINE>(function),memory,0,nullptr);
    if(!thread.valid())ok=false;
    bool finished=false;
    if(ok){finished=WaitForSingleObject(thread,10000)==WAIT_OBJECT_0;ok=finished;if(ok&&exit_code)ok=GetExitCodeThread(thread,exit_code)!=FALSE;}
    // A timed-out thread may still be reading its argument. Never free that memory underneath it.
    if(!thread.valid()||finished)VirtualFreeEx(process,memory,0,MEM_RELEASE);
    return ok;
}
bool inject(Broker& broker,const wchar_t* dll,const Start& start) {
    // Resolve the actual owner of LoadLibraryW rather than assuming identical DLL bases.
    auto load=GetProcAddress(GetModuleHandleW(L"kernel32.dll"),"LoadLibraryW");HMODULE owner=nullptr;
    if(!GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS|GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,reinterpret_cast<LPCWSTR>(load),&owner))return false;
    wchar_t owner_path[MAX_PATH]{};GetModuleFileNameW(owner,owner_path,MAX_PATH);
    const wchar_t* basename=wcsrchr(owner_path,L'\\');basename=basename?basename+1:owner_path;
    auto remote_owner=remote_module(GetProcessId(broker.process),basename);if(!remote_owner)return false;
    if(!remote_call(broker.process,remote_owner+(reinterpret_cast<uintptr_t>(load)-reinterpret_cast<uintptr_t>(owner)),dll,(wcslen(dll)+1)*sizeof(wchar_t)))return false;
    auto remote=remote_module(GetProcessId(broker.process),dll);if(!remote)return false;
    HMODULE local=GetModuleHandleW(dll);auto entry=GetProcAddress(local,"FramesRemoteStart");if(!entry)return false;
    DWORD result=0;return remote_call(broker.process,remote+(reinterpret_cast<uintptr_t>(entry)-reinterpret_cast<uintptr_t>(local)),&start,sizeof(start),&result)&&result==ERROR_SUCCESS;
}
}
extern "C" __declspec(dllexport) void* __cdecl FramesBrokerAttach(const wchar_t* dll,uint64_t hwnd,uint64_t generation,DWORD* error) {
    if(error)*error=ERROR_INVALID_PARAMETER;if(!dll||!hwnd||!generation)return nullptr;
    DWORD pid=0;GetWindowThreadProcessId(reinterpret_cast<HWND>(hwnd),&pid);if(!pid||pid==GetCurrentProcessId())return nullptr;
    auto broker=std::make_unique<Broker>();broker->generation=generation;
    broker->process.value=OpenProcess(PROCESS_CREATE_THREAD|PROCESS_QUERY_INFORMATION|PROCESS_VM_OPERATION|PROCESS_VM_WRITE|PROCESS_VM_READ|SYNCHRONIZE,FALSE,pid);
    if(!broker->process.valid()){if(error)*error=GetLastError();return nullptr;}
    BOOL wow=TRUE;if(!IsWow64Process(broker->process,&wow)||wow||!same_identity(broker->process)){if(error)*error=ERROR_ACCESS_DENIED;return nullptr;}
    Start start;start.hwnd=hwnd;start.generation=generation;start.broker_pid=GetCurrentProcessId();
    if(BCryptGenRandom(nullptr,start.nonce,32,BCRYPT_USE_SYSTEM_PREFERRED_RNG)<0){if(error)*error=ERROR_GEN_FAILURE;return nullptr;}
    std::wstring name=L"\\\\.\\pipe\\FramesShared-";const wchar_t* digits=L"0123456789abcdef";
    for(auto byte:start.nonce){name+=digits[byte>>4];name+=digits[byte&15];}wcscpy_s(start.pipe,name.c_str());
    std::vector<BYTE> user;token_info(GetCurrentProcess(),TokenUser,user);LPWSTR sid=nullptr;
    if(user.empty()||!ConvertSidToStringSidW(reinterpret_cast<TOKEN_USER*>(user.data())->User.Sid,&sid))return nullptr;
    std::wstring sddl=L"D:P(A;;GA;;;"+std::wstring(sid)+L")";LocalFree(sid);PSECURITY_DESCRIPTOR descriptor=nullptr;
    if(!ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.c_str(),SDDL_REVISION_1,&descriptor,nullptr))return nullptr;
    SECURITY_ATTRIBUTES attributes{sizeof(attributes),descriptor,FALSE};
    broker->pipe.value=CreateNamedPipeW(name.c_str(),PIPE_ACCESS_DUPLEX|FILE_FLAG_OVERLAPPED|FILE_FLAG_FIRST_PIPE_INSTANCE,PIPE_TYPE_BYTE|PIPE_READMODE_BYTE|PIPE_WAIT|PIPE_REJECT_REMOTE_CLIENTS,1,256*1024,256*1024,0,&attributes);LocalFree(descriptor);
    if(!broker->pipe.valid()){if(error)*error=GetLastError();return nullptr;}
    Handle event(CreateEventW(nullptr,TRUE,FALSE,nullptr));OVERLAPPED ov{};ov.hEvent=event;
    BOOL connected=ConnectNamedPipe(broker->pipe,&ov);DWORD connect_error=GetLastError();
    if(!connected&&connect_error!=ERROR_IO_PENDING&&connect_error!=ERROR_PIPE_CONNECTED){if(error)*error=connect_error;return nullptr;}
    bool injected=inject(*broker,dll,start);
    if(!injected||(!connected&&connect_error==ERROR_IO_PENDING&&WaitForSingleObject(event,10000)!=WAIT_OBJECT_0)){
        CancelIoEx(broker->pipe,&ov);DWORD bytes=0;GetOverlappedResult(broker->pipe,&ov,&bytes,TRUE);if(error)*error=ERROR_DLL_INIT_FAILED;return nullptr;
    }
    ULONG client=0;Hello hello;
    if(!GetNamedPipeClientProcessId(broker->pipe,&client)||client!=pid||!transfer(broker->pipe,&hello,sizeof(hello),false,2000)
        ||hello.magic!=Magic||hello.abi!=Abi||hello.size!=sizeof(hello)||hello.pid!=pid||memcmp(hello.nonce,start.nonce,32)
        ||!transfer(broker->pipe,&broker->latest,sizeof(Status),false,5000)||broker->latest.magic!=Magic||broker->latest.abi!=Abi||broker->latest.size!=sizeof(Status)||broker->latest.phase==3){if(error)*error=broker->latest.error?broker->latest.error:ERROR_DLL_INIT_FAILED;return nullptr;}
    if(error)*error=0;return broker.release();
}
extern "C" __declspec(dllexport) BOOL __cdecl FramesBrokerSend(void* handle,const Command* input) {
    if(!handle||!input)return FALSE;auto& broker=*static_cast<Broker*>(handle);Command c=*input;c.magic=Magic;c.size=sizeof(c);c.abi=Abi;c.generation=broker.generation;c.sequence=++broker.sequence;
    LARGE_INTEGER sent;QueryPerformanceCounter(&sent);c.reserved[0]=sent.QuadPart;
    return transfer(broker.pipe,&c,sizeof(c),true,100);
}
extern "C" __declspec(dllexport) BOOL __cdecl FramesBrokerPoll(void* handle,Status* output) {
    if(!handle||!output)return FALSE;auto& broker=*static_cast<Broker*>(handle);
    if(WaitForSingleObject(broker.process,0)!=WAIT_TIMEOUT){SetLastError(ERROR_PROCESS_ABORTED);return FALSE;}
    DWORD bytes=0;if(!PeekNamedPipe(broker.pipe,nullptr,0,nullptr,&bytes,nullptr))return FALSE;
    // Drain a bounded number of complete status records; don't block the caller for a future record.
    unsigned count=0;while(bytes>0&&count++<16){if(!transfer(broker.pipe,&broker.latest,sizeof(Status),false,100))return FALSE;
        if(broker.latest.magic!=Magic||broker.latest.size!=sizeof(Status)||broker.latest.abi!=Abi||broker.latest.generation!=broker.generation||broker.latest.width>MaxCursor||broker.latest.height>MaxCursor){SetLastError(ERROR_INVALID_DATA);return FALSE;}
        if(!PeekNamedPipe(broker.pipe,nullptr,0,nullptr,&bytes,nullptr))return FALSE;}
    *output=broker.latest;return TRUE;
}
extern "C" __declspec(dllexport) void __cdecl FramesBrokerClose(void* handle) {
    if(!handle)return;auto broker=std::unique_ptr<Broker>(static_cast<Broker*>(handle));Command c;c.kind=Detach;FramesBrokerSend(handle,&c);
    // Closing the pipe also restores the game if the explicit detach cannot be delivered.
}
