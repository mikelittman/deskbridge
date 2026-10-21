//! Key codes on the wire are macOS virtual key codes (`CGKeyCode`).
//! Every viewer sends those, and each host translates them to its own keys.

use winit::keyboard::KeyCode;

pub fn mac_key(key: KeyCode) -> Option<u16> {
    Some(match key {
        KeyCode::KeyA => 0x00,
        KeyCode::KeyS => 0x01,
        KeyCode::KeyD => 0x02,
        KeyCode::KeyF => 0x03,
        KeyCode::KeyH => 0x04,
        KeyCode::KeyG => 0x05,
        KeyCode::KeyZ => 0x06,
        KeyCode::KeyX => 0x07,
        KeyCode::KeyC => 0x08,
        KeyCode::KeyV => 0x09,
        KeyCode::KeyB => 0x0B,
        KeyCode::KeyQ => 0x0C,
        KeyCode::KeyW => 0x0D,
        KeyCode::KeyE => 0x0E,
        KeyCode::KeyR => 0x0F,
        KeyCode::KeyY => 0x10,
        KeyCode::KeyT => 0x11,
        KeyCode::Digit1 => 0x12,
        KeyCode::Digit2 => 0x13,
        KeyCode::Digit3 => 0x14,
        KeyCode::Digit4 => 0x15,
        KeyCode::Digit6 => 0x16,
        KeyCode::Digit5 => 0x17,
        KeyCode::Equal => 0x18,
        KeyCode::Digit9 => 0x19,
        KeyCode::Digit7 => 0x1A,
        KeyCode::Minus => 0x1B,
        KeyCode::Digit8 => 0x1C,
        KeyCode::Digit0 => 0x1D,
        KeyCode::BracketRight => 0x1E,
        KeyCode::KeyO => 0x1F,
        KeyCode::KeyU => 0x20,
        KeyCode::BracketLeft => 0x21,
        KeyCode::KeyI => 0x22,
        KeyCode::KeyP => 0x23,
        KeyCode::Enter => 0x24,
        KeyCode::KeyL => 0x25,
        KeyCode::KeyJ => 0x26,
        KeyCode::Quote => 0x27,
        KeyCode::KeyK => 0x28,
        KeyCode::Semicolon => 0x29,
        KeyCode::Backslash => 0x2A,
        KeyCode::Comma => 0x2B,
        KeyCode::Slash => 0x2C,
        KeyCode::KeyN => 0x2D,
        KeyCode::KeyM => 0x2E,
        KeyCode::Period => 0x2F,
        KeyCode::Tab => 0x30,
        KeyCode::Space => 0x31,
        KeyCode::Backquote => 0x32,
        KeyCode::Backspace => 0x33,
        KeyCode::Escape => 0x35,
        KeyCode::SuperLeft | KeyCode::Meta => 0x37,
        KeyCode::ShiftLeft => 0x38,
        KeyCode::CapsLock => 0x39,
        KeyCode::AltLeft => 0x3A,
        KeyCode::ControlLeft => 0x3B,
        KeyCode::ShiftRight => 0x3C,
        KeyCode::AltRight => 0x3D,
        KeyCode::ControlRight => 0x3E,
        KeyCode::SuperRight => 0x36,
        KeyCode::F1 => 0x7A,
        KeyCode::F2 => 0x78,
        KeyCode::F3 => 0x63,
        KeyCode::F4 => 0x76,
        KeyCode::F5 => 0x60,
        KeyCode::F6 => 0x61,
        KeyCode::F7 => 0x62,
        KeyCode::F8 => 0x64,
        KeyCode::F9 => 0x65,
        KeyCode::F10 => 0x6D,
        KeyCode::F11 => 0x67,
        KeyCode::F12 => 0x6F,
        KeyCode::ArrowLeft => 0x7B,
        KeyCode::ArrowRight => 0x7C,
        KeyCode::ArrowDown => 0x7D,
        KeyCode::ArrowUp => 0x7E,
        KeyCode::Home => 0x73,
        KeyCode::End => 0x77,
        KeyCode::PageUp => 0x74,
        KeyCode::PageDown => 0x79,
        KeyCode::Delete => 0x75,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_use_the_mac_virtual_key_codes() {
        assert_eq!(mac_key(KeyCode::KeyA), Some(0x00));
        assert_eq!(mac_key(KeyCode::Enter), Some(0x24));
        assert_eq!(mac_key(KeyCode::Space), Some(0x31));
        assert_eq!(mac_key(KeyCode::ArrowLeft), Some(0x7B));
    }
}
