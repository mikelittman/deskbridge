use std::ffi::c_void;

use apple_cf::cf::{CFDictionary, CFString, CFType};
use apple_cf::raw::kCFBooleanTrue;

use crate::error::PlatformError;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
}

/// Check the permissions the host needs, and ask macOS to show the system prompt
/// when one is missing. A grant does not apply to this process, so a missing
/// permission is a startup error and the host must be launched again.
pub fn ensure_host_permissions(inject: bool) -> Result<(), PlatformError> {
    let mut screen = screen_recording_active();
    if !screen {
        // Shows the Screen Recording prompt. The result stays false until the next launch.
        screen = unsafe { CGRequestScreenCaptureAccess() };
    }
    let accessibility = !inject || accessibility_active(true);
    if screen && accessibility {
        return Ok(());
    }
    Err(PlatformError::new(restart_message(!screen, !accessibility)))
}

fn screen_recording_active() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

fn accessibility_active(prompt: bool) -> bool {
    unsafe {
        if !prompt {
            return AXIsProcessTrustedWithOptions(std::ptr::null()) != 0;
        }
        let key = CFString::new("AXTrustedCheckOptionPrompt");
        let value = CFType::from_raw_borrowed(kCFBooleanTrue.cast_mut().cast())
            .expect("kCFBooleanTrue");
        let options = CFDictionary::from_pairs(&[(&key, &value)]);
        AXIsProcessTrustedWithOptions(options.as_ptr()) != 0
    }
}

fn restart_message(screen: bool, accessibility: bool) -> String {
    let (what, setting) = match (screen, accessibility) {
        (true, true) => (
            "Screen Recording and Accessibility are off for this process.",
            "Turn both on for the terminal that launches deskbridge in System Settings → Privacy & Security",
        ),
        (true, false) => (
            "Screen Recording is off for this process.",
            "Turn it on for the terminal that launches deskbridge in System Settings → Privacy & Security → Screen Recording",
        ),
        (false, true) => (
            "Accessibility is off for this process.",
            "Turn it on for the terminal that launches deskbridge in System Settings → Privacy & Security → Accessibility",
        ),
        (false, false) => unreachable!("restart_message is only used when a permission is missing"),
    };
    format!("{what} {setting}, then start deskbridge again. macOS applies the change on the next launch.")
}
