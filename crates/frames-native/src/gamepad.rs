//! SDL runs on a helper process's main thread, independently of either renderer.
#[cfg(windows)]
use frames_core::devices::DeviceMessage;
use frames_core::devices::{DeviceInfo, GamepadState, MAX_GAMEPADS};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{BufRead, Write},
    process::{Child, Stdio},
    time::{Duration, Instant},
};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureCommand {
    Select {
        id: String,
        slot: u8,
        generation: u64,
    },
    Remove {
        slot: u8,
    },
    Focus {
        active: bool,
    },
    Rumble {
        slot: u8,
        generation: u64,
        elapsed_ms: u64,
        low: u16,
        high: u16,
    },
    Stop,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureEvent {
    Ready {
        elapsed_ms: u64,
    },
    Inventory {
        devices: Vec<DeviceInfo>,
    },
    State {
        slot: u8,
        generation: u64,
        state: GamepadState,
        elapsed_ms: u64,
    },
    Removed {
        slot: u8,
        generation: u64,
    },
    Error {
        message: String,
    },
}
pub struct Capture {
    child: Option<Child>,
    commands: std::sync::mpsc::SyncSender<CaptureCommand>,
    pub events: tokio::sync::mpsc::Receiver<CaptureEvent>,
}
impl Capture {
    pub fn start() -> anyhow::Result<Self> {
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg("gamepad-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command.spawn()?;
        let output = child.stdout.take().unwrap();
        let mut input = child.stdin.take().unwrap();
        let (commands, receive) = std::sync::mpsc::sync_channel::<CaptureCommand>(32);
        let (tx, events) = tokio::sync::mpsc::channel(128);
        std::thread::Builder::new()
            .name("frames-gamepad-events".into())
            .spawn(move || {
                let reader = std::io::BufReader::new(output);
                for line in reader.lines() {
                    let event = match line {
                        Ok(line) if line.len() <= 32 * 1024 => serde_json::from_str(&line)
                            .unwrap_or_else(|e| CaptureEvent::Error {
                                message: format!("Controller worker: {e}"),
                            }),
                        _ => break,
                    };
                    if tx.blocking_send(event).is_err() {
                        break;
                    }
                }
            })?;
        std::thread::Builder::new()
            .name("frames-gamepad-commands".into())
            .spawn(move || {
                while let Ok(command) = receive.recv() {
                    if serde_json::to_writer(&mut input, &command).is_err()
                        || input.write_all(b"\n").is_err()
                        || input.flush().is_err()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            child: Some(child),
            commands,
            events,
        })
    }
    pub fn command(&self, command: CaptureCommand) -> anyhow::Result<()> {
        self.commands
            .try_send(command)
            .map_err(|_| anyhow::anyhow!("Controller helper is busy or stopped"))
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        // Give SDL time to send zero-rumble reports before forcing a stuck helper
        // to exit. A detached cleanup thread cannot stall the media runtime.
        let _ = self
            .commands
            .try_send(CaptureCommand::Focus { active: false });
        let _ = self.commands.try_send(CaptureCommand::Stop);
        if let Some(mut child) = self.child.take() {
            let _ = std::thread::Builder::new()
                .name("frames-gamepad-cleanup".into())
                .spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(2);
                    while Instant::now() < deadline {
                        if matches!(child.try_wait(), Ok(Some(_))) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                });
        }
    }
}

fn emit(output: &mut impl Write, event: CaptureEvent) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *output, &event)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}
struct Selected {
    pad: sdl3::gamepad::Gamepad,
    generation: u64,
    previous: Option<GamepadState>,
    sent: Instant,
}
impl Drop for Selected {
    fn drop(&mut self) {
        let _ = self.pad.set_rumble(0, 0, 0);
    }
}
pub fn capture_worker() -> anyhow::Result<()> {
    let result = capture_loop();
    if let Err(error) = &result {
        let _ = emit(
            &mut std::io::stdout().lock(),
            CaptureEvent::Error {
                message: error.to_string(),
            },
        );
    }
    result
}
fn capture_loop() -> anyhow::Result<()> {
    #[cfg(windows)]
    let _timer = TimerResolution::new();
    let epoch = Instant::now();
    sdl3::hint::set("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1");
    let sdl = sdl3::init().map_err(|e| anyhow::anyhow!("SDL3: {e}"))?;
    let gamepads = sdl
        .gamepad()
        .map_err(|e| anyhow::anyhow!("SDL3 gamepads: {e}"))?;
    let mut pump = sdl
        .event_pump()
        .map_err(|e| anyhow::anyhow!("SDL3 events: {e}"))?;
    let (tx, commands) = std::sync::mpsc::sync_channel(32);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if line.len() > 4096 {
                break;
            }
            let Ok(command) = serde_json::from_str::<CaptureCommand>(&line) else {
                break;
            };
            if tx.send(command).is_err() {
                return;
            }
        }
        let _ = tx.send(CaptureCommand::Stop);
    });
    let mut selected: HashMap<u8, Selected> = HashMap::new();
    let mut inventory_at = epoch - Duration::from_secs(1);
    let mut focused = false;
    let mut output = std::io::stdout().lock();
    emit(
        &mut output,
        CaptureEvent::Ready {
            elapsed_ms: epoch.elapsed().as_millis() as u64,
        },
    )?;
    loop {
        // Pumping every millisecond keeps SDL on this process's main thread.
        for event in pump.poll_iter() {
            use sdl3::event::Event;
            let transition = match event {
                Event::GamepadButtonDown { which, button, .. } => Some((which, button, true)),
                Event::GamepadButtonUp { which, button, .. } => Some((which, button, false)),
                _ => None,
            };
            if let Some((id, button, down)) = transition
                && focused
            {
                for (&slot, pad) in &mut selected {
                    if pad.pad.id().ok() != Some(id) {
                        continue;
                    }
                    let mut state = pad.previous.unwrap_or_else(|| read_state(&pad.pad));
                    let flag = button_flag(button);
                    if down {
                        state.buttons |= flag;
                    } else {
                        state.buttons &= !flag;
                    }
                    emit(
                        &mut output,
                        CaptureEvent::State {
                            slot,
                            generation: pad.generation,
                            state,
                            elapsed_ms: epoch.elapsed().as_millis() as u64,
                        },
                    )?;
                    pad.previous = Some(state);
                    pad.sent = Instant::now();
                }
            }
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                CaptureCommand::Stop => {
                    for pad in selected.values_mut() {
                        let _ = pad.pad.set_rumble(0, 0, 0);
                    }
                    return Ok(());
                }
                CaptureCommand::Select {
                    id,
                    slot,
                    generation,
                } => {
                    anyhow::ensure!(
                        (slot as usize) < MAX_GAMEPADS && generation != 0,
                        "Invalid controller selection"
                    );
                    let id: u32 = id.parse()?;
                    let pad = gamepads
                        .open(sdl3::joystick::JoystickId::new(id))
                        .map_err(|e| anyhow::anyhow!("Cannot open gamepad: {e}"))?;
                    if let Some(mut old) = selected.remove(&slot) {
                        let _ = old.pad.set_rumble(0, 0, 0);
                    }
                    selected.insert(
                        slot,
                        Selected {
                            pad,
                            generation,
                            previous: None,
                            sent: epoch,
                        },
                    );
                    inventory_at = epoch - Duration::from_secs(1);
                }
                CaptureCommand::Remove { slot } => {
                    if let Some(mut pad) = selected.remove(&slot) {
                        let _ = pad.pad.set_rumble(0, 0, 0);
                    }
                    inventory_at = epoch - Duration::from_secs(1);
                }
                CaptureCommand::Focus { active } => {
                    focused = active;
                    for pad in selected.values_mut() {
                        pad.previous = None;
                        if !focused {
                            let _ = pad.pad.set_rumble(0, 0, 0);
                        }
                    }
                }
                CaptureCommand::Rumble {
                    slot,
                    generation,
                    elapsed_ms,
                    low,
                    high,
                } => {
                    if let Some(pad) = selected.get_mut(&slot)
                        && pad.generation == generation
                    {
                        let fresh = epoch
                            .elapsed()
                            .as_millis()
                            .saturating_sub(elapsed_ms as u128)
                            < 100;
                        let _ = pad.pad.set_rumble(
                            if focused && fresh { low } else { 0 },
                            if focused && fresh { high } else { 0 },
                            100,
                        );
                    }
                }
            }
        }
        let now = Instant::now();
        let removed: Vec<_> = selected
            .iter()
            .filter(|(_, p)| !p.pad.connected())
            .map(|(slot, p)| (*slot, p.generation))
            .collect();
        for (slot, generation) in removed {
            selected.remove(&slot);
            emit(&mut output, CaptureEvent::Removed { slot, generation })?;
        }
        if now.duration_since(inventory_at) >= Duration::from_secs(1) {
            let devices = gamepads
                .gamepads()
                .map_err(|e| anyhow::anyhow!("Controller discovery: {e}"))?
                .into_iter()
                .take(32)
                .map(|id| DeviceInfo {
                    status: None,
                    physical_id: gamepads
                        .path_for_id(id)
                        .ok()
                        .and_then(|path| physical_id(&path))
                        .or_else(|| {
                            gamepads
                                .vendor_for_id(id)
                                .zip(gamepads.product_for_id(id))
                                .map(|(vendor, product)| {
                                    format!("vidpid:{vendor:04x}:{product:04x}")
                                })
                        }),
                    id: id.to_string(),
                    name: gamepads
                        .name_for_id(id)
                        .unwrap_or_else(|_| "Gamepad".into())
                        .chars()
                        .take(80)
                        .collect(),
                    forwarded: selected.values().any(|p| p.pad.id().ok() == Some(id)),
                    error: None,
                })
                .collect();
            emit(&mut output, CaptureEvent::Inventory { devices })?;
            inventory_at = now;
        }
        for (&slot, pad) in &mut selected {
            let state = if focused {
                read_state(&pad.pad)
            } else {
                GamepadState::default()
            };
            if pad.previous != Some(state)
                || now.duration_since(pad.sent) >= Duration::from_millis(20)
            {
                emit(
                    &mut output,
                    CaptureEvent::State {
                        slot,
                        generation: pad.generation,
                        state,
                        elapsed_ms: epoch.elapsed().as_millis() as u64,
                    },
                )?;
                pad.previous = Some(state);
                pad.sent = now;
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(windows)]
struct TimerResolution;
#[cfg(windows)]
#[link(name = "winmm")]
unsafe extern "system" {
    fn timeBeginPeriod(period: u32) -> u32;
    fn timeEndPeriod(period: u32) -> u32;
}
#[cfg(windows)]
impl TimerResolution {
    fn new() -> Self {
        unsafe {
            timeBeginPeriod(1);
        }
        Self
    }
}
#[cfg(windows)]
impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe {
            timeEndPeriod(1);
        }
    }
}
fn read_state(pad: &sdl3::gamepad::Gamepad) -> GamepadState {
    use sdl3::gamepad::{Axis, Button};
    let mut buttons = 0;
    for (button, flag) in [
        (Button::DPadUp, 1),
        (Button::DPadDown, 2),
        (Button::DPadLeft, 4),
        (Button::DPadRight, 8),
        (Button::Start, 0x10),
        (Button::Back, 0x20),
        (Button::LeftStick, 0x40),
        (Button::RightStick, 0x80),
        (Button::LeftShoulder, 0x100),
        (Button::RightShoulder, 0x200),
        (Button::Guide, 0x400),
        (Button::South, 0x1000),
        (Button::East, 0x2000),
        (Button::West, 0x4000),
        (Button::North, 0x8000),
    ] {
        if pad.button(button) {
            buttons |= flag;
        }
    }
    GamepadState {
        buttons,
        left_x: pad.axis(Axis::LeftX),
        left_y: pad.axis(Axis::LeftY).saturating_neg(),
        right_x: pad.axis(Axis::RightX),
        right_y: pad.axis(Axis::RightY).saturating_neg(),
        left_trigger: (pad.axis(Axis::TriggerLeft).max(0) as u32 * 255 / 32767) as u8,
        right_trigger: (pad.axis(Axis::TriggerRight).max(0) as u32 * 255 / 32767) as u8,
    }
}

fn button_flag(button: sdl3::gamepad::Button) -> u16 {
    use sdl3::gamepad::Button;
    match button {
        Button::DPadUp => 1,
        Button::DPadDown => 2,
        Button::DPadLeft => 4,
        Button::DPadRight => 8,
        Button::Start => 0x10,
        Button::Back => 0x20,
        Button::LeftStick => 0x40,
        Button::RightStick => 0x80,
        Button::LeftShoulder => 0x100,
        Button::RightShoulder => 0x200,
        Button::Guide => 0x400,
        Button::South => 0x1000,
        Button::East => 0x2000,
        Button::West => 0x4000,
        Button::North => 0x8000,
        _ => 0,
    }
}

fn physical_id(path: &str) -> Option<String> {
    #[cfg(windows)]
    {
        #[link(name = "cfgmgr32")]
        unsafe extern "system" {
            fn CM_Locate_DevNodeW(node: *mut u32, id: *const u16, flags: u32) -> u32;
            fn CM_Get_Parent(parent: *mut u32, node: u32, flags: u32) -> u32;
            fn CM_Get_Device_IDW(node: u32, buffer: *mut u16, length: u32, flags: u32) -> u32;
        }
        let parts: Vec<_> = path
            .trim_start_matches("\\\\?\\")
            .split('#')
            .take(3)
            .collect();
        if parts.len() != 3 {
            return None;
        }
        let id: Vec<u16> = parts.join("\\").encode_utf16().chain(Some(0)).collect();
        let mut node = 0;
        if unsafe { CM_Locate_DevNodeW(&mut node, id.as_ptr(), 0) } != 0 {
            return None;
        }
        for _ in 0..16 {
            let mut buffer = [0u16; 512];
            if unsafe { CM_Get_Device_IDW(node, buffer.as_mut_ptr(), buffer.len() as u32, 0) } != 0
            {
                return None;
            }
            let id = String::from_utf16_lossy(&buffer[..buffer.iter().position(|c| *c == 0)?]);
            if id.starts_with("BTH") {
                return None;
            }
            if id.starts_with("USB\\VID_") && !id.contains("&MI_") {
                return Some(id);
            }
            let mut parent = 0;
            if unsafe { CM_Get_Parent(&mut parent, node, 0) } != 0 {
                return None;
            }
            node = parent;
        }
        None
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

pub fn host_available() -> Result<(), String> {
    #[cfg(windows)]
    {
        vigem_client::Client::connect().map(|_| ()).map_err(|_| {
            "Install the ViGEmBus driver on the Windows host (Frames devices setup gamepad).".into()
        })
    }
    #[cfg(not(windows))]
    {
        Err("Gamepad receiving requires a Windows host in this release.".into())
    }
}

pub struct Host {
    pub input: frames_core::devices::Sender,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for Host {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}
impl Host {
    pub fn start(output: frames_core::devices::Sender) -> anyhow::Result<Self> {
        host_available().map_err(anyhow::Error::msg)?;
        let (input, receiver) = frames_core::devices::channel();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(windows)]
        {
            let stopping = stop.clone();
            std::thread::Builder::new()
                .name("frames-virtual-gamepads".into())
                .spawn(move || {
                    if let Err(error) = windows_host(receiver, output, stopping) {
                        tracing::warn!(%error,"gamepad backend ended");
                    }
                })?;
        }
        #[cfg(not(windows))]
        {
            let _ = (receiver, output);
        }
        Ok(Self { input, stop })
    }
}

#[cfg(windows)]
struct PadWorker {
    input: frames_core::devices::Sender,
    generation: u64,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
#[cfg(windows)]
impl Drop for PadWorker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}
#[cfg(windows)]
fn windows_host(
    mut input: frames_core::devices::Receiver,
    output: frames_core::devices::Sender,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<()> {
    use std::sync::{Arc, Mutex, atomic::Ordering};
    let client = Arc::new(vigem_client::Client::connect()?);
    let clock = Arc::new(Mutex::new(frames_core::devices::InputClock::default()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut targets:HashMap<u8,PadWorker>=HashMap::new();
        let mut timer=tokio::time::interval(Duration::from_millis(5));
        loop {
            if stop.load(Ordering::Acquire) { break; }
            tokio::select! {
                _=timer.tick()=>{},
                message=input.recv()=>{
                    let Some(message)=message else { break; };
                    match message {
                        DeviceMessage::Clock { elapsed_ms }=>clock.lock().unwrap().observe(elapsed_ms,Instant::now()),
                        DeviceMessage::GamepadAttach { slot,generation,.. }=>{
                            targets.remove(&slot);
                            let (tx,rx)=frames_core::devices::channel();
                            let stopping=Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let flag=stopping.clone(); let client=client.clone(); let clock=clock.clone(); let feedback=output.clone();
                            match std::thread::Builder::new().name(format!("frames-gamepad-{slot}")).spawn(move || {
                                if let Err(error)=windows_pad(client,rx,feedback.clone(),clock,flag,slot,generation) {
                                    let _=feedback.send(DeviceMessage::GamepadStatus { slot,generation,attached:false,error:Some(error.to_string()) });
                                }
                            }) {
                                Ok(_)=>{ targets.insert(slot,PadWorker { input:tx,generation,stop:stopping }); },
                                Err(error)=>{ let _=output.send(DeviceMessage::GamepadStatus { slot,generation,attached:false,error:Some(error.to_string()) }); }
                            }
                        }
                        DeviceMessage::GamepadDetach { slot,generation }=>{
                            if targets.get(&slot).is_some_and(|t|t.generation==generation) { targets.remove(&slot); }
                        }
                        report @ DeviceMessage::GamepadState { slot,generation,.. }=>{
                            if let Some(target)=targets.get(&slot) && target.generation==generation { let _=target.input.send(report); }
                        }
                        DeviceMessage::Release=>for target in targets.values() { let _=target.input.send(DeviceMessage::Release); },
                        _=>{}
                    }
                }
            }
        }
        Ok(())
    })
}

#[cfg(windows)]
fn windows_pad(
    client: std::sync::Arc<vigem_client::Client>,
    mut input: frames_core::devices::Receiver,
    output: frames_core::devices::Sender,
    clock: std::sync::Arc<std::sync::Mutex<frames_core::devices::InputClock>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    slot: u8,
    generation: u64,
) -> anyhow::Result<()> {
    use std::sync::{Arc, Mutex, atomic::Ordering};
    use vigem_client::{TargetId, XButtons, XGamepad, Xbox360Wired};
    struct Target {
        pad: Xbox360Wired<Arc<vigem_client::Client>>,
        notification: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for Target {
        fn drop(&mut self) {
            let _ = self.pad.update(&XGamepad::default());
            let _ = self.pad.unplug();
            if let Some(thread) = self.notification.take() {
                let _ = thread.join();
            }
        }
    }
    // A slow driver plug-in cannot delay another pad's reports or watchdog.
    let mut target = Target {
        pad: Xbox360Wired::new(client, TargetId::XBOX360_WIRED),
        notification: None,
    };
    target.pad.plugin()?;
    target.pad.wait_ready()?;
    if stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let rumble = Arc::new(Mutex::new((0, 0)));
    let feedback = rumble.clone();
    target.notification = Some(
        target
            .pad
            .request_notification()?
            .spawn_thread(move |_, event| {
                *feedback.lock().unwrap() = (
                    event.large_motor as u16 * 257,
                    event.small_motor as u16 * 257,
                );
            }),
    );
    output.send(DeviceMessage::GamepadStatus {
        slot,
        generation,
        attached: true,
        error: None,
    })?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut sequence=0;
        let mut client_ms=0;
        let mut last=Instant::now()-Duration::from_secs(1);
        let mut neutral=true;
        let mut timer=tokio::time::interval(Duration::from_millis(5));
        let mut feedback_at=Instant::now();
        loop {
            if stop.load(Ordering::Acquire) { break; }
            tokio::select! {
                _=timer.tick()=>{
                    if !neutral && (last.elapsed()>=frames_core::devices::REPORT_TTL || !clock.lock().unwrap().fresh(client_ms,Instant::now())) {
                        target.pad.update(&XGamepad::default())?; neutral=true;
                    }
                    if feedback_at.elapsed()>=Duration::from_millis(20) {
                        let (low,high)=if neutral { (0,0) } else { *rumble.lock().unwrap() };
                        let _=output.send(DeviceMessage::Rumble { slot,generation,elapsed_ms:client_ms,low,high }); feedback_at=Instant::now();
                    }
                }
                message=input.recv()=>{
                    let Some(message)=message else { break; };
                    match message {
                        DeviceMessage::GamepadState { sequence:next,elapsed_ms,state,.. } if next>sequence=>{
                            sequence=next;
                            if !clock.lock().unwrap().fresh(elapsed_ms,Instant::now()) {
                                target.pad.update(&XGamepad::default())?; neutral=true; continue;
                            }
                            target.pad.update(&XGamepad { buttons:XButtons { raw:state.buttons },left_trigger:state.left_trigger,right_trigger:state.right_trigger,
                                thumb_lx:state.left_x,thumb_ly:state.left_y,thumb_rx:state.right_x,thumb_ry:state.right_y })?;
                            last=Instant::now();client_ms=elapsed_ms;neutral=false;
                        }
                        DeviceMessage::Release=>{ target.pad.update(&XGamepad::default())?;neutral=true; }
                        _=>{}
                    }
                }
            }
        }
        Ok(())
    })
}
