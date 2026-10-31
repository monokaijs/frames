//! Observe only synthetic red/green transitions in explicitly named windows.
//! Captured pixels remain in memory; output contains timestamps and dimensions.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use std::time::{Duration, Instant, SystemTime};
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(
        args.len() >= 4,
        "presentation_probe SECONDS REPORT.json EXACT_WINDOW_TITLE..."
    );
    let seconds: u64 = args[1].parse()?;
    let mut streams = Vec::new();
    for title in &args[3..] {
        anyhow::ensure!(
            title.contains("Frames QA source") || title.ends_with("Remote Desktop - RustDesk"),
            "only QA or RustDesk comparison windows are accepted"
        );
        streams.push((
            title.clone(),
            frames_native::diagnostic_capture_window(title)?,
            None,
        ));
    }
    let started = Instant::now();
    let mut observations = Vec::new();
    while started.elapsed() < Duration::from_secs(seconds) {
        for (title, stream, previous) in &mut streams {
            if !stream.frames.has_changed().unwrap_or(false) {
                continue;
            }
            let frame = stream.frames.borrow_and_update().clone();
            let Some(frame) = frame else {
                continue;
            };
            let center = ((frame.height / 2 * frame.width + frame.width / 2) * 4) as usize;
            let pixel = &frame.bgra[center..center + 4];
            let green = if pixel[1] > 150 && pixel[0] < 100 && pixel[2] < 100 {
                Some(true)
            } else if pixel[2] > 150 && pixel[0] < 100 && pixel[1] < 100 {
                Some(false)
            } else {
                None
            };
            if let Some(green) = green
                && *previous != Some(green)
            {
                *previous = Some(green);
                observations.push(serde_json::json!({"window":title,"green":green,"unix_us":SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_micros() as u64,"width":frame.width,"height":frame.height}));
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    std::fs::write(
        &args[2],
        serde_json::to_vec_pretty(
            &serde_json::json!({"scope":"native window capture of synthetic transitions; includes observer sampling, excludes physical panel","pixels_persisted":false,"observations":observations}),
        )?,
    )?;
    println!("Recorded {} synthetic transitions", observations.len());
    Ok(())
}
#[cfg(not(windows))]
fn main() {
    eprintln!("The native presentation observer runs on Windows.");
}
