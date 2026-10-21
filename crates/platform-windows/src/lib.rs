//! Windows screen capture, encode, present, input, and system audio.
//!
//! Capture uses Windows.Graphics.Capture. Picture uses Media Foundation
//! hardware H.264 or HEVC. Sound is a WASAPI loopback of what the machine plays.

#[cfg(target_os = "windows")]
mod audio;
#[cfg(target_os = "windows")]
mod capture;
#[cfg(target_os = "windows")]
mod codec;
#[cfg(target_os = "windows")]
mod decode;
#[cfg(target_os = "windows")]
mod encode;
#[cfg(target_os = "windows")]
mod error;
#[cfg(target_os = "windows")]
mod host;
#[cfg(target_os = "windows")]
mod input;
#[cfg(target_os = "windows")]
mod present;

#[cfg(target_os = "windows")]
pub use audio::AudioPlayer;
#[cfg(target_os = "windows")]
pub use codec::{hardware_decode_codecs, hardware_encode_codecs, require_encode_codec};
#[cfg(target_os = "windows")]
pub use decode::{Decoder, Picture};
#[cfg(target_os = "windows")]
pub use error::PlatformError;
#[cfg(target_os = "windows")]
pub use host::{Host, HostOptions, Outbound};
#[cfg(target_os = "windows")]
pub use present::{CursorDraw, Renderer};

#[cfg(target_os = "windows")]
pub fn ensure_host_permissions(inject: bool) -> Result<(), PlatformError> {
    host::prepare(inject)
}

#[cfg(not(target_os = "windows"))]
mod other;

#[cfg(not(target_os = "windows"))]
pub use other::*;
