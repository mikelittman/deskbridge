//! Pointer and keyboard injection.
//!
//! Wire key codes are macOS virtual key codes. The portal wants an XKB keycode,
//! which is the evdev code plus 8. X11 uses the keysym for the same key.

use std::sync::atomic::{AtomicBool, Ordering};

use deskbridge_proto::{Input, MouseButton};

use crate::capture::{inject_route, InjectEvent, InjectRoute};
use crate::capture::DisplayGeometry;

const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

pub struct Injector {
    geometry: DisplayGeometry,
    route: Option<InjectRoute>,
    x11: Option<X11Input>,
    logged: bool,
    x: i32,
    y: i32,
}

impl Injector {
    pub fn new(geometry: DisplayGeometry) -> Self {
        let route = inject_route();
        let x11 = match &route {
            Some(InjectRoute::X11 { screen }) => X11Input::open(*screen).ok(),
            _ => None,
        };
        Self { geometry, route, x11, logged: false, x: 0, y: 0 }
    }

    pub fn apply(&mut self, input: Input) {
        if self.route.is_none() && self.x11.is_none() {
            self.complain();
            return;
        }
        match input {
            Input::MouseMove { x, y } => {
                self.x = x;
                self.y = y;
                let (gx, gy) = self.geometry.pixel_to_global(x, y);
                self.send(InjectEvent::Move(gx, gy));
            }
            Input::MouseButton { button, down } => {
                let code = match button {
                    MouseButton::Left => BTN_LEFT,
                    MouseButton::Right => BTN_RIGHT,
                    MouseButton::Middle => BTN_MIDDLE,
                };
                self.send(InjectEvent::Button { button: code, down });
            }
            Input::Wheel { dx, dy } => self.send(InjectEvent::Scroll { dx, dy }),
            Input::Key { code, down, .. } => {
                if let Some(evdev) = evdev_from_mac(code) {
                    // The portal keycode is the XKB code: evdev plus 8.
                    self.send(InjectEvent::Key { code: evdev as i32 + 8, down });
                }
            }
        }
    }

    fn send(&mut self, event: InjectEvent) {
        match &self.route {
            Some(InjectRoute::Portal { tx, .. }) => {
                if tx.send(event).is_err() {
                    self.complain();
                }
            }
            Some(InjectRoute::X11 { .. }) => self.send_x11(event),
            None => self.complain(),
        }
    }

    fn send_x11(&mut self, event: InjectEvent) {
        let Some(x11) = self.x11.as_mut() else {
            self.complain();
            return;
        };
        if !x11.apply(&event) {
            self.complain();
        }
    }

    fn complain(&mut self) {
        if !self.logged {
            self.logged = true;
            eprintln!("deskbridge: input injection is unavailable");
        }
    }
}

struct X11Input {
    _libx11: libloading::Library,
    _libxtst: libloading::Library,
    display: *mut std::ffi::c_void,
    screen: i32,
    keysym_to_keycode: unsafe extern "C" fn(*mut std::ffi::c_void, u64) -> u8,
    fake_key: unsafe extern "C" fn(*mut std::ffi::c_void, u32, i32, u64) -> i32,
    fake_button: unsafe extern "C" fn(*mut std::ffi::c_void, u32, i32, u64) -> i32,
    fake_motion: unsafe extern "C" fn(*mut std::ffi::c_void, i32, i32, i32, u64) -> i32,
    flush: unsafe extern "C" fn(*mut std::ffi::c_void) -> i32,
    close_display: unsafe extern "C" fn(*mut std::ffi::c_void) -> i32,
}

unsafe impl Send for X11Input {}

impl X11Input {
    fn open(screen: i32) -> Result<Self, ()> {
        let libx11 = unsafe { libloading::Library::new("libX11.so.6") }.map_err(|_| ())?;
        let libxtst = unsafe { libloading::Library::new("libXtst.so.6") }.map_err(|_| ())?;
        unsafe fn sym<T: Copy>(lib: &libloading::Library, name: &[u8]) -> Result<T, ()> {
            let symbol: libloading::Symbol<T> = unsafe { lib.get(name).map_err(|_| ())? };
            Ok(*symbol)
        }
        let open: unsafe extern "C" fn(*const i8) -> *mut std::ffi::c_void = unsafe { sym(&libx11, b"XOpenDisplay\0") }?;
        let display = unsafe { open(std::ptr::null()) };
        if display.is_null() {
            return Err(());
        }
        Ok(Self {
            keysym_to_keycode: unsafe { sym(&libx11, b"XKeysymToKeycode\0") }?,
            flush: unsafe { sym(&libx11, b"XFlush\0") }?,
            close_display: unsafe { sym(&libx11, b"XCloseDisplay\0") }?,
            fake_key: unsafe { sym(&libxtst, b"XTestFakeKeyEvent\0") }?,
            fake_button: unsafe { sym(&libxtst, b"XTestFakeButtonEvent\0") }?,
            fake_motion: unsafe { sym(&libxtst, b"XTestFakeMotionEvent\0") }?,
            _libx11: libx11,
            _libxtst: libxtst,
            display,
            screen,
        })
    }

    fn apply(&mut self, event: &InjectEvent) -> bool {
        let ok = match event {
            InjectEvent::Move(x, y) => unsafe {
                (self.fake_motion)(self.display, self.screen, *x as i32, *y as i32, 0)
            },
            InjectEvent::Button { button, down } => {
                let number = match *button {
                    BTN_RIGHT => 3,
                    BTN_MIDDLE => 2,
                    _ => 1,
                };
                unsafe { (self.fake_button)(self.display, number, i32::from(*down), 0) }
            }
            InjectEvent::Scroll { dx, dy } => {
                let mut status = 0;
                if *dy != 0 {
                    let button = if *dy > 0 { 4 } else { 5 };
                    status |= unsafe { (self.fake_button)(self.display, button, 1, 0) };
                    status |= unsafe { (self.fake_button)(self.display, button, 0, 0) };
                }
                if *dx != 0 {
                    let button = if *dx > 0 { 7 } else { 6 };
                    status |= unsafe { (self.fake_button)(self.display, button, 1, 0) };
                    status |= unsafe { (self.fake_button)(self.display, button, 0, 0) };
                }
                status
            }
            InjectEvent::Key { code, down } => {
                let evdev = code.saturating_sub(8) as u16;
                let Some(keysym) = keysym_from_evdev(evdev) else { return true };
                let keycode = unsafe { (self.keysym_to_keycode)(self.display, keysym) };
                if keycode == 0 {
                    return true;
                }
                unsafe { (self.fake_key)(self.display, u32::from(keycode), i32::from(*down), 0) }
            }
        };
        unsafe { (self.flush)(self.display) };
        ok == 0 || ok == 1
    }
}

impl Drop for X11Input {
    fn drop(&mut self) {
        if !self.display.is_null() {
            unsafe { (self.close_display)(self.display) };
        }
    }
}

fn keysym_from_evdev(code: u16) -> Option<u64> {
    Some(match code {
        30 => 0x0061, // a
        48 => 0x0062,
        46 => 0x0063,
        32 => 0x0064,
        18 => 0x0065,
        33 => 0x0066,
        34 => 0x0067,
        35 => 0x0068,
        23 => 0x0069,
        36 => 0x006a,
        37 => 0x006b,
        38 => 0x006c,
        50 => 0x006d,
        49 => 0x006e,
        24 => 0x006f,
        25 => 0x0070,
        16 => 0x0071,
        19 => 0x0072,
        31 => 0x0073,
        20 => 0x0074,
        22 => 0x0075,
        47 => 0x0076,
        17 => 0x0077,
        45 => 0x0078,
        21 => 0x0079,
        44 => 0x007a,
        2 => 0x0031,
        3 => 0x0032,
        4 => 0x0033,
        5 => 0x0034,
        6 => 0x0035,
        7 => 0x0036,
        8 => 0x0037,
        9 => 0x0038,
        10 => 0x0039,
        11 => 0x0030,
        28 => 0xff0d,
        57 => 0x0020,
        105 => 0xff51,
        103 => 0xff52,
        106 => 0xff53,
        108 => 0xff54,
        1 => 0xff1b,
        14 => 0xff08,
        15 => 0xff09,
        42 | 54 => 0xffe1,
        29 | 97 => 0xffe3,
        56 | 100 => 0xffe9,
        125 | 126 => 0xffeb,
        58 => 0xffe5,
        12 => 0x002d,
        13 => 0x003d,
        26 => 0x005b,
        27 => 0x005d,
        43 => 0x005c,
        39 => 0x003b,
        40 => 0x0027,
        41 => 0x0060,
        51 => 0x002c,
        52 => 0x002e,
        53 => 0x002f,
        111 => 0xffff,
        102 => 0xff50,
        107 => 0xff57,
        104 => 0xff55,
        109 => 0xff56,
        59 => 0xffbe,
        60 => 0xffbf,
        61 => 0xffc0,
        62 => 0xffc1,
        63 => 0xffc2,
        64 => 0xffc3,
        65 => 0xffc4,
        66 => 0xffc5,
        67 => 0xffc6,
        68 => 0xffc7,
        87 => 0xffc8,
        88 => 0xffc9,
        _ => return None,
    })
}

/// macOS virtual key code to an evdev key code.
pub(crate) fn evdev_from_mac(code: u16) -> Option<u16> {
    Some(match code {
        0x00 => 30, // A
        0x0B => 48, // B
        0x08 => 46, // C
        0x02 => 32, // D
        0x0E => 18, // E
        0x03 => 33, // F
        0x05 => 34, // G
        0x04 => 35, // H
        0x22 => 23, // I
        0x26 => 36, // J
        0x28 => 37, // K
        0x25 => 38, // L
        0x2E => 50, // M
        0x2D => 49, // N
        0x1F => 24, // O
        0x23 => 25, // P
        0x0C => 16, // Q
        0x0F => 19, // R
        0x01 => 31, // S
        0x11 => 20, // T
        0x20 => 22, // U
        0x09 => 47, // V
        0x0D => 17, // W
        0x07 => 45, // X
        0x10 => 21, // Y
        0x06 => 44, // Z
        0x12 => 2,  // 1
        0x13 => 3,
        0x14 => 4,
        0x15 => 5,
        0x17 => 6,
        0x16 => 7,
        0x1A => 8,
        0x1C => 9,
        0x19 => 10,
        0x1D => 11, // 0
        0x24 => 28, // Return
        0x31 => 57, // Space
        0x7B => 105, // Left
        0x7E => 103, // Up
        0x7C => 106, // Right
        0x7D => 108, // Down
        0x35 => 1,  // Escape
        0x33 => 14, // Backspace
        0x30 => 15, // Tab
        0x38 => 42, // Shift
        0x3C => 54, // Right shift
        0x3B => 29, // Control
        0x3E => 97, // Right control
        0x3A => 56, // Option
        0x3D => 100, // Right option
        0x37 => 125, // Command
        0x39 => 58, // Caps lock
        0x1B => 12, // Minus
        0x18 => 13, // Equal
        0x21 => 26, // [
        0x1E => 27, // ]
        0x2A => 43, // backslash
        0x29 => 39, // semicolon
        0x27 => 40, // quote
        0x32 => 41, // grave
        0x2B => 51, // comma
        0x2F => 52, // period
        0x2C => 53, // slash
        0x75 => 111, // forward delete
        0x73 => 102, // home
        0x77 => 107, // end
        0x74 => 104, // page up
        0x79 => 109, // page down
        0x7A => 59, // F1
        0x78 => 60,
        0x63 => 61,
        0x76 => 62,
        0x60 => 63,
        0x61 => 64,
        0x62 => 65,
        0x64 => 66,
        0x65 => 67,
        0x6D => 68, // F10
        0x67 => 87, // F11
        0x6F => 88, // F12
        _ => return None,
    })
}

#[allow(dead_code)]
static LOGGED: AtomicBool = AtomicBool::new(false);
