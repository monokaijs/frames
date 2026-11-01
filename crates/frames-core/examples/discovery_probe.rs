//! Exercise the metadata/preview protocol without opening a control session.
use base64::Engine;
use frames_core::{
    protocol::{self, Hello, Message, SessionPurpose},
    transport,
    trust::Identity,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(8), run()).await?
}

async fn run() -> anyhow::Result<()> {
    let arguments: Vec<_> = std::env::args().collect();
    let address = arguments.get(1).expect("address").parse()?;
    let identity = Identity::load()?;
    let endpoint = transport::client(&identity)?;
    let connection = endpoint.connect(address, "frames.local")?.await?;
    let (mut send, mut receive) = connection.open_bi().await?;
    protocol::send_json(
        &mut send,
        &Message::Hello(Hello {
            version: frames_core::PROTOCOL_VERSION,
            device_id: identity.device_id,
            name: "Discovery test client".into(),
            platform: std::env::consts::OS.into(),
            codecs: vec![],
            shared_game: Some(frames_core::shared_game::Capabilities::V1),
            purpose: SessionPurpose::Preview,
        }),
    )
    .await?;
    let Message::Hello(peer) = protocol::recv_json(&mut receive).await? else {
        anyhow::bail!("missing hello");
    };
    anyhow::ensure!(
        peer.device_id == transport::peer_id(&connection)?,
        "identity mismatch"
    );
    protocol::send_json(&mut send, &Message::PairApproved).await?;
    anyhow::ensure!(
        matches!(
            protocol::recv_json(&mut receive).await?,
            Message::PairApproved
        ),
        "missing approval"
    );
    protocol::send_json(
        &mut send,
        &Message::PreviewRequest {
            include_image: true,
        },
    )
    .await?;
    let Message::DevicePreview { png_base64 } = protocol::recv_json(&mut receive).await? else {
        anyhow::bail!("missing preview reply");
    };
    let png = png_base64
        .map(|value| base64::engine::general_purpose::STANDARD.decode(value))
        .transpose()?;
    if let (Some(path), Some(png)) = (arguments.get(2), &png) {
        std::fs::write(path, png)?;
    }
    println!(
        "{}",
        serde_json::json!({"name":peer.name,"device":peer.device_id,"preview_bytes":png.as_ref().map_or(0,Vec::len)})
    );
    connection.close(0u32.into(), b"probe complete");
    Ok(())
}
