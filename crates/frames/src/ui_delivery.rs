//! The session loop never waits for a native UI or a stalled helper control pipe.
use frames_native::{ClientSender, ClientUpdate};
use std::{
    collections::VecDeque,
    sync::{Arc, Condvar, Mutex},
};

#[derive(Default)]
struct State {
    pending: VecDeque<ClientUpdate>,
    stopped: bool,
}
pub struct Delivery {
    state: Arc<(Mutex<State>, Condvar)>,
}
impl Delivery {
    pub fn new(ui: ClientSender) -> Self {
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = state.clone();
        std::thread::spawn(move || {
            loop {
                let update = {
                    let mut state = worker.0.lock().unwrap();
                    while !state.stopped && state.pending.is_empty() {
                        state = worker.1.wait(state).unwrap();
                    }
                    if state.stopped {
                        return;
                    }
                    state.pending.pop_front().unwrap()
                };
                if ui.send(update).is_err() {
                    worker.0.lock().unwrap().stopped = true;
                    return;
                }
            }
        });
        Self { state }
    }
    pub fn send(&self, update: ClientUpdate) -> anyhow::Result<()> {
        let mut state = self.state.0.lock().unwrap();
        anyhow::ensure!(!state.stopped, "UI delivery stopped");
        if let ClientUpdate::SharedCursor(mut cursor) = update {
            if let Some(ClientUpdate::SharedCursor(old)) = state
                .pending
                .iter_mut()
                .find(|u| matches!(u, ClientUpdate::SharedCursor(_)))
            {
                if cursor.png_base64.is_none() {
                    cursor.png_base64 = old.png_base64.take();
                }
                *old = cursor;
            } else {
                anyhow::ensure!(state.pending.len() < 64, "UI control delivery exhausted");
                state.pending.push_back(ClientUpdate::SharedCursor(cursor));
            }
            drop(state);
            self.state.1.notify_one();
            return Ok(());
        }
        if let Some(old) = state.pending.iter_mut().find(|old| replaces(old, &update)) {
            *old = update;
        } else {
            // A failed essential-control admission terminates the session and
            // releases held input. Essential acknowledgements are never evicted.
            anyhow::ensure!(state.pending.len() < 64, "UI control delivery exhausted");
            state.pending.push_back(update);
        }
        drop(state);
        self.state.1.notify_one();
        Ok(())
    }
}
impl Drop for Delivery {
    fn drop(&mut self) {
        self.state.0.lock().unwrap().stopped = true;
        self.state.1.notify_one();
    }
}
fn replaces(old: &ClientUpdate, new: &ClientUpdate) -> bool {
    match (old, new) {
        (ClientUpdate::Windows(_), ClientUpdate::Windows(_))
        | (ClientUpdate::SharedSession(_), ClientUpdate::SharedSession(_))
        | (ClientUpdate::RemoteCursor { .. }, ClientUpdate::RemoteCursor { .. })
        | (ClientUpdate::Peer { .. }, ClientUpdate::Peer { .. })
        | (ClientUpdate::TransferProgress { .. }, ClientUpdate::TransferProgress { .. }) => true,
        (ClientUpdate::Status(a, _), ClientUpdate::Status(b, _))
        | (ClientUpdate::Menu { window: a, .. }, ClientUpdate::Menu { window: b, .. }) => a == b,
        _ => false,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn blocked_ui_does_not_block_latest_snapshots_or_evict_essential_acknowledgements() {
        let (ui, receiver) = frames_native::client_channel(1);
        ui.send(ClientUpdate::RemoteCursor { visible: true })
            .unwrap();
        let delivery = Delivery::new(ui);
        delivery
            .send(ClientUpdate::Peer {
                platform: "fixture".into(),
                device_id: "fixture".into(),
                name: "fixture".into(),
            })
            .unwrap();
        for _ in 0..1000 {
            delivery.send(ClientUpdate::Windows(Vec::new())).unwrap();
        }
        delivery.send(ClientUpdate::Shutdown).unwrap();
        assert!(delivery.state.0.lock().unwrap().pending.len() <= 3);
        let mut shutdown = false;
        for _ in 0..4 {
            if matches!(
                receiver
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap(),
                ClientUpdate::Shutdown
            ) {
                shutdown = true;
                break;
            }
        }
        assert!(shutdown);
    }
}
