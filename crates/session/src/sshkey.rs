//! ssh-ed25519 public keys and the bytes a viewer signs.
//!
//! The host stores the public key. The viewer signs with the matching private
//! key through an SSH agent. Verification uses the public key only.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ring::signature::{UnparsedPublicKey, ED25519};
#[cfg(test)]
use ring::signature::KeyPair;

use crate::SessionError;

pub const AUTH_LABEL: &[u8] = b"deskbridge-auth-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedKey {
    /// OpenSSH wire encoding: the type string followed by the 32-byte key.
    pub blob: Vec<u8>,
    pub comment: String,
}

/// Bytes the viewer signs and the host verifies.
pub fn signing_payload(nonce: &[u8; 32], fingerprint: &[u8; 32], public_key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(AUTH_LABEL.len() + 64 + public_key.len());
    out.extend_from_slice(AUTH_LABEL);
    out.extend_from_slice(nonce);
    out.extend_from_slice(fingerprint);
    out.extend_from_slice(public_key);
    out
}

pub fn parse_public_key_line(line: &str) -> Result<AuthorizedKey, SessionError> {
    let line = line.trim();
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let Some(pos) = tokens.iter().position(|token| *token == "ssh-ed25519") else {
        if let Some(kind) = tokens.iter().copied().find(|token| is_key_type(token)) {
            return Err(SessionError::new(format!(
                "unsupported key type {kind}. Deskbridge authorizes ssh-ed25519 keys"
            )));
        }
        return Err(SessionError::new(
            "an authorized key line starts with ssh-ed25519",
        ));
    };
    let encoded = tokens.get(pos + 1).ok_or_else(|| {
        SessionError::new("ssh-ed25519 line is missing the key data")
    })?;
    let blob = STANDARD
        .decode(encoded)
        .map_err(|_| SessionError::new("ssh-ed25519 key data is not valid base64"))?;
    parse_ed25519_public_blob(&blob)?;
    let comment = tokens[pos + 2..].join(" ");
    Ok(AuthorizedKey { blob, comment })
}

pub fn parse_authorized_file(text: &str) -> Result<Vec<AuthorizedKey>, SessionError> {
    let mut keys = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match parse_public_key_line(trimmed) {
            Ok(key) => keys.push(key),
            Err(err) => {
                return Err(SessionError::new(format!("line {}: {err}", index + 1)));
            }
        }
    }
    Ok(keys)
}

pub fn public_key_line(blob: &[u8], comment: &str) -> Result<String, SessionError> {
    parse_ed25519_public_blob(blob)?;
    let encoded = STANDARD.encode(blob);
    if comment.is_empty() {
        Ok(format!("ssh-ed25519 {encoded}"))
    } else {
        Ok(format!("ssh-ed25519 {encoded} {comment}"))
    }
}

pub fn verify_client_signature(
    public_blob: &[u8],
    payload: &[u8],
    signature_blob: &[u8],
) -> Result<(), SessionError> {
    let raw_public = parse_ed25519_public_blob(public_blob)?;
    let raw_signature = parse_ed25519_signature_blob(signature_blob)?;
    let key = UnparsedPublicKey::new(&ED25519, raw_public.as_ref());
    key.verify(payload, raw_signature.as_ref())
        .map_err(|_| SessionError::new("ssh signature did not verify"))
}

#[cfg(test)]
pub(crate) fn ed25519_public_blob(raw: &[u8]) -> Result<Vec<u8>, SessionError> {
    if raw.len() != 32 {
        return Err(SessionError::new("ssh-ed25519 public key is the wrong length"));
    }
    let mut out = Vec::new();
    push_ssh_string(&mut out, b"ssh-ed25519")?;
    push_ssh_string(&mut out, raw)?;
    Ok(out)
}

#[cfg(test)]
pub(crate) fn ed25519_signature_blob(raw: &[u8]) -> Result<Vec<u8>, SessionError> {
    if raw.len() != 64 {
        return Err(SessionError::new("ssh-ed25519 signature is the wrong length"));
    }
    let mut out = Vec::new();
    push_ssh_string(&mut out, b"ssh-ed25519")?;
    push_ssh_string(&mut out, raw)?;
    Ok(out)
}

pub(crate) fn push_ssh_string(out: &mut Vec<u8>, data: &[u8]) -> Result<(), SessionError> {
    let len = u32::try_from(data.len()).map_err(|_| SessionError::new("ssh field is too long"))?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(data);
    Ok(())
}

pub(crate) fn read_ssh_string<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], SessionError> {
    if input.len() < 4 {
        return Err(SessionError::new("truncated ssh field"));
    }
    let len = u32::from_be_bytes(input[..4].try_into().expect("four bytes")) as usize;
    *input = &input[4..];
    if input.len() < len {
        return Err(SessionError::new("truncated ssh field"));
    }
    let (head, tail) = input.split_at(len);
    *input = tail;
    Ok(head)
}

fn parse_ed25519_public_blob(blob: &[u8]) -> Result<[u8; 32], SessionError> {
    let mut rest = blob;
    let kind = read_ssh_string(&mut rest)?;
    if kind != b"ssh-ed25519" {
        return Err(SessionError::new("ssh public key is not ssh-ed25519"));
    }
    let raw = read_ssh_string(&mut rest)?;
    if !rest.is_empty() {
        return Err(SessionError::new("ssh public key has trailing bytes"));
    }
    let raw: [u8; 32] = raw
        .try_into()
        .map_err(|_| SessionError::new("ssh-ed25519 public key is the wrong length"))?;
    Ok(raw)
}

fn parse_ed25519_signature_blob(blob: &[u8]) -> Result<[u8; 64], SessionError> {
    let mut rest = blob;
    let kind = read_ssh_string(&mut rest)?;
    if kind != b"ssh-ed25519" {
        return Err(SessionError::new("ssh signature is not ssh-ed25519"));
    }
    let raw = read_ssh_string(&mut rest)?;
    if !rest.is_empty() {
        return Err(SessionError::new("ssh signature has trailing bytes"));
    }
    let raw: [u8; 64] = raw
        .try_into()
        .map_err(|_| SessionError::new("ssh-ed25519 signature is the wrong length"))?;
    Ok(raw)
}

fn is_key_type(token: &str) -> bool {
    token.starts_with("ssh-") || token.starts_with("ecdsa-") || token.starts_with("sk-")
}

#[cfg(test)]
pub(crate) struct TestKey {
    pub public_blob: Vec<u8>,
    pub comment: String,
    pair: ring::signature::Ed25519KeyPair,
}

#[cfg(test)]
impl TestKey {
    pub(crate) fn generate(comment: &str) -> Self {
        let document = ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
            .expect("ed25519 key");
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("ed25519 key");
        let public_blob = ed25519_public_blob(pair.public_key().as_ref()).expect("public blob");
        Self {
            public_blob,
            comment: comment.to_string(),
            pair,
        }
    }

    pub(crate) fn line(&self) -> String {
        public_key_line(&self.public_blob, &self.comment).expect("key line")
    }

    pub(crate) fn sign(&self, payload: &[u8]) -> Vec<u8> {
        let raw = self.pair.sign(payload);
        ed25519_signature_blob(raw.as_ref()).expect("signature blob")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_public_key_line_roundtrips() {
        let key = TestKey::generate("laptop");
        let parsed = parse_public_key_line(&key.line()).unwrap();
        assert_eq!(parsed.blob, key.public_blob);
        assert_eq!(parsed.comment, "laptop");
        let payload = signing_payload(&[7; 32], &[9; 32], &key.public_blob);
        let signature = key.sign(&payload);
        verify_client_signature(&key.public_blob, &payload, &signature).unwrap();
        let mut wrong = payload.clone();
        wrong[0] ^= 1;
        assert!(verify_client_signature(&key.public_blob, &wrong, &signature).is_err());
    }

    #[test]
    fn the_sample_line_parses() {
        let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBERERERERERERERERERERERERERERERERERERERERER desk";
        let key = parse_public_key_line(line).unwrap();
        assert_eq!(key.comment, "desk");
        assert_eq!(parse_ed25519_public_blob(&key.blob).unwrap(), [0x11; 32]);
    }

    #[test]
    fn other_key_types_are_rejected() {
        let err = parse_public_key_line("ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQC comment").unwrap_err();
        assert!(err.to_string().contains("ssh-ed25519"), "{err}");
    }

    #[test]
    fn an_authorized_file_skips_comments() {
        let key = TestKey::generate("one");
        let text = format!("# kept aside\n\n{}\n", key.line());
        let keys = parse_authorized_file(&text).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].comment, "one");
    }
}
