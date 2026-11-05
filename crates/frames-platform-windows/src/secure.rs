//! The secure desktop (UAC prompts, the sign-in and lock screens) for the
//! Windows service agent, ported from v1: only a LocalSystem process on the
//! input desktop can see or drive it. While it is up, the host shows it
//! through DXGI Desktop Duplication instead of Windows Graphics Capture,
//! and sends input from a thread attached to it.
//!
//! Capture and input each run on their own thread, switched to the input
//! desktop (a thread with windows cannot switch desktops).

use anyhow::{Context, Result};
use frames_input::{Event, Motion, Pointer};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::StationsAndDesktops::*;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::Interface;

/// Whether this process is the installed service's privileged agent.
pub fn privileged() -> bool {
    std::env::var_os("FRAMES_PRIVILEGED_AGENT").is_some()
}

/// Whether the input desktop is a secure one (not the user's "Default").
pub fn secure_desktop_active() -> bool {
    unsafe {
        let Ok(desktop) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_READOBJECTS)
        else {
            // Not readable from here: the Winlogon desktop is.
            return true;
        };
        let mut name = [0u16; 256];
        let read = GetUserObjectInformationW(
            windows::Win32::Foundation::HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            512,
            None,
        );
        let _ = CloseDesktop(desktop);
        let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        read.is_ok() && String::from_utf16_lossy(&name[..end]) != "Default"
    }
}

/// This thread on the input desktop, until dropped.
struct DesktopScope {
    old: HDESK,
    current: HDESK,
}

impl DesktopScope {
    fn enter() -> Result<Self> {
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

/// The newest secure-desktop picture: packed BGRA, width, height, and a
/// number that changes with each one.
pub type Picture = (Arc<Vec<u8>>, u32, u32, u64);

/// Desktop Duplication of the input desktop's first output, on its own
/// thread; stops when dropped.
pub struct SecureCapture {
    latest: Arc<Mutex<Option<Picture>>>,
    stop: Arc<AtomicBool>,
}

impl SecureCapture {
    pub fn start() -> Result<Self> {
        let latest = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready) = std::sync::mpsc::sync_channel(1);
        {
            let (latest, stop) = (latest.clone(), stop.clone());
            std::thread::spawn(move || {
                if let Err(error) = unsafe { duplicate(latest, stop, ready_tx) } {
                    eprintln!("secure desktop capture ended: {error:#}");
                }
            });
        }
        ready
            .recv_timeout(Duration::from_secs(5))
            .context("secure desktop capture did not start")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self { latest, stop })
    }

    pub fn latest(&self) -> Option<Picture> {
        self.latest.lock().unwrap().clone()
    }
}

impl Drop for SecureCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

unsafe fn duplicate(
    latest: Arc<Mutex<Option<Picture>>>,
    stop: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<()> {
    unsafe {
        let setup = (|| -> Result<_> {
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
                    if !output.GetDesc()?.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    let (mut device, mut context) = (None, None);
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
                    let (device, context) =
                        (device.context("no device")?, context.context("no context")?);
                    let output: IDXGIOutput1 = output.cast()?;
                    if let Ok(duplication) = output.DuplicateOutput(&device) {
                        return Ok((scope, device, context, duplication));
                    }
                }
            }
            anyhow::bail!("no secure desktop output can be duplicated")
        })();
        let (_scope, device, context, duplication) = match setup {
            Ok(parts) => parts,
            Err(error) => {
                let _ = ready.send(Err(format!("{error:#}")));
                return Err(error);
            }
        };
        let _ = ready.send(Ok(()));
        let mut staging = None::<ID3D11Texture2D>;
        let mut size = (0, 0);
        let mut number = 0u64;
        while !stop.load(Ordering::Acquire) {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            if let Err(error) = duplication.AcquireNextFrame(16, &mut info, &mut resource) {
                if error.code() == DXGI_ERROR_WAIT_TIMEOUT {
                    continue;
                }
                return Err(error.into());
            }
            let copied = (|| -> Result<()> {
                let texture: ID3D11Texture2D = resource.context("empty desktop picture")?.cast()?;
                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                if size != (desc.Width, desc.Height) {
                    size = (desc.Width, desc.Height);
                    desc.Usage = D3D11_USAGE_STAGING;
                    desc.BindFlags = 0;
                    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                    desc.MiscFlags = 0;
                    staging = None;
                    device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                }
                let staging = staging.as_ref().context("no staging texture")?;
                context.CopyResource(staging, &texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let (width, height) = (desc.Width as usize, desc.Height as usize);
                let mut bgra = vec![0u8; width * height * 4];
                for row in 0..height {
                    std::ptr::copy_nonoverlapping(
                        (mapped.pData as *const u8).add(row * mapped.RowPitch as usize),
                        bgra.as_mut_ptr().add(row * width * 4),
                        width * 4,
                    );
                }
                context.Unmap(staging, 0);
                number += 1;
                *latest.lock().unwrap() = Some((Arc::new(bgra), desc.Width, desc.Height, number));
                Ok(())
            })();
            let _ = duplication.ReleaseFrame();
            copied?;
        }
        Ok(())
    }
}

/// Input to the secure desktop from a thread attached to it; stops when
/// dropped.
pub struct SecureInput {
    commands: std::sync::mpsc::Sender<Command>,
}

enum Command {
    Pointer(Pointer),
    Motion(Motion),
    Event(Event),
}

impl SecureInput {
    pub fn start() -> Result<Self> {
        let (commands, receive) = std::sync::mpsc::channel::<Command>();
        let (ready_tx, ready) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let scope = DesktopScope::enter();
            let failed = scope.as_ref().err().map(|e| format!("{e:#}"));
            let _ = ready_tx.send(failed.map_or(Ok(()), Err));
            let Ok(_scope) = scope else { return };
            let screen = unsafe {
                use windows::Win32::UI::WindowsAndMessaging::{
                    GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
                };
                windows::Win32::Foundation::RECT {
                    left: 0,
                    top: 0,
                    right: GetSystemMetrics(SM_CXSCREEN),
                    bottom: GetSystemMetrics(SM_CYSCREEN),
                }
            };
            let mut injector =
                crate::input::InputInjector::new(crate::input::InputTarget::Desktop(screen));
            while let Ok(command) = receive.recv() {
                match command {
                    Command::Pointer(pointer) => {
                        injector.pointer(&pointer);
                    }
                    Command::Motion(motion) => {
                        injector.motion(&motion);
                    }
                    Command::Event(event) => injector.event(event),
                }
            }
            injector.release_all();
        });
        ready
            .recv_timeout(Duration::from_secs(5))
            .context("secure desktop input did not start")?
            .map_err(anyhow::Error::msg)?;
        Ok(Self { commands })
    }

    pub fn pointer(&self, pointer: Pointer) {
        let _ = self.commands.send(Command::Pointer(pointer));
    }

    pub fn motion(&self, motion: Motion) {
        let _ = self.commands.send(Command::Motion(motion));
    }

    pub fn event(&self, event: Event) {
        let _ = self.commands.send(Command::Event(event));
    }
}

/// A packed BGRA picture as a texture on `device` (for the encoder).
pub fn upload(
    device: &ID3D11Device,
    bgra: &[u8],
    width: u32,
    height: u32,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: bgra.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture))? };
    texture.context("no texture")
}
