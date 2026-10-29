use crate::session::Outbound;
use frames_core::{
    codec::{self, CodecCapability},
    media::{self, EncodedFrame, Reassembler},
    model::WindowId,
    protocol::{Message, StreamConfig},
};
use frames_native::{ClientUpdate, Desktop};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

pub struct HostStream {
    pub task: tokio::task::JoinHandle<()>,
    pub ready: Arc<Notify>,
    pub reset: Arc<Notify>,
}
impl HostStream {
    pub async fn shutdown(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}
impl Drop for HostStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub fn start_audio(
    desktop: Arc<dyn Desktop>,
    connection: quinn::Connection,
    out: Outbound,
    generations: Arc<AtomicU32>,
) -> HostStream {
    let ready = Arc::new(Notify::new());
    let reset = Arc::new(Notify::new());
    let notify = ready.clone();
    let restart = reset.clone();
    let task = tokio::spawn(async move {
        let result=async{loop{
        let d=desktop.clone();let capture=tokio::task::spawn_blocking(move||d.audio()).await??;let _guard=capture.guard;
        let mut encoder=frames_core::audio::encode(capture.format,capture.packets).await?;let generation=generations.fetch_add(1,Ordering::Relaxed);
        out.send(Message::AudioStarted{generation}).await?;tokio::time::timeout(Duration::from_secs(10),notify.notified()).await?;let mut number=0;
        loop{tokio::select!{_=restart.notified()=>break,page=encoder.pages.recv()=>{let Some(bytes)=page else{anyhow::bail!("Opus encoder stopped");};let frame=EncodedFrame{window:media::AUDIO_WINDOW,generation,number,keyframe:number==0,bytes};number+=1;
            for packet in media::fragment(&frame,connection.max_datagram_size().unwrap_or(1200).min(1200))?{connection.send_datagram(packet.into())?;tokio::task::yield_now().await;}
        }}}
        encoder.writer.abort();let _=encoder.child.kill().await;
    }#[allow(unreachable_code)]Ok::<(),anyhow::Error>(())}.await;
        if let Err(error) = result {
            let _ = out
                .send(Message::Error {
                    message: format!("Audio: {error}"),
                })
                .await;
        }
    });
    HostStream { task, ready, reset }
}
pub fn start_host(
    desktop: Arc<dyn Desktop>,
    window: WindowId,
    capability: CodecCapability,
    fps: u32,
    connection: quinn::Connection,
    out: Outbound,
    generations: Arc<AtomicU32>,
) -> HostStream {
    let ready = Arc::new(Notify::new());
    let reset = Arc::new(Notify::new());
    let ready_task = ready.clone();
    let reset_task = reset.clone();
    let task = tokio::spawn(async move {
        let result=async{
            let mut capture=tokio::task::spawn_blocking(move||desktop.capture(window)).await??;
            loop{
                let initial=loop{if let Some(frame)=capture.frames.borrow().clone(){break frame;}capture.frames.changed().await?;};
                let (width,height)=(initial.width,initial.height);let generation=generations.fetch_add(1,Ordering::Relaxed);
                let mut encoder=codec::encode(width,height,fps,&capability,capture.frames.clone()).await?;
                out.send(Message::StreamStarted(StreamConfig{window,generation,width,height,codec:capability.codec,encoder:capability.encoder.clone().unwrap_or_default(),hardware:capability.hardware,fps,cpu_readback:true})).await?;
                tokio::time::timeout(Duration::from_secs(10),ready_task.notified()).await?;
                let mut number=0u64;
                loop{
                    tokio::select!{
                        _=reset_task.notified()=>break,
                        changed=capture.frames.changed()=>{changed?;if capture.frames.borrow().as_ref().is_some_and(|f|f.width!=width||f.height!=height){break;}},
                        next=encoder.frames.recv()=>{
                            let Some(bytes)=next else{anyhow::bail!("encoder {0} stopped",capability.encoder.as_deref().unwrap_or("unknown"));};
                            let keyframe=codec::is_keyframe(capability.codec,&bytes,number==0);
                            let frame=EncodedFrame{window,generation,number,keyframe,bytes};number+=1;
                            let packets=media::fragment(&frame,connection.max_datagram_size().ok_or_else(||anyhow::anyhow!("peer disabled datagrams"))?.min(1200))?;
                            let deadline=tokio::time::Instant::now()+Duration::from_millis(if keyframe{100}else{30});
                            let mut congested=false;
                            for packet in packets{
                                while connection.datagram_send_buffer_space()<packet.len(){if tokio::time::Instant::now()>deadline{congested=true;break;}tokio::time::sleep(Duration::from_micros(250)).await;}
                                if congested{break;}
                                connection.send_datagram(packet.into())?;
                                tokio::task::yield_now().await;
                            }
                            if congested{break;}
                        }
                    }
                }
                encoder.input_task.abort();let _=encoder.child.kill().await;
                out.send(Message::StreamStopped{window,generation}).await?;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            #[allow(unreachable_code)] Ok::<(),anyhow::Error>(())
        }.await;
        if let Err(error) = result {
            tracing::warn!(?window,%error,"stream failed");
            let _ = out
                .send(Message::Error {
                    message: format!("Window {}: {error}", window.id),
                })
                .await;
        }
    });
    HostStream { task, ready, reset }
}

struct ClientStream {
    generation: u32,
    next: u64,
    decoder: codec::Decoder,
    render: tokio::task::JoinHandle<()>,
    recovering: bool,
}
impl Drop for ClientStream {
    fn drop(&mut self) {
        self.render.abort();
    }
}
struct ClientAudio {
    generation: u32,
    decoder: frames_core::audio::Decoder,
    render: tokio::task::JoinHandle<()>,
}
impl Drop for ClientAudio {
    fn drop(&mut self) {
        self.render.abort();
    }
}
#[derive(Default)]
pub struct ClientMedia {
    streams: std::collections::HashMap<WindowId, ClientStream>,
    reassembly: Reassembler,
    audio: Option<ClientAudio>,
}
impl ClientMedia {
    pub fn clear_video(&mut self) {
        self.streams.clear();
        self.reassembly = Reassembler::default();
    }
    pub fn reconcile(&mut self, graph: &frames_core::model::WindowGraph) {
        self.streams
            .retain(|window, _| graph.windows.contains_key(window));
    }
    pub fn pause(&mut self, window: WindowId) {
        self.streams.remove(&window);
    }
    pub async fn start_audio(&mut self, generation: u32, out: &Outbound) -> anyhow::Result<()> {
        let player = tokio::task::spawn_blocking(frames_native::audio::Playback::new).await??;
        let mut decoder = frames_core::audio::decode(player.format).await?;
        let (_, empty) = tokio::sync::mpsc::channel(1);
        let mut pcm = std::mem::replace(&mut decoder.pcm, empty);
        let render = tokio::spawn(async move {
            while let Some(bytes) = pcm.recv().await {
                player.push(&bytes);
            }
        });
        self.audio = Some(ClientAudio {
            generation,
            decoder,
            render,
        });
        out.send(Message::StreamReady {
            window: media::AUDIO_WINDOW,
            generation,
        })
        .await?;
        Ok(())
    }
    pub async fn start(
        &mut self,
        config: StreamConfig,
        ui: std::sync::mpsc::SyncSender<ClientUpdate>,
        out: &Outbound,
    ) -> anyhow::Result<()> {
        let decoder = codec::decode(config.codec, config.width, config.height).await?;
        let mut frames = decoder.output.clone();
        let window = config.window;
        let render = tokio::spawn(async move {
            while frames.changed().await.is_ok() {
                let frame = frames.borrow_and_update().clone();
                if let Some(frame) = frame {
                    let _ = ui.try_send(ClientUpdate::Frame(window, frame));
                }
            }
        });
        self.streams.insert(
            window,
            ClientStream {
                generation: config.generation,
                next: 0,
                decoder,
                render,
                recovering: false,
            },
        );
        out.send(Message::StreamReady {
            window,
            generation: config.generation,
        })
        .await?;
        Ok(())
    }
    pub fn stop(&mut self, window: WindowId, generation: u32) {
        if self
            .streams
            .get(&window)
            .is_some_and(|s| s.generation == generation)
        {
            self.streams.remove(&window);
        }
    }
    pub async fn packet(&mut self, packet: &[u8], out: &Outbound) -> anyhow::Result<()> {
        let Some(frame) = self.reassembly.push(packet, std::time::Instant::now()) else {
            return Ok(());
        };
        if frame.window == media::AUDIO_WINDOW {
            if let Some(audio) = &self.audio
                && audio.generation == frame.generation
            {
                let _ = audio.decoder.input.try_send(frame.bytes);
            }
            return Ok(());
        }
        let Some(stream) = self.streams.get_mut(&frame.window) else {
            return Ok(());
        };
        if stream.generation != frame.generation || stream.recovering || frame.number < stream.next
        {
            return Ok(());
        }
        if frame.number != stream.next && !frame.keyframe {
            stream.recovering = true;
            out.send(Message::Keyframe {
                window: frame.window,
            })
            .await?;
            return Ok(());
        }
        stream.next = frame.number + 1;
        if stream.decoder.input.try_send(frame.bytes).is_err() {
            stream.recovering = true;
            out.send(Message::Keyframe {
                window: frame.window,
            })
            .await?;
        }
        Ok(())
    }
    pub async fn expire(&mut self, out: &Outbound) -> anyhow::Result<()> {
        if self.reassembly.expire(std::time::Instant::now()) > 0 {
            for (&window, stream) in &mut self.streams {
                if !stream.recovering {
                    stream.recovering = true;
                    out.send(Message::Keyframe { window }).await?;
                }
            }
            if self.audio.is_some() {
                out.send(Message::Keyframe {
                    window: media::AUDIO_WINDOW,
                })
                .await?;
            }
        }
        Ok(())
    }
}
