#include "wire.h"
#include <cstdio>
#include <cstring>
#include <thread>
#include <atomic>
#include <chrono>

namespace {
HWND window=nullptr;std::atomic<unsigned> raw_keys{0},raw_mouse{0},legacy_keys{0},legacy_mouse{0},errors{0};
bool buffered=false;unsigned quit_result=99;
bool raw_w=false,legacy_w=false,raw_left=false,legacy_left=false;
unsigned buffer_calls=0;
bool report_requested=false;
LARGE_INTEGER frequency{},last_frame{};uint64_t frames=0;double frame_total_ms=0,frame_max_ms=0;
void validate(const RAWINPUT& input) {
    if(reinterpret_cast<uintptr_t>(input.header.hDevice)!=0xFA200001&&reinterpret_cast<uintptr_t>(input.header.hDevice)!=0xFA200002)return;
    if(input.header.dwSize!=sizeof(RAWINPUTHEADER)+(input.header.dwType==RIM_TYPEKEYBOARD?sizeof(RAWKEYBOARD):sizeof(RAWMOUSE)))++errors;
    if(input.header.dwType==RIM_TYPEKEYBOARD)++raw_keys;else if(input.header.dwType==RIM_TYPEMOUSE)++raw_mouse;
    if(input.header.dwType==RIM_TYPEKEYBOARD&&input.data.keyboard.VKey=='W')raw_w=(input.data.keyboard.Flags&RI_KEY_BREAK)==0;
    if(input.header.dwType==RIM_TYPEKEYBOARD){UINT scan=input.data.keyboard.MakeCode,vk=input.data.keyboard.VKey;
        if((scan==0x2a||scan==0x36)&&vk!=VK_SHIFT)++errors;if(scan==0x1d&&vk!=VK_CONTROL)++errors;if(scan==0x38&&vk!=VK_MENU)++errors;}
    if(input.header.dwType==RIM_TYPEMOUSE){if(input.data.mouse.usButtonFlags&RI_MOUSE_LEFT_BUTTON_DOWN)raw_left=true;if(input.data.mouse.usButtonFlags&RI_MOUSE_LEFT_BUTTON_UP)raw_left=false;}
    RID_DEVICE_INFO info{};info.cbSize=sizeof(info);UINT bytes=sizeof(info);
    if(GetRawInputDeviceInfoW(input.header.hDevice,RIDI_DEVICEINFO,&info,&bytes)==UINT(-1))++errors;
}
void report() {
    auto module=GetModuleHandleW(L"FramesInput.dll");auto pending=module?reinterpret_cast<uint64_t(__cdecl*)()>(GetProcAddress(module,"FramesRemotePending")):nullptr;uint64_t pending_value=pending?pending():0;
    BYTE keyboard[256]{};GetKeyboardState(keyboard);POINT cursor{};GetCursorPos(&cursor);
    UINT count=0;GetRegisteredRawInputDevices(nullptr,&count,sizeof(RAWINPUTDEVICE));
    printf("{\"pending\":%llu,\"buffer_calls\":%u,",pending_value,buffer_calls);
    printf("\"event\":\"report\",\"w_raw\":%d,\"w_legacy\":%d,\"left_raw\":%d,\"left_legacy\":%d,\"w_async\":%d,\"w_queue\":%d,\"w_array\":%d,\"raw_keys\":%u,\"raw_mouse\":%u,\"legacy_keys\":%u,\"legacy_mouse\":%u,\"errors\":%u,\"registrations\":%u,\"x\":%ld,\"y\":%ld,\"frames\":%llu,\"frame_mean_ms\":%.3f,\"frame_max_ms\":%.3f}\n",
        raw_w,legacy_w,raw_left,legacy_left,(GetAsyncKeyState('W')&0x8000)!=0,(GetKeyState('W')&0x8000)!=0,(keyboard['W']&0x80)!=0,
        raw_keys.load(),raw_mouse.load(),legacy_keys.load(),legacy_mouse.load(),errors.load(),count,cursor.x,cursor.y,frames,frames>1?frame_total_ms/(frames-1):0,frame_max_ms);fflush(stdout);
}
LRESULT CALLBACK procedure(HWND hwnd,UINT msg,WPARAM wp,LPARAM lp) {
    if(msg==WM_INPUT) {
        if((static_cast<uint64_t>(lp)&0xFFFF000000000000ULL)!=0xFA10000000000000ULL)return DefWindowProcW(hwnd,msg,wp,lp);
        UINT bytes=0;if(GetRawInputData(reinterpret_cast<HRAWINPUT>(lp),RID_INPUT,nullptr,&bytes,sizeof(RAWINPUTHEADER))!=0||(bytes!=sizeof(RAWINPUTHEADER)+sizeof(RAWKEYBOARD)&&bytes!=sizeof(RAWINPUTHEADER)+sizeof(RAWMOUSE))){++errors;return 0;}
        UINT required=bytes;RAWINPUT input{};UINT small=1;SetLastError(0);
        RAWINPUTHEADER header{};UINT header_size=sizeof(header);
        if(GetRawInputData(reinterpret_cast<HRAWINPUT>(lp),RID_HEADER,&header,&header_size,sizeof(header))!=sizeof(header)||header.dwSize!=required)++errors;
        if(GetRawInputData(reinterpret_cast<HRAWINPUT>(lp),RID_INPUT,&input,&small,sizeof(RAWINPUTHEADER))!=UINT(-1)||small!=required)++errors;
        bytes=sizeof(input);if(GetRawInputData(reinterpret_cast<HRAWINPUT>(lp),RID_INPUT,&input,&bytes,sizeof(RAWINPUTHEADER))!=required)++errors;else validate(input);
        bytes=sizeof(input);if(GetRawInputData(reinterpret_cast<HRAWINPUT>(lp),RID_INPUT,&input,&bytes,sizeof(RAWINPUTHEADER))!=required)++errors;
        return DefWindowProcW(hwnd,msg,wp,lp);
    }
    if(msg==WM_KEYDOWN||msg==WM_KEYUP||msg==WM_SYSKEYDOWN||msg==WM_SYSKEYUP)++legacy_keys;
    if((msg==WM_KEYDOWN||msg==WM_KEYUP||msg==WM_SYSKEYDOWN||msg==WM_SYSKEYUP)&&wp=='W')legacy_w=msg==WM_KEYDOWN||msg==WM_SYSKEYDOWN;
    if(msg==WM_KEYDOWN||msg==WM_KEYUP||msg==WM_SYSKEYDOWN||msg==WM_SYSKEYUP){UINT scan=(lp>>16)&255;if((scan==0x2a||scan==0x36)&&wp!=VK_SHIFT)++errors;if(scan==0x1d&&wp!=VK_CONTROL)++errors;if(scan==0x38&&wp!=VK_MENU)++errors;}
    if(msg==WM_LBUTTONDOWN)legacy_left=true;if(msg==WM_LBUTTONUP)legacy_left=false;
    if(msg>=WM_MOUSEFIRST&&msg<=WM_MOUSELAST)++legacy_mouse;
    if(msg==WM_APP+1){if(buffered)report_requested=true;else report();return 0;}
    if(msg==WM_APP+2){SetCursorPos(1,1);RECT r{0,0,4,4};ClipCursor(&r);ShowCursor(FALSE);SetForegroundWindow(hwnd);SetWindowPos(hwnd,HWND_TOPMOST,0,0,0,0,SWP_NOSIZE|SWP_NOMOVE);ShowWindowAsync(hwnd,SW_SHOWMAXIMIZED);report();return 0;}
    if(msg==WM_APP+3){PostQuitMessage(0);return 0;}
    if(msg==WM_APP+4){printf("{\"event\":\"paused\"}\n");fflush(stdout);Sleep(5000);return 0;}
    if(msg==WM_APP+5){frames=0;frame_total_ms=frame_max_ms=0;last_frame={};report();return 0;}
    if(msg==WM_APP+6){ShowWindow(hwnd,SW_SHOWMINNOACTIVE);report();return 0;}
    if(msg==WM_APP+7){ShowWindow(hwnd,SW_SHOWNOACTIVATE);report();return 0;}
    if(msg==WM_APP+8){DestroyWindow(hwnd);report();return 0;}
    if(msg==WM_TIMER){InvalidateRect(hwnd,nullptr,FALSE);return 0;}
    if(msg==WM_PAINT){LARGE_INTEGER now;QueryPerformanceCounter(&now);if(last_frame.QuadPart){double interval=(now.QuadPart-last_frame.QuadPart)*1000.0/frequency.QuadPart;frame_total_ms+=interval;if(interval>frame_max_ms)frame_max_ms=interval;}last_frame=now;++frames;PAINTSTRUCT ps{};auto dc=BeginPaint(hwnd,&ps);RECT r{};GetClientRect(hwnd,&r);FillRect(dc,&r,reinterpret_cast<HBRUSH>(COLOR_WINDOW+1));DrawTextW(dc,L"Frames shared input fixture",-1,&r,DT_CENTER|DT_VCENTER|DT_SINGLELINE);EndPaint(hwnd,&ps);return 0;}
    return DefWindowProcW(hwnd,msg,wp,lp);
}
}
int main(int argc,char** argv) {
    bool hidden_raw=false,soak=false;for(int i=1;i<argc;++i){if(!strcmp(argv[i],"--buffered"))buffered=true;if(!strcmp(argv[i],"--hidden-raw"))hidden_raw=true;if(!strcmp(argv[i],"--soak"))soak=true;}
    WNDCLASSW cls{};cls.lpfnWndProc=procedure;cls.hInstance=GetModuleHandleW(nullptr);cls.lpszClassName=L"FramesSharedInputFixture";
    RegisterClassW(&cls);window=CreateWindowExW(WS_EX_NOACTIVATE|(soak?WS_EX_TOOLWINDOW:0),cls.lpszClassName,L"Frames shared input fixture",WS_OVERLAPPEDWINDOW,soak?-30000:80,soak?-30000:80,640,480,nullptr,nullptr,cls.hInstance,nullptr);
    ShowWindow(window,SW_SHOWNOACTIVATE);
    QueryPerformanceFrequency(&frequency);SetTimer(window,1,16,nullptr);
    HWND raw_window=hidden_raw?CreateWindowExW(0,cls.lpszClassName,L"Frames hidden raw sink",0,0,0,0,0,HWND_MESSAGE,nullptr,cls.hInstance,nullptr):window;
    RAWINPUTDEVICE devices[2]={{1,6,RIDEV_INPUTSINK,raw_window},{1,2,RIDEV_INPUTSINK,raw_window}};
    if(!RegisterRawInputDevices(devices,2,sizeof(RAWINPUTDEVICE)))return 2;
    printf("{\"event\":\"ready\",\"pid\":%lu,\"hwnd\":%llu}\n",GetCurrentProcessId(),reinterpret_cast<uint64_t>(window));fflush(stdout);
    std::thread([]{char command[64];while(fgets(command,sizeof(command),stdin)){if(!strncmp(command,"diagnostic",10)){auto module=GetModuleHandleW(L"FramesInput.dll");auto reason=module?reinterpret_cast<uint64_t(__cdecl*)()>(GetProcAddress(module,"FramesRemoteStopReason")):nullptr;uint64_t value=reason?reason():0;printf("{\"event\":\"diagnostic\",\"window_alive\":%d,\"stop_reason\":%lu,\"stop_error\":%lu}\n",IsWindow(window)!=FALSE,static_cast<DWORD>(value>>32),static_cast<DWORD>(value));fflush(stdout);}else if(!strncmp(command,"reset",5))PostMessageW(window,WM_APP+5,0,0);else if(!strncmp(command,"pause",5))PostMessageW(window,WM_APP+4,0,0);else if(!strncmp(command,"effects",7))PostMessageW(window,WM_APP+2,0,0);else if(!strncmp(command,"minimize",8))PostMessageW(window,WM_APP+6,0,0);else if(!strncmp(command,"restore",7))PostMessageW(window,WM_APP+7,0,0);else if(!strncmp(command,"destroy",7))PostMessageW(window,WM_APP+8,0,0);else if(!strncmp(command,"quit",4)){PostMessageW(window,WM_APP+3,0,0);return;}else PostMessageW(window,WM_APP+1,0,0);}}).detach();
    MSG message{};
    if(!buffered) {BOOL result;while((result=GetMessageW(&message,nullptr,0,0))>0){TranslateMessage(&message);DispatchMessageW(&message);}quit_result=static_cast<unsigned>(result);}
    else {
        bool running=true;while(running){MsgWaitForMultipleObjectsEx(0,nullptr,16,QS_RAWINPUT,MWMO_INPUTAVAILABLE);GetQueueStatus(QS_RAWINPUT);alignas(8) BYTE bytes[64*sizeof(RAWINPUT)]{};UINT size=sizeof(bytes);
            ++buffer_calls;UINT count=GetRawInputBuffer(reinterpret_cast<RAWINPUT*>(bytes),&size,sizeof(RAWINPUTHEADER));
            if(count!=UINT(-1)){if(count&&size!=sizeof(bytes))++errors;auto input=reinterpret_cast<RAWINPUT*>(bytes);for(UINT n=0;n<count;++n){validate(*input);input=reinterpret_cast<RAWINPUT*>((reinterpret_cast<uintptr_t>(input)+input->header.dwSize+7)&~uintptr_t(7));}}
            if(report_requested){report_requested=false;report();}
            // A buffered-only engine does not fall back to individual reads in WM_INPUT.
            auto pump=[&](UINT low,UINT high){while(PeekMessageW(&message,nullptr,low,high,PM_REMOVE)){if(message.message==WM_QUIT){running=false;quit_result=0;break;}TranslateMessage(&message);DispatchMessageW(&message);}};
            pump(0,WM_INPUT-1);if(running)pump(WM_INPUT+1,0xFFFF);
        }
    }
    DestroyWindow(window);printf("{\"event\":\"quit\",\"result\":%u}\n",quit_result);fflush(stdout);return errors?3:0;
}
