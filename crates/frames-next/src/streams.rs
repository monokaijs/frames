//! The viewer's streams after its hello: each starts with one byte saying
//! what it carries, so the host dispatches on that instead of relying on the
//! order streams arrive in (which broke when only one side had a service on).

pub const INPUT: u8 = 1;
pub const CLIPBOARD: u8 = 2;
pub const FILE: u8 = 3;
/// The viewer asks for a file from the host's home folder.
pub const FETCH: u8 = 4;

/// Streams the host opens after the stream description, also tagged.
pub const CURSOR: u8 = 10;
/// One forwarded ADB connection (host 127.0.0.1:15037 → the viewer's
/// ADB server).
pub const ADB: u8 = 11;

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

/// Viewer: accepts the host's streams after its stream description: the
/// cursor stream goes to `cursor`; ADB streams are piped to `adb` (the
/// viewer's ADB server) when the viewer turned the bridge on.
pub async fn route_viewer(
    session: frames_net::Session,
    cursor: tokio::sync::mpsc::UnboundedSender<iroh::endpoint::RecvStream>,
    adb: Option<std::net::SocketAddr>,
) {
    loop {
        let Ok((send, mut recv)) = session.accept_channel().await else {
            return;
        };
        let mut kind = [0u8; 1];
        if recv.read_exact(&mut kind).await.is_err() {
            continue;
        }
        match (kind[0], adb) {
            (CURSOR, _) => {
                let _ = cursor.send(recv);
            }
            (ADB, Some(server)) => {
                tokio::spawn(async move {
                    match tokio::net::TcpStream::connect(server).await {
                        Ok(socket) => pipe(socket, (send, recv)).await,
                        Err(error) => println!("ADB bridge: cannot reach {server}: {error}"),
                    }
                });
            }
            _ => {}
        }
    }
}

/// Copies both ways between a TCP connection and a stream until either
/// side closes.
pub async fn pipe(socket: tokio::net::TcpStream, (mut send, mut recv): Streams) {
    let (mut read, mut write) = socket.into_split();
    let up = async {
        let _ = tokio::io::copy(&mut read, &mut send).await;
        let _ = send.finish();
    };
    let down = async {
        let _ = tokio::io::copy(&mut recv, &mut write).await;
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut write).await;
    };
    tokio::join!(up, down);
}

/// Host: forwards local TCP connections on `listen` to the viewer's ADB
/// server, one stream each. Only this machine can connect (loopback).
pub async fn adb_listener(session: frames_net::Session, listen: std::net::SocketAddr) {
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(listener) => listener,
        Err(error) => {
            println!("ADB bridge: cannot listen on {listen}: {error}");
            return;
        }
    };
    println!(
        "ADB bridge: adb -H {} -P {} devices",
        listen.ip(),
        listen.port()
    );
    while let Ok((socket, _)) = listener.accept().await {
        let session = session.clone();
        tokio::spawn(async move {
            match open(&session, ADB).await {
                Ok(streams) => pipe(socket, streams).await,
                Err(error) => println!("ADB bridge: {error:#}"),
            }
        });
    }
}
