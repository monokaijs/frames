// Frames adaptation of SplitScreen-Me ProtoInput (MIT), pinned in vendor/frames-protoinput.
// Replaces physical input selection and the GUI/focus loops with a leased remote source.
#include "wire.h"
#include <easyhook.h>
#include <algorithm>
#include <array>
#include <atomic>
#include <cstring>
#include <vector>

using namespace FramesInput;
namespace {
constexpr UINT LegacyMessage=WM_APP+0x3B1;
constexpr size_t Capacity=8192;
constexpr uint64_t TokenPrefix=0xFA10000000000000ULL,TokenMask=0xFFFF000000000000ULL;
constexpr uintptr_t KeyboardDevice=0xFA200001,MouseDevice=0xFA200002;
struct Record { uint64_t token=0,generation=0;DWORD thread=0;bool raw=false,read=false,dequeued=false;
    RAWINPUT input{};MSG message{};UINT vk=0;bool down=false;uint64_t sent=0; };
struct Registration { bool present=false; RAWINPUTDEVICE device{}; };
struct State {
    SRWLOCK lock=SRWLOCK_INIT;std::atomic<bool> attached{false},worker{false};
    std::atomic<DWORD> stop_reason{0},stop_error{0};std::atomic<unsigned> markers{0};
    HWND target=nullptr,focus=nullptr,capture=nullptr;uint64_t generation=0,sequence=0,token=0,lease=0;
    bool playing=false,clipped=false,needs_neutralize=false;std::atomic<bool> destroyed{false};RECT clip{};POINT cursor{};int display_count=0;HCURSOR image=nullptr;
    BYTE keys[256]{};bool edges[256]{};Registration keyboard,mouse;
    std::array<Record,Capacity> records{};size_t next=0;DWORD error=0;bool installed=false;
    uint64_t sent=0,delivered=0,latency_max=0,histogram[32]{};LARGE_INTEGER frequency{};
} state;
struct Lock { Lock(){AcquireSRWLockExclusive(&state.lock);}~Lock(){ReleaseSRWLockExclusive(&state.lock);} };
struct ThreadState { uint64_t generation=0,last_raw=0,seen[4]{};BYTE keys[256]{};MSG last{};bool synthetic=false; };
thread_local ThreadState queue_state;
std::array<HOOK_TRACE_INFO,64> hooks{};size_t hook_count=0;
std::array<void*,64> original_locations{},target_locations{};
HMODULE module_handle=nullptr;

#define ORIGINAL(name) decltype(&name) real_##name=&name
ORIGINAL(GetAsyncKeyState);ORIGINAL(GetKeyState);ORIGINAL(GetKeyboardState);ORIGINAL(SetKeyboardState);
ORIGINAL(GetCursorPos);ORIGINAL(SetCursorPos);ORIGINAL(GetPhysicalCursorPos);ORIGINAL(SetPhysicalCursorPos);
ORIGINAL(ClipCursor);ORIGINAL(GetClipCursor);ORIGINAL(ShowCursor);ORIGINAL(SetCursor);ORIGINAL(GetCursorInfo);ORIGINAL(SetSystemCursor);
ORIGINAL(GetForegroundWindow);ORIGINAL(GetActiveWindow);ORIGINAL(GetFocus);ORIGINAL(SetForegroundWindow);ORIGINAL(SetActiveWindow);ORIGINAL(SetFocus);
ORIGINAL(SetCapture);ORIGINAL(ReleaseCapture);ORIGINAL(GetCapture);ORIGINAL(WindowFromPoint);ORIGINAL(ShowWindow);ORIGINAL(SetWindowPos);
ORIGINAL(DestroyWindow);
ORIGINAL(ShowWindowAsync);
ORIGINAL(RegisterRawInputDevices);ORIGINAL(GetRegisteredRawInputDevices);ORIGINAL(GetRawInputData);ORIGINAL(GetRawInputBuffer);
ORIGINAL(GetRawInputDeviceList);ORIGINAL(GetRawInputDeviceInfoW);ORIGINAL(GetRawInputDeviceInfoA);
ORIGINAL(GetMessageW);ORIGINAL(GetMessageA);ORIGINAL(PeekMessageW);ORIGINAL(PeekMessageA);ORIGINAL(TranslateMessage);
ORIGINAL(GetQueueStatus);ORIGINAL(GetInputState);ORIGINAL(GetMessagePos);ORIGINAL(GetMessageTime);
ORIGINAL(MsgWaitForMultipleObjects);ORIGINAL(MsgWaitForMultipleObjectsEx);
ORIGINAL(DefWindowProcW);ORIGINAL(DefWindowProcA);ORIGINAL(DefRawInputProc);
#undef ORIGINAL

bool owned(HWND window) { return window && (window==state.target || IsChild(state.target,window)); }
bool process_window(HWND window) {DWORD pid=0;return window&&GetWindowThreadProcessId(window,&pid)&&pid==GetCurrentProcessId();}
bool token_handle(HRAWINPUT h) {return (reinterpret_cast<uint64_t>(h)&TokenMask)==TokenPrefix;}
bool virtual_device(HANDLE h){return reinterpret_cast<uintptr_t>(h)==KeyboardDevice||reinterpret_cast<uintptr_t>(h)==MouseDevice;}
void consume_marker(){auto value=state.markers.load();while(value&&!state.markers.compare_exchange_weak(value,value-1)){};}
void finish_marker_drain(){
    if(state.attached)return;
    for(const auto& r:state.records)if(r.token&&(!r.raw||!r.dequeued))return;
    // Notifications are hints; the lifetime of owned live records is authoritative.
    state.markers=0;
}
Record* record(uint64_t token) {
    auto& r=state.records[token&(Capacity-1)];
    return r.token==token && r.generation==state.generation ? &r:nullptr;
}
Record* allocate() {
    for(size_t n=0;n<Capacity;++n) {
        size_t index=state.next++%Capacity;auto& r=state.records[index];
        if(!r.token) {r=Record{};r.token=TokenPrefix|((++state.token&0x7FFFFFFFFULL)<<13)|index;r.generation=state.generation;r.sent=state.sent;return &r;}
    }
    state.error=ERROR_NOT_ENOUGH_QUOTA;state.playing=false;state.needs_neutralize=true;return nullptr;
}
void modifiers(BYTE* keys) {
    keys[VK_SHIFT]=(keys[VK_LSHIFT]|keys[VK_RSHIFT])&0x80;
    keys[VK_CONTROL]=(keys[VK_LCONTROL]|keys[VK_RCONTROL])&0x80;
    keys[VK_MENU]=(keys[VK_LMENU]|keys[VK_RMENU])&0x80;
}
void set_key(BYTE* keys,UINT vk,bool down) {
    if(vk>255)return;
    BYTE toggle=keys[vk]&1;
    if(down && !(keys[vk]&0x80) && (vk==VK_CAPITAL||vk==VK_NUMLOCK||vk==VK_SCROLL))toggle^=1;
    keys[vk]=toggle|(down?0x80:0);modifiers(keys);
}
void sync_queue() {
    if(queue_state.generation!=state.generation) {queue_state=ThreadState{};queue_state.generation=state.generation;}
}
void delivered(Record& r) {
    if(!r.sent||!state.frequency.QuadPart)return;LARGE_INTEGER now;QueryPerformanceCounter(&now);
    uint64_t us=(now.QuadPart-r.sent)*1000000ULL/state.frequency.QuadPart;unsigned bin=0;uint64_t value=us;
    while(value>1&&bin<31){value>>=1;++bin;}++state.histogram[bin];++state.delivered;state.latency_max=std::max(state.latency_max,us);r.sent=0;
}
UINT mouse_flags() {UINT f=0;for(auto p:{std::pair<UINT,UINT>{VK_LBUTTON,MK_LBUTTON},{VK_RBUTTON,MK_RBUTTON},{VK_MBUTTON,MK_MBUTTON},{VK_XBUTTON1,MK_XBUTTON1},{VK_XBUTTON2,MK_XBUTTON2},{VK_CONTROL,MK_CONTROL},{VK_SHIFT,MK_SHIFT}})if(state.keys[p.first]&0x80)f|=p.second;return f;}
HWND receiver(const Registration& registration) {return registration.present && registration.device.hwndTarget?registration.device.hwndTarget:(state.focus?state.focus:state.target);}
void legacy(HWND window,UINT message,WPARAM wparam,LPARAM lparam,UINT vk=0,bool down=false) {
    if(!owned(window))window=state.target;
    if(window!=state.target&&message>=WM_MOUSEFIRST&&message<=WM_MOUSELAST&&message!=WM_MOUSEWHEEL&&message!=WM_MOUSEHWHEEL){POINT point{static_cast<SHORT>(LOWORD(lparam)),static_cast<SHORT>(HIWORD(lparam))};MapWindowPoints(state.target,window,&point,1);lparam=MAKELPARAM(point.x,point.y);}
    auto r=allocate();if(!r)return;r->thread=GetWindowThreadProcessId(window,nullptr);r->vk=vk;r->down=down;
    r->message.hwnd=window;r->message.message=message;r->message.wParam=wparam;r->message.lParam=lparam;
    r->message.time=GetTickCount();r->message.pt=state.cursor;ClientToScreen(state.target,&r->message.pt);
    // Preserve the real message ID so games using filtered Get/PeekMessage ranges see it.
    // The process-local hook restores lParam before the game receives the message.
    ++state.markers;if(!PostMessageW(window,message,wparam,static_cast<LPARAM>(r->token))){consume_marker();r->token=0;}
}
void raw(RAWINPUT input,Registration& registration,UINT vk=0,bool down=false) {
    if(!registration.present)return;
    // Engines often register a separate hidden/message-only raw-input sink.
    // Preserve that registration without changing the selected capture window.
    HWND window=receiver(registration);if(!process_window(window))return;
    auto r=allocate();if(!r)return;r->raw=true;r->thread=GetWindowThreadProcessId(window,nullptr);r->vk=vk;r->down=down;
    r->message.pt=state.cursor;ClientToScreen(state.target,&r->message.pt);r->message.time=GetTickCount();
    input.header.dwSize=sizeof(RAWINPUTHEADER)+(input.header.dwType==RIM_TYPEKEYBOARD?sizeof(RAWKEYBOARD):sizeof(RAWMOUSE));input.header.wParam=RIM_INPUT;r->input=input;
    ++state.markers;if(!PostMessageW(window,WM_INPUT,RIM_INPUT,static_cast<LPARAM>(r->token))){consume_marker();r->token=0;}
}
void activation(bool active) {
    legacy(state.target,WM_ACTIVATE,active?WA_ACTIVE:WA_INACTIVE,0);
    legacy(state.target,WM_ACTIVATEAPP,active,0);
    legacy(state.target,active?WM_SETFOCUS:WM_KILLFOCUS,0,0);
}
bool no_legacy(const Registration& r){return r.present&&(r.device.dwFlags&RIDEV_NOLEGACY)==RIDEV_NOLEGACY;}
HWND mouse_target(){if(state.capture)return state.capture;HWND window=state.target;POINT point=state.cursor;for(unsigned depth=0;depth<32;++depth){HWND child=ChildWindowFromPointEx(window,point,CWP_SKIPINVISIBLE|CWP_SKIPDISABLED|CWP_SKIPTRANSPARENT);if(!child||child==window)break;MapWindowPoints(window,child,&point,1);window=child;}return window;}
void key_event(UINT scan,bool down) {
    bool extended=(scan&0xFF00)==0xE000;UINT code=scan&0xFF;
    UINT vk=MapVirtualKeyW((extended?0xE000:0)|code,MAPVK_VSC_TO_VK_EX);if(vk>255||!vk)return;
    bool previous=(state.keys[vk]&0x80)!=0,alt=(state.keys[VK_MENU]&0x80)!=0;
    set_key(state.keys,vk,down);if(down&&!previous)state.edges[vk]=true;
    UINT message_vk=vk==VK_LSHIFT||vk==VK_RSHIFT?VK_SHIFT:vk==VK_LCONTROL||vk==VK_RCONTROL?VK_CONTROL:vk==VK_LMENU||vk==VK_RMENU?VK_MENU:vk;
    RAWINPUT ri{};ri.header.dwType=RIM_TYPEKEYBOARD;ri.header.hDevice=reinterpret_cast<HANDLE>(KeyboardDevice);
    ri.data.keyboard.MakeCode=static_cast<USHORT>(code);ri.data.keyboard.VKey=static_cast<USHORT>(message_vk);
    ri.data.keyboard.Flags=(down?RI_KEY_MAKE:RI_KEY_BREAK)|(extended?RI_KEY_E0:0);
    UINT message=alt||vk==VK_LMENU||vk==VK_RMENU?(down?WM_SYSKEYDOWN:WM_SYSKEYUP):(down?WM_KEYDOWN:WM_KEYUP);
    ri.data.keyboard.Message=message;raw(ri,state.keyboard,vk,down);
    LPARAM lp=1|(code<<16)|(extended?1ULL<<24:0)|(alt?1ULL<<29:0)|(previous?1ULL<<30:0)|(!down?1ULL<<31:0);
    if(!no_legacy(state.keyboard))legacy(state.focus?state.focus:state.target,message,message_vk,lp,vk,down);
}
void button_event(UINT button,bool down) {
    static const UINT vks[]={0,VK_LBUTTON,VK_RBUTTON,VK_MBUTTON,VK_XBUTTON1,VK_XBUTTON2};
    static const UINT downs[]={0,WM_LBUTTONDOWN,WM_RBUTTONDOWN,WM_MBUTTONDOWN,WM_XBUTTONDOWN,WM_XBUTTONDOWN};
    static const UINT ups[]={0,WM_LBUTTONUP,WM_RBUTTONUP,WM_MBUTTONUP,WM_XBUTTONUP,WM_XBUTTONUP};
    static const USHORT flags[]={0,RI_MOUSE_LEFT_BUTTON_DOWN,RI_MOUSE_RIGHT_BUTTON_DOWN,RI_MOUSE_MIDDLE_BUTTON_DOWN,RI_MOUSE_BUTTON_4_DOWN,RI_MOUSE_BUTTON_5_DOWN};
    if(button<1||button>5)return;UINT vk=vks[button];set_key(state.keys,vk,down);if(down)state.edges[vk]=true;
    RAWINPUT ri{};ri.header.dwType=RIM_TYPEMOUSE;ri.header.hDevice=reinterpret_cast<HANDLE>(MouseDevice);
    ri.data.mouse.usButtonFlags=down?flags[button]:static_cast<USHORT>(flags[button]<<1);raw(ri,state.mouse,vk,down);
    UINT f=mouse_flags();if(button>=4)f|=(button==4?XBUTTON1:XBUTTON2)<<16;
    if(!no_legacy(state.mouse))legacy(mouse_target(),down?downs[button]:ups[button],f,MAKELPARAM(state.cursor.x,state.cursor.y),vk,down);
}
void neutralize(bool notify) {
    state.needs_neutralize=false;
    state.sent=0;
    BYTE held[256];memcpy(held,state.keys,256);
    // Invalidate queued presses before adding releases; old tokens cannot revive keys.
    for(auto& r:state.records) {
        if(r.token&&r.raw&&r.dequeued) {
            // A handle already handed to the game remains readable until dispatch completes.
            // An unread old event becomes neutral, so a delayed read cannot revive a press.
            if(!r.read){r.down=false;if(r.input.header.dwType==RIM_TYPEKEYBOARD){r.input.data.keyboard.Flags|=RI_KEY_BREAK;r.input.data.keyboard.Message=WM_KEYUP;}
                else{r.input.data.mouse.lLastX=r.input.data.mouse.lLastY=0;r.input.data.mouse.usButtonFlags=RI_MOUSE_LEFT_BUTTON_UP|RI_MOUSE_RIGHT_BUTTON_UP|RI_MOUSE_MIDDLE_BUTTON_UP|RI_MOUSE_BUTTON_4_UP|RI_MOUSE_BUTTON_5_UP;}}
        }else if(!(r.token&&r.vk&&!r.down))r.token=0;
    }
    for(UINT vk=0;vk<256;++vk)if(held[vk]&0x80) {
        if(vk==VK_SHIFT||vk==VK_CONTROL||vk==VK_MENU)continue;
        UINT scan=MapVirtualKeyW(vk,MAPVK_VK_TO_VSC_EX);
        if(vk>VK_XBUTTON2 && scan && notify)key_event(scan,false);
    }
    for(UINT b=1;b<=5;++b) {UINT vk=b==1?VK_LBUTTON:b==2?VK_RBUTTON:b==3?VK_MBUTTON:b==4?VK_XBUTTON1:VK_XBUTTON2;if((held[vk]&0x80)&&notify)button_event(b,false);}
    memset(state.keys,0,256);memset(state.edges,0,sizeof(state.edges));
    state.capture=nullptr;state.clipped=false;
    // Reset all per-thread queued views lazily, including queues no longer pumping.
    ++state.generation;
    // Retag the newly posted releases, which belong to the new private-state generation.
    for(auto& r:state.records)if(r.token)r.generation=state.generation;
}

SHORT WINAPI hook_GetAsyncKeyState(int key) {if(!state.attached)return real_GetAsyncKeyState(key);if(key<0||key>255)return 0;Lock l;if(!state.playing)return 0;SHORT v=(state.keys[key]&0x80?static_cast<SHORT>(0x8000):0)|(state.edges[key]?1:0);state.edges[key]=false;return v;}
SHORT WINAPI hook_GetKeyState(int key) {if(!state.attached)return real_GetKeyState(key);if(key<0||key>255)return 0;Lock l;sync_queue();if(!state.playing)return 0;return (queue_state.keys[key]&0x80?static_cast<SHORT>(0x8000):0)|(queue_state.keys[key]&1);}
BOOL WINAPI hook_GetKeyboardState(PBYTE keys) {if(!state.attached)return real_GetKeyboardState(keys);if(!keys){SetLastError(ERROR_INVALID_PARAMETER);return FALSE;}Lock l;sync_queue();if(state.playing)memcpy(keys,queue_state.keys,256);else memset(keys,0,256);return TRUE;}
BOOL WINAPI hook_SetKeyboardState(LPBYTE keys) {if(!state.attached)return real_SetKeyboardState(keys);if(!keys){SetLastError(ERROR_INVALID_PARAMETER);return FALSE;}Lock l;sync_queue();memcpy(queue_state.keys,keys,256);return TRUE;}
BOOL WINAPI hook_GetCursorPos(LPPOINT p) {if(!state.attached)return real_GetCursorPos(p);if(!p){SetLastError(ERROR_INVALID_PARAMETER);return FALSE;}Lock l;*p=state.cursor;ClientToScreen(state.target,p);return TRUE;}
BOOL WINAPI hook_GetPhysicalCursorPos(LPPOINT p) {return state.attached?hook_GetCursorPos(p):real_GetPhysicalCursorPos(p);}
BOOL WINAPI hook_SetCursorPos(int x,int y) {if(!state.attached)return real_SetCursorPos(x,y);Lock l;POINT p{x,y};ScreenToClient(state.target,&p);state.cursor=p;return TRUE;}
BOOL WINAPI hook_SetPhysicalCursorPos(int x,int y) {return state.attached?hook_SetCursorPos(x,y):real_SetPhysicalCursorPos(x,y);}
BOOL WINAPI hook_ClipCursor(const RECT* rect) {if(!state.attached)return real_ClipCursor(rect);Lock l;state.clipped=rect!=nullptr;if(rect){state.clip=*rect;MapWindowPoints(nullptr,state.target,reinterpret_cast<POINT*>(&state.clip),2);}return TRUE;}
BOOL WINAPI hook_GetClipCursor(LPRECT rect) {if(!state.attached)return real_GetClipCursor(rect);if(!rect){SetLastError(ERROR_INVALID_PARAMETER);return FALSE;}Lock l;if(state.clipped){*rect=state.clip;MapWindowPoints(state.target,nullptr,reinterpret_cast<POINT*>(rect),2);}else{GetClientRect(state.target,rect);MapWindowPoints(state.target,nullptr,reinterpret_cast<POINT*>(rect),2);}return TRUE;}
int WINAPI hook_ShowCursor(BOOL show) {if(!state.attached)return real_ShowCursor(show);Lock l;state.display_count+=show?1:-1;return state.display_count;}
HCURSOR WINAPI hook_SetCursor(HCURSOR cursor) {if(!state.attached)return real_SetCursor(cursor);Lock l;auto old=state.image;state.image=cursor;return old;}
BOOL WINAPI hook_SetSystemCursor(HCURSOR cursor,DWORD id) {if(!state.attached)return real_SetSystemCursor(cursor,id);Lock l;state.image=cursor;return TRUE;}
BOOL WINAPI hook_GetCursorInfo(PCURSORINFO info) {if(!state.attached)return real_GetCursorInfo(info);if(!info||info->cbSize!=sizeof(CURSORINFO)){SetLastError(ERROR_INVALID_PARAMETER);return FALSE;}Lock l;info->flags=state.display_count>=0&&state.image?CURSOR_SHOWING:0;info->hCursor=state.image;info->ptScreenPos=state.cursor;ClientToScreen(state.target,&info->ptScreenPos);return TRUE;}
HWND WINAPI hook_GetForegroundWindow(){if(!state.attached)return real_GetForegroundWindow();Lock l;return state.playing?state.target:nullptr;}
HWND WINAPI hook_GetActiveWindow(){if(!state.attached)return real_GetActiveWindow();return hook_GetForegroundWindow();}
HWND WINAPI hook_GetFocus(){if(!state.attached)return real_GetFocus();Lock l;return state.playing?state.focus:nullptr;}
BOOL WINAPI hook_SetForegroundWindow(HWND h){if(!state.attached)return real_SetForegroundWindow(h);Lock l;return owned(h);}
HWND WINAPI hook_SetActiveWindow(HWND h){if(!state.attached)return real_SetActiveWindow(h);Lock l;return owned(h)?state.target:nullptr;}
HWND WINAPI hook_SetFocus(HWND h){if(!state.attached)return real_SetFocus(h);Lock l;auto old=state.focus;if(!h||owned(h))state.focus=h;return old;}
HWND WINAPI hook_SetCapture(HWND h){if(!state.attached)return real_SetCapture(h);Lock l;auto old=state.capture;if(owned(h))state.capture=h;return old;}
BOOL WINAPI hook_ReleaseCapture(){if(!state.attached)return real_ReleaseCapture();Lock l;state.capture=nullptr;return TRUE;}
HWND WINAPI hook_GetCapture(){if(!state.attached)return real_GetCapture();Lock l;return state.capture;}
HWND WINAPI hook_WindowFromPoint(POINT point){if(!state.attached)return real_WindowFromPoint(point);Lock l;ScreenToClient(state.target,&point);return ChildWindowFromPointEx(state.target,point,CWP_SKIPINVISIBLE|CWP_SKIPDISABLED);}
int nonactivating(int command){if(command==SW_HIDE||command==SW_SHOWNOACTIVATE||command==SW_SHOWNA||command==SW_SHOWMINNOACTIVE)return command;if(command==SW_MINIMIZE||command==SW_SHOWMINIMIZED||command==SW_FORCEMINIMIZE)return SW_SHOWMINNOACTIVE;return SW_SHOWNOACTIVATE;}
BOOL WINAPI hook_ShowWindow(HWND h,int command){if(state.attached)command=nonactivating(command);return real_ShowWindow(h,command);}
BOOL WINAPI hook_ShowWindowAsync(HWND h,int command){if(state.attached)command=nonactivating(command);return real_ShowWindowAsync(h,command);}
BOOL WINAPI hook_SetWindowPos(HWND h,HWND after,int x,int y,int cx,int cy,UINT flags){if(state.attached){flags|=SWP_NOACTIVATE|SWP_NOZORDER;after=nullptr;}return real_SetWindowPos(h,after,x,y,cx,cy,flags);}
BOOL WINAPI hook_DestroyWindow(HWND h){if(state.attached&&h==state.target){Lock l;state.destroyed=true;neutralize(false);state.playing=false;}return real_DestroyWindow(h);}

bool keyboard_mouse(const RAWINPUTDEVICE& d){return d.usUsagePage==1&&(d.usUsage==2||d.usUsage==6);}
void snapshot_registrations() {
    UINT count=0;real_GetRegisteredRawInputDevices(nullptr,&count,sizeof(RAWINPUTDEVICE));
    std::vector<RAWINPUTDEVICE> devices(count);if(count&&real_GetRegisteredRawInputDevices(devices.data(),&count,sizeof(RAWINPUTDEVICE))==UINT(-1))return;
    for(auto& d:devices)if(keyboard_mouse(d)){auto& r=d.usUsage==6?state.keyboard:state.mouse;r.present=true;r.device=d;}
}
void unregister_physical() {RAWINPUTDEVICE d[2]={{1,2,RIDEV_REMOVE,nullptr},{1,6,RIDEV_REMOVE,nullptr}};real_RegisterRawInputDevices(d,2,sizeof(RAWINPUTDEVICE));}
void restore_registrations() {
    // Serialize the pass-through transition with registrations made by game threads.
    // A post-detach registration must not be overwritten by our older snapshot.
    Lock l;
    unregister_physical();if(state.keyboard.present)real_RegisterRawInputDevices(&state.keyboard.device,1,sizeof(RAWINPUTDEVICE));if(state.mouse.present)real_RegisterRawInputDevices(&state.mouse.device,1,sizeof(RAWINPUTDEVICE));
    state.attached=false;
}
BOOL WINAPI hook_RegisterRawInputDevices(PCRAWINPUTDEVICE devices,UINT count,UINT size) {
    Lock lock;
    if(!state.attached)return real_RegisterRawInputDevices(devices,count,size);
    if(!devices||!count||count>512||size!=sizeof(RAWINPUTDEVICE)){SetLastError(ERROR_INVALID_PARAMETER);return FALSE;}
    for(UINT i=0;i<count;++i){const auto& d=devices[i];if(keyboard_mouse(d)) {
        if((d.dwFlags&RIDEV_REMOVE)&&d.hwndTarget){SetLastError(ERROR_INVALID_PARAMETER);return FALSE;}
        if(d.hwndTarget&&!process_window(d.hwndTarget)){SetLastError(ERROR_INVALID_WINDOW_HANDLE);return FALSE;}
    }}
    // Validate and apply non-keyboard/mouse registrations before committing the private snapshot.
    for(UINT i=0;i<count;++i)if(!keyboard_mouse(devices[i])&&!real_RegisterRawInputDevices(&devices[i],1,size))return FALSE;
    for(UINT i=0;i<count;++i)if(keyboard_mouse(devices[i])){auto& r=devices[i].usUsage==6?state.keyboard:state.mouse;r.present=!(devices[i].dwFlags&RIDEV_REMOVE);r.device=devices[i];}
    unregister_physical();return TRUE;
}
UINT WINAPI hook_GetRegisteredRawInputDevices(PRAWINPUTDEVICE output,PUINT size,UINT cb) {
    if(!state.attached)return real_GetRegisteredRawInputDevices(output,size,cb);
    if(!size||cb!=sizeof(RAWINPUTDEVICE)){SetLastError(ERROR_INVALID_PARAMETER);return UINT(-1);}
    RAWINPUTDEVICE list[514]{};UINT count=512;if(real_GetRegisteredRawInputDevices(list,&count,cb)==UINT(-1))return UINT(-1);
    count=real_GetRegisteredRawInputDevices(list,&count,cb);if(count==UINT(-1))return count;
    UINT n=0;for(UINT i=0;i<count;++i)if(!keyboard_mouse(list[i]))list[n++]=list[i];
    {Lock l;if(state.keyboard.present)list[n++]=state.keyboard.device;if(state.mouse.present)list[n++]=state.mouse.device;}
    if(!output){*size=n;return 0;}if(*size<n){*size=n;SetLastError(ERROR_INSUFFICIENT_BUFFER);return UINT(-1);}memcpy(output,list,n*cb);*size=n;return n;
}
UINT WINAPI hook_GetRawInputData(HRAWINPUT handle,UINT command,LPVOID output,PUINT size,UINT header) {
    if(!token_handle(handle))return real_GetRawInputData(handle,command,output,size,header);
    if(!size||header!=sizeof(RAWINPUTHEADER)||(command!=RID_INPUT&&command!=RID_HEADER)){SetLastError(ERROR_INVALID_PARAMETER);return UINT(-1);}
    Lock l;auto r=record(reinterpret_cast<uint64_t>(handle));if(!r||!r->raw){SetLastError(ERROR_INVALID_HANDLE);return UINT(-1);}
    UINT required=command==RID_INPUT?r->input.header.dwSize:sizeof(RAWINPUTHEADER);
    if(!output){*size=required;return 0;}if(*size<required){*size=required;SetLastError(ERROR_INSUFFICIENT_BUFFER);return UINT(-1);}
    memcpy(output,&r->input,required);*size=required;if(command==RID_INPUT){if(!r->read)delivered(*r);r->read=true;}return required;
}
void remove_raw_marker(uint64_t token){
    MSG message{};
    // Restrict the peek to posted messages: dispatching sent messages while
    // holding our state lock could reenter an input hook from the window proc.
    while(real_PeekMessageW(&message,nullptr,WM_INPUT,WM_INPUT,PM_NOREMOVE|PM_QS_POSTMESSAGE)){
        auto value=static_cast<uint64_t>(message.lParam);
        if(!token_handle(reinterpret_cast<HRAWINPUT>(value)))break;
        if(value!=token&&record(value))break;
        real_PeekMessageW(&message,nullptr,WM_INPUT,WM_INPUT,PM_REMOVE|PM_QS_POSTMESSAGE);
        consume_marker();
        if(value==token)break;
    }
}
UINT private_raw_buffer(PRAWINPUT output,PUINT size,UINT header) {
    if(!size||header!=sizeof(RAWINPUTHEADER)){SetLastError(ERROR_INVALID_PARAMETER);return UINT(-1);}
    Lock l;DWORD thread=GetCurrentThreadId();UINT count=0,used=0;
    Record* first=nullptr;for(auto& r:state.records)if(r.token&&r.raw&&!r.dequeued&&r.thread==thread&&r.generation==state.generation&&(!first||r.token<first->token))first=&r;
    if(!first){remove_raw_marker(0);if(!state.attached){finish_marker_drain();return UINT(-2);}}
    if(!output){*size=first?first->input.header.dwSize:0;return 0;}if(first&&*size<first->input.header.dwSize){*size=first->input.header.dwSize;SetLastError(ERROR_INSUFFICIENT_BUFFER);return UINT(-1);}
    while(first) {
        UINT stride=(first->input.header.dwSize+7)&~7;if(used+stride>*size)break;
        memset(reinterpret_cast<BYTE*>(output)+used,0,stride);memcpy(reinterpret_cast<BYTE*>(output)+used,&first->input,first->input.header.dwSize);
        remove_raw_marker(first->token);
        sync_queue();if(first->vk)set_key(queue_state.keys,first->vk,first->down);delivered(*first);first->token=0;++count;used+=stride;first=nullptr;
        for(auto& r:state.records)if(r.token&&r.raw&&!r.dequeued&&r.thread==thread&&r.generation==state.generation&&(!first||r.token<first->token))first=&r;
    }
    remove_raw_marker(0);finish_marker_drain();if(!count)*size=0;return count;
}
UINT WINAPI hook_GetRawInputBuffer(PRAWINPUT output,PUINT size,UINT header) {
    if(!state.attached&&!state.markers)return real_GetRawInputBuffer(output,size,header);
    UINT result=private_raw_buffer(output,size,header);
    return result==UINT(-2)?real_GetRawInputBuffer(output,size,header):result;
}
bool pending_raw_marker(){MSG msg{};return real_PeekMessageW(&msg,nullptr,WM_INPUT,WM_INPUT,PM_NOREMOVE|PM_QS_POSTMESSAGE)&&token_handle(reinterpret_cast<HRAWINPUT>(msg.lParam));}

constexpr DWORD PrivateQueueFlags=QS_KEY|QS_MOUSEMOVE|QS_MOUSEBUTTON|QS_RAWINPUT;
DWORD private_queue(DWORD requested,bool update){
    constexpr DWORD flags[]={QS_KEY,QS_MOUSEMOVE,QS_MOUSEBUTTON,QS_RAWINPUT};
    uint64_t latest[4]{};DWORD current=0,changed=0,thread=GetCurrentThreadId();Lock l;sync_queue();
    for(const auto& r:state.records)if(r.token&&r.generation==state.generation&&r.thread==thread){
        int kind=-1;if(r.raw&&!r.dequeued)kind=3;
        else if(!r.raw){if(r.message.message>=WM_KEYFIRST&&r.message.message<=WM_KEYLAST)kind=0;else if(r.message.message==WM_MOUSEMOVE)kind=1;else if(r.message.message>=WM_MOUSEFIRST&&r.message.message<=WM_MOUSELAST)kind=2;}
        if(kind>=0){current|=flags[kind];latest[kind]=std::max(latest[kind],r.token);}
    }
    for(unsigned i=0;i<4;++i)if(requested&flags[i]){if(latest[i]>queue_state.seen[i])changed|=flags[i];if(update)queue_state.seen[i]=std::max(queue_state.seen[i],latest[i]);}
    return ((current&requested)<<16)|(changed&requested);
}
DWORD WINAPI hook_GetQueueStatus(UINT flags){DWORD actual=real_GetQueueStatus(flags);if(!state.attached){if(!state.markers)return actual;DWORD pending=private_queue(flags,true);if((flags&QS_RAWINPUT)&&pending_raw_marker())pending|=MAKELONG(QS_RAWINPUT,QS_RAWINPUT);return actual|pending;}DWORD mask=(flags&PrivateQueueFlags);mask|=mask<<16;return (actual&~mask)|private_queue(flags,true);}
BOOL WINAPI hook_GetInputState(){if(state.attached)return (private_queue(QS_KEY|QS_MOUSEBUTTON,false)>>16)!=0;return real_GetInputState()||(state.markers&&(private_queue(QS_KEY|QS_MOUSEBUTTON,false)>>16)!=0);}
DWORD WINAPI hook_GetMessagePos(){if(!state.attached)return real_GetMessagePos();Lock l;sync_queue();POINT p=queue_state.synthetic?queue_state.last.pt:state.cursor;if(!queue_state.synthetic)ClientToScreen(state.target,&p);return MAKELONG(p.x,p.y);}
LONG WINAPI hook_GetMessageTime(){if(!state.attached)return real_GetMessageTime();Lock l;sync_queue();return queue_state.synthetic?queue_state.last.time:real_GetMessageTime();}
DWORD WINAPI hook_MsgWaitForMultipleObjectsEx(DWORD count,const HANDLE* handles,DWORD timeout,DWORD mask,DWORD flags){
    bool attached=state.attached;
    if((!attached&&!state.markers)||!(mask&PrivateQueueFlags))return real_MsgWaitForMultipleObjectsEx(count,handles,timeout,mask,flags);
    bool ready=(private_queue(mask,false)>>16)!=0;
    if(!attached){ready=ready||((mask&QS_RAWINPUT)&&pending_raw_marker());if(!ready)return real_MsgWaitForMultipleObjectsEx(count,handles,timeout,mask,flags);}
    DWORD mapped=(attached?(mask&~PrivateQueueFlags):mask)|QS_POSTMESSAGE;
    if(ready){DWORD result=real_MsgWaitForMultipleObjectsEx(count,handles,0,mapped,flags|MWMO_INPUTAVAILABLE);if(result!=WAIT_TIMEOUT)return result;if(!(flags&MWMO_WAITALL)||!count)return WAIT_OBJECT_0+count;}
    return real_MsgWaitForMultipleObjectsEx(count,handles,timeout,mapped,flags|(ready?MWMO_INPUTAVAILABLE:0));
}
DWORD WINAPI hook_MsgWaitForMultipleObjects(DWORD count,const HANDLE* handles,BOOL all,DWORD timeout,DWORD mask){if(!state.attached&&!state.markers)return real_MsgWaitForMultipleObjects(count,handles,all,timeout,mask);return hook_MsgWaitForMultipleObjectsEx(count,handles,timeout,mask,all?MWMO_WAITALL:0);}
UINT WINAPI hook_GetRawInputDeviceList(PRAWINPUTDEVICELIST output,PUINT size,UINT cb) {
    if(!state.attached)return real_GetRawInputDeviceList(output,size,cb);
    if(!size||cb!=sizeof(RAWINPUTDEVICELIST)){SetLastError(ERROR_INVALID_PARAMETER);return UINT(-1);}
    RAWINPUTDEVICELIST list[514]{};UINT count=512;if(real_GetRawInputDeviceList(list,&count,cb)==UINT(-1))return UINT(-1);
    count=real_GetRawInputDeviceList(list,&count,cb);if(count==UINT(-1))return count;
    UINT n=0;for(UINT i=0;i<count;++i)if(list[i].dwType!=RIM_TYPEMOUSE&&list[i].dwType!=RIM_TYPEKEYBOARD)list[n++]=list[i];
    list[n++]={reinterpret_cast<HANDLE>(KeyboardDevice),RIM_TYPEKEYBOARD};list[n++]={reinterpret_cast<HANDLE>(MouseDevice),RIM_TYPEMOUSE};
    if(!output){*size=n;return 0;}if(*size<n){*size=n;SetLastError(ERROR_INSUFFICIENT_BUFFER);return UINT(-1);}memcpy(output,list,n*cb);*size=n;return n;
}
template<typename Character> UINT device_info(HANDLE device,UINT command,LPVOID output,PUINT size) {
    if(!size){SetLastError(ERROR_INVALID_PARAMETER);return UINT(-1);}
    if(command==RIDI_DEVICEINFO) {
        UINT required=sizeof(RID_DEVICE_INFO);if(!output){*size=required;return 0;}if(*size<required){*size=required;SetLastError(ERROR_INSUFFICIENT_BUFFER);return UINT(-1);}
        auto info=static_cast<RID_DEVICE_INFO*>(output);if(info->cbSize!=required){SetLastError(ERROR_INVALID_PARAMETER);return UINT(-1);}
        *info={};info->cbSize=required;info->dwType=reinterpret_cast<uintptr_t>(device)==KeyboardDevice?RIM_TYPEKEYBOARD:RIM_TYPEMOUSE;
        if(info->dwType==RIM_TYPEKEYBOARD){info->keyboard.dwType=4;info->keyboard.dwNumberOfKeysTotal=104;info->keyboard.dwNumberOfFunctionKeys=12;}
        else{info->mouse.dwId=1;info->mouse.dwNumberOfButtons=5;info->mouse.dwSampleRate=1000;info->mouse.fHasHorizontalWheel=TRUE;}
        *size=required;return required;
    }
    if(command==RIDI_DEVICENAME) {
        const char* name=reinterpret_cast<uintptr_t>(device)==KeyboardDevice?"\\\\?\\FramesSharedKeyboard":"\\\\?\\FramesSharedMouse";
        UINT length=static_cast<UINT>(strlen(name)),required=length+1;
        if(!output){*size=required;return 0;}if(*size<required){*size=required;SetLastError(ERROR_INSUFFICIENT_BUFFER);return UINT(-1);}
        auto p=static_cast<Character*>(output);for(UINT i=0;i<required;++i)p[i]=name[i];*size=required;return length;
    }
    SetLastError(ERROR_INVALID_FUNCTION);return UINT(-1);
}
UINT WINAPI hook_GetRawInputDeviceInfoW(HANDLE d,UINT c,LPVOID p,PUINT n){return virtual_device(d)?device_info<wchar_t>(d,c,p,n):real_GetRawInputDeviceInfoW(d,c,p,n);}
UINT WINAPI hook_GetRawInputDeviceInfoA(HANDLE d,UINT c,LPVOID p,PUINT n){return virtual_device(d)?device_info<char>(d,c,p,n):real_GetRawInputDeviceInfoA(d,c,p,n);}

bool filter(MSG* msg,bool remove) {
    Lock l;sync_queue();queue_state.synthetic=false;
    if(remove&&queue_state.last_raw){auto prior=record(queue_state.last_raw);if(prior&&prior->read)prior->token=0;queue_state.last_raw=0;}
    if(msg->message!=WM_INPUT&&(static_cast<uint64_t>(msg->lParam)&TokenMask)==TokenPrefix) {
        if(remove)consume_marker();
        auto r=record(static_cast<uint64_t>(msg->lParam));if(!r||r->raw){if(remove)finish_marker_drain();msg->message=WM_NULL;return false;}
        *msg=r->message;if(remove){if(r->vk)set_key(queue_state.keys,r->vk,r->down);r->token=0;}
        if(remove)finish_marker_drain();queue_state.last=*msg;queue_state.synthetic=true;return true;
    }
    if(msg->message==WM_INPUT) {
        if(!token_handle(reinterpret_cast<HRAWINPUT>(msg->lParam))) {
            if(!state.attached)return true;
            if(remove)real_DefWindowProcW(msg->hwnd,WM_INPUT,msg->wParam,msg->lParam);
            msg->message=WM_NULL;return false;
        }
        if(remove)consume_marker();
        auto r=record(static_cast<uint64_t>(msg->lParam));if(!r||!r->raw){if(remove)finish_marker_drain();msg->message=WM_NULL;return false;}
        msg->pt=r->message.pt;msg->time=r->message.time;queue_state.last=*msg;queue_state.synthetic=true;
        if(remove){r->dequeued=true;if(r->vk)set_key(queue_state.keys,r->vk,r->down);queue_state.last_raw=r->token;finish_marker_drain();}return true;
    }
    if(!state.attached)return true;
    if((msg->message>=WM_KEYFIRST&&msg->message<=WM_KEYLAST)||(msg->message>=WM_MOUSEFIRST&&msg->message<=WM_MOUSELAST)
        ||msg->message==WM_KILLFOCUS||msg->message==WM_SETFOCUS||msg->message==WM_ACTIVATE||msg->message==WM_ACTIVATEAPP||msg->message==WM_NCACTIVATE) {
        msg->message=WM_NULL;return false;
    }
    return true;
}
BOOL WINAPI hook_GetMessageW(LPMSG msg,HWND h,UINT a,UINT b){BOOL result=real_GetMessageW(msg,h,a,b);if(result>0)filter(msg,true);return result;}
BOOL WINAPI hook_GetMessageA(LPMSG msg,HWND h,UINT a,UINT b){BOOL result=real_GetMessageA(msg,h,a,b);if(result>0)filter(msg,true);return result;}
BOOL WINAPI hook_PeekMessageW(LPMSG msg,HWND h,UINT a,UINT b,UINT flags){BOOL result=real_PeekMessageW(msg,h,a,b,flags);if(result)filter(msg,(flags&PM_REMOVE)!=0);return result;}
BOOL WINAPI hook_PeekMessageA(LPMSG msg,HWND h,UINT a,UINT b,UINT flags){BOOL result=real_PeekMessageA(msg,h,a,b,flags);if(result)filter(msg,(flags&PM_REMOVE)!=0);return result;}
BOOL WINAPI hook_TranslateMessage(const MSG* msg) {
    if(!state.attached||!msg||!queue_state.synthetic||msg->hwnd!=queue_state.last.hwnd||msg->message!=queue_state.last.message)return real_TranslateMessage(msg);
    if(msg->message!=WM_KEYDOWN&&msg->message!=WM_SYSKEYDOWN)return FALSE;
    wchar_t text[8]{};int count=ToUnicodeEx(static_cast<UINT>(msg->wParam),static_cast<UINT>((msg->lParam>>16)&255),queue_state.keys,text,8,4,GetKeyboardLayout(0));
    if(count>0){Lock l;for(int i=0;i<count;++i)legacy(msg->hwnd,msg->message==WM_SYSKEYDOWN?WM_SYSCHAR:WM_CHAR,text[i],msg->lParam);}return count!=0;
}
LRESULT finish_raw(UINT message,LPARAM parameter) {
    if(message==WM_INPUT&&token_handle(reinterpret_cast<HRAWINPUT>(parameter))){Lock l;auto r=record(static_cast<uint64_t>(parameter));if(r)r->token=0;return 0;}return -1;
}
LRESULT WINAPI hook_DefWindowProcW(HWND h,UINT m,WPARAM w,LPARAM p){if(finish_raw(m,p)==0)return 0;return real_DefWindowProcW(h,m,w,p);}
LRESULT WINAPI hook_DefWindowProcA(HWND h,UINT m,WPARAM w,LPARAM p){if(finish_raw(m,p)==0)return 0;return real_DefWindowProcA(h,m,w,p);}
LRESULT WINAPI hook_DefRawInputProc(PRAWINPUT* input,INT count,UINT header){if(!state.attached)return real_DefRawInputProc(input,count,header);if(!input||count<0||header!=sizeof(RAWINPUTHEADER)){SetLastError(ERROR_INVALID_PARAMETER);return 0;}for(INT i=0;i<count;++i)if(input[i]&&!virtual_device(input[i]->header.hDevice))real_DefRawInputProc(&input[i],1,header);return 0;}

template<typename T> bool install(const char* name,T replacement,T& original) {
    if(hook_count==hooks.size())return false;
    auto target=GetProcAddress(GetModuleHandleW(L"user32.dll"),name);if(!target)return false;
    auto& hook=hooks[hook_count];NTSTATUS status=LhInstallHook(target,reinterpret_cast<void*>(replacement),nullptr,&hook);
    if(status<0)return false;original_locations[hook_count]=&original;target_locations[hook_count]=reinterpret_cast<void*>(target);++hook_count;
    if(LhGetHookBypassAddress(&hook,reinterpret_cast<PVOID**>(&original))<0)return false;
    ULONG excluded=GetCurrentThreadId();return LhSetExclusiveACL(&excluded,1,&hook)>=0;
}
bool install_hooks() {
    if(state.installed){ULONG excluded=GetCurrentThreadId();for(size_t i=0;i<hook_count;++i)if(LhSetExclusiveACL(&excluded,1,&hooks[i])<0)return false;return true;}
#define HOOK(name) if(!install(#name,hook_##name,real_##name))return false
    HOOK(GetAsyncKeyState);HOOK(GetKeyState);HOOK(GetKeyboardState);HOOK(SetKeyboardState);
    HOOK(GetCursorPos);HOOK(SetCursorPos);HOOK(GetPhysicalCursorPos);HOOK(SetPhysicalCursorPos);HOOK(ClipCursor);HOOK(GetClipCursor);
    HOOK(ShowCursor);HOOK(SetCursor);HOOK(SetSystemCursor);HOOK(GetCursorInfo);
    HOOK(GetForegroundWindow);HOOK(GetActiveWindow);HOOK(GetFocus);HOOK(SetForegroundWindow);HOOK(SetActiveWindow);HOOK(SetFocus);
    HOOK(GetCapture);HOOK(SetCapture);HOOK(ReleaseCapture);HOOK(WindowFromPoint);HOOK(ShowWindow);HOOK(SetWindowPos);
    HOOK(DestroyWindow);
    HOOK(ShowWindowAsync);
    HOOK(RegisterRawInputDevices);HOOK(GetRegisteredRawInputDevices);HOOK(GetRawInputData);HOOK(GetRawInputBuffer);
    HOOK(GetRawInputDeviceList);HOOK(GetRawInputDeviceInfoW);HOOK(GetRawInputDeviceInfoA);
    HOOK(GetMessageW);HOOK(GetMessageA);HOOK(PeekMessageW);HOOK(PeekMessageA);HOOK(TranslateMessage);
    HOOK(GetQueueStatus);HOOK(GetInputState);HOOK(GetMessagePos);HOOK(GetMessageTime);HOOK(MsgWaitForMultipleObjects);HOOK(MsgWaitForMultipleObjectsEx);
    HOOK(DefWindowProcW);HOOK(DefWindowProcA);HOOK(DefRawInputProc);
#undef HOOK
    state.installed=true;return true;
}
void cursor_image(Status& result,HCURSOR cursor) {
    if(!cursor)return;ICONINFO icon{};if(!GetIconInfo(cursor,&icon))return;
    BITMAP bitmap{};GetObjectW(icon.hbmColor?icon.hbmColor:icon.hbmMask,sizeof(bitmap),&bitmap);
    UINT w=std::min<UINT>(std::max<LONG>(1,bitmap.bmWidth),MaxCursor),h=std::min<UINT>(std::max<LONG>(1,icon.hbmColor?bitmap.bmHeight:bitmap.bmHeight/2),MaxCursor);
    result.hot_x=std::min<UINT>(icon.xHotspot,w-1);result.hot_y=std::min<UINT>(icon.yHotspot,h-1);
    BITMAPINFO info{};info.bmiHeader.biSize=sizeof(BITMAPINFOHEADER);info.bmiHeader.biWidth=w;info.bmiHeader.biHeight=-static_cast<LONG>(h);info.bmiHeader.biPlanes=1;info.bmiHeader.biBitCount=32;info.bmiHeader.biCompression=BI_RGB;
    HDC dc=CreateCompatibleDC(nullptr);void* bits=nullptr;HBITMAP dib=dc?CreateDIBSection(dc,&info,DIB_RGB_COLORS,&bits,nullptr,0):nullptr;
    if(!dib||!bits){if(dib)DeleteObject(dib);if(dc)DeleteDC(dc);if(icon.hbmColor)DeleteObject(icon.hbmColor);if(icon.hbmMask)DeleteObject(icon.hbmMask);return;}
    result.width=w;result.height=h;auto old=SelectObject(dc,dib);
    std::array<BYTE,CursorBytes> black{};memset(bits,0,w*h*4);DrawIconEx(dc,0,0,cursor,w,h,0,nullptr,DI_NORMAL);memcpy(black.data(),bits,w*h*4);
    memset(bits,255,w*h*4);DrawIconEx(dc,0,0,cursor,w,h,0,nullptr,DI_NORMAL);
    auto white=static_cast<BYTE*>(bits);for(UINT i=0;i<w*h;++i){int alpha=255;for(int c=0;c<3;++c)alpha=std::min(alpha,255-std::max(0,int(white[i*4+c])-int(black[i*4+c])));for(int c=0;c<3;++c)result.rgba[i*4+c]=alpha?static_cast<BYTE>(std::min(255,int(black[i*4+2-c])*255/alpha)):0;result.rgba[i*4+3]=static_cast<BYTE>(alpha);}
    SelectObject(dc,old);DeleteObject(dib);DeleteDC(dc);if(icon.hbmColor)DeleteObject(icon.hbmColor);if(icon.hbmMask)DeleteObject(icon.hbmMask);
}
bool write_snapshot(HANDLE pipe,Status& snapshot,const std::atomic<bool>& stop) {
    OVERLAPPED ov{};ov.hEvent=CreateEventW(nullptr,TRUE,FALSE,nullptr);if(!ov.hEvent)return false;
    DWORD done=0;BOOL ok=WriteFile(pipe,&snapshot,sizeof(snapshot),&done,&ov);
    if(!ok&&GetLastError()==ERROR_IO_PENDING){
        while(!stop&&WaitForSingleObject(ov.hEvent,100)==WAIT_TIMEOUT){}
        if(stop)CancelIoEx(pipe,&ov);
        ok=GetOverlappedResult(pipe,&ov,&done,TRUE);
    }
    DWORD error=ok?ERROR_SUCCESS:GetLastError();CloseHandle(ov.hEvent);SetLastError(error);
    return ok&&done==sizeof(snapshot);
}
bool status(HANDLE pipe,uint64_t generation,DWORD timeout=100,const std::atomic<bool>* stop=nullptr) {
    Status s;HCURSOR image;{Lock l;s.generation=generation;s.phase=state.error?3:state.playing?1:2;s.error=state.error;s.x=state.cursor.x;s.y=state.cursor.y;s.visible=state.image&&state.display_count>=0;image=state.image;s.image_generation=reinterpret_cast<uint64_t>(image);s.delivered=state.delivered;s.latency_max_us=state.latency_max;memcpy(s.latency_histogram,state.histogram,sizeof(state.histogram));}
    cursor_image(s,image);s.visible=s.visible&&s.width&&s.height;
    return stop?write_snapshot(pipe,s,*stop):transfer(pipe,&s,sizeof(s),true,timeout);
}
// Status backpressure must never block the input worker or its 500 ms deadman.
// A single replaceable notification bounds pending work, including cursor snapshots.
struct StatusWriter { HANDLE pipe,event;uint64_t generation;std::atomic<bool> stop{false},failed{false};std::atomic<DWORD> error{0}; };
DWORD WINAPI write_status(void* argument) {
    auto& writer=*static_cast<StatusWriter*>(argument);
    while(!writer.stop){
        if(WaitForSingleObject(writer.event,100)!=WAIT_OBJECT_0)continue;
        if(writer.stop)break;
        if(!status(writer.pipe,writer.generation,500,&writer.stop)){writer.error=GetLastError();writer.failed=true;break;}
    }
    return 0;
}
void apply(const Command& c) {
    Lock l;
    state.sent=c.reserved[0];
    if(c.kind==Release){neutralize(true);state.playing=false;activation(false);return;}
    if(c.kind==Heartbeat){state.lease=GetTickCount64();return;}
    if(c.kind==Resume){state.lease=GetTickCount64();state.error=0;if(!state.playing){state.playing=true;state.focus=state.target;activation(true);}return;}
    if(!state.playing||state.destroyed)return;
    if(c.kind==Key)key_event(static_cast<UINT>(c.a),c.b!=0);
    else if(c.kind==Button){
        if(!c.reserved[1]){state.cursor={c.c,c.d};RECT bounds{};GetClientRect(state.target,&bounds);if(state.clipped)bounds=state.clip;
            state.cursor.x=std::clamp(state.cursor.x,bounds.left,std::max(bounds.left,bounds.right-1));state.cursor.y=std::clamp(state.cursor.y,bounds.top,std::max(bounds.top,bounds.bottom-1));}
        button_event(c.a,c.b!=0);
    }
    else if(c.kind==Absolute||c.kind==Relative) {
        int dx=c.kind==Relative?c.a:c.a-state.cursor.x,dy=c.kind==Relative?c.b:c.b-state.cursor.y;
        if(c.kind==Relative){state.cursor.x+=dx;state.cursor.y+=dy;}else state.cursor={c.a,c.b};
        RECT bounds{};GetClientRect(state.target,&bounds);if(state.clipped)bounds=state.clip;
        state.cursor.x=std::clamp(state.cursor.x,bounds.left,std::max(bounds.left,bounds.right-1));state.cursor.y=std::clamp(state.cursor.y,bounds.top,std::max(bounds.top,bounds.bottom-1));
        RAWINPUT ri{};ri.header.dwType=RIM_TYPEMOUSE;ri.header.hDevice=reinterpret_cast<HANDLE>(MouseDevice);ri.data.mouse.lLastX=dx;ri.data.mouse.lLastY=dy;raw(ri,state.mouse);
        if(!no_legacy(state.mouse))legacy(mouse_target(),WM_MOUSEMOVE,mouse_flags(),MAKELPARAM(state.cursor.x,state.cursor.y));
    } else if(c.kind==Scroll) {
        for(int axis=0;axis<2;++axis){int delta=axis?c.b:c.a;if(!delta)continue;RAWINPUT ri{};ri.header.dwType=RIM_TYPEMOUSE;ri.header.hDevice=reinterpret_cast<HANDLE>(MouseDevice);ri.data.mouse.usButtonFlags=axis?RI_MOUSE_WHEEL:RI_MOUSE_HWHEEL;ri.data.mouse.usButtonData=static_cast<USHORT>(delta);raw(ri,state.mouse);POINT screen=state.cursor;ClientToScreen(state.target,&screen);if(!no_legacy(state.mouse))legacy(state.target,axis?WM_MOUSEWHEEL:WM_MOUSEHWHEEL,MAKEWPARAM(mouse_flags(),static_cast<SHORT>(delta)),MAKELPARAM(screen.x,screen.y));}
    } else if(c.kind==Text&&c.a>0&&c.a<=0xFFFF)legacy(state.focus?state.focus:state.target,WM_CHAR,c.a,1);
    if(state.error)neutralize(true);
}
DWORD WINAPI run(void* parameter) {
    Start config=*static_cast<Start*>(parameter);delete static_cast<Start*>(parameter);
    HANDLE pipe=CreateFileW(config.pipe,GENERIC_READ|GENERIC_WRITE,0,nullptr,OPEN_EXISTING,FILE_FLAG_OVERLAPPED,nullptr);
    if(pipe==INVALID_HANDLE_VALUE){state.worker=false;return 1;}
    ULONG server=0;DWORD session=0,broker_session=1;
    if(!GetNamedPipeServerProcessId(pipe,&server)||server!=config.broker_pid||!ProcessIdToSessionId(server,&broker_session)||!ProcessIdToSessionId(GetCurrentProcessId(),&session)||session!=broker_session){CloseHandle(pipe);state.worker=false;return 2;}
    Hello hello;hello.pid=GetCurrentProcessId();memcpy(hello.nonce,config.nonce,32);
    if(!transfer(pipe,&hello,sizeof(hello),true,1000)){CloseHandle(pipe);state.worker=false;return 3;}
    SetThreadDpiAwarenessContext(GetWindowDpiAwarenessContext(reinterpret_cast<HWND>(config.hwnd)));
    {Lock l;state.target=reinterpret_cast<HWND>(config.hwnd);state.destroyed=false;state.focus=state.target;state.capture=nullptr;state.generation=config.generation;state.sequence=0;state.error=0;state.playing=false;state.lease=GetTickCount64();state.keyboard={};state.mouse={};state.image=LoadCursorW(nullptr,IDC_ARROW);state.display_count=0;neutralize(false);snapshot_registrations();}
    QueryPerformanceFrequency(&state.frequency);state.delivered=state.latency_max=0;memset(state.histogram,0,sizeof(state.histogram));
    bool installed=install_hooks();
    if(installed){Lock l;state.keyboard={};state.mouse={};snapshot_registrations();unregister_physical();state.attached=true;}else{state.error=ERROR_NOT_SUPPORTED;LhUninstallAllHooks();LhWaitForPendingRemovals();for(size_t i=0;i<hook_count;++i)memcpy(original_locations[i],&target_locations[i],sizeof(void*));hooks={};hook_count=0;state.installed=false;}
    if(!status(pipe,config.generation,1000))installed=false;
    StatusWriter writer{pipe,CreateEventW(nullptr,FALSE,FALSE,nullptr),config.generation};
    HANDLE writer_thread=writer.event&&installed?CreateThread(nullptr,0,write_status,&writer,0,nullptr):nullptr;
    if(!writer_thread)installed=false;
    state.stop_reason=state.stop_error=0;
    auto expire_lease=[&] {
        Lock l;
        if(state.playing&&(GetTickCount64()-state.lease>=500||IsIconic(state.target))){neutralize(true);state.playing=false;activation(false);SetEvent(writer.event);}
    };
    while(installed) {
        if(writer.failed){state.stop_reason=1;state.stop_error=writer.error.load();break;}
        if(state.destroyed){state.stop_reason=2;break;}
        if(!IsWindow(state.target)){state.stop_reason=3;break;}
        DWORD pid=0;GetWindowThreadProcessId(state.target,&pid);if(pid!=GetCurrentProcessId()){state.stop_reason=4;break;}
        auto now=GetTickCount64();DWORD timeout=100;
        {Lock l;if(state.playing)timeout=now<state.lease+500?static_cast<DWORD>(std::min<ULONGLONG>(100,state.lease+500-now)):1;}
        Command c;bool received=transfer(pipe,&c,sizeof(c),false,timeout);
        DWORD error=GetLastError();if(!received&&error!=ERROR_TIMEOUT){state.stop_reason=5;state.stop_error=error;break;}
        // A scheduler/process stall can leave an old heartbeat queued in the pipe.
        // Expire the old lease before that heartbeat updates its timestamp.
        expire_lease();
        if(received){
            auto bounded=[](int32_t value,int32_t limit){return value>=-limit&&value<=limit;};
            bool valid=c.magic==Magic&&c.size==sizeof(c)&&c.abi==Abi&&c.generation==config.generation&&c.sequence>state.sequence&&c.kind>=1&&c.kind<=Text;
            if(c.kind==Key)valid=valid&&c.a>0&&c.a<=0xE0FF&&(c.b==0||c.b==1);
            if(c.kind==Absolute||c.kind==Relative)valid=valid&&bounded(c.a,1000000)&&bounded(c.b,1000000);
            if(c.kind==Button)valid=valid&&c.a>=1&&c.a<=5&&(c.b==0||c.b==1)&&bounded(c.c,1000000)&&bounded(c.d,1000000)&&c.reserved[1]<=1;
            if(c.kind==Scroll)valid=valid&&bounded(c.a,32767)&&bounded(c.b,32767);
            if(c.kind==Text)valid=valid&&c.a>0&&c.a<=0xFFFF;
            if(!valid){state.stop_reason=6;break;}state.sequence=c.sequence;if(c.kind==Detach){state.stop_reason=7;break;}apply(c);
            if(c.kind==Heartbeat||c.kind==Resume||c.kind==Release)SetEvent(writer.event);
        }
        expire_lease();
        {Lock l;if(state.needs_neutralize){neutralize(true);state.playing=false;activation(false);SetEvent(writer.event);}}
    }
    {Lock l;neutralize(true);state.playing=false;activation(false);}
    writer.stop=true;if(writer.event)SetEvent(writer.event);
    if(writer_thread){WaitForSingleObject(writer_thread,INFINITE);CloseHandle(writer_thread);}
    if(writer.event)CloseHandle(writer.event);
    restore_registrations();CloseHandle(pipe);state.worker=false;return 0;
}
}

extern "C" __declspec(dllexport) uint64_t __cdecl FramesRemoteStopReason(){return (static_cast<uint64_t>(state.stop_reason.load())<<32)|state.stop_error.load();}
// Local fixture diagnostics: low 16 bits = unread raw records, next 32 = posted owned markers.
extern "C" __declspec(dllexport) uint64_t __cdecl FramesRemotePending(){Lock l;uint64_t raw=0;for(const auto& r:state.records)if(r.token&&r.raw&&!r.dequeued)++raw;return raw|(uint64_t(state.markers.load())<<16)|(uint64_t(state.attached.load())<<63);}

extern "C" __declspec(dllexport) DWORD WINAPI FramesRemoteStart(void* parameter) {
    if(!parameter)return ERROR_INVALID_PARAMETER;const auto& start=*static_cast<Start*>(parameter);
    if(start.magic!=Magic||start.size!=sizeof(Start)||start.abi!=Abi||!start.generation||start.pipe[127]!=0)return ERROR_INVALID_PARAMETER;
    DWORD pid=0;GetWindowThreadProcessId(reinterpret_cast<HWND>(start.hwnd),&pid);if(pid!=GetCurrentProcessId())return ERROR_INVALID_WINDOW_HANDLE;
    bool expected=false;if(!state.worker.compare_exchange_strong(expected,true))return ERROR_BUSY;
    auto copy=new Start(start);HANDLE thread=CreateThread(nullptr,0,run,copy,0,nullptr);
    if(!thread){delete copy;state.worker=false;return GetLastError();}CloseHandle(thread);return ERROR_SUCCESS;
}
extern "C" BOOL WINAPI EasyHookDllMain(HMODULE,DWORD,LPVOID);
BOOL WINAPI DllMain(HMODULE module,DWORD reason,LPVOID reserved) {
    if(reason==DLL_PROCESS_ATTACH)module_handle=module;
    return EasyHookDllMain(module,reason,reserved);
}
