#[path = "device_probe.rs"]
mod device_probe;
#[path = "ui_delivery.rs"]
mod ui_delivery;
use anyhow::Context;
use base64::Engine;
pub use device_probe::probe_device;
use device_probe::serve_preview;
use frames_core::{
    codec::{self, Codec, CodecCapability},
    model::*,
    protocol::{self, Hello, Message, SessionPurpose},
    transfer::{self, TransferMessage},
    transport,
    trust::{Identity, TrustStore},
};
use frames_native::{ClientEvent, ClientUpdate, Desktop};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, atomic::AtomicU32},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    sync::{Semaphore, mpsc},
};
#[path = "input_dispatch.rs"]
mod input_dispatch;

fn hello(identity: &Identity, codecs: Vec<CodecCapability>, purpose: SessionPurpose) -> Message {
    Message::Hello(Hello {
        version: frames_core::PROTOCOL_VERSION,
        device_id: identity.device_id.clone(),
        name: frames_native::device_name(),
        platform: std::env::consts::OS.into(),
        codecs,
        shared_game: Some(frames_core::shared_game::Capabilities::V1),
        purpose,
    })
}

async fn approval(
    name: String,
    code: String,
    reason: &'static str,
) -> anyhow::Result<frames_native::ConnectionDecision> {
    tokio::task::spawn_blocking(move || {
        if std::env::var_os("FRAMES_PAIRING_CONSOLE").is_some() {
            use std::io::{BufRead, Write};
            eprintln!("\n{reason}: {name:?}\nVerify the SAME code on both devices: {code}");
            eprint!("Type trust to accept and trust, accept to accept once, or no to decline: ");
            std::io::stderr().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            Ok(match line.trim() {
                "trust" | "yes" => frames_native::ConnectionDecision::Trust,
                "accept" => frames_native::ConnectionDecision::Accept,
                _ => frames_native::ConnectionDecision::Decline,
            })
        } else {
            frames_native::confirm_pairing(&name, &code, reason)
        }
    })
    .await?
}

async fn handshake(
    connection: &quinn::Connection,
    identity: &Identity,
    codecs: Vec<CodecCapability>,
    server: bool,
    allow_pair: bool,
    unattended: bool,
    purpose: SessionPurpose,
) -> anyhow::Result<(quinn::SendStream, quinn::RecvStream, Hello)> {
    let (mut send, mut receive) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        if server {
            connection.accept_bi().await
        } else {
            connection.open_bi().await
        }
    })
    .await??;
    protocol::send_json(&mut send, &hello(identity, codecs, purpose)).await?;
    let peer = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        protocol::recv_json(&mut receive),
    )
    .await??
    {
        Message::Hello(h) => h,
        _ => anyhow::bail!("expected protocol hello"),
    };
    anyhow::ensure!(
        peer.version == frames_core::PROTOCOL_VERSION,
        "incompatible protocol version"
    );
    anyhow::ensure!(
        peer.name.len() <= 256 && peer.codecs.len() <= 32 && peer.platform.len() <= 32,
        "invalid peer capabilities"
    );
    let id = transport::peer_id(connection)?;
    anyhow::ensure!(
        peer.device_id == id,
        "device identity does not match TLS certificate"
    );
    let mut store = TrustStore::load()?;
    let known = store.peers.get(&id);
    let preview = if server { peer.purpose } else { purpose } == SessionPurpose::Preview;
    if !server
        && !preview
        && let Ok(expected) = std::env::var("FRAMES_EXPECTED_DEVICE_ID")
    {
        anyhow::ensure!(
            expected.is_empty() || expected == id,
            "The device at this address has changed. Select it again and verify its pairing code."
        );
    }
    let needs_approval = if preview {
        false
    } else if server {
        frames_core::trust::incoming_needs_approval(known, unattended)
    } else {
        known.is_none()
    };
    let mut decision = frames_native::ConnectionDecision::Accept;
    if needs_approval {
        anyhow::ensure!(
            !server || allow_pair || known.is_some_and(|p| p.allow_control),
            "new device pairing disabled; start host with --pair"
        );
        decision = approval(
            peer.name.clone(),
            transport::verification_code(connection)?,
            if known.is_none() {
                "Pair new device"
            } else {
                "Approve session"
            },
        )
        .await?;
        anyhow::ensure!(
            decision != frames_native::ConnectionDecision::Decline,
            "connection declined"
        );
    }
    protocol::send_json(&mut send, &Message::PairApproved).await?;
    anyhow::ensure!(
        matches!(
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                protocol::recv_json(&mut receive)
            )
            .await??,
            Message::PairApproved
        ),
        "peer did not approve pairing"
    );
    if decision == frames_native::ConnectionDecision::Trust {
        store.approve(
            id,
            peer.name.clone(),
            if server { unattended } else { true },
            server,
        )?;
    }
    Ok((send, receive, peer))
}

#[derive(Clone)]
pub struct Outbound {
    pub(crate) devices: frames_core::devices::Sender,
    pub(crate) media: Option<crate::media_sender::MediaSender>,
    control: mpsc::Sender<Message>,
    realtime: mpsc::Sender<Message>,
    input: mpsc::Sender<Message>,
    bulk: mpsc::Sender<Message>,
    progress: Option<frames_native::ClientSender>,
}
#[cfg(test)]
pub(crate) fn test_outbound() -> (Outbound, mpsc::Receiver<Message>) {
    let (control, rx) = mpsc::channel(64);
    let (bulk, _) = mpsc::channel(4);
    (
        Outbound {
            devices: frames_core::devices::channel().0,
            media: None,
            realtime: control.clone(),
            input: control.clone(),
            control,
            bulk,
            progress: None,
        },
        rx,
    )
}
impl Outbound {
    fn progress(&self, name: String, done: u64, total: u64) {
        if let Some(ui) = &self.progress {
            let _ = ui.try_send(ClientUpdate::TransferProgress { name, done, total });
        }
    }
    pub async fn send(&self, message: Message) -> anyhow::Result<()> {
        if let Message::Device(message) = message {
            return self.devices.send(message);
        }
        let channel = if interactive(&message) {
            &self.input
        } else if realtime(&message) {
            &self.realtime
        } else if matches!(
            &message,
            Message::Transfer(TransferMessage::Chunk { .. } | TransferMessage::Complete { .. })
        ) {
            &self.bulk
        } else {
            &self.control
        };
        channel
            .send(message)
            .await
            .map_err(|_| anyhow::anyhow!("session writer stopped"))
    }
    pub fn blocking_send(&self, message: Message) -> anyhow::Result<()> {
        self.control
            .blocking_send(message)
            .map_err(|_| anyhow::anyhow!("control writer stopped"))
    }
}
fn interactive(message: &Message) -> bool {
    matches!(
        message,
        Message::Input { .. }
            | Message::SharedInput { .. }
            | Message::SharedHeartbeat { .. }
            | Message::SharedResume { .. }
            | Message::InputClock { .. }
            | Message::ReleaseInput
            | Message::Command { .. }
            | Message::Visibility { .. }
            | Message::SetViewMode { .. }
            | Message::SetRemoteCursor { .. }
    )
}
fn realtime(message: &Message) -> bool {
    matches!(
        message,
        Message::StreamStarted(_)
            | Message::StreamReady { .. }
            | Message::StreamStopped { .. }
            | Message::StreamPaused { .. }
            | Message::Keyframe { .. }
            | Message::Feedback { .. }
            | Message::MediaStatus { .. }
            | Message::AudioStarted { .. }
            | Message::ClockProbe { .. }
            | Message::ClockReply { .. }
    )
}
struct Incoming {
    devices: Option<mpsc::Receiver<frames_core::devices::DeviceMessage>>,
    bulk: Option<mpsc::Receiver<Message>>,
    realtime: mpsc::Receiver<Message>,
    control: mpsc::Receiver<Message>,
    input: mpsc::Receiver<Message>,
}
impl Incoming {
    async fn recv(&mut self) -> Option<Message> {
        tokio::select! {
            biased;
            message = self.input.recv() => message,
            message = self.realtime.recv() => message,
            message = self.control.recv() => message,
            message = async { match &mut self.bulk { Some(bulk) => bulk.recv().await, None => std::future::pending().await } } => message,
        }
    }
}
struct Control {
    send: Outbound,
    receive: Incoming,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
impl Drop for Control {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
fn control(
    connection: quinn::Connection,
    mut writer: quinn::SendStream,
    mut reader: quinn::RecvStream,
) -> Control {
    let _ = frames_core::media::monotonic_micros();
    let media = crate::media_sender::MediaSender::new(connection.clone());
    let bulk_media = media.clone();
    let (tx, mut outgoing) = mpsc::channel::<Message>(64);
    let (realtime_tx, mut realtime_out) = mpsc::channel::<Message>(64);
    let (realtime_incoming, realtime_rx) = mpsc::channel::<Message>(64);
    let (bulk_tx, mut bulk) = mpsc::channel::<Message>(4);
    let (input_tx, mut inputs) = mpsc::channel::<Message>(64);
    let (input_incoming, input_rx) = mpsc::channel::<Message>(64);
    let (incoming, rx) = mpsc::channel(64);
    let (bulk_incoming, bulk_rx) = mpsc::channel(4);
    let (device_tx, mut device_out) = frames_core::devices::channel();
    let (device_incoming, device_rx) = mpsc::channel(128);
    let device_incoming = Arc::new(std::sync::Mutex::new(Some(device_incoming)));
    let device_connection = connection.clone();
    let device_write = tokio::spawn(async move {
        let result = async {
            let mut writer = device_connection.open_uni().await?;
            writer.set_priority(10)?;
            writer.write_all(b"G").await?;
            while let Some(message) = device_out.recv().await {
                protocol::send_json(&mut writer, &Message::Device(message)).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(%error, "device writer ended");
        }
    });
    let conn = connection.clone();
    let reader_incoming = incoming.clone();
    let bulk_connection = connection.clone();
    let write = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            if let Err(error) = protocol::send_json(&mut writer, &message).await {
                tracing::warn!(%error,"control writer failed");
                if conn.close_reason().is_none() {
                    conn.close(1u32.into(), b"control writer failed");
                }
                break;
            }
        }
        conn.close(0u32.into(), b"control writer ended");
    });
    let read = tokio::spawn(async move {
        loop {
            match protocol::recv_json(&mut reader).await {
                Ok(message) => {
                    if reader_incoming.send(message).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(%error,"control reader ended");
                    if connection.close_reason().is_none() {
                        connection.close(1u32.into(), b"control reader failed");
                    }
                    break;
                }
            }
        }
        connection.close(0u32.into(), b"control reader ended");
    });
    let outgoing_connection = bulk_connection.clone();
    let input_connection = bulk_connection.clone();
    let input_write = tokio::spawn(async move {
        let result = async {
            let mut writer = input_connection.open_uni().await?;
            writer.set_priority(10)?;
            writer.write_all(b"I").await?;
            while let Some(message) = inputs.recv().await {
                protocol::send_json(&mut writer, &message).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(%error, "input writer failed");
            input_connection.close(1u32.into(), b"input writer failed");
        }
    });
    let realtime_connection = bulk_connection.clone();
    let realtime_write = tokio::spawn(async move {
        let result = async {
            let mut writer = realtime_connection.open_uni().await?;
            writer.set_priority(8)?;
            writer.write_all(b"M").await?;
            while let Some(message) = realtime_out.recv().await {
                protocol::send_json(&mut writer, &message).await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            tracing::debug!(%error,"media control stream ended");
            realtime_connection.close(1u32.into(), b"media control writer failed");
        }
    });
    let bulk_write = tokio::spawn(async move {
        let Ok(mut writer) = outgoing_connection.open_uni().await else {
            return;
        };
        let _ = writer.set_priority(-10);
        if writer.write_all(b"F").await.is_err() {
            return;
        }
        while let Some(message) = bulk.recv().await {
            let size = match &message {
                Message::Transfer(TransferMessage::Chunk { data_base64, .. }) => data_base64.len(),
                _ => 256,
            };
            bulk_media.throttle_bulk(size).await;
            if protocol::send_json(&mut writer, &message).await.is_err() {
                break;
            }
        }
    });
    let bulk_read = tokio::spawn(async move {
        while let Ok(mut reader) = bulk_connection.accept_uni().await {
            let input_incoming = input_incoming.clone();
            let realtime_incoming = realtime_incoming.clone();
            let bulk_incoming = bulk_incoming.clone();
            let device_incoming = device_incoming.clone();
            let conn = bulk_connection.clone();
            tokio::spawn(async move {
                let Ok(lane) = reader.read_u8().await else {
                    return;
                };
                if !matches!(lane, b'I' | b'F' | b'M' | b'G') {
                    conn.close(1u32.into(), b"invalid stream lane");
                    return;
                }
                if lane == b'G' {
                    let Some(sender) = device_incoming.lock().unwrap().take() else {
                        let _ = reader.stop(1u32.into());
                        return;
                    };
                    while let Ok(device) = protocol::recv_device(&mut reader).await {
                        if sender.try_send(device).is_err() {
                            let _ = reader.stop(1u32.into());
                            break;
                        }
                    }
                    // Dropping this sole sender releases pads and USB leases in the actor.
                    return;
                }
                while let Ok(message) = protocol::recv_json(&mut reader).await {
                    let valid = if lane == b'I' {
                        interactive(&message)
                    } else if lane == b'M' {
                        realtime(&message)
                    } else {
                        matches!(
                            &message,
                            Message::Transfer(
                                TransferMessage::Chunk { .. } | TransferMessage::Complete { .. }
                            )
                        )
                    };
                    if !valid {
                        conn.close(1u32.into(), b"invalid bulk message");
                        break;
                    }
                    let destination = if lane == b'I' {
                        &input_incoming
                    } else if lane == b'M' {
                        &realtime_incoming
                    } else {
                        &bulk_incoming
                    };
                    if destination.send(message).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Control {
        send: Outbound {
            devices: device_tx,
            media: Some(media),
            realtime: realtime_tx,
            control: tx,
            input: input_tx,
            bulk: bulk_tx,
            progress: None,
        },
        receive: Incoming {
            devices: Some(device_rx),
            bulk: Some(bulk_rx),
            realtime: realtime_rx,
            control: rx,
            input: input_rx,
        },
        tasks: vec![
            write,
            read,
            input_write,
            realtime_write,
            device_write,
            bulk_write,
            bulk_read,
        ],
    }
}

fn downloads() -> PathBuf {
    if let Some(path) = std::env::var_os("FRAMES_DOWNLOAD_DIR") {
        return path.into();
    }
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Downloads")
        .join("Frames")
}

pub async fn host(
    address: SocketAddr,
    pair: bool,
    unattended: bool,
    exclude: Vec<String>,
    receive_dir: Option<PathBuf>,
    max_streams: usize,
    clipboard: bool,
) -> anyhow::Result<()> {
    host_mode(
        address,
        pair,
        unattended,
        exclude,
        receive_dir,
        max_streams,
        clipboard,
        None,
        None,
    )
    .await
}

pub async fn shared_host(
    address: SocketAddr,
    pair: bool,
    unattended: bool,
    exclude: Vec<String>,
    window: Option<u64>,
) -> anyhow::Result<()> {
    host_mode(
        address,
        pair,
        unattended,
        exclude,
        None,
        1,
        false,
        Some(window),
        None,
    )
    .await
}
pub async fn selected_host(
    address: SocketAddr,
    pair: bool,
    unattended: bool,
    exclude: Vec<String>,
    windows: Vec<String>,
    isolated: bool,
    max_streams: usize,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !isolated || windows.len() == 1,
        "One window mode requires exactly one selection"
    );
    host_mode(
        address,
        pair,
        unattended,
        exclude,
        None,
        max_streams,
        false,
        None,
        Some((windows, isolated)),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn host_mode(
    address: SocketAddr,
    pair: bool,
    unattended: bool,
    exclude: Vec<String>,
    receive_dir: Option<PathBuf>,
    max_streams: usize,
    clipboard: bool,
    shared: Option<Option<u64>>,
    selected: Option<(Vec<String>, bool)>,
) -> anyhow::Result<()> {
    anyhow::ensure!((1..=32).contains(&max_streams), "max-streams must be 1..32");
    let identity = Arc::new(Identity::load()?);
    let codecs = codec::probe().await;
    anyhow::ensure!(!codecs.is_empty(), "no working codecs; run frames doctor");
    let desktop = if let Some((windows, isolated)) = selected {
        if isolated {
            frames_native::shared_selected_desktop(exclude, windows.into_iter().next().unwrap())?
        } else {
            frames_native::selected_desktop(exclude, windows)?
        }
    } else if let Some(window) = shared {
        frames_native::shared_desktop(exclude, window)?
    } else {
        frames_native::desktop(exclude)?
    };
    let endpoint = transport::server(&identity, address)?;
    let mut advertised_name = frames_native::device_name();
    let mut advertisement = if std::env::var_os("FRAMES_NO_DISCOVERY").is_none() {
        Some(frames_core::discovery::advertise(
            &advertised_name,
            &identity.device_id,
            endpoint.local_addr()?.port(),
        )?)
    } else {
        None
    };
    let semaphore = Arc::new(Semaphore::new(1));
    let preview_slots = Arc::new(Semaphore::new(2));
    let pending_slots = Arc::new(Semaphore::new(4));
    let transfer_slots = Arc::new(Semaphore::new(4));
    let active = Arc::new(std::sync::Mutex::new(None::<(String, quinn::Connection)>));
    let active_transfers = Arc::new(std::sync::Mutex::new(HashMap::<
        String,
        (quinn::Connection, Arc<Semaphore>),
    >::new()));
    let receive_dir = receive_dir.unwrap_or_else(downloads);
    eprintln!(
        "Frames host listening on {}\nIdentity: {}\nDownloads: {}",
        endpoint.local_addr()?,
        identity.device_id,
        receive_dir.display()
    );
    let mut health = tokio::time::interval(std::time::Duration::from_secs(1));
    let result = loop {
        tokio::select! {
            _=tokio::signal::ctrl_c()=>break Ok(()),
            _=health.tick()=>{
                if let Err(error)=desktop.check_health(){break Err(error);}
                let name=frames_native::device_name();
                if advertisement.is_some() && name != advertised_name {
                    match frames_core::discovery::advertise(&name, &identity.device_id, endpoint.local_addr()?.port()) {
                        Ok(updated) => { if let Some(old)=advertisement.replace(updated){let _=old.shutdown();} advertised_name=name; }
                        Err(error) => tracing::warn!(%error,"could not update advertised name"),
                    }
                }
            }
            incoming=endpoint.accept()=>{
                let Some(incoming)=incoming else{break Ok(());};
                let Ok(pending_permit)=pending_slots.clone().try_acquire_owned()else{incoming.refuse();continue;};
                let(identity,desktop,codecs,receive_dir)=(identity.clone(),desktop.clone(),codecs.clone(),receive_dir.clone());
                let(controller_slots,transfer_slots,active)=(semaphore.clone(),transfer_slots.clone(),active.clone());
                let active_transfers=active_transfers.clone();
                let preview_slots=preview_slots.clone();
                tokio::spawn(async move{
                    let result=async{
                        let connection=tokio::time::timeout(std::time::Duration::from_secs(10),incoming).await??;
                        let(send,receive,peer)=handshake(&connection,&identity,codecs.clone(),true,pair,unattended,SessionPurpose::Control).await?;
                        drop(pending_permit);
                        if peer.purpose==SessionPurpose::Preview {
                            return serve_preview(connection,send,receive,peer,desktop,preview_slots).await;
                        }
                        let controlling=peer.purpose==SessionPurpose::Control;
                        if controlling {
                            let mut address=connection.remote_address();address.set_port(47990);
                            if let Err(error)=crate::connection_history::remember(address,&peer.device_id,&peer.name,true){tracing::warn!(%error,"could not save device history");}
                        }
                        let peer_id=peer.device_id.clone();
                        let mut transfer_lease=None;
                        let _permit=if controlling{
                            {let old=active.lock().unwrap();if let Some((id,old_connection))=old.as_ref(){anyhow::ensure!(id==&peer.device_id,"another device is controlling this host");old_connection.close(0u32.into(),b"reconnected by same trusted device");}}
                            let permit=tokio::time::timeout(std::time::Duration::from_secs(5),controller_slots.acquire_owned()).await??;
                            *active.lock().unwrap()=Some((peer.device_id.clone(),connection.clone()));permit
                        }else{
                            let slot={
                                let mut slots=active_transfers.lock().unwrap();
                                let slot=if let Some((old,slot))=slots.get(&peer_id){old.close(0u32.into(),b"transfer reconnected by same device");slot.clone()}else{Arc::new(Semaphore::new(1))};
                                slots.insert(peer_id.clone(),(connection.clone(),slot.clone()));slot
                            };
                            transfer_lease=Some(tokio::time::timeout(std::time::Duration::from_secs(5),slot.acquire_owned()).await??);
                            tokio::time::timeout(std::time::Duration::from_secs(5),transfer_slots.acquire_owned()).await??
                        };
                        let result=host_session(connection.clone(),control(connection.clone(),send,receive),desktop.clone(),codecs,peer,receive_dir,max_streams,clipboard).await;
                        if controlling{desktop.release_input();let mut slot=active.lock().unwrap();if slot.as_ref().is_some_and(|(_,c)|c.stable_id()==connection.stable_id()){slot.take();}}
                        else{let mut slots=active_transfers.lock().unwrap();if slots.get(&peer_id).is_some_and(|(c,_)|c.stable_id()==connection.stable_id()){slots.remove(&peer_id);}}
                        drop(transfer_lease);
                        if let Err(error)=&result{let reason=error.to_string();connection.close(1u32.into(),&reason.as_bytes()[..reason.len().min(1024)]);}else{connection.close(0u32.into(),b"session ended");}result
                    }.await;
                    if let Err(error)=result{tracing::warn!(%error,"host session ended");}
                });
            }
        }
    };
    endpoint.close(0u32.into(), b"host shutting down");
    desktop.release_input();
    endpoint.wait_idle().await;
    result
}

#[allow(clippy::too_many_arguments)] // Authenticated session dependencies, distinct from persistent options.
async fn host_session(
    connection: quinn::Connection,
    mut io: Control,
    desktop: Arc<dyn Desktop>,
    codecs: Vec<CodecCapability>,
    peer: Hello,
    receive_dir: PathBuf,
    max_streams: usize,
    clipboard: bool,
) -> anyhow::Result<()> {
    let _update_lease = crate::update::session_lease()?;
    anyhow::ensure!(
        desktop.shared_session().is_none()
            || peer.shared_game == Some(frames_core::shared_game::Capabilities::V1),
        "This viewer does not support the required shared-game input and lease capabilities"
    );
    anyhow::ensure!(
        desktop.shared_session().is_none() || peer.purpose == SessionPurpose::Control,
        "File transfers are unavailable in shared game mode"
    );
    struct ControlCleanup(Arc<dyn Desktop>, bool);
    impl Drop for ControlCleanup {
        fn drop(&mut self) {
            if self.1 {
                self.0.end_control();
            }
        }
    }
    let _control_cleanup = ControlCleanup(desktop.clone(), peer.purpose == SessionPurpose::Control);
    let preparing = if peer.purpose == SessionPurpose::Control {
        let d = desktop.clone();
        tokio::task::spawn_blocking(move || d.prepare_control()).await??
    } else {
        None
    };
    if let Some(state) = &preparing {
        io.send
            .send(Message::SessionPolicy {
                shared: Some(state.clone()),
            })
            .await?;
    }
    if peer.purpose == SessionPurpose::Control {
        let d = desktop.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || d.begin_control()).await? {
            if let Some(state) = desktop.shared_session() {
                let _ = io.send.send(Message::SharedState(state)).await;
            }
            return Err(error);
        }
    }
    let shared = desktop.shared_session();
    if let Some(state) = &shared {
        io.send.send(Message::SharedState(state.clone())).await?;
    } else {
        io.send
            .send(Message::SessionPolicy { shared: None })
            .await?;
    }
    let epoch = uuid::Uuid::new_v4().to_string();
    let _devices = if peer.purpose == SessionPurpose::Control && shared.is_none() {
        Some(crate::device_session::start_host(
            connection.clone(),
            io.send.clone(),
            io.receive.devices.take().unwrap(),
            desktop.allow_device_bridge(),
        ))
    } else {
        io.receive.devices.take();
        None
    };
    io.send
        .send(Message::Ready {
            host_epoch: epoch.clone(),
        })
        .await?;
    let mut known = HashMap::<WindowId, RemoteWindow>::new();
    let mut sequence = 0;
    let mut streams = HashMap::<WindowId, crate::streaming::HostStream>::new();
    let mut subscriptions = HashMap::<WindowId, Subscription>::new();
    let generations = Arc::new(AtomicU32::new(1));
    let completed_files = crate::file_worker::CompletedFiles::default();
    let transfers = crate::file_worker::FileWorker::new(
        receive_dir,
        desktop.allow_file_transfer(),
        io.receive.bulk.take().unwrap(),
        io.send.clone(),
        None,
        completed_files.clone(),
    );
    let mut preference = None;
    let mut failures = HashMap::<WindowId, u32>::new();
    let mut media_losses = HashMap::<WindowId, u64>::new();
    let mut audio = None::<crate::streaming::HostStream>;
    let mut shared_audio_requested = false;
    let scoped_audio = shared.is_some() || !desktop.allow_desktop();

    let outgoing_files = Arc::new(std::sync::Mutex::new(HashMap::<String, PathBuf>::new()));
    let mut outgoing_jobs = HashMap::<String, tokio::task::JoinHandle<()>>::new();
    let menu_slots = Arc::new(Semaphore::new(2));
    let preview_slot = Arc::new(Semaphore::new(1));
    let mut last_preview = std::time::Instant::now() - std::time::Duration::from_secs(5);
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let clipboard_worker =
        (clipboard && desktop.allow_clipboard() && peer.purpose == SessionPurpose::Control)
            .then(|| ClipboardWorker::start(desktop.clone(), epoch.clone(), io.send.clone()))
            .transpose()?;
    let mut scan = 0u64;
    let mut view_mode = ViewMode::Windows;
    let mut show_cursor = true;
    let mut last_shared_phase = None;
    let mut scan_mode = view_mode;
    let mut window_scan: Option<tokio::task::JoinHandle<anyhow::Result<Vec<RemoteWindow>>>> = None;
    let mut adb_bridge = None::<frames_core::device_bridge::HostBridge>;
    let mut input = input_dispatch::Dispatcher::new(
        desktop.clone(),
        io.send.clone(),
        peer.purpose == SessionPurpose::Control,
    )?;
    loop {
        tokio::select! {
            _=connection.closed()=>break,
            _=connection.read_datagram()=>{},
            result=async{window_scan.as_mut().unwrap().await},if window_scan.is_some()=>{
                window_scan.take();
                if scan_mode!=view_mode{continue;}
                let list=match result{
                    Ok(Ok(list))=>list,
                    Ok(Err(error))=>{tracing::warn!(%error,"window scan failed; retaining the last snapshot");continue;},
                    Err(error)=>{tracing::warn!(%error,"window scan task failed; retaining the last snapshot");continue;}
                };
                let next:HashMap<_,_>=list.iter().map(|w|(w.key,w.clone())).collect();
                // Header inclusion is fixed when capture starts. Recreate only
                // the affected stream after the host applies the new setting.
                for window in &list {
                    if known.get(&window.key).is_some_and(|old| old.show_header != window.show_header) {
                        if let Some(mut stream) = streams.remove(&window.key) {
                            stream.shutdown().await;
                            io.send.send(Message::StreamPaused { window: window.key }).await?;
                        }
                        failures.remove(&window.key);
                    }
                }
                if sequence==0{sequence=1;io.send.send(Message::Snapshot{sequence,windows:list}).await?;}
                else{
                    let upsert:Vec<_>=list.into_iter().filter(|w|known.get(&w.key)!=Some(w)).collect();let removed:Vec<_>=known.keys().filter(|k|!next.contains_key(k)).copied().collect();
                    if !upsert.is_empty()||!removed.is_empty(){sequence+=1;io.send.send(Message::Delta{sequence,upsert,removed:removed.clone()}).await?;for key in removed{streams.remove(&key);subscriptions.remove(&key);}}
                }
                known=next;
            },
            _=tick.tick()=>{
                scan+=1;
                if scoped_audio&&shared.is_none(){
                    let visible=known.values().any(|w|w.visible&&!w.minimized);
                    if shared_audio_requested&&visible{if audio.is_none(){audio=Some(crate::streaming::start_audio(desktop.clone(),connection.clone(),io.send.clone(),generations.clone()));}}else{audio=None;}
                }
                if let Some(session)=&shared {
                    let audio_visible=known.get(&session.window).is_some_and(|w|w.visible&&!w.minimized);
                    if shared_audio_requested&&audio_visible {
                        if audio.is_none(){audio=Some(crate::streaming::start_audio(desktop.clone(),connection.clone(),io.send.clone(),generations.clone()));}
                    }else{audio=None;}
                    match desktop.shared_cursor() {
                        Ok(Some(mut cursor))=>{cursor.visible &= show_cursor;io.send.send(Message::SharedCursor{window:session.window,generation:session.generation,cursor}).await?;},
                        Ok(None)=>{},Err(error)=>{desktop.release_input();io.send.send(Message::Error{message:format!("Shared input: {error}")}).await?;break;}
                    }
                    if let Some(state)=desktop.shared_session() && last_shared_phase!=Some(state.phase){last_shared_phase=Some(state.phase);io.send.send(Message::SharedState(state)).await?;}
                }
                if scan.is_multiple_of(10){anyhow::ensure!(TrustStore::load()?.peers.get(&peer.device_id).is_some_and(|p|p.allow_control),"device access revoked");}
                if peer.purpose==SessionPurpose::Transfer{continue;}
                if window_scan.is_none(){
                    let d=desktop.clone();scan_mode=view_mode;
                    window_scan=Some(tokio::task::spawn_blocking(move||d.surfaces(view_mode)));
                }
                let ended:Vec<_>=streams.iter().filter(|(_,s)|s.task.is_finished()).map(|(&key,_)|key).collect();for key in ended{streams.remove(&key);*failures.entry(key).or_default()+=1;}
                let desired=frames_core::scheduler::select(subscriptions.iter().map(|(&key,sub)|frames_core::scheduler::Demand{
                    window:key,visible:sub.visible&&known.get(&key).is_some_and(|w|w.visible&&!w.minimized),focused:sub.focused,running:streams.contains_key(&key)
                }),max_streams);
                let evicted:Vec<_>=streams.keys().filter(|key|!desired.contains(key)).copied().collect();
                for key in evicted{if let Some(mut stream)=streams.remove(&key){stream.shutdown().await;io.send.send(Message::StreamPaused{window:key}).await?;}}
                for key in desired{
                    let sub=&subscriptions[&key];
                    if let Some(stream)=streams.get(&key){stream.set_fps(sub.fps());}
                    if let std::collections::hash_map::Entry::Vacant(entry)=streams.entry(key){
                        if failures.get(&key).copied().unwrap_or(0)>1&&!scan.is_multiple_of(50){continue;}
                        let selected=if preference.is_none()&&failures.get(&key).copied().unwrap_or(0)>0{Some(Codec::Vp9)}else{preference};
                        let cap=codec::negotiate(&codecs,&peer.codecs,selected)?;
                        entry.insert(crate::streaming::start_host(desktop.clone(),key,cap,crate::streaming::VideoOptions{fps:sub.fps(),show_cursor},connection.clone(),io.send.clone(),generations.clone()));
                    }
                }

            },
            message=io.receive.recv()=>{let Some(message)=message else{break;};
                if let Some(session)=&shared && !session.permits(&message) {io.send.send(Message::Error{message:"This operation is unavailable when sharing one window".into()}).await?;continue;}
                if peer.purpose==SessionPurpose::Transfer{anyhow::ensure!(matches!(&message,Message::Transfer(_)|Message::DownloadFile{..}|Message::Ping{..}|Message::Pong{..}|Message::Goodbye),"transfer sessions cannot control the desktop");}
                if !desktop.allow_file_transfer()&&matches!(&message,Message::Transfer(_)|Message::DownloadFile{..}|Message::DropFiles{..}){
                    io.send.send(Message::Error{message:"File transfers are unavailable for this sharing scope".into()}).await?;
                    continue;
                }
                match message{
                Message::PreviewRequest{include_image:true}=>{
                    if last_preview.elapsed()>=std::time::Duration::from_secs(3) && let Ok(permit)=preview_slot.clone().try_acquire_owned(){
                        last_preview=std::time::Instant::now();
                        let window=subscriptions.iter().find(|(_,s)|s.visible&&s.focused).or_else(||subscriptions.iter().find(|(_,s)|s.visible)).map(|(id,_)|*id);
                        if let Some(window)=window {
                            let source=desktop.clone();let out=io.send.clone();
                            tokio::spawn(async move{let _permit=permit;if let Ok(bytes)=crate::device_preview::capture(source,Some(window)).await{let _=out.send(Message::DevicePreview{png_base64:Some(base64::engine::general_purpose::STANDARD.encode(bytes))}).await;}});
                        }
                    }
                },
                Message::SetRemoteCursor{visible}=>{
                    if show_cursor!=visible{
                        for(key,mut stream)in streams.drain(){stream.shutdown().await;io.send.send(Message::StreamPaused{window:key}).await?;}
                        show_cursor=visible;failures.clear();
                    }
                    io.send.send(Message::RemoteCursorChanged{visible:show_cursor}).await?;
                },
                Message::SetViewMode{mode}=>{
                    if mode==ViewMode::Desktop&&!desktop.allow_desktop(){io.send.send(Message::ViewModeChanged{mode:ViewMode::Windows,error:Some("Only the host-selected windows are shared".into())}).await?;continue;}
                    if let Some(job)=window_scan.take(){let _=job.await;}
                    let d=desktop.clone();let result=tokio::task::spawn_blocking(move||d.surfaces(mode)).await?;
                    match result{
                        Ok(list)=>{
                            input.release()?;for(_,mut stream)in streams.drain(){stream.shutdown().await;}
                            subscriptions.clear();failures.clear();view_mode=mode;sequence+=1;
                            known=list.iter().map(|w|(w.key,w.clone())).collect();
                            io.send.send(Message::ViewModeChanged{mode,error:None}).await?;
                            io.send.send(Message::Snapshot{sequence,windows:list}).await?;
                        },
                        Err(error)=>io.send.send(Message::ViewModeChanged{mode:view_mode,error:Some(error.to_string())}).await?,
                    }
                },
                Message::AdbBridge{enabled}=>{
                    if let Some(mut bridge)=adb_bridge.take(){bridge.shutdown().await;}
                    let result=if enabled{
                        if desktop.allow_device_bridge(){frames_core::device_bridge::HostBridge::start(connection.clone()).await.map(Some)}
                        else{Err(anyhow::anyhow!("Device forwarding requires a normal user-session host"))}
                    }else{Ok(None)};
                    match result{Ok(bridge)=>{let address=bridge.as_ref().map(|b|b.address.to_string());adb_bridge=bridge;io.send.send(Message::AdbBridgeStatus{enabled:adb_bridge.is_some(),address,error:None}).await?;},Err(error)=>io.send.send(Message::AdbBridgeStatus{enabled:false,address:None,error:Some(error.to_string())}).await?}
                },
                Message::Visibility{window,visible,focused}=>{if known.contains_key(&window){let subscription=subscriptions.entry(window).or_default();
                    if shared.is_some() && (!visible||!focused){input.release()?;}
                    subscription.update(visible,focused);if !visible{streams.remove(&window);}else if let Some(stream)=streams.get(&window){stream.set_fps(subscription.fps());}}},
                Message::SelectCodec{codec}=>{codec::negotiate(&codecs,&peer.codecs,codec)?;preference=codec;streams.clear();failures.clear();},
                Message::StreamReady{window,generation}=>if window==frames_core::media::AUDIO_WINDOW{if let Some(audio)=&audio{audio.acknowledge(generation);}}else if let Some(stream)=streams.get(&window){stream.acknowledge(generation);},
                Message::ClockProbe{client_us}=>{let received_us=frames_core::media::monotonic_micros();io.send.send(Message::ClockReply{client_us,received_us,sent_us:frames_core::media::monotonic_micros()}).await?;},
                Message::Feedback{window,generation,mut stats}=>{let previous=media_losses.insert(window,stats.delivery.missing_shards).unwrap_or(0);if stats.delivery.missing_shards>previous && let Some(sender)=&io.send.media{sender.note_loss();}
                    if let Some(stream)=streams.get(&window){if let Some(sender)=&io.send.media{stats.queue_micros=stats.queue_micros.max(sender.queue_micros());tracing::trace!(bytes=sender.bytes(),late=sender.late(),"media admission totals");}let path=connection.stats().path;stats.congestion_events=path.congestion_events;stats.congestion_window_bytes=path.cwnd;stats.transport_lost_packets=path.lost_packets;stream.feedback(generation,stats,path.rtt,path.min_rtt);}},
                Message::Keyframe{window,generation}=>if window==frames_core::media::AUDIO_WINDOW{if let Some(audio)=&audio{audio.request_keyframe(generation);}}else if let Some(stream)=streams.get(&window){stream.request_keyframe(generation);},
                Message::AudioPreference{enabled}=>{audio=None;if scoped_audio{shared_audio_requested=enabled;}else if enabled{audio=Some(crate::streaming::start_audio(desktop.clone(),connection.clone(),io.send.clone(),generations.clone()));}},
                Message::Command{request,window,action}=>{
                    if matches!(&action,WindowAction::Menu){
                        if let Ok(slot)=menu_slots.clone().try_acquire_owned(){
                            let d=desktop.clone();let out=io.send.clone();let actual=known.get(&window).cloned();
                            tokio::task::spawn_blocking(move||{
                                let _slot=slot;
                                let result=d.menu(window).and_then(|items|out.blocking_send(Message::Menu{window,items}));
                                let _=out.blocking_send(Message::Ack{request,window,actual,error:result.err().map(|e|e.to_string())});
                            });
                        }
                    }else{
                        input.action(request,window,action,known.get(&window).cloned(),view_mode)?;
                    }
                },
                Message::SharedResume{generation,elapsed_ms}=>input.resume(generation,elapsed_ms)?,
                Message::SharedHeartbeat{generation}=>desktop.shared_heartbeat(generation)?,
                Message::SharedInput{window,generation,event,elapsed_ms}=>{if let Err(error)=input.shared_input(window,generation,event,elapsed_ms){io.send.send(Message::Error{message:error.to_string()}).await?;}},
                Message::InputClock{elapsed_ms}=>input.clock(elapsed_ms),
                Message::Input{window,event,elapsed_ms}=>input.input(window,event,elapsed_ms)?,
                Message::ReleaseInput=>input.release()?,
                message @ Message::Clipboard{..}=>{if let Some(worker)=&clipboard_worker{worker.push(message);}},
                Message::Transfer(TransferMessage::Accept{id,offset})=>{if let Some(path)=outgoing_files.lock().unwrap().get(&id).cloned(){let out=io.send.clone();let key=id.clone();outgoing_jobs.insert(key,tokio::spawn(async move{if let Err(error)=upload_chunks(&id,&path,offset,&out).await{let _=out.send(Message::Error{message:error.to_string()}).await;}}));}},
                Message::Transfer(TransferMessage::Cancel{id,..})=>{if let Some(job)=outgoing_jobs.remove(&id){job.abort();}transfers.cancel(&id).await?;},
                Message::Transfer(message)=>transfers.submit(message)?,
                Message::DropFiles{window,transfers:ids,x,y}=>{if desktop.allow_file_transfer(){let paths:Vec<_>=ids.iter().filter_map(|id|completed_files.lock().unwrap().get(id).cloned()).collect();if paths.len()==ids.len()&&!paths.is_empty(){let d=desktop.clone();let out=io.send.clone();tokio::task::spawn_blocking(move||{if let Err(error)=d.drop_files(window,&paths,x,y){let _=out.blocking_send(Message::Error{message:format!("File drop: {error}")});}});}}},
                Message::DownloadFile{path}=>if desktop.allow_file_transfer(){
                    let out=io.send.clone();let files=outgoing_files.clone();
                    tokio::spawn(async move{let result=async{
                        let home=std::env::var_os("USERPROFILE").or_else(||std::env::var_os("HOME")).map(PathBuf::from).context("no home directory")?;
                        let resolved=tokio::fs::canonicalize(home.join(path)).await?;let root=tokio::fs::canonicalize(home).await?;
                        anyhow::ensure!(resolved.starts_with(root)&&resolved.is_file(),"download must be a file inside the host user's home folder");
                        let(name,size,hash)=file_metadata(&resolved).await?;let id=transfer::transfer_id(&name,&hash,size);
                        files.lock().unwrap().insert(id.clone(),resolved);out.send(Message::Transfer(TransferMessage::Offer{id,name,size,sha256:hash})).await?;Ok::<(),anyhow::Error>(())
                    }.await;if let Err(error)=result{let _=out.send(Message::Error{message:error.to_string()}).await;}});
                },
                Message::Ping{nonce}=>io.send.send(Message::Pong{nonce}).await?,Message::Goodbye=>break,_=>{}
            }}
        }
    }
    if let Some(job) = window_scan.take() {
        job.abort();
    }
    drop(streams);
    drop(adb_bridge);
    drop(input);
    Ok(())
}

async fn connect(
    address: SocketAddr,
    codecs: Vec<CodecCapability>,
    purpose: SessionPurpose,
) -> anyhow::Result<(quinn::Endpoint, quinn::Connection, Control, Hello)> {
    let identity = Identity::load()?;
    let endpoint = transport::client(&identity)?;
    let connection = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        endpoint.connect(address, "frames.local")?,
    )
    .await
    .context(connection_timeout_hint())??;
    let (send, receive, peer) =
        handshake(&connection, &identity, codecs, false, true, true, purpose).await?;
    Ok((
        endpoint,
        connection.clone(),
        control(connection, send, receive),
        peer,
    ))
}

fn connection_timeout_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "QUIC connection timed out. Check the host address and UDP port, and allow Frames in System Settings > Privacy & Security > Local Network before retrying"
    } else {
        "QUIC connection timed out. Check the host address, UDP port, and firewall settings"
    }
}

pub async fn client(
    address: SocketAddr,
    requested: Option<Codec>,
    ui: frames_native::ClientSender,
    mut events: mpsc::UnboundedReceiver<ClientEvent>,
    clipboard: bool,
    audio: bool,
    initial_view: ViewMode,
) -> anyhow::Result<()> {
    let _update_lease = crate::update::session_lease()?;
    let codecs = codec::probe_decoders().await;
    anyhow::ensure!(
        !codecs.is_empty(),
        "no working media codecs; run frames doctor"
    );
    let delivery = ui_delivery::Delivery::new(ui.clone());
    let (_endpoint, connection, mut io, peer) =
        connect(address, codecs.clone(), SessionPurpose::Control).await?;
    if let Err(error) =
        crate::connection_history::remember(address, &peer.device_id, &peer.name, false)
    {
        tracing::warn!(%error,"could not save device history");
    }
    let shared = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            match io
                .receive
                .recv()
                .await
                .context("host closed before session policy")?
            {
                Message::SessionPolicy { shared } => return Ok::<_, anyhow::Error>(shared),
                Message::Error { message } => anyhow::bail!(message),
                Message::Goodbye => anyhow::bail!("host ended the session"),
                _ => {}
            }
        }
    })
    .await??;
    if let Some(session) = &shared {
        anyhow::ensure!(
            peer.shared_game == Some(frames_core::shared_game::Capabilities::V1)
                && session.generation != 0,
            "Host shared-game capabilities are incompatible"
        );
        delivery.send(ClientUpdate::SharedSession(Box::new(session.clone())))?;
    }
    let input_epoch = std::time::Instant::now();
    let mut last_shared_ui = input_epoch;
    let devices = if shared.is_none() {
        Some(crate::device_session::Client::start(
            connection.clone(),
            io.send.clone(),
            io.receive.devices.take().unwrap(),
            ui.clone(),
            input_epoch,
            peer.platform == "windows",
        ))
    } else {
        io.receive.devices.take();
        None
    };
    io.send.send(Message::InputClock { elapsed_ms: 0 }).await?;
    io.send.progress = Some(ui.clone());
    let selected = codec::negotiate(&peer.codecs, &codecs, requested)?;
    tracing::info!(codec=?selected.codec,encoder=?selected.encoder,"media negotiation");
    io.send
        .send(Message::SelectCodec { codec: requested })
        .await?;
    io.send
        .send(Message::AudioPreference { enabled: audio })
        .await?;
    io.send
        .send(Message::SetViewMode {
            mode: if shared.is_some() {
                ViewMode::Windows
            } else {
                initial_view
            },
        })
        .await?;
    delivery.send(ClientUpdate::Peer {
        platform: peer.platform.clone(),
        device_id: peer.device_id.clone(),
        name: peer.name.clone(),
    })?;
    let mut graph = WindowGraph::default();
    let media =
        crate::streaming::ClientMediaWorker::new(connection.clone(), ui.clone(), io.send.clone());
    let mut request = 0u64;
    let clipboard_worker = if clipboard && shared.is_none() {
        Some(ClipboardWorker::start(
            frames_native::desktop(vec![])?,
            "client".into(),
            io.send.clone(),
        )?)
    } else {
        None
    };
    let mut timer = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut ticks = 0;
    let mut uploads = HashMap::<String, (PathBuf, WindowId, f64, f64)>::new();
    let mut upload_jobs = HashMap::<String, tokio::task::JoinHandle<()>>::new();
    let downloads = crate::file_worker::FileWorker::new(
        downloads(),
        shared.is_none(),
        io.receive.bulk.take().unwrap(),
        io.send.clone(),
        Some(ui.clone()),
        Default::default(),
    );
    type PreparedFile = (
        PathBuf,
        WindowId,
        f64,
        f64,
        Result<(String, u64, String), String>,
    );
    let (prepared_tx, mut prepared_rx) = mpsc::channel::<PreparedFile>(4);
    let prepare_slots = Arc::new(Semaphore::new(2));
    let prepare_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut adb_enabled = false;
    let mut adb_requested = false;
    let (adb_gate, _) = tokio::sync::watch::channel(false);
    let mut adb_jobs = tokio::task::JoinSet::new();
    let mut pending_event = None;
    loop {
        tokio::select! {
            reason=connection.closed()=>{tracing::info!(%reason,"Host connection closed");break;},
            _=adb_jobs.join_next(),if !adb_jobs.is_empty()=>{},
            stream=connection.accept_bi()=>if let Ok((mut send,mut receive))=stream{
                if adb_jobs.len()>=16{let _=send.reset(1u32.into());let _=receive.stop(1u32.into());}
                else{
                    let exports=devices.as_ref().map(|d|d.exports()).unwrap_or_default();let media=io.send.media.clone();let adb_allowed=adb_enabled;let mut adb_cancel=adb_gate.subscribe();
                    adb_jobs.spawn(async move{
                        let result=async {
                            let mut magic=[0u8;8];tokio::time::timeout(std::time::Duration::from_secs(5),receive.read_exact(&mut magic)).await??;
                            if &magic==b"FADB0001" && adb_allowed { tokio::select! { result=frames_core::device_bridge::serve_local_after_magic(send,receive)=>result, _=adb_cancel.wait_for(|enabled| !*enabled)=>Ok(()) } }
                            else if &magic==frames_core::usbip::MAGIC { crate::device_session::serve_usb(send,receive,exports,media).await }
                            else { anyhow::bail!("Unrequested device bridge stream") }
                        }.await;
                        if let Err(error)=result{tracing::debug!(%error,"device stream ended");}
                    });
                }
            },
            prepared=prepared_rx.recv()=>if let Some((path,window,x,y,result))=prepared{match result{Ok((name,size,hash))=>{let id=transfer::transfer_id(&name,&hash,size);uploads.insert(id.clone(),(path,window,x,y));io.send.send(Message::Transfer(TransferMessage::Offer{id,name,size,sha256:hash})).await?;},Err(error)=>{let _=ui.try_send(ClientUpdate::Status(window,error));}}},
            _=timer.tick()=>{ticks+=1;
                if shared.is_none() && ticks%50==10{io.send.send(Message::PreviewRequest{include_image:true}).await?;}
                if let Some(session)=&shared && last_shared_ui.elapsed()<std::time::Duration::from_millis(300){io.send.send(Message::SharedHeartbeat{generation:session.generation}).await?;}
                if ticks%10==0{io.send.send(Message::InputClock{elapsed_ms:input_epoch.elapsed().as_millis() as u64}).await?;}

            },
            event=async{if pending_event.is_some(){pending_event.take()}else{events.recv().await}}=>match event{
                Some(ClientEvent::SharedAlive{created})=>{last_shared_ui=last_shared_ui.max(created);},
                Some(ClientEvent::ResumeInput{created})=>if let Some(session)=&shared && created.elapsed()<std::time::Duration::from_millis(300){io.send.send(Message::SharedResume{generation:session.generation,elapsed_ms:created.saturating_duration_since(input_epoch).as_millis() as u64}).await?;},
                Some(ClientEvent::Devices(action))=>if let Some(devices)=&devices{devices.command(action)},
                Some(ClientEvent::Decoded{window,generation,number,micros})=>media.send(crate::streaming::MediaCommand::Decoded(window,generation,number,micros)).await?,
                Some(ClientEvent::MediaRecovery{window,generation})=>media.send(crate::streaming::MediaCommand::Recover(window,generation)).await?,
                Some(ClientEvent::DecoderFailed{window,generation,reason})=>media.send(crate::streaming::MediaCommand::Failed(window,generation,reason)).await?,
                Some(ClientEvent::Presented{window,generation,number,at})=>media.send(crate::streaming::MediaCommand::Presented(window,generation,number,at)).await?,
                Some(ClientEvent::RemoteCursor{visible})=>io.send.send(Message::SetRemoteCursor{visible}).await?,
                Some(ClientEvent::ViewMode(mode))=>{if shared.is_some(){continue;}io.send.send(Message::ReleaseInput).await?;io.send.send(Message::SetViewMode{mode}).await?;},
                Some(ClientEvent::AdbBridge{enabled})=>{if shared.is_some(){continue;}
                    let result=if enabled{frames_core::device_bridge::probe_local_adb().await}else{Ok(())};
                    match result{Ok(())=>{adb_requested=enabled;adb_enabled=enabled;adb_gate.send_replace(adb_enabled);io.send.send(Message::AdbBridge{enabled}).await?;},Err(error)=>{delivery.send(ClientUpdate::AdbBridge{enabled:false,address:None,error:Some(error.to_string())})?;}}
                },
                Some(ClientEvent::Visibility{window,visible,focused})=>{if focused && let Some(devices)=&devices{devices.command(frames_core::devices::DeviceAction::Focus{active:true});}
                    if !visible{media.send(crate::streaming::MediaCommand::Pause(window)).await?;}io.send.send(Message::Visibility{window,visible,focused}).await?;},
                Some(ClientEvent::Action{window,action})=>{if shared.is_some(){continue;}request+=1;io.send.send(Message::Command{request,window,action}).await?;},
                Some(event @ (ClientEvent::Input{..}|ClientEvent::TimedInput{..}))=>{
                    let (mut window,mut event,mut created)=match event {
                        ClientEvent::Input{window,event}=>(window,event,std::time::Instant::now()),
                        ClientEvent::TimedInput{window,event,created}=>(window,event,created),
                        _=>unreachable!()
                    };
                    if matches!(event,frames_core::input::InputEvent::Pointer{..}) {
                        for _ in 0..4096 {
                            let Ok(next)=events.try_recv() else{break;};
                            match next {
                                ClientEvent::Input{window:w,event:e} if matches!(e,frames_core::input::InputEvent::Pointer{..})=>{window=w;event=e;created=std::time::Instant::now();},
                                ClientEvent::TimedInput{window:w,event:e,created:c} if matches!(e,frames_core::input::InputEvent::Pointer{..})=>{window=w;event=e;created=c;},
                                other=>{pending_event=Some(other);break;}
                            }
                        }
                    }
                    let elapsed_ms=created.saturating_duration_since(input_epoch).as_millis() as u64;
                    if let Some(session)=&shared {if window==session.window&&event.valid(){io.send.send(Message::SharedInput{window,generation:session.generation,event,elapsed_ms}).await?;}}
                    else{io.send.send(Message::Input{window,event,elapsed_ms}).await?;}
                },
                Some(ClientEvent::SendFiles{window,paths,x,y})=>{if shared.is_some(){continue;}for path in paths.into_iter().take(100){let tx=prepared_tx.clone();let slots=prepare_slots.clone();let epoch=prepare_epoch.clone();let version=epoch.load(std::sync::atomic::Ordering::Relaxed);tokio::spawn(async move{let Ok(_permit)=slots.acquire_owned().await else{return;};let result=file_metadata(&path).await.map_err(|e|e.to_string());if epoch.load(std::sync::atomic::Ordering::Relaxed)==version{let _=tx.send((path,window,x,y,result)).await;}});}},
                Some(ClientEvent::CancelTransfers)=>{prepare_epoch.fetch_add(1,std::sync::atomic::Ordering::Relaxed);let mut ids=downloads.active_ids();ids.extend(uploads.keys().cloned());for id in ids{if let Some(job)=upload_jobs.remove(&id){job.abort();}uploads.remove(&id);downloads.cancel(&id).await?;io.send.send(Message::Transfer(TransferMessage::Cancel{id,reason:"canceled locally".into()})).await?;}let _=ui.try_send(ClientUpdate::TransferProgress{name:"Transfers canceled".into(),done:0,total:0});},
                Some(ClientEvent::DownloadFile{path})=>if shared.is_none(){io.send.send(Message::DownloadFile{path}).await?;},
                Some(ClientEvent::ReleaseInput)=>{if let Some(devices)=&devices{devices.command(frames_core::devices::DeviceAction::Release);}io.send.send(Message::ReleaseInput).await?;},
                Some(ClientEvent::Disconnected)|None=>{tracing::info!("Client UI requested disconnect or closed its event channel");break;},
            },
            message=io.receive.recv()=>{let Some(message)=message else{tracing::info!("Host control stream closed");break;};match message{
                Message::DevicePreview{png_base64:Some(png)}=>{
                    if png.len()<=crate::device_preview::MAX_PREVIEW_BYTES*4/3+4 {
                        let id=peer.device_id.clone();
                        tokio::task::spawn_blocking(move||{
                            if let Ok(bytes)=base64::engine::general_purpose::STANDARD.decode(png){
                                let trusted=TrustStore::load().is_ok_and(|store|store.peers.contains_key(&id));
                                let _=crate::device_preview::save(&id,&bytes,trusted);
                            }
                        });
                    }
                },
                Message::SharedState(state)=>{if shared.as_ref().is_some_and(|s|s.window==state.window&&s.generation==state.generation){delivery.send(ClientUpdate::SharedSession(Box::new(state)))?;}},
                Message::SharedCursor{window,generation,cursor}=>{if cursor.valid() && shared.as_ref().is_some_and(|s|s.window==window&&s.generation==generation){delivery.send(ClientUpdate::SharedCursor(cursor))?;}},
                Message::RemoteCursorChanged{visible}=>delivery.send(ClientUpdate::RemoteCursor{visible})?,
                Message::ViewModeChanged{mode,error}=>{if error.is_none(){media.send(crate::streaming::MediaCommand::Clear).await?;}delivery.send(ClientUpdate::ViewMode{mode,error})?;},
                Message::AdbBridgeStatus{enabled,address,error}=>{
                    adb_enabled=enabled&&adb_requested;adb_gate.send_replace(adb_enabled);

                    if enabled&&!adb_requested{io.send.send(Message::AdbBridge{enabled:false}).await?;}
                    if !enabled{adb_requested=false;}
                    delivery.send(ClientUpdate::AdbBridge{enabled:adb_enabled,address:if adb_enabled{address}else{None},error})?;
                },
                Message::Snapshot{sequence,windows}=>{graph.snapshot(sequence,windows)?;media.send(crate::streaming::MediaCommand::Reconcile(graph.windows.keys().copied().collect())).await?;delivery.send(ClientUpdate::Windows(graph.windows.values().cloned().collect()))?;},
                Message::Delta{sequence,upsert,removed}=>{graph.delta(sequence,upsert,&removed)?;media.send(crate::streaming::MediaCommand::Reconcile(graph.windows.keys().copied().collect())).await?;delivery.send(ClientUpdate::Windows(graph.windows.values().cloned().collect()))?;},
                Message::ClockReply{client_us,received_us,sent_us}=>media.send(crate::streaming::MediaCommand::Clock(client_us,received_us,sent_us,frames_core::media::monotonic_micros())).await?,
                Message::MediaStatus{window,generation,number}=>media.send(crate::streaming::MediaCommand::Status(window,generation,number)).await?,
                Message::StreamStarted(config)=>{media.send(crate::streaming::MediaCommand::Start(config)).await?;},
                Message::StreamStopped{window,generation}=>media.send(crate::streaming::MediaCommand::Stop(window,generation)).await?,
                Message::StreamPaused{window}=>media.send(crate::streaming::MediaCommand::Pause(window)).await?,
                Message::AudioStarted{generation}=>media.send(crate::streaming::MediaCommand::Audio(generation)).await?,
                Message::Ack{window,actual,error,..}=>delivery.send(ClientUpdate::Ack(window,actual,error))?,
                Message::Menu{window,items}=>{validate_menu(&items)?;delivery.send(ClientUpdate::Menu{window,items})?;},
                Message::Transfer(TransferMessage::Accept{id,offset})=>if let Some((path,..))=uploads.get(&id){let path=path.clone();let out=io.send.clone();let key=id.clone();upload_jobs.insert(key,tokio::spawn(async move{if let Err(error)=upload_chunks(&id,&path,offset,&out).await{let _=out.send(Message::Error{message:error.to_string()}).await;}}));},
                Message::Transfer(TransferMessage::Finished{id,..})=>if let Some((_,window,x,y))=uploads.remove(&id){io.send.send(Message::DropFiles{window,transfers:vec![id],x,y}).await?;},
                Message::Transfer(message) => downloads.submit(message)?,
        message @ Message::Clipboard{..}=>{if let Some(worker)=&clipboard_worker{worker.push(message);}},
                Message::Error{message}=>{tracing::warn!(%message,"host error");if let Some(session)=&shared{delivery.send(ClientUpdate::Status(session.window,message))?;}else if let Some((&window,_))=graph.windows.iter().next(){delivery.send(ClientUpdate::Ack(window,None,Some(message)))?;}},Message::Goodbye=>{tracing::info!("Host sent session goodbye");break;},_=>{}
            }}
        }
    }
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        io.send.send(Message::ReleaseInput),
    )
    .await;
    if let Some(quinn::ConnectionError::ApplicationClosed(reason)) = connection.close_reason()
        && reason.error_code != 0u32.into()
    {
        anyhow::bail!(
            "Host ended the session: {}",
            String::from_utf8_lossy(&reason.reason)
        );
    }
    connection.close(0u32.into(), b"client disconnected");
    Ok(())
}

pub(crate) async fn receive_transfer(
    receiver: &mut transfer::Receiver,
    message: TransferMessage,
) -> anyhow::Result<Option<TransferMessage>> {
    Ok(match message {
        TransferMessage::Offer {
            id,
            name,
            size,
            sha256,
        } => Some(receiver.offer(&id, &name, size, &sha256).await?),
        TransferMessage::Chunk {
            id,
            offset,
            data_base64,
        } => {
            let bytes = base64::engine::general_purpose::STANDARD.decode(data_base64)?;
            receiver.chunk(&id, offset, &bytes).await?;
            None
        }
        TransferMessage::Complete { id } => {
            if receiver.is_canceled(&id) {
                None
            } else {
                Some(receiver.finish(&id).await?)
            }
        }
        TransferMessage::Cancel { id, .. } => {
            receiver.cancel(&id).await?;
            None
        }
        _ => None,
    })
}

// Native clipboard calls (and PNG conversion) can block. Keep them off both
// session loops, with a replaceable pending remote value instead of a backlog.
struct ClipboardWorker {
    shared: Arc<(std::sync::Mutex<Option<Message>>, std::sync::Condvar)>,
    stopped: Arc<std::sync::atomic::AtomicBool>,
}
impl ClipboardWorker {
    fn start(desktop: Arc<dyn Desktop>, origin: String, out: Outbound) -> anyhow::Result<Self> {
        let shared = Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new()));
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let pending = shared.clone();
        let stop = stopped.clone();
        std::thread::Builder::new()
            .name("frames-clipboard".into())
            .spawn(move || {
                let mut state = frames_core::clipboard::ClipboardState::default();
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    let (lock, ready) = &*pending;
                    let message = {
                        let mut slot = lock.lock().unwrap();
                        if slot.is_none() && !stop.load(std::sync::atomic::Ordering::Acquire) {
                            slot = ready
                                .wait_timeout(slot, std::time::Duration::from_millis(500))
                                .unwrap()
                                .0;
                        }
                        slot.take()
                    };
                    if stop.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let result = if let Some(Message::Clipboard {
                        origin,
                        revision,
                        text,
                        png_base64,
                    }) = message
                    {
                        apply_clipboard(
                            desktop.as_ref(),
                            &mut state,
                            &origin,
                            revision,
                            text,
                            png_base64,
                        )
                    } else {
                        poll_clipboard(desktop.as_ref(), &mut state, &origin, &out)
                    };
                    if let Err(error) = result {
                        tracing::debug!(%error, "clipboard unavailable");
                    }
                }
            })?;
        Ok(Self { shared, stopped })
    }
    fn push(&self, message: Message) {
        *self.shared.0.lock().unwrap() = Some(message);
        self.shared.1.notify_one();
    }
}
impl Drop for ClipboardWorker {
    fn drop(&mut self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        self.shared.1.notify_one();
    }
}
fn poll_clipboard(
    desktop: &dyn Desktop,
    state: &mut frames_core::clipboard::ClipboardState,
    origin: &str,
    out: &Outbound,
) -> anyhow::Result<()> {
    use frames_core::clipboard::Content;
    let content = if let Ok(Some(png)) = desktop.clipboard_image() {
        if png.len() > 2 * 1024 * 1024 {
            return Ok(());
        }
        Some(Content::Png(png))
    } else {
        desktop
            .clipboard()
            .ok()
            .flatten()
            .filter(|s| s.len() <= 2 * 1024 * 1024)
            .map(Content::Text)
    };
    if let Some(content) = content {
        let mut next = state.clone();
        if let Some(revision) = next.local(origin, &content) {
            let (text, png_base64) = match content {
                Content::Text(text) => (Some(text), None),
                Content::Png(png) => (
                    None,
                    Some(base64::engine::general_purpose::STANDARD.encode(png)),
                ),
            };
            let message = Message::Clipboard {
                origin: origin.into(),
                revision,
                text,
                png_base64,
            };
            if serde_json::to_vec(&message)?.len() <= protocol::MAX_CONTROL_BYTES {
                out.blocking_send(message)?;
                *state = next;
            }
        }
    }
    Ok(())
}
fn apply_clipboard(
    desktop: &dyn Desktop,
    state: &mut frames_core::clipboard::ClipboardState,
    origin: &str,
    revision: u64,
    text: Option<String>,
    png: Option<String>,
) -> anyhow::Result<()> {
    use frames_core::clipboard::Content;
    anyhow::ensure!(
        origin.len() <= 128 && revision < u64::MAX / 2,
        "invalid clipboard revision"
    );
    let content = if let Some(png) = png {
        let bytes = base64::engine::general_purpose::STANDARD.decode(png)?;
        anyhow::ensure!(bytes.len() <= 2 * 1024 * 1024, "clipboard image too large");
        Content::Png(bytes)
    } else if let Some(text) = text {
        anyhow::ensure!(text.len() <= 2 * 1024 * 1024, "clipboard text too large");
        Content::Text(text)
    } else {
        return Ok(());
    };
    let mut next = state.clone();
    if next.remote(origin, revision, &content) {
        match content {
            Content::Text(text) => desktop.set_clipboard(&text)?,
            Content::Png(png) => desktop.set_clipboard_image(&png)?,
        }
    }
    *state = next;
    Ok(())
}

pub async fn send_file(address: SocketAddr, path: PathBuf) -> anyhow::Result<()> {
    let size = tokio::fs::metadata(&path).await?.len();
    let sha256 = transfer::hash_file(&path).await?;
    let name = path
        .file_name()
        .context("file has no name")?
        .to_str()
        .context("non-Unicode filenames are unsupported")?
        .to_owned();
    transfer::safe_name(&name)?;
    let id = transfer::transfer_id(&name, &sha256, size);
    let (endpoint, connection, mut io, _peer) =
        connect(address, vec![], SessionPurpose::Transfer).await?;
    io.send
        .send(Message::Transfer(TransferMessage::Offer {
            id: id.clone(),
            name,
            size,
            sha256,
        }))
        .await?;
    let mut uploaded = false;
    let mut finished = false;
    while let Some(message) = io.receive.recv().await {
        match message {
            Message::Transfer(TransferMessage::Accept { id: ack, offset }) if ack == id => {
                anyhow::ensure!(offset <= size, "peer requested invalid resume offset");
                let mut file = tokio::fs::File::open(&path).await?;
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                let mut position = offset;
                let mut buffer = vec![0; transfer::CHUNK_BYTES];
                loop {
                    let n = file.read(&mut buffer).await?;
                    if n == 0 {
                        break;
                    }
                    io.send
                        .send(Message::Transfer(TransferMessage::Chunk {
                            id: id.clone(),
                            offset: position,
                            data_base64: base64::engine::general_purpose::STANDARD
                                .encode(&buffer[..n]),
                        }))
                        .await?;
                    position += n as u64;
                    tokio::time::sleep(std::time::Duration::from_millis(6)).await;
                }
                io.send
                    .send(Message::Transfer(TransferMessage::Complete {
                        id: id.clone(),
                    }))
                    .await?;
                uploaded = true;
            }
            Message::Transfer(TransferMessage::Finished { id: ack, name }) if ack == id => {
                println!("Transferred {size} bytes as {name}");
                finished = true;
                break;
            }
            Message::Error { message } => anyhow::bail!("{message}"),
            _ => {}
        }
    }
    anyhow::ensure!(
        uploaded && finished,
        "transfer ended before verified completion"
    );
    connection.close(0u32.into(), b"transfer complete");
    drop(io);
    endpoint.wait_idle().await;
    Ok(())
}

async fn file_metadata(path: &std::path::Path) -> anyhow::Result<(String, u64, String)> {
    let metadata = tokio::fs::metadata(path).await?;
    anyhow::ensure!(
        metadata.is_file(),
        "select regular files; folder transfer is not supported"
    );
    let name = path
        .file_name()
        .context("missing filename")?
        .to_str()
        .context("non-Unicode filename")?
        .to_owned();
    transfer::safe_name(&name)?;
    let hash = transfer::hash_file(path).await?;
    Ok((name, metadata.len(), hash))
}
async fn upload_chunks(
    id: &str,
    path: &std::path::Path,
    offset: u64,
    out: &Outbound,
) -> anyhow::Result<()> {
    let mut file = tokio::fs::File::open(path).await?;
    anyhow::ensure!(
        offset <= file.metadata().await?.len(),
        "invalid resume offset"
    );
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut position = offset;
    let total = file.metadata().await?.len();
    let name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    out.progress(name.clone(), offset, total);
    let mut last_progress = std::time::Instant::now();
    let mut bytes = vec![0; transfer::CHUNK_BYTES];
    loop {
        let count = file.read(&mut bytes).await?;
        if count == 0 {
            break;
        }
        out.send(Message::Transfer(TransferMessage::Chunk {
            id: id.into(),
            offset: position,
            data_base64: base64::engine::general_purpose::STANDARD.encode(&bytes[..count]),
        }))
        .await?;
        position += count as u64;
        if last_progress.elapsed() > std::time::Duration::from_millis(100) {
            out.progress(name.clone(), position, total);
            last_progress = std::time::Instant::now();
        }
        tokio::time::sleep(std::time::Duration::from_millis(6)).await;
    }
    out.progress(name, position, total);
    out.send(Message::Transfer(TransferMessage::Complete {
        id: id.into(),
    }))
    .await?;
    Ok(())
}
pub async fn fetch_file(address: SocketAddr, path: String) -> anyhow::Result<()> {
    let (endpoint, connection, mut io, _) =
        connect(address, vec![], SessionPurpose::Transfer).await?;
    let mut receiver = transfer::Receiver::new(downloads()).await?;
    io.send.send(Message::DownloadFile { path }).await?;
    let mut finished = false;
    while let Some(message) = io.receive.recv().await {
        match message {
            Message::Transfer(message) => {
                if let Some(reply) = receive_transfer(&mut receiver, message).await? {
                    if let TransferMessage::Finished { name, .. } = &reply {
                        println!("Downloaded {}", receiver.received_path(name)?.display());
                        finished = true;
                    }
                    io.send.send(Message::Transfer(reply)).await?;
                    if finished {
                        break;
                    }
                }
            }
            Message::Error { message } => anyhow::bail!("{message}"),
            _ => {}
        }
    }
    anyhow::ensure!(finished, "download ended before verified completion");
    connection.close(0u32.into(), b"download complete");
    drop(io);
    endpoint.wait_idle().await;
    Ok(())
}
