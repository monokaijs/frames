use super::*;
use std::time::Duration;

pub struct DeviceProbe {
    pub name: String,
    pub device_id: String,
    pub png: Option<Vec<u8>>,
}

pub async fn probe_device(
    address: SocketAddr,
    expected_id: &str,
    include_image: bool,
) -> anyhow::Result<DeviceProbe> {
    tokio::time::timeout(Duration::from_secs(7), async {
        let identity = Identity::load()?;
        let endpoint = transport::client(&identity)?;
        let connection = tokio::time::timeout(
            Duration::from_secs(2),
            endpoint.connect(address, "frames.local")?,
        )
        .await??;
        let (mut send, mut receive, peer) = handshake(
            &connection,
            &identity,
            vec![],
            false,
            false,
            false,
            SessionPurpose::Preview,
        )
        .await?;
        anyhow::ensure!(
            expected_id.is_empty() || peer.device_id == expected_id,
            "device identity changed"
        );
        let include_image =
            include_image && TrustStore::load()?.peers.contains_key(&peer.device_id);
        protocol::send_json(&mut send, &Message::PreviewRequest { include_image }).await?;
        let png = match protocol::recv_json(&mut receive).await? {
            Message::DevicePreview { png_base64 } => {
                if let Some(png) = png_base64.filter(|_| include_image) {
                    anyhow::ensure!(
                        png.len() <= crate::device_preview::MAX_PREVIEW_BYTES * 4 / 3 + 4,
                        "preview too large"
                    );
                    let bytes = base64::engine::general_purpose::STANDARD.decode(png)?;
                    crate::device_preview::decode(&bytes)?;
                    Some(bytes)
                } else {
                    None
                }
            }
            _ => anyhow::bail!("invalid device preview reply"),
        };
        send.finish()?;
        connection.close(0u32.into(), b"device preview complete");
        Ok(DeviceProbe {
            name: peer.name,
            device_id: peer.device_id,
            png,
        })
    })
    .await
    .context("device is offline")?
}

pub(super) async fn serve_preview(
    connection: quinn::Connection,
    mut send: quinn::SendStream,
    mut receive: quinn::RecvStream,
    peer: Hello,
    desktop: Arc<dyn Desktop>,
    slots: Arc<Semaphore>,
) -> anyhow::Result<()> {
    let result = async {
        let request =
            tokio::time::timeout(Duration::from_secs(3), protocol::recv_json(&mut receive))
                .await??;
        let Message::PreviewRequest { include_image } = request else {
            anyhow::bail!("expected device preview request");
        };
        let allowed =
            frames_core::trust::allows_preview(TrustStore::load()?.peers.get(&peer.device_id));
        let mut png_base64 = None;
        if include_image
            && allowed
            && let Ok(_permit) = slots.try_acquire_owned()
            && let Ok(bytes) = crate::device_preview::capture(desktop, None).await
        {
            png_base64 = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
        protocol::send_json(&mut send, &Message::DevicePreview { png_base64 }).await?;
        send.finish()?;
        let _ = tokio::time::timeout(Duration::from_secs(2), send.stopped()).await;
        Ok(())
    }
    .await;
    connection.close(0u32.into(), b"device preview complete");
    result
}
