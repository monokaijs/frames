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

/// Encoders to try, best first: `--encoder` alone when given; otherwise
/// each hardware encoder whose GPU is present (discrete first), then Media
/// Foundation and software AV1, so a missing or outdated driver (NVENC
/// needs API 12.0, driver 522+) falls back instead of failing.
pub fn backends(o: &Options) -> Vec<&'static str> {
    if o.encoder_explicit {
        return vec![match o.encoder.as_str() {
            "nvenc" => "nvenc",
            "amf" => "amf",
            "qsv" => "qsv",
            "mf" => "mf",
            _ => "software",
        }];
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
        bitrate_bps: o.mbps * 1_000_000,
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
            Some(hwnd) => WgcCapture::window(hwnd, &device, shared.clone())?,
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
    // One window (--window), the QA source, or the whole primary monitor.
    let shared_window = match &o.window {
        Some(needle) => {
            let (hwnd, title) = win::find_window(needle)
                .with_context(|| format!("no window whose title contains {needle:?}"))?;
            println!("sharing one window: {title}");
            Some(hwnd)
        }
        None => None,
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
        (None, Some(hwnd)) => InputTarget::WindowArea(hwnd),
        (None, None) => InputTarget::Desktop(primary_monitor_rect()),
    });
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
    };
    handle
        .block_on(send_info(session, info))
        .context("sending the stream description")?;
    // Cursor shape and visibility, opened after the description so the
    // viewer accepts the two streams in that order.
    let (mut cursor_stream, _) =
        handle.block_on(crate::streams::open(session, crate::streams::CURSOR))?;
    if o.adb && o.control {
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
        max_bps: (o.mbps * 2).max(20) * 1_000_000,
        start_bps: o.mbps * 1_000_000,
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
    let mut frames = 0u64;
    let mut encode_us = Vec::new();
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
            host.set_max_bps(match path.kind {
                PathKind::Relay => relay_max_bps,
                PathKind::Direct => rate.max_bps,
            });
            println!(
                "path changed to {path_kind:?} {}",
                path_key.as_deref().unwrap_or("")
            );
        }
        let frame = match pending.take() {
            Some(frame) => Some(frame),
            None => match capture.next(clock.now_us() + 500) {
                Ok(CaptureEvent::Frame(frame)) => Some(frame),
                Ok(CaptureEvent::Lost) => bail!("the capture source went away"),
                Ok(CaptureEvent::Resized { width, height }) => {
                    bail!("the source resized to {width}x{height}; restart the host")
                }
                Ok(CaptureEvent::Timeout) => None,
                Err(error) => bail!("capture failed: {error}"),
            },
        };
        if qa.is_none()
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
        if let Some(frame) = frame {
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
                Some(pointer) if o.control => {
                    injector.pointer(&pointer);
                }
                Some(_) => {}
                None => match Motion::decode(&datagram) {
                    Some(motion) if o.control => {
                        injector.motion(&motion);
                    }
                    Some(_) => {}
                    None => host.on_datagram(arrived, &datagram, rtt),
                },
            }
        }
        while let Ok(event) = events.try_recv() {
            if o.control {
                injector.event(event);
            }
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
            if qa.is_some() {
                let counts = &win::QA_INPUT;
                let load =
                    |c: &std::sync::atomic::AtomicU64| c.load(std::sync::atomic::Ordering::Relaxed);
                println!(
                    "host input: applied {} | QA window received moves {} buttons {} wheels {} keys {}",
                    injector.applied,
                    load(&counts.moves),
                    load(&counts.buttons),
                    load(&counts.wheels),
                    load(&counts.keys),
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
