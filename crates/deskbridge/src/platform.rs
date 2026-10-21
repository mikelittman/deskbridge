//! The host and viewer backend for this operating system.

#[cfg(target_os = "macos")]
pub use deskbridge_platform_macos::*;
#[cfg(target_os = "linux")]
pub use deskbridge_platform_linux::*;
#[cfg(target_os = "windows")]
pub use deskbridge_platform_windows::*;
