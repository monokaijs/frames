use frames_core::{
    protocol::{self, Message},
    transport,
    trust::{Identity, fingerprint},
};
use rustls::pki_types::PrivatePkcs8KeyDer;

fn identity() -> Identity {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["frames.local".into()]).unwrap();
    let certificate = cert.der().clone();
    let device_id = fingerprint(&certificate);
    Identity {
        certificate,
        key: PrivatePkcs8KeyDer::from(signing_key.serialize_der()).into(),
        device_id,
    }
}
#[tokio::test]
async fn mutual_tls_binds_both_identities_and_pairing_code_to_the_connection() {
    let host = identity();
    let client = identity();
    let expected_host = host.device_id.clone();
    let expected_client = client.device_id.clone();
    let server = transport::server(&host, "127.0.0.1:0".parse().unwrap()).unwrap();
    let address = server.local_addr().unwrap();
    let client_endpoint = transport::client(&client).unwrap();
    let host_task = tokio::spawn(async move {
        let connection = server.accept().await.unwrap().await.unwrap();
        assert_eq!(transport::peer_id(&connection).unwrap(), expected_client);
        let code = transport::verification_code(&connection).unwrap();
        let (mut tx, mut rx) = connection.accept_bi().await.unwrap();
        assert!(matches!(
            protocol::recv_json(&mut rx).await.unwrap(),
            Message::Ping { nonce: 42 }
        ));
        protocol::send_json(&mut tx, &Message::Pong { nonce: 42 })
            .await
            .unwrap();
        let _ = connection.closed().await;
        code
    });
    let connection = client_endpoint
        .connect(address, "frames.local")
        .unwrap()
        .await
        .unwrap();
    assert_eq!(transport::peer_id(&connection).unwrap(), expected_host);
    let code = transport::verification_code(&connection).unwrap();
    let (mut tx, mut rx) = connection.open_bi().await.unwrap();
    protocol::send_json(&mut tx, &Message::Ping { nonce: 42 })
        .await
        .unwrap();
    assert!(matches!(
        protocol::recv_json(&mut rx).await.unwrap(),
        Message::Pong { nonce: 42 }
    ));
    connection.close(0u32.into(), b"test complete");
    assert_eq!(host_task.await.unwrap(), code);
}

#[tokio::test]
async fn ordered_input_crosses_quic_while_a_large_control_stream_is_flow_blocked() {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };
    use tokio::io::AsyncReadExt;
    let server = transport::server(&identity(), "127.0.0.1:0".parse().unwrap()).unwrap();
    let address = server.local_addr().unwrap();
    let client = transport::client(&identity()).unwrap();
    let (passed, received) = tokio::sync::oneshot::channel();
    let host = tokio::spawn(async move {
        let connection = server.accept().await.unwrap().await.unwrap();
        let _blocked_control = connection.accept_bi().await.unwrap();
        let mut input = connection.accept_uni().await.unwrap();
        assert_eq!(input.read_u8().await.unwrap(), b'I');
        assert!(matches!(
            protocol::recv_json(&mut input).await.unwrap(),
            Message::InputClock { elapsed_ms: 1 }
        ));
        assert!(matches!(
            protocol::recv_json(&mut input).await.unwrap(),
            Message::ReleaseInput
        ));
        passed.send(()).unwrap();
        connection.closed().await;
    });
    let connection = client
        .connect(address, "frames.local")
        .unwrap()
        .await
        .unwrap();
    let (mut control, _reader) = connection.open_bi().await.unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let flag = finished.clone();
    let blocked = tokio::spawn(async move {
        protocol::send_json(
            &mut control,
            &Message::Clipboard {
                origin: "fixture".into(),
                revision: 1,
                text: Some("x".repeat(3 * 1024 * 1024)),
                png_base64: None,
            },
        )
        .await
        .unwrap();
        flag.store(true, Ordering::Release);
    });
    let mut input = connection.open_uni().await.unwrap();
    input.set_priority(10).unwrap();
    input.write_all(b"I").await.unwrap();
    protocol::send_json(&mut input, &Message::InputClock { elapsed_ms: 1 })
        .await
        .unwrap();
    protocol::send_json(&mut input, &Message::ReleaseInput)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), received)
        .await
        .expect("input blocked behind clipboard")
        .unwrap();
    assert!(
        !finished.load(Ordering::Acquire),
        "control fixture did not block"
    );
    blocked.abort();
    connection.close(0u32.into(), b"test complete");
    host.await.unwrap();
}
