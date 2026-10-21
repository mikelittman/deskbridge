//! Re-export of the shared frame pacer. The macOS host and the other platforms
//! use the same rules.

pub use deskbridge_session::pace::{changed_fraction, encode_fps, picture_size, FramePace};
