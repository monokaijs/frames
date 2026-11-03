//! Carrying v1 trust over to v2 device keys.
//!
//! v1 trusts a peer by the SHA-256 fingerprint of its self-signed certificate
//! (`frames_core::trust::fingerprint`). After a v2 handshake each side sends
//! a [`LegacyProof`]: its v1 certificate and a P-256 signature, made with the
//! v1 key, over its v2 device id and this connection's TLS exporter. The
//! verifier checks the signature against the certificate and looks the
//! certificate's fingerprint up in its v1 trust store; a match re-keys that
//! entry under the v2 id. Binding the exporter makes the proof useless on any
//! other connection.

use anyhow::{Context, Result, ensure};
use iroh::EndpointId;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair};
use sha2::{Digest, Sha256};

const DOMAIN: &[u8] = b"frames-migrate-v1";
pub const EXPORTER_LABEL: &[u8] = b"frames-migrate";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegacyProof {
    pub certificate: Vec<u8>,
    pub signature: Vec<u8>,
}

fn message(device: &EndpointId, exporter: &[u8; 32]) -> Vec<u8> {
    let mut message = Vec::with_capacity(DOMAIN.len() + 64);
    message.extend_from_slice(DOMAIN);
    message.extend_from_slice(device.as_bytes());
    message.extend_from_slice(exporter);
    message
}

/// The v1 fingerprint of a certificate: hex SHA-256 of its DER.
pub fn fingerprint(certificate: &[u8]) -> String {
    hex::encode(Sha256::digest(certificate))
}

/// Signs `device` with the v1 key for this connection.
pub fn prove(
    certificate: &[u8],
    pkcs8: &[u8],
    device: &EndpointId,
    exporter: &[u8; 32],
) -> Result<LegacyProof> {
    let rng = SystemRandom::new();
    let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8, &rng)
        .ok()
        .context("the v1 key is not a P-256 PKCS#8 key")?;
    let signature = key
        .sign(&rng, &message(device, exporter))
        .ok()
        .context("signing failed")?;
    Ok(LegacyProof {
        certificate: certificate.to_vec(),
        signature: signature.as_ref().to_vec(),
    })
}

/// Checks a peer's proof and returns its v1 fingerprint.
pub fn verify(proof: &LegacyProof, device: &EndpointId, exporter: &[u8; 32]) -> Result<String> {
    ensure!(
        proof.certificate.len() <= 16 * 1024,
        "certificate too large"
    );
    let der = pki_types::CertificateDer::from(proof.certificate.as_slice());
    let certificate = webpki::EndEntityCert::try_from(&der)
        .ok()
        .context("malformed v1 certificate")?;
    certificate
        .verify_signature(
            webpki::ring::ECDSA_P256_SHA256,
            &message(device, exporter),
            &proof.signature,
        )
        .ok()
        .context("the v1 signature does not match this device and connection")?;
    Ok(fingerprint(&proof.certificate))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::DeviceKey;

    fn legacy() -> (Vec<u8>, Vec<u8>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["frames.local".into()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (cert.der().to_vec(), key.serialize_der())
    }

    #[test]
    fn a_proof_verifies_only_for_its_device_and_connection() {
        let (certificate, pkcs8) = legacy();
        let device = DeviceKey::from_legacy_pkcs8(&pkcs8).unwrap().id();
        let exporter = [7u8; 32];
        let proof = prove(&certificate, &pkcs8, &device, &exporter).unwrap();
        assert_eq!(
            verify(&proof, &device, &exporter).unwrap(),
            fingerprint(&certificate)
        );

        assert!(
            verify(&proof, &device, &[8u8; 32]).is_err(),
            "replayed on another connection"
        );
        let other = DeviceKey::generate().id();
        assert!(
            verify(&proof, &other, &exporter).is_err(),
            "claimed by another device"
        );
        let (stranger, _) = legacy();
        let forged = LegacyProof {
            certificate: stranger,
            ..proof.clone()
        };
        assert!(
            verify(&forged, &device, &exporter).is_err(),
            "someone else's certificate"
        );
    }
}
