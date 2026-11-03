use crate::{
    ALPN,
    trust::{Identity, fingerprint},
};
use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use std::{net::SocketAddr, sync::Arc, time::Duration};

/// TLS proves possession during pairing; application authorization is mandatory
/// after the handshake and before any window metadata, input, or media is sent.
#[derive(Debug)]
struct PairingVerifier {
    provider: Arc<rustls::crypto::CryptoProvider>,
}
impl ServerCertVerifier for PairingVerifier {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
impl ClientCertVerifier for PairingVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        ServerCertVerifier::verify_tls12_signature(self, message, cert, signature)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        ServerCertVerifier::verify_tls13_signature(self, message, cert, signature)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        ServerCertVerifier::supported_verify_schemes(self)
    }
}

/// Congestion controller for all session traffic. Cubic is quinn's default;
/// `FRAMES_CONGESTION=bbr|newreno` selects another for measurement. In the
/// netsim benchmark quinn's BBR sustained more frames under random loss but
/// overshot shallow bottleneck queues, so it is not the default.
fn congestion_controller() -> Arc<dyn quinn::congestion::ControllerFactory + Send + Sync> {
    match std::env::var("FRAMES_CONGESTION")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "bbr" => Arc::new(quinn::congestion::BbrConfig::default()),
        "newreno" => Arc::new(quinn::congestion::NewRenoConfig::default()),
        _ => Arc::new(quinn::congestion::CubicConfig::default()),
    }
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    config.congestion_controller_factory(congestion_controller());
    config.keep_alive_interval(Some(Duration::from_secs(5)));
    config.max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap()));
    config.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    config.datagram_send_buffer_size(crate::media::DATAGRAM_SEND_BUFFER_BYTES);
    config.max_concurrent_bidi_streams(16u32.into());
    config.max_concurrent_uni_streams(32u32.into());
    Arc::new(config)
}

/// Kernel buffer for media sockets. A single high-bitrate picture arrives as a
/// burst of 100+ datagrams at LAN speed; the OS default (64 KB on Windows)
/// overflows before the receive task runs, and those drops look like network
/// loss to QUIC: reduced congestion window, repaired or lost pictures.
const UDP_BUFFER_BYTES: [usize; 4] = [8 << 20, 4 << 20, 2 << 20, 1 << 20];
fn udp_socket(address: SocketAddr) -> anyhow::Result<std::net::UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(address),
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    // Best effort: platforms cap the size (macOS at kern.ipc.maxsockbuf).
    for bytes in UDP_BUFFER_BYTES {
        if socket.set_recv_buffer_size(bytes).is_ok() {
            break;
        }
    }
    for bytes in UDP_BUFFER_BYTES {
        if socket.set_send_buffer_size(bytes).is_ok() {
            break;
        }
    }
    socket.bind(&address.into())?;
    Ok(socket.into())
}
fn endpoint(
    address: SocketAddr,
    server: Option<quinn::ServerConfig>,
) -> anyhow::Result<quinn::Endpoint> {
    Ok(quinn::Endpoint::new(
        quinn::EndpointConfig::default(),
        server,
        udp_socket(address)?,
        Arc::new(quinn::TokioRuntime),
    )?)
}

pub fn server(identity: &Identity, address: SocketAddr) -> anyhow::Result<quinn::Endpoint> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(PairingVerifier { provider }))
        .with_single_cert(vec![identity.certificate.clone()], identity.key.clone_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls)?,
    ));
    config.transport_config(transport());
    endpoint(address, Some(config))
}

pub fn client(identity: &Identity) -> anyhow::Result<quinn::Endpoint> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PairingVerifier { provider }))
        .with_client_auth_cert(vec![identity.certificate.clone()], identity.key.clone_key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
    ));
    config.transport_config(transport());
    let mut endpoint = endpoint("0.0.0.0:0".parse()?, None)?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

pub fn peer_id(connection: &quinn::Connection) -> anyhow::Result<String> {
    let identity = connection
        .peer_identity()
        .ok_or_else(|| anyhow::anyhow!("peer supplied no TLS identity"))?;
    let certificates = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| anyhow::anyhow!("unexpected TLS identity type"))?;
    let cert = certificates
        .first()
        .ok_or_else(|| anyhow::anyhow!("empty TLS identity"))?;
    Ok(fingerprint(cert))
}

pub fn verification_code(connection: &quinn::Connection) -> anyhow::Result<String> {
    let mut key = [0u8; 8];
    connection
        .export_keying_material(&mut key, b"frames pairing verification", b"v1")
        .map_err(|_| anyhow::anyhow!("TLS exporter unavailable"))?;
    Ok(format!("{:08}", u64::from_be_bytes(key) % 100_000_000))
}
