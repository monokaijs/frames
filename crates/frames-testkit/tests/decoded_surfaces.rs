//! A viewer that shows only the newest frame gives a backlog's older frames
//! back to the decoder during the burst (`ClientSession::poll_latest`), so a
//! hardware decoder's fixed surface pool does not run out after a stall.

use frames_media::api::{
    AccessUnit, CaptureEvent, CaptureSource, Decoded, DecoderCaps, EncodeParams, Gop, MediaClock,
    MediaError, PlatformSurface, Surface, VideoDecoder,
};
use frames_media::client::{ClientConfig, ClientSession};
use frames_media::host::{HostConfig, HostSession};
use frames_media::rate::RateConfig;
use frames_testkit::capture::SyntheticCapture;
use frames_testkit::clock::VirtualClock;
use frames_testkit::codec::{EncoderModel, SyntheticDecoder, SyntheticEncoder};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Decoded surfaces alive now, and the most at once.
#[derive(Default)]
struct Pool {
    live: AtomicUsize,
    peak: AtomicUsize,
}

/// Stands in for a decoder pool surface: counted while it exists.
#[derive(Debug)]
struct Lease {
    pool: Arc<Pool>,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pool")
    }
}

impl Lease {
    fn new(pool: &Arc<Pool>) -> Self {
        let live = pool.live.fetch_add(1, Ordering::SeqCst) + 1;
        pool.peak.fetch_max(live, Ordering::SeqCst);
        Self { pool: pool.clone() }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.pool.live.fetch_sub(1, Ordering::SeqCst);
    }
}

impl PlatformSurface for Lease {
    fn width(&self) -> u32 {
        1920
    }
    fn height(&self) -> u32 {
        1080
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// The synthetic decoder, with each decoded frame holding a pool surface.
struct PoolDecoder {
    inner: SyntheticDecoder,
    pool: Arc<Pool>,
}

impl VideoDecoder for PoolDecoder {
    fn caps(&self) -> DecoderCaps {
        self.inner.caps()
    }

    fn decode(&mut self, unit: AccessUnit) -> Result<Decoded, MediaError> {
        Ok(match self.inner.decode(unit)? {
            Decoded::Frame(mut frame) => {
                frame.surface = Surface::Platform(Arc::new(Lease::new(&self.pool)));
                Decoded::Frame(frame)
            }
            other => other,
        })
    }
}

const BACKLOG: u64 = 12;

/// Encodes `BACKLOG` frames and delivers them all before the viewer polls
/// (a stall, then the burst).
fn backlog() -> (ClientSession, PoolDecoder, Arc<Pool>, u64) {
    let clock = VirtualClock::new(0);
    let rate = RateConfig {
        min_bps: 1_000_000,
        max_bps: 80_000_000,
        start_bps: 20_000_000,
    };
    let params = EncodeParams {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_bps: rate.start_bps,
        vbv_frames: 1.0,
        gop: Gop::Infinite,
        intra_refresh_frames: Some(6),
        slices: 1,
    };
    let mut capture = SyntheticCapture::new(&clock, 1920, 1080, 60);
    let mut encoder = SyntheticEncoder::new(&clock, params, EncoderModel::default());
    let mut host = HostSession::new(HostConfig::new(1, 1, rate));
    let mut client = ClientSession::new(ClientConfig::new(1, 1));
    let mut encoded = 0;
    while encoded < BACKLOG {
        let now = clock.now_us();
        if capture.next_due_us() <= now
            && let Ok(CaptureEvent::Frame(frame)) = capture.next(now)
        {
            host.encode(now, &mut encoder, &frame).unwrap();
            encoded += 1;
        }
        while let Some(datagram) = host.poll_transmit(now) {
            client.on_datagram(now, &datagram);
        }
        clock.advance_to(capture.next_due_us().max(now + 1));
    }
    // The host paces its sends: let them all out before the viewer polls.
    let end = clock.now_us() + 500_000;
    while clock.now_us() < end {
        let now = clock.now_us();
        while let Some(datagram) = host.poll_transmit(now) {
            client.on_datagram(now, &datagram);
        }
        clock.advance(1_000);
    }
    let now = clock.now_us();
    let pool = Arc::new(Pool::default());
    let decoder = PoolDecoder {
        inner: SyntheticDecoder::new(),
        pool: pool.clone(),
    };
    (client, decoder, pool, now)
}

#[test]
fn the_newest_frame_only_holds_one_surface_through_a_backlog() {
    let (mut client, mut decoder, pool, now) = backlog();
    let (latest, superseded) = client.poll_latest(now, &mut decoder);
    let latest = latest.expect("the backlog decodes");
    assert!(superseded >= 8, "a real backlog: {superseded} older frames");
    assert_eq!(latest.frame, superseded, "the newest frame is kept");
    // While a frame replaces the previous one, two surfaces exist; never
    // more, however long the backlog.
    assert!(
        pool.peak.load(Ordering::SeqCst) <= 2,
        "peak {} surfaces",
        pool.peak.load(Ordering::SeqCst)
    );
    assert_eq!(pool.live.load(Ordering::SeqCst), 1);
    drop(latest);
    assert_eq!(pool.live.load(Ordering::SeqCst), 0);
}

#[test]
fn poll_still_returns_every_decoded_frame() {
    // `poll` keeps its meaning (tests and recorders want every frame), and
    // holds every surface until the caller drops them: what ran a static
    // hardware pool out before viewers used `poll_latest`.
    let (mut client, mut decoder, pool, now) = backlog();
    let frames = client.poll(now, &mut decoder);
    assert!(frames.len() >= 9, "a real backlog: {} frames", frames.len());
    assert_eq!(pool.peak.load(Ordering::SeqCst), frames.len());
    drop(frames);
    assert_eq!(pool.live.load(Ordering::SeqCst), 0);
}
