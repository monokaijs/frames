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
                width: width as u32,
                height: height as u32,
                bgra: pixels,
            }));
            let mut encoder = codec::encode(width as u32, height as u32, 30, &capability, rx)
                .await
                .unwrap();
            let mut decoder = codec::decode(capability.codec, width as u32, height as u32)
                .await
                .unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
            let mut count = 0;
            let mut decoded = 0;
            let result = loop {
                tokio::select! {
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
