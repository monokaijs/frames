//! Full-desktop capture exists only for the explicitly installed privileged agent.
//! Normal seamless windows always use WGC CreateForWindow.
use crate::{Capture, CaptureStream};
use frames_core::{media::RawFrame, model::*};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{Direct3D::D3D_DRIVER_TYPE_UNKNOWN, Direct3D11::*, Dxgi::*},
        System::{StationsAndDesktops::*, Threading::GetCurrentThreadId},
        UI::WindowsAndMessaging::*,
    },
    core::Interface,
};
pub const HANDLE: u64 = u64::MAX;
pub fn enabled() -> bool {
    std::env::var_os("FRAMES_PRIVILEGED_AGENT").is_some()
}
pub fn active() -> bool {
    unsafe {
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
        else {
            return true;
        };
        let mut name = [0u16; 256];
        let result = GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            512,
            None,
        );
        let _ = CloseDesktop(desktop);
        result.is_ok()
            && String::from_utf16_lossy(
                &name[..name.iter().position(|&v| v == 0).unwrap_or(name.len())],
            ) != "Default"
    }
}
pub fn window(key: WindowId) -> RemoteWindow {
    unsafe {
        RemoteWindow {
            key,
            app_id: "app.frames.secure-desktop".into(),
            app_name: "Secure desktop".into(),
            title: "Windows login / secure desktop".into(),
            icon_png: None,
            parent: None,
            role: WindowRole::Desktop,
            bounds: Bounds {
                x: 0.,
                y: 0.,
                width: GetSystemMetrics(SM_CXSCREEN) as f64,
                height: GetSystemMetrics(SM_CYSCREEN) as f64,
            },
            scale: 1.,
            resize: ResizeCapability::Fixed,
            minimized: false,
            visible: true,
            modal: false,
        }
    }
}
pub struct DesktopScope {
    old: HDESK,
    current: HDESK,
}
impl DesktopScope {
    pub fn enter() -> anyhow::Result<Self> {
        unsafe {
            let old = GetThreadDesktop(GetCurrentThreadId())?;
            let current = OpenInputDesktop(
                DESKTOP_CONTROL_FLAGS(0),
                false,
                DESKTOP_ACCESS_FLAGS(
                    DESKTOP_READOBJECTS.0 | DESKTOP_WRITEOBJECTS.0 | DESKTOP_SWITCHDESKTOP.0,
                ),
            )?;
            if let Err(error) = SetThreadDesktop(current) {
                let _ = CloseDesktop(current);
                return Err(error.into());
            }
            Ok(Self { old, current })
        }
    }
}
impl Drop for DesktopScope {
    fn drop(&mut self) {
        unsafe {
            let _ = SetThreadDesktop(self.old);
            let _ = CloseDesktop(self.current);
        }
    }
}
struct Guard(Arc<AtomicBool>);
impl Capture for Guard {
    fn stop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.stop();
    }
}
pub fn capture() -> anyhow::Result<CaptureStream> {
    anyhow::ensure!(enabled(), "secure desktop requires installed service agent");
    let (tx, frames) = tokio::sync::watch::channel(None);
    let stop = Arc::new(AtomicBool::new(false));
    let guard = Guard(stop.clone());
    let flag = stop.clone();
    let (ready, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = unsafe { run(tx, flag, ready) };
        if let Err(error) = result {
            tracing::warn!(%error,"secure desktop capture ended");
        }
    });
    rx.recv_timeout(std::time::Duration::from_secs(5))?
        .map_err(anyhow::Error::msg)?;
    Ok(CaptureStream {
        frames,
        guard: Box::new(guard),
    })
}
unsafe fn run(
    tx: tokio::sync::watch::Sender<Option<RawFrame>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> anyhow::Result<()> {
    unsafe {
        let setup = (|| -> anyhow::Result<_> {
            let scope = DesktopScope::enter()?;
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            for index in 0..16 {
                let Ok(adapter) = factory.EnumAdapters1(index) else {
                    break;
                };
                for output_index in 0..16 {
                    let Ok(output) = adapter.EnumOutputs(output_index) else {
                        break;
                    };
                    let desc = output.GetDesc()?;
                    if !desc.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    let mut device = None;
                    let mut context = None;
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )?;
                    let device = device.unwrap();
                    let context = context.unwrap();
                    let output: IDXGIOutput1 = output.cast()?;
                    if let Ok(duplication) = output.DuplicateOutput(&device) {
                        return Ok((scope, device, context, duplication));
                    }
                }
            }
            anyhow::bail!("no accessible secure-desktop output")
        })();
        let (_scope, device, context, duplication) = match setup {
            Ok(parts) => parts,
            Err(error) => {
                let _ = ready.send(Err(error.to_string()));
                return Err(error);
            }
        };
        let _ = ready.send(Ok(()));
        let mut staging = None::<ID3D11Texture2D>;
        let mut dimensions = (0, 0);
        while !stop.load(Ordering::Acquire) {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            if let Err(error) = duplication.AcquireNextFrame(16, &mut info, &mut resource) {
                if error.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    continue;
                }
                return Err(error.into());
            }
            let result = (|| -> anyhow::Result<()> {
                let texture: ID3D11Texture2D = resource.context("empty desktop texture")?.cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                if dimensions != (desc.Width, desc.Height) {
                    dimensions = (desc.Width, desc.Height);
                    desc.Usage = D3D11_USAGE_STAGING;
                    desc.BindFlags = 0;
                    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                    desc.MiscFlags = 0;
                    device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                }
                let staging = staging.as_ref().unwrap();
                context.CopyResource(staging, &texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let mut bgra = vec![0; desc.Width as usize * desc.Height as usize * 4];
                for row in 0..desc.Height as usize {
                    std::ptr::copy_nonoverlapping(
                        (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                        bgra.as_mut_ptr().add(row * desc.Width as usize * 4),
                        desc.Width as usize * 4,
                    );
                }
                context.Unmap(staging, 0);
                tx.send_replace(Some(RawFrame {
                    width: desc.Width,
                    height: desc.Height,
                    bgra,
                }));
                Ok(())
            })();
            let _ = duplication.ReleaseFrame();
            result?;
        }
        Ok(())
    }
}
use anyhow::Context;
