//! Compiled on other operating systems so the workspace still builds.

pub const HOST_UNAVAILABLE: &str = "Windows screen capture is not in this build yet. The host backend will use Windows.Graphics.Capture and Media Foundation.";
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
