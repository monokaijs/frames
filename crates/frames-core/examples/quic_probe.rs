use frames_core::{transport, trust::Identity};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let identity = Identity::load()?;
    let mut checked = 0;
    let mut failed = 0;
    for address in std::env::args().skip(1) {
        checked += 1;
        let endpoint = transport::client(&identity)?;
        let start = std::time::Instant::now();
        match tokio::time::timeout(
            std::time::Duration::from_secs(4),
            endpoint.connect(address.parse()?, "frames.local")?,
        )
        .await
        {
            Ok(Ok(connection)) => {
                println!(
                    "{address}: TLS established in {:?}, local {:?}, peer {}",
                    start.elapsed(),
                    connection.local_ip(),
                    transport::peer_id(&connection)?
                );
                connection.close(0u32.into(), b"diagnostic complete");
                endpoint.wait_idle().await;
            }
            result => {
                failed += 1;
                println!("{address}: {result:?}");
            }
        }
    }
    anyhow::ensure!(checked > 0, "supply one or more numeric IP:port endpoints");
    anyhow::ensure!(failed == 0, "{failed} of {checked} TLS probes failed");
    Ok(())
}
