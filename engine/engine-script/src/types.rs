use engine_scene::ParamValue;
use rve_sdk::{Key, MouseButton};
use std::collections::BTreeMap;
use std::fmt;

/// Script parameters in deterministic key order.
pub type ScriptParams = BTreeMap<String, ParamValue>;

/// Time exposed to scripts for one lifecycle phase.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScriptTime {
    pub elapsed: f64,
    pub frame: u64,
}

/// Copyable input snapshot shared by every script during one update.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScriptInput {
    key_down: u128,
    key_pressed: u128,
    key_released: u128,
    mouse_down: u8,
    mouse_pressed: u8,
    mouse_released: u8,
    cursor: [f32; 2],
    mouse_delta: [f32; 2],
    wheel: [f32; 2],
}

impl ScriptInput {
    /// Applies a key transition. Repeated presses while held do not create another edge.
    pub fn set_key(&mut self, key: Key, pressed: bool) {
        let bit = 1_u128 << (key as u32);
        if pressed {
            if self.key_down & bit == 0 {
                self.key_down |= bit;
                self.key_pressed |= bit;
            }
        } else if self.key_down & bit != 0 {
            self.key_down &= !bit;
            self.key_released |= bit;
        }
    }

    /// Applies a mouse-button transition. Repeated presses while held do not create another edge.
    pub fn set_mouse_button(&mut self, button: MouseButton, pressed: bool) {
        let bit = 1_u8 << (button as u32);
        if pressed {
            if self.mouse_down & bit == 0 {
                self.mouse_down |= bit;
                self.mouse_pressed |= bit;
            }
        } else if self.mouse_down & bit != 0 {
            self.mouse_down &= !bit;
            self.mouse_released |= bit;
        }
    }

    /// Sets the cursor position in physical pixels.
    pub fn set_cursor(&mut self, x: f32, y: f32) {
        self.cursor = [x, y];
    }

    /// Adds relative mouse motion to this frame's accumulated delta.
    pub fn add_mouse_delta(&mut self, dx: f32, dy: f32) {
        self.mouse_delta[0] += dx;
        self.mouse_delta[1] += dy;
    }

    /// Adds wheel motion in lines to this frame's accumulated delta.
    pub fn add_wheel(&mut self, dx: f32, dy: f32) {
        self.wheel[0] += dx;
        self.wheel[1] += dy;
    }

    /// Returns whether the key is currently held.
    pub fn key_down(&self, key: Key) -> bool {
        self.key_down & (1_u128 << (key as u32)) != 0
    }

    /// Returns whether the key became held in this frame.
    pub fn key_pressed(&self, key: Key) -> bool {
        self.key_pressed & (1_u128 << (key as u32)) != 0
    }

    /// Returns whether the key became released in this frame.
    pub fn key_released(&self, key: Key) -> bool {
        self.key_released & (1_u128 << (key as u32)) != 0
    }

    /// Returns the down/pressed/released bit flags for a stable key code.
    pub fn key_state(&self, code: u32) -> u32 {
        let Some(key) = Key::from_code(code) else {
            return 0;
        };
        let bit = 1_u128 << (key as u32);
        state_flags(
            self.key_down & bit != 0,
            self.key_pressed & bit != 0,
            self.key_released & bit != 0,
        )
    }

    /// Returns whether the mouse button is currently held.
    pub fn mouse_down(&self, button: MouseButton) -> bool {
        self.mouse_down & (1_u8 << (button as u32)) != 0
    }

    /// Returns whether the mouse button became held in this frame.
    pub fn mouse_pressed(&self, button: MouseButton) -> bool {
        self.mouse_pressed & (1_u8 << (button as u32)) != 0
    }

    /// Returns whether the mouse button became released in this frame.
    pub fn mouse_released(&self, button: MouseButton) -> bool {
        self.mouse_released & (1_u8 << (button as u32)) != 0
    }

    /// Returns the down/pressed/released bit flags for a stable mouse-button code.
    pub fn mouse_button_state(&self, code: u32) -> u32 {
        let Some(button) = MouseButton::from_code(code) else {
            return 0;
        };
        let bit = 1_u8 << (button as u32);
        state_flags(
            self.mouse_down & bit != 0,
            self.mouse_pressed & bit != 0,
            self.mouse_released & bit != 0,
        )
    }

    /// Returns cursor position in physical pixels.
    pub fn cursor(&self) -> [f32; 2] {
        self.cursor
    }

    /// Returns accumulated relative mouse motion since the preceding snapshot.
    pub fn mouse_delta(&self) -> [f32; 2] {
        self.mouse_delta
    }

    /// Returns accumulated wheel motion in lines since the preceding snapshot.
    pub fn wheel(&self) -> [f32; 2] {
        self.wheel
    }

    /// Adds released edges for every held key and button.
    pub fn release_all(&mut self) {
        self.key_released |= self.key_down;
        self.key_down = 0;
        self.mouse_released |= self.mouse_down;
        self.mouse_down = 0;
    }

    /// Clears accumulated pointer motion while preserving the last cursor position.
    pub fn clear_pointer(&mut self) {
        self.mouse_delta = [0.0; 2];
        self.wheel = [0.0; 2];
    }

    /// Releases held input and drops pending presses and pointer motion for Background.
    pub fn reset_for_background(&mut self) {
        self.key_released |= self.key_down;
        self.key_pressed = 0;
        self.key_down = 0;
        self.mouse_released |= self.mouse_down;
        self.mouse_pressed = 0;
        self.mouse_down = 0;
        self.clear_pointer();
    }

    /// Copies the current frame and clears edge and accumulated-delta fields.
    pub fn take_frame(&mut self) -> Self {
        let frame = *self;
        self.clear_frame_transients();
        frame
    }

    /// Clears frame edges and deltas while preserving held state and cursor position.
    pub(crate) fn clear_frame_transients(&mut self) {
        self.key_pressed = 0;
        self.key_released = 0;
        self.mouse_pressed = 0;
        self.mouse_released = 0;
        self.mouse_delta = [0.0; 2];
        self.wheel = [0.0; 2];
    }
}

fn state_flags(down: bool, pressed: bool, released: bool) -> u32 {
    (u32::from(down) * rve_sdk::abi::RVE_INPUT_DOWN)
        | (u32::from(pressed) * rve_sdk::abi::RVE_INPUT_PRESSED)
        | (u32::from(released) * rve_sdk::abi::RVE_INPUT_RELEASED)
}

/// Local object transform exposed to scripts. Rotation uses degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScriptTransform {
    pub position: [f32; 3],
    pub rotation: [f32; 3],
    pub scale: [f32; 3],
}

impl ScriptTransform {
    pub(crate) fn is_finite(self) -> bool {
        self.position
            .into_iter()
            .chain(self.rotation)
            .chain(self.scale)
            .all(f32::is_finite)
    }
}

/// Severity for buffered script events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScriptEventLevel {
    Info,
    Warn,
    Error,
}

/// Buffered log or runtime-error event; scripts never print through the host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptEvent {
    pub level: ScriptEventLevel,
    pub source: String,
    pub message: String,
}

impl fmt::Display for ScriptEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.source, self.message)
    }
}
