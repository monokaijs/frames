//! The viewer's half of the input lane, shared by the Windows and macOS
//! viewers: the newest pointer position goes out as a datagram, everything
//! else on the reliable input stream, in order.

use crate::stats::Samples;
use bytes::Bytes;
use frames_input::{Event, Pointer};
use frames_media::api::{MediaClock, MediaTransport};
use frames_media::clock::SystemClock;

/// Input a viewer window received, stamped with the engine clock at the
/// event's own time.
#[derive(Clone, Copy, Debug)]
pub enum Captured {
    Move { x: u16, y: u16, at_us: u64 },
    Event { event: Event, at_us: u64 },
}

pub struct InputLane {
    tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pointer_seq: u32,
    /// Event time → handed to the transport, µs.
    pub wire: Samples,
    pub sent_moves: u64,
    pub sent_events: u64,
}

impl InputLane {
    /// Opens the input stream; a task writes what [`Self::send`] queues.
    pub async fn open(
        session: &frames_net::Session,
    ) -> anyhow::Result<(Self, tokio::task::JoinHandle<()>)> {
        let (mut stream, _) = session.open_channel().await?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let writer = tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if stream.write_all(&bytes).await.is_err() {
                    break;
                }
            }
            let _ = stream.finish();
        });
        Ok((
            Self {
                tx,
                pointer_seq: 0,
                wire: Samples::new(0),
                sent_moves: 0,
                sent_events: 0,
            },
            writer,
        ))
    }

    pub fn send(
        &mut self,
        media: &frames_net::MediaChannel,
        clock: &SystemClock,
        captured: Vec<Captured>,
    ) {
        if captured.is_empty() {
            return;
        }
        let mut latest = None;
        let mut records = Vec::new();
        let mut stamps = Vec::new();
        for item in captured {
            match item {
                Captured::Move { x, y, at_us } => latest = Some((x, y, at_us)),
                Captured::Event { event, at_us } => {
                    event.encode(&mut records);
                    stamps.push(at_us);
                    self.sent_events += 1;
                }
            }
        }
        if !records.is_empty() && self.tx.send(records).is_ok() {
            let now = clock.now_us();
            for at in stamps {
                self.wire.push(now.saturating_sub(at));
            }
        }
        if let Some((x, y, at_us)) = latest {
            self.pointer_seq = self.pointer_seq.wrapping_add(1);
            let pointer = Pointer {
                seq: self.pointer_seq,
                time_us: at_us as u32,
                x,
                y,
            };
            if media
                .try_send(Bytes::copy_from_slice(&pointer.encode()))
                .is_ok()
            {
                self.wire.push(clock.now_us().saturating_sub(at_us));
                self.sent_moves += 1;
            }
        }
    }

    /// Asks the host to let go of everything held (the viewer is leaving).
    pub fn release_all(&self) {
        let mut bytes = Vec::new();
        Event::ReleaseAll.encode(&mut bytes);
        let _ = self.tx.send(bytes);
    }

    /// Prints this second's input-to-wire latency, if any input went out.
    pub fn report_window(&mut self) {
        let (inputs, p50, p95) = self.wire.take_window();
        if inputs > 0 {
            println!(
                "client input: {inputs} sent, input to wire p50 {:.0} p95 {:.0} µs",
                p50 * 1e3,
                p95 * 1e3
            );
        }
    }

    pub fn print_summary(&mut self, cursor_updates: u64) {
        let (inputs, p50, p95, _) = self.wire.summary();
        println!(
            "INPUT moves={} events={} cursor_updates={cursor_updates} measured={inputs} input_to_wire p50={:.0} p95={:.0} us",
            self.sent_moves,
            self.sent_events,
            p50 * 1e3,
            p95 * 1e3
        );
    }
}
