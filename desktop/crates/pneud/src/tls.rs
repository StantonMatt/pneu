//! Full-certificate pins with P-256/self-signature/validity checks. CertificateVerify
//! is delegated to rustls's ring verifier, never asserted without verification.
use crate::identity::Identity;
use pneu_proto::{Hash, Id, certificate_fingerprint};
use rustls::{
    DigitallySignedStruct, DistinguishedName, Error, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};
use subtle::ConstantTimeEq;

#[derive(Debug)]
pub struct Invitation {
    pub token: Hash,
    pub expires_at: u64,
    pub deadline: Instant,
}
#[derive(Debug, Default)]
pub struct Admission {
    pub peers: HashMap<Hash, Id>,
    pub invitation: Option<Invitation>,
    pub last_unknown: Option<Instant>,
}
#[derive(Debug)]
pub struct ClientVerifier {
    pub admission: Arc<Mutex<Admission>>,
    provisional: AtomicBool,
    slot: Arc<AtomicBool>,
}
impl ClientVerifier {
    pub fn release_provisional(&self) {
        if self.provisional.swap(false, Ordering::SeqCst) {
            self.slot.store(false, Ordering::SeqCst);
        }
    }
    pub fn new(admission: Arc<Mutex<Admission>>, slot: Arc<AtomicBool>) -> Self {
        Self {
            admission,
            provisional: AtomicBool::new(false),
            slot,
        }
    }
}
impl Drop for ClientVerifier {
    fn drop(&mut self) {
        if self.provisional.load(Ordering::SeqCst) {
            self.slot.store(false, Ordering::SeqCst);
        }
    }
}
fn invalid() -> Error {
    Error::General("Pneu certificate authentication failed".into())
}
pub fn validate_certificate(
    cert: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    now: UnixTime,
) -> Result<(), Error> {
    use x509_parser::{parse_x509_certificate, time::ASN1Time};
    if !intermediates.is_empty() {
        return Err(invalid());
    }
    let (rest, parsed) = parse_x509_certificate(cert.as_ref()).map_err(|_| invalid())?;
    let key = parsed.public_key();
    let curve = key
        .algorithm
        .parameters
        .as_ref()
        .and_then(|p| p.as_oid().ok())
        .map(|o| o.to_id_string());
    if !rest.is_empty()
        || key.algorithm.algorithm.to_id_string() != "1.2.840.10045.2.1"
        || curve.as_deref() != Some("1.2.840.10045.3.1.7")
        || parsed.issuer() != parsed.subject()
        || parsed.signature_algorithm.algorithm.to_id_string() != "1.2.840.10045.4.3.2"
    {
        return Err(invalid());
    }
    let time = ASN1Time::from_timestamp(now.as_secs() as i64).map_err(|_| invalid())?;
    if !parsed.validity().is_valid_at(time) {
        return Err(invalid());
    }
    parsed.verify_signature(None).map_err(|_| invalid())?;
    Ok(())
}
fn signature(
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, Error> {
    if dss.scheme != SignatureScheme::ECDSA_NISTP256_SHA256 {
        return Err(invalid());
    }
    rustls::crypto::verify_tls13_signature(
        message,
        cert,
        dss,
        &rustls::crypto::ring::default_provider().signature_verification_algorithms,
    )
}
impl ClientCertVerifier for ClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        validate_certificate(cert, intermediates, now)?;
        let fp = certificate_fingerprint(cert.as_ref());
        let mut state = self.admission.lock().map_err(|_| invalid())?;
        if !state.peers.contains_key(&fp) {
            let instant = Instant::now();
            if !state
                .invitation
                .as_ref()
                .is_some_and(|i| i.expires_at > crate::now_ms() && i.deadline > instant)
                || state
                    .last_unknown
                    .is_some_and(|t| instant.duration_since(t).as_secs() < 1)
            {
                return Err(invalid());
            }
            if self
                .slot
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Err(invalid());
            }
            self.provisional.store(true, Ordering::SeqCst);
            state.last_unknown = Some(instant);
        }
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(invalid())
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ECDSA_NISTP256_SHA256]
    }
}
#[derive(Debug)]
struct PinVerifier(Hash);
impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        validate_certificate(cert, intermediates, now)?;
        if !bool::from(
            certificate_fingerprint(cert.as_ref())
                .as_str()
                .as_bytes()
                .ct_eq(self.0.as_str().as_bytes()),
        ) {
            return Err(invalid());
        }
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(invalid())
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ECDSA_NISTP256_SHA256]
    }
}
pub fn server_config(
    identity: &Identity,
    verifier: Arc<ClientVerifier>,
) -> anyhow::Result<rustls::ServerConfig> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_client_cert_verifier(verifier)
    .with_single_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    config.max_early_data_size = 0;
    Ok(config)
}
pub fn client_config(identity: &Identity, pin: Hash) -> anyhow::Result<rustls::ClientConfig> {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(PinVerifier(pin)))
    .with_client_auth_cert(vec![identity.cert.clone()], identity.key.clone_key())?;
    config.resumption = rustls::client::Resumption::disabled();
    config.enable_early_data = false;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_non_p256_expired_and_broken_self_signature() -> anyhow::Result<()> {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let valid =
            rcgen::CertificateParams::new(vec!["unused.invalid".into()])?.self_signed(&key)?;
        let now =
            UnixTime::since_unix_epoch(std::time::Duration::from_secs(crate::now_ms() / 1000));
        validate_certificate(valid.der(), &[], now)?;
        let p384 = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384)?;
        let other = rcgen::CertificateParams::new(vec![])?.self_signed(&p384)?;
        assert!(validate_certificate(other.der(), &[], now).is_err());
        let mut old = rcgen::CertificateParams::new(vec![])?;
        old.not_after = rcgen::date_time_ymd(2000, 1, 1);
        assert!(validate_certificate(old.self_signed(&key)?.der(), &[], now).is_err());
        let mut broken = valid.der().to_vec();
        *broken.last_mut().unwrap() ^= 1;
        assert!(validate_certificate(&broken.into(), &[], now).is_err());
        assert!(validate_certificate(valid.der(), &[valid.der().clone()], now).is_err());
        Ok(())
    }
}
