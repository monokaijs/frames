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
        atomic::{AtomicBool, AtomicU32, Ordering},
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
    tuning: Arc<StreamTuning>,
}
struct StreamTuning {
    bitrate: AtomicU32,
    scale: AtomicU32,
    fec: AtomicU32,
    focused: AtomicBool,
    controller: std::sync::Mutex<frames_core::adaptive::Controller>,
}
impl Default for StreamTuning {
    fn default() -> Self {
        Self {
            bitrate: AtomicU32::new(12_000_000),
            scale: AtomicU32::new(100),
            fec: AtomicU32::new(4),
            focused: AtomicBool::new(true),
            controller: std::sync::Mutex::new(frames_core::adaptive::Controller::new(
                60,
                std::time::Instant::now(),
            )),
        }
    }
}
impl StreamTuning {
    fn source_size(&self, width: u32, height: u32) {
        let mut controller = self.controller.lock().unwrap();
        controller.set_source_size(width, height);
        self.bitrate
            .store(controller.quality().bitrate, Ordering::Relaxed);
    }
}
impl HostStream {
    pub fn set_fps(&self, fps: u32) {
        self.tuning.focused.store(fps >= 45, Ordering::Relaxed);
        let mut controller = self.tuning.controller.lock().unwrap();
        controller.set_fps(fps.clamp(1, 240));
        self.rate.store(controller.quality().fps, Ordering::Relaxed);
    }
    pub fn feedback(
        &self,
        generation: u32,
        stats: frames_core::adaptive::Feedback,
        rtt: Duration,
        min_rtt: Duration,
    ) {
        if self.generation.load(Ordering::Acquire) != generation {
            return;
        }
        let mut controller = self.tuning.controller.lock().unwrap();
        let previous = controller.quality();
        let q = controller.update(stats, rtt, min_rtt, false, std::time::Instant::now());
        if q != previous {
            tracing::info!(
                generation,
                bitrate = q.bitrate,
                scale = q.scale_percent,
                fps = q.fps,
                parity = q.fec.parity_per_32,
                queue_us = stats.queue_micros,
                rtt_us = rtt.as_micros() as u64,
                "stream quality changed"
            );
        }
        self.tuning
            .fec
            .store(q.fec.parity_per_32 as u32, Ordering::Relaxed);
        self.tuning.bitrate.store(q.bitrate, Ordering::Relaxed);
        self.tuning.scale.store(q.scale_percent, Ordering::Relaxed);
        self.rate.store(q.fps, Ordering::Relaxed);
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
    async fn unavailable_audio_does_not_prevent_video_start_or_readiness() {
        let (out, mut messages) = crate::session::test_outbound();
        let (ui, updates) = frames_native::client_channel(8);
        let mut client = ClientMedia::default();
        client
            .install_audio(7, &out, &ui, Err(anyhow::anyhow!("no playback device")))
            .await
            .unwrap();
        assert!(client.audio.is_none());
        assert!(matches!(
            messages.recv().await,
            Some(Message::AudioPreference { enabled: false })
        ));
        let window = WindowId {
            id: 910,
            generation: 1,
        };
        client
            .start(fixture_config(window, 3), ui, &out)
            .await
            .unwrap();
        assert!(client.streams.contains_key(&window));
        assert!(
            matches!(messages.recv().await, Some(Message::StreamReady { window: key, generation: 3 }) if key == window)
        );
        assert!(
            matches!(updates.try_recv(), Ok(ClientUpdate::Status(key, message)) if key == window && message.contains("no playback device"))
        );
    }
    fn fixture_config(window: WindowId, generation: u32) -> StreamConfig {
        StreamConfig {
            window,
            generation,
            width: 128,
            height: 128,
            source_width: 128,
            source_height: 128,
            codec: codec::Codec::H264,
            encoder: "fixture".into(),
            hardware: false,
            fps: 60,
            cpu_readback: true,
            fec: Default::default(),
            target_bitrate: 12_000_000,
        }
    }
    #[tokio::test]
    async fn completely_lost_idle_frame_is_recovered_and_requests_are_coalesced() {
        let (out, mut messages) = crate::session::test_outbound();
        let (ui, _) = frames_native::client_channel(4);
        let window = WindowId {
            id: 909,
            generation: 1,
        };
        let mut client = ClientMedia::default();
        let decoder = codec::Decoder::external(|_| Ok(()));
        client
            .start_prepared(fixture_config(window, 7), ui, &out, Some(decoder))
            .await
            .unwrap();
        while messages.try_recv().is_ok() {}
        client.status(window, 7, 0);
        client
            .streams
            .get_mut(&window)
            .unwrap()
            .latest_host
            .as_mut()
            .unwrap()
            .1 -= Duration::from_millis(51);
        client.expire(&out).await.unwrap();
        assert!(matches!(
            messages.try_recv().unwrap(),
            Message::Keyframe { generation: 7, .. }
        ));
        client.expire(&out).await.unwrap();
        assert!(messages.try_recv().is_err());
        // The recovery response itself can disappear. Its retry uses the same generation.
        client.streams.get_mut(&window).unwrap().recovery_sent =
            Some(std::time::Instant::now() - Duration::from_millis(51));
        client.expire(&out).await.unwrap();
        assert!(matches!(
            messages.try_recv().unwrap(),
            Message::Keyframe { generation: 7, .. }
        ));
    }
    #[tokio::test]
    async fn stalled_decoder_discards_its_dependent_chain_without_blocking_other_work() {
        let (out, mut messages) = crate::session::test_outbound();
        let (ui, _) = frames_native::client_channel(4);
        let window = WindowId {
            id: 910,
            generation: 1,
        };
        let mut client = ClientMedia::default();
        client
            .start_prepared(
                fixture_config(window, 7),
                ui,
                &out,
                Some(codec::Decoder::external(|_| Ok(()))),
            )
            .await
            .unwrap();
        while messages.try_recv().is_ok() {}
        for number in 0..3 {
            let frame = EncodedFrame {
                window,
                generation: 7,
                number,
                keyframe: number == 0,
                bytes: vec![u8::from(number == 0); 100],
                captured_us: 0,
                encoded_us: 0,
            };
            for packet in media::fragment(&frame, 1200).unwrap() {
                client.packet(&packet, &out).await.unwrap();
            }
            tokio::task::yield_now().await;
        }
        assert!(client.streams[&window].recovering);
        assert!(client.streams[&window].pending.is_empty());
        assert!(matches!(
            messages.try_recv().unwrap(),
            Message::Keyframe { generation: 7, .. }
        ));
    }
    #[tokio::test]
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
            fec: media::FecProfile::default(),
            target_bitrate: 12_000_000,
            source_width: 128,
            source_height: 128,
        };
        let window = config(1, 1).window;
        client.start(config(1, 1), ui.clone(), &out).await.unwrap();
        let original = client.streams[&window].decoder.worker.id();
        client.pause(window);
        client.start(config(1, 2), ui.clone(), &out).await.unwrap();
        assert_eq!(client.streams[&window].decoder.worker.id(), original);
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
            fec: media::FecProfile::default(),
            target_bitrate: 12_000_000,
            source_width: 128,
            source_height: 128,
        };
        let mut client = ClientMedia::default();
        client.start(config(a, 1), ui.clone(), &out).await.unwrap();
        client.start(config(b, 2), ui.clone(), &out).await.unwrap();
        while messages.try_recv().is_ok() {}
        let lost = EncodedFrame {
            encoded_us: 0,
            captured_us: 0,
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
        let mut data = vec![1];
        data.extend_from_slice(include_bytes!("../../frames-core/fixtures/probe.h264"));
        for number in [1, 0] {
            let frame = EncodedFrame {
                encoded_us: 0,
                captured_us: 0,
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
            tuning: Arc::default(),
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
            tuning: Arc::default(),
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
    #[tokio::test]
    async fn recovery_retries_and_accepts_an_idr_without_restarting_or_renegotiating() {
        let (out, mut messages) = crate::session::test_outbound();
        let (ui, _updates) = frames_native::client_channel(16);
        let window = WindowId {
            id: 701,
            generation: 1,
        };
        let mut client = ClientMedia::default();
        client
            .start(
                StreamConfig {
                    window,
                    generation: 9,
                    width: 128,
                    height: 128,
                    source_width: 128,
                    source_height: 128,
                    codec: codec::Codec::H264,
                    encoder: "fixture".into(),
                    hardware: false,
                    fps: 60,
                    cpu_readback: true,
                    fec: Default::default(),
                    target_bitrate: 12_000_000,
                },
                ui,
                &out,
            )
            .await
            .unwrap();
        let worker = client.streams[&window].decoder.worker.id();
        while messages.try_recv().is_ok() {}
        let incomplete = EncodedFrame {
            encoded_us: 0,
            window,
            generation: 9,
            number: 0,
            keyframe: false,
            bytes: vec![0; 3000],
            captured_us: 0,
        };
        let packets = media::fragment(&incomplete, 1200).unwrap();
        client.reassembly.push(
            &packets[0],
            std::time::Instant::now() - Duration::from_secs(1),
        );
        client.expire(&out).await.unwrap();
        assert!(
            matches!(messages.try_recv().unwrap(),Message::Keyframe { window:w,generation:9 } if w==window)
        );
        client.streams.get_mut(&window).unwrap().recovery_sent =
            Some(std::time::Instant::now() - Duration::from_millis(51));
        client.expire(&out).await.unwrap();
        assert!(matches!(
            messages.try_recv().unwrap(),
            Message::Keyframe { generation: 9, .. }
        ));
        let mut bytes = vec![1];
        bytes.extend_from_slice(include_bytes!("../../frames-core/fixtures/probe.h264"));
        let idr = EncodedFrame {
            encoded_us: 0,
            window,
            generation: 9,
            number: 2,
            keyframe: true,
            bytes,
            captured_us: 0,
        };
        for packet in media::fragment(&idr, 1200).unwrap() {
            client.packet(&packet, &out).await.unwrap();
        }
        let stream = &client.streams[&window];
        assert_eq!(stream.generation, 9);
        assert_eq!(stream.next, 3);
        assert!(!stream.recovering);
        assert_eq!(stream.decoder.worker.id(), worker);
        assert!(messages.try_recv().is_err());
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
        let _registration = out
            .media
            .as_ref()
            .map(|sender| sender.register(media::AUDIO_WINDOW));
        let result=async{loop{
        let d=desktop.clone();let capture=tokio::task::spawn_blocking(move||d.audio()).await??;let _guard=capture.guard;
        let mut encoder=frames_core::audio::encode(capture.format,capture.packets).await?;let generation=generations.fetch_add(1,Ordering::Relaxed);
        task_generation.store(generation,Ordering::Release);
        out.send(Message::AudioStarted{generation}).await?;tokio::time::timeout(Duration::from_secs(10),ready_rx.wait_for(|value|*value==generation)).await??;let mut number=0;let mut previous:Option<Vec<u8>>=None;
        loop{tokio::select!{_=async{let _=restart.wait_for(|value|*value==generation).await;}=>break,page=encoder.packets.recv()=>{let Some(bytes)=page else{anyhow::bail!("Opus encoder stopped");};let mtu=connection.max_datagram_size().unwrap_or(1200).min(1200);let redundant=out.media.as_ref().is_some_and(|sender|sender.audio_redundancy());
            let payload=frames_core::audio::packet_with_previous(&bytes,if redundant{previous.as_deref()}else{None},mtu.saturating_sub(media::HEADER_BYTES))?;previous=Some(bytes);
            let frame=EncodedFrame{ encoded_us: media::monotonic_micros(),captured_us:0,window:media::AUDIO_WINDOW,generation,number,keyframe:number==0,bytes:payload};number+=1;
            if let Some(sender)=&out.media{let deadline=tokio::time::Instant::now()+Duration::from_millis(20);let _=sender.send(frame,connection.max_datagram_size().unwrap_or(1200),media::FecProfile{parity_per_32:0},0,deadline,true).await?;}
        }}}
        encoder.shutdown().await;
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
        tuning: Arc::default(),
    }
}
pub struct VideoOptions {
    pub fps: u32,
    pub show_cursor: bool,
    pub bandwidth_mbps: u32,
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
    let VideoOptions {
        fps,
        show_cursor,
        bandwidth_mbps,
    } = options;
    let (ready, mut ready_rx) = watch::channel(0);
    let active_generation = Arc::new(AtomicU32::new(0));
    let task_generation = active_generation.clone();
    let rate = Arc::new(AtomicU32::new(fps));
    let task_rate = rate.clone();
    let tuning = Arc::new(StreamTuning::default());
    tuning.controller.lock().unwrap().set_fps(fps);
    tuning.focused.store(fps >= 45, Ordering::Relaxed);
    tuning
        .controller
        .lock()
        .unwrap()
        .set_bandwidth_mbps(bandwidth_mbps);
    let settings = tuning.clone();
    let (reset, mut reset_task) = watch::channel(0);
    let task = tokio::spawn(async move {
        let result = async {
            let sender = out.media.clone().ok_or_else(|| anyhow::anyhow!("missing session media sender"))?;
            let _registration = sender.register(window);
            let native_desktop = desktop.clone(); let native_capability = capability.clone();
            match tokio::task::spawn_blocking(move || native_desktop.encoded_capture(window, show_cursor, &native_capability, fps)).await? {
                Ok(Some(native)) => {
                    let result = run_native_host(native, window, &capability, &connection, &out, &generations, &task_generation, &task_rate, &settings, &mut ready_rx, &mut reset_task).await;
                    if connection.close_reason().is_some() { return result; }
                    if let Err(error) = result { tracing::warn!(%error, "native GPU pipeline failed; using CPU codec backend"); }
                },
                Ok(None) => {},
                Err(error) => tracing::info!(%error, "native GPU encoder probe failed; using CPU codec backend"),
            }
            let mut capture = tokio::task::spawn_blocking(move || desktop.capture_for_encoding(window, show_cursor, capability.hardware)).await??;

            loop {
                let (source_width, source_height) = loop { if let Some(frame) = capture.frames.borrow().as_ref() { break (frame.width, frame.height); } capture.frames.changed().await?; };
                settings.source_size(source_width, source_height);
                let scale = settings.scale.load(Ordering::Relaxed);
                let (width, height) = ((source_width * scale / 100).max(1), (source_height * scale / 100).max(1));
                let generation = generations.fetch_add(1, Ordering::Relaxed);
                sender.set_budget(window, settings.bitrate.load(Ordering::Relaxed), settings.fec.load(Ordering::Relaxed) as u8, settings.focused.load(Ordering::Relaxed));
                let mut encoder = codec::encode_paced(width, height, 60, &capability, capture.frames.clone(), task_rate.clone()).await?;
                task_generation.store(generation, Ordering::Release);
                let target = sender.encoder_target(window, settings.bitrate.load(Ordering::Relaxed));
                if target > encoder.bitrate.swap(target, Ordering::Relaxed) { encoder.request_keyframe(); }
                let first_packet = tokio::time::timeout(Duration::from_secs(3), encoder.frames.recv()).await?.ok_or_else(|| anyhow::anyhow!("encoder stopped during its surface probe"))?;
                let cpu_readback = !first_packet.surface_encoded;
                let fec = media::FecProfile { parity_per_32: settings.fec.load(Ordering::Relaxed) as u8 };
                out.send(Message::StreamStarted(StreamConfig { window, generation, width, height, codec: capability.codec,
                    encoder: capability.encoder.clone().unwrap_or_default(), hardware: capability.hardware, fps,
                    cpu_readback, fec, target_bitrate: settings.bitrate.load(Ordering::Relaxed), source_width, source_height })).await?;
                tokio::time::timeout(Duration::from_secs(10), ready_rx.wait_for(|v| *v == generation)).await??;
                let mut encode_times = media::LatencyHistogram::default();
                let mut capture_encode = media::LatencyHistogram::default();
                let mut last_capture = None;
                let mut status_ticks = 0;
                let mut first_packet = Some(first_packet);
                let mut first_output = true;
                let mut number = 0; let mut latest = None; let mut awaiting_keyframe = true;
                let mut status = tokio::time::interval(Duration::from_millis(50)); status.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        changed = reset_task.changed() => {
                            changed?;
                            if *reset_task.borrow_and_update() == generation { encoder.request_keyframe(); awaiting_keyframe = true; }
                        },
                        _ = status.tick() => {
                            status_ticks += 1;
                            if status_ticks % 20 == 0 && std::env::var("FRAMES_MEDIA_DIAGNOSTICS").as_deref() == Ok("1") { tracing::info!(?window,generation,capture_encode_p50_us=capture_encode.percentile(50),capture_encode_p95_us=capture_encode.percentile(95),encode_p50_us=encode_times.percentile(50),encode_p95_us=encode_times.percentile(95),encode_p99_us=encode_times.percentile(99),"video encoder timing"); sender.report_timings(); }
                            if let Some(number) = latest { out.send(Message::MediaStatus { window, generation, number }).await?; }
                            sender.set_budget(window, settings.bitrate.load(Ordering::Relaxed), settings.fec.load(Ordering::Relaxed) as u8, settings.focused.load(Ordering::Relaxed));
                            let target = sender.encoder_target(window, settings.bitrate.load(Ordering::Relaxed));
                            let previous = encoder.bitrate.swap(target, Ordering::Relaxed);
                            if target > previous.saturating_add((previous/25).max(64_000)) { encoder.request_keyframe(); }
                            if settings.scale.load(Ordering::Relaxed) != scale { break; }
                        },
                        changed = capture.frames.changed() => {
                            changed?;
                            if capture.frames.borrow().as_ref().is_some_and(|f| f.width != source_width || f.height != source_height) { break; }
                        },
                        packet = async { if let Some(packet) = first_packet.take() { Some(packet) } else { encoder.frames.recv().await } } => {
                            let Some(packet) = packet else { anyhow::bail!("encoder stopped"); };
                            encode_times.record(packet.encode_micros as u64);
                            capture_encode.record(packet.encoded_at.saturating_duration_since(packet.captured_at).as_micros() as u64);
                            if last_capture != Some(packet.captured_at) && packet.encode_micros > 25_000 {
                                let q = settings.controller.lock().unwrap().congestion(std::time::Instant::now());
                                settings.bitrate.store(q.bitrate,Ordering::Relaxed);settings.scale.store(q.scale_percent,Ordering::Relaxed);task_rate.store(q.fps,Ordering::Relaxed);
                            }
                            last_capture = Some(packet.captured_at);
                            if awaiting_keyframe && !packet.keyframe { continue; }
                            awaiting_keyframe = false;
                            let captured_us = media::timestamp_micros(packet.captured_at);
                            let deadline = (if first_output { tokio::time::Instant::now() } else { tokio::time::Instant::from_std(packet.encoded_at) }) + Duration::from_millis(if packet.keyframe { 50 } else { 25 }); first_output = false;
                            let frame = EncodedFrame { encoded_us: media::timestamp_micros(packet.encoded_at), captured_us: 0, window, generation, number, keyframe: packet.keyframe, bytes: packet.bytes };
                            latest = Some(number); number += 1;
                            let fec = if std::env::var("FRAMES_FEC").as_deref() == Ok("0") { media::FecProfile { parity_per_32: 0 } }
                                else { media::FecProfile { parity_per_32: if frame.keyframe { 8 } else { settings.fec.load(Ordering::Relaxed) as u8 } } };
                            let mtu = connection.max_datagram_size().ok_or_else(|| anyhow::anyhow!("peer disabled datagrams"))?;
                            if !sender.send(frame, mtu, fec, captured_us, deadline, settings.focused.load(Ordering::Relaxed)).await? {
                                awaiting_keyframe = true; encoder.request_keyframe();
                                let q = settings.controller.lock().unwrap().congestion(std::time::Instant::now());
                                settings.bitrate.store(q.bitrate, Ordering::Relaxed); settings.scale.store(q.scale_percent, Ordering::Relaxed); task_rate.store(q.fps, Ordering::Relaxed);
                            }
                        }
                    }
                }
                encoder.shutdown().await;
                out.send(Message::StreamStopped { window, generation }).await?;
                sender.remove(window);
            }
            #[allow(unreachable_code)] Ok::<(), anyhow::Error>(())
        }.await;
        if let Err(error) = result {
            tracing::warn!(?window, %error, "stream failed");
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
        tuning,
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
    config: StreamConfig,
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
    recovery_sent: Option<std::time::Instant>,
    latest_host: Option<(u64, std::time::Instant)>,
    presented: u64,
    last_presented: Option<(u64, std::time::Instant)>,
    gaps: u64,
    gap_histogram: media::LatencyHistogram,
    late: u64,
    capture_times: std::collections::VecDeque<(u64, std::time::Instant)>,
    capture_to_present: media::LatencyHistogram,
    encoded_receive: media::LatencyHistogram,
    receive_decode: media::LatencyHistogram,
    received_at: std::collections::VecDeque<(u64, std::time::Instant)>,
    recovery_started: Option<std::time::Instant>,
    recovery_times: media::LatencyHistogram,
}
impl Drop for ClientStream {
    fn drop(&mut self) {
        self.render.abort();
    }
}
struct ClientAudio {
    jitter: frames_core::audio::JitterBuffer,
    playback: Arc<frames_native::audio::PlaybackStats>,
    generation: u32,
    decoder: frames_core::audio::Decoder,
    render: tokio::task::JoinHandle<()>,
}
impl ClientAudio {
    fn drain(&mut self, now: std::time::Instant) {
        self.jitter.drain(now, |packet| {
            let _ = self.decoder.input.try_send(packet);
        });
    }
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
    clock: media::ClockEstimator,
    audio: Option<ClientAudio>,
    audio_error: Option<String>,
}
impl ClientMedia {
    pub fn clear_video(&mut self) {
        self.streams.clear();
        self.reassembly = Reassembler::default();
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
        // a hidden window need not cold-start a codec worker, without unbounded workers.
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
    async fn install_audio(
        &mut self,
        generation: u32,
        out: &Outbound,
        ui: &frames_native::ClientSender,
        prepared: anyhow::Result<(frames_native::audio::Playback, frames_core::audio::Decoder)>,
    ) -> anyhow::Result<()> {
        self.reassembly.forget_window(media::AUDIO_WINDOW);
        self.audio = None;
        let (player, mut decoder) = match prepared {
            Ok(prepared) => {
                self.audio_error = None;
                prepared
            }
            Err(error) => {
                let message = format!("Audio unavailable: {error}");
                tracing::warn!(%error, "audio playback unavailable; retaining video and control");
                for &window in self.streams.keys() {
                    let _ = ui.try_send(ClientUpdate::Status(window, message.clone()));
                }
                self.audio_error = Some(message);
                out.send(Message::AudioPreference { enabled: false })
                    .await?;
                return Ok(());
            }
        };
        let playback = player.stats.clone();
        let (_, empty) = tokio::sync::mpsc::channel(1);
        let mut pcm = std::mem::replace(&mut decoder.pcm, empty);
        let render = tokio::spawn(async move {
            while let Some(bytes) = pcm.recv().await {
                player.push(&bytes);
            }
        });
        self.audio = Some(ClientAudio {
            jitter: Default::default(),
            playback,
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
    #[cfg(test)]
    pub async fn start(
        &mut self,
        config: StreamConfig,
        ui: frames_native::ClientSender,
        out: &Outbound,
    ) -> anyhow::Result<()> {
        self.start_prepared(config, ui, out, None).await
    }
    async fn start_prepared(
        &mut self,
        config: StreamConfig,
        ui: frames_native::ClientSender,
        out: &Outbound,
        prepared: Option<codec::Decoder>,
    ) -> anyhow::Result<()> {
        if let Some(message) = &self.audio_error {
            let _ = ui.try_send(ClientUpdate::Status(config.window, message.clone()));
        }
        self.reassembly.forget_window(config.window);
        // H.264/HEVC IDRs reset reference pictures without restarting the
        // decoder worker. Keeping it warm avoids a cold-start recovery loop.
        if let Some(stream) = self.streams.get_mut(&config.window)
            && stream.generation < config.generation
            && stream.codec == config.codec
            && matches!(config.codec, codec::Codec::H264 | codec::Codec::Hevc)
            && stream.dimensions == (config.width, config.height)
            && !stream.decoder.is_external()
            && !stream.decoder.input.is_closed()
            && stream.decoder.output.has_changed().is_ok()
            && (stream.paused_at.is_none() || stream.decoder.in_flight() == 0)
        {
            if stream.paused_at.take().is_some() {
                stream.render = render_video(config.window, stream.decoder.output.clone(), ui);
            }
            stream.decoder.set_generation(config.generation);
            stream.last_presented = None;
            stream.latest_host = None;
            stream.recovery_sent = None;
            stream.config = config.clone();
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
        let decoder = if let Some(decoder) = prepared {
            decoder
        } else {
            codec::decode(config.codec, config.width, config.height).await?
        };
        decoder.set_generation(config.generation);
        let frames = decoder.output.clone();
        let window = config.window;
        let render = render_video(window, frames, ui);
        self.streams.insert(
            window,
            ClientStream {
                config: config.clone(),
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
                recovery_sent: None,
                latest_host: None,
                presented: 0,
                last_presented: None,
                gaps: 0,
                gap_histogram: Default::default(),
                late: 0,
                capture_times: Default::default(),
                capture_to_present: Default::default(),
                encoded_receive: Default::default(),
                receive_decode: Default::default(),
                received_at: Default::default(),
                recovery_started: None,
                recovery_times: Default::default(),
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
            self.streams.get(&window).is_some_and(|s| {
                s.generation == generation
                    && (!s.recovering || Reassembler::keyframe(packet))
                    && number >= s.next
            })
        };
        if !current {
            return Ok(());
        }
        let pending_bytes: usize = self
            .streams
            .values()
            .map(|s| {
                s.pending
                    .values()
                    .map(|(_, f)| f.bytes.len())
                    .sum::<usize>()
                    + s.decoder.retained_bytes()
            })
            .sum::<usize>()
            + self.audio.as_ref().map_or(0, |a| a.jitter.retained_bytes());
        self.reassembly.set_external_bytes(pending_bytes);
        let frame = self.reassembly.push(packet, std::time::Instant::now());
        self.expire(out).await?;
        let Some(frame) = frame else {
            return Ok(());
        };
        let encoded_age = self.clock.age(frame.encoded_us, media::monotonic_micros());
        let age_limit = Duration::from_millis(if frame.window == media::AUDIO_WINDOW {
            20
        } else {
            50
        }) + Duration::from_micros(self.clock.uncertainty_us as u64);
        if encoded_age.is_some_and(|age| age > age_limit) {
            if frame.window == media::AUDIO_WINDOW {
                if let Some(audio) = &mut self.audio {
                    audio.jitter.skip_before(frame.number + 1);
                }
            } else if let Some(stream) = self.streams.get_mut(&frame.window) {
                stream.recovering = true;
                stream.pending.clear();
                stream.late += 1;
            }
            self.expire(out).await?;
            return Ok(());
        }
        if frame.window == media::AUDIO_WINDOW {
            if let Some(audio) = &mut self.audio
                && audio.generation == frame.generation
                && frame.number >= audio.jitter.next
                && let Some((current, previous)) = frames_core::audio::split_packet(&frame.bytes)
            {
                audio.jitter.push(frame.number, current, previous);
                audio.drain(std::time::Instant::now());
            }
            return Ok(());
        }
        let Some(stream) = self.streams.get_mut(&frame.window) else {
            return Ok(());
        };
        if stream.generation != frame.generation
            || (stream.recovering && !frame.keyframe)
            || frame.number < stream.next
        {
            return Ok(());
        }
        if let Some(age) = encoded_age {
            stream.encoded_receive.record(age.as_micros() as u64);
        }
        if frame.keyframe {
            if let Some(started) = stream.recovery_started.take() {
                stream
                    .recovery_times
                    .record(started.elapsed().as_micros() as u64);
            }
            stream.recovering = false;
            stream.recovery_sent = None;
            stream.decoder.discard_before(frame.number);
            self.reassembly
                .forget_before(frame.window, frame.generation, frame.number + 1);
        }
        if frame.number != stream.next && !frame.keyframe {
            if stream.pending.len() >= 4
                || self.reassembly.retained_bytes() + pending_bytes + frame.bytes.len()
                    > media::MAX_REASSEMBLY_BYTES
            {
                stream.recovering = true;
                stream.recovery_sent = Some(std::time::Instant::now());
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
            if stream.decoder.output.borrow().is_some() && stream.decoder.input.capacity() == 0 {
                // A burst can fill the queue before its writer gets scheduled.
                // Let it run before declaring a slow decoder and resetting video.
                tokio::task::yield_now().await;
            }
            let backlog = stream.decoder.in_flight();
            while stream.queued_at.len() > backlog {
                stream.queued_at.pop_front();
            }
            let overloaded = stream.decoder.output.borrow().is_some()
                && backlog >= 2
                && stream
                    .queued_at
                    .front()
                    .is_some_and(|at| at.elapsed() > Duration::from_millis(40));
            let now = std::time::Instant::now();
            let captured_at = self
                .clock
                .age(frame.captured_us, media::monotonic_micros())
                .and_then(|age| now.checked_sub(age));
            if let Some(at) = captured_at {
                stream.capture_times.push_back((frame.number, at));
                while stream.capture_times.len() > 128 {
                    stream.capture_times.pop_front();
                }
            }
            stream.received_at.push_back((frame.number, now));
            while stream.received_at.len() > 128 {
                stream.received_at.pop_front();
            }
            let packet = codec::EncodedPacket {
                surface_encoded: false,
                encode_micros: 0,
                generation: frame.generation,
                number: frame.number,
                captured_at: captured_at.unwrap_or(now),
                encoded_at: std::time::Instant::now(),
                keyframe: frame.keyframe,
                bytes: frame.bytes,
            };
            if overloaded || stream.decoder.try_submit_packet(packet).is_err() {
                tracing::warn!(window=?frame.window,generation=frame.generation,"video decoder input queue full; requesting fresh stream");
                stream.recovering = true;
                stream.recovery_sent = Some(std::time::Instant::now());
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
    pub fn status(&mut self, window: WindowId, generation: u32, number: u64) {
        if let Some(s) = self.streams.get_mut(&window)
            && s.generation == generation
        {
            // Repeated status for an idle desktop must not keep postponing detection.
            if s.latest_host.is_none_or(|(old, _)| number > old) {
                s.latest_host = Some((number, std::time::Instant::now()));
            }
        }
    }
    pub fn presented(
        &mut self,
        window: WindowId,
        generation: u32,
        number: u64,
        at: std::time::Instant,
    ) {
        if let Some(s) = self.streams.get_mut(&window)
            && s.generation == generation
        {
            if let Some((old, previous)) = s.last_presented {
                if number <= old {
                    return;
                }
                let gap = at.saturating_duration_since(previous);
                s.gap_histogram.record(gap.as_micros() as u64);
                if gap > Duration::from_millis(50) {
                    s.gaps += 1;
                }
            }
            if let Some((_, captured)) = s.capture_times.iter().find(|(n, _)| *n == number) {
                s.capture_to_present
                    .record(at.saturating_duration_since(*captured).as_micros() as u64);
            }
            s.capture_times.retain(|(n, _)| *n > number);
            s.last_presented = Some((number, at));
            s.presented += 1;
        }
    }
    pub fn next_deadline(&self, retry: Duration) -> std::time::Instant {
        let mut next = self
            .reassembly
            .next_deadline()
            .unwrap_or_else(|| std::time::Instant::now() + Duration::from_millis(100));
        if let Some(audio) = &self.audio
            && let Some(at) = audio.jitter.deadline
        {
            next = next.min(at);
        }
        for s in self.streams.values().filter(|s| s.paused_at.is_none()) {
            if s.recovering && s.recovery_sent.is_none() {
                next = next.min(std::time::Instant::now());
            }
            if let Some(at) = s.recovery_sent {
                next = next.min(at + retry);
            }
            for (at, _) in s.pending.values() {
                next = next.min(*at + Duration::from_millis(25));
            }
            if let Some((number, at)) = s.latest_host
                && number >= s.next
            {
                next = next.min(at + Duration::from_millis(50));
            }
        }
        next
    }
    pub async fn feedback(&self, out: &Outbound) -> anyhow::Result<()> {
        if let Some(audio) = &self.audio {
            let stats = frames_core::adaptive::Feedback {
                delivery: media::DeliveryStats {
                    received_shards: audio.jitter.received,
                    missing_shards: audio.jitter.concealed,
                    ..Default::default()
                },
                audio_underruns: audio.playback.underruns.load(Ordering::Relaxed),
                ..Default::default()
            };
            out.send(Message::Feedback {
                window: media::AUDIO_WINDOW,
                generation: audio.generation,
                stats,
            })
            .await?;
        }
        for (&window, s) in &self.streams {
            if s.paused_at.is_some() {
                continue;
            }
            let stats = frames_core::adaptive::Feedback {
                delivery: self.reassembly.stats(window),
                decoded: s.decoder.metrics.decoded.load(Ordering::Relaxed),
                presented: s.presented,
                late_frames: s.late,
                decoder_backlog: s.decoder.in_flight() as u32,
                presentation_gaps: s.gaps,
                decode_p95_micros: s
                    .decoder
                    .metrics
                    .histogram
                    .lock()
                    .unwrap()
                    .percentile(95)
                    .min(u32::MAX as u64) as u32,
                present_gap_p95_micros: s.gap_histogram.percentile(95).min(u32::MAX as u64) as u32,
                ..Default::default()
            };
            out.send(Message::Feedback {
                window,
                generation: s.generation,
                stats,
            })
            .await?;
        }
        Ok(())
    }
    pub async fn expire(&mut self, out: &Outbound) -> anyhow::Result<()> {
        self.expire_with_retry(out, Duration::from_millis(50)).await
    }
    async fn expire_with_retry(&mut self, out: &Outbound, retry: Duration) -> anyhow::Result<()> {
        let now = std::time::Instant::now();
        self.reassembly.expire(now);
        if let Some(audio) = &mut self.audio {
            audio.drain(now);
        }
        for (window, generation, number) in self.reassembly.take_expired() {
            if let Some(s) = self.streams.get_mut(&window)
                && s.generation == generation
                && number >= s.next
            {
                s.recovering = true;
                s.pending.clear();
                s.late += 1;
            }
        }
        for (&window, s) in &mut self.streams {
            if s.paused_at.is_some() {
                continue;
            }
            if s.decoder.metrics.failed.swap(false, Ordering::AcqRel)
                || s.pending
                    .values()
                    .any(|(at, _)| now.saturating_duration_since(*at) >= Duration::from_millis(25))
                || s.latest_host.is_some_and(|(number, at)| {
                    number >= s.next
                        && now.saturating_duration_since(at) >= Duration::from_millis(50)
                })
            {
                s.recovering = true;
                s.pending.clear();
            }
            if s.recovering
                && s.recovery_sent
                    .is_none_or(|at| now.saturating_duration_since(at) >= retry)
            {
                out.send(Message::Keyframe {
                    window,
                    generation: s.generation,
                })
                .await?;
                s.recovery_started.get_or_insert(now);
                s.recovery_sent = Some(now);
                // Retire the watchdog until the next sender status or retry.
                s.latest_host = None;
            }
        }
        Ok(())
    }
}

pub enum MediaCommand {
    Clock(u64, u64, u64, u64),
    Start(StreamConfig),
    Stop(WindowId, u32),
    Pause(WindowId),
    Clear,
    Reconcile(Vec<WindowId>),
    Audio(u32),
    Status(WindowId, u32, u64),
    Decoded(WindowId, u32, u64, u64),
    Presented(WindowId, u32, u64, std::time::Instant),
    Recover(WindowId, u32),
    Failed(WindowId, u32, String),
}
pub struct ClientMediaWorker {
    commands: tokio::sync::mpsc::Sender<MediaCommand>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for ClientMediaWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl ClientMediaWorker {
    pub fn new(
        connection: quinn::Connection,
        ui: frames_native::ClientSender,
        out: Outbound,
    ) -> Self {
        let (commands, mut incoming) = tokio::sync::mpsc::channel(64);
        let runtime = tokio::runtime::Handle::current();
        let task = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
            let mut media = ClientMedia::default();
            let mut feedback_ticks = 0;
            let mut last_probe = None;
            let mut feedback = tokio::time::interval(Duration::from_millis(100));
            feedback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut starting = tokio::task::JoinSet::new();
            let mut audio_starting = tokio::task::JoinSet::new();
            let mut requested_audio = 0;
            let mut software_only = std::collections::HashSet::new();
            let mut requested = std::collections::HashMap::new();
            let mut serial = 0u64;
            loop {
                let retry = (connection.rtt() * 2)
                    .clamp(Duration::from_millis(50), Duration::from_millis(250));
                let deadline = media.next_deadline(retry);
                let result: anyhow::Result<()> = async { tokio::select! {
                    _ = connection.closed() => return Ok(()),
                    command = incoming.recv() => match command {
                        None => return Ok(()),
                        Some(MediaCommand::Start(config)) => {
                            anyhow::ensure!(config.width > 0 && config.height > 0 && config.width <= 8192 && config.height <= 8192 && config.fec.validate(), "invalid media configuration");
                            serial += 1; requested.insert(config.window, serial);
                            if starting.len() >= 16 { anyhow::bail!("too many pending media configurations"); }
                            let tag = serial; let software = software_only.contains(&config.window); let native_ui = ui.clone();
                            starting.spawn(async move { let decoder = prepare_decoder(&config, native_ui, software).await; (tag, config, decoder) });
                        },
                        Some(MediaCommand::Stop(w, g)) => media.stop(w, g),
                        Some(MediaCommand::Pause(w)) => { requested.remove(&w); media.pause(w); },
                        Some(MediaCommand::Clear) => { requested.clear(); starting.abort_all(); media.clear_video(); },
                        Some(MediaCommand::Reconcile(windows)) => {
                            requested.retain(|w, _| windows.contains(w));
                            let removed: Vec<_> = media.streams.keys().filter(|w| !windows.contains(w)).copied().collect();
                            for w in removed { media.streams.remove(&w); media.reassembly.forget_window(w); }
                        },
                        Some(MediaCommand::Audio(g)) => {
                            requested_audio = g;
                            if audio_starting.len() >= 2 { audio_starting.abort_all(); }
                            audio_starting.spawn(async move { let result = async {
                                let player = tokio::task::spawn_blocking(frames_native::audio::Playback::new).await??;
                                let decoder = frames_core::audio::decode(player.format).await?;
                                Ok::<_, anyhow::Error>((player,decoder))
                            }.await; (g,result) });
                        },
                        Some(MediaCommand::Clock(t0,t1,t2,t3)) => if last_probe == Some(t0) { media.clock.observe(t0,t1,t2,t3); last_probe = None; },
                        Some(MediaCommand::Status(w, g, n)) => media.status(w, g, n),
                        Some(MediaCommand::Decoded(w, g, n, micros)) => { if let Some(s) = media.streams.get_mut(&w) && s.generation == g { s.decoder.acknowledge_external(n, micros); if let Some((_,at)) = s.received_at.iter().find(|(number,_)|*number==n) { s.receive_decode.record(at.elapsed().as_micros() as u64); } s.received_at.retain(|(number,_)|*number>n); } },
                        Some(MediaCommand::Presented(w, g, n, at)) => media.presented(w, g, n, at),
                        Some(MediaCommand::Recover(w, g)) => {
                            if let Some(s) = media.streams.get_mut(&w) && s.generation == g {
                                s.recovering = true; s.pending.clear(); s.decoder.discard_before(s.next);
                            }
                        },
                        Some(MediaCommand::Failed(w, g, reason)) => {
                            if let Some(s) = media.streams.get(&w) && s.generation == g && software_only.insert(w) {
                                tracing::warn!(?w, %reason, "switching this window to software decoding");
                                let config = s.config.clone(); media.streams.remove(&w); serial += 1; requested.insert(w, serial); let tag = serial;
                                starting.spawn(async move { let decoder = codec::decode_software(config.codec, config.width, config.height).await; (tag, config, decoder) });
                                out.send(Message::Keyframe { window: w, generation: g }).await?;
                            }
                        },
                    },
                    initialized = audio_starting.join_next(), if !audio_starting.is_empty() => {
                        if let Some(Ok((g, prepared))) = initialized && requested_audio == g { media.install_audio(g, &out, &ui, prepared).await?; }
                    },
                    initialized = starting.join_next(), if !starting.is_empty() => {
                        if let Some(Ok((tag, config, decoder))) = initialized && requested.get(&config.window) == Some(&tag) {
                            media.start_prepared(config, ui.clone(), &out, Some(decoder?)).await?;
                        }
                    },
                    packet = connection.read_datagram() => { media.packet(&packet?, &out).await?; },
                    _ = tokio::time::sleep_until(deadline.into()) => { media.expire_with_retry(&out, retry).await?; },
                    _ = feedback.tick() => {
                        media.expire_with_retry(&out, retry).await?; media.feedback(&out).await?;
                        feedback_ticks += 1;
                        if feedback_ticks % 10 == 1 { let client_us = media::monotonic_micros(); last_probe = Some(client_us); out.send(Message::ClockProbe { client_us }).await?; }
                        if feedback_ticks % 10 == 0 && std::env::var("FRAMES_MEDIA_DIAGNOSTICS").as_deref() == Ok("1") {
                            for (window,s) in &media.streams { tracing::info!(?window,generation=s.generation,decoded=s.decoder.metrics.decoded.load(Ordering::Relaxed),presented=s.presented,gaps=s.gaps,
                                capture_present_p50_us=s.capture_to_present.percentile(50),capture_present_p95_us=s.capture_to_present.percentile(95),capture_present_p99_us=s.capture_to_present.percentile(99),present_gap_p95_us=s.gap_histogram.percentile(95),present_gap_p99_us=s.gap_histogram.percentile(99),present_gap_max_us=s.gap_histogram.percentile(100),encoded_receive_p95_us=s.encoded_receive.percentile(95),receive_decode_p95_us=s.receive_decode.percentile(95),decode_p95_us=s.decoder.metrics.histogram.lock().unwrap().percentile(95),recovery_p95_us=s.recovery_times.percentile(95),clock_uncertainty_us=media.clock.uncertainty_us,"media delivery"); }
                        }
                    },
                } Ok(()) }.await;
                if connection.close_reason().is_some() || incoming.is_closed() {
                    break;
                }
                if let Err(error) = result {
                    tracing::warn!(%error, "media worker stopped");
                    connection.close(1u32.into(), b"media worker failed");
                    break;
                }
            }
        })
        });
        Self { commands, task }
    }
    pub async fn send(&self, command: MediaCommand) -> anyhow::Result<()> {
        self.commands
            .send(command)
            .await
            .map_err(|_| anyhow::anyhow!("media worker stopped"))
    }
}

async fn prepare_decoder(
    config: &StreamConfig,
    ui: frames_native::ClientSender,
    software: bool,
) -> anyhow::Result<codec::Decoder> {
    #[cfg(target_os = "macos")]
    if !software
        && ui.supports_native_video()
        && std::env::var("FRAMES_GPU").as_deref() != Ok("0")
        && std::env::var("FRAMES_HW_DECODE").as_deref() != Ok("0")
        && matches!(config.codec, codec::Codec::H264 | codec::Codec::Hevc)
    {
        let config = config.clone();
        return Ok(codec::Decoder::external(move |packet| {
            ui.try_send(ClientUpdate::EncodedVideo {
                window: config.window,
                generation: config.generation,
                codec: config.codec,
                width: config.width,
                height: config.height,
                packet,
            })
            .map_err(|e| anyhow::anyhow!("native video queue: {e:?}"))
        }));
    }
    #[cfg(not(target_os = "macos"))]
    let _ = ui.supports_native_video();
    if software {
        codec::decode_software(config.codec, config.width, config.height).await
    } else {
        codec::decode_for_presentation(config.codec, config.width, config.height).await
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_native_host(
    mut capture: frames_native::NativeEncodedCapture,
    window: WindowId,
    capability: &CodecCapability,
    connection: &quinn::Connection,
    out: &Outbound,
    generations: &AtomicU32,
    active_generation: &AtomicU32,
    rate: &AtomicU32,
    tuning: &StreamTuning,
    ready: &mut watch::Receiver<u32>,
    reset: &mut watch::Receiver<u32>,
) -> anyhow::Result<()> {
    let sender = out
        .media
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing media sender"))?;
    let mut first = capture
        .packets
        .recv()
        .await
        .ok_or_else(|| anyhow::anyhow!("native encoder stopped"))?;

    loop {
        tuning.source_size(first.source_width, first.source_height);
        let size = (first.width, first.height);
        let generation = generations.fetch_add(1, Ordering::Relaxed);
        active_generation.store(generation, Ordering::Release);
        sender.set_budget(
            window,
            tuning.bitrate.load(Ordering::Relaxed),
            tuning.fec.load(Ordering::Relaxed) as u8,
            tuning.focused.load(Ordering::Relaxed),
        );
        out.send(Message::StreamStarted(StreamConfig {
            window,
            generation,
            width: first.width,
            height: first.height,
            source_width: first.source_width,
            source_height: first.source_height,
            codec: capability.codec,
            encoder: capability.encoder.clone().unwrap_or_default(),
            hardware: true,
            cpu_readback: false,
            fps: rate.load(Ordering::Relaxed),
            fec: media::FecProfile::default(),
            target_bitrate: tuning.bitrate.load(Ordering::Relaxed),
        }))
        .await?;
        tokio::time::timeout(
            Duration::from_secs(10),
            ready.wait_for(|g| *g == generation),
        )
        .await??;
        let mut pending = Some(first);
        let mut number = 0;
        let mut native_next = None;
        let mut awaiting_keyframe = true;
        let mut latest = None;
        let mut status = tokio::time::interval(Duration::from_millis(50));
        status.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        first = loop {
            tokio::select! {
                _ = connection.closed() => anyhow::bail!("media connection closed"),
                changed = reset.changed() => { changed?; if *reset.borrow_and_update() == generation { awaiting_keyframe = true; let _ = capture.control.try_send(frames_native::NativeEncoderCommand::Keyframe); } },
                _ = status.tick() => {
                    if let Some(number) = latest { out.send(Message::MediaStatus { window, generation, number }).await?; }
                    let budget = tuning.bitrate.load(Ordering::Relaxed); let fps = rate.load(Ordering::Relaxed); let scale = tuning.scale.load(Ordering::Relaxed);
                    sender.set_budget(window, budget, tuning.fec.load(Ordering::Relaxed) as u8, tuning.focused.load(Ordering::Relaxed));
                    let bitrate = sender.encoder_target(window, budget);
                    let _ = capture.control.try_send(frames_native::NativeEncoderCommand::Configure { bitrate, fps, scale });
                    if awaiting_keyframe { let _ = capture.control.try_send(frames_native::NativeEncoderCommand::Keyframe); }
                },
                frame = async { if let Some(frame) = pending.take() { Some(frame) } else { capture.packets.recv().await } } => {
                    let frame = frame.ok_or_else(|| anyhow::anyhow!("native encoder stopped"))?;
                    if (frame.width, frame.height) != size { break frame; }
                    if native_next.is_some_and(|next| next != frame.packet.number) { awaiting_keyframe = true; let _ = capture.control.try_send(frames_native::NativeEncoderCommand::Keyframe); }
                    native_next = Some(frame.packet.number + 1);
                    if awaiting_keyframe && !frame.packet.keyframe { continue; } awaiting_keyframe = false;
                    let profile = media::FecProfile { parity_per_32: if std::env::var("FRAMES_FEC").as_deref() == Ok("0") { 0 } else if frame.packet.keyframe { 8 } else { tuning.fec.load(Ordering::Relaxed) as u8 } };
                    let captured_us = media::timestamp_micros(frame.packet.captured_at);
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(if frame.packet.keyframe { 50 } else { 25 });
                    let encoded = EncodedFrame { encoded_us: media::timestamp_micros(frame.packet.encoded_at), captured_us: 0, window, generation, number, keyframe: frame.packet.keyframe, bytes: frame.packet.bytes };
                    latest = Some(number); number += 1;
                    if !sender.send(encoded, connection.max_datagram_size().unwrap_or(1200), profile, captured_us, deadline, tuning.focused.load(Ordering::Relaxed)).await? {
                        awaiting_keyframe = true; let _ = capture.control.try_send(frames_native::NativeEncoderCommand::Keyframe);
                        let q = tuning.controller.lock().unwrap().congestion(std::time::Instant::now());
                        tuning.bitrate.store(q.bitrate, Ordering::Relaxed); tuning.scale.store(q.scale_percent, Ordering::Relaxed); rate.store(q.fps, Ordering::Relaxed);
                    }
                }
            }
        };
        out.send(Message::StreamStopped { window, generation })
            .await?;
    }
}
