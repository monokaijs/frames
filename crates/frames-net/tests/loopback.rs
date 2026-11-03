//! The v2 engine over real QUIC (iroh) on loopback.

use frames_media::api::{CaptureEvent, CaptureSource, EncodeParams, Gop, MediaTransport};
use frames_media::client::{ClientConfig, ClientSession};
use frames_media::host::{HostConfig, HostSession};
use frames_media::rate::RateConfig;
use frames_net::migration;
use frames_net::{DeviceKey, Node, NodeConfig, Relays};
use frames_testkit::capture::SyntheticCapture;
use frames_testkit::clock::VirtualClock;
use frames_testkit::codec::{EncoderModel, SyntheticDecoder, SyntheticEncoder};
use frames_testkit::netsim::percentile;
use iroh::EndpointAddr;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

fn local() -> NodeConfig {
    NodeConfig {
        relays: Relays::Disabled,
        discovery: false,
        bind: Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))),
        ..NodeConfig::default()
    }
}

async fn pair() -> (Node, Node, frames_net::Session, frames_net::Session) {
    let host = Node::bind(&DeviceKey::generate(), local()).await.unwrap();
    let client = Node::bind(&DeviceKey::generate(), local()).await.unwrap();
    let port = host
        .bound_sockets()
        .into_iter()
        .find(SocketAddr::is_ipv4)
        .unwrap()
        .port();
    let address =
        EndpointAddr::new(host.id()).with_ip_addr(SocketAddr::from((Ipv4Addr::LOCALHOST, port)));
    let (outgoing, incoming) = tokio::join!(client.connect(address), host.accept());
    (host, client, incoming.unwrap().unwrap(), outgoing.unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_agree_on_pairing_codes_and_carry_v1_trust() {
    let (_host, _client, at_host, at_client) = pair().await;
    assert_eq!(
        at_host.pairing_code().unwrap(),
        at_client.pairing_code().unwrap()
    );
    assert_eq!(at_host.pairing_code().unwrap().len(), 8);
    assert_eq!(at_host.remote(), _client.id());

    // The client proves its v1 identity; the host verifies it against this
    // connection's exporter.
    let legacy = rcgen::KeyPair::generate().unwrap();
    let certificate = rcgen::CertificateParams::new(vec!["frames.local".into()])
        .unwrap()
        .self_signed(&legacy)
        .unwrap();
    let exporter = at_client.exporter(migration::EXPORTER_LABEL).unwrap();
    let proof = migration::prove(
        certificate.der(),
        &legacy.serialize_der(),
        &_client.id(),
        &exporter,
    )
    .unwrap();
    let host_exporter = at_host.exporter(migration::EXPORTER_LABEL).unwrap();
    assert_eq!(
        exporter, host_exporter,
        "both ends derive the same exporter"
    );
    assert_eq!(
        migration::verify(&proof, &at_host.remote(), &host_exporter).unwrap(),
        migration::fingerprint(certificate.der())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_media_engine_streams_over_real_quic() {
    let (_host, _client, at_host, at_client) = pair().await;
    let host_media = at_host.media();
    let client_media = at_client.media();

    let rate = RateConfig {
        min_bps: 1_000_000,
        max_bps: 40_000_000,
        start_bps: 10_000_000,
    };
    let mut config = HostConfig::new(1, 1, rate);
    config.max_datagram = host_media.path().max_datagram.min(1200);
    let mut host = HostSession::new(config);
    let mut client_config = ClientConfig::new(1, 1);
    client_config.max_datagram = client_media.path().max_datagram.min(1200);
    let mut client = ClientSession::new(client_config);

    // Synthetic capture and codec on a virtual clock that follows real time.
    let clock = VirtualClock::new(0);
    let started = Instant::now();
    let real_now = || started.elapsed().as_micros() as u64;
    let mut capture = SyntheticCapture::new(&clock, 1920, 1080, 60);
    let params = EncodeParams {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_bps: rate.start_bps,
        vbv_frames: 1.0,
        gop: Gop::Infinite,
        intra_refresh_frames: None,
        slices: 2,
    };
    let mut encoder = SyntheticEncoder::new(&clock, params, EncoderModel::default());
    let mut decoder = SyntheticDecoder::new();
    let mut captured = 0u64;
    let mut send_failures = 0u64;

    while started.elapsed() < Duration::from_secs(3) {
        clock.advance_to(real_now());
        let now = real_now();
        let rtt = host_media.path().rtt_us;
        client.set_rtt(rtt);
        if capture.next_due_us() <= now
            && let Ok(CaptureEvent::Frame(frame)) = capture.next(now)
        {
            captured += 1;
            host.encode(now, &mut encoder, &frame).unwrap();
            host_media.set_target_rate(u64::from(host.target_bps()));
        }
        while let Some(datagram) = host.poll_transmit(now) {
            if host_media.try_send(datagram).is_err() {
                send_failures += 1;
            }
        }
        while let Some(datagram) = client_media.try_recv() {
            client.on_datagram(real_now(), &datagram);
        }
        client.poll(real_now(), &mut decoder);
        if let Some(feedback) = client.poll_feedback(real_now()) {
            let _ = client_media.try_send(feedback);
        }
        while let Some(datagram) = host_media.try_recv() {
            host.on_datagram(real_now(), &datagram, rtt);
        }
        tokio::time::sleep(Duration::from_micros(500)).await;
    }

    let stats = client.stats();
    let latency_p95 = percentile(&stats.latency_us, 95);
    println!(
        "real QUIC loopback: captured {captured} clean {} lost {} keyframes {} p50 {:.1} ms p95 {:.1} ms target {:.1} Mbps send failures {send_failures}",
        stats.frames_clean,
        stats.frames_lost,
        host.stats().keyframes,
        percentile(&stats.latency_us, 50) as f64 / 1e3,
        latency_p95 as f64 / 1e3,
        f64::from(host.target_bps()) / 1e6,
    );
    assert_eq!(stats.decoder_broken, 0);
    assert_eq!(host.stats().keyframes, 1, "{:?}", host.stats());
    assert!(stats.frames_clean * 100 >= captured * 95, "{stats:?}");
    // Loopback, synthetic 3 ms encode, scheduler sleeps of 0.5 ms. Debug
    // builds run QUIC and crypto unoptimized, alongside other tests.
    let bound_us = if cfg!(debug_assertions) {
        40_000
    } else {
        15_000
    };
    assert!(latency_p95 <= bound_us, "p95 {latency_p95} µs");
}
