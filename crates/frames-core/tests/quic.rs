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
        assert!(matches!(
            protocol::recv_json(&mut rx).await.unwrap(),
            Message::SelectCodec {
                codec: None,
                bandwidth_mbps: 50,
                quality: frames_core::adaptive::QualityPreset::High
            }
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
    protocol::send_json(
        &mut tx,
        &Message::SelectCodec {
            codec: None,
            bandwidth_mbps: 50,
            quality: frames_core::adaptive::QualityPreset::High,
        },
    )
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

#[tokio::test]
async fn gamepad_tap_and_rumble_cross_quic_while_control_is_blocked() {
    use frames_core::devices::{DeviceMessage, GamepadState};
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
    let endpoint = transport::client(&identity()).unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let host = tokio::spawn(async move {
        let connection = server.accept().await.unwrap().await.unwrap();
        let _blocked = connection.accept_bi().await.unwrap();
        let mut input = connection.accept_uni().await.unwrap();
        assert_eq!(input.read_u8().await.unwrap(), b'G');
        for expected in [0x1000, 0] {
            assert!(
                matches!(protocol::recv_device(&mut input).await.unwrap(),DeviceMessage::GamepadState { state,.. } if state.buttons==expected)
            );
        }
        let mut output = connection.open_uni().await.unwrap();
        output.set_priority(10).unwrap();
        output.write_all(b"G").await.unwrap();
        protocol::send_json(
            &mut output,
            &Message::Device(DeviceMessage::Rumble {
                elapsed_ms: 0,
                slot: 0,
                generation: 1,
                low: 257,
                high: 514,
            }),
        )
        .await
        .unwrap();
        tx.send(()).unwrap();
        connection.closed().await;
    });
    let connection = endpoint
        .connect(address, "frames.local")
        .unwrap()
        .await
        .unwrap();
    let (mut control, _) = connection.open_bi().await.unwrap();
    let finished = Arc::new(AtomicBool::new(false));
    let flag = finished.clone();
    let blocked = tokio::spawn(async move {
        let _ = protocol::send_json(
            &mut control,
            &Message::Clipboard {
                origin: "fixture".into(),
                revision: 1,
                text: Some("x".repeat(3 * 1024 * 1024)),
                png_base64: None,
            },
        )
        .await;
        flag.store(true, Ordering::Release);
    });
    let mut input = connection.open_uni().await.unwrap();
    input.set_priority(10).unwrap();
    input.write_all(b"G").await.unwrap();
    for (sequence, buttons) in [(1, 0x1000), (2, 0)] {
        protocol::send_json(
            &mut input,
            &Message::Device(DeviceMessage::GamepadState {
                slot: 0,
                generation: 1,
                sequence,
                elapsed_ms: 0,
                state: GamepadState {
                    buttons,
                    ..Default::default()
                },
            }),
        )
        .await
        .unwrap();
    }
    tokio::time::timeout(Duration::from_secs(2), rx)
        .await
        .expect("gamepad reports blocked behind clipboard")
        .unwrap();
    let mut feedback = tokio::time::timeout(Duration::from_secs(2), connection.accept_uni())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(feedback.read_u8().await.unwrap(), b'G');
    assert!(matches!(
        protocol::recv_device(&mut feedback).await.unwrap(),
        DeviceMessage::Rumble {
            low: 257,
            high: 514,
            ..
        }
    ));
    assert!(
        !finished.load(Ordering::Acquire),
        "control fixture did not block"
    );
    blocked.abort();
    connection.close(0u32.into(), b"test complete");
    host.await.unwrap();
}

#[tokio::test]
async fn device_messages_have_a_small_independent_size_limit() {
    use tokio::io::AsyncWriteExt;
    let (mut tx, mut rx) = tokio::io::duplex(64);
    tx.write_u32((protocol::MAX_DEVICE_BYTES + 1) as u32)
        .await
        .unwrap();
    assert!(
        protocol::recv_device(&mut rx)
            .await
            .unwrap_err()
            .to_string()
            .contains("length")
    );
}
