//! Flip-model presenter.
//!
//! Decoded surfaces (NV12 texture-array slices from D3D11VA, or BGRA) are
//! scaled and colour-converted by the D3D11 video processor straight into the
//! swap chain's back buffer, with no intermediate texture or shader pass.
//! The swap chain is created on the surface's own device (the decoder's), and
//! presents with `DXGI_PRESENT_ALLOW_TEARING` in [`PresentMode::Immediate`]
//! when the system supports it.

use frames_codec::d3d11::{D3D11Surface, create_device};
use frames_media::api::{
    DecodedFrame, MediaClock, MediaError, PresentCaps, PresentMode, PresentStamp, Presenter,
    Surface,
};
use std::mem::ManuallyDrop;
use std::sync::Arc;
use windows::Win32::Foundation::{HANDLE, HWND, RECT, TRUE};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::BOOL;
use windows::core::Interface;

fn backend(error: windows::core::Error) -> MediaError {
    MediaError::Backend(error.to_string())
}

struct Processor {
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: (u32, u32),
    output: (u32, u32),
}

struct State {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap: IDXGISwapChain2,
    waitable: HANDLE,
    tearing: bool,
    size: (u32, u32),
    video: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    processor: Option<Processor>,
    upload: Option<(ID3D11Texture2D, (u32, u32))>,
}

pub struct D3D11Presenter {
    hwnd: HWND,
    clock: Arc<dyn MediaClock>,
    state: Option<State>,
    refresh_hz: u32,
    tearing: bool,
}

// The swap chain is driven from one thread at a time.
unsafe impl Send for D3D11Presenter {}

fn tearing_supported(factory: &IDXGIFactory2) -> bool {
    let Ok(factory) = factory.cast::<IDXGIFactory5>() else {
        return false;
    };
    let mut allowed = BOOL(0);
    unsafe {
        factory
            .CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                &mut allowed as *mut BOOL as *mut _,
                std::mem::size_of::<BOOL>() as u32,
            )
            .is_ok()
            && allowed.as_bool()
    }
}

fn display_refresh_hz() -> u32 {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    if unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool() {
        mode.dmDisplayFrequency.max(1)
    } else {
        60
    }
}

impl D3D11Presenter {
    pub fn new(hwnd: HWND, clock: Arc<dyn MediaClock>) -> Self {
        Self {
            hwnd,
            clock,
            state: None,
            refresh_hz: display_refresh_hz(),
            tearing: false,
        }
    }

    fn client_size(&self) -> (u32, u32) {
        let mut rect = RECT::default();
        let _ = unsafe { GetClientRect(self.hwnd, &mut rect) };
        (
            (rect.right - rect.left).max(1) as u32,
            (rect.bottom - rect.top).max(1) as u32,
        )
    }

    fn state_for(&mut self, device: ID3D11Device) -> Result<&mut State, MediaError> {
        let matches = self
            .state
            .as_ref()
            .is_some_and(|s| s.device.as_raw() == device.as_raw());
        if !matches {
            self.state = None;
            let context = unsafe { device.GetImmediateContext() }.map_err(backend)?;
            let adapter = unsafe { device.cast::<IDXGIDevice>().and_then(|d| d.GetAdapter()) }
                .map_err(backend)?;
            let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }.map_err(backend)?;
            let tearing = tearing_supported(&factory);
            let size = self.client_size();
            let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
            if tearing {
                flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
            }
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: size.0,
                Height: size.1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                Flags: flags,
                ..Default::default()
            };
            let swap: IDXGISwapChain2 =
                unsafe { factory.CreateSwapChainForHwnd(&device, self.hwnd, &desc, None, None) }
                    .and_then(|s| s.cast())
                    .map_err(backend)?;
            unsafe { swap.SetMaximumFrameLatency(1) }.map_err(backend)?;
            let waitable = unsafe { swap.GetFrameLatencyWaitableObject() };
            let video: ID3D11VideoDevice = device.cast().map_err(backend)?;
            let video_context: ID3D11VideoContext = context.cast().map_err(backend)?;
            self.tearing = tearing;
            self.state = Some(State {
                device,
                context,
                swap,
                waitable,
                tearing,
                size,
                video,
                video_context,
                processor: None,
                upload: None,
            });
        }
        Ok(self.state.as_mut().expect("created"))
    }

    fn device_for(&self, surface: &Surface) -> Result<ID3D11Device, MediaError> {
        match surface {
            Surface::Platform(platform) => platform
                .as_any()
                .downcast_ref::<D3D11Surface>()
                .ok_or(MediaError::Unsupported("presenter takes D3D11 surfaces"))?
                .device()
                .map_err(backend),
            Surface::Cpu(_) => match &self.state {
                Some(state) => Ok(state.device.clone()),
                None => create_device(None).map(|(d, _)| d).map_err(backend),
            },
        }
    }
}

impl State {
    fn processor(
        &mut self,
        input: (u32, u32),
        output: (u32, u32),
    ) -> Result<&Processor, MediaError> {
        if self
            .processor
            .as_ref()
            .is_none_or(|p| p.input != input || p.output != output)
        {
            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputWidth: output.0,
                OutputHeight: output.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
                ..Default::default()
            };
            let enumerator =
                unsafe { self.video.CreateVideoProcessorEnumerator(&desc) }.map_err(backend)?;
            let processor =
                unsafe { self.video.CreateVideoProcessor(&enumerator, 0) }.map_err(backend)?;
            unsafe {
                // No automatic processing: no denoise, no frame-rate tricks.
                self.video_context
                    .VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
                self.video_context.VideoProcessorSetStreamFrameFormat(
                    &processor,
                    0,
                    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                );
            }
            self.processor = Some(Processor {
                enumerator,
                processor,
                input,
                output,
            });
        }
        Ok(self.processor.as_ref().expect("created"))
    }

    fn upload(
        &mut self,
        image: &frames_media::api::CpuImage,
    ) -> Result<ID3D11Texture2D, MediaError> {
        let size = (image.width, image.height);
        if self.upload.as_ref().is_none_or(|(_, s)| *s != size) {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: size.0,
                Height: size.1,
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
            let mut texture = None;
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut texture)) }
                .map_err(backend)?;
            self.upload = Some((texture.expect("created"), size));
        }
        let texture = self.upload.as_ref().expect("created").0.clone();
        let row = size.0 as usize * 4;
        let stride = (image.stride as usize).max(row);
        if image.data.len() < stride * (size.1 as usize - 1) + row {
            return Err(MediaError::Backend(
                "CPU image shorter than its size".into(),
            ));
        }
        unsafe {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&texture, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))
                .map_err(backend)?;
            for y in 0..size.1 as usize {
                std::ptr::copy_nonoverlapping(
                    image.data.as_ptr().add(y * stride),
                    (mapped.pData as *mut u8).add(y * mapped.RowPitch as usize),
                    row,
                );
            }
            self.context.Unmap(&texture, 0);
        }
        Ok(texture)
    }
}

/// Largest rectangle with the source's aspect ratio centred in `target`.
fn fit(source: (u32, u32), target: (u32, u32)) -> RECT {
    let (sw, sh) = (u64::from(source.0.max(1)), u64::from(source.1.max(1)));
    let (tw, th) = (u64::from(target.0), u64::from(target.1));
    let (w, h) = if tw * sh <= th * sw {
        (tw, tw * sh / sw)
    } else {
        (th * sw / sh, th)
    };
    let left = ((tw - w) / 2) as i32;
    let top = ((th - h) / 2) as i32;
    RECT {
        left,
        top,
        right: left + w as i32,
        bottom: top + h as i32,
    }
}

impl Presenter for D3D11Presenter {
    fn caps(&self) -> PresentCaps {
        PresentCaps {
            tearing: self.tearing,
            variable_refresh: self.tearing,
            refresh_hz: self.refresh_hz,
        }
    }

    fn wait_ready(&mut self, deadline_us: u64) -> bool {
        let Some(state) = &self.state else {
            return true;
        };
        let now = self.clock.now_us();
        let ms = deadline_us.saturating_sub(now).div_ceil(1000).min(100) as u32;
        unsafe { WaitForSingleObject(state.waitable, ms) }.0 == 0
    }

    fn present(
        &mut self,
        frame: DecodedFrame,
        mode: PresentMode,
    ) -> Result<PresentStamp, MediaError> {
        let device = self.device_for(&frame.surface)?;
        let size = self.client_size();
        let state = self.state_for(device)?;
        if state.size != size {
            state.processor = None;
            let mut flags = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0;
            if state.tearing {
                flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0;
            }
            unsafe {
                state.swap.ResizeBuffers(
                    0,
                    size.0,
                    size.1,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(flags),
                )
            }
            .map_err(backend)?;
            state.size = size;
        }
        let (texture, subresource, input, matrix_709, full_range) = match &frame.surface {
            Surface::Platform(platform) => {
                let s = platform
                    .as_any()
                    .downcast_ref::<D3D11Surface>()
                    .expect("checked in device_for");
                (
                    s.texture.clone(),
                    s.subresource,
                    (s.width, s.height),
                    s.matrix_709,
                    s.full_range,
                )
            }
            Surface::Cpu(image) => (
                state.upload(image)?,
                0,
                (image.width, image.height),
                true,
                true,
            ),
        };
        let processor = state.processor(input, size)?;
        let (enumerator, video_processor) =
            (processor.enumerator.clone(), processor.processor.clone());
        unsafe {
            let back: ID3D11Texture2D = state.swap.GetBuffer(0).map_err(backend)?;
            let mut output = None;
            state
                .video
                .CreateVideoProcessorOutputView(
                    &back,
                    &enumerator,
                    &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                        ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                        },
                    },
                    Some(&mut output),
                )
                .map_err(backend)?;
            let mut input_view = None;
            state
                .video
                .CreateVideoProcessorInputView(
                    &texture,
                    &enumerator,
                    &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                        FourCC: 0,
                        ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                        Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                            Texture2D: D3D11_TEX2D_VPIV {
                                MipSlice: 0,
                                ArraySlice: subresource,
                            },
                        },
                    },
                    Some(&mut input_view),
                )
                .map_err(backend)?;
            // YCbCr matrix bit 2, nominal range bits 4-5 (1 = studio, 2 = full).
            let colour = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: (u32::from(matrix_709) << 2) | (if full_range { 2 } else { 1 } << 4),
            };
            let target = fit(input, size);
            let vc = &state.video_context;
            vc.VideoProcessorSetStreamColorSpace(&video_processor, 0, &colour);
            vc.VideoProcessorSetOutputColorSpace(
                &video_processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE { _bitfield: 0 },
            );
            vc.VideoProcessorSetStreamDestRect(&video_processor, 0, true, Some(&target));
            vc.VideoProcessorSetOutputTargetRect(&video_processor, true, None);
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: TRUE,
                pInputSurface: ManuallyDrop::new(input_view),
                ..Default::default()
            };
            let result = vc.VideoProcessorBlt(
                &video_processor,
                output.as_ref().expect("created"),
                0,
                std::slice::from_ref(&stream),
            );
            ManuallyDrop::drop(&mut stream.pInputSurface);
            result.map_err(backend)?;
            let (interval, flags) = match mode {
                PresentMode::Immediate if state.tearing => (0, DXGI_PRESENT_ALLOW_TEARING),
                PresentMode::Immediate => (0, DXGI_PRESENT(0)),
                PresentMode::Vsync => (1, DXGI_PRESENT(0)),
            };
            state.swap.Present(interval, flags).ok().map_err(backend)?;
        }
        Ok(PresentStamp {
            frame: frame.frame,
            presented_us: self.clock.now_us(),
        })
    }
}
