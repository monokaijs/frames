//! Synthetic in-process codec timing. No capture, input injection, or device audio.
use frames_core::{
    codec::{self, Codec, CodecCapability},
    media::RawFrame,
};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let capabilities = codec::probe().await;
    let default = codec::negotiate(&capabilities, &capabilities, None)?;
    let name = args
        .get(1)
        .map(String::as_str)
        .unwrap_or(default.encoder.as_deref().unwrap());
    let width: u32 = args.get(2).map(|v| v.parse()).transpose()?.unwrap_or(1920);
    let height: u32 = args.get(3).map(|v| v.parse()).transpose()?.unwrap_or(1080);
    let codec = if name.starts_with("hevc") || name == "libx265" {
        Codec::Hevc
    } else if name.starts_with("av1") || name == "libaom-av1" {
        Codec::Av1
    } else if name == "libvpx-vp9" {
        Codec::Vp9
    } else {
        Codec::H264
    };
    let cap = CodecCapability {
        codec,
        encoder: Some(name.into()),
        decoder: true,
        hardware: !name.starts_with("lib"),
    };
    let (source, captures) = tokio::sync::watch::channel(None);
    let mut encoder = codec::encode(width, height, 60, &cap, captures).await?;
    let mut decoder = codec::decode(codec, width, height).await?;
    let mut samples = Vec::new();
    let result = async {
        for index in 0..40 {
            let red = index % 2 == 0;
            let pixel = if red { [20, 30, 220, 255] } else { [20, 220, 30, 255] };
            let frame = RawFrame { media_generation: 0, #[cfg(windows)] gpu: None, captured_at: std::time::Instant::now(), frame_number: 0, width, height, bgra: pixel.repeat((width * height) as usize).into() };
            let start = Instant::now();
            source.send_replace(Some(frame));
            let packet = tokio::time::timeout(Duration::from_secs(5), encoder.frames.recv()).await?.ok_or_else(|| anyhow::anyhow!("encoder stopped"))?;
            let encoded = start.elapsed().as_micros() as u64;
            decoder.input.send(packet).await?;
            tokio::time::timeout(Duration::from_secs(5), decoder.output.changed()).await??;
            let elapsed = start.elapsed().as_micros() as u64;
            let frame = decoder.output.borrow().clone().unwrap();
            let center = ((height / 2 * width + width / 2) * 4) as usize;
            anyhow::ensure!(frame.bgra[center + if red { 2 } else { 1 }] > 150, "stale or incorrect pixels");
            samples.push(serde_json::json!({"encode_us":encoded,"decode_us":elapsed-encoded,"total_us":elapsed}));
            tokio::time::sleep(Duration::from_millis(35)).await;
        }
        let percentile = |field: &str, percent: usize| {
            let mut values: Vec<_> = samples.iter().skip(1).map(|s| s[field].as_u64().unwrap()).collect();
            values.sort_unstable();
            values[(values.len() * percent).div_ceil(100).saturating_sub(1)] as f64 / 1000.0
        };
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"encoder":name,"width":width,"height":height,"sample_count":samples.len(),"warmup_samples":1,"encode_p50_ms":percentile("encode_us",50),"encode_p95_ms":percentile("encode_us",95),"decode_p50_ms":percentile("decode_us",50),"decode_p95_ms":percentile("decode_us",95),"total_p50_ms":percentile("total_us",50),"total_p95_ms":percentile("total_us",95),"samples":samples,"scope":"generated flat pixels through live in-process codec workers; excludes capture, networking, native presentation, and panel"}))?);
        Ok::<_,anyhow::Error>(())
    }.await;
    encoder.shutdown().await;
    decoder.shutdown().await;
    result
}
