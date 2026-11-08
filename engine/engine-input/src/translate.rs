//! Translation from `gilrs` events to engine gamepad events. The functions are pure, so tests
//! can drive them without hardware or a live `Gilrs` context.

use gilrs::{Axis, Button, EventType};
use rve_sdk::{GamepadAxis, GamepadButton};

/// Engine-side gamepad event for one pad. Stick and trigger values are raw; the mapper applies
/// the dead zone and the trigger threshold when it builds a snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MapperEvent {
    Connected,
    Disconnected,
    /// A digital button; `pressed` is `false` on release.
    Button {
        button: GamepadButton,
        pressed: bool,
    },
    /// An analog stick axis or trigger value.
    Axis {
        axis: GamepadAxis,
        value: f32,
    },
    /// A D-pad axis value: positive is right for `horizontal` and up for vertical.
    DPad {
        horizontal: bool,
        value: f32,
    },
}

/// Translates one `gilrs` event. Returns `None` for events the engine does not use.
pub fn translate(event: EventType) -> Option<MapperEvent> {
    match event {
        EventType::Connected => Some(MapperEvent::Connected),
        EventType::Disconnected => Some(MapperEvent::Disconnected),
        EventType::ButtonPressed(button, _) => button_event(button, true),
        EventType::ButtonReleased(button, _) => button_event(button, false),
        EventType::ButtonChanged(button, value, _) => trigger_event(button, value),
        EventType::AxisChanged(axis, value, _) => axis_event(axis, value),
        _ => None,
    }
}

/// Maps a digital press or release. Analog triggers are excluded: they arrive through
/// `ButtonChanged` values, so their press events would count the same input twice.
pub fn button_event(button: Button, pressed: bool) -> Option<MapperEvent> {
    let button = match button {
        Button::South => GamepadButton::South,
        Button::East => GamepadButton::East,
        Button::North => GamepadButton::North,
        Button::West => GamepadButton::West,
        Button::LeftTrigger => GamepadButton::LeftBumper,
        Button::RightTrigger => GamepadButton::RightBumper,
        Button::Select => GamepadButton::Select,
        Button::Start => GamepadButton::Start,
        Button::Mode => GamepadButton::Mode,
        Button::LeftThumb => GamepadButton::LeftStick,
        Button::RightThumb => GamepadButton::RightStick,
        Button::DPadUp => GamepadButton::DPadUp,
        Button::DPadDown => GamepadButton::DPadDown,
        Button::DPadLeft => GamepadButton::DPadLeft,
        Button::DPadRight => GamepadButton::DPadRight,
        Button::LeftTrigger2 | Button::RightTrigger2 | Button::C | Button::Z | Button::Unknown => {
            return None;
        }
    };
    Some(MapperEvent::Button { button, pressed })
}

/// Maps an analog trigger value. Only the `*Trigger2` buttons carry analog values; other buttons
/// report their state through press and release events.
pub fn trigger_event(button: Button, value: f32) -> Option<MapperEvent> {
    let axis = match button {
        Button::LeftTrigger2 => GamepadAxis::LeftTrigger,
        Button::RightTrigger2 => GamepadAxis::RightTrigger,
        _ => return None,
    };
    Some(MapperEvent::Axis { axis, value })
}

/// Maps a stick axis or D-pad axis value.
pub fn axis_event(axis: Axis, value: f32) -> Option<MapperEvent> {
    let axis = match axis {
        Axis::LeftStickX => GamepadAxis::LeftX,
        Axis::LeftStickY => GamepadAxis::LeftY,
        Axis::RightStickX => GamepadAxis::RightX,
        Axis::RightStickY => GamepadAxis::RightY,
        Axis::DPadX => {
            return Some(MapperEvent::DPad {
                horizontal: true,
                value,
            });
        }
        Axis::DPadY => {
            return Some(MapperEvent::DPad {
                horizontal: false,
                value,
            });
        }
        Axis::LeftZ | Axis::RightZ | Axis::Unknown => return None,
    };
    Some(MapperEvent::Axis { axis, value })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressed_and_released_buttons_map_once_and_changed_events_are_ignored_for_them() {
        assert_eq!(
            button_event(Button::South, true),
            Some(MapperEvent::Button {
                button: GamepadButton::South,
                pressed: true,
            })
        );
        assert_eq!(
            button_event(Button::South, false),
            Some(MapperEvent::Button {
                button: GamepadButton::South,
                pressed: false,
            })
        );
        assert_eq!(trigger_event(Button::South, 1.0), None);
        assert_eq!(trigger_event(Button::DPadUp, 1.0), None);
    }

    #[test]
    fn analog_triggers_come_only_from_changed_values_and_bumpers_from_presses() {
        assert_eq!(button_event(Button::LeftTrigger2, true), None);
        assert_eq!(button_event(Button::RightTrigger2, false), None);
        assert_eq!(
            trigger_event(Button::LeftTrigger2, 0.7),
            Some(MapperEvent::Axis {
                axis: GamepadAxis::LeftTrigger,
                value: 0.7,
            })
        );
        assert_eq!(
            trigger_event(Button::RightTrigger2, 0.2),
            Some(MapperEvent::Axis {
                axis: GamepadAxis::RightTrigger,
                value: 0.2,
            })
        );
        assert_eq!(
            button_event(Button::LeftTrigger, true),
            Some(MapperEvent::Button {
                button: GamepadButton::LeftBumper,
                pressed: true,
            })
        );
        assert_eq!(
            button_event(Button::RightTrigger, true),
            Some(MapperEvent::Button {
                button: GamepadButton::RightBumper,
                pressed: true,
            })
        );
    }

    #[test]
    fn dpad_axes_become_directional_events_and_sticks_map_to_axis_names() {
        assert_eq!(
            axis_event(Axis::DPadX, -1.0),
            Some(MapperEvent::DPad {
                horizontal: true,
                value: -1.0,
            })
        );
        assert_eq!(
            axis_event(Axis::DPadY, 1.0),
            Some(MapperEvent::DPad {
                horizontal: false,
                value: 1.0,
            })
        );
        assert_eq!(
            axis_event(Axis::RightStickY, -0.25),
            Some(MapperEvent::Axis {
                axis: GamepadAxis::RightY,
                value: -0.25,
            })
        );
    }

    #[test]
    fn unknown_and_unmapped_buttons_and_axes_are_ignored() {
        for button in [Button::C, Button::Z, Button::Unknown] {
            assert_eq!(button_event(button, true), None);
        }
        assert_eq!(axis_event(Axis::LeftZ, 0.5), None);
        assert_eq!(axis_event(Axis::Unknown, 0.5), None);
    }

    #[test]
    fn translate_passes_connection_events_and_ignores_unused_ones() {
        assert_eq!(
            translate(EventType::Connected),
            Some(MapperEvent::Connected)
        );
        assert_eq!(
            translate(EventType::Disconnected),
            Some(MapperEvent::Disconnected)
        );
        assert_eq!(translate(EventType::Dropped), None);
        assert_eq!(translate(EventType::ForceFeedbackEffectCompleted), None);
    }
}
