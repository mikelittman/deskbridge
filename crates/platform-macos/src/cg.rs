use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

pub const HID_EVENT_TAP: u32 = 0;
pub const MOUSE_MOVED: u32 = 5;
pub const LEFT_MOUSE_DOWN: u32 = 1;
pub const LEFT_MOUSE_UP: u32 = 2;
pub const RIGHT_MOUSE_DOWN: u32 = 3;
pub const RIGHT_MOUSE_UP: u32 = 4;
pub const LEFT_MOUSE_DRAGGED: u32 = 6;
pub const RIGHT_MOUSE_DRAGGED: u32 = 7;
pub const OTHER_MOUSE_DOWN: u32 = 25;
pub const OTHER_MOUSE_UP: u32 = 26;
pub const OTHER_MOUSE_DRAGGED: u32 = 27;
pub const SCROLL_UNIT_PIXEL: u32 = 0;
pub const SCROLL_UNIT_LINE: u32 = 1;
pub const MOUSE_BUTTON_NUMBER: u32 = 3;

pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_ALTERNATE: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    pub fn CGEventCreate(source: *mut c_void) -> *mut c_void;
    pub fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
    pub fn CGEventCreateMouseEvent(
        source: *mut c_void,
        mouse_type: u32,
        point: CGPoint,
        button: u32,
    ) -> *mut c_void;
    pub fn CGEventCreateKeyboardEvent(source: *mut c_void, key: u16, down: bool) -> *mut c_void;
    pub fn CGEventCreateScrollWheelEvent(
        source: *mut c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
    ) -> *mut c_void;
    pub fn CGEventSetFlags(event: *mut c_void, flags: u64);
    pub fn CGEventSetIntegerValueField(event: *mut c_void, field: u32, value: i64);
    pub fn CGEventPost(tap: u32, event: *mut c_void);
    pub fn CGDisplayCopyDisplayMode(display: u32) -> *mut c_void;
    pub fn CGDisplayModeGetRefreshRate(mode: *mut c_void) -> f64;
    pub fn CGDisplayModeRelease(mode: *mut c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    pub fn CFRelease(value: *const c_void);
}

pub fn mouse_global() -> Option<(f64, f64)> {
    unsafe {
        let event = CGEventCreate(std::ptr::null_mut());
        if event.is_null() {
            return None;
        }
        let point = CGEventGetLocation(event);
        CFRelease(event);
        Some((point.x, point.y))
    }
}

pub fn refresh_millihz(display_id: u32) -> u32 {
    unsafe {
        let mode = CGDisplayCopyDisplayMode(display_id);
        if mode.is_null() {
            return 60_000;
        }
        let hz = CGDisplayModeGetRefreshRate(mode);
        CGDisplayModeRelease(mode);
        if hz.is_finite() && hz >= 1.0 {
            (hz * 1000.0).round() as u32
        } else {
            60_000
        }
    }
}

pub fn post(event: *mut c_void) {
    if event.is_null() {
        return;
    }
    unsafe {
        CGEventPost(HID_EVENT_TAP, event);
        CFRelease(event);
    }
}
