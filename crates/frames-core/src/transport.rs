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

fn transport() -> Arc<quinn::TransportConfig> {
    let mut config = quinn::TransportConfig::default();
    config.keep_alive_interval(Some(Duration::from_secs(5)));
    config.max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap()));
    config.datagram_receive_buffer_size(Some(8 * 1024 * 1024));
    config.datagram_send_buffer_size(1024 * 1024);
    config.max_concurrent_bidi_streams(16u32.into());
    config.max_concurrent_uni_streams(32u32.into());
    Arc::new(config)
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
    Ok(quinn::Endpoint::server(config, address)?)
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
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
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
