use deskbridge_proto::{mods, Input, MouseButton};

use crate::capture::DisplayGeometry;
use crate::cg::{
    self, post, CGPoint, FLAG_ALTERNATE, FLAG_COMMAND, FLAG_CONTROL, FLAG_SHIFT, LEFT_MOUSE_DOWN,
    LEFT_MOUSE_DRAGGED, LEFT_MOUSE_UP, MOUSE_BUTTON_NUMBER, MOUSE_MOVED, OTHER_MOUSE_DOWN,
    OTHER_MOUSE_DRAGGED, OTHER_MOUSE_UP, RIGHT_MOUSE_DOWN, RIGHT_MOUSE_DRAGGED, RIGHT_MOUSE_UP,
    SCROLL_UNIT_LINE, SCROLL_UNIT_PIXEL,
};

pub struct Injector {
    geometry: DisplayGeometry,
    left: bool,
    right: bool,
    middle: bool,
    x: i32,
    y: i32,
}

impl Injector {
    pub fn new(geometry: DisplayGeometry) -> Self {
        Self {
            geometry,
            left: false,
            right: false,
            middle: false,
            x: 0,
            y: 0,
        }
    }

    pub fn apply(&mut self, input: Input) {
        match input {
            Input::MouseMove { x, y } => self.mouse_move(x, y),
            Input::MouseButton { button, down } => self.button(button, down),
            Input::Wheel { dx, dy } => self.wheel(dx, dy),
            Input::Key { code, down, modifiers } => self.key(code, down, modifiers),
        }
    }

    fn point(&self, x: i32, y: i32) -> CGPoint {
        let (gx, gy) = self.geometry.pixel_to_global(x, y);
        CGPoint { x: gx, y: gy }
    }

    fn mouse_move(&mut self, x: i32, y: i32) {
        self.x = x;
        self.y = y;
        let kind = if self.left {
            LEFT_MOUSE_DRAGGED
        } else if self.right {
            RIGHT_MOUSE_DRAGGED
        } else if self.middle {
            OTHER_MOUSE_DRAGGED
        } else {
            MOUSE_MOVED
        };
        let button = if self.right {
            1
        } else if self.middle {
            2
        } else {
            0
        };
        unsafe {
            let event = cg::CGEventCreateMouseEvent(
                std::ptr::null_mut(),
                kind,
                self.point(x, y),
                button,
            );
            cg::CGEventSetIntegerValueField(event, MOUSE_BUTTON_NUMBER, i64::from(button));
            post(event);
        }
    }

    fn button(&mut self, button: MouseButton, down: bool) {
        let (kind_down, kind_up, number) = match button {
            MouseButton::Left => {
                self.left = down;
                (LEFT_MOUSE_DOWN, LEFT_MOUSE_UP, 0)
            }
            MouseButton::Right => {
                self.right = down;
                (RIGHT_MOUSE_DOWN, RIGHT_MOUSE_UP, 1)
            }
            MouseButton::Middle => {
                self.middle = down;
                (OTHER_MOUSE_DOWN, OTHER_MOUSE_UP, 2)
            }
        };
        let kind = if down { kind_down } else { kind_up };
        unsafe {
            let event =
                cg::CGEventCreateMouseEvent(std::ptr::null_mut(), kind, self.point(self.x, self.y), number);
            cg::CGEventSetIntegerValueField(event, MOUSE_BUTTON_NUMBER, i64::from(number));
            post(event);
        }
    }

    fn wheel(&self, dx: i32, dy: i32) {
        let pixel = dx.unsigned_abs() > 20 || dy.unsigned_abs() > 20;
        let units = if pixel { SCROLL_UNIT_PIXEL } else { SCROLL_UNIT_LINE };
        unsafe {
            let event = cg::CGEventCreateScrollWheelEvent(
                std::ptr::null_mut(),
                units,
                2,
                -dy,
                -dx,
            );
            post(event);
        }
    }

    fn key(&self, code: u16, down: bool, modifiers: u8) {
        unsafe {
            let event = cg::CGEventCreateKeyboardEvent(std::ptr::null_mut(), code, down);
            cg::CGEventSetFlags(event, cg_flags(modifiers));
            post(event);
        }
    }
}

fn cg_flags(modifiers: u8) -> u64 {
    let mut flags = 0;
    if modifiers & mods::SHIFT != 0 {
        flags |= FLAG_SHIFT;
    }
    if modifiers & mods::CTRL != 0 {
        flags |= FLAG_CONTROL;
    }
    if modifiers & mods::ALT != 0 {
        flags |= FLAG_ALTERNATE;
    }
    if modifiers & mods::META != 0 {
        flags |= FLAG_COMMAND;
    }
    flags
}
