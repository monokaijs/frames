//! D3D11 surfaces shared by Windows capture, encoders, decoders, and the
//! presenter, and device creation on a chosen adapter.

use frames_media::api::PlatformSurface;
use std::any::Any;
use std::sync::Arc;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_DESC1, IDXGIAdapter1, IDXGIFactory1,
};
use windows::core::Interface;

/// PCI vendor id of NVIDIA adapters.
pub const NVIDIA: u32 = 0x10de;

/// A D3D11 texture (or one array slice of it) carried through the pipeline.
/// `keepalive` holds whatever owns the texture's storage (a decoder's surface
/// lease, a capture pool slot) until every user dropped the surface.
pub struct D3D11Surface {
    pub texture: ID3D11Texture2D,
    pub subresource: u32,
    pub width: u32,
    pub height: u32,
    /// BT.709 / full-range hints for YUV textures (decoder output).
    pub matrix_709: bool,
    pub full_range: bool,
    pub keepalive: Option<Arc<dyn Any + Send + Sync>>,
}

// COM pointers to free-threaded D3D11 objects; the device is created with
// multithread protection.
unsafe impl Send for D3D11Surface {}
unsafe impl Sync for D3D11Surface {}

impl std::fmt::Debug for D3D11Surface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("D3D11Surface")
            .field("texture", &self.texture.as_raw())
            .field("subresource", &self.subresource)
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

impl PlatformSurface for D3D11Surface {
    fn width(&self) -> u32 {
        self.width
    }
    fn height(&self) -> u32 {
        self.height
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl D3D11Surface {
    pub fn device(&self) -> windows::core::Result<ID3D11Device> {
        unsafe { self.texture.GetDevice() }
    }
}

/// An adapter found by [`adapters`].
pub struct Adapter {
    pub adapter: IDXGIAdapter1,
    pub vendor: u32,
    pub name: String,
    pub dedicated_video_memory: usize,
}

/// Every hardware adapter, in DXGI order (index 0 is the default).
pub fn adapters() -> windows::core::Result<Vec<Adapter>> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut out = Vec::new();
    for index in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(index) }) else {
            break;
        };
        let desc: DXGI_ADAPTER_DESC1 = unsafe { adapter.GetDesc1()? };
        // Skip the software (WARP / Basic Render) adapter.
        if desc.Flags & 2 != 0 {
            continue;
        }
        let end = desc
            .Description
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(desc.Description.len());
        out.push(Adapter {
            adapter,
            vendor: desc.VendorId,
            name: String::from_utf16_lossy(&desc.Description[..end]),
            dedicated_video_memory: desc.DedicatedVideoMemory,
        });
    }
    Ok(out)
}

/// Creates a multithread-protected device on `adapter` (BGRA and video
/// support), or on the default adapter.
pub fn create_device(
    adapter: Option<&IDXGIAdapter1>,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    unsafe {
        D3D11CreateDevice(
            adapter
                .map(|a| a.cast::<windows::Win32::Graphics::Dxgi::IDXGIAdapter>())
                .transpose()?
                .as_ref(),
            if adapter.is_some() {
                D3D_DRIVER_TYPE_UNKNOWN
            } else {
                windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE
            },
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }
    let (device, context) = (device.expect("created"), context.expect("created"));
    let threading: windows::Win32::Graphics::Direct3D11::ID3D11Multithread = context.cast()?;
    unsafe {
        let _ = threading.SetMultithreadProtected(true);
    }
    Ok((device, context))
}

/// A device on the first NVIDIA adapter: NVENC only opens on its own GPU's
/// device, and the default adapter can be an integrated GPU.
pub fn nvidia_device() -> anyhow::Result<(ID3D11Device, ID3D11DeviceContext, String)> {
    let adapter = adapters()?
        .into_iter()
        .find(|a| a.vendor == NVIDIA)
        .ok_or_else(|| anyhow::anyhow!("no NVIDIA adapter"))?;
    let (device, context) = create_device(Some(&adapter.adapter))?;
    Ok((device, context, adapter.name))
}
