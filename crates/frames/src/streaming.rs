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
use tokio::sync::watch;

pub struct HostStream {
    pub task: tokio::task::JoinHandle<()>,
    ready: watch::Sender<u32>,
    generation: Arc<AtomicU32>,
    rate: Arc<AtomicU32>,
    reset: watch::Sender<u32>,
}
impl HostStream {
    pub fn set_fps(&self, fps: u32) {
        self.rate.store(fps.clamp(1, 240), Ordering::Relaxed);
    }
    pub fn acknowledge(&self, generation: u32) -> bool {
        if self.generation.load(Ordering::Acquire) != generation {
            return false;
        }
        self.ready.send_replace(generation);
        true
    }
    pub fn request_keyframe(&self, generation: u32) {
        if self.generation.load(Ordering::Acquire) == generation {
            self.reset.send_replace(generation);
        }
    }
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

#[cfg(test)]
mod ready_tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires FFmpeg; opens no native windows and sends no input"]
    async fn hidden_windows_reuse_only_a_bounded_set_of_drained_decoders() {
        let (out, _messages) = crate::session::test_outbound();
        let (ui, _updates) = frames_native::client_channel(16);
        let mut client = ClientMedia::default();
        let config = |id, generation| StreamConfig {
            window: WindowId { id, generation: 1 },
            generation,
            width: 128,
            height: 128,
            codec: codec::Codec::H264,
            encoder: "fixture".into(),
            hardware: false,
            fps: 60,
            cpu_readback: true,
        };
        let window = config(1, 1).window;
        client.start(config(1, 1), ui.clone(), &out).await.unwrap();
        let original = client.streams[&window].decoder.child.id();
        client.pause(window);
        client.start(config(1, 2), ui.clone(), &out).await.unwrap();
        assert_eq!(client.streams[&window].decoder.child.id(), original);
        assert!(client.streams[&window].paused_at.is_none());
        for id in 1..=3 {
            if id != 1 {
                client.start(config(id, 1), ui.clone(), &out).await.unwrap();
            }
            client.pause(config(id, 1).window);
        }
        assert_eq!(client.streams.len(), 2);
        assert!(!client.streams.contains_key(&window));
    }
    #[tokio::test]
    #[ignore = "requires FFmpeg; does not capture or control a desktop"]
    async fn packet_loss_and_reordering_are_isolated_to_the_affected_generation() {
        let (out, mut messages) = crate::session::test_outbound();
        let (ui, _) = frames_native::client_channel(16);
        let a = WindowId {
            id: 101,
            generation: 1,
        };
        let b = WindowId {
            id: 102,
            generation: 1,
        };
        let config = |window, generation| StreamConfig {
            window,
            generation,
            width: 128,
            height: 128,
            codec: codec::Codec::H264,
            encoder: "test".into(),
            hardware: false,
            fps: 60,
            cpu_readback: true,
        };
        let mut client = ClientMedia::default();
        client.start(config(a, 1), ui.clone(), &out).await.unwrap();
        client.start(config(b, 2), ui.clone(), &out).await.unwrap();
        while messages.try_recv().is_ok() {}
        let lost = EncodedFrame {
            window: a,
            generation: 1,
            number: 0,
            keyframe: true,
            bytes: vec![1; 3000],
        };
        let packets = media::fragment(&lost, 1200).unwrap();
        client.reassembly.push(
            &packets[0],
            std::time::Instant::now() - Duration::from_secs(1),
        );
        client.expire(&out).await.unwrap();
        assert!(
            matches!(messages.try_recv().unwrap(),Message::Keyframe{window,generation:1} if window==a)
        );
        assert!(messages.try_recv().is_err());
        assert!(!client.streams[&b].recovering);
        client.start(config(a, 3), ui, &out).await.unwrap();
        while messages.try_recv().is_ok() {}
        // Late fragments from the retired generation must not restart gen 3.
        client.packet(&packets[0], &out).await.unwrap();
        client.expire(&out).await.unwrap();
        assert!(!client.streams[&a].recovering);
        assert!(messages.try_recv().is_err());
        let data = include_bytes!("../../frames-core/fixtures/probe.h264").to_vec();
        for number in [1, 0] {
            let frame = EncodedFrame {
                window: b,
                generation: 2,
                number,
                keyframe: number == 0,
                bytes: data.clone(),
            };
            for packet in media::fragment(&frame, 1200).unwrap() {
                client.packet(&packet, &out).await.unwrap();
            }
            if number == 1 {
                assert_eq!(client.streams[&b].next, 0);
                assert!(!client.streams[&b].recovering);
            }
        }
        assert_eq!(client.streams[&b].next, 2);
        assert!(client.streams[&b].pending.is_empty());
        assert!(messages.try_recv().is_err());
    }
    #[tokio::test]
    async fn stale_recovery_cannot_restart_another_generation() {
        let (reset, mut receiver) = watch::channel(0);
        let generation = Arc::new(AtomicU32::new(8));
        let stream = HostStream {
            task: tokio::spawn(std::future::pending()),
            ready: watch::channel(0).0,
            generation: generation.clone(),
            rate: Arc::new(AtomicU32::new(24)),
            reset,
        };
        stream.request_keyframe(8);
        generation.store(9, Ordering::Release);
        stream.request_keyframe(8);
        assert!(
            tokio::time::timeout(Duration::from_millis(5), receiver.wait_for(|v| *v == 9))
                .await
                .is_err()
        );
        stream.request_keyframe(9);
        assert_eq!(*receiver.wait_for(|v| *v == 9).await.unwrap(), 9);
    }
    #[tokio::test]
    async fn stale_or_duplicate_ready_cannot_release_a_new_generation() {
        let (ready, mut receiver) = watch::channel(0);
        let generation = Arc::new(AtomicU32::new(8));
        let stream = HostStream {
            task: tokio::spawn(std::future::pending()),
            ready,
            generation: generation.clone(),
            rate: Arc::new(AtomicU32::new(24)),
            reset: watch::channel(0).0,
        };
        assert!(!stream.acknowledge(7));
        assert_eq!(*receiver.borrow(), 0);
        assert!(stream.acknowledge(8));
        assert!(stream.acknowledge(8));
        generation.store(9, Ordering::Release);
        assert!(!stream.acknowledge(8));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                receiver.wait_for(|value| *value == 9)
            )
            .await
            .is_err()
        );
        assert!(stream.acknowledge(9));
        assert_eq!(*receiver.wait_for(|value| *value == 9).await.unwrap(), 9);
    }
}
pub fn start_audio(
    desktop: Arc<dyn Desktop>,
    connection: quinn::Connection,
    out: Outbound,
    generations: Arc<AtomicU32>,
) -> HostStream {
    let (ready, mut ready_rx) = watch::channel(0);
    let active_generation = Arc::new(AtomicU32::new(0));
    let task_generation = active_generation.clone();
    let (reset, mut restart) = watch::channel(0);
    let task = tokio::spawn(async move {
        let result=async{loop{
        let d=desktop.clone();let capture=tokio::task::spawn_blocking(move||d.audio()).await??;let _guard=capture.guard;
        let mut encoder=frames_core::audio::encode(capture.format,capture.packets).await?;let generation=generations.fetch_add(1,Ordering::Relaxed);
        task_generation.store(generation,Ordering::Release);
        out.send(Message::AudioStarted{generation}).await?;tokio::time::timeout(Duration::from_secs(10),ready_rx.wait_for(|value|*value==generation)).await??;let mut number=0;
        loop{tokio::select!{_=async{let _=restart.wait_for(|value|*value==generation).await;}=>break,page=encoder.pages.recv()=>{let Some(bytes)=page else{anyhow::bail!("Opus encoder stopped");};let frame=EncodedFrame{window:media::AUDIO_WINDOW,generation,number,keyframe:number==0,bytes};number+=1;
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
    HostStream {
        task,
        ready,
        reset,
        generation: active_generation,
        rate: Arc::new(AtomicU32::new(0)),
    }
}
pub struct VideoOptions {
    pub fps: u32,
    pub show_cursor: bool,
}

pub fn start_host(
    desktop: Arc<dyn Desktop>,
    window: WindowId,
    capability: CodecCapability,
    options: VideoOptions,
    connection: quinn::Connection,
    out: Outbound,
    generations: Arc<AtomicU32>,
) -> HostStream {
    let VideoOptions { fps, show_cursor } = options;
    let (ready, mut ready_rx) = watch::channel(0);
    let active_generation = Arc::new(AtomicU32::new(0));
    let task_generation = active_generation.clone();
    let rate = Arc::new(AtomicU32::new(fps));
    let task_rate = rate.clone();
    let (reset, mut reset_task) = watch::channel(0);
    let task = tokio::spawn(async move {
        let result=async{
            let mut capture=tokio::task::spawn_blocking(move||desktop.capture(window,show_cursor)).await??;
            loop{
                let initial=loop{if let Some(frame)=capture.frames.borrow().clone(){break frame;}capture.frames.changed().await?;};
                let (width,height)=(initial.width,initial.height);let generation=generations.fetch_add(1,Ordering::Relaxed);
                let mut encoder=codec::encode_paced(width,height,60,&capability,capture.frames.clone(),task_rate.clone()).await?;
                task_generation.store(generation,Ordering::Release);
                out.send(Message::StreamStarted(StreamConfig{window,generation,width,height,codec:capability.codec,encoder:capability.encoder.clone().unwrap_or_default(),hardware:capability.hardware,fps,cpu_readback:true})).await?;
                tokio::time::timeout(Duration::from_secs(10),ready_rx.wait_for(|value|*value==generation)).await??;
                let mut number=0u64;
                loop{
                    tokio::select!{
                        _=async{let _=reset_task.wait_for(|value|*value==generation).await;}=>break,
                        changed=capture.frames.changed()=>{changed?;if capture.frames.borrow().as_ref().is_some_and(|f|f.width!=width||f.height!=height){break;}},
                        next=encoder.frames.recv()=>{
                            let Some(bytes)=next else{anyhow::bail!("encoder {0} stopped",capability.encoder.as_deref().unwrap_or("unknown"));};
                            let keyframe=codec::is_keyframe(capability.codec,&bytes,number==0);
                            let frame=EncodedFrame{window,generation,number,keyframe,bytes};number+=1;
                            let packets=media::fragment(&frame,connection.max_datagram_size().ok_or_else(||anyhow::anyhow!("peer disabled datagrams"))?.min(1200))?;
                            let deadline=tokio::time::Instant::now()+Duration::from_millis(if keyframe{100}else{30});
                            let mut congested=false;
                            for (index,packet) in packets.into_iter().enumerate(){
                                if tokio::time::Instant::now()>=deadline{congested=true;break;}
                                match tokio::time::timeout_at(deadline,connection.send_datagram_wait(packet.into())).await{
                                    Ok(result)=>result?,
                                    Err(_)=>{congested=true;break;}
                                }
                                if index%32==31{tokio::task::yield_now().await;}
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
    HostStream {
        task,
        ready,
        reset,
        generation: active_generation,
        rate,
    }
}

fn render_video(
    window: WindowId,
    mut frames: watch::Receiver<Option<frames_core::media::RawFrame>>,
    ui: frames_native::ClientSender,
) -> tokio::task::JoinHandle<()> {
    // Mark any retained pre-hide picture as seen before publishing the task.
    // The next notification must correspond to newly decoded output.
    frames.borrow_and_update();
    tokio::spawn(async move {
        while frames.changed().await.is_ok() {
            if let Some(frame) = frames.borrow_and_update().clone() {
                let _ = ui.try_send(ClientUpdate::Frame(window, frame));
            }
        }
    })
}
struct ClientStream {
    generation: u32,
    codec: codec::Codec,
    dimensions: (u32, u32),
    next: u64,
    decoder: codec::Decoder,
    render: tokio::task::JoinHandle<()>,
    recovering: bool,
    paused_at: Option<std::time::Instant>,
    pending: std::collections::BTreeMap<u64, (std::time::Instant, EncodedFrame)>,
    queued_at: std::collections::VecDeque<std::time::Instant>,
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
        let removed: Vec<_> = self
            .streams
            .keys()
            .filter(|w| !graph.windows.contains_key(w))
            .copied()
            .collect();
        for window in removed {
            self.reassembly.forget_window(window);
        }
        self.streams
            .retain(|window, _| graph.windows.contains_key(window));
    }
    pub fn pause(&mut self, window: WindowId) {
        self.reassembly.forget_window(window);
        if let Some(stream) = self.streams.get_mut(&window)
            && matches!(stream.codec, codec::Codec::H264 | codec::Codec::Hevc)
        {
            stream.render.abort();
            stream.recovering = true;
            stream.paused_at = Some(std::time::Instant::now());
            stream.pending.clear();
            stream.queued_at.clear();
        } else {
            self.streams.remove(&window);
        }
        // Idle decoders consume no video work. Retain at most two so restoring
        // a hidden window need not cold-start FFmpeg, without unbounded workers.
        let mut idle: Vec<_> = self
            .streams
            .iter()
            .filter_map(|(key, stream)| stream.paused_at.map(|at| (*key, at)))
            .collect();
        idle.sort_by_key(|(_, at)| *at);
        let excess = idle.len().saturating_sub(2);
        for (key, _) in idle.into_iter().take(excess) {
            self.streams.remove(&key);
        }
    }
    pub async fn start_audio(&mut self, generation: u32, out: &Outbound) -> anyhow::Result<()> {
        self.reassembly.forget_window(media::AUDIO_WINDOW);
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
        ui: frames_native::ClientSender,
        out: &Outbound,
    ) -> anyhow::Result<()> {
        self.reassembly.forget_window(config.window);
        // H.264/HEVC IDRs reset reference pictures without restarting the
        // decoder process. Keeping it warm avoids a cold-start recovery loop.
        if let Some(stream) = self.streams.get_mut(&config.window)
            && stream.generation < config.generation
            && stream.codec == config.codec
            && matches!(config.codec, codec::Codec::H264 | codec::Codec::Hevc)
            && stream.dimensions == (config.width, config.height)
            && !stream.decoder.input.is_closed()
            && stream.decoder.output.has_changed().is_ok()
            && (stream.paused_at.is_none() || stream.decoder.in_flight() == 0)
        {
            if stream.paused_at.take().is_some() {
                stream.render = render_video(config.window, stream.decoder.output.clone(), ui);
            }
            stream.generation = config.generation;
            stream.next = 0;
            stream.recovering = false;
            stream.pending.clear();
            stream.queued_at.clear();
            tracing::info!(window=?config.window,generation=config.generation,"reusing video decoder for new keyframe");
            out.send(Message::StreamReady {
                window: config.window,
                generation: config.generation,
            })
            .await?;
            return Ok(());
        }
        tracing::info!(window=?config.window,generation=config.generation,encoder=%config.encoder,"starting video decoder");
        let decoder = codec::decode(config.codec, config.width, config.height).await?;
        let frames = decoder.output.clone();
        let window = config.window;
        let render = render_video(window, frames, ui);
        self.streams.insert(
            window,
            ClientStream {
                generation: config.generation,
                codec: config.codec,
                dimensions: (config.width, config.height),
                next: 0,
                decoder,
                render,
                recovering: false,
                paused_at: None,
                pending: Default::default(),
                queued_at: Default::default(),
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
            self.reassembly.forget_window(window);
        }
        if let Some(stream) = self.streams.get_mut(&window)
            && stream.generation == generation
            && matches!(stream.codec, codec::Codec::H264 | codec::Codec::Hevc)
        {
            // Encoder recovery sends Stopped before Started. Keep this decoder
            // available for that new IDR; hide/evict/close remove it explicitly.
            stream.recovering = true;
            return;
        }
        if self
            .streams
            .get(&window)
            .is_some_and(|s| s.generation == generation)
        {
            self.streams.remove(&window);
        }
    }
    pub async fn packet(&mut self, packet: &[u8], out: &Outbound) -> anyhow::Result<()> {
        let Some((window, generation, number)) = Reassembler::identity(packet) else {
            return Ok(());
        };
        let current = if window == media::AUDIO_WINDOW {
            self.audio
                .as_ref()
                .is_some_and(|a| a.generation == generation)
        } else {
            self.streams
                .get(&window)
                .is_some_and(|s| s.generation == generation && !s.recovering && number >= s.next)
        };
        if !current {
            return Ok(());
        }
        let frame = self.reassembly.push(packet, std::time::Instant::now());
        self.expire(out).await?;
        let Some(frame) = frame else {
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
            if stream.pending.len() >= 4 {
                stream.recovering = true;
                stream.pending.clear();
                out.send(Message::Keyframe {
                    window: frame.window,
                    generation: frame.generation,
                })
                .await?;
            } else {
                stream
                    .pending
                    .entry(frame.number)
                    .or_insert((std::time::Instant::now(), frame));
            }
            return Ok(());
        }
        let mut next = Some(frame);
        while let Some(frame) = next.take() {
            stream.pending.retain(|number, _| *number > frame.number);
            stream.next = frame.number + 1;
            if stream.decoder.output.borrow().is_some()
                && stream.decoder.input.max_capacity() - stream.decoder.input.capacity() >= 4
            {
                // A burst can fill the queue before its writer gets scheduled.
                // Let it run before declaring a slow decoder and resetting video.
                tokio::task::yield_now().await;
            }
            let backlog = stream.decoder.in_flight();
            while stream.queued_at.len() > backlog {
                stream.queued_at.pop_front();
            }
            let overloaded = stream.decoder.output.borrow().is_some()
                && backlog >= 4
                && stream
                    .queued_at
                    .front()
                    .is_some_and(|at| at.elapsed() > Duration::from_millis(100));
            if overloaded || stream.decoder.try_submit(frame.bytes).is_err() {
                tracing::warn!(window=?frame.window,generation=frame.generation,"video decoder input queue full; requesting fresh stream");
                stream.recovering = true;
                out.send(Message::Keyframe {
                    window: frame.window,
                    generation: frame.generation,
                })
                .await?;
                stream.pending.clear();
                return Ok(());
            }
            stream.queued_at.push_back(std::time::Instant::now());
            next = stream.pending.remove(&stream.next).map(|(_, frame)| frame);
        }
        Ok(())
    }
    pub async fn expire(&mut self, out: &Outbound) -> anyhow::Result<()> {
        self.reassembly.expire(std::time::Instant::now());
        for (window, generation, number) in self.reassembly.take_expired() {
            if let Some(stream) = self.streams.get_mut(&window)
                && stream.generation == generation
                && number >= stream.next
                && !stream.recovering
            {
                tracing::warn!(
                    ?window,
                    generation = stream.generation,
                    "video assembly expired; requesting fresh stream"
                );
                stream.recovering = true;
                stream.pending.clear();
                out.send(Message::Keyframe { window, generation }).await?;
            }
            if window == media::AUDIO_WINDOW
                && self
                    .audio
                    .as_ref()
                    .is_some_and(|a| a.generation == generation)
            {
                out.send(Message::Keyframe {
                    window: media::AUDIO_WINDOW,
                    generation,
                })
                .await?;
            }
        }
        for (&window, stream) in &mut self.streams {
            if !stream.recovering
                && stream
                    .pending
                    .values()
                    .any(|(at, _)| at.elapsed() > Duration::from_millis(50))
            {
                stream.recovering = true;
                stream.pending.clear();
                out.send(Message::Keyframe {
                    window,
                    generation: stream.generation,
                })
                .await?;
            }
        }
        Ok(())
    }
}
