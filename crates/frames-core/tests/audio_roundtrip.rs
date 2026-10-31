use frames_core::audio::{self, PcmFormat};
use std::time::Duration;

#[tokio::test]
async fn one_ten_millisecond_packet_decodes_without_more_input_or_eof() {
    let format = PcmFormat {
        rate: 48000,
        channels: 2,
    };
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let mut encoder = audio::encode(format, rx).await.unwrap();
    let mut decoder = audio::decode(format).await.unwrap();
    let pcm: Vec<u8> = (0..480)
        .flat_map(|i| {
            let value = (i as f32 * 440. * std::f32::consts::TAU / 48000.).sin() * 0.2;
            [value, value].into_iter().flat_map(f32::to_le_bytes)
        })
        .collect();
    tx.send(pcm).await.unwrap();
    let packet = tokio::time::timeout(Duration::from_secs(2), encoder.packets.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(!packet.starts_with(b"OggS"));
    decoder.input.send(packet).await.unwrap();
    let pcm = tokio::time::timeout(Duration::from_secs(2), decoder.pcm.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pcm.len(), audio::packet_bytes(format));
    assert!(
        pcm.chunks_exact(4)
            .any(|v| f32::from_le_bytes(v.try_into().unwrap()).abs() > 0.05)
    );
    // Both input senders remain open: stopping idle workers must wake their waits.
    tokio::time::timeout(Duration::from_secs(2), encoder.shutdown())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), decoder.shutdown())
        .await
        .unwrap();
    drop(tx);
}

#[tokio::test]
async fn device_rates_and_channel_counts_resample_without_drift() {
    for (rate, channels) in [(44100, 1), (96000, 6), (192000, 8)] {
        let format = PcmFormat { rate, channels };
        let output = PcmFormat {
            rate: 44100,
            channels: 2,
        };
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let mut encoder = audio::encode(format, rx).await.unwrap();
        let mut decoder = audio::decode(output).await.unwrap();
        let source = tokio::spawn(async move {
            for block in 0..24 {
                let pcm: Vec<u8> = (0..rate / 100)
                    .flat_map(|i| {
                        let value =
                            ((block * rate / 100 + i) as f32 * 440. * std::f32::consts::TAU
                                / rate as f32)
                                .sin()
                                * 0.2;
                        std::iter::repeat_n(value, channels as usize).flat_map(f32::to_le_bytes)
                    })
                    .collect();
                if tx.send(pcm).await.is_err() {
                    break;
                }
            }
        });
        let mut count = 0;
        let mut peak = 0f32;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
        loop {
            tokio::select! {
                _=tokio::time::sleep_until(deadline)=>panic!("Resampling stalled for {rate}/{channels}"),
                Some(packet)=encoder.packets.recv()=>{decoder.input.send(packet).await.unwrap();},
                pcm=decoder.pcm.recv()=>{
                    let pcm=pcm.unwrap();count+=pcm.len()/8;
                    for value in pcm.chunks_exact(4){let value=f32::from_le_bytes(value.try_into().unwrap());assert!(value.is_finite());peak=peak.max(value.abs());}
                    if count>=441*20 {break;}
                }
            }
        }
        assert!(peak > 0.05);
        assert!(count <= 441 * 21, "Resampler produced excess samples");
        source.abort();
        encoder.shutdown().await;
        decoder.shutdown().await;
    }
}
#[tokio::test]
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
        tokio::select! {_=tokio::time::sleep_until(end)=>break,page=encoder.packets.recv()=>{if let Some(page)=page{if decoder.input.send(page).await.is_err(){break;}}else{break;}},pcm=decoder.pcm.recv()=>{result=pcm;break;}}
    }
    source.abort();
    encoder.shutdown().await;
    decoder.shutdown().await;
    let pcm = result.expect("no decoded Opus audio");
    assert_eq!(pcm.len(), audio::packet_bytes(format));
    assert!(
        pcm.as_chunks::<4>()
            .0
            .iter()
            .map(|unit| f32::from_le_bytes(*unit).abs())
            .any(|v| v > 0.05)
    );
}
