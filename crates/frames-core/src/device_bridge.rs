//! Opt-in ADB server forwarding, not USB device emulation.
//! Only loopback endpoints are reachable; the paired QUIC session supplies trust.
use anyhow::Context;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

pub const HOST_ADB_PORT: u16 = 15037;
pub const LOCAL_ADB_PORT: u16 = 5037;
const MAGIC: &[u8; 8] = b"FADB0001";
const MAX_CONNECTIONS: usize = 8;

pub struct HostBridge {
    pub address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for HostBridge {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl HostBridge {
    pub async fn shutdown(&mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
    pub async fn start(connection: quinn::Connection) -> anyhow::Result<Self> {
        Self::start_on(connection, HOST_ADB_PORT).await
    }
    async fn start_on(connection: quinn::Connection, port: u16) -> anyhow::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .context("ADB bridge port is already in use or unavailable")?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            let mut jobs = JoinSet::new();
            loop {
                tokio::select! {
                    _=connection.closed()=>break,
                    _=jobs.join_next(),if !jobs.is_empty()=>{},
                    incoming=listener.accept()=>{
                        let Ok((socket,_))=incoming else{break;};
                        if jobs.len()>=MAX_CONNECTIONS{drop(socket);continue;}
                        let connection=connection.clone();
                        jobs.spawn(async move{
                            let result=async{
                                let(mut send,mut receive)=connection.open_bi().await?;
                                send.set_priority(-5)?;
                                send.write_all(MAGIC).await?;
                                let status=tokio::time::timeout(std::time::Duration::from_secs(5),receive.read_u8()).await??;
                                anyhow::ensure!(status==0,"client ADB server is unavailable");
                                forward(send,receive,socket).await
                            }.await;
                            if let Err(error)=result{tracing::debug!(%error,"ADB forwarding connection ended");}
                        });
                    }
                }
            }
            // JoinSet aborts every connection when its listener/session is dropped.
        });
        Ok(Self { address, task })
    }
}

pub async fn probe_local_adb() -> anyhow::Result<()> {
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        TcpStream::connect((Ipv4Addr::LOCALHOST, LOCAL_ADB_PORT)),
    )
    .await?
    .context("Start the local Android ADB server first (adb start-server)")?;
    Ok(())
}

pub async fn serve_local(
    send: quinn::SendStream,
    receive: quinn::RecvStream,
) -> anyhow::Result<()> {
    serve_at(
        send,
        receive,
        SocketAddr::from((Ipv4Addr::LOCALHOST, LOCAL_ADB_PORT)),
    )
    .await
}
async fn serve_at(
    mut send: quinn::SendStream,
    mut receive: quinn::RecvStream,
    address: SocketAddr,
) -> anyhow::Result<()> {
    let mut magic = [0; 8];
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        receive.read_exact(&mut magic),
    )
    .await??;
    anyhow::ensure!(&magic == MAGIC, "unknown device bridge stream");
    let socket = match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        TcpStream::connect(address),
    )
    .await
    {
        Ok(Ok(socket)) => socket,
        _ => {
            let _ = send.write_all(&[1]).await;
            anyhow::bail!("local ADB server unavailable");
        }
    };
    send.set_priority(-5)?;
    send.write_all(&[0]).await?;
    forward(send, receive, socket).await
}
async fn forward(
    mut send: quinn::SendStream,
    mut receive: quinn::RecvStream,
    mut socket: TcpStream,
) -> anyhow::Result<()> {
    socket.set_nodelay(true)?;
    let (read, write) = socket.split();
    let upload = async {
        tokio::io::copy(
            &mut tokio::io::BufReader::with_capacity(16384, read),
            &mut send,
        )
        .await?;
        send.finish()?;
        Ok::<_, anyhow::Error>(())
    };
    let download = async {
        let mut write = write;
        tokio::io::copy(&mut receive, &mut write).await?;
        write.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    };
    tokio::try_join!(upload, download)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        transport,
        trust::{Identity, fingerprint},
    };
    fn identity() -> Identity {
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["frames.local".into()]).unwrap();
        let certificate = cert.der().clone();
        let device_id = fingerprint(&certificate);
        Identity {
            certificate,
            key: rustls::pki_types::PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
            device_id,
        }
    }
    #[tokio::test]
    #[ignore = "requires adb on PATH; uses a fake ADB server and never accesses a physical device"]
    async fn installed_adb_client_lists_a_fixture_through_quic() {
        tokio::time::timeout(std::time::Duration::from_secs(15),async{
            let fake=TcpListener::bind((Ipv4Addr::LOCALHOST,0)).await.unwrap();let address=fake.local_addr().unwrap();
            let fake_task=tokio::spawn(async move{loop{
                let Ok((mut socket,_))=fake.accept().await else{break;};
                let mut size=[0;4];if socket.read_exact(&mut size).await.is_err(){continue;}
                let size=usize::from_str_radix(std::str::from_utf8(&size).unwrap(),16).unwrap();assert!(size<1024);
                let mut request=vec![0;size];socket.read_exact(&mut request).await.unwrap();
                let value=match request.as_slice(){b"host:version"=>b"0029".as_slice(),b"host:devices"=>b"FramesFixture\tdevice\n".as_slice(),b"host:devices-l"=>b"FramesFixture device product:fixture model:fixture transport_id:1\n".as_slice(),other=>panic!("unexpected fixture ADB request: {}",String::from_utf8_lossy(other))};
                socket.write_all(format!("OKAY{:04x}",value.len()).as_bytes()).await.unwrap();socket.write_all(value).await.unwrap();
            }});
            let server=transport::server(&identity(),"127.0.0.1:0".parse().unwrap()).unwrap();let endpoint=transport::client(&identity()).unwrap();
            let connecting=endpoint.connect(server.local_addr().unwrap(),"frames.local").unwrap();
            let(host,client)=tokio::join!(async{server.accept().await.unwrap().await.unwrap()},connecting);let client=client.unwrap();let conn=client.clone();
            let local=tokio::spawn(async move{let mut jobs=JoinSet::new();loop{tokio::select!{
                stream=conn.accept_bi()=>{let Ok((send,receive))=stream else{break;};jobs.spawn(serve_at(send,receive,address));},
                _=jobs.join_next(),if !jobs.is_empty()=>{},
            }}});
            let bridge=HostBridge::start_on(host,0).await.unwrap();
            let mut command=tokio::process::Command::new(std::env::var_os("FRAMES_ADB").unwrap_or_else(||"adb".into()));
            command.args(["-H","127.0.0.1","-P",&bridge.address.port().to_string(),"devices"]).kill_on_drop(true);
            #[cfg(windows)]command.creation_flags(0x08000000);
            let output=command.output().await.unwrap();assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stdout).contains("FramesFixture\tdevice"));
            drop(bridge);client.close(0u32.into(),b"fixture done");local.abort();fake_task.abort();
        }).await.unwrap();
    }
    #[tokio::test]
    async fn adb_requests_roundtrip_and_detaching_closes_the_listener() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let adb = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let adb_address = adb.local_addr().unwrap();
            let fake_adb = tokio::spawn(async move {
                let (mut socket, _) = adb.accept().await.unwrap();
                let mut request = [0; 16];
                socket.read_exact(&mut request).await.unwrap();
                assert_eq!(&request, b"000chost:version");
                socket.write_all(b"OKAY00040029").await.unwrap();
            });
            let server = transport::server(&identity(), "127.0.0.1:0".parse().unwrap()).unwrap();
            let endpoint = transport::client(&identity()).unwrap();
            let connecting = endpoint
                .connect(server.local_addr().unwrap(), "frames.local")
                .unwrap();
            let (host, client) = tokio::join!(
                async { server.accept().await.unwrap().await.unwrap() },
                connecting
            );
            let client = client.unwrap();
            let bridge = HostBridge::start_on(host, 0).await.unwrap();
            let port = bridge.address;
            assert!(port.ip().is_loopback());
            let client_task = tokio::spawn(async move {
                let (send, receive) = client.accept_bi().await.unwrap();
                serve_at(send, receive, adb_address).await.unwrap();
                client
            });
            let mut tool = TcpStream::connect(port).await.unwrap();
            tool.write_all(b"000chost:version").await.unwrap();
            tool.shutdown().await.unwrap();
            let mut result = Vec::new();
            tool.read_to_end(&mut result).await.unwrap();
            assert_eq!(result, b"OKAY00040029");
            fake_adb.await.unwrap();
            let _client = client_task.await.unwrap();
            drop(bridge);
            for _ in 0..50 {
                if TcpStream::connect(port).await.is_err() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            panic!("ADB listener survived detach");
        })
        .await
        .unwrap();
    }
}
