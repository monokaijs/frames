use crate::{Capture, CaptureStream, ClientEvent, ClientUpdate, Desktop};
use anyhow::Context;
use base64::Engine;
use frames_core::{input::InputEvent, media::RawFrame, model::*};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
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
pub fn confirm_pairing(name: &str, code: &str, reason: &str) -> anyhow::Result<bool> {
    Ok(Command::new(helper_path()?)
        .args(["--approve", name, code, reason])
        .status()?
        .success())
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
    Ok(serde_json::from_slice(&output.stdout)?)
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
    next: AtomicU64,
}
impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.get_mut().kill();
        let _ = self.child.get_mut().wait();
    }
}
impl Bridge {
    fn new() -> anyhow::Result<Arc<Self>> {
        let mut child = Command::new(helper_path()?)
            .arg("--host")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let pending = Arc::new(Mutex::new(
            HashMap::<u64, std::sync::mpsc::SyncSender<Value>>::new(),
        ));
        let frames = Arc::new(Mutex::new(
            HashMap::<u64, watch::Sender<Option<RawFrame>>>::new(),
        ));
        let audio = Arc::new(Mutex::new(None::<tokio::sync::mpsc::Sender<Vec<u8>>>));
        let sound = audio.clone();
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
                } else if value["event"] == "audio" {
                    if let Some(sender) = sound.lock().as_ref() {
                        if let Some(encoded) = value["pcm"].as_str() {
                            if let Ok(bytes) =
                                base64::engine::general_purpose::STANDARD.decode(encoded)
                            {
                                let _ = sender.try_send(bytes);
                            }
                        }
                    }
                } else if value["event"] == "frame" {
                    let Some(handle) = value["handle"].as_u64() else {
                        continue;
                    };
                    let Some(sender) = streams.lock().get(&handle).cloned() else {
                        continue;
                    };
                    let width = value["width"].as_u64().unwrap_or(0) as u32;
                    let height = value["height"].as_u64().unwrap_or(0) as u32;
                    if let Some(data) = value["bgra"].as_str() {
                        if let Ok(bgra) = base64::engine::general_purpose::STANDARD.decode(data) {
                            let frame = RawFrame {
                                width,
                                height,
                                bgra,
                            };
                            if frame.validate() {
                                sender.send_replace(Some(frame));
                            }
                        }
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
            next: AtomicU64::new(1),
        }))
    }
    fn request(&self, command: &str, data: Value) -> anyhow::Result<Value> {
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
            let response = rx.recv_timeout(std::time::Duration::from_secs(10))?;
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
    fn capture(&self, key: WindowId) -> anyhow::Result<CaptureStream> {
        let handle = self.handle(key)?;
        let (tx, frames) = watch::channel(None);
        self.bridge.frames.lock().insert(handle, tx);
        let guard = Guard {
            bridge: self.bridge.clone(),
            handle,
        };
        self.bridge.request("capture", json!({"handle":handle}))?;
        Ok(CaptureStream {
            frames,
            guard: Box::new(guard),
        })
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
        let (tx, packets) = tokio::sync::mpsc::channel(8);
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
    updates: std::sync::mpsc::Receiver<ClientUpdate>,
    events: tokio::sync::mpsc::UnboundedSender<ClientEvent>,
) -> anyhow::Result<()> {
    let mut child = Command::new(helper_path()?)
        .args(["--client", &host])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
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
                Some("view_mode") => serde_json::from_value(value["mode"].clone())
                    .ok()
                    .map(ClientEvent::ViewMode),
                Some("adb_bridge") => Some(ClientEvent::AdbBridge {
                    enabled: value["enabled"].as_bool().unwrap_or(false),
                }),
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
                        .map(|event| ClientEvent::Input { window, event })
                }),
                Some("release") => Some(ClientEvent::ReleaseInput),
                _ => None,
            };
            if let Some(event) = event {
                let _ = events.send(event);
            }
        }
        let _ = events.send(ClientEvent::Disconnected);
    });
    for update in updates {
        let value = match update {
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
                json!({"type":"frame","window":window,"width":frame.width,"height":frame.height,"bgra":base64::engine::general_purpose::STANDARD.encode(frame.bgra)})
            }
            ClientUpdate::Status(window, status) => {
                json!({"type":"status","window":window,"status":status})
            }
            ClientUpdate::Ack(window, actual, error) => {
                json!({"type":"ack","window":window,"actual":actual,"error":error})
            }
            ClientUpdate::Shutdown => break,
        };
        if serde_json::to_writer(&mut input, &value).is_err() || input.write_all(b"\n").is_err() {
            break;
        }
    }
    drop(input);
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    Ok(())
}
