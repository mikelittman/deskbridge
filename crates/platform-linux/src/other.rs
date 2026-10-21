//! Compiled on other operating systems so the workspace still builds.
//! The Linux desktop path is `cfg(target_os = "linux")`.

pub const HOST_UNAVAILABLE: &str = "Linux screen capture is not in this build yet. The host backend will use PipeWire or X11, with VAAPI or OpenH264.";
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
