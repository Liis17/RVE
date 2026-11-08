use crate::types::ScriptInput;
use engine_project::{InputAction, InputActionKind, InputSettings};
use rve_sdk::{Key, MouseButton, PhysicalInput};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
const MAX_INPUTS_PER_SLOT: usize = rve_sdk::abi::RVE_INPUT_MAX_BINDINGS;

#[derive(Clone, Debug, PartialEq)]
struct ActionDefinition {
    kind: InputActionKind,
    slots: BTreeMap<String, Vec<PhysicalInput>>,
    analog: Option<AnalogSource>,
    scale: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct ActionDefinitions {
    contexts: BTreeMap<String, BTreeMap<String, ActionDefinition>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AnalogSource {
    MouseDelta,
    Wheel,
    MouseX,
    MouseY,
    WheelX,
    WheelY,
}

impl AnalogSource {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "MouseDelta" => Some(Self::MouseDelta),
            "Wheel" => Some(Self::Wheel),
            "MouseX" => Some(Self::MouseX),
            "MouseY" => Some(Self::MouseY),
            "WheelX" => Some(Self::WheelX),
            "WheelY" => Some(Self::WheelY),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct ActionCatalog {
    contexts: BTreeMap<String, BTreeMap<String, InputActionKind>>,
    action_kinds: BTreeMap<String, InputActionKind>,
    action_indices: BTreeMap<String, usize>,
    binding_indices: BTreeMap<String, BTreeMap<String, usize>>,
    action_count: usize,
    binding_count: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ActionValue {
    pub(crate) down: bool,
    pub(crate) pressed: bool,
    pub(crate) released: bool,
    pub(crate) value: [f32; 2],
}

/// Immutable action values and binding definitions for one input frame.
#[derive(Clone, Debug, Default)]
pub(crate) struct ActionFrame {
    input: ScriptInput,
    context: Option<Arc<str>>,
    values: Vec<ActionValue>,
    definitions: Arc<ActionDefinitions>,
    catalog: Arc<ActionCatalog>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ActionMutation {
    SetContext(String),
    Rebind {
        context: String,
        action: String,
        slot: String,
        inputs: Vec<PhysicalInput>,
    },
    ResetBindings,
}

#[derive(Clone, Debug)]
pub(crate) struct InputActions {
    base: Arc<ActionDefinitions>,
    definitions: Arc<ActionDefinitions>,
    catalog: Arc<ActionCatalog>,
    context: Option<Arc<str>>,
    frame: Arc<ActionFrame>,
    context_suppression: InputMask,
    binding_suppression: Vec<InputMask>,
    changed_bindings: Vec<bool>,
    active_actions: Vec<bool>,
    tombstones: Vec<usize>,
    pending_mutations: Vec<ActionMutation>,
}

#[derive(Clone, Copy, Debug, Default)]
struct InputMask {
    keys: u128,
    mouse: u8,
}

impl InputActions {
    pub(crate) fn new(settings: InputSettings) -> Result<Self, String> {
        settings.validate()?;
        let definitions = Arc::new(compile_definitions(&settings)?);
        let catalog = Arc::new(compile_catalog(&definitions));
        let context = settings.default_context.map(Arc::from);
        let binding_count = catalog.binding_count;
        let action_count = catalog.action_count;
        let frame = Arc::new(ActionFrame {
            input: ScriptInput::default(),
            context: context.clone(),
            values: vec![ActionValue::default(); action_count],
            definitions: Arc::clone(&definitions),
            catalog: Arc::clone(&catalog),
        });
        Ok(Self {
            base: Arc::clone(&definitions),
            definitions,
            catalog,
            context,
            frame,
            context_suppression: InputMask::default(),
            binding_suppression: vec![InputMask::default(); binding_count],
            changed_bindings: vec![false; binding_count],
            active_actions: vec![false; action_count],
            tombstones: Vec::with_capacity(action_count),
            pending_mutations: Vec::new(),
        })
    }

    pub(crate) fn frame(&self) -> Arc<ActionFrame> {
        Arc::clone(&self.frame)
    }

    pub(crate) fn queue_mutations(&mut self, mutations: Vec<ActionMutation>) {
        self.pending_mutations.extend(mutations);
    }

    pub(crate) fn set_input(&mut self, input: ScriptInput) {
        let held = unreleased_down_inputs(input);
        self.context_suppression.keys &= held.keys;
        self.context_suppression.mouse &= held.mouse;
        for suppression in &mut self.binding_suppression {
            suppression.keys &= held.keys;
            suppression.mouse &= held.mouse;
        }

        self.changed_bindings.fill(false);
        self.active_actions.fill(false);
        self.tombstones.clear();
        let old_context = self.context.clone();
        let mut context = old_context.clone();
        let mut definitions = None;
        for mutation in self.pending_mutations.drain(..) {
            match mutation {
                ActionMutation::SetContext(name) => context = Some(Arc::from(name)),
                ActionMutation::Rebind {
                    context,
                    action,
                    slot,
                    inputs,
                } => {
                    definitions
                        .get_or_insert_with(|| (*self.definitions).clone())
                        .contexts
                        .get_mut(&context)
                        .unwrap()
                        .get_mut(&action)
                        .unwrap()
                        .slots
                        .insert(slot, inputs);
                }
                ActionMutation::ResetBindings => definitions = Some((*self.base).clone()),
            }
        }

        let changed_context = context != old_context;
        let changed_actions = definitions
            .as_ref()
            .map(|definitions| changed_action_bindings(&self.definitions, definitions));
        let definitions = if let Some(definitions) = definitions {
            if changed_actions.as_ref().is_some_and(BTreeSet::is_empty) {
                Arc::clone(&self.definitions)
            } else {
                Arc::new(definitions)
            }
        } else {
            Arc::clone(&self.definitions)
        };
        let changed_actions = changed_actions.filter(|actions| !actions.is_empty());

        if let Some(changed_actions) = &changed_actions {
            for (context_name, action_name) in changed_actions {
                let index = self.catalog.binding_indices[context_name][action_name];
                self.changed_bindings[index] = true;
            }
        }

        if changed_context {
            for (index, value) in self.frame.values.iter().enumerate() {
                if value.down {
                    self.tombstones.push(index);
                }
            }
            self.context_suppression.keys |= held.keys;
            self.context_suppression.mouse |= held.mouse;
        } else if let Some(changed_actions) = &changed_actions {
            for (changed_context, action) in changed_actions {
                if context.as_deref() == Some(changed_context.as_str()) {
                    let action_index = self.catalog.action_indices[action];
                    if self.frame.values[action_index].down
                        && !self.tombstones.contains(&action_index)
                    {
                        self.tombstones.push(action_index);
                    }
                    let binding_index = self.catalog.binding_indices[changed_context][action];
                    let definition = &definitions.contexts[changed_context][action];
                    let bound = bound_inputs(definition);
                    self.binding_suppression[binding_index].keys |= held.keys & bound.keys;
                    self.binding_suppression[binding_index].mouse |= held.mouse & bound.mouse;
                }
            }
        }

        let frame = Arc::make_mut(&mut self.frame);
        if changed_context {
            frame.values.fill(ActionValue::default());
        }
        if let Some(context_name) = context.as_deref()
            && let Some(actions) = definitions.contexts.get(context_name)
        {
            for (name, definition) in actions {
                let action_index = self.catalog.action_indices[name];
                self.active_actions[action_index] = true;
                let binding_index = self.catalog.binding_indices[context_name][name];
                let previous_down = !changed_context
                    && !self.changed_bindings[binding_index]
                    && frame.values[action_index].down;
                let suppression = self.binding_suppression[binding_index];
                frame.values[action_index] = evaluate_action(
                    input,
                    definition,
                    previous_down,
                    self.context_suppression.keys | suppression.keys,
                    self.context_suppression.mouse | suppression.mouse,
                );
            }
        }
        for (index, active) in self.active_actions.iter().enumerate() {
            if !active {
                frame.values[index] = ActionValue::default();
            }
        }
        for index in &self.tombstones {
            frame.values[*index] = ActionValue {
                released: true,
                ..ActionValue::default()
            };
        }

        self.context = context;
        self.definitions = definitions;
        frame.input = input;
        frame.context = self.context.clone();
        frame.definitions = Arc::clone(&self.definitions);
    }

    pub(crate) fn clear_frame_transients(&mut self) {
        let mut input = self.frame.input;
        input.clear_frame_transients();
        let definitions = &self.definitions;
        let catalog = &self.catalog;
        let context_suppression = self.context_suppression;
        let binding_suppression = &self.binding_suppression;
        self.active_actions.fill(false);
        let frame = Arc::make_mut(&mut self.frame);
        if let Some(context) = self.context.as_deref()
            && let Some(actions) = definitions.contexts.get(context)
        {
            for (name, definition) in actions {
                let action_index = catalog.action_indices[name];
                self.active_actions[action_index] = true;
                let binding_index = catalog.binding_indices[context][name];
                let previous_down = frame.values[action_index].down;
                let suppression = binding_suppression[binding_index];
                frame.values[action_index] = evaluate_action(
                    input,
                    definition,
                    previous_down,
                    context_suppression.keys | suppression.keys,
                    context_suppression.mouse | suppression.mouse,
                );
            }
        }
        for (index, active) in self.active_actions.iter().enumerate() {
            if !active {
                frame.values[index] = ActionValue::default();
            }
        }
        frame.input = input;
        frame.context = self.context.clone();
        frame.definitions = Arc::clone(&self.definitions);
    }
}

impl Default for InputActions {
    fn default() -> Self {
        Self::new(InputSettings::default()).expect("empty input settings are valid")
    }
}

impl ActionFrame {
    #[cfg(test)]
    pub(crate) fn empty(input: ScriptInput) -> Self {
        Self {
            input,
            ..Self::default()
        }
    }

    pub(crate) fn input(&self) -> &ScriptInput {
        &self.input
    }
    pub(crate) fn context(&self) -> Option<&str> {
        self.context.as_deref()
    }
    pub(crate) fn state(
        &self,
        name: &str,
        expected_kind: InputActionKind,
    ) -> Result<ActionValue, String> {
        let kind = self
            .catalog
            .action_kinds
            .get(name)
            .ok_or_else(|| format!("unknown input action {name:?}"))?;
        if *kind != expected_kind {
            return Err(format!(
                "input action {name:?} has kind {kind:?}, expected {expected_kind:?}"
            ));
        }
        let index = self.catalog.action_indices[name];
        Ok(self.values[index])
    }
    pub(crate) fn bindings(
        &self,
        context: &str,
        action: &str,
        slot: &str,
    ) -> Result<Vec<String>, String> {
        let actions = self
            .catalog
            .contexts
            .get(context)
            .ok_or_else(|| format!("unknown input context {context:?}"))?;
        let kind = actions
            .get(action)
            .ok_or_else(|| format!("unknown input action {action:?} in context {context:?}"))?;
        validate_slot_name(*kind, slot)?;
        let values = &self.definitions.contexts[context][action].slots[slot];
        Ok(values.iter().copied().map(PhysicalInput::name).collect())
    }
    pub(crate) fn validate_mutation(&self, mutation: &ActionMutation) -> Result<(), String> {
        match mutation {
            ActionMutation::SetContext(name) => {
                if !self.catalog.contexts.contains_key(name) {
                    return Err(format!("unknown input context {name:?}"));
                }
            }
            ActionMutation::Rebind {
                context,
                action,
                slot,
                inputs,
            } => {
                let actions = self
                    .catalog
                    .contexts
                    .get(context)
                    .ok_or_else(|| format!("unknown input context {context:?}"))?;
                let kind = actions.get(action).ok_or_else(|| {
                    format!("unknown input action {action:?} in context {context:?}")
                })?;
                validate_slot_name(*kind, slot)?;
                if inputs.len() > MAX_INPUTS_PER_SLOT {
                    return Err(format!("input slot exceeds {MAX_INPUTS_PER_SLOT} bindings"));
                }
                let mut seen = HashSet::new();
                if inputs.iter().any(|input| !seen.insert(*input)) {
                    return Err("input slot contains a duplicate binding".to_owned());
                }
            }
            ActionMutation::ResetBindings => {}
        }
        Ok(())
    }
}

fn compile_definitions(settings: &InputSettings) -> Result<ActionDefinitions, String> {
    let mut contexts = BTreeMap::new();
    for (context_name, actions) in &settings.contexts {
        let mut compiled_actions = BTreeMap::new();
        for (action_name, action) in actions {
            let definition = match action {
                InputAction::Button { inputs } => ActionDefinition {
                    kind: InputActionKind::Button,
                    slots: BTreeMap::from([("inputs".to_owned(), parse_inputs(inputs)?)]),
                    analog: None,
                    scale: 1.0,
                },
                InputAction::Axis {
                    negative,
                    positive,
                    analog,
                    scale,
                } => ActionDefinition {
                    kind: InputActionKind::Axis,
                    slots: BTreeMap::from([
                        ("negative".to_owned(), parse_inputs(negative)?),
                        ("positive".to_owned(), parse_inputs(positive)?),
                    ]),
                    analog: analog.as_deref().and_then(AnalogSource::parse),
                    scale: *scale,
                },
                InputAction::Vector2 {
                    up,
                    down,
                    left,
                    right,
                    analog,
                    scale,
                } => ActionDefinition {
                    kind: InputActionKind::Vector2,
                    slots: BTreeMap::from([
                        ("up".to_owned(), parse_inputs(up)?),
                        ("down".to_owned(), parse_inputs(down)?),
                        ("left".to_owned(), parse_inputs(left)?),
                        ("right".to_owned(), parse_inputs(right)?),
                    ]),
                    analog: analog.as_deref().and_then(AnalogSource::parse),
                    scale: *scale,
                },
            };
            compiled_actions.insert(action_name.clone(), definition);
        }
        contexts.insert(context_name.clone(), compiled_actions);
    }
    Ok(ActionDefinitions { contexts })
}

fn compile_catalog(definitions: &ActionDefinitions) -> ActionCatalog {
    let mut contexts = BTreeMap::new();
    let mut action_kinds = BTreeMap::new();
    let mut action_indices = BTreeMap::new();
    let mut binding_indices = BTreeMap::new();
    let mut binding_count = 0;
    for (context_name, actions) in &definitions.contexts {
        let mut context_actions = BTreeMap::new();
        let mut context_indices = BTreeMap::new();
        for (name, definition) in actions {
            action_kinds.insert(name.clone(), definition.kind);
            let action_index = action_indices.len();
            action_indices.entry(name.clone()).or_insert(action_index);
            context_actions.insert(name.clone(), definition.kind);
            context_indices.insert(name.clone(), binding_count);
            binding_count += 1;
        }
        contexts.insert(context_name.clone(), context_actions);
        binding_indices.insert(context_name.clone(), context_indices);
    }
    ActionCatalog {
        contexts,
        action_kinds,
        action_count: action_indices.len(),
        binding_count,
        action_indices,
        binding_indices,
    }
}

fn parse_inputs(inputs: &[String]) -> Result<Vec<PhysicalInput>, String> {
    inputs
        .iter()
        .map(|name| {
            PhysicalInput::from_name(name).ok_or_else(|| format!("unknown physical input {name:?}"))
        })
        .collect()
}

fn changed_action_bindings(
    old: &ActionDefinitions,
    new: &ActionDefinitions,
) -> BTreeSet<(String, String)> {
    let mut changed = BTreeSet::new();
    for context in old.contexts.keys().chain(new.contexts.keys()) {
        let old_actions = old.contexts.get(context);
        let new_actions = new.contexts.get(context);
        if let Some(actions) = old_actions {
            for action in actions.keys() {
                if old_actions.and_then(|actions| actions.get(action))
                    != new_actions.and_then(|actions| actions.get(action))
                {
                    changed.insert((context.clone(), action.clone()));
                }
            }
        }
        if let Some(actions) = new_actions {
            for action in actions.keys() {
                if old_actions.and_then(|actions| actions.get(action))
                    != new_actions.and_then(|actions| actions.get(action))
                {
                    changed.insert((context.clone(), action.clone()));
                }
            }
        }
    }
    changed
}

fn evaluate_action(
    input: ScriptInput,
    definition: &ActionDefinition,
    previous_down: bool,
    suppressed_keys: u128,
    suppressed_mouse: u8,
) -> ActionValue {
    match definition.kind {
        InputActionKind::Button => {
            let mut down = false;
            let mut any_pressed = false;
            for binding in &definition.slots["inputs"] {
                if is_suppressed(*binding, suppressed_keys, suppressed_mouse) {
                    continue;
                }
                let state = physical_state(input, *binding);
                down |= state & RVE_INPUT_DOWN != 0;
                any_pressed |= state & RVE_INPUT_PRESSED != 0;
            }
            ActionValue {
                down,
                pressed: !previous_down && (down || any_pressed),
                released: !down && (previous_down || any_pressed),
                value: [if down { 1.0 } else { 0.0 }, 0.0],
            }
        }
        InputActionKind::Axis => {
            let negative = slot_down(
                input,
                &definition.slots["negative"],
                suppressed_keys,
                suppressed_mouse,
            );
            let positive = slot_down(
                input,
                &definition.slots["positive"],
                suppressed_keys,
                suppressed_mouse,
            );
            let analog = match definition.analog {
                Some(AnalogSource::MouseX) => input.mouse_delta()[0],
                Some(AnalogSource::MouseY) => input.mouse_delta()[1],
                Some(AnalogSource::WheelX) => input.wheel()[0],
                Some(AnalogSource::WheelY) => input.wheel()[1],
                _ => 0.0,
            };
            ActionValue {
                value: [
                    (if positive { 1.0 } else { 0.0 }) - (if negative { 1.0 } else { 0.0 })
                        + analog * definition.scale,
                    0.0,
                ],
                ..ActionValue::default()
            }
        }
        InputActionKind::Vector2 => {
            let right = slot_down(
                input,
                &definition.slots["right"],
                suppressed_keys,
                suppressed_mouse,
            );
            let left = slot_down(
                input,
                &definition.slots["left"],
                suppressed_keys,
                suppressed_mouse,
            );
            let up = slot_down(
                input,
                &definition.slots["up"],
                suppressed_keys,
                suppressed_mouse,
            );
            let down = slot_down(
                input,
                &definition.slots["down"],
                suppressed_keys,
                suppressed_mouse,
            );
            let mut x = (if right { 1.0 } else { 0.0 }) - (if left { 1.0 } else { 0.0 });
            let mut y = (if up { 1.0 } else { 0.0 }) - (if down { 1.0 } else { 0.0 });
            let length_squared: f32 = x * x + y * y;
            if length_squared > 1.0 {
                let inverse_length = length_squared.sqrt().recip();
                x *= inverse_length;
                y *= inverse_length;
            }
            let analog = match definition.analog {
                Some(AnalogSource::MouseDelta) => input.mouse_delta(),
                Some(AnalogSource::Wheel) => input.wheel(),
                _ => [0.0; 2],
            };
            ActionValue {
                value: [
                    x + analog[0] * definition.scale,
                    y + analog[1] * definition.scale,
                ],
                ..ActionValue::default()
            }
        }
    }
}

fn slot_down(
    input: ScriptInput,
    bindings: &[PhysicalInput],
    suppressed_keys: u128,
    suppressed_mouse: u8,
) -> bool {
    bindings.iter().any(|binding| {
        !is_suppressed(*binding, suppressed_keys, suppressed_mouse)
            && physical_state(input, *binding) & RVE_INPUT_DOWN != 0
    })
}

fn physical_state(input: ScriptInput, binding: PhysicalInput) -> u32 {
    match binding {
        PhysicalInput::Key(key) => input.key_state(key as u32),
        PhysicalInput::MouseButton(button) => input.mouse_button_state(button as u32),
        PhysicalInput::GamepadButton { .. } => 0,
    }
}

fn is_suppressed(binding: PhysicalInput, keys: u128, mouse: u8) -> bool {
    match binding {
        PhysicalInput::Key(key) => keys & (1_u128 << key as u32) != 0,
        PhysicalInput::MouseButton(button) => mouse & (1_u8 << button as u32) != 0,
        PhysicalInput::GamepadButton { .. } => false,
    }
}

fn unreleased_down_inputs(input: ScriptInput) -> InputMask {
    let mut held = InputMask::default();
    for code in 0..=Key::SuperRight as u32 {
        let state = input.key_state(code);
        if state & RVE_INPUT_DOWN != 0 && state & RVE_INPUT_RELEASED == 0 {
            held.keys |= 1_u128 << code;
        }
    }
    for code in 0..=MouseButton::Forward as u32 {
        let state = input.mouse_button_state(code);
        if state & RVE_INPUT_DOWN != 0 && state & RVE_INPUT_RELEASED == 0 {
            held.mouse |= 1_u8 << code;
        }
    }
    held
}

fn bound_inputs(definition: &ActionDefinition) -> InputMask {
    let mut bound = InputMask::default();
    for inputs in definition.slots.values() {
        for input in inputs {
            match input {
                PhysicalInput::Key(key) => bound.keys |= 1_u128 << *key as u32,
                PhysicalInput::MouseButton(button) => bound.mouse |= 1_u8 << *button as u32,
                PhysicalInput::GamepadButton { .. } => {}
            }
        }
    }
    bound
}
fn validate_slot_name(kind: InputActionKind, slot: &str) -> Result<(), String> {
    let valid = match kind {
        InputActionKind::Button => slot == "inputs",
        InputActionKind::Axis => matches!(slot, "negative" | "positive"),
        InputActionKind::Vector2 => matches!(slot, "up" | "down" | "left" | "right"),
    };
    if valid {
        Ok(())
    } else {
        Err(format!("unknown {kind:?} input slot {slot:?}"))
    }
}

const RVE_INPUT_DOWN: u32 = 1;
const RVE_INPUT_PRESSED: u32 = 2;
const RVE_INPUT_RELEASED: u32 = 4;

#[cfg(test)]
mod tests {
    use super::{ActionMutation, InputActions};
    use crate::types::ScriptInput;
    use engine_project::{InputAction, InputActionKind, InputSettings};
    use rve_sdk::{Key, PhysicalInput};
    use std::collections::BTreeMap;

    fn settings() -> InputSettings {
        InputSettings {
            default_context: Some("game".to_owned()),
            contexts: BTreeMap::from([
                (
                    "game".to_owned(),
                    BTreeMap::from([
                        (
                            "Jump".to_owned(),
                            InputAction::Button {
                                inputs: vec!["Space".to_owned(), "W".to_owned()],
                            },
                        ),
                        (
                            "Fire".to_owned(),
                            InputAction::Button {
                                inputs: vec!["A".to_owned(), "B".to_owned()],
                            },
                        ),
                        (
                            "Climb".to_owned(),
                            InputAction::Button {
                                inputs: vec!["W".to_owned()],
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
                        (
                            "Look".to_owned(),
                            InputAction::Vector2 {
                                up: vec![],
                                down: vec![],
                                left: vec![],
                                right: vec![],
                                analog: Some("MouseDelta".to_owned()),
                                scale: 0.5,
                            },
                        ),
                        (
                            "Throttle".to_owned(),
                            InputAction::Axis {
                                negative: vec!["Q".to_owned()],
                                positive: vec!["E".to_owned()],
                                analog: Some("WheelY".to_owned()),
                                scale: 2.0,
                            },
                        ),
                    ]),
                ),
                (
                    "menu".to_owned(),
                    BTreeMap::from([
                        (
                            "Jump".to_owned(),
                            InputAction::Button {
                                inputs: vec!["Escape".to_owned()],
                            },
                        ),
                        (
                            "Back".to_owned(),
                            InputAction::Button {
                                inputs: vec!["Escape".to_owned()],
                            },
                        ),
                        (
                            "Click".to_owned(),
                            InputAction::Button {
                                inputs: vec!["Mouse.Left".to_owned()],
                            },
                        ),
                    ]),
                ),
            ]),
        }
    }

    fn actions() -> InputActions {
        InputActions::new(settings()).unwrap()
    }

    fn physical(name: &str) -> PhysicalInput {
        PhysicalInput::from_name(name).unwrap()
    }

    #[test]
    fn button_actions_aggregate_taps_and_final_down_state() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::W, true);
        input.set_key(Key::W, false);
        input.set_key(Key::Space, true);
        actions.set_input(input.take_frame());

        let jump = actions
            .frame()
            .state("Jump", InputActionKind::Button)
            .unwrap();
        assert!(jump.down);
        assert!(jump.pressed);
        assert!(!jump.released);

        input.set_key(Key::Space, false);
        actions.set_input(input.take_frame());
        let jump = actions
            .frame()
            .state("Jump", InputActionKind::Button)
            .unwrap();
        assert!(!jump.down);
        assert!(!jump.pressed);
        assert!(jump.released);

        input.set_key(Key::W, true);
        input.set_key(Key::W, false);
        actions.set_input(input.take_frame());
        let jump = actions
            .frame()
            .state("Jump", InputActionKind::Button)
            .unwrap();
        assert!(!jump.down);
        assert!(jump.pressed);
        assert!(jump.released);
    }

    #[test]
    fn press_release_press_in_one_snapshot_uses_final_down_and_formula_edges() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::A, true);
        input.set_key(Key::A, false);
        input.set_key(Key::A, true);
        actions.set_input(input.take_frame());

        let fire = actions
            .frame()
            .state("Fire", InputActionKind::Button)
            .unwrap();
        assert!(fire.down);
        assert!(fire.pressed);
        assert!(!fire.released);
    }

    #[test]
    fn known_inactive_actions_are_zero_and_unknown_or_wrong_kind_queries_fail() {
        let actions = actions();
        let frame = actions.frame();
        assert_eq!(frame.context(), Some("game"));
        assert_eq!(
            frame.state("Back", InputActionKind::Button).unwrap(),
            Default::default()
        );
        assert!(
            frame
                .state("Missing", InputActionKind::Button)
                .unwrap_err()
                .contains("unknown input action")
        );
        assert!(
            frame
                .state("Move", InputActionKind::Button)
                .unwrap_err()
                .contains("expected Button")
        );
    }

    #[test]
    fn digital_axes_cancel_and_mouse_wheel_values_scale_without_clamping() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::Q, true);
        input.set_key(Key::E, true);
        input.add_wheel(3.0, -2.0);
        actions.set_input(input.take_frame());
        assert_eq!(
            actions
                .frame()
                .state("Throttle", InputActionKind::Axis)
                .unwrap()
                .value[0],
            -4.0
        );

        input.set_key(Key::Q, false);
        actions.set_input(input.take_frame());
        assert_eq!(
            actions
                .frame()
                .state("Throttle", InputActionKind::Axis)
                .unwrap()
                .value[0],
            1.0
        );
    }

    #[test]
    fn vectors_cancel_opposites_normalize_diagonals_and_add_pointer_delta() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::W, true);
        input.set_key(Key::D, true);
        input.add_mouse_delta(2.0, -4.0);
        actions.set_input(input.take_frame());

        let movement = actions
            .frame()
            .state("Move", InputActionKind::Vector2)
            .unwrap()
            .value;
        let diagonal = std::f32::consts::FRAC_1_SQRT_2;
        assert!((movement[0] - diagonal).abs() < 0.00001);
        assert!((movement[1] - diagonal).abs() < 0.00001);
        assert_eq!(
            actions
                .frame()
                .state("Look", InputActionKind::Vector2)
                .unwrap()
                .value,
            [1.0, -2.0]
        );

        input.set_key(Key::S, true);
        input.set_key(Key::A, true);
        actions.set_input(input.take_frame());
        assert_eq!(
            actions
                .frame()
                .state("Move", InputActionKind::Vector2)
                .unwrap()
                .value,
            [0.0, 0.0]
        );
    }

    #[test]
    fn context_mutations_are_delayed_tombstone_old_actions_and_suppress_held_inputs() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::Space, true);
        actions.set_input(input.take_frame());
        assert!(
            actions
                .frame()
                .state("Jump", InputActionKind::Button)
                .unwrap()
                .down
        );

        actions.queue_mutations(vec![ActionMutation::SetContext("menu".to_owned())]);
        assert_eq!(actions.frame().context(), Some("game"));
        input.set_key(Key::Escape, true);
        actions.set_input(input.take_frame());

        let frame = actions.frame();
        assert_eq!(frame.context(), Some("menu"));
        let jump = frame.state("Jump", InputActionKind::Button).unwrap();
        assert!(!jump.down && !jump.pressed && jump.released);
        let back = frame.state("Back", InputActionKind::Button).unwrap();
        assert!(!back.down && !back.pressed && !back.released);

        actions.set_input(input.take_frame());
        let frame = actions.frame();
        assert!(
            !frame
                .state("Jump", InputActionKind::Button)
                .unwrap()
                .released
        );
        assert!(!frame.state("Back", InputActionKind::Button).unwrap().down);

        input.set_key(Key::Escape, false);
        actions.set_input(input.take_frame());
        input.set_key(Key::Escape, true);
        actions.set_input(input.take_frame());
        let back = actions
            .frame()
            .state("Back", InputActionKind::Button)
            .unwrap();
        assert!(back.down && back.pressed);
    }

    #[test]
    fn rebind_and_reset_emit_tombstones_and_suppress_held_bindings() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::Space, true);
        actions.set_input(input.take_frame());

        actions.queue_mutations(vec![ActionMutation::Rebind {
            context: "game".to_owned(),
            action: "Jump".to_owned(),
            slot: "inputs".to_owned(),
            inputs: vec![physical("D")],
        }]);
        input.set_key(Key::D, true);
        actions.set_input(input.take_frame());
        let rebound = actions
            .frame()
            .state("Jump", InputActionKind::Button)
            .unwrap();
        assert!(!rebound.down && !rebound.pressed && rebound.released);

        input.set_key(Key::Space, false);
        input.set_key(Key::D, false);
        actions.set_input(input.take_frame());
        input.set_key(Key::D, true);
        actions.set_input(input.take_frame());
        assert!(
            actions
                .frame()
                .state("Jump", InputActionKind::Button)
                .unwrap()
                .down
        );

        actions.queue_mutations(vec![ActionMutation::ResetBindings]);
        actions.set_input(input.take_frame());
        let reset = actions
            .frame()
            .state("Jump", InputActionKind::Button)
            .unwrap();
        assert!(!reset.down && !reset.pressed && reset.released);

        input.set_key(Key::D, false);
        actions.set_input(input.take_frame());
        input.set_key(Key::Space, true);
        actions.set_input(input.take_frame());
        assert!(
            actions
                .frame()
                .state("Jump", InputActionKind::Button)
                .unwrap()
                .down
        );
    }

    #[test]
    fn rebinding_inactive_context_does_not_suppress_active_held_actions() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::W, true);
        actions.set_input(input.take_frame());

        actions.queue_mutations(vec![ActionMutation::Rebind {
            context: "menu".to_owned(),
            action: "Jump".to_owned(),
            slot: "inputs".to_owned(),
            inputs: vec![physical("B")],
        }]);
        actions.set_input(input.take_frame());

        let frame = actions.frame();
        assert_eq!(frame.context(), Some("game"));
        assert_eq!(
            frame.state("Move", InputActionKind::Vector2).unwrap().value,
            [0.0, 1.0]
        );
        let climb = frame.state("Climb", InputActionKind::Button).unwrap();
        assert!(climb.down);
        assert!(!climb.pressed && !climb.released);
        let jump = frame.state("Jump", InputActionKind::Button).unwrap();
        assert!(jump.down);
        assert!(!jump.pressed && !jump.released);
    }

    #[test]
    fn rebind_and_reset_suppression_is_scoped_to_changed_actions() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::W, true);
        actions.set_input(input.take_frame());
        assert!(
            actions
                .frame()
                .state("Climb", InputActionKind::Button)
                .unwrap()
                .down
        );

        actions.queue_mutations(vec![ActionMutation::Rebind {
            context: "game".to_owned(),
            action: "Jump".to_owned(),
            slot: "inputs".to_owned(),
            inputs: vec![physical("D")],
        }]);
        input.set_key(Key::D, true);
        actions.set_input(input.take_frame());

        let frame = actions.frame();
        let jump = frame.state("Jump", InputActionKind::Button).unwrap();
        assert!(!jump.down && !jump.pressed && jump.released);
        let climb = frame.state("Climb", InputActionKind::Button).unwrap();
        assert!(climb.down);
        assert!(!climb.pressed && !climb.released);
        let movement = frame.state("Move", InputActionKind::Vector2).unwrap().value;
        let diagonal = std::f32::consts::FRAC_1_SQRT_2;
        assert!((movement[0] - diagonal).abs() < 0.00001);
        assert!((movement[1] - diagonal).abs() < 0.00001);

        actions.queue_mutations(vec![ActionMutation::ResetBindings]);
        actions.set_input(input.take_frame());

        let frame = actions.frame();
        let jump = frame.state("Jump", InputActionKind::Button).unwrap();
        assert!(!jump.down && !jump.pressed && !jump.released);
        let climb = frame.state("Climb", InputActionKind::Button).unwrap();
        assert!(climb.down);
        assert!(!climb.pressed && !climb.released);
        assert_eq!(
            frame.state("Move", InputActionKind::Vector2).unwrap().value,
            [diagonal, diagonal]
        );
    }

    #[test]
    fn context_suppression_clears_on_release_edge_before_key_and_mouse_evaluation() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::Escape, true);
        input.set_mouse_button(rve_sdk::MouseButton::Left, true);
        actions.queue_mutations(vec![ActionMutation::SetContext("menu".to_owned())]);
        actions.set_input(input.take_frame());
        assert!(
            !actions
                .frame()
                .state("Back", InputActionKind::Button)
                .unwrap()
                .down
        );
        assert!(
            !actions
                .frame()
                .state("Click", InputActionKind::Button)
                .unwrap()
                .down
        );

        input.set_key(Key::Escape, false);
        input.set_key(Key::Escape, true);
        input.set_mouse_button(rve_sdk::MouseButton::Left, false);
        input.set_mouse_button(rve_sdk::MouseButton::Left, true);
        actions.set_input(input.take_frame());

        for name in ["Back", "Click"] {
            let action = actions
                .frame()
                .state(name, InputActionKind::Button)
                .unwrap();
            assert!(
                action.down && action.pressed,
                "{name} should press on re-press"
            );
            assert!(
                !action.released,
                "{name} should not report release while down"
            );
        }
    }

    #[test]
    fn steady_state_input_keeps_compiled_definitions() {
        let mut actions = actions();
        let definitions = std::sync::Arc::as_ptr(&actions.definitions);
        let frame_definitions = std::sync::Arc::as_ptr(&actions.frame.definitions);
        let frame = std::sync::Arc::as_ptr(&actions.frame);
        let values = actions.frame.values.as_ptr();
        let values_capacity = actions.frame.values.capacity();

        actions.set_input(ScriptInput::default());

        assert!(std::ptr::eq(
            definitions,
            std::sync::Arc::as_ptr(&actions.definitions)
        ));
        assert!(std::ptr::eq(
            frame_definitions,
            std::sync::Arc::as_ptr(&actions.frame.definitions)
        ));
        assert!(std::ptr::eq(frame, std::sync::Arc::as_ptr(&actions.frame)));
        assert_eq!(values, actions.frame.values.as_ptr());
        assert_eq!(values_capacity, actions.frame.values.capacity());

        let snapshot = actions.frame();
        let snapshot_value = snapshot.state("Jump", InputActionKind::Button).unwrap();
        let mut input = ScriptInput::default();
        input.set_key(Key::Space, true);
        actions.set_input(input.take_frame());
        assert_eq!(
            snapshot.state("Jump", InputActionKind::Button).unwrap(),
            snapshot_value
        );
        assert!(
            actions
                .frame()
                .state("Jump", InputActionKind::Button)
                .unwrap()
                .down
        );

        drop(snapshot);
        let values = actions.frame.values.as_ptr();
        actions.set_input(input.take_frame());
        assert_eq!(values, actions.frame.values.as_ptr());
    }

    #[test]
    fn focus_and_background_releases_produce_action_released_edges() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::Space, true);
        actions.set_input(input.take_frame());
        input.release_all();
        actions.set_input(input.take_frame());
        assert!(
            actions
                .frame()
                .state("Jump", InputActionKind::Button)
                .unwrap()
                .released
        );

        input.set_key(Key::W, true);
        actions.set_input(input.take_frame());
        input.add_mouse_delta(9.0, 7.0);
        input.add_wheel(4.0, 2.0);
        input.reset_for_background();
        actions.set_input(input.take_frame());
        assert!(
            actions
                .frame()
                .state("Jump", InputActionKind::Button)
                .unwrap()
                .released
        );
        assert_eq!(
            actions
                .frame()
                .state("Look", InputActionKind::Vector2)
                .unwrap()
                .value,
            [0.0, 0.0]
        );
    }

    #[test]
    fn clearing_frame_transients_preserves_down_and_digital_values_for_destroy() {
        let mut actions = actions();
        let mut input = ScriptInput::default();
        input.set_key(Key::W, true);
        input.add_mouse_delta(3.0, -2.0);
        actions.set_input(input.take_frame());
        actions.clear_frame_transients();

        let frame = actions.frame();
        let jump = frame.state("Jump", InputActionKind::Button).unwrap();
        assert!(jump.down && !jump.pressed && !jump.released);
        assert_eq!(
            frame.state("Move", InputActionKind::Vector2).unwrap().value,
            [0.0, 1.0]
        );
        assert_eq!(
            frame.state("Look", InputActionKind::Vector2).unwrap().value,
            [0.0, 0.0]
        );
        assert!(frame.input().key_down(Key::W));
        assert!(!frame.input().key_pressed(Key::W));
        assert_eq!(frame.input().mouse_delta(), [0.0, 0.0]);
    }
}
