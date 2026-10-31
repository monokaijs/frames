//! Bounded UI control queue with replaceable decoded frames and native wakeups.
use crate::ClientUpdate;
use frames_core::{media::RawFrame, model::WindowId};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Condvar, Mutex, mpsc},
    time::{Duration, Instant},
};

enum Pending {
    Control(Box<ClientUpdate>),
    Frame(WindowId),
}
struct State {
    queue: VecDeque<Pending>,
    frames: HashMap<WindowId, RawFrame>,
    controls: usize,
    senders: usize,
    closed: bool,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
    space: Condvar,
    capacity: usize,
}
#[derive(Debug)]
pub enum ClientSendError {
    Full,
    Disconnected,
}
impl std::fmt::Display for ClientSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Full => "client control queue full",
            Self::Disconnected => "client UI disconnected",
        })
    }
}
impl std::error::Error for ClientSendError {}
pub struct ClientSender(Arc<Shared>);
pub struct ClientReceiver(Arc<Shared>);

pub fn client_channel(capacity: usize) -> (ClientSender, ClientReceiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: VecDeque::new(),
            frames: HashMap::new(),
            controls: 0,
            senders: 1,
            closed: false,
            wake: None,
        }),
        ready: Condvar::new(),
        space: Condvar::new(),
        capacity: capacity.max(1),
    });
    (ClientSender(shared.clone()), ClientReceiver(shared))
}
impl Clone for ClientSender {
    fn clone(&self) -> Self {
        self.0.state.lock().unwrap().senders += 1;
        Self(self.0.clone())
    }
}
impl Drop for ClientSender {
    fn drop(&mut self) {
        let wake = {
            let mut state = self.0.state.lock().unwrap();
            state.senders -= 1;
            state.wake.clone()
        };
        self.0.ready.notify_all();
        if let Some(wake) = wake {
            wake();
        }
    }
}
impl ClientSender {
    fn push(&self, update: ClientUpdate, blocking: bool) -> Result<(), ClientSendError> {
        let mut state = self.0.state.lock().unwrap();
        if !matches!(update, ClientUpdate::Frame(..)) {
            while !state.closed && state.controls >= self.0.capacity {
                if !blocking {
                    return Err(ClientSendError::Full);
                }
                state = self.0.space.wait(state).unwrap();
            }
        }
        if state.closed {
            return Err(ClientSendError::Disconnected);
        }
        let empty = state.queue.is_empty();
        match update {
            ClientUpdate::Frame(window, frame) => {
                if !state.frames.contains_key(&window) {
                    if state.frames.len() >= self.0.capacity {
                        let oldest = state
                            .queue
                            .iter()
                            .position(|p| matches!(p, Pending::Frame(_)))
                            .unwrap();
                        if let Some(Pending::Frame(key)) = state.queue.remove(oldest) {
                            state.frames.remove(&key);
                        }
                    }
                    state.queue.push_back(Pending::Frame(window));
                }
                state.frames.insert(window, frame);
            }
            control => {
                state.controls += 1;
                state.queue.push_back(Pending::Control(Box::new(control)));
            }
        }
        let wake = if empty { state.wake.clone() } else { None };
        drop(state);
        self.0.ready.notify_one();
        if let Some(wake) = wake {
            wake();
        }
        Ok(())
    }
    pub fn send(&self, update: ClientUpdate) -> Result<(), ClientSendError> {
        self.push(update, true)
    }
    pub fn try_send(&self, update: ClientUpdate) -> Result<(), ClientSendError> {
        self.push(update, false)
    }
}
impl ClientReceiver {
    pub fn set_waker(&self, wake: impl Fn() + Send + Sync + 'static) {
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(wake);
        let pending = {
            let mut state = self.0.state.lock().unwrap();
            state.wake = Some(wake.clone());
            !state.queue.is_empty()
        };
        if pending {
            wake();
        }
    }
    fn pop(&self, state: &mut State) -> Option<ClientUpdate> {
        match state.queue.pop_front()? {
            Pending::Frame(window) => Some(ClientUpdate::Frame(
                window,
                state.frames.remove(&window).unwrap(),
            )),
            Pending::Control(update) => {
                state.controls -= 1;
                self.0.space.notify_one();
                Some(*update)
            }
        }
    }
    pub fn try_recv(&self) -> Result<ClientUpdate, mpsc::TryRecvError> {
        let mut state = self.0.state.lock().unwrap();
        self.pop(&mut state).ok_or(if state.senders == 0 {
            mpsc::TryRecvError::Disconnected
        } else {
            mpsc::TryRecvError::Empty
        })
    }
    pub fn recv_timeout(&self, timeout: Duration) -> Result<ClientUpdate, mpsc::RecvTimeoutError> {
        let deadline = Instant::now() + timeout;
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some(update) = self.pop(&mut state) {
                return Ok(update);
            }
            if state.senders == 0 {
                return Err(mpsc::RecvTimeoutError::Disconnected);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(mpsc::RecvTimeoutError::Timeout);
            };
            state = self.0.ready.wait_timeout(state, remaining).unwrap().0;
        }
    }
    pub fn try_iter(&self) -> impl Iterator<Item = ClientUpdate> + '_ {
        std::iter::from_fn(|| self.try_recv().ok())
    }
}
impl Iterator for ClientReceiver {
    type Item = ClientUpdate;
    fn next(&mut self) -> Option<ClientUpdate> {
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some(update) = self.pop(&mut state) {
                return Some(update);
            }
            if state.senders == 0 {
                return None;
            }
            state = self.0.ready.wait(state).unwrap();
        }
    }
}
impl Drop for ClientReceiver {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.closed = true;
        state.queue.clear();
        state.frames.clear();
        state.wake = None;
        self.0.space.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn frame(window: u64, value: u8) -> ClientUpdate {
        ClientUpdate::Frame(
            WindowId {
                id: window,
                generation: 1,
            },
            RawFrame {
                width: 1,
                height: 1,
                bgra: vec![value; 4].into(),
            },
        )
    }
    #[test]
    fn stalled_ui_gets_latest_pixels_and_preserves_control_order() {
        let (tx, rx) = client_channel(2);
        tx.send(ClientUpdate::RemoteCursor { visible: true })
            .unwrap();
        for n in 0..=255 {
            tx.try_send(frame(1, n)).unwrap();
        }
        tx.send(ClientUpdate::Shutdown).unwrap();
        assert!(matches!(
            rx.try_recv(),
            Ok(ClientUpdate::RemoteCursor { visible: true })
        ));
        assert!(
            matches!(rx.try_recv(), Ok(ClientUpdate::Frame(_, pixels)) if pixels.bgra[0] == 255)
        );
        assert!(matches!(rx.try_recv(), Ok(ClientUpdate::Shutdown)));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn frame_flood_is_bounded_and_does_not_fill_control_capacity() {
        let (tx, rx) = client_channel(2);
        for window in 0..100 {
            tx.try_send(frame(window, 1)).unwrap();
        }
        tx.try_send(ClientUpdate::Shutdown).unwrap();
        assert_eq!(rx.try_iter().count(), 3);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let wakes = count.clone();
        rx.set_waker(move || {
            wakes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        tx.try_send(frame(1, 1)).unwrap();
        tx.try_send(frame(1, 2)).unwrap();
        assert_eq!(count.load(std::sync::atomic::Ordering::Relaxed), 1);
        drop(rx);
        assert!(tx.send(ClientUpdate::Shutdown).is_err());
    }
}
