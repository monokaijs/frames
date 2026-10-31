//! A cancellable native codec worker. Cancellation wakes idle input/output waits.
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::{sync::watch, task::JoinHandle};
pub struct Worker {
    stop: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
    id: u64,
}
impl Worker {
    pub fn new(stop: watch::Sender<bool>, task: JoinHandle<()>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            stop,
            task: Some(task),
            id: NEXT.fetch_add(1, Ordering::Relaxed),
        }
    }
    pub fn id(&self) -> u64 {
        self.id
    }
    pub async fn shutdown(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}
