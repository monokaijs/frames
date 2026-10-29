use anyhow::Context;
use base64::Engine;
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

fn hello(identity: &Identity, codecs: Vec<CodecCapability>, purpose: SessionPurpose) -> Message {
    Message::Hello(Hello {
        version: frames_core::PROTOCOL_VERSION,
        device_id: identity.device_id.clone(),
        name: std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "Frames device".into()),
        platform: std::env::consts::OS.into(),
        codecs,
        purpose,
    })
}

async fn approval(name: String, code: String, reason: &'static str) -> anyhow::Result<bool> {
    tokio::task::spawn_blocking(move || {
        if std::env::var_os("FRAMES_PAIRING_CONSOLE").is_some() {
            use std::io::{BufRead, Write};
            eprintln!("\n{reason}: {name:?}\nVerify the SAME code on both devices: {code}");
            eprint!("Type yes to approve: ");
            std::io::stderr().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            Ok(line.trim() == "yes")
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
    let needs_approval = if server {
        frames_core::trust::incoming_needs_approval(known, unattended)
    } else {
        known.is_none()
    };
    if needs_approval {
        anyhow::ensure!(
            !server || allow_pair || known.is_some_and(|p| p.allow_control),
            "new device pairing disabled; start host with --pair"
        );
        anyhow::ensure!(
            approval(
                peer.name.clone(),
                transport::verification_code(connection)?,
                if known.is_none() {
                    "Pair new device"
                } else {
                    "Approve session"
                }
            )
            .await?,
            "pairing/session rejected"
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
    if needs_approval {
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
    control: mpsc::Sender<Message>,
    bulk: mpsc::Sender<Message>,
    progress: Option<std::sync::mpsc::SyncSender<ClientUpdate>>,
}
impl Outbound {
    fn progress(&self, name: String, done: u64, total: u64) {
        if let Some(ui) = &self.progress {
            let _ = ui.try_send(ClientUpdate::TransferProgress { name, done, total });
        }
    }
    pub async fn send(&self, message: Message) -> anyhow::Result<()> {
        let channel = if matches!(
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
struct Control {
    send: Outbound,
    receive: mpsc::Receiver<Message>,
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
    let (tx, mut outgoing) = mpsc::channel::<Message>(64);
    let (bulk_tx, mut bulk) = mpsc::channel::<Message>(4);
    let (incoming, rx) = mpsc::channel(64);
    let conn = connection.clone();
    let reader_incoming = incoming.clone();
    let bulk_connection = connection.clone();
    let write = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            if protocol::send_json(&mut writer, &message).await.is_err() {
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
                    tracing::debug!(%error,"control reader ended");
                    break;
                }
            }
        }
        connection.close(0u32.into(), b"control reader ended");
    });
    let outgoing_connection = bulk_connection.clone();
    let bulk_write = tokio::spawn(async move {
        let Ok(mut writer) = outgoing_connection.open_uni().await else {
            return;
        };
        let _ = writer.set_priority(-10);
        while let Some(message) = bulk.recv().await {
            if protocol::send_json(&mut writer, &message).await.is_err() {
                break;
            }
        }
    });
    let bulk_read = tokio::spawn(async move {
        while let Ok(mut reader) = bulk_connection.accept_uni().await {
            let incoming = incoming.clone();
            let conn = bulk_connection.clone();
            tokio::spawn(async move {
                while let Ok(message) = protocol::recv_json(&mut reader).await {
                    if !matches!(
                        &message,
                        Message::Transfer(
                            TransferMessage::Chunk { .. } | TransferMessage::Complete { .. }
                        )
                    ) {
                        conn.close(1u32.into(), b"invalid bulk message");
                        break;
                    }
                    if incoming.send(message).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Control {
        send: Outbound {
            control: tx,
            bulk: bulk_tx,
            progress: None,
        },
        receive: rx,
        tasks: vec![write, read, bulk_write, bulk_read],
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
    anyhow::ensure!((1..=32).contains(&max_streams), "max-streams must be 1..32");
    let identity = Arc::new(Identity::load()?);
    let codecs = codec::probe().await;
    anyhow::ensure!(!codecs.is_empty(), "no working codecs; run frames doctor");
    let desktop = frames_native::desktop(exclude)?;
    let endpoint = transport::server(&identity, address)?;
    let _advertisement = if std::env::var_os("FRAMES_NO_DISCOVERY").is_none() {
        Some(frames_core::discovery::advertise(
            &format!("Frames-{}", &identity.device_id[..8]),
            &identity.device_id,
            endpoint.local_addr()?.port(),
        )?)
    } else {
        None
    };
    let semaphore = Arc::new(Semaphore::new(1));
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
    loop {
        tokio::select! {
            _=tokio::signal::ctrl_c()=>break,
            incoming=endpoint.accept()=>{
                let Some(incoming)=incoming else{break;};
                let Ok(pending_permit)=pending_slots.clone().try_acquire_owned()else{incoming.refuse();continue;};
                let(identity,desktop,codecs,receive_dir)=(identity.clone(),desktop.clone(),codecs.clone(),receive_dir.clone());
                let(controller_slots,transfer_slots,active)=(semaphore.clone(),transfer_slots.clone(),active.clone());
                let active_transfers=active_transfers.clone();
                tokio::spawn(async move{
                    let result=async{
                        let connection=tokio::time::timeout(std::time::Duration::from_secs(10),incoming).await??;
                        let(send,receive,peer)=handshake(&connection,&identity,codecs.clone(),true,pair,unattended,SessionPurpose::Control).await?;
                        drop(pending_permit);
                        let controlling=peer.purpose==SessionPurpose::Control;
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
    }
    endpoint.close(0u32.into(), b"host shutting down");
    desktop.release_input();
    endpoint.wait_idle().await;
    Ok(())
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
    let epoch = uuid::Uuid::new_v4().to_string();
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
    let mut transfers = transfer::Receiver::new(receive_dir).await?;
    let mut preference = None;
    let mut failures = HashMap::<WindowId, u32>::new();
    let mut audio = None::<crate::streaming::HostStream>;
    let mut completed_files = HashMap::<String, PathBuf>::new();
    let mut outgoing_files = HashMap::<String, PathBuf>::new();
    let mut outgoing_jobs = HashMap::<String, tokio::task::JoinHandle<()>>::new();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut clip_state = frames_core::clipboard::ClipboardState::default();
    let mut scan = 0u64;
    let mut view_mode = ViewMode::Windows;
    let mut adb_bridge = None::<frames_core::device_bridge::HostBridge>;
    loop {
        tokio::select! {
            _=connection.closed()=>break,
            packet=connection.read_datagram()=>{if peer.purpose==SessionPurpose::Control&&let Ok(packet)=packet&& packet.starts_with(b"FP")&&packet.len()<2048&& let Ok(Message::Input{window,event:frames_core::input::InputEvent::Pointer{x,y}})=serde_json::from_slice(&packet[2..]){let _=desktop.input(window,&frames_core::input::InputEvent::Pointer{x,y});}},
            _=tick.tick()=>{
                scan+=1;
                if scan.is_multiple_of(10){anyhow::ensure!(TrustStore::load()?.peers.get(&peer.device_id).is_some_and(|p|p.allow_control),"device access revoked");}
                if peer.purpose==SessionPurpose::Transfer{continue;}
                let d=desktop.clone();let list=tokio::task::spawn_blocking(move||d.surfaces(view_mode)).await??;
                let next:HashMap<_,_>=list.iter().map(|w|(w.key,w.clone())).collect();
                if sequence==0{sequence=1;io.send.send(Message::Snapshot{sequence,windows:list}).await?;}
                else{
                    let upsert:Vec<_>=list.into_iter().filter(|w|known.get(&w.key)!=Some(w)).collect();let removed:Vec<_>=known.keys().filter(|k|!next.contains_key(k)).copied().collect();
                    if !upsert.is_empty()||!removed.is_empty(){sequence+=1;io.send.send(Message::Delta{sequence,upsert,removed:removed.clone()}).await?;for key in removed{streams.remove(&key);subscriptions.remove(&key);}}
                }
                known=next;
                let ended:Vec<_>=streams.iter().filter(|(_,s)|s.task.is_finished()).map(|(&key,_)|key).collect();for key in ended{streams.remove(&key);*failures.entry(key).or_default()+=1;}
                let desired=frames_core::scheduler::select(subscriptions.iter().map(|(&key,sub)|frames_core::scheduler::Demand{
                    window:key,visible:sub.visible&&known.get(&key).is_some_and(|w|w.visible&&!w.minimized),focused:sub.focused,running:streams.contains_key(&key)
                }),max_streams);
                let evicted:Vec<_>=streams.keys().filter(|key|!desired.contains(key)).copied().collect();
                for key in evicted{if let Some(mut stream)=streams.remove(&key){stream.shutdown().await;io.send.send(Message::StreamPaused{window:key}).await?;}}
                for key in desired{
                    let sub=&subscriptions[&key];
                    if let std::collections::hash_map::Entry::Vacant(entry)=streams.entry(key){
                        if failures.get(&key).copied().unwrap_or(0)>1&&!scan.is_multiple_of(50){continue;}
                        let selected=if preference.is_none()&&failures.get(&key).copied().unwrap_or(0)>0{Some(Codec::Vp9)}else{preference};
                        let cap=codec::negotiate(&codecs,&peer.codecs,selected)?;
                        entry.insert(crate::streaming::start_host(desktop.clone(),key,cap,sub.fps(),connection.clone(),io.send.clone(),generations.clone()));
                    }
                }
                if clipboard&&scan.is_multiple_of(5){poll_clipboard(desktop.as_ref(),&mut clip_state,&epoch,&io.send).await?;}
            },
            message=io.receive.recv()=>{let Some(message)=message else{break;};
                if peer.purpose==SessionPurpose::Transfer{anyhow::ensure!(matches!(&message,Message::Transfer(_)|Message::DownloadFile{..}|Message::Ping{..}|Message::Pong{..}|Message::Goodbye),"transfer sessions cannot control the desktop");}
                if !desktop.allow_file_transfer()&&matches!(&message,Message::Transfer(_)|Message::DownloadFile{..}|Message::DropFiles{..}){
                    io.send.send(Message::Error{message:"File transfers require a user-session host; the privileged login agent does not perform file operations".into()}).await?;
                    continue;
                }
                match message{
                Message::SetViewMode{mode}=>{
                    let d=desktop.clone();let result=tokio::task::spawn_blocking(move||d.surfaces(mode)).await?;
                    match result{
                        Ok(list)=>{
                            desktop.release_input();for(_,mut stream)in streams.drain(){stream.shutdown().await;}
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
                Message::Visibility{window,visible,focused}=>{if known.contains_key(&window){let subscription=subscriptions.entry(window).or_default();let changed_focus=subscription.focused!=focused;subscription.update(visible,focused);if !visible||changed_focus{streams.remove(&window);}}},
                Message::SelectCodec{codec}=>{codec::negotiate(&codecs,&peer.codecs,codec)?;preference=codec;streams.clear();failures.clear();},
                Message::StreamReady{window,..}=>if window==frames_core::media::AUDIO_WINDOW{if let Some(audio)=&audio{audio.ready.notify_one();}}else if let Some(stream)=streams.get(&window){stream.ready.notify_one();},
                Message::Keyframe{window}=>if window==frames_core::media::AUDIO_WINDOW{if let Some(audio)=&audio{audio.reset.notify_one();}}else if let Some(stream)=streams.get(&window){stream.reset.notify_one();},
                Message::AudioPreference{enabled}=>{audio=None;if enabled{audio=Some(crate::streaming::start_audio(desktop.clone(),connection.clone(),io.send.clone(),generations.clone()));}},
                Message::Command{request,window,action}=>{let result=if matches!(&action,WindowAction::Menu){match desktop.menu(window){Ok(items)=>{io.send.send(Message::Menu{window,items}).await?;Ok(())},Err(error)=>Err(error)}}else{desktop.action(window,&action)};let actual=desktop.surfaces(view_mode)?.into_iter().find(|w|w.key==window);io.send.send(Message::Ack{request,window,actual,error:result.err().map(|e|e.to_string())}).await?;},
                Message::Input{window,event}=>{if let Err(error)=desktop.input(window,&event){tracing::debug!(%error,"input rejected");}},
                Message::ReleaseInput=>desktop.release_input(),
                Message::Clipboard{origin,revision,text,png_base64}if clipboard=>{if let Err(error)=apply_clipboard(desktop.as_ref(),&mut clip_state,&origin,revision,text,png_base64){tracing::debug!(%error,"clipboard update unavailable");}},
                Message::Transfer(TransferMessage::Accept{id,offset})=>{if let Some(path)=outgoing_files.get(&id).cloned(){let out=io.send.clone();let key=id.clone();outgoing_jobs.insert(key,tokio::spawn(async move{if let Err(error)=upload_chunks(&id,&path,offset,&out).await{let _=out.send(Message::Error{message:error.to_string()}).await;}}));}},
                Message::Transfer(TransferMessage::Cancel{id,..})=>{if let Some(job)=outgoing_jobs.remove(&id){job.abort();}transfers.cancel(&id).await?;},
                Message::Transfer(message)=>{if !desktop.allow_file_transfer(){io.send.send(Message::Error{message:"File transfers require a user-session host; the privileged login agent does not perform file operations".into()}).await?;}else{match receive_transfer(&mut transfers,message).await{Ok(Some(reply))=>{if let TransferMessage::Finished{id,name}=&reply{completed_files.insert(id.clone(),transfers.received_path(name)?);}io.send.send(Message::Transfer(reply)).await?;},Ok(None)=>{},Err(error)=>io.send.send(Message::Error{message:format!("File transfer: {error}")}).await?}}},
                Message::DropFiles{window,transfers:ids,x,y}=>{if desktop.allow_file_transfer(){let paths:Vec<_>=ids.iter().filter_map(|id|completed_files.get(id).cloned()).collect();if paths.len()==ids.len()&&!paths.is_empty(){let d=desktop.clone();let out=io.send.clone();tokio::task::spawn_blocking(move||{if let Err(error)=d.drop_files(window,&paths,x,y){let _=out.blocking_send(Message::Error{message:format!("File drop: {error}")});}});}}},
                Message::DownloadFile{path}=>{if desktop.allow_file_transfer(){let home=std::env::var_os("USERPROFILE").or_else(||std::env::var_os("HOME")).map(PathBuf::from).context("no home directory")?;let resolved=tokio::fs::canonicalize(home.join(path)).await?;let root=tokio::fs::canonicalize(home).await?;anyhow::ensure!(resolved.starts_with(root)&&resolved.is_file(),"download must be a file inside the host user's home folder");let(name,size,hash)=file_metadata(&resolved).await?;let id=transfer::transfer_id(&name,&hash,size);outgoing_files.insert(id.clone(),resolved);io.send.send(Message::Transfer(TransferMessage::Offer{id,name,size,sha256:hash})).await?;}},
                Message::Ping{nonce}=>io.send.send(Message::Pong{nonce}).await?,Message::Goodbye=>break,_=>{}
            }}
        }
    }
    drop(streams);
    drop(adb_bridge);
    if peer.purpose == SessionPurpose::Control {
        desktop.release_input();
    }
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
    ui: std::sync::mpsc::SyncSender<ClientUpdate>,
    mut events: mpsc::UnboundedReceiver<ClientEvent>,
    clipboard: bool,
    audio: bool,
    initial_view: ViewMode,
) -> anyhow::Result<()> {
    let codecs = codec::probe().await;
    anyhow::ensure!(
        !codecs.is_empty(),
        "no working media codecs; run frames doctor"
    );
    let (_endpoint, connection, mut io, peer) =
        connect(address, codecs.clone(), SessionPurpose::Control).await?;
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
        .send(Message::SetViewMode { mode: initial_view })
        .await?;
    ui.send(ClientUpdate::Peer {
        platform: peer.platform.clone(),
        device_id: peer.device_id.clone(),
        name: peer.name.clone(),
    })?;
    let mut graph = WindowGraph::default();
    let mut media = crate::streaming::ClientMedia::default();
    let mut request = 0u64;
    let local = frames_native::desktop(vec![])?;
    let mut clip_state = frames_core::clipboard::ClipboardState::default();
    let mut timer = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut ticks = 0;
    let mut uploads = HashMap::<String, (PathBuf, WindowId, f64, f64)>::new();
    let mut upload_jobs = HashMap::<String, tokio::task::JoinHandle<()>>::new();
    let mut downloads = transfer::Receiver::new(downloads()).await?;
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
    let mut adb_jobs = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _=connection.closed()=>break,
            _=adb_jobs.join_next(),if !adb_jobs.is_empty()=>{},
            stream=connection.accept_bi()=>if let Ok((mut send,mut receive))=stream{
                if !adb_enabled||adb_jobs.len()>=8{let _=send.reset(1u32.into());let _=receive.stop(1u32.into());}
                else{adb_jobs.spawn(async move{if let Err(error)=frames_core::device_bridge::serve_local(send,receive).await{tracing::debug!(%error,"ADB device stream ended");}});}
            },
            prepared=prepared_rx.recv()=>if let Some((path,window,x,y,result))=prepared{match result{Ok((name,size,hash))=>{let id=transfer::transfer_id(&name,&hash,size);uploads.insert(id.clone(),(path,window,x,y));io.send.send(Message::Transfer(TransferMessage::Offer{id,name,size,sha256:hash})).await?;},Err(error)=>{let _=ui.try_send(ClientUpdate::Status(window,error));}}},
            _=timer.tick()=>{media.expire(&io.send).await?;ticks+=1;if clipboard&&ticks%5==0{poll_clipboard(local.as_ref(),&mut clip_state,"client",&io.send).await?;}},
            packet=connection.read_datagram()=>match packet{Ok(packet)=>media.packet(&packet,&io.send).await?,Err(_)=>break},
            event=events.recv()=>match event{
                Some(ClientEvent::ViewMode(mode))=>{io.send.send(Message::ReleaseInput).await?;io.send.send(Message::SetViewMode{mode}).await?;},
                Some(ClientEvent::AdbBridge{enabled})=>{
                    let result=if enabled{frames_core::device_bridge::probe_local_adb().await}else{Ok(())};
                    match result{Ok(())=>{adb_requested=enabled;adb_enabled=enabled;if !enabled{adb_jobs.abort_all();}io.send.send(Message::AdbBridge{enabled}).await?;},Err(error)=>{ui.send(ClientUpdate::AdbBridge{enabled:false,address:None,error:Some(error.to_string())})?;}}
                },
                Some(ClientEvent::Visibility{window,visible,focused})=>{if !visible{media.pause(window);}io.send.send(Message::Visibility{window,visible,focused}).await?;},
                Some(ClientEvent::Action{window,action})=>{request+=1;io.send.send(Message::Command{request,window,action}).await?;},
                Some(ClientEvent::Input{window,event:frames_core::input::InputEvent::Pointer{x,y}})=>{let mut packet=b"FP".to_vec();packet.extend(serde_json::to_vec(&Message::Input{window,event:frames_core::input::InputEvent::Pointer{x,y}})?);let _=connection.send_datagram(packet.into());},
                Some(ClientEvent::Input{window,event})=>io.send.send(Message::Input{window,event}).await?,
                Some(ClientEvent::SendFiles{window,paths,x,y})=>{for path in paths.into_iter().take(100){let tx=prepared_tx.clone();let slots=prepare_slots.clone();let epoch=prepare_epoch.clone();let version=epoch.load(std::sync::atomic::Ordering::Relaxed);tokio::spawn(async move{let Ok(_permit)=slots.acquire_owned().await else{return;};let result=file_metadata(&path).await.map_err(|e|e.to_string());if epoch.load(std::sync::atomic::Ordering::Relaxed)==version{let _=tx.send((path,window,x,y,result)).await;}});}},
                Some(ClientEvent::CancelTransfers)=>{prepare_epoch.fetch_add(1,std::sync::atomic::Ordering::Relaxed);let mut ids=downloads.active_ids();ids.extend(uploads.keys().cloned());for id in ids{if let Some(job)=upload_jobs.remove(&id){job.abort();}uploads.remove(&id);downloads.cancel(&id).await?;io.send.send(Message::Transfer(TransferMessage::Cancel{id,reason:"canceled locally".into()})).await?;}let _=ui.try_send(ClientUpdate::TransferProgress{name:"Transfers canceled".into(),done:0,total:0});},
                Some(ClientEvent::DownloadFile{path})=>io.send.send(Message::DownloadFile{path}).await?,
                Some(ClientEvent::ReleaseInput)=>io.send.send(Message::ReleaseInput).await?,Some(ClientEvent::Disconnected)|None=>break,
            },
            message=io.receive.recv()=>{let Some(message)=message else{break;};match message{
                Message::ViewModeChanged{mode,error}=>{if error.is_none(){media.clear_video();}ui.send(ClientUpdate::ViewMode{mode,error})?;},
                Message::AdbBridgeStatus{enabled,address,error}=>{
                    adb_enabled=enabled&&adb_requested;
                    if !adb_enabled{adb_jobs.abort_all();}
                    if enabled&&!adb_requested{io.send.send(Message::AdbBridge{enabled:false}).await?;}
                    if !enabled{adb_requested=false;}
                    ui.send(ClientUpdate::AdbBridge{enabled:adb_enabled,address:if adb_enabled{address}else{None},error})?;
                },
                Message::Snapshot{sequence,windows}=>{graph.snapshot(sequence,windows)?;media.reconcile(&graph);ui.send(ClientUpdate::Windows(graph.windows.values().cloned().collect()))?;},
                Message::Delta{sequence,upsert,removed}=>{graph.delta(sequence,upsert,&removed)?;media.reconcile(&graph);ui.send(ClientUpdate::Windows(graph.windows.values().cloned().collect()))?;},
                Message::StreamStarted(config)=>{ui.send(ClientUpdate::Status(config.window,format!("{:?} / {} / CPU readback",config.codec,config.encoder)))?;media.start(config,ui.clone(),&io.send).await?;},
                Message::StreamStopped{window,generation}=>media.stop(window,generation),
                Message::StreamPaused{window}=>media.pause(window),
                Message::AudioStarted{generation}=>media.start_audio(generation,&io.send).await?,
                Message::Ack{window,actual,error,..}=>ui.send(ClientUpdate::Ack(window,actual,error))?,
                Message::Menu{window,items}=>{validate_menu(&items)?;ui.send(ClientUpdate::Menu{window,items})?;},
                Message::Transfer(TransferMessage::Accept{id,offset})=>if let Some((path,..))=uploads.get(&id){let path=path.clone();let out=io.send.clone();let key=id.clone();upload_jobs.insert(key,tokio::spawn(async move{if let Err(error)=upload_chunks(&id,&path,offset,&out).await{let _=out.send(Message::Error{message:error.to_string()}).await;}}));},
                Message::Transfer(TransferMessage::Finished{id,..})=>if let Some((_,window,x,y))=uploads.remove(&id){io.send.send(Message::DropFiles{window,transfers:vec![id],x,y}).await?;},
                Message::Transfer(message) => {
                    let id = match &message { TransferMessage::Offer { id, .. } | TransferMessage::Chunk { id, .. } => Some(id.clone()), _ => None };
                    if let Some(reply) = receive_transfer(&mut downloads, message).await? {
                        if let TransferMessage::Finished { name, .. } = &reply { ui.send(ClientUpdate::ReceivedFile { path: downloads.received_path(name)? })?; }
                        io.send.send(Message::Transfer(reply)).await?;
                    }
                    if let Some(id) = id && let Some((name, done, total)) = downloads.progress(&id) {
                        let _ = ui.try_send(ClientUpdate::TransferProgress { name, done, total });
                    }
                },
        Message::Clipboard{origin,revision,text,png_base64}if clipboard=>{if let Err(error)=apply_clipboard(local.as_ref(),&mut clip_state,&origin,revision,text,png_base64){tracing::debug!(%error,"clipboard update unavailable");}},
                Message::Error{message}=>tracing::warn!(%message,"host error"),Message::Goodbye=>break,_=>{}
            }}
        }
    }
    let _ = io.send.send(Message::ReleaseInput).await;
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

async fn receive_transfer(
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

async fn poll_clipboard(
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
                out.send(message).await?;
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
