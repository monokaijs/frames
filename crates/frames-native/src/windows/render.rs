#![allow(unsafe_op_in_unsafe_fn)]
use frames_core::media::RawFrame;
use windows::{
    Win32::{
        Foundation::*,
        Graphics::{
            Direct3D::{Fxc::D3DCompile, *},
            Direct3D11::*,
            Dxgi::{Common::*, *},
        },
        UI::WindowsAndMessaging::GetClientRect,
    },
    core::*,
};
pub struct Renderer {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap: IDXGISwapChain,
    target: Option<ID3D11RenderTargetView>,
    size: (u32, u32),
    texture: Option<ID3D11Texture2D>,
    view: Option<ID3D11ShaderResourceView>,
    dimensions: (u32, u32),
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
}
impl Renderer {
    pub unsafe fn new(hwnd: HWND) -> anyhow::Result<Self> {
        unsafe {
            let mut device = None;
            let mut context = None;
            let mut swap = None;
            let desc = DXGI_SWAP_CHAIN_DESC {
                BufferDesc: DXGI_MODE_DESC {
                    Width: 1,
                    Height: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    ..Default::default()
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 1,
                OutputWindow: hwnd,
                Windowed: TRUE,
                // Compatibility presentation path for virtual-display adapters.
                SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
                ..Default::default()
            };
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut swap),
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.unwrap();
            let context = context.unwrap();
            let swap = swap.unwrap();
            let source=b"Texture2D picture:register(t0);SamplerState linearSampler:register(s0);struct V{float4 p:SV_Position;float2 uv:TEXCOORD0;};V vs(uint id:SV_VertexID){float2 p[4]={float2(-1,-1),float2(-1,1),float2(1,-1),float2(1,1)};V v;v.p=float4(p[id],0,1);v.uv=float2((p[id].x+1)/2,(1-p[id].y)/2);return v;}float4 ps(V v):SV_Target{return picture.Sample(linearSampler,v.uv);}";
            let compile =
                |entry: PCSTR, target: PCSTR| -> anyhow::Result<Vec<u8>> {
                    let mut code = None;
                    D3DCompile(
                        source.as_ptr().cast(),
                        source.len(),
                        PCSTR::null(),
                        None,
                        None,
                        entry,
                        target,
                        0,
                        0,
                        &mut code,
                        None,
                    )?;
                    let code = code.unwrap();
                    Ok(std::slice::from_raw_parts(
                        code.GetBufferPointer().cast(),
                        code.GetBufferSize(),
                    )
                    .to_vec())
                };
            let mut vertex = None;
            let mut pixel = None;
            device.CreateVertexShader(
                &compile(s!("vs"), s!("vs_5_0"))?,
                None,
                Some(&mut vertex),
            )?;
            device.CreatePixelShader(&compile(s!("ps"), s!("ps_5_0"))?, None, Some(&mut pixel))?;
            let mut sampler = None;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            Ok(Self {
                device,
                context,
                swap,
                target: None,
                size: (0, 0),
                texture: None,
                view: None,
                dimensions: (0, 0),
                vertex: vertex.unwrap(),
                pixel: pixel.unwrap(),
                sampler: sampler.unwrap(),
            })
        }
    }
    pub unsafe fn draw(
        &mut self,
        hwnd: HWND,
        frame: &RawFrame,
        content: RECT,
    ) -> anyhow::Result<()> {
        unsafe {
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect)?;
            let size = (rect.right.max(0) as u32, rect.bottom.max(0) as u32);
            if size.0 == 0 || size.1 == 0 {
                return Ok(());
            }
            if self.size != size {
                self.context.OMSetRenderTargets(None, None);
                self.target = None;
                self.swap.ResizeBuffers(
                    0,
                    size.0,
                    size.1,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )?;
                let buffer: ID3D11Texture2D = self.swap.GetBuffer(0)?;
                self.device
                    .CreateRenderTargetView(&buffer, None, Some(&mut self.target))?;
                self.size = size;
            }
            if self.dimensions != (frame.width, frame.height) {
                self.view = None;
                self.texture = None;
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: frame.width,
                    Height: frame.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    ..Default::default()
                };
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut self.texture))?;
                self.device.CreateShaderResourceView(
                    self.texture.as_ref().unwrap(),
                    None,
                    Some(&mut self.view),
                )?;
                self.dimensions = (frame.width, frame.height);
            }
            let texture = self.texture.as_ref().unwrap();
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(texture, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            for row in 0..frame.height as usize {
                std::ptr::copy_nonoverlapping(
                    frame.bgra.as_ptr().add(row * frame.width as usize * 4),
                    (mapped.pData as *mut u8).add(row * mapped.RowPitch as usize),
                    frame.width as usize * 4,
                );
            }
            self.context.Unmap(texture, 0);
            self.context
                .ClearRenderTargetView(self.target.as_ref().unwrap(), &[0., 0., 0., 1.]);
            self.context
                .OMSetRenderTargets(Some(std::slice::from_ref(&self.target)), None);
            self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: content.left as f32,
                TopLeftY: content.top as f32,
                Width: (content.right - content.left) as f32,
                Height: (content.bottom - content.top) as f32,
                MinDepth: 0.,
                MaxDepth: 1.,
            }]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLESTRIP);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShader(&self.pixel, None);
            self.context
                .PSSetShaderResources(0, Some(std::slice::from_ref(&self.view)));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.Draw(4, 0);
            self.swap.Present(0, DXGI_PRESENT(0)).ok()?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "system" fn fixture(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe { windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, message, w, l) }
    }
    #[test]
    #[ignore = "requires an interactive Windows desktop and D3D11; opens a temporary renderer fixture"]
    fn gpu_presentation_is_visible_to_direct_window_capture() {
        use windows::Win32::{
            Graphics::Gdi::UpdateWindow, System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::*,
        };
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let worker = std::thread::spawn(move || unsafe {
            let instance = GetModuleHandleW(None).unwrap();
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(fixture),
                hInstance: instance.into(),
                lpszClassName: w!("FramesGpuFixture"),
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                w!("FramesGpuFixture"),
                w!("Frames GPU presentation fixture"),
                WS_OVERLAPPEDWINDOW,
                120,
                120,
                420,
                320,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .unwrap();
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = UpdateWindow(hwnd);
            let mut renderer = Renderer::new(hwnd).unwrap();
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect).unwrap();
            let mut bgra = vec![0; 128 * 128 * 4];
            for pixel in bgra.chunks_mut(4) {
                pixel.copy_from_slice(&[230, 40, 20, 255]);
            }
            renderer
                .draw(
                    hwnd,
                    &RawFrame {
                        width: 128,
                        height: 128,
                        bgra,
                    },
                    rect,
                )
                .unwrap();
            ready_tx.send(hwnd.0 as u64).unwrap();
            let mut message = MSG::default();
            while stop_rx.try_recv().is_err() {
                while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            drop(renderer);
            let _ = DestroyWindow(hwnd);
        });
        let handle = ready_rx.recv().unwrap();
        let mut capture = super::super::capture::start(handle).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut correct = false;
        while std::time::Instant::now() < end {
            if let Some(frame) = capture.frames.borrow().as_ref() {
                let index = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
                let pixel = &frame.bgra[index..index + 4];
                if pixel[0] > 180 && pixel[2] < 80 {
                    correct = true;
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        capture.guard.stop();
        let _ = stop_tx.send(());
        worker.join().unwrap();
        assert!(correct, "D3D11 picture was not captured correctly");
    }
}
