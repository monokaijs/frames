//! FEC processing cost only. Does not measure network or display latency.
use frames_core::{media::*, model::WindowId};
use std::time::{Duration, Instant};
fn main() -> anyhow::Result<()> {
    let count: usize = std::env::args()
        .nth(1)
        .map(|s| s.parse())
        .transpose()?
        .unwrap_or(1000);
    anyhow::ensure!(count > 0, "sample count must be positive");
    prewarm_fec();
    let mut output = Vec::new();
    for length in [25_000, 90_000, 250_000] {
        let mut encode = Vec::new();
        let mut reconstruct = Vec::new();
        let mut wire = 0;
        let mut receiver = Reassembler::default();
        for number in 0..count + 16 {
            let frame = EncodedFrame {
                window: WindowId {
                    id: 1,
                    generation: 1,
                },
                generation: 1,
                number: number as u64,
                keyframe: false,
                captured_us: 0,
                encoded_us: 0,
                bytes: vec![37; length],
            };
            let at = Instant::now();
            let packets = fragment(&frame, 1200)?;
            let encoded = at.elapsed().as_nanos() as u64;
            let at = Instant::now();
            let mut result = None;
            for p in &packets {
                if p[46] != 0 {
                    result = receiver.push(p, at).or(result);
                }
            }
            let decoded = at.elapsed().as_nanos() as u64;
            anyhow::ensure!(
                result.is_some_and(|f| f.bytes == frame.bytes),
                "failed reconstruction"
            );
            receiver.expire(at + Duration::from_millis(51));
            wire = packets.iter().map(Vec::len).sum::<usize>();
            if number >= 16 {
                encode.push(encoded);
                reconstruct.push(decoded);
            }
        }
        encode.sort_unstable();
        reconstruct.sort_unstable();
        let p = |values: &Vec<u64>, percent: usize| {
            values[(count * percent).div_ceil(100) - 1] as f64 / 1_000_000.0
        };
        output.push(serde_json::json!({"frame_bytes":length,"wire_bytes":wire,"samples":count,"encode_p50_ms":p(&encode,50),"encode_p95_ms":p(&encode,95),"encode_p99_ms":p(&encode,99),"reconstruct_p50_ms":p(&reconstruct,50),"reconstruct_p95_ms":p(&reconstruct,95),"reconstruct_p99_ms":p(&reconstruct,99)}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"scope":"FEC packetization and reassembly with one source-shard erasure per block; 16 warmup frames; no network or display","results":output})
        )?
    );
    Ok(())
}
