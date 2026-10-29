use frames_core::{codec, media::RawFrame};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires a local FFmpeg executable; exercises actual encoder/decoder processes"]
async fn every_advertised_codec_roundtrips_a_live_window_frame() {
    let capabilities = codec::probe().await;
    assert!(
        !capabilities.is_empty(),
        "no encoder passed the runtime probe"
    );
    for capability in capabilities {
        eprintln!(
            "Testing {:?} with {:?}",
            capability.codec, capability.encoder
        );
        let mut pixels = vec![0; 128 * 96 * 4];
        for pixel in pixels.chunks_mut(4) {
            pixel.copy_from_slice(&[20, 40, 220, 255]);
        }
        let (tx, rx) = tokio::sync::watch::channel(Some(RawFrame {
            width: 128,
            height: 96,
            bgra: pixels,
        }));
        let mut encoder = codec::encode(128, 96, 30, &capability, rx).await.unwrap();
        let mut decoder = codec::decode(capability.codec, 128, 96).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let mut count = 0;
        let result = loop {
            tokio::select! {
                _=tokio::time::sleep_until(deadline)=>break None,
                frame=encoder.frames.recv()=>{let frame=frame.unwrap_or_else(||panic!("{:?} encoder {:?} stopped",capability.codec,capability.encoder));count+=1;decoder.input.send(frame).await.unwrap();},
                result=decoder.output.changed()=>{result.unwrap();let frame=decoder.output.borrow().clone();if frame.is_some(){break frame;}}
            }
        };
        encoder.input_task.abort();
        let _ = encoder.child.kill().await;
        let _ = decoder.child.kill().await;
        drop(tx);
        let frame = result.unwrap_or_else(|| {
            panic!(
                "{:?} failed live roundtrip after {count} encoded frames",
                capability.codec
            )
        });
        assert_eq!((frame.width, frame.height), (128, 96));
        let pixel = &frame.bgra[(48 * 128 + 64) * 4..][..4];
        assert!(
            pixel[2] > 150 && pixel[0] < 80,
            "unexpected decoded pixels for {:?}: {pixel:?}",
            capability.codec
        );
    }
}
