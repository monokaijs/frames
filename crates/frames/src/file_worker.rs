//! File IO and checksum verification never run inside an interactive session loop.
use crate::session::Outbound;
use frames_core::{
    protocol::Message,
    transfer::{self, TransferMessage},
};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
pub type CompletedFiles = Arc<Mutex<HashMap<String, PathBuf>>>;
pub struct FileWorker {
    commands: mpsc::Sender<TransferMessage>,
    active: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FileWorker {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl FileWorker {
    pub fn new(
        root: PathBuf,
        allowed: bool,
        mut bulk: mpsc::Receiver<Message>,
        out: Outbound,
        ui: Option<frames_native::ClientSender>,
        completed: CompletedFiles,
    ) -> Self {
        let (commands, mut incoming) = mpsc::channel(16);
        let active = Arc::new(Mutex::new(Vec::new()));
        let ids = active.clone();
        let task = tokio::spawn(async move {
            let mut receiver = if allowed {
                match transfer::Receiver::new(root).await {
                    Ok(receiver) => Some(receiver),
                    Err(error) => {
                        let _ = out
                            .send(Message::Error {
                                message: error.to_string(),
                            })
                            .await;
                        return;
                    }
                }
            } else {
                None
            };
            loop {
                let message = tokio::select! { biased;
                    message = incoming.recv() => { let Some(message) = message else { break; }; message },
                    message = bulk.recv() => { let Some(Message::Transfer(message)) = message else { break; }; message },
                };
                let Some(receiver) = receiver.as_mut() else {
                    let _ = out
                        .send(Message::Error {
                            message: "File transfers require a user-session host".into(),
                        })
                        .await;
                    continue;
                };
                let id = match &message {
                    TransferMessage::Offer { id, .. }
                    | TransferMessage::Chunk { id, .. }
                    | TransferMessage::Complete { id }
                    | TransferMessage::Cancel { id, .. } => Some(id.clone()),
                    _ => None,
                };
                let result = crate::session::receive_transfer(receiver, message).await;
                *ids.lock().unwrap() = receiver.active_ids();
                match result {
                    Ok(reply) => {
                        if let Some(reply) = reply {
                            if let TransferMessage::Finished { id, name } = &reply
                                && let Ok(path) = receiver.received_path(name)
                            {
                                completed.lock().unwrap().insert(id.clone(), path.clone());
                                if let Some(ui) = ui.clone() {
                                    let _ = tokio::task::spawn_blocking(move || {
                                        ui.send(frames_native::ClientUpdate::ReceivedFile { path })
                                    })
                                    .await;
                                }
                            }
                            if out.send(Message::Transfer(reply)).await.is_err() {
                                break;
                            }
                        }
                        if let Some(id) = id
                            && let Some((name, done, total)) = receiver.progress(&id)
                            && let Some(ui) = &ui
                        {
                            let _ = ui.try_send(frames_native::ClientUpdate::TransferProgress {
                                name,
                                done,
                                total,
                            });
                        }
                    }
                    Err(error) => {
                        let _ = out
                            .send(Message::Error {
                                message: format!("File transfer: {error}"),
                            })
                            .await;
                    }
                }
            }
        });
        Self {
            commands,
            active,
            task,
        }
    }
    pub fn submit(&self, message: TransferMessage) -> anyhow::Result<()> {
        self.commands
            .try_send(message)
            .map_err(|_| anyhow::anyhow!("file control queue full or stopped"))
    }
    pub fn active_ids(&self) -> Vec<String> {
        self.active.lock().unwrap().clone()
    }
    pub async fn cancel(&self, id: &str) -> anyhow::Result<()> {
        self.submit(TransferMessage::Cancel {
            id: id.into(),
            reason: "canceled locally".into(),
        })
    }
}
