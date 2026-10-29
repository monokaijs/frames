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
