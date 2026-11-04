//! The viewer's streams after its hello: each starts with one byte saying
//! what it carries, so the host dispatches on that instead of relying on the
//! order streams arrive in (which broke when only one side had a service on).

pub const INPUT: u8 = 1;
pub const CLIPBOARD: u8 = 2;
pub const FILE: u8 = 3;
/// The viewer asks for a file from the host's home folder.
pub const FETCH: u8 = 4;

pub type Streams = (iroh::endpoint::SendStream, iroh::endpoint::RecvStream);

/// Opens a stream of `kind`.
pub async fn open(session: &frames_net::Session, kind: u8) -> anyhow::Result<Streams> {
    let (mut send, recv) = session.open_channel().await?;
    send.write_all(&[kind]).await?;
    Ok((send, recv))
}

/// Where the host sends each kind of stream.
pub struct Routes {
    pub input: Option<tokio::sync::mpsc::UnboundedSender<Streams>>,
    pub clipboard: Option<std::sync::mpsc::Sender<Streams>>,
    pub file: Option<tokio::sync::mpsc::UnboundedSender<Streams>>,
    pub fetch: Option<tokio::sync::mpsc::UnboundedSender<Streams>>,
}

/// Host: accepts the viewer's streams until the session ends and hands each
/// to its service; streams of a kind no service takes are dropped.
pub async fn route(session: frames_net::Session, routes: Routes) {
    loop {
        let Ok((send, mut recv)) = session.accept_channel().await else {
            return;
        };
        let mut kind = [0u8; 1];
        if recv.read_exact(&mut kind).await.is_err() {
            continue;
        }
        let streams = (send, recv);
        match kind[0] {
            INPUT => {
                if let Some(route) = &routes.input {
                    let _ = route.send(streams);
                }
            }
            CLIPBOARD => {
                if let Some(route) = &routes.clipboard {
                    let _ = route.send(streams);
                }
            }
            FILE => {
                if let Some(route) = &routes.file {
                    let _ = route.send(streams);
                }
            }
            FETCH => {
                if let Some(route) = &routes.fetch {
                    let _ = route.send(streams);
                }
            }
            _ => {}
        }
    }
}
