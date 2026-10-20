//! OpenSSH agent client. 1Password speaks this protocol on its agent socket.
//!
//! The agent lists public keys and signs with the private key. The private key
//! stays in the agent.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::sshkey::{public_key_line, push_ssh_string, read_ssh_string, signing_payload};
use crate::SessionError;

const AGENT_FAILURE: u8 = 5;
const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;
const MAX_MESSAGE: usize = 256 * 1024;
const MAX_IDENTITIES: u32 = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentIdentity {
    pub blob: Vec<u8>,
    pub comment: String,
}

pub struct SignedKey {
    pub public_key: Vec<u8>,
    pub signature: Vec<u8>,
}

trait AgentIo: Read + Write {}
impl<T: Read + Write> AgentIo for T {}

struct Agent {
    stream: Box<dyn AgentIo + Send>,
}

pub fn agent_available() -> bool {
    !candidate_sockets().is_empty()
}

pub(crate) fn sign_with_agent(
    chosen: Option<&[u8]>,
    nonce: &[u8; 32],
    fingerprint: &[u8; 32],
    allowed: &[Vec<u8>],
) -> Result<Option<SignedKey>, SessionError> {
    let sockets = candidate_sockets();
    if sockets.is_empty() {
        return Err(SessionError::agent_down(
            "could not find an SSH agent. Start the 1Password SSH agent, or set SSH_AUTH_SOCK",
        ));
    }
    let mut located = Vec::new();
    let mut failure = None;
    for socket in &sockets {
        match list_identities(socket) {
            Ok(found) => add_identities(&mut located, socket, found),
            Err(err) => failure = Some(err),
        }
    }
    if located.is_empty() {
        if let Some(err) = failure {
            return Err(err);
        }
    }
    let identities: Vec<AgentIdentity> = located.iter().map(|item| item.identity.clone()).collect();
    let Some(public_key) = select_key(&identities, allowed, chosen)? else {
        return Ok(None);
    };
    let socket = located
        .iter()
        .find(|item| item.identity.blob == public_key)
        .map(|item| item.socket.clone())
        .ok_or_else(|| SessionError::fatal("the SSH agent has no private key for this public key"))?;
    sign_with_agent_at(&socket, Some(public_key.as_slice()), nonce, fingerprint, allowed)
}

pub(crate) fn sign_with_agent_at(
    path: &Path,
    chosen: Option<&[u8]>,
    nonce: &[u8; 32],
    fingerprint: &[u8; 32],
    allowed: &[Vec<u8>],
) -> Result<Option<SignedKey>, SessionError> {
    let mut agent = Agent::connect(path)?;
    let identities = agent.identities()?;
    let Some(public_key) = select_key(&identities, allowed, chosen)? else {
        return Ok(None);
    };
    let payload = signing_payload(nonce, fingerprint, &public_key);
    eprintln!("waiting for the SSH agent to sign. Approve deskbridge in 1Password if it asks.");
    let signature = agent.sign(&public_key, &payload)?;
    Ok(Some(SignedKey { public_key, signature }))
}

pub(crate) fn select_key(
    identities: &[AgentIdentity],
    allowed: &[Vec<u8>],
    chosen: Option<&[u8]>,
) -> Result<Option<Vec<u8>>, SessionError> {
    if allowed.is_empty() {
        if chosen.is_some() {
            return Err(SessionError::fatal("this host has no authorized SSH keys"));
        }
        return Ok(None);
    }
    if let Some(chosen) = chosen {
        if !allowed.iter().any(|key| key == chosen) {
            return Err(SessionError::fatal("the host does not authorize this key"));
        }
        if !identities.iter().any(|identity| identity.blob == chosen) {
            return Err(SessionError::fatal(
                "the SSH agent has no private key for this public key",
            ));
        }
        return Ok(Some(chosen.to_vec()));
    }
    let overlap: Vec<&AgentIdentity> = identities
        .iter()
        .filter(|identity| allowed.iter().any(|key| key == &identity.blob))
        .collect();
    match overlap.len() {
        0 => Ok(None),
        1 => Ok(Some(overlap[0].blob.clone())),
        _ => {
            let mut message = String::from(
                "several of your keys are authorized by the host. Pass --key with one of:",
            );
            for identity in overlap {
                message.push('\n');
                message.push_str(&public_key_line(&identity.blob, &identity.comment)?);
            }
            Err(SessionError::fatal(message))
        }
    }
}

struct LocatedKey {
    identity: AgentIdentity,
    socket: PathBuf,
}

fn candidate_sockets() -> Vec<PathBuf> {
    let mut sockets = Vec::new();
    if let Some(sock) = std::env::var_os("SSH_AUTH_SOCK") {
        let path = PathBuf::from(sock);
        if !path.as_os_str().is_empty() && path.exists() {
            sockets.push(path);
        }
    }
    if let Some(fallback) = onepassword_agent_socket() {
        if fallback.exists() && !sockets.iter().any(|have| have == &fallback) {
            sockets.push(fallback);
        }
    }
    sockets
}

fn list_identities(path: &Path) -> Result<Vec<AgentIdentity>, SessionError> {
    Agent::connect(path)?.identities()
}

fn add_identities(out: &mut Vec<LocatedKey>, socket: &Path, found: Vec<AgentIdentity>) {
    for identity in found {
        if out.iter().any(|have| have.identity.blob == identity.blob) {
            continue;
        }
        out.push(LocatedKey {
            identity,
            socket: socket.to_path_buf(),
        });
    }
}

fn onepassword_agent_socket() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        Some(
            PathBuf::from(home)
                .join("Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock"),
        )
    }
    #[cfg(target_os = "linux")]
    {
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".1password/agent.sock"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(PathBuf::from(r"\\.\pipe\openssh-ssh-agent"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

impl Agent {
    fn connect(path: &Path) -> Result<Self, SessionError> {
        let stream = open_stream(path).map_err(|_| {
            SessionError::agent_down(format!(
                "could not reach the SSH agent at {}. Start the 1Password SSH agent, or set SSH_AUTH_SOCK",
                path.display()
            ))
        })?;
        Ok(Self { stream })
    }

    fn identities(&mut self) -> Result<Vec<AgentIdentity>, SessionError> {
        write_message(&mut self.stream, REQUEST_IDENTITIES, &[])?;
        let (kind, payload) = read_message(&mut self.stream)?;
        if kind == AGENT_FAILURE {
            return Err(SessionError::agent_down(
                "the SSH agent refused to list keys. Unlock 1Password and try again",
            ));
        }
        if kind != IDENTITIES_ANSWER {
            return Err(SessionError::agent_down(format!(
                "the SSH agent returned message {kind} instead of its key list"
            )));
        }
        parse_identities(&payload)
    }

    fn sign(&mut self, public_key: &[u8], data: &[u8]) -> Result<Vec<u8>, SessionError> {
        let mut payload = Vec::new();
        push_ssh_string(&mut payload, public_key)?;
        push_ssh_string(&mut payload, data)?;
        payload.extend_from_slice(&0u32.to_be_bytes());
        write_message(&mut self.stream, SIGN_REQUEST, &payload)?;
        let (kind, payload) = read_message(&mut self.stream)?;
        if kind == AGENT_FAILURE {
            return Err(SessionError::fatal(
                "the SSH agent refused to sign. Approve deskbridge in 1Password and try again",
            ));
        }
        if kind != SIGN_RESPONSE {
            return Err(SessionError::fatal(format!(
                "the SSH agent returned message {kind} instead of a signature"
            )));
        }
        let mut rest = payload.as_slice();
        let signature = read_ssh_string(&mut rest)?.to_vec();
        if !rest.is_empty() {
            return Err(SessionError::fatal("the SSH agent signature has trailing bytes"));
        }
        Ok(signature)
    }
}

#[cfg(unix)]
fn open_stream(path: &Path) -> io::Result<Box<dyn AgentIo + Send>> {
    let stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(Box::new(stream))
}

#[cfg(windows)]
fn open_stream(path: &Path) -> io::Result<Box<dyn AgentIo + Send>> {
    let file = std::fs::OpenOptions::new().read(true).write(true).open(path)?;
    Ok(Box::new(file))
}

#[cfg(not(any(unix, windows)))]
fn open_stream(_path: &Path) -> io::Result<Box<dyn AgentIo + Send>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this operating system has no SSH agent socket",
    ))
}

fn write_message(stream: &mut dyn AgentIo, kind: u8, payload: &[u8]) -> Result<(), SessionError> {
    let len = u32::try_from(payload.len() + 1).map_err(|_| SessionError::new("ssh agent message is too long"))?;
    stream
        .write_all(&len.to_be_bytes())
        .and_then(|_| stream.write_all(&[kind]))
        .and_then(|_| stream.write_all(payload))
        .and_then(|_| stream.flush())
        .map_err(|err| SessionError::agent_down(format!("the SSH agent connection failed: {err}")))
}

fn read_message(stream: &mut dyn AgentIo) -> Result<(u8, Vec<u8>), SessionError> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).map_err(agent_io)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(SessionError::agent_down("the SSH agent sent a message of the wrong length"));
    }
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).map_err(agent_io)?;
    Ok((body[0], body[1..].to_vec()))
}

fn agent_io(err: io::Error) -> SessionError {
    if err.kind() == io::ErrorKind::TimedOut || err.kind() == io::ErrorKind::WouldBlock {
        SessionError::agent_down(
            "the SSH agent did not answer. Approve the request in 1Password and try again",
        )
    } else {
        SessionError::agent_down(format!("the SSH agent connection failed: {err}"))
    }
}

fn parse_identities(payload: &[u8]) -> Result<Vec<AgentIdentity>, SessionError> {
    let mut rest = payload;
    if rest.len() < 4 {
        return Err(SessionError::agent_down("the SSH agent key list was truncated"));
    }
    let count = u32::from_be_bytes(rest[..4].try_into().expect("four bytes"));
    rest = &rest[4..];
    if count > MAX_IDENTITIES {
        return Err(SessionError::agent_down("the SSH agent returned too many keys"));
    }
    let mut identities = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let blob = read_ssh_string(&mut rest)?.to_vec();
        let comment = read_ssh_string(&mut rest)?;
        let comment = String::from_utf8_lossy(comment).into_owned();
        identities.push(AgentIdentity { blob, comment });
    }
    Ok(identities)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshkey::{
        push_ssh_string, read_ssh_string, signing_payload, verify_client_signature, TestKey,
    };

    #[test]
    fn select_key_uses_the_single_overlap() {
        let one = TestKey::generate("one");
        let two = TestKey::generate("two");
        let identities = vec![
            AgentIdentity { blob: one.public_blob.clone(), comment: "one".into() },
            AgentIdentity { blob: two.public_blob.clone(), comment: "two".into() },
        ];
        let chosen = select_key(&identities, &[one.public_blob.clone()], None)
            .unwrap()
            .unwrap();
        assert_eq!(chosen, one.public_blob);
        assert!(select_key(&identities, &[], None).unwrap().is_none());
        let err = select_key(&identities, &[one.public_blob.clone(), two.public_blob.clone()], None)
            .unwrap_err();
        assert!(err.to_string().contains("one"), "{err}");
        assert!(err.to_string().contains("two"), "{err}");
        assert!(err.is_fatal());
    }

    #[test]
    fn a_named_key_must_be_on_the_host_and_in_the_agent() {
        let one = TestKey::generate("one");
        let identities = vec![AgentIdentity { blob: one.public_blob.clone(), comment: "one".into() }];
        let chosen = select_key(&identities, &[one.public_blob.clone()], Some(&one.public_blob))
            .unwrap()
            .unwrap();
        assert_eq!(chosen, one.public_blob);
        let other = TestKey::generate("other");
        assert!(select_key(&identities, &[one.public_blob.clone()], Some(&other.public_blob)).is_err());
        assert!(select_key(&identities, &[other.public_blob.clone()], Some(&one.public_blob)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_agent_signs_the_challenge() {
        use std::os::unix::net::UnixListener;

        let key = TestKey::generate("laptop");
        let path = std::env::temp_dir().join(format!(
            "deskbridge-agent-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let public_blob = key.public_blob.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let (kind, _) = read_message(&mut stream).unwrap();
            assert_eq!(kind, REQUEST_IDENTITIES);
            let mut body = Vec::new();
            body.extend_from_slice(&1u32.to_be_bytes());
            push_ssh_string(&mut body, &key.public_blob).unwrap();
            push_ssh_string(&mut body, key.comment.as_bytes()).unwrap();
            write_message(&mut stream, IDENTITIES_ANSWER, &body).unwrap();
            let (kind, payload) = read_message(&mut stream).unwrap();
            assert_eq!(kind, SIGN_REQUEST);
            let mut rest = payload.as_slice();
            let pubkey = read_ssh_string(&mut rest).unwrap().to_vec();
            let data = read_ssh_string(&mut rest).unwrap().to_vec();
            assert_eq!(pubkey, key.public_blob);
            let signature = key.sign(&data);
            let mut body = Vec::new();
            push_ssh_string(&mut body, &signature).unwrap();
            write_message(&mut stream, SIGN_RESPONSE, &body).unwrap();
        });

        let nonce = [3u8; 32];
        let fingerprint = [4u8; 32];
        let signed = sign_with_agent_at(&path, None, &nonce, &fingerprint, &[public_blob.clone()])
            .unwrap()
            .unwrap();
        let payload = signing_payload(&nonce, &fingerprint, &public_blob);
        verify_client_signature(&signed.public_key, &payload, &signed.signature).unwrap();
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn keys_from_a_second_agent_are_kept() {
        let first = TestKey::generate("apple");
        let second = TestKey::generate("1password");
        let mut located = Vec::new();
        add_identities(
            &mut located,
            std::path::Path::new("/tmp/apple.sock"),
            vec![AgentIdentity { blob: first.public_blob.clone(), comment: "apple".into() }],
        );
        add_identities(
            &mut located,
            std::path::Path::new("/tmp/1password.sock"),
            vec![
                AgentIdentity { blob: first.public_blob.clone(), comment: "apple".into() },
                AgentIdentity { blob: second.public_blob.clone(), comment: "1password".into() },
            ],
        );
        assert_eq!(located.len(), 2);
        assert_eq!(located[0].socket, std::path::Path::new("/tmp/apple.sock"));
        assert_eq!(located[1].socket, std::path::Path::new("/tmp/1password.sock"));
        assert_eq!(located[1].identity.blob, second.public_blob);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_onepassword_socket_path_is_the_documented_one() {
        let path = onepassword_agent_socket().unwrap();
        assert!(path.ends_with("2BUA8C4S2C.com.1password/t/agent.sock"));
    }
}
