use std::path::Path;
use windows::{
    Win32::{
        Foundation::*,
        System::{Com::*, Ole::*, SystemServices::MK_LBUTTON},
        UI::{Input::KeyboardAndMouse::*, Shell::*, WindowsAndMessaging::*},
    },
    core::*,
};
#[implement(IDropSource)]
struct DropSource;
impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(
        &self,
        escape: BOOL,
        keys: windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS,
    ) -> HRESULT {
        if escape.as_bool() {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON.0 == 0 {
            DRAGDROP_S_DROP
        } else {
            HRESULT(0)
        }
    }
    fn GiveFeedback(&self, _: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
/// Materialized files use the Shell's IDataObject. Only copy semantics are offered.
pub fn drop_file(path: &Path, target: HWND, point: POINT) -> anyhow::Result<()> {
    let path = path.to_path_buf();
    let target = target.0 as usize;
    std::thread::spawn(move||->anyhow::Result<()>{unsafe{
        OleInitialize(None)?;
        let mut pidl=std::ptr::null_mut();let filename=super::wide(&path.to_string_lossy());SHParseDisplayName(PCWSTR(filename.as_ptr()),None,&mut pidl,0,None)?;
        let result=(||->anyhow::Result<()>{let mut last=std::ptr::null_mut();let folder:IShellFolder=SHBindToParent(pidl,Some(&mut last))?;let data:IDataObject=folder.GetUIObjectOf(HWND::default(),&[last],None)?;
            let _=SetForegroundWindow(HWND(target as *mut _));
            let source=CreateWindowExW(WS_EX_TOOLWINDOW|WS_EX_TOPMOST,w!("STATIC"),w!("Frames file drop"),WS_POPUP,point.x-1,point.y-1,3,3,None,None,None,None)?;let _=ShowWindow(source,SW_SHOWNOACTIVATE);SetCursorPos(point.x,point.y)?;
            let button=|flags|INPUT{r#type:INPUT_MOUSE,Anonymous:INPUT_0{mi:MOUSEINPUT{dwFlags:flags,..Default::default()}}};let _=SendInput(&[button(MOUSEEVENTF_LEFTDOWN)],std::mem::size_of::<INPUT>() as i32);
            let raw=source.0 as usize;let release=std::thread::spawn(move||{std::thread::sleep(std::time::Duration::from_millis(60));let source=HWND(raw as *mut _);let _=ShowWindow(source,SW_HIDE);let _=SetCursorPos(point.x+1,point.y);std::thread::sleep(std::time::Duration::from_millis(100));let _=SendInput(&[button(MOUSEEVENTF_LEFTUP)],std::mem::size_of::<INPUT>() as i32);});
            let drop_source:IDropSource=DropSource.into();let mut effect=DROPEFFECT_NONE;let result=DoDragDrop(&data,&drop_source,DROPEFFECT_COPY,&mut effect);let _=release.join();let _=DestroyWindow(source);result.ok()?;anyhow::ensure!(effect==DROPEFFECT_COPY,"application did not accept the file drop; file remains in Frames receive folder");Ok(())})();CoTaskMemFree(Some(pidl.cast()));OleUninitialize();result
    }}).join().map_err(|_|anyhow::anyhow!("file drag worker panicked"))?
}

pub fn begin_local_file(path: &Path) -> anyhow::Result<()> {
    unsafe {
        OleInitialize(None)?;
        let filename = super::wide(&path.to_string_lossy());
        let mut pidl = std::ptr::null_mut();
        SHParseDisplayName(PCWSTR(filename.as_ptr()), None, &mut pidl, 0, None)?;
        let result = (|| -> anyhow::Result<()> {
            let mut last = std::ptr::null_mut();
            let folder: IShellFolder = SHBindToParent(pidl, Some(&mut last))?;
            let data: IDataObject = folder.GetUIObjectOf(HWND::default(), &[last], None)?;
            let source: IDropSource = DropSource.into();
            let mut effect = DROPEFFECT_NONE;
            DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect).ok()?;
            Ok(())
        })();
        CoTaskMemFree(Some(pidl.cast()));
        OleUninitialize();
        result
    }
}
