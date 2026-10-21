//! QUIC session for one Deskbridge host and one viewer.
//!
//! Video rides one unidirectional stream per frame. Each frame is written to
//! completion, in order. The next frame may start while the previous one is
//! still being acknowledged, and the host decides how many may be in flight.
//! Audio rides its own unidirectional stream per packet, ahead of video.
//! Keyboard, mouse, cursor, and codec setup ride one reliable bidirectional
//! stream and are sent ahead of both.

mod agent;
mod identity;
pub mod pace;
mod sshkey;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use deskbridge_proto::{AudioPacket, CodecId, Control, Framer, VideoFrame, AUDIO_MAGIC, AUDIO_PREFIX_LEN, FRAME_MAGIC, PROTOCOL_VERSION};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
pub use quinn::Endpoint;
use quinn::{ClientConfig, Connection, RecvStream, SendStream, ServerConfig, TransportConfig, VarInt};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use tokio::sync::mpsc;

pub use agent::agent_available;
pub use identity::{codes_match, hex, parse_fingerprint, Identity};
pub use sshkey::{parse_authorized_file, parse_public_key_line, public_key_line, AuthorizedKey};

use agent::{sign_with_agent, SignedKey};
use sshkey::{signing_payload, verify_client_signature};

#[derive(Debug)]
pub struct SessionError {
    message: String,
    kind: ErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorKind {
    /// A connection failure. The viewer tries the next address.
    Remote,
    /// Authentication or a key choice failed. The viewer stops.
    Fatal,
    /// The SSH agent could not be used. A pairing code can still be tried.
    AgentDown,
}

impl SessionError {
    pub fn new(err: impl ToString) -> Self {
        Self { message: err.to_string(), kind: ErrorKind::Remote }
    }

    pub(crate) fn fatal(err: impl ToString) -> Self {
        Self { message: err.to_string(), kind: ErrorKind::Fatal }
    }

    pub(crate) fn agent_down(err: impl ToString) -> Self {
        Self { message: err.to_string(), kind: ErrorKind::AgentDown }
    }

    pub fn is_fatal(&self) -> bool {
        self.kind == ErrorKind::Fatal
    }

    fn agent_unavailable(&self) -> bool {
        self.kind == ErrorKind::AgentDown
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SessionError {}

const RESET_NEWER_FRAME: VarInt = VarInt::from_u32(1);
const CONTROL_PRIORITY: i32 = 16;
const AUDIO_PRIORITY: i32 = 8;
const VIDEO_PRIORITY: i32 = 0;
const VIDEO_QUEUE: usize = 8;
const AUDIO_QUEUE: usize = 6;

/// Counts one frame until the viewer has acknowledged it, or the send is dropped.
struct Ticket(Arc<AtomicUsize>);

impl Ticket {
    fn take(backlog: &Arc<AtomicUsize>) -> Self {
        backlog.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(backlog))
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

struct QueuedFrame {
    frame: VideoFrame,
    ticket: Option<Ticket>,
}

/// One acknowledged video frame, plus the path state at that moment.
#[derive(Clone, Copy, Debug)]
pub struct Delivery {
    pub bytes: usize,
    pub elapsed: Duration,
    pub rtt: Duration,
    pub min_rtt: Duration,
    pub cwnd: u64,
}

/// What the capture should do for the link it is actually on.
///
/// A delivery that keeps up with the configured bitrate runs at the display
/// rate and puts enough frames in flight to cover the round trip. A delivery
/// that falls behind, or a round trip that swells, keeps a single frame in
/// flight and lowers the bitrate. A link that stays that slow steps the
/// picture down.
pub struct LinkHealth {
    state: Mutex<LinkState>,
}

struct LinkState {
    configured_bps: u32,
    pace_bps: u32,
    depth: u8,
    spatial: u8,
    ema_bps: f32,
    slow: u8,
    fast: u8,
}

impl LinkHealth {
    pub fn new(configured_bps: u32) -> Self {
        let configured_bps = configured_bps.max(1);
        Self {
            state: Mutex::new(LinkState {
                configured_bps,
                pace_bps: configured_bps,
                // Two frames until the first acknowledgements arrive, so a fast
                // link is not stuck at one frame per round trip during startup.
                depth: 2,
                spatial: 0,
                ema_bps: configured_bps as f32,
                slow: 0,
                fast: 0,
            }),
        }
    }

    /// Frames the capture may have acknowledged-or-in-flight at once.
    pub fn depth(&self) -> usize {
        self.state.lock().map(|state| state.depth as usize).unwrap_or(1)
    }

    pub fn pace_bps(&self) -> u32 {
        self.state
            .lock()
            .map(|state| state.pace_bps)
            .unwrap_or(1)
    }

    /// The link is carrying the bitrate the user asked for.
    pub fn full_rate(&self) -> bool {
        self.state
            .lock()
            .map(|state| state.pace_bps >= state.configured_bps)
            .unwrap_or(false)
    }

    /// 0 is the full picture, 1 is about two thirds, 2 is half.
    pub fn spatial(&self) -> u8 {
        self.state.lock().map(|state| state.spatial).unwrap_or(0)
    }

    pub fn note_delivery(&self, sample: Delivery) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        update_ema(&mut state, &sample);
        let configured = state.configured_bps;
        if delivery_on_time(&sample, configured) {
            state.fast = state.fast.saturating_add(1);
            state.slow = 0;
            if state.fast >= 4 && link_can_carry(&state, &sample, configured) {
                state.depth = depth_for_rtt(unloaded_rtt(&sample));
                state.pace_bps = configured;
                if state.fast >= 8 && state.spatial > 0 && link_can_grow(&state, &sample) {
                    state.spatial -= 1;
                    // Stay at full rate, and require another run of good frames to grow again.
                    state.fast = 4;
                }
            } else if state.fast >= 4 && state.pace_bps < configured {
                // These frames arrived on time because they are small. Ease the
                // rate toward what the window can hold, and keep a single frame
                // in flight until the path can carry the configured bitrate.
                state.depth = 1;
                if let Some(rate) = capacity_bps(sample.cwnd, sample.rtt.max(Duration::from_millis(1))) {
                    let ceiling = (rate * 0.85).clamp(500_000.0, configured as f32);
                    let raised = (state.pace_bps as f32 * 1.2).min(ceiling);
                    if raised > state.pace_bps as f32 {
                        state.pace_bps = raised.round() as u32;
                    }
                }
            }
        } else {
            state.fast = 0;
            state.slow = state.slow.saturating_add(1);
            if state.slow >= 2 {
                state.depth = 1;
                let cap = pace_cap(&state, &sample);
                state.pace_bps = (cap * 0.85).clamp(500_000.0, configured as f32).round() as u32;
                if cap < 2_500_000.0 && state.spatial < 2 && state.slow >= 4 {
                    state.spatial += 1;
                    // Stay conservative, and require another run of slow frames to shrink again.
                    state.slow = 2;
                }
            }
        }
    }
}

fn unloaded_rtt(sample: &Delivery) -> Duration {
    if sample.min_rtt.is_zero() {
        sample.rtt
    } else {
        sample.min_rtt
    }
}

/// How many frame intervals fit in one unloaded round trip, capped so a
/// misjudged link cannot queue a long run of pictures.
fn depth_for_rtt(rtt: Duration) -> u8 {
    const FRAME_US: u128 = 1_000_000 / 60;
    let rtt_us = rtt.as_micros();
    if rtt_us == 0 {
        return 1;
    }
    let depth = ((rtt_us + FRAME_US - 1) / FRAME_US) as u8;
    depth.clamp(1, 6)
}

fn delivery_on_time(sample: &Delivery, configured_bps: u32) -> bool {
    let base = unloaded_rtt(sample);
    // A swollen round trip means the path is buffering. Treat that as late
    // even when this frame took about one (inflated) round trip.
    let inflated = sample.rtt > base.saturating_mul(2)
        && sample.rtt.saturating_sub(base) > Duration::from_millis(30);
    if inflated {
        return false;
    }
    let serialize_us =
        (sample.bytes as u64).saturating_mul(8_000_000) / u64::from(configured_bps.max(1));
    let budget = base
        .saturating_add(Duration::from_micros(serialize_us))
        .saturating_add(Duration::from_millis(25));
    sample.elapsed <= budget
}

fn update_ema(state: &mut LinkState, sample: &Delivery) {
    let elapsed = sample.elapsed.max(Duration::from_micros(500));
    let raw = (sample.bytes as f64) * 8.0 / elapsed.as_secs_f64();
    let raw = raw.clamp(1.0, state.configured_bps as f64 * 4.0) as f32;
    if raw < state.ema_bps {
        state.ema_bps = state.ema_bps * 0.5 + raw * 0.5;
    } else {
        state.ema_bps = state.ema_bps * 0.85 + raw * 0.15;
    }
}

/// Bits per second the congestion window can sustain. `None` until the window is real.
fn capacity_bps(cwnd: u64, rtt: Duration) -> Option<f32> {
    if cwnd < 1_200 {
        return None;
    }
    let secs = rtt.as_secs_f64();
    if secs < 0.001 {
        return None;
    }
    Some((cwnd as f64 * 8.0 / secs) as f32)
}

fn pace_cap(state: &LinkState, sample: &Delivery) -> f32 {
    match capacity_bps(sample.cwnd, sample.rtt.max(Duration::from_millis(1))) {
        Some(rate) => rate.min(state.ema_bps),
        None => state.ema_bps,
    }
}

fn link_can_carry(state: &LinkState, sample: &Delivery, configured_bps: u32) -> bool {
    let need = configured_bps as f32 * 0.8;
    match capacity_bps(sample.cwnd, sample.rtt.max(Duration::from_millis(1))) {
        Some(rate) => rate >= need,
        None => state.ema_bps >= need,
    }
}

fn link_can_grow(state: &LinkState, sample: &Delivery) -> bool {
    match capacity_bps(sample.cwnd, sample.rtt.max(Duration::from_millis(1))) {
        Some(rate) => rate >= 8_000_000.0,
        None => state.ema_bps >= 8_000_000.0,
    }
}

/// How the viewer proved it may take the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerProof {
    PairingCode,
    SshKey { comment: String },
}

/// A paired host connection. Dropping it closes the QUIC connection.
pub struct PairedHost {
    pub peer_codecs: Vec<CodecId>,
    pub viewer: ViewerProof,
    /// Largest picture this viewer asked for.
    pub resolution: Option<(u32, u32)>,
    pub from_peer: mpsc::UnboundedReceiver<Control>,
    to_peer: mpsc::UnboundedSender<Control>,
    video_tx: mpsc::Sender<QueuedFrame>,
    audio_tx: mpsc::Sender<AudioPacket>,
    backlog: Mutex<Option<Arc<AtomicUsize>>>,
    link: Arc<Mutex<Option<Arc<LinkHealth>>>>,
    conn: Connection,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl PairedHost {
    pub fn send_control(&self, message: Control) {
        let _ = self.to_peer.send(message);
    }

    /// Queue a frame. The sender writes it to completion before the next one.
    ///
    /// Returns false when this viewer's queue is full. The frame is not sent,
    /// and the caller should request a keyframe so the viewer can resync.
    pub fn offer_frame(&self, frame: VideoFrame) -> bool {
        let ticket = self.backlog.lock().ok().and_then(|slot| {
            slot.as_ref().map(|backlog| Ticket::take(backlog))
        });
        self.video_tx
            .try_send(QueuedFrame { frame, ticket })
            .is_ok()
    }

    /// Queue a frame. A full queue drops this frame.
    pub fn send_frame(&self, frame: VideoFrame) {
        let _ = self.offer_frame(frame);
    }

    /// Queue an audio packet. A full queue drops it so sound stays current.
    pub fn send_audio(&self, packet: AudioPacket) -> bool {
        self.audio_tx.try_send(packet).is_ok()
    }

    /// Frames queued here increment `backlog` until they are acknowledged or dropped.
    pub fn share_backlog(&self, backlog: Arc<AtomicUsize>) {
        if let Ok(mut slot) = self.backlog.lock() {
            *slot = Some(backlog);
        }
    }

    /// Deliveries on this connection update the shared link estimate.
    pub fn share_link(&self, health: Arc<LinkHealth>) {
        if let Ok(mut slot) = self.link.lock() {
            *slot = Some(health);
        }
    }

    /// Tell the viewer this session is over, and wait briefly so the close can leave.
    ///
    /// A hard kill never reaches this path. The viewer then ends on the idle timeout.
    pub async fn shutdown(self) {
        self.conn.close(VarInt::from_u32(0), b"closed");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    /// Move the inbound control channel out so another task can read it.
    ///
    /// `send_control` and `send_frame` stay on this value. The replacement
    /// receiver is already closed.
    pub fn take_inbound(&mut self) -> mpsc::UnboundedReceiver<Control> {
        let (_closed, rx) = mpsc::unbounded_channel();
        std::mem::replace(&mut self.from_peer, rx)
    }
}

impl Drop for PairedHost {
    fn drop(&mut self) {
        self.conn.close(VarInt::from_u32(0), b"closed");
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// A paired viewer connection.
pub struct PairedClient {
    pub peer_codecs: Vec<CodecId>,
    pub fingerprint: [u8; 32],
    pub from_peer: mpsc::UnboundedReceiver<Control>,
    pub frames: FrameInbox,
    pub audio: AudioInbox,
    to_peer: mpsc::UnboundedSender<Control>,
    conn: Connection,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// Holds the newest video frame. Older frames are dropped.
#[derive(Clone)]
pub struct FrameInbox {
    inner: Arc<(std::sync::Mutex<Option<VideoFrame>>, std::sync::Condvar)>,
}

impl FrameInbox {
    fn new() -> Self {
        Self {
            inner: Arc::new((std::sync::Mutex::new(None), std::sync::Condvar::new())),
        }
    }

    fn push(&self, frame: VideoFrame) {
        let (lock, condvar) = &*self.inner;
        *lock.lock().expect("frame inbox") = Some(frame);
        condvar.notify_one();
    }

    /// Take the newest frame, waiting up to `timeout`.
    pub fn recv_timeout(&self, timeout: Duration) -> Option<VideoFrame> {
        let (lock, condvar) = &*self.inner;
        let mut guard = lock.lock().expect("frame inbox");
        let start = std::time::Instant::now();
        loop {
            if let Some(frame) = guard.take() {
                return Some(frame);
            }
            let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
                return None;
            };
            let (next, status) = condvar.wait_timeout(guard, remaining).expect("frame inbox");
            guard = next;
            if status.timed_out() && guard.is_none() {
                return None;
            }
        }
    }

    /// Take the newest frame if one is waiting.
    pub fn try_recv(&self) -> Option<VideoFrame> {
        self.inner.0.lock().expect("frame inbox").take()
    }
}

/// Audio packets in order. A backlog past a few frames drops the oldest so
/// playback cannot fall behind the picture.
#[derive(Clone)]
pub struct AudioInbox {
    inner: Arc<(std::sync::Mutex<std::collections::VecDeque<AudioPacket>>, std::sync::Condvar)>,
}

impl AudioInbox {
    fn new() -> Self {
        Self {
            inner: Arc::new((
                std::sync::Mutex::new(std::collections::VecDeque::new()),
                std::sync::Condvar::new(),
            )),
        }
    }

    fn push(&self, packet: AudioPacket) {
        let (lock, condvar) = &*self.inner;
        let mut queue = lock.lock().expect("audio inbox");
        queue.push_back(packet);
        while queue.len() > 24 {
            queue.pop_front();
        }
        condvar.notify_one();
    }

    pub fn try_recv(&self) -> Option<AudioPacket> {
        self.inner.0.lock().expect("audio inbox").pop_front()
    }

    pub fn recv_timeout(&self, timeout: Duration) -> Option<AudioPacket> {
        let (lock, condvar) = &*self.inner;
        let mut guard = lock.lock().expect("audio inbox");
        let start = std::time::Instant::now();
        loop {
            if let Some(packet) = guard.pop_front() {
                return Some(packet);
            }
            let Some(remaining) = timeout.checked_sub(start.elapsed()) else {
                return None;
            };
            let (next, status) = condvar.wait_timeout(guard, remaining).expect("audio inbox");
            guard = next;
            if status.timed_out() && guard.is_empty() {
                return None;
            }
        }
    }
}

impl PairedClient {
    pub fn send_control(&self, message: Control) {
        let _ = self.to_peer.send(message);
    }

    /// Why the connection is gone, or `None` while it is still open.
    pub fn close_reason(&self) -> Option<String> {
        self.conn.close_reason().map(|err| err.to_string())
    }
}

impl Drop for PairedClient {
    fn drop(&mut self) {
        self.conn.close(VarInt::from_u32(0), b"closed");
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn transport() -> Arc<TransportConfig> {
    let mut transport = TransportConfig::default();
    transport.max_concurrent_uni_streams(VarInt::from_u32(512));
    transport.max_concurrent_bidi_streams(VarInt::from_u32(8));
    transport.stream_receive_window(VarInt::from_u32(8 * 1024 * 1024));
    transport.receive_window(VarInt::from_u32(16 * 1024 * 1024));
    // Large enough for one full-screen keyframe. The host keeps only `depth`
    // frames in flight, so a slow link cannot fill this window.
    transport.send_window(2 * 1024 * 1024);
    // Finish one video stream before sharing the link with the next frame.
    transport.send_fairness(false);
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
    transport.max_idle_timeout(Some(Duration::from_secs(10).try_into().expect("idle timeout")));
    Arc::new(transport)
}

pub fn listen(addr: SocketAddr, identity: &Identity) -> Result<Endpoint, SessionError> {
    identity::install_crypto_provider();
    let crypto = identity::server_crypto(identity)?;
    let quic = QuicServerConfig::try_from(crypto).map_err(SessionError::new)?;
    let mut server = ServerConfig::with_crypto(Arc::new(quic));
    server.transport_config(transport());
    Endpoint::server(server, addr).map_err(SessionError::new)
}

/// Stop accepting and close every connection on this endpoint.
pub fn close_endpoint(endpoint: &Endpoint) {
    endpoint.close(VarInt::from_u32(0), b"stopped");
}

/// What the viewer uses to prove itself. An SSH key signs the host's nonce.
/// The pairing code is the other proof.
pub struct ClientAuth {
    pub code: Option<String>,
    pub keys: KeyChoice,
}

pub enum KeyChoice {
    PairingCodeOnly,
    /// Ask the SSH agent. `chosen` is the ssh-ed25519 wire blob from `--key`.
    Agent { chosen: Option<Vec<u8>> },
    Direct(DirectKey),
}

pub struct DirectKey {
    pub public_key: Vec<u8>,
    pub sign: Box<dyn Fn(&[u8]) -> Result<Vec<u8>, SessionError> + Send>,
}

/// Accept one connection and complete the pairing handshake.
pub async fn accept_one(
    endpoint: &Endpoint,
    identity: &Identity,
    host_codecs: Vec<CodecId>,
    authorized: &[AuthorizedKey],
) -> Result<PairedHost, SessionError> {
    let incoming = endpoint
        .accept()
        .await
        .ok_or_else(|| SessionError::new("endpoint closed"))?;
    let conn = incoming.await.map_err(SessionError::new)?;
    let (mut send, mut recv) = conn.accept_bi().await.map_err(SessionError::new)?;
    let mut framer = Framer::new();
    let mut pending = VecDeque::new();

    let hello = next_control(&mut recv, &mut framer, &mut pending).await?;
    let Control::Hello(hello) = hello else {
        return Err(SessionError::new("client did not start with Hello"));
    };
    if hello.version != PROTOCOL_VERSION {
        return Err(SessionError::new(format!(
            "client speaks protocol {}, this host speaks {PROTOCOL_VERSION}",
            hello.version
        )));
    }

    let nonce = random_nonce()?;
    write_control(
        &mut send,
        &Control::HelloAck(deskbridge_proto::HelloAck {
            version: PROTOCOL_VERSION,
            codecs: host_codecs,
            fingerprint: identity.fingerprint,
            nonce,
            authorized_keys: authorized.iter().map(|key| key.blob.clone()).collect(),
        }),
    )
    .await?;

    let proof = next_control(&mut recv, &mut framer, &mut pending).await?;
    let viewer = match proof {
        Control::Pair { code } => {
            if !codes_match(&code, &identity.code) {
                return deny(&mut send, "pairing code rejected").await;
            }
            ViewerProof::PairingCode
        }
        Control::Authenticate { public_key, signature } => {
            let Some(key) = authorized.iter().find(|key| key.blob == public_key) else {
                return deny(&mut send, "ssh key rejected").await;
            };
            let payload = signing_payload(&nonce, &identity.fingerprint, &public_key);
            if verify_client_signature(&public_key, &payload, &signature).is_err() {
                return deny(&mut send, "ssh key rejected").await;
            }
            ViewerProof::SshKey { comment: key.comment.clone() }
        }
        _ => return deny(&mut send, "client did not authenticate").await,
    };
    write_control(&mut send, &Control::PairOk).await?;

    let link = spawn_link(conn, send, recv, framer, pending, hello.codecs, true);
    Ok(PairedHost {
        viewer,
        peer_codecs: link.peer_codecs,
        resolution: hello.resolution,
        from_peer: link.from_peer,
        to_peer: link.to_peer,
        video_tx: link.video_tx,
        audio_tx: link.audio_tx,
        backlog: Mutex::new(None),
        link: link.link,
        conn: link.conn,
        tasks: link.tasks,
    })
}

async fn deny(send: &mut SendStream, message: &str) -> Result<PairedHost, SessionError> {
    let _ = write_control(send, &Control::PairReject).await;
    // Wait until the reject is acknowledged so the client can read it
    // before this connection is dropped.
    let _ = send.finish();
    let _ = send.stopped().await;
    Err(SessionError::fatal(message))
}

pub async fn connect(
    addr: SocketAddr,
    auth: &ClientAuth,
    expected_fingerprint: Option<[u8; 32]>,
    client_codecs: Vec<CodecId>,
    resolution: Option<(u32, u32)>,
) -> Result<PairedClient, SessionError> {
    identity::install_crypto_provider();
    let seen = Arc::new(std::sync::Mutex::new(None));
    let verifier = Arc::new(PinVerifier {
        expected: expected_fingerprint,
        seen: Arc::clone(&seen),
        provider: Arc::new(rustls::crypto::ring::default_provider()),
    });
    let mut crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![b"deskbridge".to_vec()];
    let quic = QuicClientConfig::try_from(crypto).map_err(SessionError::new)?;
    let mut client = ClientConfig::new(Arc::new(quic));
    client.transport_config(transport());

    let mut endpoint = Endpoint::client(SocketAddr::from(([0, 0, 0, 0], 0))).map_err(SessionError::new)?;
    endpoint.set_default_client_config(client);
    let connecting = endpoint.connect(addr, "deskbridge").map_err(SessionError::new)?;
    let conn = match tokio::time::timeout(Duration::from_secs(8), connecting).await {
        Ok(result) => result.map_err(SessionError::new)?,
        Err(_) => return Err(SessionError::new("timed out")),
    };

    let fingerprint = seen
        .lock()
        .expect("fingerprint lock")
        .ok_or_else(|| SessionError::fatal("server certificate was not presented"))?;
    if let Some(expected) = expected_fingerprint {
        if fingerprint != expected {
            return Err(SessionError::fatal("host fingerprint does not match"));
        }
    }

    let (mut send, mut recv) = conn.open_bi().await.map_err(SessionError::new)?;
    write_control(
        &mut send,
        &Control::Hello(deskbridge_proto::Hello {
            version: PROTOCOL_VERSION,
            codecs: client_codecs,
            name: "deskbridge".into(),
            resolution,
        }),
    )
    .await?;

    let mut framer = Framer::new();
    let mut pending = VecDeque::new();
    let ack = match tokio::time::timeout(
        Duration::from_secs(8),
        next_control(&mut recv, &mut framer, &mut pending),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => return Err(SessionError::new("timed out")),
    };
    let Control::HelloAck(ack) = ack else {
        return Err(SessionError::fatal("host did not acknowledge"));
    };
    if ack.version != PROTOCOL_VERSION {
        return Err(SessionError::fatal("host protocol version differs"));
    }
    if ack.fingerprint != fingerprint {
        return Err(SessionError::fatal("certificate fingerprint and HelloAck disagree"));
    }

    let signed = match &auth.keys {
        KeyChoice::PairingCodeOnly => None,
        KeyChoice::Agent { chosen } => {
            let forced = chosen.is_some();
            let chosen = chosen.clone();
            let allowed = ack.authorized_keys.clone();
            let nonce = ack.nonce;
            let signed_fingerprint = ack.fingerprint;
            let signed = tokio::task::spawn_blocking(move || {
                sign_with_agent(chosen.as_deref(), &nonce, &signed_fingerprint, &allowed)
            })
            .await
            .map_err(|err| SessionError::new(format!("ssh agent task failed: {err}")))?;
            match signed {
                Ok(proof) => proof,
                Err(err) if err.agent_unavailable() && !forced && auth.code.is_some() => {
                    eprintln!("deskbridge: {err}");
                    eprintln!("deskbridge: using the pairing code");
                    None
                }
                Err(err) => return Err(err),
            }
        }
        KeyChoice::Direct(key) => {
            Some(sign_direct(key, &ack.nonce, &ack.fingerprint, &ack.authorized_keys)?)
        }
    };
    let sent_key = if let Some(proof) = signed {
        write_control(
            &mut send,
            &Control::Authenticate {
                public_key: proof.public_key,
                signature: proof.signature,
            },
        )
        .await?;
        true
    } else if let Some(code) = &auth.code {
        write_control(&mut send, &Control::Pair { code: code.clone() }).await?;
        false
    } else {
        return Err(SessionError::fatal(
            "the host's authorized keys do not match the SSH agent. Pass --key, or pass --code",
        ));
    };
    match next_control(&mut recv, &mut framer, &mut pending).await? {
        Control::PairOk => {}
        Control::PairReject if sent_key => {
            return Err(SessionError::fatal("host rejected the ssh key"));
        }
        Control::PairReject => return Err(SessionError::fatal("host rejected the pairing code")),
        _ => return Err(SessionError::fatal("unexpected reply to the pairing proof")),
    }

    let paired = spawn_link(conn, send, recv, framer, pending, ack.codecs, false);
    Ok(PairedClient {
        peer_codecs: paired.peer_codecs,
        fingerprint,
        from_peer: paired.from_peer,
        frames: paired.frames,
        audio: paired.audio,
        to_peer: paired.to_peer,
        conn: paired.conn,
        tasks: paired.tasks,
    })
}

fn sign_direct(
    key: &DirectKey,
    nonce: &[u8; 32],
    fingerprint: &[u8; 32],
    allowed: &[Vec<u8>],
) -> Result<SignedKey, SessionError> {
    if !allowed.iter().any(|item| item == &key.public_key) {
        let message = if allowed.is_empty() {
            "this host has no authorized SSH keys"
        } else {
            "the host does not authorize this key"
        };
        return Err(SessionError::fatal(message));
    }
    let payload = signing_payload(nonce, fingerprint, &key.public_key);
    let signature = (key.sign)(&payload)?;
    Ok(SignedKey {
        public_key: key.public_key.clone(),
        signature,
    })
}

fn random_nonce() -> Result<[u8; 32], SessionError> {
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).map_err(SessionError::new)?;
    Ok(nonce)
}

struct LinkParts {
    peer_codecs: Vec<CodecId>,
    from_peer: mpsc::UnboundedReceiver<Control>,
    to_peer: mpsc::UnboundedSender<Control>,
    frames: FrameInbox,
    audio: AudioInbox,
    video_tx: mpsc::Sender<QueuedFrame>,
    audio_tx: mpsc::Sender<AudioPacket>,
    link: Arc<Mutex<Option<Arc<LinkHealth>>>>,
    conn: Connection,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

fn spawn_link(
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    framer: Framer,
    pending: VecDeque<Control>,
    peer_codecs: Vec<CodecId>,
    reply_to_ping: bool,
) -> LinkParts {
    let (to_peer, mut outbound) = mpsc::unbounded_channel::<Control>();
    let (from_peer_tx, from_peer) = mpsc::unbounded_channel::<Control>();
    let frames = FrameInbox::new();
    let frame_tx = frames.clone();
    let audio = AudioInbox::new();
    let audio_rx_inbox = audio.clone();
    let (video_tx, mut video_rx) = mpsc::channel::<QueuedFrame>(VIDEO_QUEUE);
    let (audio_tx, mut audio_packets) = mpsc::channel::<AudioPacket>(AUDIO_QUEUE);
    let link: Arc<Mutex<Option<Arc<LinkHealth>>>> = Arc::new(Mutex::new(None));
    let link_slot = Arc::clone(&link);

    let writer = tokio::spawn(async move {
        let mut send = send;
        let _ = send.set_priority(CONTROL_PRIORITY);
        while let Some(message) = outbound.recv().await {
            if write_control(&mut send, &message).await.is_err() {
                break;
            }
        }
    });

    let to_peer_for_ping = to_peer.clone();
    let reader = tokio::spawn(async move {
        let mut recv = recv;
        let mut framer = framer;
        let mut pending = pending;
        loop {
            let message = match next_control(&mut recv, &mut framer, &mut pending).await {
                Ok(message) => message,
                Err(_) => break,
            };
            if reply_to_ping {
                if let Control::Ping { client_us } = message {
                    let _ = to_peer_for_ping.send(Control::Pong {
                        client_us,
                        host_us: mono_us(),
                    });
                    continue;
                }
            }
            if from_peer_tx.send(message).is_err() {
                break;
            }
        }
    });

    let video_conn = conn.clone();
    let video = tokio::spawn(async move {
        while let Some(QueuedFrame { frame, ticket }) = video_rx.recv().await {
            let Ok(mut stream) = video_conn.open_uni().await else {
                break;
            };
            let _ = stream.set_priority(VIDEO_PRIORITY);
            let started = std::time::Instant::now();
            let bytes = frame.payload.len();
            if write_frame(&mut stream, &frame).await.is_err() {
                let _ = stream.reset(RESET_NEWER_FRAME);
                continue;
            }
            // The next frame can start while this one is still being acknowledged.
            // The ticket stays until the viewer has the bytes, which is what
            // tells the host whether the link has room.
            let slot = Arc::clone(&link_slot);
            let conn = video_conn.clone();
            tokio::spawn(async move {
                let _ticket = ticket;
                if stream.stopped().await.is_err() {
                    return;
                }
                let health = slot.lock().ok().and_then(|slot| slot.clone());
                if let Some(health) = health {
                    health.note_delivery(Delivery {
                        bytes,
                        elapsed: started.elapsed(),
                        rtt: conn.rtt(),
                        min_rtt: conn.min_rtt(),
                        cwnd: conn.stats().path.cwnd,
                    });
                }
            });
        }
    });

    let audio_conn = conn.clone();
    let audio_task = tokio::spawn(async move {
        while let Some(packet) = audio_packets.recv().await {
            let Ok(mut stream) = audio_conn.open_uni().await else {
                break;
            };
            let _ = stream.set_priority(AUDIO_PRIORITY);
            if write_audio(&mut stream, &packet).await.is_err() {
                let _ = stream.reset(RESET_NEWER_FRAME);
                continue;
            }
            // The next packet can start while this one is still acknowledged.
            // Audio is not part of the video pace.
            tokio::spawn(async move {
                let _ = stream.stopped().await;
            });
        }
    });

    let frame_conn = conn.clone();
    let frame_reader = tokio::spawn(async move {
        loop {
            let Ok(mut stream) = frame_conn.accept_uni().await else { break };
            match read_incoming(&mut stream).await {
                Ok(Incoming::Video(frame)) => frame_tx.push(frame),
                Ok(Incoming::Audio(packet)) => audio_rx_inbox.push(packet),
                Err(_) => continue,
            }
        }
    });

    LinkParts {
        peer_codecs,
        from_peer,
        to_peer,
        frames,
        audio,
        video_tx,
        audio_tx,
        link,
        conn,
        tasks: vec![writer, reader, video, audio_task, frame_reader],
    }
}

/// Microseconds since the first call in this process. Host and client clocks are not the same.
pub fn mono_us() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let start = START.get_or_init(std::time::Instant::now);
    start.elapsed().as_micros() as u64
}

async fn write_control(send: &mut SendStream, message: &Control) -> Result<(), SessionError> {
    let bytes = message.encode().map_err(SessionError::new)?;
    send.write_all(&bytes).await.map_err(SessionError::new)
}

async fn next_control(
    recv: &mut RecvStream,
    framer: &mut Framer,
    pending: &mut VecDeque<Control>,
) -> Result<Control, SessionError> {
    if let Some(message) = pending.pop_front() {
        return Ok(message);
    }
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = recv
            .read(&mut buf)
            .await
            .map_err(SessionError::new)?
            .ok_or_else(|| SessionError::new("control stream closed"))?;
        let messages = framer.push(&buf[..n]).map_err(SessionError::new)?;
        if messages.is_empty() {
            continue;
        }
        let mut messages = messages.into_iter();
        let first = messages.next().expect("non-empty");
        pending.extend(messages);
        return Ok(first);
    }
}

async fn write_frame(send: &mut SendStream, frame: &VideoFrame) -> Result<(), SessionError> {
    let header = frame.header_bytes();
    send.write_all(&header).await.map_err(SessionError::new)?;
    send.write_all(&frame.payload).await.map_err(SessionError::new)?;
    send.finish().map_err(SessionError::new)
}

enum Incoming {
    Video(VideoFrame),
    Audio(AudioPacket),
}

async fn read_incoming(recv: &mut RecvStream) -> Result<Incoming, SessionError> {
    let mut magic = [0u8; 4];
    recv.read_exact(&mut magic).await.map_err(SessionError::new)?;
    if &magic == AUDIO_MAGIC {
        return Ok(Incoming::Audio(read_audio(recv).await?));
    }
    if &magic != FRAME_MAGIC {
        return Err(SessionError::new("stream magic was not recognized"));
    }
    let mut rest = [0u8; deskbridge_proto::FRAME_HEADER_LEN - 4];
    recv.read_exact(&mut rest).await.map_err(SessionError::new)?;
    let mut header = [0u8; deskbridge_proto::FRAME_HEADER_LEN];
    header[..4].copy_from_slice(&magic);
    header[4..].copy_from_slice(&rest);
    let (mut frame, _) = VideoFrame::decode_header(&header).map_err(SessionError::new)?;
    frame.payload = recv_rest(recv, 8 * 1024 * 1024).await?;
    Ok(Incoming::Video(frame))
}

async fn read_audio(recv: &mut RecvStream) -> Result<AudioPacket, SessionError> {
    let mut rest = [0u8; AUDIO_PREFIX_LEN - 4];
    recv.read_exact(&mut rest).await.map_err(SessionError::new)?;
    let mut header = [0u8; AUDIO_PREFIX_LEN];
    header[..4].copy_from_slice(AUDIO_MAGIC);
    header[4..].copy_from_slice(&rest);
    let (sample_rate, channels, capture_us, cookie_len) =
        AudioPacket::decode_prefix(&header).map_err(SessionError::new)?;
    let mut cookie = vec![0u8; cookie_len as usize];
    if cookie_len > 0 {
        recv.read_exact(&mut cookie).await.map_err(SessionError::new)?;
    }
    let payload = recv_rest(recv, 16 * 1024).await?;
    if payload.is_empty() {
        return Err(SessionError::new("empty audio packet"));
    }
    Ok(AudioPacket {
        sample_rate,
        channels,
        capture_us,
        cookie,
        payload,
    })
}

async fn write_audio(send: &mut SendStream, packet: &AudioPacket) -> Result<(), SessionError> {
    let prefix = packet.prefix_bytes().map_err(SessionError::new)?;
    send.write_all(&prefix).await.map_err(SessionError::new)?;
    if !packet.cookie.is_empty() {
        send.write_all(&packet.cookie).await.map_err(SessionError::new)?;
    }
    send.write_all(&packet.payload).await.map_err(SessionError::new)?;
    send.finish().map_err(SessionError::new)
}

async fn recv_rest(recv: &mut RecvStream, limit: usize) -> Result<Vec<u8>, SessionError> {
    let mut payload = Vec::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match recv.read(&mut buf).await.map_err(SessionError::new)? {
            Some(0) => continue,
            Some(n) => {
                if payload.len() + n > limit {
                    return Err(SessionError::new("stream payload exceeds its cap"));
                }
                payload.extend_from_slice(&buf[..n]);
            }
            None => return Ok(payload),
        }
    }
}

#[derive(Debug)]
struct PinVerifier {
    expected: Option<[u8; 32]>,
    seen: Arc<std::sync::Mutex<Option<[u8; 32]>>>,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = identity::fingerprint_of(end_entity.as_ref());
        if let Some(expected) = self.expected {
            if fingerprint != expected {
                return Err(rustls::Error::General("host fingerprint mismatch".into()));
            }
        }
        *self.seen.lock().expect("fingerprint lock") = Some(fingerprint);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod link_tests {
    use super::{Delivery, LinkHealth};
    use std::time::Duration;

    fn deliver(
        health: &LinkHealth,
        bytes: usize,
        elapsed_ms: u64,
        rtt_ms: u64,
        min_rtt_ms: u64,
        cwnd: u64,
    ) {
        health.note_delivery(Delivery {
            bytes,
            elapsed: Duration::from_millis(elapsed_ms),
            rtt: Duration::from_millis(rtt_ms),
            min_rtt: Duration::from_millis(min_rtt_ms),
            cwnd,
        });
    }

    #[test]
    fn a_low_delay_link_runs_at_full_rate_with_one_frame_in_flight() {
        let health = LinkHealth::new(20_000_000);
        assert_eq!(health.depth(), 2);
        for _ in 0..4 {
            deliver(&health, 40_000, 8, 8, 8, 200_000);
        }
        assert_eq!(health.depth(), 1);
        assert!(health.full_rate());
        assert_eq!(health.pace_bps(), 20_000_000);
        assert_eq!(health.spatial(), 0);
    }

    #[test]
    fn a_fat_link_with_a_longer_round_trip_pipelines_frames() {
        let health = LinkHealth::new(20_000_000);
        // 150 KB of window over 40 ms is about 30 Mbps, above the 20 Mbps target.
        for _ in 0..4 {
            deliver(&health, 40_000, 42, 40, 40, 150_000);
        }
        assert_eq!(health.depth(), 3);
        assert!(health.full_rate());
        assert_eq!(health.spatial(), 0);
    }

    #[test]
    fn a_bloated_round_trip_stops_piling_frames() {
        let health = LinkHealth::new(20_000_000);
        for _ in 0..3 {
            deliver(&health, 40_000, 120, 120, 20, 200_000);
        }
        assert_eq!(health.depth(), 1);
        assert!(health.pace_bps() < 10_000_000, "pace {}", health.pace_bps());
        assert_eq!(health.spatial(), 0);
    }

    #[test]
    fn a_slow_link_shrinks_and_a_recovered_link_grows_back() {
        let health = LinkHealth::new(20_000_000);
        // 10 KB of window over 100 ms is about 0.8 Mbps.
        for _ in 0..6 {
            deliver(&health, 80_000, 500, 100, 100, 10_000);
        }
        assert_eq!(health.depth(), 1);
        assert!(health.spatial() >= 1, "spatial {}", health.spatial());
        assert!(health.pace_bps() < 20_000_000);
        assert!(health.pace_bps() >= 500_000, "pace {}", health.pace_bps());
        assert!(!health.full_rate());

        for _ in 0..12 {
            deliver(&health, 20_000, 10, 10, 10, 200_000);
        }
        assert_eq!(health.depth(), 1);
        assert_eq!(health.spatial(), 0);
        assert_eq!(health.pace_bps(), 20_000_000);
        assert!(health.full_rate());
    }

    #[test]
    fn a_thin_link_that_delivers_small_frames_does_not_jump_back() {
        let health = LinkHealth::new(20_000_000);
        // 12 KB of window over 50 ms is about 1.9 Mbps.
        for _ in 0..4 {
            deliver(&health, 80_000, 400, 50, 50, 12_000);
        }
        assert_eq!(health.depth(), 1);
        assert!(health.pace_bps() < 5_000_000, "pace {}", health.pace_bps());
        for _ in 0..8 {
            deliver(&health, 4_000, 55, 50, 50, 12_000);
        }
        assert_eq!(health.depth(), 1);
        assert!(health.pace_bps() < 5_000_000, "pace {}", health.pace_bps());
        assert!(!health.full_rate());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deskbridge_proto::{Input, VideoFrame};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn loopback_pairs_and_moves_a_frame_and_an_input() {
        let identity = Identity::generate().unwrap();
        let endpoint = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let code = identity.code.clone();
        let fingerprint = identity.fingerprint;
        let host_task = tokio::spawn(async move {
            accept_one(&endpoint, &identity, vec![CodecId::Hevc, CodecId::H264], &[]).await
        });

        let auth = ClientAuth {
            code: Some(code),
            keys: KeyChoice::PairingCodeOnly,
        };
        let mut client = connect(addr, &auth, Some(fingerprint), vec![CodecId::Hevc, CodecId::H264], None)
            .await
            .unwrap();
        let mut host = host_task.await.unwrap().unwrap();
        assert!(matches!(host.viewer, ViewerProof::PairingCode));
        assert_eq!(client.peer_codecs, vec![CodecId::Hevc, CodecId::H264]);
        assert_eq!(host.peer_codecs, vec![CodecId::Hevc, CodecId::H264]);

        host.send_frame(VideoFrame {
            id: 3,
            keyframe: true,
            codec: CodecId::Hevc,
            capture_us: 50,
            encode_us: 2000,
            payload: b"access-unit".to_vec(),
        });
        let frame = client.frames.recv_timeout(Duration::from_secs(3)).expect("frame");
        assert_eq!(frame.id, 3);
        assert_eq!(frame.payload, b"access-unit");
        assert!(frame.keyframe);

        assert!(host.send_audio(deskbridge_proto::AudioPacket {
            sample_rate: 48_000,
            channels: 2,
            capture_us: 7,
            cookie: vec![0x11, 0x90],
            payload: b"aac-frame".to_vec(),
        }));
        let audio = client.audio.recv_timeout(Duration::from_secs(3)).expect("audio");
        assert_eq!(audio.payload, b"aac-frame");
        assert_eq!(audio.cookie, vec![0x11, 0x90]);
        assert_eq!(audio.sample_rate, 48_000);
        assert_eq!(audio.channels, 2);

        client.send_control(Control::Input(Input::MouseMove { x: 4, y: 5 }));
        let input = wait_control(&mut host.from_peer).await;
        assert_eq!(input, Control::Input(Input::MouseMove { x: 4, y: 5 }));

        client.send_control(Control::Ping { client_us: 10 });
        let pong = wait_control(&mut client.from_peer).await;
        match pong {
            Control::Pong { client_us, .. } => {
                assert_eq!(client_us, 10);
            }
            other => panic!("expected pong, got {other:?}"),
        }

        assert!(client.close_reason().is_none());
        host.shutdown().await;
        let reason = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let reason = client.close_reason();
                let disconnected = matches!(
                    client.from_peer.try_recv(),
                    Err(mpsc::error::TryRecvError::Disconnected)
                );
                if disconnected {
                    if let Some(reason) = reason {
                        return reason;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("client did not see the host close");
        assert!(
            reason.to_ascii_lowercase().contains("closed"),
            "{reason}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wrong_code_is_rejected() {
        let identity = Identity::generate().unwrap();
        let endpoint = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let host_task = tokio::spawn(async move {
            accept_one(&endpoint, &identity, vec![CodecId::H264], &[]).await
        });
        let auth = ClientAuth {
            code: Some("WRONGCODE".into()),
            keys: KeyChoice::PairingCodeOnly,
        };
        match connect(addr, &auth, None, vec![CodecId::H264], None).await {
            Err(err) => assert!(
                err.to_string().contains("rejected") || err.to_string().contains("closed"),
                "{err}"
            ),
            Ok(_) => panic!("wrong pairing code was accepted"),
        }
        match host_task.await.expect("host task") {
            Err(err) => assert!(err.to_string().contains("rejected"), "{err}"),
            Ok(_) => panic!("host accepted a wrong pairing code"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_authorized_ssh_key_pairs_and_a_different_key_is_rejected() {
        let key = sshkey::TestKey::generate("laptop");
        let identity = Identity::generate().unwrap();
        let endpoint = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let authorized = vec![AuthorizedKey {
            blob: key.public_blob.clone(),
            comment: key.comment.clone(),
        }];
        let host_task = tokio::spawn(async move {
            accept_one(&endpoint, &identity, vec![CodecId::H264], &authorized).await
        });
        let public_key = key.public_blob.clone();
        let auth = ClientAuth {
            code: None,
            keys: KeyChoice::Direct(DirectKey {
                public_key: public_key.clone(),
                sign: Box::new(move |payload| Ok(key.sign(payload))),
            }),
        };
        let client = connect(addr, &auth, None, vec![CodecId::H264], None).await.unwrap();
        let host = host_task.await.unwrap().unwrap();
        assert_eq!(
            host.viewer,
            ViewerProof::SshKey { comment: "laptop".into() }
        );
        drop(client);
        drop(host);

        let other = sshkey::TestKey::generate("other");
        let identity = Identity::generate().unwrap();
        let endpoint = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let authorized = vec![AuthorizedKey {
            blob: public_key.clone(),
            comment: "laptop".into(),
        }];
        let host_task = tokio::spawn(async move {
            accept_one(&endpoint, &identity, vec![CodecId::H264], &authorized).await
        });
        let auth = ClientAuth {
            code: None,
            keys: KeyChoice::Direct(DirectKey {
                public_key,
                sign: Box::new(move |payload| Ok(other.sign(payload))),
            }),
        };
        match connect(addr, &auth, None, vec![CodecId::H264], None).await {
            Err(err) => assert!(err.is_fatal() && err.to_string().contains("rejected"), "{err}"),
            Ok(_) => panic!("a key the host does not hold was accepted"),
        }
        match host_task.await.expect("host task") {
            Err(err) => assert!(err.to_string().contains("ssh key rejected"), "{err}"),
            Ok(_) => panic!("host accepted an unauthorized key"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pairing_code_still_works_when_ssh_keys_are_authorized() {
        let key = sshkey::TestKey::generate("laptop");
        let identity = Identity::generate().unwrap();
        let endpoint = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let code = identity.code.clone();
        let authorized = vec![AuthorizedKey {
            blob: key.public_blob,
            comment: key.comment,
        }];
        let host_task = tokio::spawn(async move {
            accept_one(&endpoint, &identity, vec![CodecId::H264], &authorized).await
        });
        let auth = ClientAuth {
            code: Some(code),
            keys: KeyChoice::PairingCodeOnly,
        };
        connect(addr, &auth, None, vec![CodecId::H264], None).await.unwrap();
        let host = host_task.await.unwrap().unwrap();
        assert!(matches!(host.viewer, ViewerProof::PairingCode));
        assert_eq!(host.resolution, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_viewer_tells_the_host_its_resolution() {
        let identity = Identity::generate().unwrap();
        let endpoint = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let code = identity.code.clone();
        let host_task = tokio::spawn(async move {
            accept_one(&endpoint, &identity, vec![CodecId::H264], &[]).await
        });
        let auth = ClientAuth {
            code: Some(code),
            keys: KeyChoice::PairingCodeOnly,
        };
        let client = connect(addr, &auth, None, vec![CodecId::H264], Some((1280, 720)))
            .await
            .unwrap();
        let host = host_task.await.unwrap().unwrap();
        assert_eq!(host.resolution, Some((1280, 720)));
        drop(client);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_host_accepts_another_viewer_after_the_first_disconnects() {
        let identity = Identity::generate().unwrap();
        let endpoint = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let code = identity.code.clone();
        let host_task = tokio::spawn(async move {
            let mut first = accept_one(&endpoint, &identity, vec![CodecId::H264], &[])
                .await
                .unwrap();
            let left = tokio::time::timeout(Duration::from_secs(3), async {
                while first.from_peer.recv().await.is_some() {}
            })
            .await;
            assert!(left.is_ok(), "first viewer did not disconnect");
            drop(first);
            accept_one(&endpoint, &identity, vec![CodecId::H264], &[])
                .await
                .unwrap()
        });

        let first = connect(
            addr,
            &ClientAuth { code: Some(code.clone()), keys: KeyChoice::PairingCodeOnly },
            None,
            vec![CodecId::H264],
            None,
        )
        .await
        .unwrap();
        drop(first);

        let second = connect(
            addr,
            &ClientAuth { code: Some(code), keys: KeyChoice::PairingCodeOnly },
            None,
            vec![CodecId::H264],
            None,
        )
        .await
        .unwrap();
        let host = host_task.await.unwrap();
        host.send_frame(VideoFrame {
            id: 1,
            keyframe: true,
            codec: CodecId::H264,
            capture_us: 1,
            encode_us: 1,
            payload: b"again".to_vec(),
        });
        let frame = second.frames.recv_timeout(Duration::from_secs(3)).expect("frame");
        assert_eq!(frame.payload, b"again");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_viewers_receive_a_frame_at_the_same_time() {
        let identity = Identity::generate().unwrap();
        let endpoint = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let code = identity.code.clone();
        let host_task = tokio::spawn(async move {
            let first = accept_one(&endpoint, &identity, vec![CodecId::H264], &[])
                .await
                .unwrap();
            let second = accept_one(&endpoint, &identity, vec![CodecId::H264], &[])
                .await
                .unwrap();
            (first, second)
        });

        let client_a = connect(
            addr,
            &ClientAuth { code: Some(code.clone()), keys: KeyChoice::PairingCodeOnly },
            None,
            vec![CodecId::H264],
            None,
        )
        .await
        .unwrap();
        let client_b = connect(
            addr,
            &ClientAuth { code: Some(code), keys: KeyChoice::PairingCodeOnly },
            None,
            vec![CodecId::H264],
            None,
        )
        .await
        .unwrap();
        let clients = [client_a, client_b];
        let (host_a, host_b) = host_task.await.unwrap();
        let frame = VideoFrame {
            id: 7,
            keyframe: true,
            codec: CodecId::H264,
            capture_us: 4,
            encode_us: 8,
            payload: b"shared".to_vec(),
        };
        host_a.send_frame(frame.clone());
        host_b.send_frame(frame);
        for client in &clients {
            let got = client.frames.recv_timeout(Duration::from_secs(3)).expect("frame");
            assert_eq!(got.id, 7);
            assert_eq!(got.payload, b"shared");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn frames_arrive_in_order_and_input_still_arrives() {
        let identity = Identity::generate().unwrap();
        let endpoint = listen("127.0.0.1:0".parse().unwrap(), &identity).unwrap();
        let addr = endpoint.local_addr().unwrap();
        let code = identity.code.clone();
        let host_task = tokio::spawn(async move {
            accept_one(&endpoint, &identity, vec![CodecId::H264], &[]).await
        });
        let auth = ClientAuth {
            code: Some(code),
            keys: KeyChoice::PairingCodeOnly,
        };
        let client = connect(addr, &auth, None, vec![CodecId::H264], None).await.unwrap();
        let mut host = host_task.await.unwrap().unwrap();

        for id in 1..=3 {
            host.send_frame(VideoFrame {
                id,
                keyframe: id == 1,
                codec: CodecId::H264,
                capture_us: id,
                encode_us: 1,
                payload: id.to_string().into_bytes(),
            });
            let frame = client.frames.recv_timeout(Duration::from_secs(3)).expect("frame");
            assert_eq!(frame.id, id);
            assert_eq!(frame.payload, id.to_string().into_bytes());
        }

        client.send_control(Control::Input(Input::MouseMove { x: 8, y: 9 }));
        let input = wait_control(&mut host.from_peer).await;
        assert_eq!(input, Control::Input(Input::MouseMove { x: 8, y: 9 }));
    }

    async fn wait_control(rx: &mut mpsc::UnboundedReceiver<Control>) -> Control {
        tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("timeout")
            .expect("control message")
    }
}
