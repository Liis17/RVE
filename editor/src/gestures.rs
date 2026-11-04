//! Window-independent navigation input normalization.
use egui::{Event, MouseWheelUnit, PointerButton, Vec2};
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScrollNavigation {
    #[default]
    Auto,
    Trackpad,
    Mouse,
}
impl ScrollNavigation {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Trackpad => "Trackpad (orbit/pan)",
            Self::Mouse => "Mouse wheel (zoom)",
        }
    }
}
#[derive(Default, Debug)]
pub struct WheelGestures {
    pub orbit: Vec2,
    pub pan: Vec2,
    pub trackpad: Option<bool>,
}
pub fn wheel_gestures(events: &[Event], line_points: f32) -> WheelGestures {
    let mut result = WheelGestures::default();
    for event in events {
        if let Event::MouseWheel {
            unit,
            delta,
            modifiers,
            ..
        } = event
        {
            result.trackpad = Some(matches!(unit, MouseWheelUnit::Point));
            if modifiers.command {
                continue;
            }
            let delta = *delta
                * if *unit == MouseWheelUnit::Line {
                    line_points
                } else {
                    1.0
                };
            if modifiers.shift {
                result.pan += delta;
            } else {
                result.orbit += delta;
            }
        }
    }
    result
}
pub fn remap_control_click(events: &mut Vec<Event>, held: &mut bool) {
    for event in events {
        match event {
            Event::PointerButton {
                button,
                pressed,
                modifiers,
                ..
            } if *button == PointerButton::Primary => {
                if *pressed {
                    *held = modifiers.ctrl && !modifiers.mac_cmd;
                    if *held {
                        *button = PointerButton::Secondary;
                    }
                } else if *held {
                    *button = PointerButton::Secondary;
                    *held = false;
                }
            }
            Event::WindowFocused(false) => *held = false,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Modifiers, pos2, vec2};
    #[test]
    fn wheel_preserves_axes_scales_lines_and_skips_command() {
        let wheel = |unit, modifiers| Event::MouseWheel {
            unit,
            phase: egui::TouchPhase::Move,
            delta: vec2(2.0, 3.0),
            modifiers,
        };
        let result = wheel_gestures(
            &[
                wheel(MouseWheelUnit::Point, Modifiers::NONE),
                wheel(MouseWheelUnit::Line, Modifiers::SHIFT),
                wheel(MouseWheelUnit::Point, Modifiers::COMMAND),
            ],
            10.0,
        );
        assert_eq!(result.orbit, vec2(2.0, 3.0));
        assert_eq!(result.pan, vec2(20.0, 30.0));
        assert_eq!(result.trackpad, Some(true));
        assert_eq!(
            wheel_gestures(&[wheel(MouseWheelUnit::Line, Modifiers::NONE)], 10.0).trackpad,
            Some(false)
        );
        assert_eq!(wheel_gestures(&[], 10.0).trackpad, None);
    }
    #[test]
    fn control_click_release_matches_press_even_after_control_released() {
        for (modifiers, expected) in [
            (Modifiers::CTRL, PointerButton::Secondary),
            (Modifiers::NONE, PointerButton::Primary),
            (
                Modifiers {
                    ctrl: true,
                    mac_cmd: true,
                    ..Modifiers::NONE
                },
                PointerButton::Primary,
            ),
        ] {
            let mut held = false;
            for pressed in [true, false] {
                let mut events = vec![Event::PointerButton {
                    pos: pos2(5.0, 5.0),
                    button: PointerButton::Primary,
                    pressed,
                    modifiers: if pressed { modifiers } else { Modifiers::NONE },
                }];
                remap_control_click(&mut events, &mut held);
                assert!(
                    matches!(events[0], Event::PointerButton { button, .. } if button == expected)
                );
            }
            assert!(!held);
        }
    }
}
