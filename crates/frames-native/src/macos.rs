use crate::{Capture, CaptureStream, ClientEvent, ClientUpdate, Desktop};
use anyhow::Context;
use base64::Engine;
use frames_core::{input::InputEvent, media::RawFrame, model::*};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    os::{
        fd::AsRawFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::sync::watch;

fn helper_path() -> anyhow::Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("FRAMES_MAC_HELPER") {
        return Ok(path.into());
    }
    let executable = std::env::current_exe()?;
    let dir = executable
        .parent()
        .context("missing executable directory")?;
    for path in [
        dir.join("FramesHelper"),
        dir.join("../Resources/FramesHelper"),
    ] {
        if path.is_file() {
            return Ok(path);
        }
    }
    anyhow::bail!("FramesHelper missing; run native/macos/build.sh or set FRAMES_MAC_HELPER")
}
pub fn confirm_pairing(
    name: &str,
    code: &str,
    reason: &str,
) -> anyhow::Result<crate::ConnectionDecision> {
    let status = Command::new(helper_path()?)
        .args(["--approve", name, code, reason])
        .status()?;
    Ok(approval_decision(status.code()))
}
/// Only the approval dialog's dedicated exit codes grant access. Exit 0 is what
/// any other helper mode returns, so reading it as consent let a peer named
/// like a helper flag skip the dialog. Crashes, kills and unknown codes decline.
fn approval_decision(code: Option<i32>) -> crate::ConnectionDecision {
    match code {
        Some(10) => crate::ConnectionDecision::Trust,
        Some(11) => crate::ConnectionDecision::Accept,
        _ => crate::ConnectionDecision::Decline,
    }
}
pub fn launcher() -> anyhow::Result<()> {
    anyhow::ensure!(
        Command::new(helper_path()?)
            .arg("--launcher")
            .status()?
            .success(),
        "Frames launcher exited with an error"
    );
    Ok(())
}
pub fn permission_status(request: bool) -> anyhow::Result<Value> {
    let mut command = Command::new(helper_path()?);
    command.arg("--permissions");
    if request {
        command.arg("--request");
    }
    let output = command.output()?;
    anyhow::ensure!(
        output.status.success(),
        "permission helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut permissions: Value = serde_json::from_slice(&output.stdout)?;
    // SMAppService.status describes the login preference, not whether launchd
    // still has the service loaded. A stopped login agent must not block updates.
    permissions["sharing_running"] = launch_agent_loaded(SHARING_AGENT)?.into();
    Ok(permissions)
}

const SHARING_AGENT: &str = "dev.creaton.frames.agent";

fn launch_agent_loaded(label: &str) -> anyhow::Result<bool> {
    Ok(Command::new("/bin/launchctl")
        .args(["print", &launch_agent_service(label)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

fn launch_agent_service(label: &str) -> String {
    format!("gui/{}/{label}", unsafe { libc::getuid() })
}

/// Asks launchd to unload a launch agent from this login session only. Its
/// registration (and for sharing at login, the Login Items entry) is
/// untouched, so launchd loads it again at the next login. Killing the process
/// instead would not work: the agent is KeepAlive, so launchd restarts it.
///
/// Never waits: the request is handed to a detached `launchctl`, which finishes
/// on its own after Frames is gone. Booting out an agent that is not loaded
/// fails harmlessly, so there is no check first either.
fn stop_launch_agent(label: &str) -> std::io::Result<Child> {
    Command::new("/bin/launchctl")
        .args(["bootout", &launch_agent_service(label)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

/// Quitting Frames must stop all of Frames, including sharing at login, which
/// otherwise keeps running (and is restarted by launchd) after the launcher
/// exits. AppKit ends a Cmd+Q or Dock Quit with `exit()`, skipping the
/// launcher's own shutdown code, so this runs as an exit handler: it covers
/// those, the menu bar's Quit Frames and a normal return alike.
///
/// The handler also runs during logout, restart and shutdown, so it must not
/// delay exit: it only spawns `launchctl` (no waiting, no logging, whose
/// writers may already be torn down) and ignores every failure.
pub fn stop_background_sharing_on_exit() {
    extern "C" fn stop() {
        let _ = stop_launch_agent(SHARING_AGENT);
    }
    static REGISTERED: std::sync::Once = std::sync::Once::new();
    REGISTERED.call_once(|| {
        if unsafe { libc::atexit(stop) } != 0 {
            tracing::warn!("cannot stop sharing at login when Frames quits");
        }
    });
}

pub fn configure_login_item() -> anyhow::Result<()> {
    anyhow::ensure!(
        Command::new(helper_path()?)
            .arg("--login-item")
            .status()?
            .success(),
        "couldn't configure the sharing login item"
    );
    Ok(())
}
pub fn open_network_settings() -> anyhow::Result<()> {
    let output = Command::new(helper_path()?)
        .arg("--network-settings")
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "couldn't open Local Network settings"
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    anyhow::ensure!(
        result["opened"].as_bool() == Some(true),
        "Choose Local Network in the open Privacy & Security settings."
    );
    Ok(())
}
pub fn request_permission(kind: &str) -> anyhow::Result<()> {
    let flag = match kind {
        "screen" => "--request-screen",
        "control" => "--request-control",
        _ => return Ok(()),
    };
    anyhow::ensure!(
        Command::new(helper_path()?)
            .args(["--permissions", flag])
            .stdout(Stdio::null())
            .status()?
            .success(),
        "couldn't request {kind} permission"
    );
    Ok(())
}
pub fn show_error(title: &str, message: &str) -> anyhow::Result<()> {
    Command::new(helper_path()?)
        .args(["--error", title, message])
        .status()?;
    Ok(())
}
struct Bridge {
    child: Mutex<Child>,
    input: Mutex<ChildStdin>,
    pending: Arc<Mutex<HashMap<u64, std::sync::mpsc::SyncSender<Value>>>>,
    frames: Arc<Mutex<HashMap<u64, watch::Sender<Option<RawFrame>>>>>,
    audio: Arc<Mutex<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>>,
    encoded: Arc<Mutex<HashMap<u64, tokio::sync::mpsc::Sender<crate::NativeEncodedFrame>>>>,
    next: AtomicU64,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.get_mut().kill();
        let _ = self.child.get_mut().wait();
    }
}
impl Bridge {
    fn check_health(&self) -> anyhow::Result<()> {
        if let Some(status) = self.child.lock().try_wait()? {
            anyhow::bail!("macOS capture helper exited: {status}");
        }
        Ok(())
    }
    fn new() -> anyhow::Result<Arc<Self>> {
        // Media must never hold up acknowledgements or input on stdout.
        let (media_read, media_write) = UnixStream::pair()?;
        let media_fd = media_write.as_raw_fd();
        let (encoded_read, encoded_write) = UnixStream::pair()?;
        let encoded_fd = encoded_write.as_raw_fd();
        let (audio_read, audio_write) = UnixStream::pair()?;
        let audio_fd = audio_write.as_raw_fd();
        let mut command = Command::new(helper_path()?);
        command
            .arg("--host")
            .env("FRAMES_MEDIA_FD", "198")
            .env("FRAMES_ENCODED_FD", "199")
            .env("FRAMES_AUDIO_FD", "200")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        // SAFETY: only async-signal-safe descriptor operations run after fork.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(encoded_fd, 199) < 0
                    || libc::fcntl(199, libc::F_SETFD, 0) < 0
                    || libc::dup2(audio_fd, 200) < 0
                    || libc::fcntl(200, libc::F_SETFD, 0) < 0
                    || libc::dup2(media_fd, 198) < 0
                    || libc::fcntl(198, libc::F_SETFD, 0) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        drop(media_write);
        drop(encoded_write);
        drop(audio_write);
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let pending = Arc::new(Mutex::new(
            HashMap::<u64, std::sync::mpsc::SyncSender<Value>>::new(),
        ));
        let frames = Arc::new(Mutex::new(
            HashMap::<u64, watch::Sender<Option<RawFrame>>>::new(),
        ));
        let audio = Arc::new(Mutex::new(None::<tokio::sync::mpsc::Sender<Vec<u8>>>));
        let encoded = Arc::new(Mutex::new(HashMap::<
            u64,
            tokio::sync::mpsc::Sender<crate::NativeEncodedFrame>,
        >::new()));
        {
            let failed_encoded = encoded.clone();
            let replies = pending.clone();
            let streams = frames.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(output).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    let Ok(value) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if let Some(id) = value["id"].as_u64() {
                        if let Some(sender) = replies.lock().remove(&id) {
                            let _ = sender.send(value);
                        }
                    } else if value["event"] == "capture_error"
                        && let Some(handle) = value["handle"].as_u64()
                    {
                        tracing::warn!(handle,error=?value["error"].as_str(),"macOS window capture stopped");
                        streams.lock().remove(&handle);
                        failed_encoded.lock().remove(&handle);
                    }
                }
            });
        }
        let encoded_frames = encoded.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(encoded_read);
            while let Ok(packet) = crate::native_media::read_packet(&mut reader) {
                if packet.kind != 3
                    || packet.bytes.len() <= crate::native_media::ENCODED_METADATA_BYTES
                {
                    continue;
                }
                if let Some(sender) = encoded_frames.lock().get(&packet.handle).cloned() {
                    let p = &packet.bytes;
                    let number = u64::from_le_bytes(p[9..17].try_into().unwrap());
                    let captured_ns = u64::from_le_bytes(p[17..25].try_into().unwrap());
                    let encode_micros = u32::from_le_bytes(p[25..29].try_into().unwrap());
                    let encoded_ns = u64::from_le_bytes(p[29..37].try_into().unwrap());
                    let mut clock = libc::timespec {
                        tv_sec: 0,
                        tv_nsec: 0,
                    };
                    let valid =
                        unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut clock) } == 0;
                    let uptime_ns = (clock.tv_sec as u64)
                        .saturating_mul(1_000_000_000)
                        .saturating_add(clock.tv_nsec as u64);
                    let age = if valid {
                        uptime_ns.saturating_sub(captured_ns) / 1000
                    } else {
                        0
                    }
                    .min(10_000_000);
                    let encoded_age = if valid {
                        uptime_ns.saturating_sub(encoded_ns) / 1000
                    } else {
                        0
                    }
                    .min(10_000_000);
                    let mut bytes = vec![p[0]];
                    bytes.extend_from_slice(&p[crate::native_media::ENCODED_METADATA_BYTES..]);
                    let now = std::time::Instant::now();
                    let _ = sender.try_send(crate::NativeEncodedFrame {
                        width: packet.width,
                        height: packet.height,
                        source_width: u32::from_le_bytes(p[1..5].try_into().unwrap()),
                        source_height: u32::from_le_bytes(p[5..9].try_into().unwrap()),
                        packet: frames_core::codec::EncodedPacket {
                            surface_encoded: true,
                            encode_micros,
                            generation: 0,
                            number,
                            captured_at: now - std::time::Duration::from_micros(age),
                            encoded_at: now - std::time::Duration::from_micros(encoded_age),
                            keyframe: p[0] == 1,
                            bytes,
                        },
                    });
                }
            }
            encoded_frames.lock().clear();
        });
        let captured_audio = audio.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(audio_read);
            while let Ok(packet) = crate::native_media::read_packet(&mut reader) {
                if packet.kind == 2
                    && let Some(sender) = captured_audio.lock().as_ref()
                {
                    let _ = sender.try_send(packet.bytes);
                }
            }
        });
        let media_frames = frames.clone();
        let media_audio = audio.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(media_read);
            loop {
                match crate::native_media::read_packet(&mut reader) {
                    Ok(packet) if packet.kind == 1 || packet.kind == 4 => {
                        let now = std::time::Instant::now();
                        let mut clock = libc::timespec {
                            tv_sec: 0,
                            tv_nsec: 0,
                        };
                        let valid =
                            unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut clock) } == 0;
                        let uptime = (clock.tv_sec as u64)
                            .saturating_mul(1_000_000_000)
                            .saturating_add(clock.tv_nsec as u64);
                        let captured = if valid && packet.captured_ns > 0 {
                            now.checked_sub(std::time::Duration::from_nanos(
                                uptime.saturating_sub(packet.captured_ns),
                            ))
                            .unwrap_or(now)
                        } else {
                            now
                        };
                        if let Some(sender) = media_frames.lock().get(&packet.handle).cloned() {
                            sender.send_replace(Some(RawFrame {
                                media_generation: 0,
                                #[cfg(windows)]
                                gpu: None,
                                captured_at: captured,
                                frame_number: packet.number,
                                width: packet.width,
                                height: packet.height,
                                bgra: packet.bytes.into(),
                            }));
                        }
                    }
                    Ok(packet) => {
                        if let Some(sender) = media_audio.lock().as_ref() {
                            let _ = sender.try_send(packet.bytes);
                        }
                    }
                    Err(error) => {
                        if error.kind() != std::io::ErrorKind::UnexpectedEof {
                            tracing::warn!(%error,"Mac media channel failed");
                        }
                        media_frames.lock().clear();
                        break;
                    }
                }
            }
        });
        Ok(Arc::new(Self {
            child: Mutex::new(child),
            input: Mutex::new(input),
            pending,
            frames,
            audio,
            encoded,
            next: AtomicU64::new(1),
        }))
    }
    fn request(&self, command: &str, data: Value) -> anyhow::Result<Value> {
        self.check_health()?;
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.pending.lock().insert(id, tx);
        let result = (|| {
            let bytes =
                serde_json::to_vec(&json!({"id":id,"command":command,"data":data,"version":1}))?;
            let mut input = self.input.lock();
            input.write_all(&bytes)?;
            input.write_all(b"\n")?;
            input.flush()?;
            drop(input);
            let response = rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .with_context(|| format!("macOS {command} reply timed out"))?;
            if let Some(error) = response["error"].as_str() {
                anyhow::bail!("{error}");
            }
            Ok(response["data"].clone())
        })();
        self.pending.lock().remove(&id);
        result
    }
}
pub struct Host {
    bridge: Arc<Bridge>,
    registry: Mutex<WindowRegistry>,
    exclusions: Vec<String>,
}
impl Host {
    pub fn new(exclusions: Vec<String>) -> anyhow::Result<Self> {
        Ok(Self {
            bridge: Bridge::new()?,
            registry: Mutex::new(WindowRegistry::default()),
            exclusions,
        })
    }
    fn handle(&self, key: WindowId) -> anyhow::Result<u64> {
        self.registry
            .lock()
            .handle(key)
            .context("window no longer exists")
    }
    fn enumerate(&self, command: &str) -> anyhow::Result<Vec<RemoteWindow>> {
        let response = self
            .bridge
            .request(command, json!({"exclude":self.exclusions}))?;
        let mut windows: Vec<RemoteWindow> = serde_json::from_value(response["windows"].clone())?;
        let handles: Vec<_> = windows
            .iter()
            .map(|w| (w.key.id, stable(&w.app_id)))
            .collect();
        let keys = self.registry.lock().reconcile(&handles);
        for window in &mut windows {
            let raw = window.key.id;
            window.key = keys[&raw];
            window.parent = window.parent.and_then(|p| keys.get(&p.id).copied());
        }
        Ok(windows)
    }
}
struct Guard {
    bridge: Arc<Bridge>,
    handle: u64,
}
impl Capture for Guard {
    fn stop(&mut self) {
        self.bridge.frames.lock().remove(&self.handle);
        self.bridge.encoded.lock().remove(&self.handle);
        let _ = self.bridge.request("stop", json!({"handle":self.handle}));
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.stop();
    }
}
struct AudioGuard(Arc<Bridge>);
impl Capture for AudioGuard {
    fn stop(&mut self) {
        self.0.audio.lock().take();
        let _ = self.0.request("audio_stop", json!({}));
    }
}
impl Drop for AudioGuard {
    fn drop(&mut self) {
        self.stop();
    }
}
impl Desktop for Host {
    fn local_window_handle(&self, key: WindowId) -> anyhow::Result<u64> {
        self.handle(key)
    }
    fn preview_image(&self, window: Option<WindowId>) -> anyhow::Result<Option<Vec<u8>>> {
        let handle = window.map(|key| self.handle(key)).transpose()?;
        let result = self.bridge.request("preview", json!({"handle":handle}))?;
        let png = result["png"].as_str().context("missing preview image")?;
        Ok(Some(base64::engine::general_purpose::STANDARD.decode(png)?))
    }
    fn check_health(&self) -> anyhow::Result<()> {
        self.bridge.check_health()
    }
    fn desktop_windows(&self) -> anyhow::Result<Vec<RemoteWindow>> {
        self.enumerate("desktop")
    }
    fn windows(&self) -> anyhow::Result<Vec<RemoteWindow>> {
        self.enumerate("windows")
    }
    fn action(&self, key: WindowId, action: &WindowAction) -> anyhow::Result<()> {
        self.bridge.request(
            "action",
            json!({"handle":self.handle(key)?,"action":action}),
        )?;
        Ok(())
    }
    fn input(&self, key: WindowId, event: &InputEvent) -> anyhow::Result<()> {
        self.bridge
            .request("input", json!({"handle":self.handle(key)?,"event":event}))?;
        Ok(())
    }
    fn release_input(&self) {
        let _ = self.bridge.request("release", json!({}));
    }
    fn capture(&self, key: WindowId, show_cursor: bool) -> anyhow::Result<CaptureStream> {
        let handle = self.handle(key)?;
        let (tx, frames) = watch::channel(None);
        self.bridge.frames.lock().insert(handle, tx);
        let guard = Guard {
            bridge: self.bridge.clone(),
            handle,
        };
        self.bridge.request(
            "capture",
            json!({"handle":handle,"show_cursor":show_cursor}),
        )?;
        Ok(CaptureStream {
            frames,
            guard: Box::new(guard),
        })
    }
    fn encoded_capture(
        &self,
        key: WindowId,
        cursor: bool,
        capability: &frames_core::codec::CodecCapability,
        fps: u32,
    ) -> anyhow::Result<Option<crate::NativeEncodedCapture>> {
        if std::env::var("FRAMES_GPU").as_deref() == Ok("0")
            || !capability.hardware
            || !matches!(
                capability.codec,
                frames_core::codec::Codec::H264 | frames_core::codec::Codec::Hevc
            )
        {
            return Ok(None);
        }
        let handle = self.handle(key)?;
        let (sender, packets) = tokio::sync::mpsc::channel(2);
        self.bridge.encoded.lock().insert(handle, sender);
        let guard = Guard {
            bridge: self.bridge.clone(),
            handle,
        };
        let delivery_token = self.bridge.next.fetch_add(1, Ordering::Relaxed);
        self.bridge.request(
            "capture_encoded",
            json!({"handle":handle,"show_cursor":cursor,"codec":capability.codec,"fps":fps,"delivery_token":delivery_token}),
        )?;
        let (delivery, delivered) = std::sync::mpsc::sync_channel::<crate::NativeFrameDelivery>(1);
        let bridge = self.bridge.clone();
        std::thread::spawn(move || {
            while let Ok(frame) = delivered.recv() {
                if let Err(error)=bridge.request("encoder_delivered",json!({"handle":handle,"number":frame.number,"refresh":frame.refresh,"delivery_token":delivery_token})) {
                    tracing::warn!(%error,"native delivery acknowledgement worker stopped");break;
                }
            }
        });
        let (control, commands) = std::sync::mpsc::sync_channel(4);
        let bridge = self.bridge.clone();
        std::thread::spawn(move || {
            while let Ok(command) = commands.recv() {
                let result = match command {
                    crate::NativeEncoderCommand::Keyframe => {
                        bridge.request("encoder_keyframe", json!({"handle":handle}))
                    }
                    crate::NativeEncoderCommand::Configure {
                        bitrate,
                        fps,
                        scale,
                    } => bridge.request(
                        "encoder_configure",
                        json!({"handle":handle,"bitrate":bitrate,"fps":fps,"scale":scale}),
                    ),
                };
                if let Err(error) = result {
                    tracing::warn!(%error,"native encoder command failed");
                    break;
                }
            }
        });
        Ok(Some(crate::NativeEncodedCapture {
            packets,
            control,
            delivery,
            guard: Box::new(guard),
        }))
    }
    fn clipboard(&self) -> anyhow::Result<Option<String>> {
        Ok(self.bridge.request("clipboard", json!({}))?["text"]
            .as_str()
            .map(ToOwned::to_owned))
    }
    fn set_clipboard(&self, text: &str) -> anyhow::Result<()> {
        self.bridge.request("set_clipboard", json!({"text":text}))?;
        Ok(())
    }
    fn clipboard_image(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let reply = self.bridge.request("clipboard_image", json!({}))?;
        reply["png"]
            .as_str()
            .map(|s| {
                base64::engine::general_purpose::STANDARD
                    .decode(s)
                    .map_err(Into::into)
            })
            .transpose()
    }
    fn set_clipboard_image(&self, png: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(png.len() <= 2 * 1024 * 1024, "clipboard image too large");
        self.bridge.request(
            "set_clipboard_image",
            json!({"png":base64::engine::general_purpose::STANDARD.encode(png)}),
        )?;
        Ok(())
    }
    fn menu(&self, key: WindowId) -> anyhow::Result<Vec<MenuItem>> {
        let response = self
            .bridge
            .request("menu", json!({"handle":self.handle(key)?}))?;
        Ok(serde_json::from_value(response["items"].clone())?)
    }
    fn audio(&self) -> anyhow::Result<crate::audio::AudioCapture> {
        let (tx, packets) = tokio::sync::mpsc::channel(2);
        *self.bridge.audio.lock() = Some(tx);
        let guard = AudioGuard(self.bridge.clone());
        let response = self.bridge.request("audio_start", json!({}))?;
        let format = serde_json::from_value(response["format"].clone())?;
        Ok(crate::audio::AudioCapture {
            format,
            packets,
            guard: Box::new(guard),
        })
    }
    fn drop_files(
        &self,
        key: WindowId,
        paths: &[std::path::PathBuf],
        x: f64,
        y: f64,
    ) -> anyhow::Result<()> {
        self.bridge.request(
            "drop_files",
            json!({"handle":self.handle(key)?,"paths":paths,"x":x,"y":y}),
        )?;
        Ok(())
    }
}
fn stable(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |hash, b| {
        (hash ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

pub fn run_client(
    host: String,
    updates: crate::ClientReceiver,
    events: tokio::sync::mpsc::UnboundedSender<ClientEvent>,
) -> anyhow::Result<()> {
    let (media_read, mut media_write) = UnixStream::pair()?;
    let media_fd = media_read.as_raw_fd();
    let mut command = Command::new(helper_path()?);
    command
        .args(["--client", &host])
        .env("FRAMES_CLIENT_MEDIA_FD", "198")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(media_fd, 198) < 0 || libc::fcntl(198, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    updates.enable_native_video();
    drop(media_read);
    // A blocked video socket must never block control updates to the helper.
    let shutdown_media = media_write.try_clone()?;
    let (video_tx, video_rx) = crate::client_channel(16);
    let video_writer = std::thread::spawn(move || {
        for update in video_rx {
            let result = match update {
                ClientUpdate::Frame(window, frame) => {
                    crate::native_media::write_video(&mut media_write, window, &frame)
                }
                ClientUpdate::EncodedVideo {
                    window,
                    generation,
                    codec,
                    width,
                    height,
                    packet,
                } => crate::native_media::write_encoded(
                    &mut media_write,
                    window,
                    generation,
                    codec,
                    width,
                    height,
                    &packet,
                ),
                _ => Ok(()),
            };
            if let Err(error) = result {
                tracing::warn!(%error, "Mac client media socket closed");
                break;
            }
        }
    });
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let media_events = events.clone();
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else {
                break;
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let window = serde_json::from_value::<WindowId>(value["window"].clone()).ok();
            let event = match value["event"].as_str() {
                Some("quality") => serde_json::from_value(value["preset"].clone())
                    .ok()
                    .map(ClientEvent::Quality),
                Some("view_mode") => serde_json::from_value(value["mode"].clone())
                    .ok()
                    .map(ClientEvent::ViewMode),
                Some("remote_cursor") => value["visible"]
                    .as_bool()
                    .map(|visible| ClientEvent::RemoteCursor { visible }),
                Some("adb_bridge") => Some(ClientEvent::AdbBridge {
                    enabled: value["enabled"].as_bool().unwrap_or(false),
                }),
                Some("devices") => serde_json::from_value(value["data"].clone())
                    .ok()
                    .map(ClientEvent::Devices),
                Some("disconnect") => Some(ClientEvent::Disconnected),
                Some("cancel_transfers") => Some(ClientEvent::CancelTransfers),
                Some("send_files") => window.map(|window| ClientEvent::SendFiles {
                    window,
                    paths: serde_json::from_value(value["paths"].clone()).unwrap_or_default(),
                    x: value["x"].as_f64().unwrap_or(0.),
                    y: value["y"].as_f64().unwrap_or(0.),
                }),
                Some("download_file") => value["path"]
                    .as_str()
                    .map(|path| ClientEvent::DownloadFile { path: path.into() }),
                Some("visibility") => window.map(|window| ClientEvent::Visibility {
                    window,
                    visible: value["visible"].as_bool().unwrap_or(false),
                    focused: value["focused"].as_bool().unwrap_or(false),
                }),
                Some("action") => window.and_then(|window| {
                    serde_json::from_value(value["action"].clone())
                        .ok()
                        .map(|action| ClientEvent::Action { window, action })
                }),
                Some("input") => window.and_then(|window| {
                    serde_json::from_value(value["input"].clone())
                        .ok()
                        .map(|event| ClientEvent::TimedInput {
                            window,
                            event,
                            created: std::time::Instant::now(),
                        })
                }),
                Some("presented") => window.map(|window| ClientEvent::Presented {
                    window,
                    generation: value["generation"].as_u64().unwrap_or(0) as u32,
                    number: value["number"].as_u64().unwrap_or(0),
                    at: std::time::Instant::now(),
                }),
                Some("decoded") => window.map(|window| ClientEvent::Decoded {
                    window,
                    generation: value["generation"].as_u64().unwrap_or(0) as u32,
                    number: value["number"].as_u64().unwrap_or(0),
                    micros: value["micros"].as_u64().unwrap_or(0),
                }),
                Some("media_recovery") => window.map(|window| ClientEvent::MediaRecovery {
                    window,
                    generation: value["generation"].as_u64().unwrap_or(0) as u32,
                }),
                Some("decoder_failed") => window.map(|window| ClientEvent::DecoderFailed {
                    window,
                    generation: value["generation"].as_u64().unwrap_or(0) as u32,
                    reason: value["reason"]
                        .as_str()
                        .unwrap_or("native decoder failed")
                        .into(),
                }),
                Some("shared_alive") => Some(ClientEvent::SharedAlive {
                    created: std::time::Instant::now(),
                }),
                Some("resume") => Some(ClientEvent::ResumeInput {
                    created: std::time::Instant::now(),
                }),
                Some("release") => Some(ClientEvent::ReleaseInput),
                Some("debug_overlay") => Some(ClientEvent::DebugOverlay {
                    enabled: value["enabled"].as_bool().unwrap_or(false),
                }),
                _ => None,
            };
            if let Some(event) = event {
                let _ = events.send(event);
            }
        }
        tracing::info!("Mac client helper event pipe closed");
        let _ = events.send(ClientEvent::Disconnected);
    });
    let mut recovering_video = std::collections::HashSet::new();
    for update in updates {
        let value = match update {
            ClientUpdate::SharedSession(session) => {
                json!({"type":"shared_session","data":session,"compatibility_text":session.compatibility.description()})
            }
            ClientUpdate::SharedCursor(cursor) => json!({"type":"shared_cursor","data":cursor}),
            ClientUpdate::Devices(list) => json!({"type":"devices","data":list}),
            encoded @ ClientUpdate::EncodedVideo { .. } => {
                let (window, generation, keyframe) = if let ClientUpdate::EncodedVideo {
                    window,
                    generation,
                    packet,
                    ..
                } = &encoded
                {
                    (*window, *generation, packet.keyframe)
                } else {
                    unreachable!()
                };
                let identity = (window, generation);
                if recovering_video.contains(&identity) && !keyframe {
                    continue;
                }
                if video_tx.try_send(encoded).is_err() {
                    if recovering_video.insert(identity) {
                        let _ =
                            media_events.send(ClientEvent::MediaRecovery { window, generation });
                    }
                } else if keyframe {
                    recovering_video.remove(&identity);
                }
                continue;
            }
            ClientUpdate::Quality(preset) => json!({"type":"quality","preset":preset}),
            ClientUpdate::RemoteCursor { visible } => {
                json!({"type":"remote_cursor","visible":visible})
            }
            ClientUpdate::ViewMode { mode, error } => {
                json!({"type":"view_mode","mode":mode,"error":error})
            }
            ClientUpdate::AdbBridge {
                enabled,
                address,
                error,
            } => json!({"type":"adb_bridge","enabled":enabled,"address":address,"error":error}),
            ClientUpdate::TransferProgress { name, done, total } => {
                json!({"type":"transfer_progress","name":name,"done":done,"total":total})
            }
            ClientUpdate::ReceivedFile { path } => json!({"type":"received_file","path":path}),
            ClientUpdate::Peer {
                platform,
                device_id,
                name,
            } => json!({"type":"peer","platform":platform,"device_id":device_id,"name":name}),
            ClientUpdate::Menu { window, items } => {
                json!({"type":"menu","window":window,"items":items})
            }
            ClientUpdate::Windows(windows) => json!({"type":"windows","windows":windows}),
            ClientUpdate::Frame(window, frame) => {
                if video_tx
                    .try_send(ClientUpdate::Frame(window, frame))
                    .is_err()
                {
                    tracing::warn!("Mac client video writer stopped");
                    break;
                }
                continue;
            }
            ClientUpdate::Status(window, status) => {
                json!({"type":"status","window":window,"status":status})
            }
            ClientUpdate::DebugOverlay(window, overlay) => {
                json!({"type":"debug","window":window,"data":overlay})
            }
            ClientUpdate::Ack(window, actual, error) => {
                json!({"type":"ack","window":window,"actual":actual,"error":error})
            }
            ClientUpdate::Shutdown => {
                tracing::info!("Mac client received session shutdown");
                break;
            }
        };
        if serde_json::to_writer(&mut input, &value).is_err() || input.write_all(b"\n").is_err() {
            tracing::warn!("Mac client helper update pipe closed");
            break;
        }
    }
    tracing::info!(status = ?child.try_wait(), "Mac client bridge ending");
    drop(video_tx);
    let _ = shutdown_media.shutdown(std::net::Shutdown::Both);
    drop(input);
    // EOF lets AppKit release capture/render resources and terminate its Dock
    // proxies. Keep cleanup bounded if a helper/driver has stopped responding.
    let until = std::time::Instant::now() + std::time::Duration::from_millis(250);
    while child.try_wait()?.is_none() && std::time::Instant::now() < until {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if child.try_wait()?.is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
    let _ = reader.join();
    let _ = video_writer.join();
    Ok(())
}

#[cfg(test)]
mod launch_agent_tests {
    use super::*;

    #[test]
    fn stopped_agent_is_not_reported_as_running() {
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        if !Command::new("/bin/launchctl")
            .args(["print", &domain])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            eprintln!("No Aqua login domain; launch agent integration requires a logged-in Mac");
            return;
        }
        let label = format!("dev.frames.update-test.{}", std::process::id());
        let service = format!("{domain}/{label}");
        let path = std::env::temp_dir().join(format!("{label}.plist"));
        struct Cleanup(String, std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new("/bin/launchctl")
                    .args(["bootout", &self.0])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = std::fs::remove_file(&self.1);
            }
        }
        let _cleanup = Cleanup(service.clone(), path.clone());
        std::fs::write(
            &path,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array><string>/bin/sleep</string><string>60</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>"#
            ),
        )
        .unwrap();
        assert!(!launch_agent_loaded(&label).unwrap());
        assert!(
            Command::new("/bin/launchctl")
                .args(["bootstrap", &domain])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(launch_agent_loaded(&label).unwrap());
        assert!(
            Command::new("/bin/launchctl")
                .args(["bootout", &service])
                .status()
                .unwrap()
                .success()
        );
        // launchd finishes a bootout asynchronously; under load the service
        // can still be listed for a moment after launchctl returns.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while launch_agent_loaded(&label).unwrap() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            !launch_agent_loaded(&label).unwrap(),
            "An unloaded service must not block Restart to update"
        );
    }

    #[test]
    fn quitting_stops_a_keepalive_agent_for_good() {
        let domain = format!("gui/{}", unsafe { libc::getuid() });
        if !Command::new("/bin/launchctl")
            .args(["print", &domain])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            eprintln!("No Aqua login domain; launch agent integration requires a logged-in Mac");
            return;
        }
        let label = format!("dev.frames.quit-test.{}", std::process::id());
        let path = std::env::temp_dir().join(format!("{label}.plist"));
        struct Cleanup(String, std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new("/bin/launchctl")
                    .args(["bootout", &self.0])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = std::fs::remove_file(&self.1);
            }
        }
        let _cleanup = Cleanup(launch_agent_service(&label), path.clone());
        // Nothing loaded: the request fails quietly instead of erroring.
        stop_launch_agent(&label).unwrap().wait().unwrap();
        assert!(!launch_agent_loaded(&label).unwrap());
        // Same lifecycle keys as dev.creaton.frames.agent.plist.
        std::fs::write(
            &path,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>Label</key><string>{label}</string>
<key>ProgramArguments</key><array><string>/bin/sleep</string><string>60</string></array>
<key>RunAtLoad</key><true/><key>KeepAlive</key><true/>
</dict></plist>"#
            ),
        )
        .unwrap();
        assert!(
            Command::new("/bin/launchctl")
                .args(["bootstrap", &domain])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(launch_agent_loaded(&label).unwrap());
        // Quit does not wait for launchctl; the request must still take effect.
        drop(stop_launch_agent(&label).unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while launch_agent_loaded(&label).unwrap() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // KeepAlive restarts a killed agent within about a second; an unloaded
        // one must stay gone.
        std::thread::sleep(std::time::Duration::from_secs(2));
        assert!(
            !launch_agent_loaded(&label).unwrap(),
            "launchd must not restart sharing at login after Quit"
        );
    }
}

#[cfg(test)]
mod approval_tests {
    use crate::ConnectionDecision::*;
    #[test]
    fn only_the_dialogs_dedicated_exit_codes_grant_access() {
        assert!(matches!(super::approval_decision(Some(10)), Trust));
        assert!(matches!(super::approval_decision(Some(11)), Accept));
        // Exit 0 is what every other helper mode returns; it must not be consent.
        for code in [Some(0), Some(1), Some(2), Some(-1), None] {
            assert!(
                matches!(super::approval_decision(code), Decline),
                "{code:?}"
            );
        }
    }
}
