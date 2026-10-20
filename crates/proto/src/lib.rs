//! Deskbridge control messages and video-frame headers.
//!
//! Little-endian, length-prefixed, no external serializer. A future Linux or
//! Windows peer speaks these same bytes.

use std::fmt;

/// Protocol version spoken by this crate.
pub const PROTOCOL_VERSION: u8 = 2;

/// Largest accepted control-message payload.
pub const MAX_CONTROL_PAYLOAD: usize = 8 * 1024 * 1024;

/// Magic at the start of a video frame stream.
pub const FRAME_MAGIC: &[u8; 4] = b"DBF1";

/// Bytes in a video frame header, excluding the payload.
pub const FRAME_HEADER_LEN: usize = 4 + 8 + 1 + 1 + 8 + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    H264 = 1,
    Hevc = 2,
}

impl CodecId {
    pub fn from_u8(value: u8) -> Result<Self, ProtoError> {
        match value {
            1 => Ok(Self::H264),
            2 => Ok(Self::Hevc),
            _ => Err(ProtoError::BadCodec(value)),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::H264 => "H.264",
            Self::Hevc => "HEVC",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum MsgType {
    Hello = 1,
    HelloAck = 2,
    Pair = 3,
    PairOk = 4,
    PairReject = 5,
    DisplayInfo = 6,
    CodecConfig = 7,
    CursorShape = 8,
    CursorPos = 9,
    Input = 10,
    KeyframeRequest = 11,
    Ping = 12,
    Pong = 13,
    Authenticate = 14,
}

impl MsgType {
    fn from_u8(value: u8) -> Result<Self, ProtoError> {
        Ok(match value {
            1 => Self::Hello,
            2 => Self::HelloAck,
            3 => Self::Pair,
            4 => Self::PairOk,
            5 => Self::PairReject,
            6 => Self::DisplayInfo,
            7 => Self::CodecConfig,
            8 => Self::CursorShape,
            9 => Self::CursorPos,
            10 => Self::Input,
            11 => Self::KeyframeRequest,
            12 => Self::Ping,
            13 => Self::Pong,
            14 => Self::Authenticate,
            _ => return Err(ProtoError::BadType(value)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub version: u8,
    /// Codecs this peer can decode.
    pub codecs: Vec<CodecId>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelloAck {
    pub version: u8,
    /// Codecs this host can encode.
    pub codecs: Vec<CodecId>,
    pub fingerprint: [u8; 32],
    /// Fresh nonce the viewer signs with its SSH private key.
    pub nonce: [u8; 32],
    /// OpenSSH wire blobs for the ssh-ed25519 keys this host accepts.
    pub authorized_keys: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayInfo {
    pub width: u32,
    pub height: u32,
    /// Refresh rate in millihertz, for example 60_000 for 60 Hz. Zero if unknown.
    pub refresh_millihz: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    pub codec: CodecId,
    pub width: u32,
    pub height: u32,
    /// NAL length field size. VideoToolbox uses 4.
    pub nal_length_size: u8,
    /// Parameter sets (VPS, SPS, PPS) without start codes.
    pub parameter_sets: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    /// Tightly packed BGRA, `width * height * 4` bytes.
    pub bgra: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left = 0,
    Right = 1,
    Middle = 2,
}

impl MouseButton {
    fn from_u8(value: u8) -> Result<Self, ProtoError> {
        match value {
            0 => Ok(Self::Left),
            1 => Ok(Self::Right),
            2 => Ok(Self::Middle),
            _ => Err(ProtoError::BadButton(value)),
        }
    }
}

/// Modifier bits carried with a key event.
pub mod mods {
    pub const SHIFT: u8 = 1 << 0;
    pub const CTRL: u8 = 1 << 1;
    pub const ALT: u8 = 1 << 2;
    pub const META: u8 = 1 << 3;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    MouseMove { x: i32, y: i32 },
    MouseButton { button: MouseButton, down: bool },
    Wheel { dx: i32, dy: i32 },
    /// `code` is a macOS virtual key code (`CGKeyCode`).
    Key { code: u16, down: bool, modifiers: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Hello(Hello),
    HelloAck(HelloAck),
    Pair { code: String },
    PairOk,
    PairReject,
    /// Proof that the viewer holds the private key for `public_key`.
    Authenticate { public_key: Vec<u8>, signature: Vec<u8> },
    DisplayInfo(DisplayInfo),
    CodecConfig(CodecConfig),
    CursorShape(CursorShape),
    CursorPos(CursorPos),
    Input(Input),
    KeyframeRequest { newest_seen: u64 },
    Ping { client_us: u64 },
    Pong { client_us: u64, host_us: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub id: u64,
    pub keyframe: bool,
    pub codec: CodecId,
    pub capture_us: u64,
    pub encode_us: u32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    Truncated,
    BadType(u8),
    BadCodec(u8),
    BadButton(u8),
    BadLength,
    TooLarge(usize),
    BadUtf8,
    BadMagic,
    BadCursor,
    Trailing,
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "truncated deskbridge message"),
            Self::BadType(v) => write!(f, "unknown message type {v}"),
            Self::BadCodec(v) => write!(f, "unknown codec id {v}"),
            Self::BadButton(v) => write!(f, "unknown mouse button {v}"),
            Self::BadLength => write!(f, "length does not match the payload"),
            Self::TooLarge(n) => write!(f, "message of {n} bytes exceeds the cap"),
            Self::BadUtf8 => write!(f, "string was not utf-8"),
            Self::BadMagic => write!(f, "video frame magic was not DBF1"),
            Self::BadCursor => write!(f, "cursor bitmap size does not match its dimensions"),
            Self::Trailing => write!(f, "bytes left over after a message"),
        }
    }
}

impl std::error::Error for ProtoError {}

struct W<'a> {
    buf: &'a mut Vec<u8>,
}

impl W<'_> {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }
    fn blob(&mut self, bytes: &[u8]) -> Result<(), ProtoError> {
        let len = u32::try_from(bytes.len()).map_err(|_| ProtoError::TooLarge(bytes.len()))?;
        self.u32(len);
        self.buf.extend_from_slice(bytes);
        Ok(())
    }
    fn str(&mut self, value: &str) -> Result<(), ProtoError> {
        self.blob(value.as_bytes())
    }
    fn codecs(&mut self, codecs: &[CodecId]) -> Result<(), ProtoError> {
        let n = u8::try_from(codecs.len()).map_err(|_| ProtoError::TooLarge(codecs.len()))?;
        self.u8(n);
        for codec in codecs {
            self.u8(*codec as u8);
        }
        Ok(())
    }
    fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }
}

struct R<'a> {
    buf: &'a [u8],
    i: usize,
}

impl<'a> R<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, i: 0 }
    }
    fn rest(&self) -> usize {
        self.buf.len().saturating_sub(self.i)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if self.rest() < n {
            return Err(ProtoError::Truncated);
        }
        let start = self.i;
        self.i += n;
        Ok(&self.buf[start..self.i])
    }
    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ProtoError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, ProtoError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, ProtoError> {
        Ok(self.u32()? as i32)
    }
    fn blob(&mut self, cap: usize) -> Result<&'a [u8], ProtoError> {
        let len = self.u32()? as usize;
        if len > cap {
            return Err(ProtoError::TooLarge(len));
        }
        self.take(len)
    }
    fn string(&mut self, cap: usize) -> Result<String, ProtoError> {
        let bytes = self.blob(cap)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| ProtoError::BadUtf8)
    }
    fn codecs(&mut self) -> Result<Vec<CodecId>, ProtoError> {
        let n = self.u8()? as usize;
        if n > 8 {
            return Err(ProtoError::TooLarge(n));
        }
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(CodecId::from_u8(self.u8()?)?);
        }
        Ok(out)
    }
    fn finish(self) -> Result<(), ProtoError> {
        if self.i != self.buf.len() {
            Err(ProtoError::Trailing)
        } else {
            Ok(())
        }
    }
}

impl Control {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut payload = Vec::new();
        let msg_type = self.write_payload(&mut payload)?;
        if payload.len() > MAX_CONTROL_PAYLOAD {
            return Err(ProtoError::TooLarge(payload.len()));
        }
        let mut out = Vec::with_capacity(5 + payload.len());
        out.push(msg_type as u8);
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&payload);
        Ok(out)
    }

    fn write_payload(&self, buf: &mut Vec<u8>) -> Result<MsgType, ProtoError> {
        let mut w = W { buf };
        let kind = match self {
            Self::Hello(hello) => {
                w.u8(hello.version);
                w.codecs(&hello.codecs)?;
                w.str(&hello.name)?;
                MsgType::Hello
            }
            Self::HelloAck(ack) => {
                w.u8(ack.version);
                w.codecs(&ack.codecs)?;
                w.raw(&ack.fingerprint);
                w.raw(&ack.nonce);
                if ack.authorized_keys.len() > 32 {
                    return Err(ProtoError::TooLarge(ack.authorized_keys.len()));
                }
                w.u8(ack.authorized_keys.len() as u8);
                for key in &ack.authorized_keys {
                    w.blob(key)?;
                }
                MsgType::HelloAck
            }
            Self::Pair { code } => {
                w.str(code)?;
                MsgType::Pair
            }
            Self::PairOk => MsgType::PairOk,
            Self::PairReject => MsgType::PairReject,
            Self::Authenticate { public_key, signature } => {
                w.blob(public_key)?;
                w.blob(signature)?;
                MsgType::Authenticate
            }
            Self::DisplayInfo(info) => {
                w.u32(info.width);
                w.u32(info.height);
                w.u32(info.refresh_millihz);
                MsgType::DisplayInfo
            }
            Self::CodecConfig(cfg) => {
                w.u8(cfg.codec as u8);
                w.u32(cfg.width);
                w.u32(cfg.height);
                w.u8(cfg.nal_length_size);
                let n = u16::try_from(cfg.parameter_sets.len())
                    .map_err(|_| ProtoError::TooLarge(cfg.parameter_sets.len()))?;
                w.u16(n);
                for set in &cfg.parameter_sets {
                    w.blob(set)?;
                }
                MsgType::CodecConfig
            }
            Self::CursorShape(cursor) => {
                let expect = cursor.width as usize * cursor.height as usize * 4;
                if cursor.bgra.len() != expect {
                    return Err(ProtoError::BadCursor);
                }
                w.u16(cursor.width);
                w.u16(cursor.height);
                w.u16(cursor.hotspot_x);
                w.u16(cursor.hotspot_y);
                w.blob(&cursor.bgra)?;
                MsgType::CursorShape
            }
            Self::CursorPos(pos) => {
                w.i32(pos.x);
                w.i32(pos.y);
                w.u8(u8::from(pos.visible));
                MsgType::CursorPos
            }
            Self::Input(input) => {
                match input {
                    Input::MouseMove { x, y } => {
                        w.u8(1);
                        w.i32(*x);
                        w.i32(*y);
                    }
                    Input::MouseButton { button, down } => {
                        w.u8(2);
                        w.u8(*button as u8);
                        w.u8(u8::from(*down));
                    }
                    Input::Wheel { dx, dy } => {
                        w.u8(3);
                        w.i32(*dx);
                        w.i32(*dy);
                    }
                    Input::Key { code, down, modifiers } => {
                        w.u8(4);
                        w.u16(*code);
                        w.u8(u8::from(*down));
                        w.u8(*modifiers);
                    }
                }
                MsgType::Input
            }
            Self::KeyframeRequest { newest_seen } => {
                w.u64(*newest_seen);
                MsgType::KeyframeRequest
            }
            Self::Ping { client_us } => {
                w.u64(*client_us);
                MsgType::Ping
            }
            Self::Pong { client_us, host_us } => {
                w.u64(*client_us);
                w.u64(*host_us);
                MsgType::Pong
            }
        };
        Ok(kind)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        if bytes.len() < 5 {
            return Err(ProtoError::Truncated);
        }
        let kind = MsgType::from_u8(bytes[0])?;
        let len = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
        if len > MAX_CONTROL_PAYLOAD {
            return Err(ProtoError::TooLarge(len));
        }
        if bytes.len() != 5 + len {
            return Err(ProtoError::BadLength);
        }
        Self::decode_payload(kind, &bytes[5..])
    }

    fn decode_payload(kind: MsgType, payload: &[u8]) -> Result<Self, ProtoError> {
        let mut r = R::new(payload);
        let msg = match kind {
            MsgType::Hello => Self::Hello(Hello {
                version: r.u8()?,
                codecs: r.codecs()?,
                name: r.string(256)?,
            }),
            MsgType::HelloAck => {
                let version = r.u8()?;
                let codecs = r.codecs()?;
                let fp = r.take(32)?;
                let mut fingerprint = [0u8; 32];
                fingerprint.copy_from_slice(fp);
                let raw_nonce = r.take(32)?;
                let mut nonce = [0u8; 32];
                nonce.copy_from_slice(raw_nonce);
                let key_count = r.u8()? as usize;
                if key_count > 32 {
                    return Err(ProtoError::TooLarge(key_count));
                }
                let mut authorized_keys = Vec::with_capacity(key_count);
                for _ in 0..key_count {
                    authorized_keys.push(r.blob(1024)?.to_vec());
                }
                Self::HelloAck(HelloAck {
                    version,
                    codecs,
                    fingerprint,
                    nonce,
                    authorized_keys,
                })
            }
            MsgType::Pair => Self::Pair { code: r.string(64)? },
            MsgType::PairOk => Self::PairOk,
            MsgType::PairReject => Self::PairReject,
            MsgType::Authenticate => Self::Authenticate {
                public_key: r.blob(1024)?.to_vec(),
                signature: r.blob(512)?.to_vec(),
            },
            MsgType::DisplayInfo => Self::DisplayInfo(DisplayInfo {
                width: r.u32()?,
                height: r.u32()?,
                refresh_millihz: r.u32()?,
            }),
            MsgType::CodecConfig => {
                let codec = CodecId::from_u8(r.u8()?)?;
                let width = r.u32()?;
                let height = r.u32()?;
                let nal_length_size = r.u8()?;
                let n = r.u16()? as usize;
                if n > 16 {
                    return Err(ProtoError::TooLarge(n));
                }
                let mut parameter_sets = Vec::with_capacity(n);
                for _ in 0..n {
                    parameter_sets.push(r.blob(64 * 1024)?.to_vec());
                }
                Self::CodecConfig(CodecConfig {
                    codec,
                    width,
                    height,
                    nal_length_size,
                    parameter_sets,
                })
            }
            MsgType::CursorShape => {
                let width = r.u16()?;
                let height = r.u16()?;
                let hotspot_x = r.u16()?;
                let hotspot_y = r.u16()?;
                let bgra = r.blob(MAX_CONTROL_PAYLOAD)?.to_vec();
                let expect = width as usize * height as usize * 4;
                if bgra.len() != expect || width > 512 || height > 512 {
                    return Err(ProtoError::BadCursor);
                }
                Self::CursorShape(CursorShape {
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    bgra,
                })
            }
            MsgType::CursorPos => Self::CursorPos(CursorPos {
                x: r.i32()?,
                y: r.i32()?,
                visible: r.u8()? != 0,
            }),
            MsgType::Input => {
                let kind = r.u8()?;
                let input = match kind {
                    1 => Input::MouseMove { x: r.i32()?, y: r.i32()? },
                    2 => Input::MouseButton {
                        button: MouseButton::from_u8(r.u8()?)?,
                        down: r.u8()? != 0,
                    },
                    3 => Input::Wheel { dx: r.i32()?, dy: r.i32()? },
                    4 => Input::Key {
                        code: r.u16()?,
                        down: r.u8()? != 0,
                        modifiers: r.u8()?,
                    },
                    _ => return Err(ProtoError::BadType(kind)),
                };
                Self::Input(input)
            }
            MsgType::KeyframeRequest => Self::KeyframeRequest { newest_seen: r.u64()? },
            MsgType::Ping => Self::Ping { client_us: r.u64()? },
            MsgType::Pong => Self::Pong {
                client_us: r.u64()?,
                host_us: r.u64()?,
            },
        };
        r.finish()?;
        Ok(msg)
    }
}

impl VideoFrame {
    pub fn header_bytes(&self) -> [u8; FRAME_HEADER_LEN] {
        let mut header = [0u8; FRAME_HEADER_LEN];
        header[0..4].copy_from_slice(FRAME_MAGIC);
        header[4..12].copy_from_slice(&self.id.to_le_bytes());
        header[12] = u8::from(self.keyframe);
        header[13] = self.codec as u8;
        header[14..22].copy_from_slice(&self.capture_us.to_le_bytes());
        header[22..26].copy_from_slice(&self.encode_us.to_le_bytes());
        header
    }

    pub fn decode_header(header: &[u8; FRAME_HEADER_LEN]) -> Result<(Self, ()), ProtoError> {
        if &header[0..4] != FRAME_MAGIC {
            return Err(ProtoError::BadMagic);
        }
        let id = u64::from_le_bytes(header[4..12].try_into().unwrap());
        let keyframe = header[12] != 0;
        let codec = CodecId::from_u8(header[13])?;
        let capture_us = u64::from_le_bytes(header[14..22].try_into().unwrap());
        let encode_us = u32::from_le_bytes(header[22..26].try_into().unwrap());
        Ok((
            Self {
                id,
                keyframe,
                codec,
                capture_us,
                encode_us,
                payload: Vec::new(),
            },
            (),
        ))
    }
}

/// Pulls length-prefixed control messages out of a byte stream.
#[derive(Debug, Default)]
pub struct Framer {
    buf: Vec<u8>,
}

impl Framer {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, data: &[u8]) -> Result<Vec<Control>, ProtoError> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        loop {
            if self.buf.len() < 5 {
                break;
            }
            let len = u32::from_le_bytes(self.buf[1..5].try_into().unwrap()) as usize;
            if len > MAX_CONTROL_PAYLOAD {
                return Err(ProtoError::TooLarge(len));
            }
            let total = 5 + len;
            if self.buf.len() < total {
                break;
            }
            let msg = Control::decode(&self.buf[..total])?;
            self.buf.drain(..total);
            out.push(msg);
        }
        Ok(out)
    }
}

/// Pick the best codec both sides can do. HEVC wins when it is on both lists.
pub fn negotiate_codec(host_encode: &[CodecId], client_decode: &[CodecId]) -> Option<CodecId> {
    if host_encode.contains(&CodecId::Hevc) && client_decode.contains(&CodecId::Hevc) {
        Some(CodecId::Hevc)
    } else if host_encode.contains(&CodecId::H264) && client_decode.contains(&CodecId::H264) {
        Some(CodecId::H264)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(msg: Control) {
        let bytes = msg.encode().unwrap();
        let decoded = Control::decode(&bytes).unwrap();
        assert_eq!(decoded, msg);
        let mut framer = Framer::new();
        let mut got = Vec::new();
        for chunk in bytes.chunks(3) {
            got.extend(framer.push(chunk).unwrap());
        }
        assert_eq!(got, vec![msg]);
    }

    #[test]
    fn every_message_roundtrips() {
        roundtrip(Control::Hello(Hello {
            version: 1,
            codecs: vec![CodecId::Hevc, CodecId::H264],
            name: "view".into(),
        }));
        let mut fingerprint = [0u8; 32];
        fingerprint[0] = 0xab;
        fingerprint[31] = 0xcd;
        let mut nonce = [0u8; 32];
        nonce[0] = 0x11;
        nonce[31] = 0x22;
        roundtrip(Control::HelloAck(HelloAck {
            version: 2,
            codecs: vec![CodecId::Hevc],
            fingerprint,
            nonce,
            authorized_keys: vec![vec![1, 2, 3, 4]],
        }));
        roundtrip(Control::Pair { code: "K7M2Q9XP".into() });
        roundtrip(Control::PairOk);
        roundtrip(Control::PairReject);
        roundtrip(Control::Authenticate {
            public_key: vec![9, 8, 7],
            signature: vec![6, 5],
        });
        roundtrip(Control::DisplayInfo(DisplayInfo {
            width: 1920,
            height: 1080,
            refresh_millihz: 60_000,
        }));
        roundtrip(Control::CodecConfig(CodecConfig {
            codec: CodecId::Hevc,
            width: 1920,
            height: 1080,
            nal_length_size: 4,
            parameter_sets: vec![vec![0x40, 0x01], vec![0x42, 0x01], vec![0x44, 0x01]],
        }));
        roundtrip(Control::CursorShape(CursorShape {
            width: 2,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            bgra: vec![1, 2, 3, 4, 5, 6, 7, 8],
        }));
        roundtrip(Control::CursorPos(CursorPos { x: -3, y: 40, visible: true }));
        roundtrip(Control::Input(Input::MouseMove { x: 10, y: 20 }));
        roundtrip(Control::Input(Input::MouseButton {
            button: MouseButton::Right,
            down: true,
        }));
        roundtrip(Control::Input(Input::Wheel { dx: -1, dy: 2 }));
        roundtrip(Control::Input(Input::Key {
            code: 0,
            down: false,
            modifiers: mods::CTRL | mods::ALT,
        }));
        roundtrip(Control::KeyframeRequest { newest_seen: 99 });
        roundtrip(Control::Ping { client_us: 1234 });
        roundtrip(Control::Pong { client_us: 1234, host_us: 1300 });
    }

    #[test]
    fn rejects_a_short_header_and_a_huge_length() {
        assert_eq!(Control::decode(&[1, 0, 0]), Err(ProtoError::Truncated));
        let mut huge = vec![1, 0xff, 0xff, 0xff, 0x7f];
        huge.resize(5, 0);
        assert!(matches!(Control::decode(&huge), Err(ProtoError::TooLarge(_))));
    }

    #[test]
    fn video_header_roundtrip() {
        let frame = VideoFrame {
            id: 7,
            keyframe: true,
            codec: CodecId::Hevc,
            capture_us: 99,
            encode_us: 2500,
            payload: vec![1, 2, 3],
        };
        let header = frame.header_bytes();
        let (decoded, _) = VideoFrame::decode_header(&header).unwrap();
        assert_eq!(decoded.id, 7);
        assert!(decoded.keyframe);
        assert_eq!(decoded.codec, CodecId::Hevc);
        assert_eq!(decoded.capture_us, 99);
        assert_eq!(decoded.encode_us, 2500);
    }

    #[test]
    fn negotiates_hevc_ahead_of_h264() {
        assert_eq!(
            negotiate_codec(&[CodecId::H264, CodecId::Hevc], &[CodecId::Hevc, CodecId::H264]),
            Some(CodecId::Hevc)
        );
        assert_eq!(
            negotiate_codec(&[CodecId::H264], &[CodecId::Hevc, CodecId::H264]),
            Some(CodecId::H264)
        );
        assert_eq!(negotiate_codec(&[CodecId::Hevc], &[CodecId::H264]), None);
    }
}
