use crate::{codec::CodecCapability, input::InputEvent, model::*};
use serde::{Deserialize, Serialize};

pub const MAX_CONTROL_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub codecs: Vec<CodecCapability>,
    #[serde(default)]
    pub purpose: SessionPurpose,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPurpose {
    #[default]
    Control,
    Transfer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Message {
    SetViewMode {
        mode: ViewMode,
    },
    ViewModeChanged {
        mode: ViewMode,
        error: Option<String>,
    },
    AdbBridge {
        enabled: bool,
    },
    AdbBridgeStatus {
        enabled: bool,
        address: Option<String>,
        error: Option<String>,
    },
    Hello(Hello),
    PairApproved,
    Ready {
        host_epoch: String,
    },
    Snapshot {
        sequence: u64,
        windows: Vec<RemoteWindow>,
    },
    Delta {
        sequence: u64,
        upsert: Vec<RemoteWindow>,
        removed: Vec<WindowId>,
    },
    Command {
        request: u64,
        window: WindowId,
        action: WindowAction,
    },
    Ack {
        request: u64,
        window: WindowId,
        actual: Option<RemoteWindow>,
        error: Option<String>,
    },
    Visibility {
        window: WindowId,
        visible: bool,
        focused: bool,
    },
    Input {
        window: WindowId,
        event: InputEvent,
    },
    ReleaseInput,
    SelectCodec {
        codec: Option<crate::codec::Codec>,
    },
    Menu {
        window: WindowId,
        items: Vec<MenuItem>,
    },
    AudioPreference {
        enabled: bool,
    },
    AudioStarted {
        generation: u32,
    },
    DropFiles {
        window: WindowId,
        transfers: Vec<String>,
        x: f64,
        y: f64,
    },
    DownloadFile {
        path: String,
    },
    StreamStarted(StreamConfig),
    StreamReady {
        window: WindowId,
        generation: u32,
    },
    StreamStopped {
        window: WindowId,
        generation: u32,
    },
    StreamPaused {
        window: WindowId,
    },
    Keyframe {
        window: WindowId,
    },
    Feedback {
        window: WindowId,
        generation: u32,
        lost: u32,
        decoded: u32,
    },
    Clipboard {
        origin: String,
        revision: u64,
        text: Option<String>,
        png_base64: Option<String>,
    },
    Transfer(crate::transfer::TransferMessage),
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    Error {
        message: String,
    },
    Goodbye,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    pub window: WindowId,
    pub generation: u32,
    pub width: u32,
    pub height: u32,
    pub codec: crate::codec::Codec,
    pub encoder: String,
    pub hardware: bool,
    pub fps: u32,
    pub cpu_readback: bool,
}

pub async fn send_json<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    message: &Message,
) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let bytes = serde_json::to_vec(message)?;
    anyhow::ensure!(
        bytes.len() <= MAX_CONTROL_BYTES,
        "control message too large"
    );
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn recv_json<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<Message> {
    use tokio::io::AsyncReadExt;
    let size = reader.read_u32().await? as usize;
    anyhow::ensure!(
        size > 0 && size <= MAX_CONTROL_BYTES,
        "invalid control message length"
    );
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}
