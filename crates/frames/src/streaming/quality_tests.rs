use super::*;
use frames_core::{adaptive::QualityPreset, input::InputEvent, model::*};

async fn connection_pair() -> (
    quinn::Endpoint,
    quinn::Endpoint,
    quinn::Connection,
    quinn::Connection,
) {
    use frames_core::{
        transport,
        trust::{Identity, fingerprint},
    };
    fn identity() -> Identity {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["frames.local".into()]).unwrap();
        let certificate = cert.der().clone();
        Identity {
            device_id: fingerprint(&certificate),
            certificate,
            key: rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
        }
    }
    let server = transport::server(&identity(), "127.0.0.1:0".parse().unwrap()).unwrap();
    let client = transport::client(&identity()).unwrap();
    let connecting = client
        .connect(server.local_addr().unwrap(), "frames.local")
        .unwrap();
    let (viewer, host) = tokio::join!(connecting, async { server.accept().await.unwrap().await });
    (server, client, host.unwrap(), viewer.unwrap())
}

struct IdleCapture {
    _sender: watch::Sender<Option<media::RawFrame>>,
}
impl frames_native::Capture for IdleCapture {
    fn stop(&mut self) {}
}
struct IdleDesktop;
impl Desktop for IdleDesktop {
    fn windows(&self) -> anyhow::Result<Vec<RemoteWindow>> {
        Ok(Vec::new())
    }
    fn action(&self, _: WindowId, _: &WindowAction) -> anyhow::Result<()> {
        Ok(())
    }
    fn input(&self, _: WindowId, _: &InputEvent) -> anyhow::Result<()> {
        Ok(())
    }
    fn release_input(&self) {}
    fn capture(&self, _: WindowId, _: bool) -> anyhow::Result<frames_native::CaptureStream> {
        let (sender, frames) = watch::channel(Some(media::RawFrame {
            media_generation: 0,
            #[cfg(windows)]
            gpu: None,
            captured_at: std::time::Instant::now(),
            frame_number: 0,
            width: 640,
            height: 360,
            bgra: [20, 30, 220, 255].repeat(640 * 360).into(),
        }));
        Ok(frames_native::CaptureStream {
            frames,
            guard: Box::new(IdleCapture { _sender: sender }),
        })
    }
    fn clipboard(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn set_clipboard(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn repeated_quality_switches_deliver_idle_pixels_through_the_media_pipeline() {
    let (_server, _client, host, viewer) = connection_pair().await;
    let window = WindowId {
        id: 414,
        generation: 1,
    };
    let capabilities = codec::probe().await;
    assert!(!capabilities.is_empty());
    let sender = crate::media_sender::MediaSender::new(host.clone());
    let generations = Arc::new(AtomicU32::new(1));
    for capability in capabilities {
        let (mut out, mut host_messages) = crate::session::test_outbound();
        out.media = Some(sender.clone());
        let (client_out, mut client_messages) = crate::session::test_outbound();
        let (ui, updates) = frames_native::client_channel(16);
        let client_media = ClientMediaWorker::new(viewer.clone(), ui, client_out);
        let mut stream = start_host(
            Arc::new(IdleDesktop),
            window,
            capability.clone(),
            VideoOptions {
                quality: QualityPreset::High,
                fps: 60,
                show_cursor: false,
                bandwidth_mbps: 0,
            },
            host.clone(),
            out,
            generations.clone(),
        );
        let mut previous = None;
        for preset in [
            QualityPreset::High,
            QualityPreset::Low,
            QualityPreset::Medium,
            QualityPreset::High,
            QualityPreset::Low,
            QualityPreset::High,
        ] {
            if previous.is_some() {
                stream.set_quality(preset);
            }
            let expected = if preset == QualityPreset::Low {
                (480, 270)
            } else {
                (640, 360)
            };
            let result = tokio::time::timeout(Duration::from_secs(8), async {
                'picture: loop {
                    tokio::select! {
                        message = host_messages.recv() => match message.unwrap() {
                            Message::StreamStarted(config) => { client_media.send(MediaCommand::Start(config)).await.unwrap(); },
                            Message::StreamStopped { window, generation } => { client_media.send(MediaCommand::Stop(window, generation)).await.unwrap(); },
                            Message::MediaStatus { window, generation, number } => { client_media.send(MediaCommand::Status(window, generation, number)).await.unwrap(); },
                            Message::Error { message } => panic!("{message}"),
                            _ => {},
                        },
                        message = client_messages.recv() => match message.unwrap() {
                            Message::StreamReady { generation, .. } => { stream.acknowledge(generation); },
                            Message::Keyframe { generation, .. } => stream.request_keyframe(generation),
                            _ => {},
                        },
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {},
                    }
                    while let Ok(update) = updates.try_recv() {
                        if let ClientUpdate::Frame(_, frame) = update {
                            let identity = (frame.media_generation, frame.frame_number);
                            if (frame.width, frame.height) == expected && previous.is_none_or(|old| identity > old) {
                                #[cfg_attr(not(windows), allow(unused_mut))] // GPU readback below is Windows-only.
                                let mut pixels = frame.bgra.to_vec();
                                #[cfg(windows)]
                                if let Some(gpu) = &frame.gpu {
                                    pixels.resize(frame.width as usize * frame.height as usize * 4, 0);
                                    codec::GpuConverter::new(&gpu.texture, frame.width, frame.height).unwrap()
                                        .readback(gpu, frame.width, frame.height, &mut pixels).unwrap();
                                }
                                assert!(pixels[2] > 150 && pixels[0] < 80);
                                break 'picture identity;
                            }
                        }
                    }
                }
            }).await;
            previous = Some(result.unwrap_or_else(|_| {
                panic!("{:?} froze switching to {preset:?}", capability.encoder)
            }));
        }
        stream.shutdown().await;
        drop(client_media);
    }
}

#[tokio::test]
async fn native_startup_silence_returns_to_the_fallback_instead_of_freezing() {
    let (_server, _client, host, _viewer) = connection_pair().await;
    let (mut out, _messages) = crate::session::test_outbound();
    out.media = Some(crate::media_sender::MediaSender::new(host.clone()));
    let (_packets, packets) = tokio::sync::mpsc::channel(1);
    let (control, _commands) = std::sync::mpsc::sync_channel(4);
    let (delivery, _delivered) = std::sync::mpsc::sync_channel(1);
    let (raw, _) = watch::channel(None);
    let capture = frames_native::NativeEncodedCapture {
        packets,
        control,
        delivery,
        guard: Box::new(IdleCapture { _sender: raw }),
    };
    let capability = CodecCapability {
        codec: codec::Codec::H264,
        encoder: Some("fixture".into()),
        decoder: true,
        hardware: true,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        run_native_host(
            capture,
            WindowId {
                id: 415,
                generation: 1,
            },
            &capability,
            &host,
            &out,
            &AtomicU32::new(1),
            &AtomicU32::new(0),
            &AtomicU32::new(60),
            &StreamTuning::default(),
            &mut watch::channel(0).1,
            &mut watch::channel(0).1,
        ),
    )
    .await
    .expect("native startup waited indefinitely");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("no initial picture")
    );
}

#[tokio::test]
async fn native_quality_refresh_silence_is_bounded_after_a_successful_picture() {
    let (_server, _client, host, _viewer) = connection_pair().await;
    let (mut out, mut messages) = crate::session::test_outbound();
    out.media = Some(crate::media_sender::MediaSender::new(host.clone()));
    let (packets_tx, packets) = tokio::sync::mpsc::channel(1);
    let now = std::time::Instant::now();
    packets_tx
        .send(frames_native::NativeEncodedFrame {
            width: 640,
            height: 360,
            source_width: 640,
            source_height: 360,
            packet: codec::EncodedPacket {
                surface_encoded: true,
                encode_micros: 1,
                generation: 0,
                number: 0,
                captured_at: now,
                encoded_at: now,
                keyframe: true,
                bytes: vec![1; 100],
            },
        })
        .await
        .unwrap();
    let (control, _commands) = std::sync::mpsc::sync_channel(4);
    let (delivery, delivered) = std::sync::mpsc::sync_channel(1);
    let (raw, _) = watch::channel(None);
    let capture = frames_native::NativeEncodedCapture {
        packets,
        control,
        delivery,
        guard: Box::new(IdleCapture { _sender: raw }),
    };
    let capability = CodecCapability {
        codec: codec::Codec::H264,
        encoder: Some("fixture".into()),
        decoder: true,
        hardware: true,
    };
    let (reset, mut reset_rx) = watch::channel(0);
    let (_ready, mut ready) = watch::channel(1);
    let generations = AtomicU32::new(1);
    let active = AtomicU32::new(0);
    let rate = AtomicU32::new(60);
    let tuning = StreamTuning::default();
    let native = run_native_host(
        capture,
        WindowId {
            id: 416,
            generation: 1,
        },
        &capability,
        &host,
        &out,
        &generations,
        &active,
        &rate,
        &tuning,
        &mut ready,
        &mut reset_rx,
    );
    let request_refresh = async {
        while let Some(message) = messages.recv().await {
            if matches!(message, Message::MediaStatus { number: 0, .. }) {
                assert_eq!(delivered.try_recv().unwrap().number, 0);
                // Simulate a live quality switch whose native backend then
                // accepts commands but never produces another access unit.
                reset.send_replace(1);
                return;
            }
        }
        panic!("native backend never delivered its first picture");
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(native, request_refresh)
    })
    .await
    .expect("native refresh waited indefinitely");
    assert!(result.unwrap_err().to_string().contains("no refresh"));
}
