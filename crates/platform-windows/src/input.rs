use deskbridge_proto::{Input, MouseButton};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY, VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT,
    VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD,
    VK_OEM_PLUS, VK_PRIOR, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SPACE, VK_TAB,
    VK_UP, VK_0, VK_1, VK_2, VK_3, VK_4, VK_5, VK_6, VK_7, VK_8, VK_9, VK_A, VK_B, VK_C, VK_D, VK_E, VK_F, VK_G, VK_H,
    VK_I, VK_J, VK_K, VK_L, VK_M, VK_N, VK_O, VK_P, VK_Q, VK_R, VK_S, VK_T, VK_U, VK_V, VK_W, VK_X, VK_Y, VK_Z,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::capture::DisplayGeometry;

const WHEEL_DELTA: i32 = 120;

pub struct Injector {
    geometry: DisplayGeometry,
    x: i32,
    y: i32,
}

impl Injector {
    pub fn new(geometry: DisplayGeometry) -> Self {
        Self { geometry, x: 0, y: 0 }
    }

    pub fn apply(&mut self, input: Input) {
        match input {
            Input::MouseMove { x, y } => self.mouse_move(x, y),
            Input::MouseButton { button, down } => self.button(button, down),
            Input::Wheel { dx, dy } => self.wheel(dx, dy),
            Input::Key { code, down, modifiers: _ } => self.key(code, down),
        }
    }

    fn mouse_move(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
        let (dx, dy) = absolute(self.geometry.pixel_to_global(x, y));
        send(mouse(dx, dy, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK));
    }

    fn button(&mut self, button: MouseButton, down: bool) {
        let (dx, dy) = absolute(self.geometry.pixel_to_global(self.x, self.y));
        let click = match (button, down) {
            (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
            (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
            (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        };
        send(mouse(
            dx,
            dy,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | click,
        ));
    }

    fn wheel(&self, dx: i32, dy: i32) {
        let (x, y) = absolute(self.geometry.pixel_to_global(self.x, self.y));
        let notch = if dx.unsigned_abs() > 20 || dy.unsigned_abs() > 20 { 1 } else { WHEEL_DELTA };
        if dy != 0 {
            send(mouse(
                x,
                y,
                dy.wrapping_mul(notch) as u32,
                MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_WHEEL,
            ));
        }
        if dx != 0 {
            send(mouse(
                x,
                y,
                dx.wrapping_mul(notch) as u32,
                MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK | MOUSEEVENTF_HWHEEL,
            ));
        }
    }

    fn key(&self, code: u16, down: bool) {
        let Some(vk) = mac_to_vk(code) else {
            return;
        };
        let mut flags = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
        if extended(code) {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        send(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
}

fn mouse(dx: i32, dy: i32, mouse_data: u32, flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(input: INPUT) {
    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn absolute(point: (f64, f64)) -> (i32, i32) {
    let (x, y) = point;
    let origin_x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let origin_y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    let nx = ((x - f64::from(origin_x)) * 65535.0 / f64::from(width - 1)).round() as i32;
    let ny = ((y - f64::from(origin_y)) * 65535.0 / f64::from(height - 1)).round() as i32;
    (nx.clamp(0, 65535), ny.clamp(0, 65535))
}

pub(crate) fn cursor_pixel(geometry: DisplayGeometry) -> Option<(i32, i32)> {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        return None;
    }
    Some(geometry.global_to_pixel(f64::from(point.x), f64::from(point.y)))
}

fn extended(code: u16) -> bool {
    matches!(
        code,
        0x7B | 0x7C | 0x7D | 0x7E | 0x73 | 0x77 | 0x74 | 0x79 | 0x75 | 0x3D | 0x3E | 0x36
    )
}

/// macOS `CGKeyCode` values, the same ones every viewer puts on the wire.
fn mac_to_vk(code: u16) -> Option<VIRTUAL_KEY> {
    Some(match code {
        0x00 => VK_A,
        0x01 => VK_S,
        0x02 => VK_D,
        0x03 => VK_F,
        0x04 => VK_H,
        0x05 => VK_G,
        0x06 => VK_Z,
        0x07 => VK_X,
        0x08 => VK_C,
        0x09 => VK_V,
        0x0B => VK_B,
        0x0C => VK_Q,
        0x0D => VK_W,
        0x0E => VK_E,
        0x0F => VK_R,
        0x10 => VK_Y,
        0x11 => VK_T,
        0x12 => VK_1,
        0x13 => VK_2,
        0x14 => VK_3,
        0x15 => VK_4,
        0x16 => VK_6,
        0x17 => VK_5,
        0x18 => VK_OEM_PLUS,
        0x19 => VK_9,
        0x1A => VK_7,
        0x1B => VK_OEM_MINUS,
        0x1C => VK_8,
        0x1D => VK_0,
        0x1E => VK_OEM_6,
        0x1F => VK_O,
        0x20 => VK_U,
        0x21 => VK_OEM_4,
        0x22 => VK_I,
        0x23 => VK_P,
        0x24 => VK_RETURN,
        0x25 => VK_L,
        0x26 => VK_J,
        0x27 => VK_OEM_7,
        0x28 => VK_K,
        0x29 => VK_OEM_1,
        0x2A => VK_OEM_5,
        0x2B => VK_OEM_COMMA,
        0x2C => VK_OEM_2,
        0x2D => VK_N,
        0x2E => VK_M,
        0x2F => VK_OEM_PERIOD,
        0x30 => VK_TAB,
        0x31 => VK_SPACE,
        0x32 => VK_OEM_3,
        0x33 => VK_BACK,
        0x35 => VK_ESCAPE,
        0x36 => VK_RWIN,
        0x37 => VK_LWIN,
        0x38 => VK_SHIFT,
        0x39 => VK_CAPITAL,
        0x3A => VK_MENU,
        0x3B => VK_CONTROL,
        0x3C => VK_RSHIFT,
        0x3D => VK_RMENU,
        0x3E => VK_RCONTROL,
        0x7A => VK_F1,
        0x78 => vk_f(2),
        0x63 => vk_f(3),
        0x76 => vk_f(4),
        0x60 => vk_f(5),
        0x61 => vk_f(6),
        0x62 => vk_f(7),
        0x64 => vk_f(8),
        0x65 => vk_f(9),
        0x6D => vk_f(10),
        0x67 => vk_f(11),
        0x6F => vk_f(12),
        0x7B => VK_LEFT,
        0x7C => VK_RIGHT,
        0x7D => VK_DOWN,
        0x7E => VK_UP,
        0x73 => VK_HOME,
        0x77 => VK_END,
        0x74 => VK_PRIOR,
        0x79 => VK_NEXT,
        0x75 => VK_DELETE,
        _ => return None,
    })
}

fn vk_f(index: u16) -> VIRTUAL_KEY {
    VIRTUAL_KEY(VK_F1.0 + index - 1)
}


