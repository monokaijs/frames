//! Gamepad and USB forwarding (`frames_devices`; USB in `crate::usb`).
//!
//! Gamepads: the viewer reads its controllers
//! (SDL3, in a `gamepad-worker` helper process) and every one connected
//! (up to four) becomes a virtual Xbox 360 controller on a Windows host
//! (ViGEmBus); rumble comes back. Only a device trusted for control may.
//!
//! One `DEVICES` stream carries [`DeviceMessage`]s as JSON lines both ways,
//! as v1 did: the viewer attaches a pad to a slot (with a generation, so
//! stale reports are dropped), streams its state with a sequence number
//! and its own clock, and the host answers with the pad's status and
//! rumble. Reports older than the clock allows are ignored (no stuck
//! buttons after a stall).

use crate::streams::Streams;
use anyhow::Result;
use frames_devices::gamepad::{Capture, CaptureCommand, CaptureEvent};
use frames_devices::protocol::{Capabilities, DeviceMessage, MAX_GAMEPADS, MAX_USB_DEVICES};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;

/// Writes one message as a JSON line.
async fn write(send: &mut iroh::endpoint::SendStream, message: &DeviceMessage) -> Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    send.write_all(&line).await?;
    Ok(())
}

/// The other end's messages, one per JSON line; invalid ones are skipped.
fn lines(recv: iroh::endpoint::RecvStream) -> tokio::sync::mpsc::Receiver<DeviceMessage> {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(recv);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(n) if n > 0 && line.len() <= 64 * 1024 => {}
                _ => return,
            }
            let Ok(message) = serde_json::from_str::<DeviceMessage>(line.trim()) else {
                continue;
            };
            if message.validate().is_ok() && tx.send(message).await.is_err() {
                return;
            }
        }
    });
    rx
}

/// Viewer: where the host's `USB` streams go (one devices session per
/// viewer process).
pub static USB_STREAMS: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<Streams>>> =
    std::sync::Mutex::new(None);

/// Host: virtual controllers for the viewer's pads and its USB devices,
/// until it closes the stream.
pub async fn host(session: frames_net::Session, (mut send, recv): Streams) -> Result<()> {
    let mut incoming = lines(recv);
    let (feedback_tx, mut feedback) = frames_devices::protocol::channel();
    let error = frames_devices::gamepad::host_available().err();
    let usb_error = frames_devices::usb::host_available().err();
    write(
        &mut send,
        &DeviceMessage::Capabilities(Capabilities {
            gamepad_host: error.is_none(),
            gamepad_error: error.clone(),
            usb_host: usb_error.is_none(),
            usb_error: usb_error.clone(),
        }),
    )
    .await?;
    // Attached USB devices, by bus id, with their generation.
    let mut bridges: HashMap<String, (u64, crate::usb::Bridge)> = HashMap::new();
    let (attached_tx, mut attached) = tokio::sync::mpsc::unbounded_channel();
    let mut pads: Option<frames_devices::gamepad::Host> = None;
    let mut epoch: Option<Instant> = None;
    loop {
        tokio::select! {
            message = incoming.recv() => {
                let Some(message) = message else { break };
                match message {
                    DeviceMessage::GamepadAttach { slot, generation, name } => {
                        if pads.is_none() {
                            match frames_devices::gamepad::Host::start(feedback_tx.clone()) {
                                Ok(host) => {
                                    if let Some(epoch) = epoch {
                                        let elapsed_ms = epoch.elapsed().as_millis() as u64;
                                        let _ = host.input.send(DeviceMessage::Clock { elapsed_ms });
                                    }
                                    pads = Some(host);
                                }
                                Err(error) => println!("gamepads unavailable: {error:#}"),
                            }
                        }
                        let result = match &pads {
                            Some(pads) => pads.input.send(DeviceMessage::GamepadAttach { slot, generation, name: name.clone() }),
                            None => Err(anyhow::anyhow!("{}", error.as_deref().unwrap_or("no virtual controllers on this host"))),
                        };
                        match result {
                            Ok(()) => println!("gamepad {slot}: {name}"),
                            Err(error) => write(&mut send, &DeviceMessage::GamepadStatus {
                                slot, generation, attached: false, error: Some(error.to_string()),
                            }).await?,
                        }
                    }
                    DeviceMessage::Clock { elapsed_ms } => {
                        // Keep the earliest epoch: delayed clock messages
                        // must not make reports look newer.
                        if let Some(candidate) = Instant::now().checked_sub(Duration::from_millis(elapsed_ms)) {
                            epoch = Some(epoch.map_or(candidate, |old| old.min(candidate)));
                        }
                        if let Some(pads) = &pads {
                            let _ = pads.input.send(DeviceMessage::Clock { elapsed_ms });
                        }
                    }
                    message @ (DeviceMessage::GamepadState { .. }
                    | DeviceMessage::GamepadDetach { .. }
                    | DeviceMessage::Release) => {
                        if let Some(pads) = &pads {
                            let _ = pads.input.send(message);
                        }
                    }
                    DeviceMessage::UsbAttach { id, generation, name } => {
                        if usb_error.is_some() || bridges.contains_key(&id) || bridges.len() >= MAX_USB_DEVICES {
                            write(&mut send, &DeviceMessage::UsbStatus {
                                id, generation, attached: false,
                                error: Some(usb_error.clone().unwrap_or_else(|| "that USB device is already attached, or all slots are in use".into())),
                            }).await?;
                            continue;
                        }
                        println!("USB: attaching {name} ({id})");
                        let (session, done) = (session.clone(), attached_tx.clone());
                        tokio::spawn(async move {
                            let result = crate::usb::bridge(session, id.clone(), generation).await;
                            let _ = done.send((id, generation, result));
                        });
                    }
                    DeviceMessage::UsbDetach { id, generation } => {
                        if let Some((version, bridge)) = bridges.remove(&id) {
                            if version == generation {
                                let port = bridge.port;
                                let _ = tokio::task::spawn_blocking(move || frames_devices::usb::detach(port)).await;
                            } else {
                                bridges.insert(id.clone(), (version, bridge));
                            }
                        }
                        write(&mut send, &DeviceMessage::UsbStatus { id, generation, attached: false, error: None }).await?;
                    }
                    _ => {}
                }
            }
            done = attached.recv() => {
                let Some((id, generation, result)) = done else { break };
                let error = match result {
                    Ok(bridge) => {
                        println!("USB: {id} attached on port {}", bridge.port);
                        bridges.insert(id.clone(), (generation, bridge));
                        None
                    }
                    Err(error) => Some(format!("{error:#}")),
                };
                write(&mut send, &DeviceMessage::UsbStatus { id, generation, attached: error.is_none(), error }).await?;
            }
            message = feedback.recv() => {
                let Some(message) = message else { break };
                write(&mut send, &message).await?;
            }
        }
    }
    // USB devices go back to the viewer; dropping `pads` unplugs every
    // virtual controller.
    for (_, (_, bridge)) in bridges.drain() {
        let port = bridge.port;
        let _ = tokio::task::spawn_blocking(move || frames_devices::usb::detach(port)).await;
    }
    Ok(())
}

struct Pad {
    id: String,
    generation: u64,
    attached: bool,
    sequence: u64,
}

/// Attaches the inventory's controllers that have no slot yet (up to four).
async fn attach_new(
    send: &mut iroh::endpoint::SendStream,
    inventory: &[frames_devices::protocol::DeviceInfo],
    pads: &mut HashMap<u8, Pad>,
    generation: &mut u64,
) -> Result<()> {
    for device in inventory {
        if pads.values().any(|p| p.id == device.id) {
            continue;
        }
        let Some(slot) = (0..MAX_GAMEPADS as u8).find(|s| !pads.contains_key(s)) else {
            break;
        };
        *generation += 1;
        write(
            send,
            &DeviceMessage::GamepadAttach {
                slot,
                generation: *generation,
                name: device.name.clone(),
            },
        )
        .await?;
        pads.insert(
            slot,
            Pad {
                id: device.id.clone(),
                generation: *generation,
                attached: false,
                sequence: 0,
            },
        );
    }
    Ok(())
}

/// Viewer: forwards every connected controller (up to four) when
/// `gamepads`, and the USB devices `usb` names (bus ids), until the
/// session ends.
pub async fn viewer(session: frames_net::Session, gamepads: bool, usb: Vec<String>) -> Result<()> {
    let (mut send, recv) = crate::streams::open(&session, crate::streams::DEVICES).await?;
    let mut incoming = lines(recv);
    let mut capture = if gamepads {
        Some(Capture::start()?)
    } else {
        None
    };
    // USB: the host's streams for exported devices, and their leases.
    let (usb_tx, mut usb_streams) = tokio::sync::mpsc::unbounded_channel();
    *USB_STREAMS.lock().unwrap() = Some(usb_tx);
    let exports: crate::usb::Exports = Default::default();
    let mut leases: HashMap<
        String,
        (
            frames_devices::usb::ExportLease,
            tokio::sync::watch::Sender<bool>,
        ),
    > = HashMap::new();
    let (exported_tx, mut exported) = tokio::sync::mpsc::unbounded_channel::<(
        String,
        anyhow::Result<frames_devices::usb::ExportLease>,
    )>();
    let mut usb_requested = false;
    let epoch = Instant::now();
    let elapsed = || epoch.elapsed().as_millis() as u64;
    let mut capture_offset = 0u64;
    let mut pads: HashMap<u8, Pad> = HashMap::new();
    let mut generation = 0u64;
    let mut host_ok = false;
    let mut inventory = Vec::new();
    let mut clock = tokio::time::interval(Duration::from_secs(1));
    write(
        &mut send,
        &DeviceMessage::Clock {
            elapsed_ms: elapsed(),
        },
    )
    .await?;
    loop {
        tokio::select! {
            () = session.closed() => break,
            _ = clock.tick() => write(&mut send, &DeviceMessage::Clock { elapsed_ms: elapsed() }).await?,
            event = async { match &mut capture { Some(c) => c.events.recv().await, None => std::future::pending().await } } => {
                let Some(event) = event else {
                    println!("gamepads: the controller helper stopped");
                    capture = None;
                    continue;
                };
                let Some(capture) = &capture else { continue };
                match event {
                    CaptureEvent::Ready { elapsed_ms } => {
                        capture_offset = elapsed().saturating_sub(elapsed_ms);
                        capture.command(CaptureCommand::Focus { active: true })?;
                    }
                    CaptureEvent::Inventory { devices } => {
                        inventory = devices;
                        if host_ok {
                            attach_new(&mut send, &inventory, &mut pads, &mut generation).await?;
                        }
                    }
                    CaptureEvent::State { slot, generation, state, elapsed_ms } => {
                        if let Some(pad) = pads.get_mut(&slot)
                            && pad.generation == generation
                            && pad.attached
                        {
                            pad.sequence += 1;
                            write(&mut send, &DeviceMessage::GamepadState {
                                slot, generation, sequence: pad.sequence,
                                elapsed_ms: capture_offset.saturating_add(elapsed_ms), state,
                            }).await?;
                        }
                    }
                    CaptureEvent::Removed { slot, generation } => {
                        if pads.get(&slot).is_some_and(|p| p.generation == generation) {
                            pads.remove(&slot);
                        }
                        write(&mut send, &DeviceMessage::GamepadDetach { slot, generation }).await?;
                    }
                    CaptureEvent::Error { message } => println!("gamepads: {message}"),
                }
            }
            streams = usb_streams.recv() => {
                if let Some(streams) = streams {
                    let exports = exports.clone();
                    tokio::spawn(async move {
                        if let Err(error) = crate::usb::serve(streams, exports).await {
                            println!("USB: {error:#}");
                        }
                    });
                }
            }
            done = exported.recv() => {
                let Some((bus, result)) = done else { continue };
                match result {
                    Ok(lease) => {
                        generation += 1;
                        let (active, watch) = tokio::sync::watch::channel(true);
                        exports.lock().unwrap().insert(bus.clone(), (generation, watch));
                        leases.insert(bus.clone(), (lease, active));
                        write(&mut send, &DeviceMessage::UsbAttach { id: bus.clone(), generation, name: bus }).await?;
                    }
                    Err(error) => println!("USB {bus}: {error:#}"),
                }
            }
            message = incoming.recv() => {
                let Some(message) = message else { break };
                match message {
                    DeviceMessage::UsbStatus { id, attached, error, .. } => {
                        if attached {
                            println!("USB {id} attached on the host");
                        } else {
                            exports.lock().unwrap().remove(&id);
                            if let Some((_, active)) = leases.remove(&id) {
                                let _ = active.send(false);
                            }
                            if let Some(error) = error {
                                println!("USB {id}: {error}");
                            }
                        }
                    }
                    DeviceMessage::Capabilities(capabilities) => {
                        if !usb.is_empty() && !usb_requested {
                            usb_requested = true;
                            match &capabilities.usb_error {
                                Some(error) => println!("USB: the host cannot take devices: {error}"),
                                None => {
                                    // Each export asks for administrator rights once.
                                    for bus in usb.iter().take(MAX_USB_DEVICES).cloned() {
                                        let done = exported_tx.clone();
                                        tokio::task::spawn_blocking(move || {
                                            let lease = frames_devices::usb::prepare_export(&bus);
                                            let _ = done.send((bus, lease));
                                        });
                                    }
                                }
                            }
                        }
                        host_ok = capabilities.gamepad_host;
                        if let Some(error) = capabilities.gamepad_error {
                            println!("gamepads: the host cannot take them: {error}");
                        }
                        if host_ok {
                            attach_new(&mut send, &inventory, &mut pads, &mut generation).await?;
                        }
                    }
                    DeviceMessage::GamepadStatus { slot, generation, attached, error } => {
                        if let Some(pad) = pads.get_mut(&slot) && pad.generation == generation {
                            if attached {
                                pad.attached = true;
                                if let Some(capture) = &capture {
                                    capture.command(CaptureCommand::Select { id: pad.id.clone(), slot, generation })?;
                                }
                                println!("gamepad {slot} forwarded");
                            } else {
                                pads.remove(&slot);
                                if let Some(capture) = &capture {
                                    let _ = capture.command(CaptureCommand::Remove { slot });
                                }
                                if let Some(error) = error {
                                    println!("gamepad {slot}: {error}");
                                }
                            }
                        }
                    }
                    DeviceMessage::Rumble { slot, generation, elapsed_ms, low, high } => {
                        let fresh = elapsed().saturating_sub(elapsed_ms) < 100;
                        let Some(capture) = &capture else { continue };
                        let _ = capture.command(CaptureCommand::Rumble {
                            slot, generation,
                            elapsed_ms: elapsed_ms.saturating_sub(capture_offset),
                            low: if fresh { low } else { 0 },
                            high: if fresh { high } else { 0 },
                        });
                    }
                    _ => {}
                }
            }
        }
    }
    let _ = write(&mut send, &DeviceMessage::Release).await;
    Ok(())
}
