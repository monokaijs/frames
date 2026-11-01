//! Synthetic GPU surface roundtrip. No desktop capture or remote input.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use frames_core::{
        codec,
        media::{RawFrame, WindowsFrame},
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };
    use windows::{
        Win32::{
            Foundation::HMODULE,
            Graphics::{Direct3D::D3D_DRIVER_TYPE_HARDWARE, Direct3D11::*, Dxgi::Common::*},
        },
        core::Interface,
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let mut device = None;
        unsafe { D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT, None, D3D11_SDK_VERSION, Some(&mut device), None, None)?; }
        let device = device.unwrap();
        let adapter=unsafe { device.cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()?.GetAdapter()?.GetDesc()? };
        println!("GPU adapter: {}", String::from_utf16_lossy(&adapter.Description).trim_end_matches('\0'));
        let context = unsafe { device.GetImmediateContext()? };
        let protection: windows::Win32::Graphics::Direct3D10::ID3D10Multithread = context.cast()?;
        unsafe { let _ = protection.SetMultithreadProtected(true); }
        let capability = codec::probe().await.into_iter().find(|c| c.hardware && c.codec == codec::Codec::H264 && c.encoder.as_deref().is_some_and(|s| s.ends_with("_nvenc"))).ok_or_else(|| anyhow::anyhow!("no NVENC encoder on this reference machine"))?;
        let width:u32=std::env::args().nth(1).map(|s|s.parse()).transpose()?.unwrap_or(1920);let height:u32=std::env::args().nth(2).map(|s|s.parse()).transpose()?.unwrap_or(1080);
        anyhow::ensure!(width>0 && height>0 && width<=8192 && height<=8192,"invalid dimensions");
        let description=D3D11_TEXTURE2D_DESC { Width:width,Height:height,MipLevels:1,ArraySize:1,Format:DXGI_FORMAT_B8G8R8A8_UNORM,SampleDesc:DXGI_SAMPLE_DESC { Count:1,Quality:0 },Usage:D3D11_USAGE_DEFAULT,BindFlags:(D3D11_BIND_SHADER_RESOURCE|D3D11_BIND_RENDER_TARGET).0 as u32,..Default::default() };
        let make_frame = |green:bool| -> anyhow::Result<RawFrame> {
            let pixels=if green {[20u8,220,30,255]}else{[20u8,30,220,255]}.repeat((width*height) as usize);
            let input=D3D11_SUBRESOURCE_DATA { pSysMem:pixels.as_ptr().cast(),SysMemPitch:width*4,..Default::default() };let mut texture=None;
            unsafe{device.CreateTexture2D(&description,Some(&input),Some(&mut texture))?;}
            Ok(RawFrame { width,height,bgra:Default::default(),captured_at:Instant::now(),frame_number:0,media_generation:0,gpu:Some(Arc::new(WindowsFrame { texture:texture.unwrap(),subresource:0,matrix_709:true,full_range:true,lease:None })) })
        };
        let (capture,frames)=tokio::sync::watch::channel(Some(make_frame(false)?));
        let mut encoder=codec::encode(width,height,60,&capability,frames).await?;
        let mut decoder=codec::decode_for_presentation(codec::Codec::H264,width,height).await?;
        let mut times=Vec::new();let mut encoded_gpu=0;let mut decoded_gpu=0;
        for index in 0..41 {
            let started=Instant::now();let green=index%2==1;
            if index>0 { capture.send_replace(Some(make_frame(green)?)); }
            let packet=tokio::time::timeout(Duration::from_secs(3),encoder.frames.recv()).await?.ok_or_else(|| anyhow::anyhow!("encoder stopped"))?;
            encoded_gpu+=usize::from(packet.surface_encoded);
            decoder.input.send(packet).await?;
            tokio::time::timeout(Duration::from_secs(3),decoder.output.changed()).await??;
            let frame=decoder.output.borrow_and_update().clone().unwrap();
            let mut pixels=vec![0;width as usize*height as usize*4];
            if let Some(gpu)=&frame.gpu { decoded_gpu+=1;let mut converter=codec::GpuConverter::new(&gpu.texture,width,height)?;converter.readback(gpu,width,height,&mut pixels)?; }
            else { pixels.copy_from_slice(&frame.bgra); }
            let center=((height/2*width+width/2)*4)as usize;
            anyhow::ensure!(pixels[center+if green{1}else{2}]>150,"incorrect or stale GPU pixels at frame {index}: {:?}",&pixels[center..center+4]);
            if index>0 { times.push(started.elapsed().as_micros()as u64); }
            tokio::time::sleep(Duration::from_millis(35)).await;
        }
        times.sort_unstable();println!("{}",serde_json::json!({"width":width,"height":height,"samples":times.len(),"encoded_gpu":encoded_gpu,"decoded_gpu":decoded_gpu,"p50_ms":times[times.len()/2]as f64/1000.0,"p95_ms":times[times.len()*95/100]as f64/1000.0,"scope":"synthetic GPU encode/decode and verification readback; excludes capture, network, presentation and panel"}));
        anyhow::ensure!(encoded_gpu==41 && decoded_gpu==41,"GPU path fell back");
        encoder.shutdown().await;decoder.shutdown().await;Ok(())
    })
}
#[cfg(not(windows))]
fn main() {
    println!("Windows GPU probe; use NativeVideoTests on macOS");
}
