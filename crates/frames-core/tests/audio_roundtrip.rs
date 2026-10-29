use frames_core::audio::{self, PcmFormat};
#[tokio::test]
#[ignore = "requires FFmpeg with libopus; does not record or play any device audio"]
async fn opus_stream_roundtrips_generated_pcm() {
    let format = PcmFormat {
        rate: 48000,
        channels: 2,
    };
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let mut encoder = audio::encode(format, rx).await.unwrap();
    let mut decoder = audio::decode(format).await.unwrap();
    let source = tokio::spawn(async move {
        for packet in 0..100 {
            let mut pcm = vec![];
            for frame in 0..960 {
                let sample =
                    (((packet * 960 + frame) as f32 * 440. * std::f32::consts::TAU / 48000.).sin()
                        * 0.2)
                        .to_le_bytes();
                pcm.extend(sample);
                pcm.extend(sample);
            }
            if tx.send(pcm).await.is_err() {
                break;
            }
        }
    });
    let end = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut result = None;
    loop {
        tokio::select! {_=tokio::time::sleep_until(end)=>break,page=encoder.pages.recv()=>{if let Some(page)=page{if decoder.input.send(page).await.is_err(){break;}}else{break;}},pcm=decoder.pcm.recv()=>{result=pcm;break;}}
    }
    source.abort();
    encoder.writer.abort();
    let _ = encoder.child.kill().await;
    let _ = decoder.child.kill().await;
    let pcm = result.expect("no decoded Opus audio");
    assert_eq!(pcm.len(), 48000 * 2 * 4 / 50);
    assert!(
        pcm.chunks_exact(4)
            .map(|unit| f32::from_le_bytes(unit.try_into().unwrap()).abs())
            .any(|v| v > 0.05)
    );
}
