//! USB passthrough between Windows computers over USB/IP (`frames_devices`
//! usb, usbip), ported from v1. The viewer exports a device it chose (an
//! elevated lease binds it to usbipd for this session); the host attaches
//! it with the USB/IP client through a loopback listener, whose connection
//! is bridged to the viewer on a `USB` stream, where it reaches the
//! viewer's usbipd (127.0.0.1:3240). Attach and detach requests go on the
//! devices stream (`crate::pads`).

use crate::streams::Streams;
use anyhow::Result;
use frames_devices::usbip;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Viewer: devices exported for this session, by bus id: the attach
/// generation and whether the export is still wanted.
pub type Exports = Arc<Mutex<HashMap<String, (u64, tokio::sync::watch::Receiver<bool>)>>>;

/// Host: one attached device; detaching drops the bridge.
pub struct Bridge {
    /// The USB/IP port it is attached on (for detaching).
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Host: attaches the viewer's `bus` with the USB/IP client, through a
/// loopback listener bridged to the viewer.
pub async fn bridge(session: frames_net::Session, bus: String, generation: u64) -> Result<Bridge> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let tcp_port = listener.local_addr()?.port();
    let failure = Arc::new(Mutex::new(None::<String>));
    let task = {
        let (bus, failure) = (bus.clone(), failure.clone());
        tokio::spawn(async move {
            let result = async {
                let (mut socket, _) = listener.accept().await?;
                socket.set_nodelay(true)?;
                let mut request = [0u8; usbip::IMPORT_REQUEST_BYTES];
                tokio::time::timeout(Duration::from_secs(10), socket.read_exact(&mut request))
                    .await??;
                usbip::validate_import(&request, &bus)?;
                let (mut send, mut receive) =
                    crate::streams::open(&session, crate::streams::USB).await?;
                send.write_all(usbip::MAGIC).await?;
                send.write_all(&generation.to_be_bytes()).await?;
                send.write_all(&[bus.len() as u8]).await?;
                send.write_all(bus.as_bytes()).await?;
                let status =
                    tokio::time::timeout(Duration::from_secs(15), receive.read_u8()).await??;
                if status != 0 {
                    let size = receive.read_u16().await?.min(2048) as usize;
                    let mut error = vec![0; size];
                    receive.read_exact(&mut error).await?;
                    anyhow::bail!("{}", String::from_utf8_lossy(&error));
                }
                let mut response = [0u8; usbip::IMPORT_REPLY_BYTES];
                receive.read_exact(&mut response).await?;
                socket.write_all(&response).await?;
                relay(socket, send, receive).await
            }
            .await;
            if let Err(error) = result {
                println!("USB {bus}: {error:#}");
                *failure.lock().unwrap() = Some(error.to_string());
            }
        })
    };
    let mut bridge = Bridge { port: 0, task };
    let attached = {
        let bus = bus.clone();
        tokio::task::spawn_blocking(move || frames_devices::usb::attach(&bus, tcp_port)).await?
    };
    bridge.port = attached.map_err(|error| {
        anyhow::anyhow!(
            "{}",
            failure
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| error.to_string())
        )
    })?;
    Ok(bridge)
}

/// Viewer: answers the host's `USB` stream for an exported device by
/// connecting it to this computer's usbipd.
pub async fn serve((mut send, mut receive): Streams, exports: Exports) -> Result<()> {
    let setup = async {
        let mut magic = [0u8; 8];
        receive.read_exact(&mut magic).await?;
        anyhow::ensure!(&magic == usbip::MAGIC, "not a USB request");
        let generation = receive.read_u64().await?;
        let length = receive.read_u8().await? as usize;
        anyhow::ensure!((1..32).contains(&length), "invalid USB device request");
        let mut bus = vec![0; length];
        receive.read_exact(&mut bus).await?;
        let bus = String::from_utf8(bus)?;
        let (version, mut active) = exports
            .lock()
            .unwrap()
            .get(&bus)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("that USB device was not chosen here"))?;
        anyhow::ensure!(
            version == generation && *active.borrow(),
            "the USB device attachment has expired"
        );
        let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, 3240)).await?;
        socket.set_nodelay(true)?;
        let response = tokio::select! {
            _ = active.changed() => anyhow::bail!("the USB device attachment was cancelled"),
            response = tokio::time::timeout(
                Duration::from_secs(10),
                usbip::import_and_probe(&mut socket, &bus),
            ) => response??,
        };
        Ok::<_, anyhow::Error>((socket, response, active))
    }
    .await;
    let (socket, response, mut active) = match setup {
        Ok(ready) => ready,
        Err(error) => {
            let text = error.to_string();
            let bytes = &text.as_bytes()[..text.len().min(2048)];
            send.write_all(&[1]).await?;
            send.write_all(&(bytes.len() as u16).to_be_bytes()).await?;
            send.write_all(bytes).await?;
            return Err(error);
        }
    };
    send.write_all(&[0]).await?;
    send.write_all(&response).await?;
    tokio::select! {
        _ = active.changed() => Ok(()),
        result = relay(socket, send, receive) => result,
    }
}

async fn relay(
    mut socket: TcpStream,
    mut send: iroh::endpoint::SendStream,
    mut receive: iroh::endpoint::RecvStream,
) -> Result<()> {
    let (mut read, mut write) = socket.split();
    tokio::try_join!(copy(&mut read, &mut send), copy(&mut receive, &mut write))?;
    Ok(())
}

async fn copy(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            writer.shutdown().await?;
            return Ok(());
        }
        writer.write_all(&buffer[..count]).await?;
    }
}
