// Short, low-amplitude generated tones for process-loopback isolation tests.
#include <windows.h>
#include <mmsystem.h>
#include <cmath>
#include <cstdio>
#include <cstdlib>
#include <vector>
#include <string>
int main(int argc,char** argv) {
    unsigned frequency=argc>1?std::atoi(argv[1]):880,duration=argc>2?std::atoi(argv[2]):4;
    if(frequency<100||frequency>4000||duration>30)return 1;
    if(argc>3){wchar_t self[32768]{};GetModuleFileNameW(nullptr,self,32768);std::wstring command=L"\""+std::wstring(self)+L"\" "+std::to_wstring(frequency)+L" "+std::to_wstring(duration);
        STARTUPINFOW startup{};startup.cb=sizeof(startup);PROCESS_INFORMATION child{};
        if(!CreateProcessW(nullptr,command.data(),nullptr,nullptr,FALSE,CREATE_NO_WINDOW,nullptr,nullptr,&startup,&child))return 2;
        printf("{\"pid\":%lu,\"child\":%lu}\n",GetCurrentProcessId(),child.dwProcessId);fflush(stdout);
        WaitForSingleObject(child.hProcess,duration*1000+3000);CloseHandle(child.hThread);CloseHandle(child.hProcess);return 0;
    }
    WAVEFORMATEX format{};format.wFormatTag=WAVE_FORMAT_PCM;format.nChannels=2;format.nSamplesPerSec=48000;format.wBitsPerSample=16;format.nBlockAlign=4;format.nAvgBytesPerSec=192000;
    HWAVEOUT device=nullptr;if(waveOutOpen(&device,WAVE_MAPPER,&format,0,0,CALLBACK_NULL)!=MMSYSERR_NOERROR)return 3;
    std::vector<short> samples(4800*2);for(unsigned i=0;i<4800;++i){short value=static_cast<short>(std::sin(i*frequency*6.283185307179586/48000)*655);samples[i*2]=samples[i*2+1]=value;}
    WAVEHDR header{};header.lpData=reinterpret_cast<char*>(samples.data());header.dwBufferLength=static_cast<DWORD>(samples.size()*sizeof(short));header.dwFlags=WHDR_BEGINLOOP|WHDR_ENDLOOP;header.dwLoops=0xFFFFFFFF;
    waveOutPrepareHeader(device,&header,sizeof(header));waveOutWrite(device,&header,sizeof(header));printf("{\"pid\":%lu}\n",GetCurrentProcessId());fflush(stdout);Sleep(duration*1000);
    waveOutReset(device);waveOutUnprepareHeader(device,&header,sizeof(header));waveOutClose(device);return 0;
}
