// WASAPI process-tree loopback. No endpoint/system-loopback fallback is possible here.
#include "wire.h"
#include <audioclient.h>
#include <mmdeviceapi.h>
#include <wrl/client.h>
#include <atomic>
#include <thread>
#include <mutex>
#include <condition_variable>
#include <deque>
#include <vector>
#include <cstring>
#include <memory>
using Microsoft::WRL::ComPtr;
namespace {
// Windows SDK 20348 layout, declared locally to also build with the 19041 SDK.
struct ActivationParams {UINT type=1;DWORD pid=0;UINT mode=0;};
static_assert(sizeof(ActivationParams)==12,"process activation ABI");
class Completion final:public IActivateAudioInterfaceCompletionHandler {
    std::atomic<ULONG> refs{1};
public:
    HANDLE event=CreateEventW(nullptr,TRUE,FALSE,nullptr);HRESULT result=E_PENDING;ComPtr<IAudioClient> client;
    ~Completion(){if(event)CloseHandle(event);}
    HRESULT STDMETHODCALLTYPE QueryInterface(REFIID id,void** out) override {if(!out)return E_POINTER;*out=nullptr;
        if(id==__uuidof(IUnknown)||id==__uuidof(IActivateAudioInterfaceCompletionHandler)||id==__uuidof(IAgileObject)){*out=static_cast<IActivateAudioInterfaceCompletionHandler*>(this);AddRef();return S_OK;}return E_NOINTERFACE;}
    ULONG STDMETHODCALLTYPE AddRef() override{return ++refs;}
    ULONG STDMETHODCALLTYPE Release() override{ULONG n=--refs;if(!n)delete this;return n;}
    HRESULT STDMETHODCALLTYPE ActivateCompleted(IActivateAudioInterfaceAsyncOperation* operation) override {
        ComPtr<IUnknown> unknown;HRESULT activated=E_FAIL;result=operation->GetActivateResult(&activated,&unknown);
        if(SUCCEEDED(result))result=activated;if(SUCCEEDED(result))result=unknown.As(&client);SetEvent(event);return S_OK;
    }
};
struct Audio {
    HANDLE ready=CreateEventW(nullptr,TRUE,FALSE,nullptr),stop=CreateEventW(nullptr,TRUE,FALSE,nullptr);
    std::atomic<HRESULT> error{E_PENDING};std::thread thread;std::mutex lock;std::condition_variable available;
    std::deque<std::vector<BYTE>> packets;size_t bytes=0;
    ~Audio(){SetEvent(stop);available.notify_all();if(thread.joinable())thread.join();CloseHandle(stop);CloseHandle(ready);}
};
HRESULT capture(Audio& audio,DWORD pid) {
    auto completion=ComPtr<Completion>();completion.Attach(new Completion());if(!completion->event)return HRESULT_FROM_WIN32(GetLastError());
    ActivationParams parameters;parameters.pid=pid;PROPVARIANT variant{};variant.vt=VT_BLOB;variant.blob.cbSize=sizeof(parameters);variant.blob.pBlobData=reinterpret_cast<BYTE*>(&parameters);
    ComPtr<IActivateAudioInterfaceAsyncOperation> operation;HRESULT hr=ActivateAudioInterfaceAsync(L"VAD\\Process_Loopback",__uuidof(IAudioClient),&variant,completion.Get(),&operation);
    if(FAILED(hr))return hr;HANDLE activation_wait[]={completion->event,audio.stop};
    if(WaitForMultipleObjects(2,activation_wait,FALSE,5000)!=WAIT_OBJECT_0)return HRESULT_FROM_WIN32(ERROR_TIMEOUT);
    if(FAILED(completion->result))return completion->result;
    ComPtr<IAudioClient> client=completion->client;WAVEFORMATEX format{};format.wFormatTag=WAVE_FORMAT_IEEE_FLOAT;format.nChannels=2;format.nSamplesPerSec=48000;format.wBitsPerSample=32;format.nBlockAlign=8;format.nAvgBytesPerSec=384000;
    hr=client->Initialize(AUDCLNT_SHAREMODE_SHARED,AUDCLNT_STREAMFLAGS_LOOPBACK|AUDCLNT_STREAMFLAGS_EVENTCALLBACK|AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,0,0,&format,nullptr);if(FAILED(hr))return hr;
    ComPtr<IAudioCaptureClient> reader;hr=client->GetService(IID_PPV_ARGS(&reader));if(FAILED(hr))return hr;
    HANDLE event=CreateEventW(nullptr,FALSE,FALSE,nullptr);if(!event)return HRESULT_FROM_WIN32(GetLastError());
    hr=client->SetEventHandle(event);if(SUCCEEDED(hr))hr=client->Start();
    if(FAILED(hr)){CloseHandle(event);return hr;}
    audio.error=S_OK;SetEvent(audio.ready);HANDLE waits[]={audio.stop,event};
    while(WaitForMultipleObjects(2,waits,FALSE,100)!=WAIT_OBJECT_0) {
        UINT frames=0;hr=reader->GetNextPacketSize(&frames);if(FAILED(hr))break;
        while(frames){BYTE* data=nullptr;DWORD flags=0;UINT count=0;hr=reader->GetBuffer(&data,&count,&flags,nullptr,nullptr);if(FAILED(hr))break;
            size_t size=static_cast<size_t>(count)*format.nBlockAlign;
            if(size<=38400){std::vector<BYTE> packet(size,0);if(!(flags&AUDCLNT_BUFFERFLAGS_SILENT))memcpy(packet.data(),data,size);
                {std::lock_guard<std::mutex> lock(audio.lock);while(audio.bytes+size>38400&&!audio.packets.empty()){audio.bytes-=audio.packets.front().size();audio.packets.pop_front();}audio.bytes+=size;audio.packets.emplace_back(std::move(packet));}audio.available.notify_one();}
            reader->ReleaseBuffer(count);hr=reader->GetNextPacketSize(&frames);if(FAILED(hr))break;
        }
        if(FAILED(hr))break;
    }
    client->Stop();CloseHandle(event);return hr;
}
}
extern "C" __declspec(dllexport) void* __cdecl FramesAudioStart(DWORD pid,HRESULT* error) {
    auto audio=std::make_unique<Audio>();auto pointer=audio.get();audio->thread=std::thread([pointer,pid]{HRESULT hr=CoInitializeEx(nullptr,COINIT_MULTITHREADED);bool initialized=SUCCEEDED(hr);if(initialized)hr=capture(*pointer,pid);pointer->error=hr;SetEvent(pointer->ready);pointer->available.notify_all();if(initialized)CoUninitialize();});
    if(WaitForSingleObject(audio->ready,6000)!=WAIT_OBJECT_0||FAILED(audio->error)){if(error)*error=audio->error;return nullptr;}if(error)*error=S_OK;return audio.release();
}
extern "C" __declspec(dllexport) UINT __cdecl FramesAudioRead(void* handle,BYTE* output,UINT capacity) {
    if(!handle||!output)return UINT(-1);auto& audio=*static_cast<Audio*>(handle);std::unique_lock<std::mutex> lock(audio.lock);
    audio.available.wait_for(lock,std::chrono::milliseconds(10),[&]{return !audio.packets.empty()||FAILED(audio.error.load());});
    if(FAILED(audio.error))return UINT(-1);if(audio.packets.empty())return 0;
    auto& packet=audio.packets.front();if(packet.size()>capacity)return UINT(-1);UINT size=static_cast<UINT>(packet.size());memcpy(output,packet.data(),size);audio.bytes-=size;audio.packets.pop_front();return size;
}
extern "C" __declspec(dllexport) void __cdecl FramesAudioClose(void* handle){delete static_cast<Audio*>(handle);}
