//! Linux screen capture, encode, present, input, and system audio.
//!
//! On a desktop this uses the XDG remote-desktop portal and PipeWire, with
//! VA-API for hardware H.264 or HEVC. X11 is the fallback when the portal is
//! not there.

#[cfg(target_os = "linux")]
mod audio;
#[cfg(target_os = "linux")]
mod capture;
#[cfg(target_os = "linux")]
mod codec;
#[cfg(target_os = "linux")]
mod decode;
#[cfg(target_os = "linux")]
mod encode;
#[cfg(target_os = "linux")]
mod error;
#[cfg(target_os = "linux")]
mod host;
#[cfg(target_os = "linux")]
mod input;
#[cfg(target_os = "linux")]
mod present;

#[cfg(target_os = "linux")]
pub use audio::AudioPlayer;
#[cfg(target_os = "linux")]
pub use codec::{hardware_decode_codecs, hardware_encode_codecs, require_encode_codec};
#[cfg(target_os = "linux")]
pub use decode::{Decoder, Picture};
#[cfg(target_os = "linux")]
pub use error::PlatformError;
#[cfg(target_os = "linux")]
pub use host::{Host, HostOptions, Outbound};
#[cfg(target_os = "linux")]
pub use present::{CursorDraw, Renderer};

#[cfg(target_os = "linux")]
pub fn ensure_host_permissions(inject: bool) -> Result<(), PlatformError> {
    host::prepare(inject)
}

#[cfg(not(target_os = "linux"))]
mod other;

#[cfg(not(target_os = "linux"))]
pub use other::*;
