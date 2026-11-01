//! Device actors own native resources; media and the UI never wait on drivers.
use crate::{media_sender::MediaSender, session::Outbound};
use frames_core::{
    devices::{self, Capabilities, DeviceAction, DeviceInfo, DeviceList, DeviceMessage},
    usbip,
};
use frames_native::{
    ClientSender, ClientUpdate,
    gamepad::{Capture, CaptureCommand, CaptureEvent},
};
use std::{
    collections::HashMap,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{mpsc, watch},
};

pub struct Actor {
    task: tokio::task::JoinHandle<()>,
}
struct PeerCleanup(devices::Sender);
impl Drop for PeerCleanup {
    fn drop(&mut self) {
        let _ = self.0.send(DeviceMessage::Unavailable {
            error: "Device forwarding ended. Reconnect the session to enable it again.".into(),
        });
    }
}
impl Drop for Actor {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub type Exports = Arc<Mutex<HashMap<String, (u64, watch::Receiver<bool>)>>>;
struct Export {
    attached: bool,
    id: String,
    generation: u64,
    registry: Exports,
    stop: watch::Sender<bool>,
    _lease: frames_native::usb::ExportLease,
}
impl Drop for Export {
    fn drop(&mut self) {
        let _ = self.stop.send(false);
        let mut registry = self.registry.lock().unwrap();
        if registry
            .get(&self.id)
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            registry.remove(&self.id);
        }
    }
}
pub struct Client {
    pub commands: mpsc::Sender<DeviceAction>,
    exports: Exports,
    _actor: Actor,
}
enum ClientNative {
    Inventory(Result<Vec<DeviceInfo>, String>),
    Export {
        id: String,
        generation: u64,
        result: Result<frames_native::usb::ExportLease, String>,
    },
}
struct PadSelection {
    id: String,
    generation: u64,
    attached: bool,
    sequence: u64,
}
impl Client {
    pub fn start(
        connection: quinn::Connection,
        out: Outbound,
        incoming: mpsc::Receiver<DeviceMessage>,
        ui: ClientSender,
        epoch: Instant,
        windows_host: bool,
    ) -> Self {
        let (commands, command_rx) = mpsc::channel(32);
        let exports = Exports::default();
        let registry = exports.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = client_actor(
                connection,
                out,
                incoming,
                ui.clone(),
                epoch,
                windows_host,
                command_rx,
                registry,
            )
            .await
            {
                let _ = ui.try_send(ClientUpdate::Devices(DeviceList {
                    error: Some(error.to_string()),
                    ..Default::default()
                }));
            }
        });
        let _ = commands.try_send(DeviceAction::Refresh);
        Self {
            commands,
            exports,
            _actor: Actor { task },
        }
    }
    pub fn command(&self, action: DeviceAction) {
        let _ = self.commands.try_send(action);
    }
    pub fn exports(&self) -> Exports {
        self.exports.clone()
    }
}

#[allow(clippy::too_many_arguments)]
async fn client_actor(
    connection: quinn::Connection,
    out: Outbound,
    mut incoming: mpsc::Receiver<DeviceMessage>,
    ui: ClientSender,
    epoch: Instant,
    windows_host: bool,
    mut commands: mpsc::Receiver<DeviceAction>,
    registry: Exports,
) -> anyhow::Result<()> {
    let mut list = DeviceList::default();
    let _cleanup = PeerCleanup(out.devices.clone());
    let mut published = None;
    let mut capture: Option<Capture> = None;
    let mut capture_offset = None;
    let mut focused = false;
    let mut focus_since = 0;
    let mut selections: HashMap<u8, PadSelection> = HashMap::new();
    let mut exports: HashMap<String, Export> = HashMap::new();
    let mut pending: HashMap<String, u64> = HashMap::new();
    let mut generation = 0u64;
    let mut inventory_pending = false;
    let (native_tx, mut native_rx) = mpsc::channel(16);
    let mut timer = tokio::time::interval(Duration::from_secs(1));
    out.devices.send(DeviceMessage::Clock {
        elapsed_ms: epoch.elapsed().as_millis() as u64,
    })?;
    loop {
        tokio::select! {
            _ = connection.closed() => break,
            _ = timer.tick() => { out.devices.send(DeviceMessage::Clock { elapsed_ms: epoch.elapsed().as_millis() as u64 })?; }
            action = commands.recv() => {
                let Some(action) = action else { break; };
                list.error = None;
                match action {
                    DeviceAction::Refresh => {
                        out.devices.send(DeviceMessage::Refresh)?;
                        if capture.is_none() && windows_host {
                            match Capture::start() { Ok(worker) => capture = Some(worker), Err(error) => list.error = Some(error.to_string()) }
                        }
                        if !inventory_pending {
                            inventory_pending = true;
                            let tx = native_tx.clone();
                            tokio::task::spawn_blocking(move || { let _ = tx.blocking_send(ClientNative::Inventory(frames_native::usb::inventory().map_err(|e| e.to_string()))); });
                        }
                    }
                    DeviceAction::Focus { active } => {
                        focused = active;
                        if active { focus_since = epoch.elapsed().as_millis() as u64; }
                        if let Some(capture) = &capture { capture.command(CaptureCommand::Focus { active })?; }
                        if !active { out.devices.send(DeviceMessage::Release)?; }
                    }
                    DeviceAction::Release => {
                        focused = false;
                        if let Some(capture) = &capture { capture.command(CaptureCommand::Focus { active: false })?; }
                        out.devices.send(DeviceMessage::Release)?;
                    }
                    DeviceAction::Setup { target } => {
                        let result = if (target == "gamepad" || target == "usb-host") && !windows_host { Err(anyhow::anyhow!("Device receiving and driver setup require a Windows host in this release.")) } else if target == "gamepad" || target == "usb-host" { out.devices.send(DeviceMessage::Setup { target }) }
                            else if target == "usb-export" { crate::device_setup::launch(&target) }
                            else { Err(anyhow::anyhow!("Unknown driver setup target")) };
                        if let Err(error) = result { list.error = Some(error.to_string()); }
                    }
                    DeviceAction::Gamepad { id, enabled } => {
                        if !enabled {
                            if let Some(slot) = selections.iter().find(|(_,s)| s.id == id).map(|(slot,_)| *slot) {
                                let selected = selections.remove(&slot).unwrap();
                                if let Some(capture) = &capture { let _ = capture.command(CaptureCommand::Remove { slot }); }
                                out.devices.send(DeviceMessage::GamepadDetach { slot, generation: selected.generation })?;
                            }
                        } else if !list.capabilities.gamepad_host { list.error = list.capabilities.gamepad_error.clone(); }
                        else if selections.values().any(|s| s.id == id) { continue; }
                        else if list.gamepads.iter().find(|d|d.id==id).is_some_and(|pad| list.usb.iter().any(|d|d.forwarded && devices::same_physical(pad.physical_id.as_deref(),d.physical_id.as_deref()))) { list.error=Some("Disconnect this device from USB forwarding before using gamepad mode.".into()); }
                        else if let Some(slot) = (0..devices::MAX_GAMEPADS as u8).find(|s| !selections.contains_key(s)) {
                            if let Some(info) = list.gamepads.iter().find(|d| d.id == id) {
                                generation += 1;
                                out.devices.send(DeviceMessage::GamepadAttach { slot, generation, name: info.name.clone() })?;
                                selections.insert(slot, PadSelection { id, generation, attached: false, sequence: 0 });
                            }
                        } else { list.error = Some("Up to four gamepads can be forwarded.".into()); }
                    }
                    DeviceAction::Usb { id, enabled } => {
                        if !enabled {
                            if let Some(export) = exports.get(&id) { out.devices.send(DeviceMessage::UsbDetach { id: id.clone(), generation: export.generation })?; }
                            else { pending.remove(&id); }
                        } else if !list.capabilities.usb_host { list.error = list.capabilities.usb_error.clone(); }
                        else if exports.contains_key(&id) || pending.contains_key(&id) { continue; }
                        else if exports.len() + pending.len() >= devices::MAX_USB_DEVICES { list.error = Some("Up to eight USB devices can be forwarded.".into()); }
                        else if list.usb.iter().find(|d|d.id==id).is_some_and(|device| list.gamepads.iter().any(|pad| selections.values().any(|s|s.id==pad.id) && devices::same_physical(pad.physical_id.as_deref(),device.physical_id.as_deref()))) { list.error=Some("Disconnect this gamepad from gamepad forwarding before using USB passthrough.".into()); }
                        else if list.usb.iter().any(|d| d.id == id && d.error.is_none()) {
                            generation += 1; let version = generation; pending.insert(id.clone(), version);
                            let tx = native_tx.clone();
                            tokio::task::spawn_blocking(move || {
                                let result = frames_native::usb::prepare_export(&id).map_err(|e| e.to_string());
                                let _ = tx.blocking_send(ClientNative::Export { id, generation: version, result });
                            });
                        }
                    }
                }
            }
            event = async { match &mut capture { Some(capture) => capture.events.recv().await, None => std::future::pending().await } } => {
                match event {
                    Some(CaptureEvent::Ready { elapsed_ms }) => {
                        capture_offset = Some((epoch.elapsed().as_millis() as u64).saturating_sub(elapsed_ms));
                        if let Some(capture) = &capture { capture.command(CaptureCommand::Focus { active: focused })?; }
                    }
                    Some(CaptureEvent::Inventory { devices }) => list.gamepads = devices,
                    Some(CaptureEvent::State { slot, generation, state, elapsed_ms }) => {
                        if let Some(selected) = selections.get_mut(&slot) && selected.generation == generation && selected.attached {
                            let elapsed_ms = capture_offset.unwrap_or(0).saturating_add(elapsed_ms);
                            if focused && elapsed_ms < focus_since { continue; }
                            let state = if focused { state } else { Default::default() };
                            selected.sequence += 1;
                            out.devices.send(DeviceMessage::GamepadState { slot, generation, sequence: selected.sequence,
                                elapsed_ms, state })?;
                        }
                        continue;
                    }
                    Some(CaptureEvent::Removed { slot, generation }) => {
                        if selections.get(&slot).is_some_and(|s| s.generation == generation) { selections.remove(&slot); }
                        out.devices.send(DeviceMessage::GamepadDetach { slot, generation })?;
                    }
                    Some(CaptureEvent::Error { message }) => {
                        list.error = Some(message); capture.take();
                        for (slot, selected) in selections.drain() { let _ = out.devices.send(DeviceMessage::GamepadDetach { slot, generation: selected.generation }); }
                    }
                    None => { capture.take(); for (slot,selected) in selections.drain() {let _=out.devices.send(DeviceMessage::GamepadDetach {slot,generation:selected.generation});} out.devices.send(DeviceMessage::Release)?; list.error = Some("Controller helper stopped. Refresh devices to restart it.".into()); }
                }
            }
            native = native_rx.recv() => if let Some(native) = native {
                match native {
                    ClientNative::Inventory(Ok(items)) => { inventory_pending = false; list.usb = items; list.usb_local_error = None; },
                    ClientNative::Inventory(Err(error)) => { inventory_pending = false; list.usb_local_error = Some(error); },
                    ClientNative::Export { id, generation, result } => {
                        if pending.remove(&id) != Some(generation) { continue; }
                        match result {
                            Ok(lease) => {
                                let (stop, receiver) = watch::channel(true);
                                registry.lock().unwrap().insert(id.clone(), (generation, receiver));
                                exports.insert(id.clone(), Export { attached:false, id: id.clone(), generation, registry: registry.clone(), stop, _lease: lease });
                                let name = list.usb.iter().find(|d| d.id == id).map(|d| d.name.clone()).unwrap_or_else(|| id.clone());
                                out.devices.send(DeviceMessage::UsbAttach { id, generation, name })?;
                            }
                            Err(error) => list.error = Some(error),
                        }
                    }
                }
            },
            message = incoming.recv() => {
                let Some(message) = message else { break; };
                match message {
                    DeviceMessage::Unavailable { error } => anyhow::bail!("{error}"),
                    DeviceMessage::Capabilities(capabilities) => list.capabilities = capabilities,
                    DeviceMessage::GamepadStatus { slot, generation, attached, error } => {
                        if let Some(selected) = selections.get_mut(&slot) && selected.generation == generation {
                            if attached {
                                selected.attached = true;
                                if let Some(capture) = &capture { capture.command(CaptureCommand::Select { id: selected.id.clone(), slot, generation })?; }
                            } else {
                                selections.remove(&slot);
                                if let Some(capture) = &capture { capture.command(CaptureCommand::Remove { slot })?; }
                            }
                            list.error = error;
                        }
                    }
                    DeviceMessage::Rumble { slot, generation, elapsed_ms, low, high } => {
                        let fresh=(epoch.elapsed().as_millis() as u64).saturating_sub(elapsed_ms)<100;
                        if let Some(capture) = &capture { capture.command(CaptureCommand::Rumble { slot, generation, elapsed_ms:elapsed_ms.saturating_sub(capture_offset.unwrap_or(0)), low:if fresh{low}else{0}, high:if fresh{high}else{0} })?; }
                        continue;
                    }
                    DeviceMessage::UsbStatus { id, generation, attached, error } => {
                        if exports.get(&id).is_some_and(|e| e.generation == generation) {
                            if !attached { exports.remove(&id); } else if let Some(export)=exports.get_mut(&id) { export.attached=true; } list.error = error;
                        }
                    }
                    _ => {}
                }
            }
        }
        for device in &mut list.gamepads {
            let selection = selections.values().find(|s| s.id == device.id);
            device.forwarded = selection.is_some();
            device.status = selection.map(|s| {
                if s.attached {
                    "forwarded"
                } else {
                    "connecting"
                }
                .into()
            });
        }
        for device in &mut list.usb {
            if exports.contains_key(&device.id) {
                // usbipd reports our own lease as shared as well. Preserve
                // conflicts for other devices, but do not label this lease
                // as an attachment made outside Frames after a refresh.
                device.error = None;
            }
            device.forwarded = exports.contains_key(&device.id) || pending.contains_key(&device.id);
            device.status = if exports.get(&device.id).is_some_and(|e| e.attached) {
                Some("forwarded".into())
            } else if device.forwarded {
                Some("connecting".into())
            } else {
                None
            };
        }
        if published.as_ref() != Some(&list)
            && ui.try_send(ClientUpdate::Devices(list.clone())).is_ok()
        {
            published = Some(list.clone());
            list.error = None;
        }
    }
    let _ = out.devices.send(DeviceMessage::Release);
    Ok(())
}

pub fn start_host(
    connection: quinn::Connection,
    out: Outbound,
    incoming: mpsc::Receiver<DeviceMessage>,
    allowed: bool,
) -> Actor {
    let task = tokio::spawn(async move {
        if let Err(error) = host_actor(connection, out, incoming, allowed).await {
            tracing::warn!(%error,"device host stopped");
        }
    });
    Actor { task }
}
enum HostNative {
    Capabilities(Result<(), String>),
    Attached {
        id: String,
        generation: u64,
        result: Result<UsbBridge, String>,
    },
    Detached {
        id: String,
        generation: u64,
        result: Result<(), String>,
    },
}
async fn host_actor(
    connection: quinn::Connection,
    out: Outbound,
    mut incoming: mpsc::Receiver<DeviceMessage>,
    allowed: bool,
) -> anyhow::Result<()> {
    let _cleanup = PeerCleanup(out.devices.clone());
    let gamepad_error = if allowed {
        frames_native::gamepad::host_available().err()
    } else {
        Some("Device forwarding requires a normal user-session host.".into())
    };
    let mut capabilities = Capabilities {
        gamepad_host: gamepad_error.is_none(),
        gamepad_error,
        usb_host: false,
        usb_error: Some(
            if allowed {
                "USB receiving support is being checked."
            } else {
                "Device forwarding requires a normal user-session host."
            }
            .into(),
        ),
    };
    out.devices
        .send(DeviceMessage::Capabilities(capabilities.clone()))?;
    let mut pads: Option<frames_native::gamepad::Host> = None;
    let mut clock = None;
    let mut bridges: HashMap<String, (u64, UsbBridge)> = HashMap::new();
    let mut pending: HashMap<String, u64> = HashMap::new();
    let mut detaching = std::collections::HashSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let (native_tx, mut native_rx) = mpsc::channel(16);
    let probe = |tx: mpsc::Sender<HostNative>| {
        tokio::task::spawn_blocking(move || {
            let _ = tx.blocking_send(HostNative::Capabilities(
                frames_native::usb::host_available(),
            ));
        });
    };
    if allowed {
        probe(native_tx.clone());
    }
    loop {
        tokio::select! {
            _ = connection.closed() => break,
            _ = tick.tick() => {
                let ended:Vec<_>=bridges.iter().filter(|(id,(_,bridge))|bridge.task.is_finished() && !detaching.contains(*id)).map(|(id,(generation,_))|(id.clone(),*generation)).collect();
                for (id,generation) in ended { bridges.remove(&id);out.devices.send(DeviceMessage::UsbStatus { id,generation,attached:false,error:Some("USB connection ended. The device was detached; reconnect it explicitly to forward again.".into()) })?; }
            }
            native = native_rx.recv() => if let Some(native) = native {
                match native {
                    HostNative::Capabilities(result) => { capabilities.usb_host = allowed && result.is_ok(); capabilities.usb_error = result.err(); out.devices.send(DeviceMessage::Capabilities(capabilities.clone()))?; }
                    HostNative::Attached { id, generation, result } => {
                        if pending.remove(&id) != Some(generation) { continue; }
                        let error = result.as_ref().err().cloned();
                        if let Ok(bridge) = result { bridges.insert(id.clone(), (generation,bridge)); }
                        out.devices.send(DeviceMessage::UsbStatus { id, generation, attached: error.is_none(), error })?;
                    }
                    HostNative::Detached { id, generation, result } => {
                        detaching.remove(&id);
                        if bridges.get(&id).is_none_or(|(version,_)| *version != generation) { continue; }
                        let error = result.err(); if error.is_none() { bridges.remove(&id); }
                        out.devices.send(DeviceMessage::UsbStatus { id, generation, attached: error.is_some(), error })?;
                    }
                }
            },
            message = incoming.recv() => {
                let Some(message) = message else { break; }; message.validate()?;
                match message {
                    DeviceMessage::Unavailable { .. } => break,
                    DeviceMessage::Refresh => {
                        capabilities.gamepad_error = if allowed { frames_native::gamepad::host_available().err() } else { Some("Device forwarding requires a normal user-session host.".into()) };
                        capabilities.gamepad_host = capabilities.gamepad_error.is_none();
                        out.devices.send(DeviceMessage::Capabilities(capabilities.clone()))?;
                        if allowed { probe(native_tx.clone()); }
                    }
                    DeviceMessage::Setup { target } => {
                        if allowed {
                            if let Err(error)=crate::device_setup::launch(&target) {
                                if target=="gamepad" { capabilities.gamepad_error=Some(error.to_string()); }
                                else { capabilities.usb_error=Some(error.to_string()); }
                                out.devices.send(DeviceMessage::Capabilities(capabilities.clone()))?;
                            }
                            probe(native_tx.clone());
                        }
                    }
                    DeviceMessage::GamepadAttach { slot, generation, name } => {
                        if allowed && pads.is_none() {
                            match frames_native::gamepad::Host::start(out.devices.clone()) {
                                Ok(host) => { if let Some(epoch) = clock { let elapsed_ms=Instant::now().saturating_duration_since(epoch).as_millis() as u64; let _ = host.input.send(DeviceMessage::Clock { elapsed_ms }); } pads=Some(host); }
                                Err(error) => { capabilities.gamepad_host=false; capabilities.gamepad_error=Some(error.to_string()); }
                            }
                        }
                        let result = pads.as_ref().map(|p| p.input.send(DeviceMessage::GamepadAttach { slot,generation,name }))
                            .unwrap_or_else(|| Err(anyhow::anyhow!("{}",capabilities.gamepad_error.as_deref().unwrap_or("Gamepad backend unavailable"))));
                        if let Err(error) = result { out.devices.send(DeviceMessage::GamepadStatus { slot,generation,attached:false,error:Some(error.to_string()) })?; }
                    }
                    DeviceMessage::Clock { elapsed_ms } => {
                        // Preserve the original epoch while the first driver starts.
                        if let Some(candidate)=Instant::now().checked_sub(Duration::from_millis(elapsed_ms)) { clock=Some(clock.map_or(candidate,|old: Instant|old.min(candidate))); }
                        if let Some(pads) = &pads { let _ = pads.input.send(DeviceMessage::Clock { elapsed_ms }); }
                    }
                    message @ (DeviceMessage::GamepadState { .. } | DeviceMessage::GamepadDetach { .. } | DeviceMessage::Release) => {
                        if let Some(pads) = &pads { let _ = pads.input.send(message); }
                    }
                    DeviceMessage::UsbAttach { id, generation, .. } => {
                        if !allowed || !capabilities.usb_host || bridges.len()+pending.len() >= devices::MAX_USB_DEVICES || bridges.contains_key(&id) || pending.contains_key(&id) {
                            out.devices.send(DeviceMessage::UsbStatus { id,generation,attached:false,error:Some(capabilities.usb_error.clone().unwrap_or_else(|| "USB device is already attached or all slots are busy.".into())) })?; continue;
                        }
                        pending.insert(id.clone(),generation);
                        let tx=native_tx.clone(); let connection=connection.clone(); let media=out.media.clone();
                        tokio::spawn(async move {
                            let result=UsbBridge::start(connection,id.clone(),generation,media).await.map_err(|e|e.to_string());
                            let _=tx.send(HostNative::Attached { id,generation,result }).await;
                        });
                    }
                    DeviceMessage::UsbDetach { id,generation } => {
                        if let Some((version,bridge))=bridges.get(&id) && *version==generation {
                            if !detaching.insert(id.clone()) { continue; }
                            let port=bridge.port; let tx=native_tx.clone();
                            tokio::task::spawn_blocking(move || { let result=frames_native::usb::detach(port).map_err(|e|e.to_string()); let _=tx.blocking_send(HostNative::Detached { id,generation,result }); });
                        } else { pending.remove(&id); out.devices.send(DeviceMessage::UsbStatus { id,generation,attached:false,error:None })?; }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

struct UsbBridge {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for UsbBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl UsbBridge {
    async fn start(
        connection: quinn::Connection,
        id: String,
        generation: u64,
        media: Option<MediaSender>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let tcp_port = listener.local_addr()?.port();
        let bus = id.clone();
        let failure = Arc::new(Mutex::new(None));
        let bridge_failure = failure.clone();
        let task = tokio::spawn(async move {
            let result = async {
                let (mut socket, _) = listener.accept().await?;
                socket.set_nodelay(true)?;
                let mut request = [0u8; usbip::IMPORT_REQUEST_BYTES];
                tokio::time::timeout(Duration::from_secs(10), socket.read_exact(&mut request))
                    .await??;
                usbip::validate_import(&request, &bus)?;
                let (mut send, mut receive) = connection.open_bi().await?;
                send.set_priority(-10)?;
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
                relay(socket, send, receive, media).await
            }
            .await;
            if let Err(error) = result {
                *bridge_failure.lock().unwrap() = Some(error.to_string());
                tracing::warn!(%error,"USB bridge ended");
            }
        });
        let mut bridge = Self { port: 0, task };
        let port = tokio::task::spawn_blocking(move || frames_native::usb::attach(&id, tcp_port))
            .await?
            .map_err(|error| {
                anyhow::anyhow!(
                    "{}",
                    failure
                        .lock()
                        .unwrap()
                        .as_deref()
                        .unwrap_or(&error.to_string())
                )
            })?;
        bridge.port = port;
        Ok(bridge)
    }
}
pub async fn serve_usb(
    mut send: quinn::SendStream,
    mut receive: quinn::RecvStream,
    exports: Exports,
    media: Option<MediaSender>,
) -> anyhow::Result<()> {
    let setup=async {
        let generation=receive.read_u64().await?; let length=receive.read_u8().await? as usize;
        anyhow::ensure!((1..32).contains(&length),"Invalid USB device request");
        let mut bus=vec![0;length]; receive.read_exact(&mut bus).await?; let bus=String::from_utf8(bus)?;
        let (version,mut active)=exports.lock().unwrap().get(&bus).cloned().ok_or_else(||anyhow::anyhow!("USB device was not explicitly selected"))?;
        anyhow::ensure!(version==generation && *active.borrow(),"USB device attachment has expired");
        let mut socket=TcpStream::connect((Ipv4Addr::LOCALHOST,3240)).await?; socket.set_nodelay(true)?;
        let response=tokio::select! {
            _=active.changed()=>anyhow::bail!("USB device attachment was cancelled"),
            response=tokio::time::timeout(Duration::from_secs(10),usbip::import_and_probe(&mut socket,&bus))=>response??,
        };
        Ok::<_,anyhow::Error>((socket,response,active))
    }.await;
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
    send.set_priority(-10)?;
    send.write_all(&[0]).await?;
    send.write_all(&response).await?;
    tokio::select! { _=active.changed()=>Ok(()), result=relay(socket,send,receive,media)=>result }
}
async fn relay(
    mut socket: TcpStream,
    mut send: quinn::SendStream,
    mut receive: quinn::RecvStream,
    media: Option<MediaSender>,
) -> anyhow::Result<()> {
    let (mut read, mut write) = socket.split();
    tokio::try_join!(
        copy_usb(&mut read, &mut send, media.clone()),
        copy_usb(&mut receive, &mut write, media)
    )?;
    Ok(())
}
async fn copy_usb(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
    media: Option<MediaSender>,
) -> anyhow::Result<()> {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            writer.shutdown().await?;
            return Ok(());
        }
        if let Some(media) = &media {
            media.throttle_bulk(count).await;
        }
        writer.write_all(&buffer[..count]).await?;
    }
}
