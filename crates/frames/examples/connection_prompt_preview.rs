//! Local UI fixture. Never creates a connection or writes trust.
fn main() -> anyhow::Result<()> {
    let decision = frames_native::confirm_pairing(
        "Studio Mac",
        "482 615",
        "Incoming connection — visual preview",
    )?;
    println!("Selected: {decision:?}");
    Ok(())
}
