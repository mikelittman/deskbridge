//! Linux screen capture, encode, and present.
//!
//! The 64-bit binary already speaks the Deskbridge protocol. The picture path
//! is the next piece: PipeWire (or X11) for the screen, and VAAPI or OpenH264
//! for the encoded frames. This crate is where that code lands.

/// Shown when `deskbridge host` runs on Linux before capture exists.
pub const HOST_UNAVAILABLE: &str = "Linux screen capture is not in this build yet. The host backend will use PipeWire or X11, with VAAPI or OpenH264.";

/// Shown when `deskbridge view` runs on Linux before the window exists.
pub const VIEWER_UNAVAILABLE: &str = "the Linux viewer window is not in this build yet";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_messages_name_the_linux_backend() {
        assert!(HOST_UNAVAILABLE.contains("PipeWire"));
        assert!(VIEWER_UNAVAILABLE.contains("Linux"));
    }
}
