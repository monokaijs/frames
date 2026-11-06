//! Windows host: WGC capture → NVENC → v2 host engine → frames-net.

use crate::{Options, StreamInfo, send_info, win};
use anyhow::{Context, Result, bail};
use frames_codec::d3d11::{
    AMD, INTEL, NVIDIA, adapters, create_device, nvidia_device, vendor_device,
};
use frames_codec::ffmpeg::{FfmpegEncoder, encoder_name};
use frames_codec::nvenc::NvencEncoder;
use frames_input::cursor::CursorMessage;
use frames_input::{Motion, Pointer};
use frames_media::api::{
    CaptureConfig, CaptureEvent, CaptureSource, EncodeParams, EncoderControl, Gop, MediaClock,
    MediaTransport, PathKind, VideoEncoder, VideoFrame,
};
use frames_media::clock::SystemClock;
use frames_media::host::{HostConfig, HostSession};
use frames_media::rate::RateConfig;
use frames_platform_windows::capture::WgcCapture;
use frames_platform_windows::input::{InputInjector, InputTarget};
use frames_platform_windows::system::{
    GpuClockHold, KeepAwake, Mmcss, TimerResolution, raise_gpu_priority,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub async fn run(session: frames_net::Session, o: Options) -> Result<()> {
    let handle = tokio::runtime::Handle::current();
    // Input, clipboard, and files arrive on the viewer's tagged streams.
    let services = crate::services::HostServices::start(&session, &o);
    tokio::task::spawn_blocking(move || stream(&handle, &session, &o, &services)).await?
}

fn primary_monitor_rect() -> windows::Win32::Foundation::RECT {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    windows::Win32::Foundation::RECT {
        left: 0,
        top: 0,
        right: unsafe { GetSystemMetrics(SM_CXSCREEN) },
        bottom: unsafe { GetSystemMetrics(SM_CYSCREEN) },
    }
}

fn first_frame(capture: &mut WgcCapture, clock: &SystemClock) -> Result<VideoFrame> {
    let give_up = clock.now_us() + 5_000_000;
    while clock.now_us() < give_up {
        win::pump();
        match capture.next(clock.now_us() + 20_000) {
            Ok(CaptureEvent::Frame(frame)) => return Ok(frame),
            Ok(CaptureEvent::Lost) => bail!("the capture source went away"),
            Ok(_) => {}
            Err(error) => bail!("capture failed: {error}"),
        }
    }
    bail!("no frame captured within 5 s")
}

/// The secure desktop, for the service agent
/// (`frames_platform_windows::secure`).
struct Secure {
    privileged: bool,
    next_check: Instant,
    capture: Option<frames_platform_windows::secure::SecureCapture>,
    input: Option<frames_platform_windows::secure::SecureInput>,
    last: u64,
    id: u64,
}

impl Secure {
    fn new() -> Self {
        Self {
            privileged: frames_platform_windows::secure::privileged(),
            next_check: Instant::now(),
            capture: None,
            input: None,
            last: 0,
            id: 1 << 48,
        }
    }

    /// Starts or stops following the secure desktop (four times a second).
    fn check(&mut self) {
        self.next_check = Instant::now() + Duration::from_millis(250);
        let active = frames_platform_windows::secure::secure_desktop_active();
        if active && self.capture.is_none() {
            match frames_platform_windows::secure::SecureCapture::start() {
                Ok(capture) => {
                    println!("showing the secure desktop");
                    self.capture = Some(capture);
                    self.input = frames_platform_windows::secure::SecureInput::start()
                        .map_err(|error| println!("secure desktop input unavailable: {error:#}"))
                        .ok();
                }
                Err(error) => println!("secure desktop unavailable: {error:#}"),
            }
        } else if !active && self.capture.is_some() {
            println!("back to the user's desktop");
            self.capture = None;
            self.input = None;
        }
    }

    /// The newest secure-desktop picture, uploaded for the encoder.
    fn frame(
        &mut self,
        device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
        clock: &SystemClock,
    ) -> Option<VideoFrame> {
        let (bgra, width, height, number) = self.capture.as_ref()?.latest()?;
        if number == self.last {
            std::thread::sleep(Duration::from_millis(2));
            return None;
        }
        self.last = number;
        let texture = frames_platform_windows::secure::upload(device, &bgra, width, height).ok()?;
        self.id += 1;
        Some(VideoFrame {
            id: self.id,
            captured_us: clock.now_us(),
            surface: frames_media::api::Surface::Platform(Arc::new(
                frames_codec::d3d11::D3D11Surface {
                    texture,
                    subresource: 0,
                    width: width & !1,
                    height: height & !1,
                    matrix_709: true,
                    full_range: true,
                    keepalive: None,
                },
            )),
            dirty: None,
        })
    }
}

/// One picture of the primary monitor as packed BGRA (for device card
/// previews), read back from the GPU through a staging texture.
pub fn screenshot() -> Result<(Vec<u8>, u32, u32)> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_STAGING, ID3D11Texture2D,
    };
    win::dpi_aware();
    let (device, context, _) = backend_device("mf")?;
    let clock = SystemClock::new();
    let shared: Arc<dyn MediaClock> = Arc::new(clock);
    let mut capture = WgcCapture::primary_monitor(&device, shared).context(CaptureFailed)?;
    capture.configure(CaptureConfig {
        fps: 10,
        separate_cursor: true,
    })?;
    let frame = first_frame(&mut capture, &clock)?;
    let frames_media::api::Surface::Platform(surface) = &frame.surface else {
        bail!("not a GPU picture");
    };
    let surface = surface
        .as_any()
        .downcast_ref::<frames_codec::d3d11::D3D11Surface>()
        .context("not a D3D11 picture")?;
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        surface.texture.GetDesc(&mut desc);
        desc.Width = surface.width;
        desc.Height = surface.height;
        desc.MipLevels = 1;
        desc.ArraySize = 1;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.context("no staging texture")?;
        let region = windows::Win32::Graphics::Direct3D11::D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: surface.width,
            bottom: surface.height,
            back: 1,
        };
        context.CopySubresourceRegion(
            &staging,
            0,
            0,
            0,
            0,
            &surface.texture,
            surface.subresource,
            Some(&region),
        );
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let (width, height) = (surface.width as usize, surface.height as usize);
        let mut bgra = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            let start = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
            bgra.extend_from_slice(std::slice::from_raw_parts(start, width * 4));
        }
        context.Unmap(&staging, 0);
        Ok((bgra, surface.width, surface.height))
    }
}

/// Encoders to try, best first: `--encoder` alone when given; otherwise
/// each hardware encoder whose GPU is present (discrete first), then Media
/// Foundation and software AV1, so a missing or outdated driver (NVENC
/// needs API 12.0, driver 522+) falls back instead of failing.
/// `--prefer-encoder` moves one to the front of that order.
pub fn backends(o: &Options) -> Vec<&'static str> {
    let named = |name: &str| match name {
        "nvenc" => "nvenc",
        "amf" => "amf",
        "qsv" => "qsv",
        "mf" => "mf",
        _ => "software",
    };
    if o.encoder_explicit {
        return vec![named(&o.encoder)];
    }
    let vendors: Vec<u32> = adapters()
        .map(|list| list.iter().map(|a| a.vendor).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for (vendor, backend) in [(NVIDIA, "nvenc"), (AMD, "amf"), (INTEL, "qsv")] {
        if vendors.contains(&vendor) {
            out.push(backend);
        }
    }
    out.extend(["mf", "software"]);
    if let Some(preferred) = &o.prefer_encoder {
        let preferred = named(preferred);
        out.retain(|backend| *backend != preferred);
        out.insert(0, preferred);
    }
    out
}

/// A device, its immediate context, and the adapter's name.
type Device = (
    windows::Win32::Graphics::Direct3D11::ID3D11Device,
    windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    String,
);

/// The device an encoder needs: hardware encoders only open on their own
/// GPU; Media Foundation and software use the default adapter.
fn backend_device(backend: &str) -> Result<Device> {
    match backend {
        "nvenc" => nvidia_device(),
        "amf" => vendor_device(AMD),
        "qsv" => vendor_device(INTEL),
        _ => {
            let (device, context) = create_device(None)?;
            let name = adapters()?
                .into_iter()
                .next()
                .map_or_else(|| "default adapter".into(), |a| a.name);
            Ok((device, context, name))
        }
    }
}

/// The codecs worth trying on `backend`, best first (a test encode decides).
fn backend_codecs(backend: &str) -> &'static [crate::Codec] {
    use crate::Codec::{Av1, H264, Hevc};
    match backend {
        // This NVENC path encodes H.264 and HEVC.
        "nvenc" | "mf" => &[Hevc, H264],
        "amf" | "qsv" => &[Hevc, Av1, H264],
        _ => &[Av1],
    }
}

fn make_encoder(
    backend: &str,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    codec: crate::Codec,
    params: EncodeParams,
    clock: Arc<dyn MediaClock>,
) -> Result<Box<dyn VideoEncoder>> {
    if backend == "nvenc" {
        return Ok(Box::new(NvencEncoder::new(
            device,
            codec.into(),
            params,
            clock,
        )?));
    }
    let name = encoder_name(codec.into(), backend)
        .with_context(|| format!("no {backend} encoder for {codec:?}"))?;
    Ok(Box::new(FfmpegEncoder::new(
        &name,
        codec.into(),
        params,
        clock,
    )?))
}

fn encode_params(o: &Options, width: u32, height: u32) -> EncodeParams {
    EncodeParams {
        width,
        height,
        fps: o.fps,
        bitrate_bps: o.start_mbps() * 1_000_000,
        vbv_frames: 1.0,
        gop: Gop::Infinite,
        intra_refresh_frames: Some(6),
        slices: 4,
    }
}

/// An encoder that works here, on the device its capture runs on.
struct Opened {
    backend: &'static str,
    device: windows::Win32::Graphics::Direct3D11::ID3D11Device,
    _context: windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
    adapter: String,
    capture: WgcCapture,
    first: VideoFrame,
    /// Codecs that passed a test encode of the first frame, best first.
    codecs: Vec<crate::Codec>,
}

/// Opens `backend`'s device and capture, and keeps the codecs that encode
/// the first frame (FFmpeg encoders only open on a real frame, so nothing
/// short of encoding one proves an encoder works).
/// The codecs `backend` encodes `frame` with, best first; an error with
/// every codec's reason when none does.
fn working_codecs(
    backend: &str,
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    frame: &VideoFrame,
    o: &Options,
    shared: &Arc<dyn MediaClock>,
) -> Result<Vec<crate::Codec>> {
    if backend == "nvenc" {
        // One reason for every codec: an old driver refuses them all.
        frames_codec::nvenc::available()?;
    }
    let (width, height) = frame.surface.size();
    let mut codecs = Vec::new();
    let mut failures = Vec::new();
    for &codec in backend_codecs(backend) {
        let params = encode_params(o, width, height);
        let tried =
            make_encoder(backend, device, codec, params, shared.clone()).and_then(|mut encoder| {
                encoder.encode(frame, &mut |_| {})?;
                // Rate control changes the bitrate within seconds, and some
                // encoders fail only then: change it once here.
                encoder.control(EncoderControl::Bitrate(params.bitrate_bps / 10 * 7))?;
                Ok(encoder.encode(frame, &mut |_| {})?)
            });
        match tried {
            Ok(()) => codecs.push(codec),
            Err(error) => failures.push(format!("{codec:?}: {error:#}")),
        }
    }
    if codecs.is_empty() {
        bail!("{}", failures.join("; "));
    }
    Ok(codecs)
}

/// A mid-gray 1280x720 BGRA frame on `device`, for test encodes that need
/// no screen capture (they also work from an SSH or service session).
fn test_frame(device: &windows::Win32::Graphics::Direct3D11::ID3D11Device) -> Result<VideoFrame> {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_SHADER_RESOURCE, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    let (width, height) = (1280u32, 720u32);
    let pixels = vec![0x80u8; (width * height * 4) as usize];
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
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    };
    let mut texture = None;
    // SAFETY: `pixels` outlives the call, which copies it.
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture))? };
    Ok(VideoFrame {
        id: 0,
        captured_us: SystemClock::new().now_us(),
        surface: frames_media::api::Surface::Platform(Arc::new(
            frames_codec::d3d11::D3D11Surface {
                texture: texture.context("no texture")?,
                subresource: 0,
                width,
                height,
                matrix_709: true,
                full_range: true,
                keepalive: None,
            },
        )),
        dirty: None,
    })
}

/// What each encoder in the fallback order can do here, by test-encoding a
/// generated frame on its GPU: the adapter and working codecs, or why not.
/// One encoder's test result: its adapter and working codecs, or why not.
pub type Probe = (&'static str, Result<(String, Vec<crate::Codec>)>);

pub fn probe_backends(o: &Options) -> Vec<Probe> {
    let shared: Arc<dyn MediaClock> = Arc::new(SystemClock::new());
    backends(o)
        .into_iter()
        .map(|backend| {
            let result = backend_device(backend).and_then(|(device, _context, adapter)| {
                let frame = test_frame(&device)?;
                Ok((
                    adapter,
                    working_codecs(backend, &device, &frame, o, &shared)?,
                ))
            });
            (backend, result)
        })
        .collect()
}

/// v1's "Window compatibility" for the shared window's executable: its
/// verified record, or why it is untested.
fn compatibility(hwnd: windows::Win32::Foundation::HWND) -> String {
    use sha2::{Digest, Sha256};
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let hash = (|| -> Result<String> {
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)? };
        let mut path = vec![0u16; 32768];
        let mut size = path.len() as u32;
        let named = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(path.as_mut_ptr()),
                &mut size,
            )
        };
        let _ = unsafe { CloseHandle(process) };
        named?;
        let path = String::from_utf16_lossy(&path[..size as usize]);
        let mut file = std::fs::File::open(path)?;
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash)?;
        Ok(format!("{:x}", hash.finalize()))
    })();
    match (hash, frames_core::shared_game::Catalog::bundled()) {
        (Ok(hash), Ok(catalog)) => catalog.lookup(&hash).description(),
        (Err(error), _) | (_, Err(error)) => {
            format!("Untested\nThe executable could not be checked: {error:#}")
        }
    }
}

/// Capture failing is not the encoder's fault, so it is reported apart.
#[derive(Debug)]
struct CaptureFailed;

impl std::fmt::Display for CaptureFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "screen capture failed (hosting needs the signed-in desktop; an SSH or service session cannot capture)",
        )
    }
}

fn open_backend(
    backend: &'static str,
    o: &Options,
    source: Option<windows::Win32::Foundation::HWND>,
    clock: &SystemClock,
    shared: &Arc<dyn MediaClock>,
) -> Result<Opened> {
    let (device, context, adapter) = backend_device(backend)?;
    let capture = || -> Result<_> {
        let mut capture = match source {
            Some(hwnd) => {
                let mut capture = WgcCapture::window(hwnd, &device, shared.clone())?;
                // Only the window's content, as v1 ("Show remote window
                // header" in the viewer's menu shows the rest).
                capture.set_header(false);
                capture
            }
            None => WgcCapture::primary_monitor(&device, shared.clone())?,
        };
        capture.configure(CaptureConfig {
            fps: o.fps,
            separate_cursor: true,
        })?;
        let first = first_frame(&mut capture, clock)?;
        Ok((capture, first))
    };
    let (capture, first) = capture().context(CaptureFailed)?;
    let codecs = working_codecs(backend, &device, &first, o, shared)?;
    Ok(Opened {
        backend,
        device,
        _context: context,
        adapter,
        capture,
        first,
        codecs,
    })
}

fn stream(
    handle: &tokio::runtime::Handle,
    session: &frames_net::Session,
    o: &Options,
    services: &crate::services::HostServices,
) -> Result<()> {
    let (events, clipboard_streams) = (&services.events, &services.clipboard);
    win::dpi_aware();
    let _mmcss = Mmcss::games();
    let _timer = TimerResolution::one_millisecond();
    let _awake = KeepAwake::display();
    let clock = SystemClock::new();
    let shared: Arc<dyn MediaClock> = Arc::new(clock);
    let qa = if o.qa_source {
        Some(win::QaSource::start(1920, 1080, o.fps)?)
    } else {
        None
    };
    // One window (by catalog id, or --window), the QA source, or the whole
    // primary monitor.
    let shared_window = match (o.window_id, &o.window) {
        (Some(id), _) => {
            let hwnd = windows::Win32::Foundation::HWND(id as usize as *mut _);
            let (_, title) = win::shareable_windows()
                .into_iter()
                .find(|(h, _)| *h == hwnd)
                .context("that window is no longer shared")?;
            println!("sharing one window: {title}");
            Some(hwnd)
        }
        (None, Some(needle)) => {
            let (hwnd, title) = win::find_window(needle)
                .with_context(|| format!("no window whose title contains {needle:?}"))?;
            println!("sharing one window: {title}");
            Some(hwnd)
        }
        (None, None) => None,
    };
    let source = qa.as_ref().map(|qa| qa.hwnd()).or(shared_window);
    // Capture on the encoder's GPU: hardware encoders only accept textures
    // from their own adapter.
    let mut opened = None;
    let mut encoder_failed = false;
    for backend in backends(o) {
        match open_backend(backend, o, source, &clock, &shared) {
            Ok(found) => {
                opened = Some(found);
                break;
            }
            Err(error) if error.downcast_ref::<CaptureFailed>().is_some() => {
                println!("{backend}: {error:#}")
            }
            Err(error) => {
                encoder_failed = true;
                println!("encoder {backend} unavailable: {error:#}")
            }
        }
    }
    let Opened {
        backend,
        device,
        _context,
        adapter,
        mut capture,
        first,
        codecs,
    } = opened.with_context(|| {
        if encoder_failed {
            "no encoder works on this machine"
        } else {
            "the screen cannot be captured here"
        }
    })?;
    let nvenc = backend == "nvenc";
    raise_gpu_priority(&device);
    // NVENC runs 2-2.5x slower when the GPU idles in a low power state
    // (spike 0-C). This holds clocks with a small GPU load for the session.
    let _hold = (nvenc && o.hold > 0)
        .then(|| GpuClockHold::start(&device, Duration::from_millis(1), o.hold))
        .transpose()?;
    println!("adapter: {adapter}");
    let api = match backend {
        "nvenc" => frames_codec::nvenc::api_version()
            .map(|v| format!(", API {v}"))
            .unwrap_or_default(),
        _ => String::new(),
    };
    println!(
        "encoder: {backend} ({}{api})",
        codecs
            .iter()
            .map(|c| format!("{c:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mut injector = InputInjector::new(match (&qa, shared_window) {
        (Some(source), _) => InputTarget::Window(source.hwnd()),
        (None, Some(hwnd)) => InputTarget::WindowArea {
            hwnd,
            header: false,
        },
        (None, None) => InputTarget::Desktop(primary_monitor_rect()),
    });
    // One window with isolated input: into that window's process only. If
    // the adapter cannot attach, the viewer gets no input at all rather
    // than the host's own mouse and keyboard.
    // Why input did not reach the shared window, for the viewer's title.
    let mut notices = crate::streams::Notices::new(session.clone(), handle.clone());
    let mut isolated = match (o.isolated, shared_window) {
        (true, Some(hwnd)) if o.control => {
            let config = crate::admit::config_dir(o.config_dir.as_ref())?;
            match crate::isolated::Isolated::attach(hwnd, &config) {
                Ok(isolated) => {
                    println!("isolated input attached: the host keeps its own mouse and keyboard");
                    Some(isolated)
                }
                Err(error) => {
                    println!("isolated input unavailable, viewing only: {error:#}");
                    None
                }
            }
        }
        _ => None,
    };
    let global_input = o.control && !o.isolated;
    let mut secure = Secure::new();
    let (width, height) = first.surface.size();
    println!("capturing {width}x{height} at up to {} fps", o.fps);
    let chosen = crate::choose_codec(o, &codecs, session.media().path().kind == PathKind::Relay);
    let codec = if codecs.contains(&chosen) {
        chosen
    } else if o.codec_explicit {
        bail!("{chosen:?} does not encode with {backend} here (it does {codecs:?})");
    } else {
        // Nothing both ends share: the best this host has.
        codecs[0]
    };
    println!("codec: {codec:?}");
    let info = StreamInfo {
        codec,
        width,
        height,
        fps: o.fps,
        platform: crate::platform().into(),
        name: crate::device_name(o),
        compatibility: shared_window.filter(|_| o.isolated).map(compatibility),
    };
    handle
        .block_on(send_info(session, info))
        .context("sending the stream description")?;
    // Cursor shape and visibility, opened after the description so the
    // viewer accepts the two streams in that order.
    let (mut cursor_stream, _) =
        handle.block_on(crate::streams::open(session, crate::streams::CURSOR))?;
    if o.adb && o.control && o.windows.is_empty() {
        handle.spawn(crate::streams::adb_listener(session.clone(), o.adb_listen));
    }
    let (cursor_tx, mut cursor_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    handle.spawn(async move {
        while let Some(bytes) = cursor_rx.recv().await {
            if cursor_stream.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });
    let send_cursor = |message: CursorMessage| {
        let mut bytes = Vec::new();
        message.encode(&mut bytes);
        let _ = cursor_tx.send(bytes);
    };
    if qa.is_some() {
        // The real cursor is not over the QA window; show a known shape.
        send_cursor(win::qa_cursor());
        send_cursor(CursorMessage::Visible(true));
    }
    let mut cursor_visible = None;

    let params = encode_params(o, width, height);
    let mut encoder = make_encoder(backend, &device, codec, params, shared.clone())?;
    let media = session.media();
    let rate = RateConfig {
        min_bps: 2_000_000,
        max_bps: o.max_mbps() * 1_000_000,
        start_bps: o.start_mbps() * 1_000_000,
    };
    let mut config = HostConfig::new(1, 1, rate);
    if let Some(pacing) = o.pacing {
        config.pacing_factor = pacing;
    }
    config.max_datagram = media.path().max_datagram.min(1200);
    let mut host = HostSession::new(config);
    // n0's public relays deliver about 8-10 Mbps; self-hosted ones are
    // sized by whoever runs them, so only the public ones are capped.
    let relay_max_bps = if o.relays.is_empty() {
        frames_net::N0_RELAY_MAX_BPS
    } else {
        rate.max_bps
    };
    // The viewer's image quality caps the rate; it can change it during the
    // session (`Event::MaxBitrate`). Relays cap it further.
    let ceiling_bps = (o.mbps * 2).max(20) * 1_000_000;
    let mut viewer_cap = o.max_mbps_cap;
    let max_bps = |kind: PathKind, cap: Option<u32>| {
        let max = cap.map_or(ceiling_bps, |mbps| ceiling_bps.min(mbps * 1_000_000));
        match kind {
            PathKind::Relay => max.min(relay_max_bps),
            PathKind::Direct => max,
        }
    };
    let mut path_kind = media.path().kind;
    let mut path_key = media.selected_path_key();
    if path_kind == PathKind::Relay {
        host.set_max_bps(relay_max_bps);
    }

    let mut clipboard: Option<crate::clip::ClipboardLink> = None;
    let started = Instant::now();
    let mut next_report = Duration::from_secs(1);
    let mut last_frame: Option<VideoFrame> = None;
    let mut last_encode_us = 0u64;
    let frame_interval_us = 1_000_000 / u64::from(o.fps.max(1));
    let mut pending = Some(first);
    let mut follower = crate::resize::Follower::new((width, height));
    // A shared application window's menus, for the viewer's window.
    let mut host_menus = o.window_id.map(crate::menus::HostMenus::start);
    // The shared window's title bar and borders are in the picture.
    let mut header = false;
    let mut resize_stream = None;
    let mut frames = 0u64;
    let mut encode_us = Vec::new();
    // The viewer's window hidden or behind others (`Event::Visibility`).
    let mut visibility = crate::pacing::Visibility::default();
    let mut held = None;
    loop {
        if o.deadline().is_some_and(|d| started.elapsed() >= d) {
            break;
        }
        if session.is_closed() {
            println!("the viewer left");
            break;
        }
        win::pump();
        let path = media.path();
        let key = media.selected_path_key();
        if path.kind != path_kind || (key.is_some() && key != path_key) {
            path_kind = path.kind;
            path_key = key;
            // Rate control restarts on the new path and probes it.
            host.path_changed();
            host.set_max_bps(max_bps(path.kind, viewer_cap));
            println!(
                "path changed to {path_kind:?} {}",
                path_key.as_deref().unwrap_or("")
            );
        }
        // The service agent shows and drives the secure desktop (UAC, sign
        // in) while it is up, through Desktop Duplication.
        if secure.privileged && secure.next_check <= Instant::now() {
            secure.check();
        }
        let frame = match pending.take() {
            Some(frame) => Some(frame),
            None if secure.capture.is_some() => {
                // Drain WGC so it does not queue frames meanwhile.
                let _ = capture.next(clock.now_us());
                secure.frame(&device, &clock)
            }
            None => match capture.next(clock.now_us() + 500) {
                Ok(CaptureEvent::Frame(frame)) => Some(frame),
                Ok(CaptureEvent::Lost) => bail!("the capture source went away"),
                // Frames follow at the new size (`crate::resize`).
                Ok(CaptureEvent::Resized { .. }) => None,
                Ok(CaptureEvent::Timeout) => None,
                Err(error) => bail!("capture failed: {error}"),
            },
        };
        // With isolated input the game's own cursor is the one to show.
        if let Some(input) = &mut isolated
            && let Err(error) = input.tick(&send_cursor)
        {
            println!("isolated input stopped, viewing only: {error:#}");
            isolated = None;
        }
        if qa.is_none()
            && isolated.is_none()
            && let Some(update) = capture.cursor()
        {
            if let Some(shape) = update.shape {
                send_cursor(CursorMessage::Shape {
                    width: shape.width as u16,
                    height: shape.height as u16,
                    hotspot_x: shape.hotspot_x as u16,
                    hotspot_y: shape.hotspot_y as u16,
                    bgra: shape.bgra.to_vec(),
                });
            }
            if cursor_visible != Some(update.visible) {
                cursor_visible = Some(update.visible);
                send_cursor(CursorMessage::Visible(update.visible));
            }
        }
        // A still picture sends no new frames; when the viewer needs a
        // recovery, encode the last one again.
        let frame = match frame {
            Some(frame) => {
                last_frame = Some(frame.clone());
                Some(frame)
            }
            None if host.wants_frame()
                && clock.now_us().saturating_sub(last_encode_us) >= frame_interval_us =>
            {
                last_frame.clone().map(|mut frame| {
                    frame.captured_us = clock.now_us();
                    frame
                })
            }
            None => None,
        };
        let frame = visibility.admit(frame, &mut held, clock.now_us(), last_encode_us);
        if frame.is_some() && visibility.take_keyframe() {
            encoder.control(EncoderControl::ForceKeyframe)?;
        }
        let sized = frame.map(|frame| {
            let size = frame.surface.size();
            (frame, size)
        });
        if let crate::resize::Step::Encode(frame, resized) = follower.step(sized, Instant::now()) {
            if let Some(size) = resized {
                let mut params = encode_params(o, size.0, size.1);
                params.bitrate_bps = host.target_bps();
                encoder.control(EncoderControl::Reconfigure(params))?;
                encoder.control(EncoderControl::ForceKeyframe)?;
                println!("source resized to {}x{}", size.0, size.1);
                crate::resize::announce(handle, session, &mut resize_stream, size)?;
            }
            let before = clock.now_us();
            last_encode_us = before;
            host.encode(before, encoder.as_mut(), &frame)?;
            encode_us.push(clock.now_us().saturating_sub(before));
            frames += 1;
        }
        if clipboard.is_none()
            && let Ok(streams) = clipboard_streams.try_recv()
        {
            clipboard = Some(crate::clip::ClipboardLink::start(
                handle,
                streams,
                o.clipboard,
                false,
            ));
        }
        if let Some(clipboard) = &mut clipboard {
            clipboard.tick();
        }
        let now = clock.now_us();
        // Every iteration, not per frame: probe clusters go out between
        // frames and need the transport paced at their rate.
        media.set_target_rate(u64::from(host.transport_target_bps()));
        while let Some(datagram) = host.poll_transmit(now) {
            let _ = media.try_send(datagram);
        }
        let rtt = path.rtt_us;
        while let Some((datagram, arrived)) = media.try_recv_timed() {
            let arrived = arrived.unwrap_or_else(|| clock.now_us());
            match Pointer::decode(&datagram) {
                // A view-only device's input is dropped here, on the host.
                Some(pointer) => {
                    if let Some(input) = secure.input.as_ref().filter(|_| o.control) {
                        input.pointer(pointer);
                    } else if let Some(input) = &mut isolated {
                        let _ = input.pointer(&pointer);
                    } else if global_input {
                        injector.pointer(&pointer);
                    }
                }
                None => match Motion::decode(&datagram) {
                    Some(motion) => {
                        if let Some(input) = secure.input.as_ref().filter(|_| o.control) {
                            input.motion(motion);
                        } else if let Some(input) = &mut isolated {
                            let _ = input.motion(&motion);
                        } else if global_input {
                            injector.motion(&motion);
                        }
                    }
                    None => host.on_datagram(arrived, &datagram, rtt),
                },
            }
        }
        if let Some(menus) = &mut host_menus
            && let Err(error) = menus.send_changes(handle, session)
        {
            println!("application menus stopped: {error:#}");
            host_menus = None;
        }
        while let Ok(event) = events.try_recv() {
            match event {
                frames_input::Event::Visibility { shown, focused } => {
                    if visibility.set(shown, focused) {
                        println!("the viewer's window: {}", visibility.describe());
                    }
                    continue;
                }
                frames_input::Event::Header { show } => {
                    // Isolated input maps onto the client area only.
                    if let Some(hwnd) = shared_window.filter(|_| isolated.is_none()) {
                        capture.set_header(show);
                        injector.set_target(InputTarget::WindowArea { hwnd, header: show });
                        header = show;
                        println!(
                            "the viewer {} the window's title bar",
                            if show { "shows" } else { "hides" }
                        );
                    }
                    continue;
                }
                frames_input::Event::RemoteCursor { show } => {
                    if let Err(error) = capture.set_cursor_capture(show) {
                        println!("cannot change cursor capture: {error}");
                    } else {
                        println!(
                            "host cursor in the picture: {}",
                            if show { "on" } else { "off" }
                        );
                    }
                    continue;
                }
                frames_input::Event::RefreshMenus => {
                    if let Some(menus) = &mut host_menus {
                        menus.refresh();
                    }
                    continue;
                }
                frames_input::Event::ResizeWindow { width, height } => {
                    if o.control
                        && let Some(hwnd) = shared_window
                    {
                        match resize_window(hwnd, header, u32::from(width), u32::from(height)) {
                            Ok(()) => println!("the viewer resized the window to {width}x{height}"),
                            Err(error) => println!("cannot resize the window: {error:#}"),
                        }
                    }
                    continue;
                }
                _ => {}
            }
            if let frames_input::Event::Menu { id } = event {
                if o.control
                    && let Some(menus) = &host_menus
                {
                    menus.invoke(id);
                }
                continue;
            }
            if let frames_input::Event::MaxBitrate { mbps } = event {
                viewer_cap = Some(u32::from(mbps).max(1));
                host.set_max_bps(max_bps(media.path().kind, viewer_cap));
                println!("the viewer's image quality: at most {mbps} Mbps");
                continue;
            }
            if event == frames_input::Event::CloseWindow {
                // As the window's close button would; it may ask to save.
                if o.control
                    && let Some(hwnd) = shared_window
                {
                    println!("the viewer asked to close the window");
                    unsafe {
                        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                            Some(hwnd),
                            windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                            windows::Win32::Foundation::WPARAM(0),
                            windows::Win32::Foundation::LPARAM(0),
                        );
                    }
                }
                continue;
            }
            if let Some(input) = secure.input.as_ref().filter(|_| o.control) {
                input.event(event);
            } else if let Some(input) = &mut isolated {
                if let Err(error) = input.event(event) {
                    println!("isolated input stopped, viewing only: {error:#}");
                    notices.send(&format!("Input stopped (view only now): {error:#}"));
                    isolated = None;
                }
            } else if global_input {
                injector.event(event);
            }
        }
        // Pointer-only failures matter too: otherwise a blocked desktop
        // looks connected until the viewer next clicks or types.
        if let Some(problem) = injector.take_problem() {
            notices.send(problem);
        }
        if started.elapsed() >= next_report {
            if next_report.as_secs() == 2 {
                for (offered, sent, received, delivered) in host.probe_report() {
                    println!(
                        "  probe {:.1} Mbps: sent {sent} received {received} delivered {}",
                        offered / 1e6,
                        delivered.map_or("-".into(), |d| format!("{:.1} Mbps", d / 1e6))
                    );
                }
            }
            let stats = host.stats();
            let p50 = crate::stats::percentile_ms(&mut encode_us, 50);
            println!(
                "host t={:>3}s frames {frames:>5} encode p50 {p50:.2} ms target {:>5.1} Mbps keyframes {} invalidations {} retx {} | {:?} rtt {:.1} ms",
                next_report.as_secs(),
                f64::from(host.target_bps()) / 1e6,
                stats.keyframes,
                stats.invalidations,
                stats.retransmissions,
                path.kind,
                path.rtt_us as f64 / 1e3,
            );
            encode_us.clear();
            if qa.is_none() && global_input {
                println!(
                    "host input: received {} events, applied {}, rejected SendInput {}",
                    services
                        .event_count
                        .load(std::sync::atomic::Ordering::Relaxed),
                    injector.applied,
                    injector.rejected(),
                );
            }
            if qa.is_some() {
                let counts = &win::QA_INPUT;
                let load =
                    |c: &std::sync::atomic::AtomicU64| c.load(std::sync::atomic::Ordering::Relaxed);
                println!(
                    "host input: applied {} | QA window received moves {} buttons {} wheels {} keys {} chars {} last {:?}",
                    injector.applied,
                    load(&counts.moves),
                    load(&counts.buttons),
                    load(&counts.wheels),
                    load(&counts.keys),
                    load(&counts.chars),
                    win::QA_TEXT.lock().unwrap().as_str(),
                );
            }
            next_report += Duration::from_secs(1);
        }
    }
    injector.release_all();
    win::pump();
    if let Some(clipboard) = &clipboard {
        println!(
            "CLIPBOARD sent={} received={} now={:?}",
            clipboard.sent,
            clipboard.received,
            clipboard.current()
        );
    }
    session.close("host stopped");
    Ok(())
}

/// Resizes a shared window so the part the viewer sees (`header`: with its
/// title bar and borders) becomes `width` x `height` pixels, as v1's
/// "Resize remote app": within its own size limits and the work area.
fn resize_window(
    hwnd: windows::Win32::Foundation::HWND,
    header: bool,
    width: u32,
    height: u32,
) -> Result<()> {
    use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        anyhow::ensure!(
            !IsZoomed(hwnd).as_bool() && !IsIconic(hwnd).as_bool(),
            "the window is maximized or minimized"
        );
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        anyhow::ensure!(style & WS_THICKFRAME.0 != 0, "the window has a fixed size");
        let mut outer = RECT::default();
        GetWindowRect(hwnd, &mut outer)?;
        let shown = frames_platform_windows::input::captured_rect(hwnd, header);
        let mut limits = MINMAXINFO::default();
        let _ = SendMessageTimeoutW(
            hwnd,
            WM_GETMINMAXINFO,
            WPARAM(0),
            LPARAM(&mut limits as *mut _ as isize),
            SMTO_ABORTIFHUNG,
            10,
            None,
        );
        anyhow::ensure!(
            !(limits.ptMinTrackSize.x > 0 && limits.ptMinTrackSize == limits.ptMaxTrackSize),
            "the window has a fixed size"
        );
        let mut monitor = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(
            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
            &mut monitor,
        );
        let work = monitor.rcWork;
        let extra_w = (outer.right - outer.left) - (shown.right - shown.left);
        let extra_h = (outer.bottom - outer.top) - (shown.bottom - shown.top);
        let clamp = |value: i32, min: i32, max: i32, screen: i32| {
            let max = if max > 0 { max } else { 32768 };
            value.min(max).min(screen.max(200)).max(min.max(100))
        };
        let new_w = clamp(
            width as i32 + extra_w,
            limits.ptMinTrackSize.x,
            limits.ptMaxTrackSize.x,
            work.right - work.left,
        );
        let new_h = clamp(
            height as i32 + extra_h,
            limits.ptMinTrackSize.y,
            limits.ptMaxTrackSize.y,
            work.bottom - work.top,
        );
        if (new_w, new_h) == (outer.right - outer.left, outer.bottom - outer.top) {
            return Ok(());
        }
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            new_w,
            new_h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        )?;
    }
    Ok(())
}
