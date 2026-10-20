use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

use crate::SessionError;

/// Host identity for one process lifetime: a pairing code and a self-signed certificate.
pub struct Identity {
    pub code: String,
    pub fingerprint: [u8; 32],
    pub cert_chain: Vec<CertificateDer<'static>>,
    pub key: PrivateKeyDer<'static>,
}

impl Identity {
    pub fn generate() -> Result<Self, SessionError> {
        let key_pair = rcgen::KeyPair::generate().map_err(|err| SessionError::new(err))?;
        let params = rcgen::CertificateParams::new(vec!["deskbridge".to_string()])
            .map_err(|err| SessionError::new(err))?;
        let cert = params
            .self_signed(&key_pair)
            .map_err(|err| SessionError::new(err))?;
        let der = cert.der().to_vec();
        let fingerprint = fingerprint_of(&der);
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        Ok(Self {
            code: pairing_code(),
            fingerprint,
            cert_chain: vec![CertificateDer::from(der)],
            key,
        })
    }

    pub fn fingerprint_hex(&self) -> String {
        hex(&self.fingerprint)
    }
}

pub fn fingerprint_of(der: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(der);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

pub fn parse_fingerprint(text: &str) -> Result<[u8; 32], SessionError> {
    let text = text.trim();
    if text.len() != 64 || !text.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err(SessionError::new(
            "a fingerprint is 64 hex characters (sha256 of the host certificate)",
        ));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(SessionError::new)?;
    }
    Ok(out)
}

fn pairing_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).expect("system random");
    bytes
        .iter()
        .map(|byte| ALPHABET[(*byte as usize) % ALPHABET.len()] as char)
        .collect()
}

/// Compare pairing codes without an early exit on the first mismatch.
pub fn codes_match(presented: &str, expected: &str) -> bool {
    let a = presented.as_bytes();
    let b = expected.as_bytes();
    let mut diff = a.len() ^ b.len();
    let n = a.len().max(b.len());
    for i in 0..n {
        let left = a.get(i).copied().unwrap_or(0);
        let right = b.get(i).copied().unwrap_or(0);
        diff |= (left ^ right) as usize;
    }
    diff == 0
}

pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn server_crypto(identity: &Identity) -> Result<rustls::ServerConfig, SessionError> {
    install_crypto_provider();
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(identity.cert_chain.clone(), identity.key.clone_key())
        .map_err(|err| SessionError::new(err))?;
    config.alpn_protocols = vec![b"deskbridge".to_vec()];
    Ok(config)
}
