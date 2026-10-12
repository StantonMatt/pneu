use anyhow::{Result, ensure};
use pneu_proto::{Hash, Id, certificate_fingerprint};
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Serialize, Deserialize)]
struct Stored {
    cert: Vec<u8>,
    key: Vec<u8>,
    device_id: Id,
}
#[derive(Debug)]
pub struct Identity {
    pub cert: CertificateDer<'static>,
    pub key: PrivateKeyDer<'static>,
    pub device_id: Id,
    pub fingerprint: Hash,
}
impl Identity {
    pub fn load_or_create(directory: &Path) -> Result<Self> {
        crate::config::private_dir(directory)?;
        let path = directory.join("identity.json");
        let stored: Stored = if path.exists() {
            ensure!(
                !fs::symlink_metadata(&path)?.file_type().is_symlink(),
                "identity is a symlink"
            );
            serde_json::from_slice(&crate::storage::read_bounded(&path, 65536)?)?
        } else {
            let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
            let cert = CertificateParams::new(vec!["pneu.local".into()])?.self_signed(&key)?;
            let stored = Stored {
                cert: cert.der().to_vec(),
                key: key.serialize_der(),
                device_id: Id::new(crate::random_hex(16))?,
            };
            crate::config::atomic_private(&path, &serde_json::to_vec(&stored)?)?;
            stored
        };
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let fingerprint = certificate_fingerprint(&stored.cert);
        Ok(Self {
            cert: stored.cert.into(),
            key: PrivatePkcs8KeyDer::from(stored.key).into(),
            device_id: stored.device_id,
            fingerprint,
        })
    }
}
