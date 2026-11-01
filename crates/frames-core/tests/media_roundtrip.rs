use frames_core::{codec, media::RawFrame};
use std::time::Duration;

// These tests allocate multiple native encoder/decoder sessions. Run them
// exclusively so a limited hardware codec service is not oversubscribed.
static CODEC_SESSIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(target_os = "macos")]
#[tokio::test]
async fn videotoolbox_focus_rate_changes_preserve_the_encoder_clock() {
    use std::sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    };
    let _sessions = CODEC_SESSIONS.lock().await;
    let capability = codec::probe()
        .await
        .into_iter()
        .find(|c| c.encoder.as_deref() == Some("h264_videotoolbox"))
        .unwrap();
    let (_capture, frames) = tokio::sync::watch::channel(Some(RawFrame {
        media_generation: 0,
        captured_at: std::time::Instant::now(),
        frame_number: 0,
        width: 256,
        height: 256,
        bgra: [20, 30, 220, 255].repeat(256 * 256).into(),
    }));
    let rate = Arc::new(AtomicU32::new(24));
    let mut encoder = codec::encode_paced(256, 256, 60, &capability, frames, rate.clone())
        .await
        .unwrap();
    encoder.frames.recv().await.unwrap();
    for fps in [24, 24, 60, 24, 60, 45, 60] {
        rate.store(fps, Ordering::Relaxed);
        encoder.request_keyframe();
        let packet = tokio::time::timeout(Duration::from_secs(3), encoder.frames.recv())
            .await
            .unwrap()
            .expect("focus change stopped VideoToolbox");
        assert!(packet.keyframe);
    }
    encoder.shutdown().await;
}

#[tokio::test]
async fn recovery_keyframes_reuse_the_encoder_and_need_no_new_capture() {
    let _sessions = CODEC_SESSIONS.lock().await;
    for capability in codec::probe().await {
        let (_capture, frames) = tokio::sync::watch::channel(Some(RawFrame {
            media_generation: 0,
            #[cfg(windows)]
            gpu: None,
            captured_at: std::time::Instant::now(),
            frame_number: 0,
            width: 128,
            height: 128,
            bgra: [20, 30, 220, 255].repeat(128 * 128).into(),
        }));
        let mut encoder = codec::encode(128, 128, 60, &capability, frames)
            .await
            .unwrap();
        let worker = encoder.worker.id();
        encoder.frames.recv().await.unwrap();
        for _ in 0..3 {
            encoder.request_keyframe();
            let packet = tokio::time::timeout(Duration::from_secs(3), encoder.frames.recv())
                .await
                .expect("keyframe request must wake an idle encoder")
                .unwrap();
            assert!(
                codec::is_keyframe(capability.codec, &packet, false),
                "{:?} did not force a keyframe",
                capability.encoder
            );
            assert_eq!(encoder.worker.id(), worker);
            // Recovery must be independently decodable after packet loss.
            let mut decoder = codec::decode(capability.codec, 128, 128).await.unwrap();
            decoder.input.send(packet).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), decoder.output.changed())
                .await
                .unwrap()
                .unwrap();
            assert!(decoder.output.borrow().is_some());
            decoder.shutdown().await;
        }
        encoder.shutdown().await;
    }
}

#[tokio::test]
async fn receiving_client_probes_all_decoders_without_an_encoder() {
    let capabilities = codec::probe_decoders().await;
    assert_eq!(
        capabilities.len(),
        4,
        "reference frames must decode through FFmpeg"
    );
    assert!(
        capabilities
            .iter()
            .all(|c| c.decoder && c.encoder.is_none() && !c.hardware)
    );
}

#[tokio::test]
async fn every_advertised_codec_roundtrips_a_live_window_frame() {
    let _sessions = CODEC_SESSIONS.lock().await;
    let capabilities = codec::probe().await;
    assert!(
        !capabilities.is_empty(),
        "no encoder passed the runtime probe"
    );
    for capability in capabilities {
        for (width, height) in [(128, 96), (129, 97)] {
            eprintln!(
                "Testing {:?} with {:?}",
                capability.codec, capability.encoder
            );
            let mut pixels = vec![0; width * height * 4];
            for (index, pixel) in pixels.chunks_mut(4).enumerate() {
                let (x, y) = (index % width, index / width);
                pixel.copy_from_slice(if y < height / 4 {
                    &[220, 30, 20, 255]
                } else if x < width / 4 {
                    &[20, 220, 30, 255]
                } else {
                    &[20, 30, 220, 255]
                });
            }
            let (tx, rx) = tokio::sync::watch::channel(Some(RawFrame {
                media_generation: 0,
                #[cfg(windows)]
                gpu: None,
                captured_at: std::time::Instant::now(),
                frame_number: 0,
                width: width as u32,
                height: height as u32,
                bgra: pixels.into(),
            }));
            let mut encoder = codec::encode(width as u32, height as u32, 30, &capability, rx)
                .await
                .unwrap();
            let mut decoder = codec::decode(capability.codec, width as u32, height as u32)
                .await
                .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
            let mut source = tokio::time::interval(Duration::from_millis(33));
            source.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut count = 0;
            let mut decoded = 0;
            let result = loop {
                tokio::select! {
                    _=source.tick()=>{ let frame = tx.borrow().clone(); tx.send_replace(frame); },
                    _=tokio::time::sleep_until(deadline)=>break None,
                    frame=encoder.frames.recv()=>{let frame=frame.unwrap_or_else(||panic!("{:?} encoder {:?} stopped",capability.codec,capability.encoder));count+=1;decoder.input.send(frame).await.unwrap();},
                    result=decoder.output.changed()=>{result.unwrap();let frame=decoder.output.borrow().clone();if let Some(frame)=frame {
                        assert_eq!((frame.width,frame.height),(width as u32,height as u32));
                        for (x,y,channel) in [(width/2,height/8,0),(width/8,height/2,1),(width/2,height/2,2),(width-8,height-8,2)] {
                            let pixel=&frame.bgra[(y*width+x)*4..][..4];
                            assert!(pixel[channel]>140 && pixel[(channel+1)%3]<100 && pixel[(channel+2)%3]<100,"{:?} {width}x{height} decoded frame {decoded} misaligned at {x},{y}: {pixel:?}",capability.codec);
                        }
                        decoded+=1;if decoded>=12 {break Some(frame);}
                    }}
                }
            };
            encoder.shutdown().await;
            decoder.shutdown().await;
            drop(tx);
            let frame = result.unwrap_or_else(|| {
                panic!(
                    "{:?} failed live roundtrip after {count} encoded frames",
                    capability.codec
                )
            });
            assert_eq!((frame.width, frame.height), (width as u32, height as u32));
            let pixel = &frame.bgra[((height / 2) * width + width / 2) * 4..][..4];
            assert!(
                pixel[2] > 150 && pixel[0] < 80,
                "unexpected decoded pixels for {:?}: {pixel:?}",
                capability.codec
            );
        }
    }
}

#[tokio::test]
async fn one_capture_encodes_and_decodes_without_a_later_capture_or_eof() {
    let _sessions = CODEC_SESSIONS.lock().await;
    for capability in codec::probe().await {
        let codec = capability.codec;
        eprintln!("Single capture: {codec:?} with {:?}", capability.encoder);
        let (tx, rx) = tokio::sync::watch::channel(Some(RawFrame {
            media_generation: 0,
            #[cfg(windows)]
            gpu: None,
            captured_at: std::time::Instant::now(),
            frame_number: 0,
            width: 128,
            height: 128,
            bgra: [20, 30, 220, 255].repeat(128 * 128).into(),
        }));
        let mut encoder = codec::encode(128, 128, 60, &capability, rx).await.unwrap();
        let packet = tokio::time::timeout(Duration::from_secs(3), encoder.frames.recv())
            .await
            .expect("encoder waited for another capture")
            .expect("encoder stopped");
        let mut decoder = codec::decode(codec, 128, 128).await.unwrap();
        decoder.input.send(packet).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), decoder.output.changed())
            .await
            .expect("decoder waited for another capture")
            .unwrap();
        let frame = decoder.output.borrow().clone().unwrap();
        assert!(frame.bgra[2] > 150 && frame.bgra[0] < 80);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), encoder.frames.recv())
                .await
                .is_err(),
            "unchanged capture was encoded repeatedly"
        );
        encoder.shutdown().await;
        decoder.shutdown().await;
        drop(tx);
    }
}
