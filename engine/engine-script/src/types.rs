use engine_scene::{CameraCursor, ParamValue};
use rve_sdk::abi::{RVE_GAMEPAD_SLOTS, RveGamepadState};
use rve_sdk::{GamepadAxis, GamepadButton, GamepadStick, Key, MouseButton};
use std::collections::BTreeMap;
use std::fmt;

/// Bits of every valid gamepad button code.
const PAD_BUTTON_MASK: u32 = (1 << GamepadButton::ALL.len()) - 1;

/// Script parameters in deterministic key order.
pub type ScriptParams = BTreeMap<String, ParamValue>;

/// Time exposed to scripts for one lifecycle phase.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScriptTime {
    pub elapsed: f64,
    pub frame: u64,
}

/// Состояние курсора окна, которое скрипты задают через `rve.input` или Rust SDK.
///
/// По умолчанию курсор видим и не удерживается. Значения применяет хост: engine-script
/// не зависит от окна и не вызывает winit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScriptCursor {
    pub visible: bool,
    pub locked: bool,
}

impl Default for ScriptCursor {
    fn default() -> Self {
        Self {
            visible: true,
            locked: false,
        }
    }
}

impl From<CameraCursor> for ScriptCursor {
    fn from(cursor: CameraCursor) -> Self {
        Self {
            visible: !cursor.hide_on_start,
            locked: cursor.lock_on_start,
        }
    }
}

/// One gamepad slot inside a [`ScriptInput`] snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PadState {
    pub(crate) connected: bool,
    down: u32,
    pressed: u32,
    released: u32,
    /// Axis values in [`GamepadAxis`] code order.
    axes: [f32; 6],
}

impl PadState {
    pub(crate) fn button_state(&self, button: GamepadButton) -> u32 {
        let bit = 1_u32 << button as u32;
        state_flags(
            self.down & bit != 0,
            self.pressed & bit != 0,
            self.released & bit != 0,
        )
    }

    pub(crate) fn axis(&self, axis: GamepadAxis) -> f32 {
        self.axes[axis as usize]
    }

    pub(crate) fn stick(&self, stick: GamepadStick) -> [f32; 2] {
        let (x, y) = match stick {
            GamepadStick::Left => (GamepadAxis::LeftX, GamepadAxis::LeftY),
            GamepadStick::Right => (GamepadAxis::RightX, GamepadAxis::RightY),
        };
        [self.axis(x), self.axis(y)]
    }

    fn to_abi(self) -> RveGamepadState {
        RveGamepadState {
            connected: u32::from(self.connected),
            down: self.down,
            pressed: self.pressed,
            released: self.released,
            axes: self.axes,
        }
    }
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
    pads: [PadState; RVE_GAMEPAD_SLOTS],
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

    /// Applies one gamepad slot's level snapshot. `index` is 0..=3 for pads 1..=4; other indices
    /// are ignored. Button edges come from the change against the previous level; axes are
    /// replaced as given.
    pub fn set_pad(&mut self, index: usize, connected: bool, buttons: u32, axes: [f32; 6]) {
        let Some(pad) = self.pads.get_mut(index) else {
            return;
        };
        let buttons = buttons & PAD_BUTTON_MASK;
        pad.connected = connected;
        pad.pressed |= buttons & !pad.down;
        pad.released |= pad.down & !buttons;
        pad.down = buttons;
        pad.axes = axes;
    }

    /// Returns the gamepad state for ABI slot `pad`: 0 is the aggregate of all slots, 1..=4 is one
    /// slot. Returns `None` for other numbers.
    pub fn gamepad_state(&self, pad: u32) -> Option<RveGamepadState> {
        match pad {
            0 => Some(self.aggregate_gamepad()),
            1..=4 => Some(self.pads[pad as usize - 1].to_abi()),
            _ => None,
        }
    }

    pub(crate) fn pad(&self, index: usize) -> PadState {
        self.pads[index]
    }

    /// Aggregate of all slots: buttons are ORed, triggers take the largest magnitude, and each
    /// stick is the whole vector of the slot with the longest stick.
    fn aggregate_gamepad(&self) -> RveGamepadState {
        let mut total = RveGamepadState::default();
        for pad in &self.pads {
            total.connected |= u32::from(pad.connected);
            total.down |= pad.down;
            total.pressed |= pad.pressed;
            total.released |= pad.released;
        }
        for (axis, value) in [GamepadAxis::LeftTrigger, GamepadAxis::RightTrigger]
            .into_iter()
            .map(|axis| {
                let value = self
                    .pads
                    .iter()
                    .map(|pad| pad.axis(axis))
                    .max_by(|a, b| a.abs().total_cmp(&b.abs()))
                    .unwrap_or(0.0);
                (axis, value)
            })
        {
            total.axes[axis as usize] = value;
        }
        for stick in [GamepadStick::Left, GamepadStick::Right] {
            let vector = self
                .pads
                .iter()
                .map(|pad| pad.stick(stick))
                .max_by(|a, b| (a[0] * a[0] + a[1] * a[1]).total_cmp(&(b[0] * b[0] + b[1] * b[1])))
                .unwrap_or([0.0; 2]);
            let (x, y) = match stick {
                GamepadStick::Left => (GamepadAxis::LeftX, GamepadAxis::LeftY),
                GamepadStick::Right => (GamepadAxis::RightX, GamepadAxis::RightY),
            };
            total.axes[x as usize] = vector[0];
            total.axes[y as usize] = vector[1];
        }
        total
    }

    /// Adds released edges for every held key, button, and gamepad button; zeroes gamepad axes.
    pub fn release_all(&mut self) {
        self.key_released |= self.key_down;
        self.key_down = 0;
        self.mouse_released |= self.mouse_down;
        self.mouse_down = 0;
        self.release_pads();
    }

    fn release_pads(&mut self) {
        for pad in &mut self.pads {
            pad.released |= pad.down;
            pad.down = 0;
            pad.axes = [0.0; 6];
        }
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
        self.release_pads();
        for pad in &mut self.pads {
            pad.pressed = 0;
        }
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
        for pad in &mut self.pads {
            pad.pressed = 0;
            pad.released = 0;
        }
    }
}

/// Longest rumble duration a script can request, in seconds.
pub const MAX_RUMBLE_SECONDS: f32 = 5.0;

/// Force-feedback request committed by a script. `pad` is 0 for every connected gamepad or 1..=4.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RumbleRequest {
    pub pad: u8,
    pub strong: f32,
    pub weak: f32,
    pub seconds: f32,
}

impl RumbleRequest {
    /// Normalizes script arguments: NaN and infinities become zero, intensities clamp to [0, 1],
    /// and the duration clamps to [0, `MAX_RUMBLE_SECONDS`]. Returns `None` for an unknown slot.
    pub fn new(pad: u32, strong: f32, weak: f32, seconds: f32) -> Option<Self> {
        let pad = u8::try_from(pad)
            .ok()
            .filter(|pad| usize::from(*pad) <= RVE_GAMEPAD_SLOTS)?;
        Some(Self {
            pad,
            strong: finite_or_zero(strong).clamp(0.0, 1.0),
            weak: finite_or_zero(weak).clamp(0.0, 1.0),
            seconds: finite_or_zero(seconds).clamp(0.0, MAX_RUMBLE_SECONDS),
        })
    }
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rve_sdk::abi::RVE_INPUT_DOWN;

    const SOUTH: u32 = 1 << GamepadButton::South as u32;
    const EAST: u32 = 1 << GamepadButton::East as u32;

    fn pad_flags(input: &ScriptInput, index: usize, button: GamepadButton) -> u32 {
        input.pad(index).button_state(button)
    }

    #[test]
    fn set_pad_derives_button_edges_from_level_changes() {
        let mut input = ScriptInput::default();
        input.set_pad(0, true, SOUTH, [0.0; 6]);
        assert_eq!(
            pad_flags(&input, 0, GamepadButton::South),
            RVE_INPUT_DOWN | rve_sdk::abi::RVE_INPUT_PRESSED
        );

        input.clear_frame_transients();
        input.set_pad(0, true, SOUTH, [0.0; 6]);
        assert_eq!(pad_flags(&input, 0, GamepadButton::South), RVE_INPUT_DOWN);

        input.set_pad(0, true, 0, [0.0; 6]);
        assert_eq!(
            pad_flags(&input, 0, GamepadButton::South),
            rve_sdk::abi::RVE_INPUT_RELEASED
        );
    }

    #[test]
    fn set_pad_ignores_unknown_slots_and_bits_outside_the_button_table() {
        let mut input = ScriptInput::default();
        input.set_pad(4, true, u32::MAX, [1.0; 6]);
        assert_eq!(input, ScriptInput::default());

        input.set_pad(1, true, u32::MAX, [0.0; 6]);
        assert_eq!(input.gamepad_state(2).unwrap().down, PAD_BUTTON_MASK);
    }

    #[test]
    fn clearing_frame_transients_twice_keeps_held_buttons_and_axes_but_drops_edges() {
        let mut input = ScriptInput::default();
        input.set_pad(0, true, SOUTH, [0.5, -0.25, 0.0, 0.0, 0.75, 0.0]);
        input.clear_frame_transients();
        input.clear_frame_transients();

        assert_eq!(pad_flags(&input, 0, GamepadButton::South), RVE_INPUT_DOWN);
        assert_eq!(input.pad(0).axis(GamepadAxis::LeftX), 0.5);
        assert_eq!(input.pad(0).axis(GamepadAxis::LeftTrigger), 0.75);
    }

    #[test]
    fn release_all_and_background_release_pads_and_zero_axes() {
        let mut input = ScriptInput::default();
        input.set_pad(2, true, SOUTH | EAST, [0.3; 6]);
        input.clear_frame_transients();
        input.release_all();
        assert_eq!(
            pad_flags(&input, 2, GamepadButton::South),
            rve_sdk::abi::RVE_INPUT_RELEASED
        );
        assert_eq!(input.pad(2).axis(GamepadAxis::RightY), 0.0);

        input.set_pad(2, true, SOUTH, [0.3; 6]);
        input.reset_for_background();
        assert_eq!(
            pad_flags(&input, 2, GamepadButton::South),
            rve_sdk::abi::RVE_INPUT_RELEASED
        );
        assert_eq!(input.pad(2).axis(GamepadAxis::LeftX), 0.0);
    }

    #[test]
    fn gamepad_state_reports_slots_and_rejects_other_numbers() {
        let mut input = ScriptInput::default();
        input.set_pad(0, true, SOUTH, [0.2, 0.0, 0.0, 0.0, 0.1, 0.0]);
        input.set_pad(1, true, EAST, [-0.4, 0.0, 0.0, 0.0, 0.9, 0.0]);

        let slot_two = input.gamepad_state(2).unwrap();
        assert_eq!(slot_two.connected, 1);
        assert_eq!(slot_two.down, EAST);
        assert_eq!(input.gamepad_state(3).unwrap().connected, 0);
        assert!(input.gamepad_state(5).is_none());

        let aggregate = input.gamepad_state(0).unwrap();
        assert_eq!(aggregate.connected, 1);
        assert_eq!(aggregate.down, SOUTH | EAST);
        assert_eq!(aggregate.axes[GamepadAxis::LeftTrigger as usize], 0.9);
    }

    #[test]
    fn aggregate_stick_is_the_whole_vector_of_the_longest_stick() {
        let mut input = ScriptInput::default();
        input.set_pad(0, true, 0, [0.3, 0.0, 0.0, 0.0, 0.0, 0.0]);
        input.set_pad(1, true, 0, [0.0, 0.5, 0.0, 0.0, 0.0, 0.0]);
        let aggregate = input.gamepad_state(0).unwrap();
        assert_eq!(aggregate.axes[GamepadAxis::LeftX as usize], 0.0);
        assert_eq!(aggregate.axes[GamepadAxis::LeftY as usize], 0.5);
    }

    #[test]
    fn rumble_requests_normalize_non_finite_and_out_of_range_arguments() {
        assert_eq!(
            RumbleRequest::new(0, f32::NAN, f32::INFINITY, -1.0),
            Some(RumbleRequest {
                pad: 0,
                strong: 0.0,
                weak: 0.0,
                seconds: 0.0,
            })
        );
        assert_eq!(
            RumbleRequest::new(4, 2.0, -1.0, 9.0),
            Some(RumbleRequest {
                pad: 4,
                strong: 1.0,
                weak: 0.0,
                seconds: MAX_RUMBLE_SECONDS,
            })
        );
        assert_eq!(RumbleRequest::new(5, 0.5, 0.5, 1.0), None);
    }
}
