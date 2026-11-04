//! Phase 3 real-hardware loop (docs/V2_PLAN.md): capture → NVENC → v2 host
//! engine → simulated LAN link (real time) → v2 client engine → D3D11VA
//! decode → immediate presentation, with per-stage latency.
//!
//! ```text
//! hw_loop [--source window|synthetic] [--codec hevc|h264] [--fps 60] [--frames 900]
//!         [--size 1920x1080] [--mbps 30] [--slices 4] [--hold CLEARS_PER_MS] [--vsync]
//! ```
//!
//! Needs an interactive desktop and an NVIDIA GPU. It only captures the QA
//! window it creates ("Frames QA source hw-loop") or synthetic GPU content.

#[cfg(not(windows))]
fn main() {
    eprintln!("hw_loop runs on Windows only");
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows_main::run()
}

#[cfg(windows)]
mod windows_main {
    use frames_codec::d3d11::{D3D11Surface, nvidia_device};
    use frames_codec::ffmpeg::FfmpegDecoder;
    use frames_codec::nvenc::NvencEncoder;
    use frames_media::api::{
        AccessUnit, CaptureConfig, CaptureEvent, CaptureSource, Codec, Decoded, DecoderCaps,
        EncodeParams, Gop, MediaClock, MediaError, MediaTransport, PresentMode, Presenter, Surface,
        VideoDecoder, VideoFrame,
    };
    use frames_media::client::{ClientConfig, ClientSession};
    use frames_media::host::{HostConfig, HostSession};
    use frames_media::rate::RateConfig;
    use frames_platform_windows::capture::WgcCapture;
    use frames_platform_windows::present::D3D11Presenter;
    use frames_platform_windows::system::{
        GpuClockHold, Mmcss, TimerResolution, raise_gpu_priority,
    };
    use frames_testkit::clock::{SystemClock, VirtualClock};
    use frames_testkit::link::LinkConfig;
    use frames_testkit::transport;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Duration;
    use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::{PCWSTR, w};

    #[derive(Clone, Debug)]
    struct Options {
        synthetic: bool,
        codec: Codec,
        fps: u32,
        frames: usize,
        width: u32,
        height: u32,
        mbps: u32,
        slices: u8,
        hold: u32,
        vsync: bool,
    }

    fn options() -> anyhow::Result<Options> {
        let mut o = Options {
            synthetic: false,
            codec: Codec::Hevc,
            fps: 60,
            frames: 900,
            width: 1920,
            height: 1080,
            mbps: 30,
            slices: 4,
            hold: 0,
            vsync: false,
        };
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            let mut value = || {
                args.next()
                    .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))
            };
            match arg.as_str() {
                "--source" => o.synthetic = value()? == "synthetic",
                "--codec" => {
                    o.codec = if value()? == "h264" {
                        Codec::H264
                    } else {
                        Codec::Hevc
                    }
                }
                "--fps" => o.fps = value()?.parse()?,
                "--frames" => o.frames = value()?.parse()?,
                "--size" => {
                    let v = value()?;
                    let (w, h) = v
                        .split_once('x')
                        .ok_or_else(|| anyhow::anyhow!("--size WxH"))?;
                    o.width = w.parse()?;
                    o.height = h.parse()?;
                }
                "--mbps" => o.mbps = value()?.parse()?,
                "--slices" => o.slices = value()?.parse()?,
                "--hold" => o.hold = value()?.parse()?,
                "--vsync" => o.vsync = true,
                other => anyhow::bail!("unknown option {other}"),
            }
        }
        Ok(o)
    }

    #[derive(Clone, Copy, Debug, Default)]
    struct Record {
        captured: u64,
        arrival: u64,
        submit: u64,
        first_slice: u64,
        encoded: u64,
        bytes: u32,
        polls: u32,
        decode_start: u64,
        decoded: u64,
        presented: u64,
    }

    type Records = Rc<RefCell<BTreeMap<u64, Record>>>;

    /// Records decode start and end per frame.
    struct TimedDecoder {
        inner: FfmpegDecoder,
        clock: SystemClock,
        records: Records,
    }

    impl VideoDecoder for TimedDecoder {
        fn caps(&self) -> DecoderCaps {
            self.inner.caps()
        }
        fn decode(&mut self, unit: AccessUnit) -> Result<Decoded, MediaError> {
            let frame = unit.frame;
            let start = self.clock.now_us();
            let result = self.inner.decode(unit);
            let end = self.clock.now_us();
            if let Some(r) = self.records.borrow_mut().get_mut(&frame) {
                r.decode_start = start;
                r.decoded = end;
            }
            result
        }
    }

    // Rc is fine: the loop is single-threaded; the trait only asks for Send.
    unsafe impl Send for TimedDecoder {}

    unsafe extern "system" fn procedure(hwnd: HWND, message: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        if message == WM_CLOSE {
            return LRESULT(0);
        }
        unsafe { DefWindowProcW(hwnd, message, w, l) }
    }

    fn window(title: PCWSTR, x: i32, width: u32, height: u32) -> anyhow::Result<HWND> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("FramesHwLoop");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            RegisterClassW(&wc);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                title,
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                x,
                0,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            Ok(hwnd)
        }
    }

    fn pump() {
        unsafe {
            let mut message = MSG::default();
            while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }

    /// Paints moving content into the QA source window with GDI.
    fn paint(hwnd: HWND, n: u64, width: u32, height: u32) {
        unsafe {
            let dc = GetDC(Some(hwnd));
            let background = CreateSolidBrush(COLORREF(0x00202020 + ((n as u32 * 3) & 0x3f)));
            FillRect(
                dc,
                &RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                },
                background,
            );
            let _ = DeleteObject(background.into());
            for stripe in 0..24u32 {
                let brush = CreateSolidBrush(COLORREF(
                    (stripe * 0x0a0b0c + n as u32 * 0x010203) & 0x00ffffff,
                ));
                let y = (stripe * height / 24) as i32;
                let x = ((n as u32 * (stripe + 3) * 7) % width) as i32;
                FillRect(
                    dc,
                    &RECT {
                        left: x,
                        top: y,
                        right: x + 240,
                        bottom: y + (height / 24) as i32,
                    },
                    brush,
                );
                let _ = DeleteObject(brush.into());
            }
            let text: Vec<u16> = format!("Frames QA source  frame {n:08}  ")
                .encode_utf16()
                .collect();
            SetBkColor(dc, COLORREF(0x00ffffff));
            for row in 0..20 {
                let _ = TextOutW(dc, 40, 40 + row * 48, &text);
            }
            ReleaseDC(Some(hwnd), dc);
        }
    }

    /// Synthetic GPU source: a high-frequency pattern scrolled per frame plus
    /// a moving box, as in spike 0-C.
    struct Synthetic {
        context: ID3D11DeviceContext,
        context1: ID3D11DeviceContext1,
        pattern: ID3D11Texture2D,
        ring: Vec<(ID3D11Texture2D, ID3D11RenderTargetView)>,
        width: u32,
        height: u32,
        n: u64,
    }

    impl Synthetic {
        fn new(device: &ID3D11Device, width: u32, height: u32) -> anyhow::Result<Self> {
            let (pw, ph) = (width * 2, height * 2);
            let mut pixels = vec![0u32; (pw * ph) as usize];
            for y in 0..ph {
                for x in 0..pw {
                    let mut h =
                        (x / 24).wrapping_mul(73_856_093) ^ (y / 40).wrapping_mul(19_349_663);
                    h ^= h >> 13;
                    h = h.wrapping_mul(0x5bd1_e995);
                    h ^= h >> 15;
                    let ink = if h & 7 < 2 { 30 } else { 220 };
                    let (r, g) = (x * 255 / pw, y * 255 / ph);
                    pixels[(y * pw + x) as usize] = 0xff00_0000 | (r << 16) | (g << 8) | ink;
                }
            }
            let mut desc = D3D11_TEXTURE2D_DESC {
                Width: pw,
                Height: ph,
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
                SysMemPitch: pw * 4,
                SysMemSlicePitch: 0,
            };
            let mut pattern = None;
            let mut ring = Vec::new();
            unsafe {
                device.CreateTexture2D(&desc, Some(&data), Some(&mut pattern))?;
                desc.Width = width;
                desc.Height = height;
                desc.BindFlags = (D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE).0 as u32;
                for _ in 0..4 {
                    let mut texture = None;
                    device.CreateTexture2D(&desc, None, Some(&mut texture))?;
                    let texture = texture.unwrap();
                    let mut view = None;
                    device.CreateRenderTargetView(&texture, None, Some(&mut view))?;
                    ring.push((texture, view.unwrap()));
                }
                let context = device.GetImmediateContext()?;
                let context1: ID3D11DeviceContext1 = windows::core::Interface::cast(&context)?;
                Ok(Self {
                    context,
                    context1,
                    pattern: pattern.unwrap(),
                    ring,
                    width,
                    height,
                    n: 0,
                })
            }
        }

        fn frame(&mut self, clock: &SystemClock) -> VideoFrame {
            let n = self.n;
            self.n += 1;
            let (texture, view) = self.ring[(n % 4) as usize].clone();
            let (w, h) = (self.width, self.height);
            unsafe {
                let left = (n as u32 * 7) % w;
                let top = (n as u32 * 3) % h;
                self.context.CopySubresourceRegion(
                    &texture,
                    0,
                    0,
                    0,
                    0,
                    &self.pattern,
                    0,
                    Some(&D3D11_BOX {
                        left,
                        top,
                        front: 0,
                        right: left + w,
                        bottom: top + h,
                        back: 1,
                    }),
                );
                let colour = [
                    (n % 60) as f32 / 60.0,
                    0.2,
                    1.0 - (n % 90) as f32 / 90.0,
                    1.0,
                ];
                let x = ((n * 11) % u64::from(w - 200)) as i32;
                let y = ((n * 5) % u64::from(h - 200)) as i32;
                let rect = RECT {
                    left: x,
                    top: y,
                    right: x + 200,
                    bottom: y + 200,
                };
                self.context1.ClearView(&view, &colour, Some(&[rect]));
                self.context.Flush();
            }
            VideoFrame {
                id: n,
                captured_us: clock.now_us(),
                surface: Surface::Platform(Arc::new(D3D11Surface {
                    texture,
                    subresource: 0,
                    width: w,
                    height: h,
                    matrix_709: true,
                    full_range: true,
                    keepalive: None,
                })),
                dirty: None,
            }
        }
    }

    fn pstate() -> String {
        std::process::Command::new("nvidia-smi")
            .args([
                "--query-gpu=pstate,clocks.video,clocks.gr,utilization.encoder,power.draw",
                "--format=csv,noheader",
            ])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|e| format!("nvidia-smi unavailable: {e}"))
    }

    fn percentile(values: &mut [u64], p: usize) -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        values.sort_unstable();
        values[(values.len() - 1) * p / 100] as f64 / 1000.0
    }

    pub fn run() -> anyhow::Result<()> {
        let o = options()?;
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let _mmcss = Mmcss::games();
        let _timer = TimerResolution::one_millisecond();
        let clock = SystemClock::new();
        let shared: Arc<dyn MediaClock> = Arc::new(clock);
        let (device, _context, adapter) = nvidia_device()?;
        raise_gpu_priority(&device);
        let _hold = if o.hold > 0 {
            Some(GpuClockHold::start(
                &device,
                Duration::from_millis(1),
                o.hold,
            )?)
        } else {
            None
        };
        println!("adapter: {adapter}; options: {o:?}");

        let source_window = if o.synthetic {
            None
        } else {
            Some(window(
                w!("Frames QA source hw-loop"),
                0,
                o.width,
                o.height,
            )?)
        };
        let viewer = window(w!("Frames hw-loop viewer"), 200, 960, 540)?;
        pump();
        let mut synthetic = if o.synthetic {
            Some(Synthetic::new(&device, o.width, o.height)?)
        } else {
            None
        };
        let mut capture = match source_window {
            Some(hwnd) => {
                paint(hwnd, 0, o.width, o.height);
                let mut c = WgcCapture::window(hwnd, &device, shared.clone())?;
                c.configure(CaptureConfig {
                    fps: o.fps,
                    separate_cursor: true,
                })?;
                Some(c)
            }
            None => None,
        };
        // The QA window repaints on its own thread at the target rate, so GDI
        // drawing never delays the engine loop.
        let painting = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let painter = source_window.map(|hwnd| {
            let (handle, running, (width, height, fps)) = (
                hwnd.0 as usize,
                painting.clone(),
                (o.width, o.height, o.fps),
            );
            std::thread::spawn(move || {
                let hwnd = HWND(handle as *mut _);
                let start = std::time::Instant::now();
                let interval = Duration::from_micros(1_000_000 / u64::from(fps));
                let mut n = 0u64;
                let mut costs = Vec::new();
                while running.load(std::sync::atomic::Ordering::Relaxed) {
                    let due = start + interval * n as u32;
                    if let Some(wait) = due.checked_duration_since(std::time::Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    let t = std::time::Instant::now();
                    paint(hwnd, n, width, height);
                    costs.push(t.elapsed().as_micros() as u64);
                    n += 1;
                }
                (n, percentile(&mut costs, 50))
            })
        });
        let params = EncodeParams {
            width: o.width,
            height: o.height,
            fps: o.fps,
            bitrate_bps: o.mbps * 1_000_000,
            vbv_frames: 1.0,
            gop: Gop::Infinite,
            intra_refresh_frames: Some(6),
            slices: o.slices,
        };
        let mut encoder = NvencEncoder::new(&device, o.codec, params, shared.clone())?;
        let rate = RateConfig {
            min_bps: 2_000_000,
            max_bps: (o.mbps * 2).max(20) * 1_000_000,
            start_bps: o.mbps * 1_000_000,
        };
        let mut host = HostSession::new(HostConfig::new(1, 1, rate));
        let mut client = ClientSession::new(ClientConfig::new(1, 1));
        let link_clock = VirtualClock::new(0);
        let (host_end, client_end) =
            transport::pair(&link_clock, LinkConfig::lan(), LinkConfig::lan(), 1);
        let records: Records = Rc::new(RefCell::new(BTreeMap::new()));
        let mut decoder = TimedDecoder {
            inner: FfmpegDecoder::new(o.codec, o.width, o.height, true)?,
            clock,
            records: records.clone(),
        };
        let mut presenter = D3D11Presenter::new(viewer, shared.clone());
        let interval = 1_000_000 / u64::from(o.fps);
        let mut next_frame_due = clock.now_us();
        let mut encoded = 0u64;
        let mut presented = 0usize;
        let mut sizes = Vec::new();
        let limit = clock.now_us() + (o.frames as u64 * interval) * 3 + 10_000_000;
        let mut gpu_state = None;
        let mut errors = Vec::new();
        while presented < o.frames && clock.now_us() < limit {
            pump();
            let now = clock.now_us();
            if encoded as usize == o.frames / 2 && gpu_state.is_none() {
                gpu_state = Some(std::thread::spawn(pstate));
            }
            // Produce at most one frame per interval.
            let mut frame = None;
            if now >= next_frame_due
                && let Some(s) = &mut synthetic
            {
                frame = Some(s.frame(&clock));
                next_frame_due += interval;
            }
            if let Some(c) = &mut capture
                && let Ok(CaptureEvent::Frame(f)) = c.next(clock.now_us() + 300)
            {
                frame = Some(f);
            }
            if let Some(f) = frame {
                let arrival = clock.now_us();
                let number = encoded;
                records.borrow_mut().insert(
                    number,
                    Record {
                        captured: f.captured_us,
                        arrival,
                        ..Default::default()
                    },
                );
                if let Err(e) = host.encode(arrival, &mut encoder, &f) {
                    errors.push(e.to_string());
                } else {
                    let t = encoder.last_timing();
                    sizes.push(u64::from(t.bytes));
                    if let Some(r) = records.borrow_mut().get_mut(&number) {
                        r.submit = t.submit_us;
                        r.first_slice = t.first_slice_us;
                        r.encoded = t.done_us;
                        r.bytes = t.bytes;
                        r.polls = t.polls;
                    }
                    encoded += 1;
                }
            }
            link_clock.advance_to(clock.now_us());
            let lnow = link_clock.now_us();
            while let Some(p) = host.poll_transmit(lnow) {
                let _ = host_end.try_send(p);
            }
            while let Some(p) = client_end.try_recv() {
                client.on_datagram(lnow, &p);
            }
            for d in client.poll(lnow, &mut decoder) {
                let number = d.frame;
                match presenter.present(
                    d,
                    if o.vsync {
                        PresentMode::Vsync
                    } else {
                        PresentMode::Immediate
                    },
                ) {
                    Ok(stamp) => {
                        presented += 1;
                        if let Some(r) = records.borrow_mut().get_mut(&number) {
                            r.presented = stamp.presented_us;
                        }
                    }
                    Err(e) => errors.push(e.to_string()),
                }
            }
            if let Some(fb) = client.poll_feedback(lnow) {
                let _ = client_end.try_send(fb);
            }
            let rtt = host_end.path().rtt_us;
            while let Some(p) = host_end.try_recv() {
                host.on_datagram(lnow, &p, rtt);
            }
            std::thread::sleep(Duration::from_micros(100));
        }

        painting.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(painter) = painter
            && let Ok((painted, cost)) = painter.join()
        {
            println!("  source window: {painted} paints, GDI paint p50 {cost:.2} ms");
        }
        let (h, c) = (host.stats(), client.stats());
        println!(
            "  host: invalidations {} forced keyframes {} retransmissions {} stale {} | client: skipped {} recoveries {} nacks {} target {:.1} Mbps",
            h.invalidations,
            h.forced_keyframes,
            h.retransmissions,
            h.dropped_stale,
            c.frames_skipped,
            c.recoveries,
            c.nacks,
            f64::from(host.target_bps()) / 1e6
        );
        let records = records.borrow();
        if let (Some(first), Some(last)) = (records.values().nth(30), records.values().last()) {
            let span = last.captured.saturating_sub(first.captured).max(1);
            let count = records.len().saturating_sub(31) as f64;
            println!(
                "  delivered capture rate {:.1} fps",
                count * 1e6 / span as f64
            );
        }
        let warm: Vec<&Record> = records
            .values()
            .skip(30)
            .filter(|r| r.presented > 0 && r.decoded > 0)
            .collect();
        let stage = |f: &dyn Fn(&Record) -> i64| -> (f64, f64) {
            let mut v: Vec<u64> = warm.iter().map(|r| f(r).max(0) as u64).collect();
            let p50 = percentile(&mut v, 50);
            let p95 = percentile(&mut v, 95);
            (p50, p95)
        };
        type Stage = Box<dyn Fn(&Record) -> i64>;
        let rows: [(&str, Stage); 8] = [
            (
                "capture arrival",
                Box::new(|r| r.arrival as i64 - r.captured as i64),
            ),
            (
                "arrival to submit",
                Box::new(|r| r.submit as i64 - r.arrival as i64),
            ),
            (
                "encode first slice",
                Box::new(|r| r.first_slice as i64 - r.submit as i64),
            ),
            (
                "encode full",
                Box::new(|r| r.encoded as i64 - r.submit as i64),
            ),
            (
                "encoded to decode start (pace+wire+reassembly)",
                Box::new(|r| r.decode_start as i64 - r.encoded as i64),
            ),
            (
                "decode",
                Box::new(|r| r.decoded as i64 - r.decode_start as i64),
            ),
            (
                "decoded to presented",
                Box::new(|r| r.presented as i64 - r.decoded as i64),
            ),
            (
                "capture to present",
                Box::new(|r| r.presented as i64 - r.captured as i64),
            ),
        ];
        println!(
            "HWLOOP source={} codec={:?} {}x{}@{} {} Mbps slices={} hold={} vsync={} frames encoded={} presented={} measured={} keyframes={} lost={} tearing={} refresh={}Hz",
            if o.synthetic {
                "synthetic"
            } else {
                "wgc-window"
            },
            o.codec,
            o.width,
            o.height,
            o.fps,
            o.mbps,
            o.slices,
            o.hold,
            o.vsync,
            encoded,
            presented,
            warm.len(),
            host.stats().keyframes,
            client.stats().frames_lost,
            presenter.caps().tearing,
            presenter.caps().refresh_hz,
        );
        for (name, f) in rows.iter() {
            let (p50, p95) = stage(f.as_ref());
            println!("  {name:<48} p50 {p50:>6.2} ms  p95 {p95:>6.2} ms");
        }
        let mut polls: Vec<u64> = warm.iter().map(|r| u64::from(r.polls) * 1000).collect();
        let mut sleeps: Vec<u64> = (0..200)
            .map(|_| {
                let start = clock.now_us();
                std::thread::sleep(Duration::from_micros(100));
                (clock.now_us() - start) * 1000
            })
            .collect();
        println!(
            "  encoder polls per picture p50 {:.0} p95 {:.0}; sleep(100us) takes p50 {:.0} us p95 {:.0} us",
            percentile(&mut polls, 50),
            percentile(&mut polls, 95),
            percentile(&mut sleeps, 50),
            percentile(&mut sleeps, 95)
        );
        let mut s = sizes.clone();
        println!(
            "  picture bytes p50 {:.0} KB p95 {:.0} KB",
            percentile(&mut s, 50),
            percentile(&mut sizes, 95)
        );
        if let Some(handle) = gpu_state {
            println!(
                "  GPU mid-run (pstate, video MHz, graphics MHz, enc %, W): {}",
                handle.join().unwrap_or_default()
            );
        }
        if !errors.is_empty() {
            errors.dedup();
            println!(
                "  errors ({}): {:?}",
                errors.len(),
                &errors[..errors.len().min(5)]
            );
        }
        Ok(())
    }
}
