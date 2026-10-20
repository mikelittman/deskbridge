//! Windows screen capture, encode, and present.
//!
//! The 64-bit binary already speaks the Deskbridge protocol. The picture path
//! is the next piece: Windows.Graphics.Capture for the screen, and Media
//! Foundation for hardware encode and decode. This crate is where that code lands.

/// Shown when `deskbridge host` runs on Windows before capture exists.
pub const HOST_UNAVAILABLE: &str = "Windows screen capture is not in this build yet. The host backend will use Windows.Graphics.Capture and Media Foundation.";

/// Shown when `deskbridge view` runs on Windows before the window exists.
pub const VIEWER_UNAVAILABLE: &str = "the Windows viewer window is not in this build yet";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_messages_name_the_windows_backend() {
        assert!(HOST_UNAVAILABLE.contains("Windows.Graphics.Capture"));
        assert!(VIEWER_UNAVAILABLE.contains("Windows"));
    }
}
