use engine_script::{Key, MouseButton, ScriptInput};
use engine_window::{Key as WindowKey, MouseButton as WindowMouseButton};
use std::sync::{Mutex, MutexGuard};

/// Event-thread accumulator for a single script update snapshot.
#[derive(Default)]
pub(crate) struct InputAccumulator {
    input: ScriptInput,
    focused: bool,
    background: bool,
}

impl InputAccumulator {
    pub(crate) fn key(&mut self, key: WindowKey, pressed: bool) {
        if self.accepting_input() {
            self.input.set_key(map_key(key), pressed);
        }
    }

    pub(crate) fn mouse_button(&mut self, button: WindowMouseButton, pressed: bool) {
        if self.accepting_input() {
            self.input
                .set_mouse_button(map_mouse_button(button), pressed);
        }
    }

    pub(crate) fn cursor_moved(&mut self, x: f64, y: f64) {
        if self.accepting_input() {
            self.input.set_cursor(x as f32, y as f32);
        }
    }

    pub(crate) fn mouse_motion(&mut self, dx: f64, dy: f64) {
        if self.accepting_input() {
            self.input.add_mouse_delta(dx as f32, dy as f32);
        }
    }

    pub(crate) fn mouse_wheel(&mut self, dx: f32, dy: f32) {
        if self.accepting_input() {
            self.input.add_wheel(dx, dy);
        }
    }

    pub(crate) fn focused(&mut self, focused: bool) {
        self.focused = focused;
        if !focused {
            self.input.release_all();
            self.input.clear_pointer();
        }
    }

    pub(crate) fn set_background(&mut self, background: bool) {
        if self.background == background {
            return;
        }
        self.background = background;
        if background {
            self.input.reset_for_background();
        }
    }

    pub(crate) fn take_frame(&mut self) -> ScriptInput {
        self.input.take_frame()
    }

    fn accepting_input(&self) -> bool {
        self.focused && !self.background
    }
}

pub(crate) fn lock_input(input: &Mutex<InputAccumulator>) -> MutexGuard<'_, InputAccumulator> {
    input
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn map_key(key: WindowKey) -> Key {
    match key {
        WindowKey::A => Key::A,
        WindowKey::B => Key::B,
        WindowKey::C => Key::C,
        WindowKey::D => Key::D,
        WindowKey::E => Key::E,
        WindowKey::F => Key::F,
        WindowKey::G => Key::G,
        WindowKey::H => Key::H,
        WindowKey::I => Key::I,
        WindowKey::J => Key::J,
        WindowKey::K => Key::K,
        WindowKey::L => Key::L,
        WindowKey::M => Key::M,
        WindowKey::N => Key::N,
        WindowKey::O => Key::O,
        WindowKey::P => Key::P,
        WindowKey::Q => Key::Q,
        WindowKey::R => Key::R,
        WindowKey::S => Key::S,
        WindowKey::T => Key::T,
        WindowKey::U => Key::U,
        WindowKey::V => Key::V,
        WindowKey::W => Key::W,
        WindowKey::X => Key::X,
        WindowKey::Y => Key::Y,
        WindowKey::Z => Key::Z,
        WindowKey::Digit0 => Key::Digit0,
        WindowKey::Digit1 => Key::Digit1,
        WindowKey::Digit2 => Key::Digit2,
        WindowKey::Digit3 => Key::Digit3,
        WindowKey::Digit4 => Key::Digit4,
        WindowKey::Digit5 => Key::Digit5,
        WindowKey::Digit6 => Key::Digit6,
        WindowKey::Digit7 => Key::Digit7,
        WindowKey::Digit8 => Key::Digit8,
        WindowKey::Digit9 => Key::Digit9,
        WindowKey::F1 => Key::F1,
        WindowKey::F2 => Key::F2,
        WindowKey::F3 => Key::F3,
        WindowKey::F4 => Key::F4,
        WindowKey::F5 => Key::F5,
        WindowKey::F6 => Key::F6,
        WindowKey::F7 => Key::F7,
        WindowKey::F8 => Key::F8,
        WindowKey::F9 => Key::F9,
        WindowKey::F10 => Key::F10,
        WindowKey::F11 => Key::F11,
        WindowKey::F12 => Key::F12,
        WindowKey::ArrowUp => Key::ArrowUp,
        WindowKey::ArrowDown => Key::ArrowDown,
        WindowKey::ArrowLeft => Key::ArrowLeft,
        WindowKey::ArrowRight => Key::ArrowRight,
        WindowKey::Space => Key::Space,
        WindowKey::Enter => Key::Enter,
        WindowKey::Escape => Key::Escape,
        WindowKey::Tab => Key::Tab,
        WindowKey::Backspace => Key::Backspace,
        WindowKey::ShiftLeft => Key::ShiftLeft,
        WindowKey::ShiftRight => Key::ShiftRight,
        WindowKey::ControlLeft => Key::ControlLeft,
        WindowKey::ControlRight => Key::ControlRight,
        WindowKey::AltLeft => Key::AltLeft,
        WindowKey::AltRight => Key::AltRight,
        WindowKey::SuperLeft => Key::SuperLeft,
        WindowKey::SuperRight => Key::SuperRight,
    }
}

fn map_mouse_button(button: WindowMouseButton) -> MouseButton {
    match button {
        WindowMouseButton::Left => MouseButton::Left,
        WindowMouseButton::Right => MouseButton::Right,
        WindowMouseButton::Middle => MouseButton::Middle,
        WindowMouseButton::Back => MouseButton::Back,
        WindowMouseButton::Forward => MouseButton::Forward,
    }
}

#[cfg(test)]
mod tests {
    use super::{InputAccumulator, map_key, map_mouse_button};
    use engine_script::{Key, MouseButton};
    use engine_window::{Key as WindowKey, MouseButton as WindowMouseButton};

    #[test]
    fn every_window_key_maps_to_the_matching_script_key() {
        let keys = [
            WindowKey::A,
            WindowKey::B,
            WindowKey::C,
            WindowKey::D,
            WindowKey::E,
            WindowKey::F,
            WindowKey::G,
            WindowKey::H,
            WindowKey::I,
            WindowKey::J,
            WindowKey::K,
            WindowKey::L,
            WindowKey::M,
            WindowKey::N,
            WindowKey::O,
            WindowKey::P,
            WindowKey::Q,
            WindowKey::R,
            WindowKey::S,
            WindowKey::T,
            WindowKey::U,
            WindowKey::V,
            WindowKey::W,
            WindowKey::X,
            WindowKey::Y,
            WindowKey::Z,
            WindowKey::Digit0,
            WindowKey::Digit1,
            WindowKey::Digit2,
            WindowKey::Digit3,
            WindowKey::Digit4,
            WindowKey::Digit5,
            WindowKey::Digit6,
            WindowKey::Digit7,
            WindowKey::Digit8,
            WindowKey::Digit9,
            WindowKey::F1,
            WindowKey::F2,
            WindowKey::F3,
            WindowKey::F4,
            WindowKey::F5,
            WindowKey::F6,
            WindowKey::F7,
            WindowKey::F8,
            WindowKey::F9,
            WindowKey::F10,
            WindowKey::F11,
            WindowKey::F12,
            WindowKey::ArrowUp,
            WindowKey::ArrowDown,
            WindowKey::ArrowLeft,
            WindowKey::ArrowRight,
            WindowKey::Space,
            WindowKey::Enter,
            WindowKey::Escape,
            WindowKey::Tab,
            WindowKey::Backspace,
            WindowKey::ShiftLeft,
            WindowKey::ShiftRight,
            WindowKey::ControlLeft,
            WindowKey::ControlRight,
            WindowKey::AltLeft,
            WindowKey::AltRight,
            WindowKey::SuperLeft,
            WindowKey::SuperRight,
        ];

        for key in keys {
            let mapped = map_key(key);
            assert_eq!(
                mapped as u32, key as u32,
                "key {key:?} has a different ABI code"
            );
            assert_eq!(
                Key::from_name(&format!("{key:?}")),
                Some(mapped),
                "key {key:?} maps to a different ABI variant"
            );
        }
    }

    #[test]
    fn every_window_mouse_button_maps_to_the_matching_script_button() {
        let buttons = [
            WindowMouseButton::Left,
            WindowMouseButton::Right,
            WindowMouseButton::Middle,
            WindowMouseButton::Back,
            WindowMouseButton::Forward,
        ];

        for button in buttons {
            let mapped = map_mouse_button(button);
            assert_eq!(
                mapped as u32, button as u32,
                "mouse button {button:?} has a different ABI code"
            );
            assert_eq!(
                MouseButton::from_name(&format!("{button:?}")),
                Some(mapped),
                "mouse button {button:?} maps to a different ABI variant"
            );
        }
    }

    #[test]
    fn key_edges_persist_until_the_frame_is_taken_and_repeat_does_not_press_again() {
        let mut input = InputAccumulator {
            focused: true,
            ..Default::default()
        };
        input.key(WindowKey::W, true);
        input.key(WindowKey::W, true);

        let frame = input.take_frame();
        assert!(frame.key_down(Key::W));
        assert!(frame.key_pressed(Key::W));
        assert!(!frame.key_released(Key::W));

        let frame = input.take_frame();
        assert!(frame.key_down(Key::W));
        assert!(!frame.key_pressed(Key::W));
        assert!(!frame.key_released(Key::W));
    }

    #[test]
    fn press_and_release_in_one_frame_reports_both_edges_without_down_state() {
        let mut input = InputAccumulator {
            focused: true,
            ..Default::default()
        };
        input.key(WindowKey::Space, true);
        input.key(WindowKey::Space, false);

        let frame = input.take_frame();
        assert!(!frame.key_down(Key::Space));
        assert!(frame.key_pressed(Key::Space));
        assert!(frame.key_released(Key::Space));
    }

    #[test]
    fn pointer_deltas_accumulate_and_clear_after_take_frame() {
        let mut input = InputAccumulator {
            focused: true,
            ..Default::default()
        };
        input.cursor_moved(120.0, 240.0);
        input.mouse_motion(1.5, -2.0);
        input.mouse_motion(0.5, 1.0);
        input.mouse_wheel(0.0, 1.0);
        input.mouse_wheel(2.0, -0.25);

        let frame = input.take_frame();
        assert_eq!(frame.cursor(), [120.0, 240.0]);
        assert_eq!(frame.mouse_delta(), [2.0, -1.0]);
        assert_eq!(frame.wheel(), [2.0, 0.75]);

        let frame = input.take_frame();
        assert_eq!(frame.mouse_delta(), [0.0; 2]);
        assert_eq!(frame.wheel(), [0.0; 2]);
        assert_eq!(frame.cursor(), [120.0, 240.0]);
    }

    #[test]
    fn mouse_edges_survive_the_frame_snapshot() {
        let mut input = InputAccumulator {
            focused: true,
            ..Default::default()
        };
        input.mouse_button(WindowMouseButton::Left, true);
        input.mouse_button(WindowMouseButton::Left, false);

        let frame = input.take_frame();
        assert!(!frame.mouse_down(MouseButton::Left));
        assert!(frame.mouse_pressed(MouseButton::Left));
        assert!(frame.mouse_released(MouseButton::Left));
    }

    #[test]
    fn focus_loss_and_background_release_all_held_input() {
        let mut input = InputAccumulator {
            focused: true,
            ..Default::default()
        };
        input.cursor_moved(30.0, 50.0);
        input.mouse_motion(2.0, 3.0);
        input.mouse_wheel(1.0, -1.0);
        input.key(WindowKey::A, true);
        input.mouse_button(WindowMouseButton::Right, true);
        input.take_frame();
        input.focused(false);

        let frame = input.take_frame();
        assert!(!frame.key_down(Key::A));
        assert!(frame.key_released(Key::A));
        assert!(!frame.mouse_down(MouseButton::Right));
        assert!(frame.mouse_released(MouseButton::Right));
        assert_eq!(frame.cursor(), [30.0, 50.0]);
        assert_eq!(frame.mouse_delta(), [0.0; 2]);
        assert_eq!(frame.wheel(), [0.0; 2]);
        input.key(WindowKey::B, true);
        assert!(!input.take_frame().key_down(Key::B));

        input.focused(true);
        input.key(WindowKey::C, true);
        input.mouse_button(WindowMouseButton::Middle, true);
        input.mouse_motion(4.0, -3.0);
        input.mouse_wheel(1.0, 2.0);
        input.set_background(true);
        let frame = input.take_frame();
        assert!(!frame.key_down(Key::C));
        assert!(!frame.key_pressed(Key::C));
        assert!(frame.key_released(Key::C));
        assert!(!frame.mouse_down(MouseButton::Middle));
        assert!(!frame.mouse_pressed(MouseButton::Middle));
        assert!(frame.mouse_released(MouseButton::Middle));
        assert_eq!(frame.cursor(), [30.0, 50.0]);
        assert_eq!(frame.mouse_delta(), [0.0; 2]);
        assert_eq!(frame.wheel(), [0.0; 2]);

        let frame = input.take_frame();
        assert!(!frame.key_released(Key::C));
        assert!(!frame.mouse_released(MouseButton::Middle));
    }
}
