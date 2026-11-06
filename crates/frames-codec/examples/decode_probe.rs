//! Decode an elementary keyframe to PPM to compare CPU and hardware pixels.
use frames_core::codec::runtime::Context;
use std::io::Write;

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let width: u32 = args[2].parse()?;
    let height: u32 = args[3].parse()?;
    let mut packet = vec![1];
    packet.extend(std::fs::read(&args[1])?);
    for hardware in [false, true] {
        let mut decoder = Context::decoder("hevc", width, height, hardware)?;
        if let Some(frame) = decoder.decode(&packet)? {
            let mut out = std::fs::File::create(format!("{}-{hardware}.ppm", args[4]))?;
            write!(out, "P6\n{} {}\n255\n", frame.width, frame.height)?;
            let rgb: Vec<u8> = frame
                .bgra
                .chunks_exact(4)
                .flat_map(|p| [p[2], p[1], p[0]])
                .collect();
            out.write_all(&rgb)?;
            println!("hardware={hardware}: {}x{}", frame.width, frame.height);
        } else {
            println!("hardware={hardware}: no decoded picture");
        }
    }
    Ok(())
}
