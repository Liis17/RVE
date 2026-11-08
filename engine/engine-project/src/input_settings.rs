use rve_sdk::PhysicalInput;
use rve_sdk::abi::split_gamepad_name;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

const MAX_CONTEXTS: usize = 32;
const MAX_ACTIONS_PER_CONTEXT: usize = 128;
const MAX_INPUTS_PER_SLOT: usize = rve_sdk::abi::RVE_INPUT_MAX_BINDINGS;

/// Project-defined action contexts, their default active context, and gamepad tuning.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSettings {
    #[serde(default)]
    pub default_context: Option<String>,
    #[serde(default)]
    pub contexts: BTreeMap<String, BTreeMap<String, InputAction>>,
    #[serde(default)]
    pub gamepad: GamepadSettings,
}

/// Gamepad tuning from `[input.gamepad]`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GamepadSettings {
    /// Radial stick dead zone in [0, 1). Values inside it read as zero.
    #[serde(default = "default_stick_deadzone")]
    pub stick_deadzone: f32,
    /// Trigger value in (0, 1) above which a trigger counts as a button press (strict `>`).
    #[serde(default = "default_trigger_threshold")]
    pub trigger_threshold: f32,
}

impl Default for GamepadSettings {
    fn default() -> Self {
        Self {
            stick_deadzone: default_stick_deadzone(),
            trigger_threshold: default_trigger_threshold(),
        }
    }
}

impl PartialEq for GamepadSettings {
    fn eq(&self, other: &Self) -> bool {
        self.stick_deadzone.to_bits() == other.stick_deadzone.to_bits()
            && self.trigger_threshold.to_bits() == other.trigger_threshold.to_bits()
    }
}

impl Eq for GamepadSettings {}

impl GamepadSettings {
    fn validate(&self) -> Result<(), String> {
        if !(self.stick_deadzone.is_finite() && (0.0..1.0).contains(&self.stick_deadzone)) {
            return Err(
                "input.gamepad.stick_deadzone должен быть конечным числом в диапазоне [0, 1)"
                    .to_owned(),
            );
        }
        if !(self.trigger_threshold.is_finite()
            && self.trigger_threshold > 0.0
            && self.trigger_threshold < 1.0)
        {
            return Err(
                "input.gamepad.trigger_threshold должен быть конечным числом в диапазоне (0, 1)"
                    .to_owned(),
            );
        }
        Ok(())
    }
}

/// The value shape an action exposes to scripts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputActionKind {
    Button,
    Axis,
    Vector2,
}

/// A button, one-dimensional axis, or two-dimensional vector action.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputAction {
    Button {
        #[serde(default)]
        inputs: Vec<String>,
    },
    Axis {
        #[serde(default)]
        negative: Vec<String>,
        #[serde(default)]
        positive: Vec<String>,
        #[serde(default)]
        analog: Option<String>,
        #[serde(default = "default_scale")]
        scale: f32,
    },
    Vector2 {
        #[serde(default)]
        up: Vec<String>,
        #[serde(default)]
        down: Vec<String>,
        #[serde(default)]
        left: Vec<String>,
        #[serde(default)]
        right: Vec<String>,
        #[serde(default)]
        analog: Option<String>,
        #[serde(default = "default_scale")]
        scale: f32,
    },
}

impl InputAction {
    pub fn kind(&self) -> InputActionKind {
        match self {
            Self::Button { .. } => InputActionKind::Button,
            Self::Axis { .. } => InputActionKind::Axis,
            Self::Vector2 { .. } => InputActionKind::Vector2,
        }
    }
}

impl PartialEq for InputAction {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Button { inputs: a }, Self::Button { inputs: b }) => a == b,
            (
                Self::Axis {
                    negative: an,
                    positive: ap,
                    analog: aa,
                    scale: ascale,
                },
                Self::Axis {
                    negative: bn,
                    positive: bp,
                    analog: ba,
                    scale: bscale,
                },
            ) => an == bn && ap == bp && aa == ba && ascale.to_bits() == bscale.to_bits(),
            (
                Self::Vector2 {
                    up: au,
                    down: ad,
                    left: al,
                    right: ar,
                    analog: aa,
                    scale: ascale,
                },
                Self::Vector2 {
                    up: bu,
                    down: bd,
                    left: bl,
                    right: br,
                    analog: ba,
                    scale: bscale,
                },
            ) => {
                au == bu
                    && ad == bd
                    && al == bl
                    && ar == br
                    && aa == ba
                    && ascale.to_bits() == bscale.to_bits()
            }
            _ => false,
        }
    }
}

impl Eq for InputAction {}

impl InputSettings {
    pub fn validate(&self) -> Result<(), String> {
        self.gamepad.validate()?;
        if self.contexts.len() > MAX_CONTEXTS {
            return Err(format!(
                "input.contexts не может содержать больше {MAX_CONTEXTS} контекстов"
            ));
        }

        for (context_name, actions) in &self.contexts {
            validate_name(context_name, "контекста")?;
            if actions.len() > MAX_ACTIONS_PER_CONTEXT {
                return Err(format!(
                    "input.contexts.{context_name} не может содержать больше {MAX_ACTIONS_PER_CONTEXT} действий"
                ));
            }
        }

        match (&self.default_context, self.contexts.is_empty()) {
            (None, false) => {
                return Err("input.default_context обязателен, если заданы контексты".to_owned());
            }
            (Some(name), _) if !self.contexts.contains_key(name) => {
                return Err(format!(
                    "input.default_context ссылается на неизвестный контекст {name:?}"
                ));
            }
            _ => {}
        }

        let mut action_kinds = HashMap::new();
        for (context_name, actions) in &self.contexts {
            for (action_name, action) in actions {
                validate_name(action_name, "действия")?;
                if let Some(previous_kind) = action_kinds.insert(action_name, action.kind())
                    && previous_kind != action.kind()
                {
                    return Err(format!(
                        "input action {action_name:?} имеет разные kind в контекстах"
                    ));
                }

                match action {
                    InputAction::Button { inputs } => {
                        validate_slot(inputs, context_name, action_name, "inputs")?;
                    }
                    InputAction::Axis {
                        negative,
                        positive,
                        analog,
                        scale,
                    } => {
                        validate_slot(negative, context_name, action_name, "negative")?;
                        validate_slot(positive, context_name, action_name, "positive")?;
                        validate_scale(*scale, context_name, action_name)?;
                        if analog.as_deref().is_some_and(|name| {
                            !matches!(name, "MouseX" | "MouseY" | "WheelX" | "WheelY")
                                && !is_gamepad_axis_analog(name)
                        }) {
                            return Err(format!(
                                "input action {action_name:?} в контексте {context_name:?} имеет неверный analog для axis"
                            ));
                        }
                    }
                    InputAction::Vector2 {
                        up,
                        down,
                        left,
                        right,
                        analog,
                        scale,
                    } => {
                        validate_slot(up, context_name, action_name, "up")?;
                        validate_slot(down, context_name, action_name, "down")?;
                        validate_slot(left, context_name, action_name, "left")?;
                        validate_slot(right, context_name, action_name, "right")?;
                        validate_scale(*scale, context_name, action_name)?;
                        if analog.as_deref().is_some_and(|name| {
                            !matches!(name, "MouseDelta" | "Wheel")
                                && !is_gamepad_stick_analog(name)
                        }) {
                            return Err(format!(
                                "input action {action_name:?} в контексте {context_name:?} имеет неверный analog для vector2"
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn validate_name(name: &str, description: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(format!(
            "имя {description} {name:?} должно соответствовать [A-Za-z0-9_]{{1,64}}"
        ));
    }
    Ok(())
}

fn validate_slot(inputs: &[String], context: &str, action: &str, slot: &str) -> Result<(), String> {
    if inputs.len() > MAX_INPUTS_PER_SLOT {
        return Err(format!(
            "input action {action:?} slot {slot:?} не может содержать больше {MAX_INPUTS_PER_SLOT} вводов"
        ));
    }
    let mut seen = HashSet::new();
    for input in inputs {
        if PhysicalInput::from_name(input).is_none() {
            return Err(format!(
                "input action {action:?} в контексте {context:?} содержит неизвестный ввод {input:?}"
            ));
        }
        if !seen.insert(input) {
            return Err(format!(
                "input action {action:?} slot {slot:?} содержит повторный ввод {input:?}"
            ));
        }
    }
    Ok(())
}

fn validate_scale(scale: f32, context: &str, action: &str) -> Result<(), String> {
    if !scale.is_finite() {
        return Err(format!(
            "input action {action:?} в контексте {context:?} должен иметь конечный scale"
        ));
    }
    Ok(())
}

fn is_gamepad_axis_analog(name: &str) -> bool {
    split_gamepad_name(name).is_some_and(|(_, axis)| {
        matches!(
            axis,
            "LeftX" | "LeftY" | "RightX" | "RightY" | "LeftTrigger" | "RightTrigger"
        )
    })
}

fn is_gamepad_stick_analog(name: &str) -> bool {
    split_gamepad_name(name).is_some_and(|(_, stick)| matches!(stick, "LeftStick" | "RightStick"))
}

fn default_scale() -> f32 {
    1.0
}

fn default_stick_deadzone() -> f32 {
    0.15
}

fn default_trigger_threshold() -> f32 {
    0.5
}

#[cfg(test)]
mod gamepad_tests {
    use super::*;

    fn settings_with_analog(action: InputAction) -> InputSettings {
        InputSettings {
            default_context: Some("game".to_owned()),
            contexts: BTreeMap::from([(
                "game".to_owned(),
                BTreeMap::from([("Look".to_owned(), action)]),
            )]),
            gamepad: GamepadSettings::default(),
        }
    }

    fn axis(analog: &str) -> InputAction {
        InputAction::Axis {
            negative: Vec::new(),
            positive: Vec::new(),
            analog: Some(analog.to_owned()),
            scale: 1.0,
        }
    }

    fn vector(analog: &str) -> InputAction {
        InputAction::Vector2 {
            up: Vec::new(),
            down: Vec::new(),
            left: Vec::new(),
            right: Vec::new(),
            analog: Some(analog.to_owned()),
            scale: 1.0,
        }
    }

    #[test]
    fn gamepad_defaults_are_used_without_a_section() {
        let settings: InputSettings = toml::from_str("").unwrap();
        assert_eq!(settings.gamepad.stick_deadzone, 0.15);
        assert_eq!(settings.gamepad.trigger_threshold, 0.5);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn rejects_non_finite_and_out_of_range_gamepad_tuning() {
        let invalid_deadzones = [f32::NAN, f32::INFINITY, -0.1, 1.0, 2.0];
        for deadzone in invalid_deadzones {
            let settings = InputSettings {
                gamepad: GamepadSettings {
                    stick_deadzone: deadzone,
                    ..GamepadSettings::default()
                },
                ..InputSettings::default()
            };
            assert!(settings.validate().is_err(), "deadzone {deadzone}");
        }
        let invalid_thresholds = [f32::NAN, f32::NEG_INFINITY, 0.0, -0.5, 1.0, 1.5];
        for threshold in invalid_thresholds {
            let settings = InputSettings {
                gamepad: GamepadSettings {
                    trigger_threshold: threshold,
                    ..GamepadSettings::default()
                },
                ..InputSettings::default()
            };
            assert!(settings.validate().is_err(), "threshold {threshold}");
        }
    }

    #[test]
    fn accepts_gamepad_analog_names_for_matching_action_kinds() {
        for name in [
            "Gamepad2.LeftTrigger",
            "Gamepad.LeftX",
            "Gamepad1.RightY",
            "Gamepad4.RightTrigger",
        ] {
            assert!(
                settings_with_analog(axis(name)).validate().is_ok(),
                "{name}"
            );
        }
        for name in ["Gamepad.LeftStick", "Gamepad3.RightStick"] {
            assert!(
                settings_with_analog(vector(name)).validate().is_ok(),
                "{name}"
            );
        }
    }

    #[test]
    fn rejects_gamepad_analog_names_with_wrong_kind_or_slot() {
        for name in [
            "Gamepad.LeftStick",
            "Gamepad.South",
            "Gamepad0.LeftX",
            "Gamepad5.LeftX",
            "LeftX",
        ] {
            assert!(
                settings_with_analog(axis(name)).validate().is_err(),
                "{name}"
            );
        }
        for name in ["Gamepad.LeftX", "Gamepad9.LeftStick", "Gamepad.Trigger"] {
            assert!(
                settings_with_analog(vector(name)).validate().is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn gamepad_settings_round_trip_through_toml() {
        let settings = InputSettings {
            gamepad: GamepadSettings {
                stick_deadzone: 0.25,
                trigger_threshold: 0.75,
            },
            ..InputSettings::default()
        };
        let serialized = toml::to_string(&settings).unwrap();
        let reloaded: InputSettings = toml::from_str(&serialized).unwrap();
        assert_eq!(reloaded, settings);
        assert!(serialized.contains("[gamepad]"));
    }
}

#[cfg(test)]
mod tests {
    use super::{InputAction, InputActionKind, InputSettings};
    use std::collections::BTreeMap;

    fn settings(input: &str) -> String {
        format!(
            "id = \"demo\"\nname = \"Demo\"\nversion = \"1\"\n[[scenes]]\npath = \"scene.rvescene\"\n{input}"
        )
    }

    #[test]
    fn parses_all_action_kinds_and_default_slot_values() {
        let parsed = InputSettings {
            default_context: Some("game".to_owned()),
            contexts: BTreeMap::from([(
                "game".to_owned(),
                BTreeMap::from([
                    (
                        "Jump".to_owned(),
                        InputAction::Button {
                            inputs: vec!["Space".to_owned(), "Mouse.Left".to_owned()],
                        },
                    ),
                    (
                        "Throttle".to_owned(),
                        InputAction::Axis {
                            negative: vec!["Q".to_owned()],
                            positive: vec!["E".to_owned()],
                            analog: Some("WheelY".to_owned()),
                            scale: 1.0,
                        },
                    ),
                    (
                        "Move".to_owned(),
                        InputAction::Vector2 {
                            up: vec!["W".to_owned()],
                            down: vec!["S".to_owned()],
                            left: vec!["A".to_owned()],
                            right: vec!["D".to_owned()],
                            analog: None,
                            scale: 1.0,
                        },
                    ),
                ]),
            )]),
            gamepad: Default::default(),
        };
        let text = toml::to_string(&parsed).unwrap();
        let decoded: InputSettings = toml::from_str(&text).unwrap();
        assert_eq!(decoded, parsed);
        assert_eq!(
            decoded.contexts["game"]["Jump"].kind(),
            InputActionKind::Button
        );

        let defaulted: InputSettings = toml::from_str(
            "[contexts.game.empty]\nkind = \"button\"\n[contexts.game.axis]\nkind = \"axis\"\n",
        )
        .unwrap();
        assert!(
            matches!(defaulted.contexts["game"]["empty"], InputAction::Button { ref inputs } if inputs.is_empty())
        );
        assert!(
            matches!(defaulted.contexts["game"]["axis"], InputAction::Axis { scale, ref negative, ref positive, .. } if scale == 1.0 && negative.is_empty() && positive.is_empty())
        );
    }

    #[test]
    fn validates_limits_names_default_context_kinds_inputs_slots_and_scale() {
        let base = settings(
            "[input]\ndefault_context = \"game\"\n[input.contexts.game.Jump]\nkind = \"button\"\ninputs = [\"Space\"]\n",
        );
        assert!(crate::ProjectSettings::parse(&base).is_ok());

        let invalid_inputs = [
            (
                "[input.contexts.game.Jump]\nkind = \"button\"\n",
                "default_context",
            ),
            (
                "[input]\ndefault_context = \"missing\"\n[input.contexts.game.Jump]\nkind = \"button\"\n",
                "неизвестный контекст",
            ),
            (
                "[input]\ndefault_context = \"game\"\n[input.contexts.game.bad-name]\nkind = \"button\"\n",
                "имя",
            ),
            (
                "[input]\ndefault_context = \"game\"\n[input.contexts.bad-name.Jump]\nkind = \"button\"\n",
                "имя",
            ),
            (
                "[input]\ndefault_context = \"game\"\n[input.contexts.game.Jump]\nkind = \"button\"\ninputs = [\"Mouse.Left\", \"Mouse.Left\"]\n",
                "повторный",
            ),
            (
                "[input]\ndefault_context = \"game\"\n[input.contexts.game.Jump]\nkind = \"button\"\ninputs = [\"Unknown\"]\n",
                "неизвестный ввод",
            ),
            (
                "[input]\ndefault_context = \"game\"\n[input.contexts.game.Jump]\nkind = \"button\"\npositive = [\"A\"]\n",
                "unknown field",
            ),
            (
                "[input]\ndefault_context = \"game\"\nextra = true\n[input.contexts.game.Jump]\nkind = \"button\"\n",
                "unknown field",
            ),
            (
                "[input]\ndefault_context = \"game\"\n[input.contexts.game.Jump]\nkind = \"button\"\ninputs = [\"A\"]\n[input.contexts.menu.Jump]\nkind = \"axis\"\n",
                "разные kind",
            ),
            (
                "[input]\ndefault_context = \"game\"\n[input.contexts.game.Throttle]\nkind = \"axis\"\nanalog = \"MouseDelta\"\n",
                "неверный analog",
            ),
            (
                "[input]\ndefault_context = \"game\"\n[input.contexts.game.Look]\nkind = \"vector2\"\nanalog = \"WheelY\"\n",
                "неверный analog",
            ),
            (
                "[input]\ndefault_context = \"game\"\n[input.contexts.game.Throttle]\nkind = \"axis\"\nscale = inf\n",
                "конечный scale",
            ),
        ];
        for (input, expected) in invalid_inputs {
            let error = crate::ProjectSettings::parse(&settings(input)).unwrap_err();
            assert!(
                error.contains(expected),
                "expected {expected:?}, got {error}"
            );
        }

        let too_many_contexts = (0..33)
            .map(|index| format!("[input.contexts.c{index}.A]\nkind = \"button\"\n"))
            .collect::<String>();
        let contexts = format!("[input]\ndefault_context = \"c0\"\n{too_many_contexts}");
        assert!(
            crate::ProjectSettings::parse(&settings(&contexts))
                .unwrap_err()
                .contains("32 контекстов")
        );

        let too_many_actions = (0..129)
            .map(|index| format!("[input.contexts.game.A{index}]\nkind = \"button\"\n"))
            .collect::<String>();
        let actions = format!("[input]\ndefault_context = \"game\"\n{too_many_actions}");
        assert!(
            crate::ProjectSettings::parse(&settings(&actions))
                .unwrap_err()
                .contains("128 действий")
        );

        let too_many_inputs = (0..17)
            .map(|index| format!("\"{}\"", char::from(b'A' + index as u8)))
            .collect::<Vec<_>>()
            .join(", ");
        let slot = format!(
            "[input]\ndefault_context = \"game\"\n[input.contexts.game.Jump]\nkind = \"button\"\ninputs = [{too_many_inputs}]\n"
        );
        assert!(
            crate::ProjectSettings::parse(&settings(&slot))
                .unwrap_err()
                .contains("16 вводов")
        );
    }

    #[test]
    fn accepts_exact_input_limits_and_name_length() {
        let contexts = (0..32)
            .map(|index| format!("[input.contexts.c{index}.Action]\nkind = \"button\"\n"))
            .collect::<String>();
        assert!(
            crate::ProjectSettings::parse(&settings(&format!(
                "[input]\ndefault_context = \"c0\"\n{contexts}"
            )))
            .is_ok()
        );

        let actions = (0..128)
            .map(|index| format!("[input.contexts.game.Action{index}]\nkind = \"button\"\n"))
            .collect::<String>();
        assert!(
            crate::ProjectSettings::parse(&settings(&format!(
                "[input]\ndefault_context = \"game\"\n{actions}"
            )))
            .is_ok()
        );

        let inputs = (0..16)
            .map(|index| format!("\"{}\"", char::from(b'A' + index as u8)))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(crate::ProjectSettings::parse(&settings(&format!(
            "[input]\ndefault_context = \"game\"\n[input.contexts.game.Jump]\nkind = \"button\"\ninputs = [{inputs}]\n"
        )))
        .is_ok());

        let context = "c".repeat(64);
        let action = "a".repeat(64);
        let names = format!(
            "[input]\ndefault_context = \"{context}\"\n[input.contexts.\"{context}\".\"{action}\"]\nkind = \"button\"\n"
        );
        assert!(crate::ProjectSettings::parse(&settings(&names)).is_ok());

        let too_long = "a".repeat(65);
        let names = format!(
            "[input]\ndefault_context = \"game\"\n[input.contexts.game.\"{too_long}\"]\nkind = \"button\"\n"
        );
        assert!(
            crate::ProjectSettings::parse(&settings(&names))
                .unwrap_err()
                .contains("имя действия")
        );
    }

    #[test]
    fn project_without_input_section_keeps_empty_input_settings() {
        let parsed = crate::ProjectSettings::parse(&settings("")).unwrap();
        assert_eq!(parsed.input, InputSettings::default());
    }

    #[test]
    fn project_input_settings_round_trip_through_to_toml() {
        let source = settings(
            "[input]\ndefault_context = \"game\"\n[input.contexts.game.Jump]\nkind = \"button\"\ninputs = [\"Space\"]\n[input.contexts.game.Look]\nkind = \"vector2\"\nanalog = \"MouseDelta\"\nscale = 0.1\n",
        );
        let parsed = crate::ProjectSettings::parse(&source).unwrap();
        let serialized = parsed.to_toml().unwrap();
        assert_eq!(crate::ProjectSettings::parse(&serialized).unwrap(), parsed);
    }
}
