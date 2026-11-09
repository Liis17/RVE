//! Ввод игрового UI: маскирование нажатий для скриптов и перевод кадра скриптов в ввод `engine-gui`.
//!
//! Общая логика runtime и Play редактора. `UiInputBridge` тикает до `ScriptHost::update`: он ставит
//! события UI в очередь скриптов, а скрипты получают кадр без поглощённого UI ввода.

use crate::host::ScriptHost;
use crate::types::ScriptInput;
use engine_gui::capture::{InputCapture, InputFrame, InputId};
use engine_gui::input::{TextInputEvent, UiAction, UiClaims, UiInput};
use epaint::Vec2;
use rve_sdk::{Key, MouseButton};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const KEY_CODES: u32 = 128;
/// Коды кнопок мыши ABI.
const MOUSE_CODES: u32 = 5;
/// Кнопки геймпада ABI: биты 0..=16.
const PAD_BITS: u32 = 17;
/// Идентификаторы захвата: клавиши — код клавиши, мышь и пад — со смещением.
const MOUSE_BASE: InputId = 1_000;
const PAD_BASE: InputId = 2_000;
/// Порог отклонения стика, с которого он даёт направление навигации.
const STICK_THRESHOLD: f32 = 0.5;
/// Биты кнопок геймпада ABI (код кнопки = номер бита).
const PAD_SOUTH: u32 = 0;
const PAD_EAST: u32 = 1;
const PAD_DPAD_UP: u32 = 13;
const PAD_DPAD_DOWN: u32 = 14;
const PAD_DPAD_LEFT: u32 = 15;
const PAD_DPAD_RIGHT: u32 = 16;

fn key_id(key: Key) -> InputId {
    key as u32
}

fn mouse_id(button: MouseButton) -> InputId {
    MOUSE_BASE + button as u32
}

fn pad_id(bit: u32) -> InputId {
    PAD_BASE + bit
}

/// Первый подключённый геймпад (индекс слота с нуля): навигация UI берёт только его.
fn ui_pad(input: &ScriptInput) -> Option<usize> {
    (0..4).find(|&index| {
        input
            .gamepad_state(index as u32 + 1)
            .is_some_and(|state| state.connected != 0)
    })
}

/// Состояние кадра в идентификаторах захвата: только нажатия, удержания и отпускания.
fn input_frame(input: &ScriptInput, pad: Option<usize>) -> InputFrame {
    let mut frame = InputFrame::default();
    for code in 0..KEY_CODES {
        let Some(key) = Key::from_code(code) else {
            continue;
        };
        if input.key_pressed(key) {
            frame.pressed.insert(key_id(key));
        }
        if input.key_down(key) {
            frame.down.insert(key_id(key));
        }
        if input.key_released(key) {
            frame.released.insert(key_id(key));
        }
    }
    for code in 0..MOUSE_CODES {
        let Some(button) = MouseButton::from_code(code) else {
            continue;
        };
        if input.mouse_pressed(button) {
            frame.pressed.insert(mouse_id(button));
        }
        if input.mouse_down(button) {
            frame.down.insert(mouse_id(button));
        }
        if input.mouse_released(button) {
            frame.released.insert(mouse_id(button));
        }
    }
    if let Some(state) = pad.and_then(|index| input.gamepad_state(index as u32 + 1)) {
        for bit in 0..PAD_BITS {
            let mask = 1 << bit;
            if state.pressed & mask != 0 {
                frame.pressed.insert(pad_id(bit));
            }
            if state.down & mask != 0 {
                frame.down.insert(pad_id(bit));
            }
            if state.released & mask != 0 {
                frame.released.insert(pad_id(bit));
            }
        }
    }
    frame
}

/// Клавиши, которыми действие UI можно выполнить с клавиатуры.
fn action_keys(action: UiAction) -> &'static [Key] {
    match action {
        UiAction::Up => &[Key::ArrowUp],
        UiAction::Down => &[Key::ArrowDown],
        UiAction::Left => &[Key::ArrowLeft],
        UiAction::Right => &[Key::ArrowRight],
        UiAction::Confirm => &[Key::Enter],
        UiAction::Cancel => &[Key::Escape],
        UiAction::Next | UiAction::Previous => &[Key::Tab],
    }
}

/// Кнопки геймпада, которыми действие UI выполняется с пада.
fn action_pad_bits(action: UiAction) -> &'static [u32] {
    match action {
        UiAction::Up => &[PAD_DPAD_UP],
        UiAction::Down => &[PAD_DPAD_DOWN],
        UiAction::Left => &[PAD_DPAD_LEFT],
        UiAction::Right => &[PAD_DPAD_RIGHT],
        UiAction::Confirm => &[PAD_SOUTH],
        UiAction::Cancel => &[PAD_EAST],
        UiAction::Next | UiAction::Previous => &[],
    }
}

/// Идентификаторы, которые UI поглотил в этом кадре: нажатие мыши, навигационные клавиши и кнопки
/// пада, а при фокусе InputField — все новые нажатия клавиатуры.
fn claimed_ids(claims: &UiClaims, input: &ScriptInput) -> BTreeSet<InputId> {
    let mut claimed = BTreeSet::new();
    if claims.pointer_primary {
        claimed.insert(mouse_id(MouseButton::Left));
    }
    for &action in &claims.actions {
        for &key in action_keys(action) {
            claimed.insert(key_id(key));
        }
        for &bit in action_pad_bits(action) {
            claimed.insert(pad_id(bit));
        }
    }
    if claims.text_focused {
        for code in 0..KEY_CODES {
            if let Some(key) = Key::from_code(code)
                && input.key_pressed(key)
            {
                claimed.insert(key_id(key));
            }
        }
    }
    claimed
}

/// Маскирует кадр для скриптов: захваченный UI ввод скрыт в нажатии, удержании и отпускании.
/// `capture` хранит захват между кадрами, поэтому удержанное нажатие остаётся скрытым до отпускания.
fn mask_frame(
    capture: &mut InputCapture,
    input: &ScriptInput,
    claims: &UiClaims,
    pad: Option<usize>,
) -> ScriptInput {
    let raw = input_frame(input, pad);
    let claimed = claimed_ids(claims, input);
    let visible = capture.apply(&raw, &claimed, claims.wheel);
    let visible_ids: BTreeSet<InputId> = visible
        .pressed
        .iter()
        .chain(&visible.down)
        .chain(&visible.released)
        .copied()
        .collect();
    let mut masked = *input;
    let mut pad_mask = 0_u32;
    let hidden = raw
        .pressed
        .iter()
        .chain(&raw.down)
        .chain(&raw.released)
        .copied()
        .filter(|id| !visible_ids.contains(id))
        .collect::<BTreeSet<InputId>>();
    for id in hidden {
        if id >= PAD_BASE {
            pad_mask |= 1 << (id - PAD_BASE);
        } else if id >= MOUSE_BASE {
            if let Some(button) = MouseButton::from_code(id - MOUSE_BASE) {
                masked.hide_mouse_button(button);
            }
        } else if let Some(key) = Key::from_code(id) {
            masked.hide_key(key);
        }
    }
    if let Some(index) = pad {
        masked.hide_pad_buttons(index, pad_mask);
        if claims.focused {
            masked.hide_pad_left_stick(index);
        }
    }
    if claims.wheel {
        masked.hide_wheel();
    }
    masked
}

/// Направление левого стика, если он отклонён дальше порога.
fn stick_action(axes: [f32; 6]) -> Option<UiAction> {
    let (x, y) = (axes[0], axes[1]);
    if x.abs() < STICK_THRESHOLD && y.abs() < STICK_THRESHOLD {
        return None;
    }
    Some(if x.abs() >= y.abs() {
        if x > 0.0 {
            UiAction::Right
        } else {
            UiAction::Left
        }
    } else if y > 0.0 {
        UiAction::Up
    } else {
        UiAction::Down
    })
}

/// Действие UI по нажатой клавише; Shift + Tab — предыдущий элемент.
fn key_action(key: Key, shift: bool) -> Option<UiAction> {
    Some(match key {
        Key::ArrowUp => UiAction::Up,
        Key::ArrowDown => UiAction::Down,
        Key::ArrowLeft => UiAction::Left,
        Key::ArrowRight => UiAction::Right,
        Key::Enter => UiAction::Confirm,
        Key::Escape => UiAction::Cancel,
        Key::Tab if shift => UiAction::Previous,
        Key::Tab => UiAction::Next,
        _ => return None,
    })
}

/// Направления, которые можно удерживать для повтора: стрелки и крестовина пада.
const HELD_KEYS: [(Key, UiAction); 4] = [
    (Key::ArrowUp, UiAction::Up),
    (Key::ArrowDown, UiAction::Down),
    (Key::ArrowLeft, UiAction::Left),
    (Key::ArrowRight, UiAction::Right),
];

/// Состояние окна и указателя на тике, которое UI видит помимо нажатий.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiTick {
    /// UI принимает ввод: окно в фокусе, консоль закрыта, сцена не на паузе.
    pub accepting: bool,
    /// Указатель в физических пикселях экрана UI; `None` — над UI его нет.
    pub pointer: Option<[f32; 2]>,
    /// Размер экрана UI в физических пикселях: UI раскладывается по нему.
    pub screen_px: [u32; 2],
    /// Текстовый ввод кадра: редактирование InputField.
    pub text: Vec<TextInputEvent>,
}

/// Ввод UI на scripts thread (и в Play редактора): захват между кадрами, состояние стика и флаг
/// фокуса InputField.
pub struct UiInputBridge {
    capture: InputCapture,
    /// UI получал ввод в прошлом тике; при переходе к закрытому вводу взаимодействие сбрасывается.
    active: bool,
    /// Направление стика, которое уже дало навигацию: новое отклонение даёт действие один раз.
    stick: Option<UiAction>,
    /// Фокус InputField для главного потока: по нему включается IME.
    text_focus: Arc<AtomicBool>,
}

impl UiInputBridge {
    /// `text_focus` выставляется в `tick`: главный поток читает его, чтобы включить IME.
    pub fn new(text_focus: Arc<AtomicBool>) -> Self {
        Self {
            capture: InputCapture::default(),
            active: false,
            stick: None,
            text_focus,
        }
    }

    /// Сбрасывает hover, нажатие и фокус UI при уходе в фон или потере ввода. Уже поглощённые
    /// нажатия остаются скрытыми до отпускания: их решает захват.
    pub fn suspend(&mut self, scripts: &mut ScriptHost) {
        if !self.active {
            return;
        }
        self.active = false;
        self.stick = None;
        self.text_focus.store(false, Ordering::Release);
        let events = scripts.ui_mut().reset_interaction();
        scripts.queue_ui_events(events);
    }

    /// Обрабатывает ввод тика: обновляет UI, ставит его события в очередь скриптов и возвращает
    /// кадр, который увидят скрипты. Без `ui.accepting` UI получает пустой ввод и сбрасывается.
    pub fn tick(
        &mut self,
        scripts: &mut ScriptHost,
        input: ScriptInput,
        ui: UiTick,
        now: f64,
    ) -> ScriptInput {
        let accepting = ui.accepting;
        if !accepting {
            self.suspend(scripts);
        }
        self.active = accepting;
        let pad = ui_pad(&input);
        let ui_input = if accepting {
            self.ui_input(&input, pad, ui.pointer, ui.text)
        } else {
            UiInput::default()
        };
        let [width, height] = ui.screen_px;
        let update =
            scripts
                .ui_mut()
                .update(&ui_input, now, Vec2::new(width as f32, height as f32));
        scripts.queue_ui_events(update.events);
        self.text_focus
            .store(accepting && update.claims.text_focused, Ordering::Release);
        mask_frame(&mut self.capture, &input, &update.claims, pad)
    }

    /// Ввод `engine-gui` из кадра скриптов: указатель, колесо, нажатия навигации и удержание.
    fn ui_input(
        &mut self,
        input: &ScriptInput,
        pad: Option<usize>,
        pointer: Option<[f32; 2]>,
        text: Vec<TextInputEvent>,
    ) -> UiInput {
        let wheel = input.wheel();
        let mut actions = Vec::new();
        let shift = input.key_down(Key::ShiftLeft) || input.key_down(Key::ShiftRight);
        for code in 0..KEY_CODES {
            let Some(key) = Key::from_code(code) else {
                continue;
            };
            if input.key_pressed(key)
                && let Some(action) = key_action(key, shift)
            {
                actions.push(action);
            }
        }
        let mut held = HELD_KEYS
            .iter()
            .find(|(key, _)| input.key_down(*key))
            .map(|(_, action)| *action);
        if let Some(state) = pad.and_then(|index| input.gamepad_state(index as u32 + 1)) {
            for (bit, action) in [
                (PAD_DPAD_UP, UiAction::Up),
                (PAD_DPAD_DOWN, UiAction::Down),
                (PAD_DPAD_LEFT, UiAction::Left),
                (PAD_DPAD_RIGHT, UiAction::Right),
            ] {
                if state.pressed & (1 << bit) != 0 {
                    actions.push(action);
                }
                if held.is_none() && state.down & (1 << bit) != 0 {
                    held = Some(action);
                }
            }
            if state.pressed & (1 << PAD_SOUTH) != 0 {
                actions.push(UiAction::Confirm);
            }
            if state.pressed & (1 << PAD_EAST) != 0 {
                actions.push(UiAction::Cancel);
            }
        }
        let axes = pad
            .and_then(|index| input.gamepad_state(index as u32 + 1))
            .map(|state| state.axes)
            .unwrap_or_default();
        let stick = stick_action(axes);
        if stick != self.stick
            && let Some(action) = stick
        {
            actions.push(action);
        }
        self.stick = stick;
        if held.is_none() {
            held = stick;
        }
        UiInput {
            pointer: pointer.map(|[x, y]| Vec2::new(x, y)),
            primary_pressed: input.mouse_pressed(MouseButton::Left),
            primary_down: input.mouse_down(MouseButton::Left),
            primary_released: input.mouse_released(MouseButton::Left),
            wheel: Vec2::new(wheel[0], wheel[1]),
            actions,
            held,
            text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key_pressed_frame(key: Key) -> ScriptInput {
        let mut input = ScriptInput::default();
        input.set_key(key, true);
        input
    }

    /// Клавиша удерживается с прошлого кадра: без фронтов нажатия и отпускания.
    fn key_held(key: Key) -> ScriptInput {
        let mut input = ScriptInput::default();
        input.set_key(key, true);
        let _ = input.take_frame();
        input
    }

    /// Клавиша отпущена в этом кадре.
    fn key_released_frame(key: Key) -> ScriptInput {
        let mut input = key_held(key);
        input.set_key(key, false);
        input
    }

    fn mouse_pressed_frame() -> ScriptInput {
        let mut input = ScriptInput::default();
        input.set_mouse_button(MouseButton::Left, true);
        input
    }

    fn mouse_held() -> ScriptInput {
        let mut input = ScriptInput::default();
        input.set_mouse_button(MouseButton::Left, true);
        let _ = input.take_frame();
        input
    }

    fn mouse_released_frame() -> ScriptInput {
        let mut input = mouse_held();
        input.set_mouse_button(MouseButton::Left, false);
        input
    }

    fn claim_primary() -> UiClaims {
        UiClaims {
            pointer_primary: true,
            ..UiClaims::default()
        }
    }

    #[test]
    fn press_claimed_by_ui_stays_hidden_through_hold_and_release() {
        let mut capture = InputCapture::default();
        let press = mask_frame(&mut capture, &mouse_pressed_frame(), &claim_primary(), None);
        assert!(!press.mouse_pressed(MouseButton::Left));
        let held = mask_frame(&mut capture, &mouse_held(), &UiClaims::default(), None);
        assert!(
            !held.mouse_down(MouseButton::Left),
            "удержание поглощённого нажатия скрыто"
        );
        let released = mask_frame(
            &mut capture,
            &mouse_released_frame(),
            &UiClaims::default(),
            None,
        );
        assert!(
            !released.mouse_released(MouseButton::Left),
            "отпускание поглощённого нажатия скрыто"
        );
    }

    #[test]
    fn game_press_and_release_stay_visible_when_ui_claims_during_hold() {
        let mut capture = InputCapture::default();
        let press = mask_frame(
            &mut capture,
            &mouse_pressed_frame(),
            &UiClaims::default(),
            None,
        );
        assert!(press.mouse_pressed(MouseButton::Left));

        // InputField открылся во время удержания: новых нажатий клавиатуры он не получает, а
        // уже начатое в игре нажатие мыши остаётся видимым.
        let input_field = UiClaims {
            text_focused: true,
            ..UiClaims::default()
        };
        let held = mask_frame(&mut capture, &mouse_held(), &input_field, None);
        assert!(held.mouse_down(MouseButton::Left));
        let released = mask_frame(
            &mut capture,
            &mouse_released_frame(),
            &UiClaims::default(),
            None,
        );
        assert!(released.mouse_released(MouseButton::Left));
    }

    #[test]
    fn wheel_over_scroll_view_is_not_seen_by_scripts() {
        let mut capture = InputCapture::default();
        let mut input = ScriptInput::default();
        input.add_wheel(0.0, 3.0);
        let claims = UiClaims {
            wheel: true,
            ..UiClaims::default()
        };
        let masked = mask_frame(&mut capture, &input, &claims, None);
        assert_eq!(masked.wheel(), [0.0; 2]);
        let free = mask_frame(&mut capture, &input, &UiClaims::default(), None);
        assert_eq!(free.wheel(), [0.0, 3.0]);
    }

    #[test]
    fn focused_input_field_hides_new_key_presses_and_their_release() {
        let mut capture = InputCapture::default();
        let claims = UiClaims {
            text_focused: true,
            ..UiClaims::default()
        };
        let press = mask_frame(&mut capture, &key_pressed_frame(Key::A), &claims, None);
        assert!(!press.key_pressed(Key::A));
        let held = mask_frame(&mut capture, &key_held(Key::A), &UiClaims::default(), None);
        assert!(!held.key_down(Key::A));
        let release = mask_frame(
            &mut capture,
            &key_released_frame(Key::A),
            &UiClaims::default(),
            None,
        );
        assert!(!release.key_released(Key::A));
    }

    #[test]
    fn navigation_key_consumed_by_ui_is_hidden_from_game() {
        let mut capture = InputCapture::default();
        let claims = UiClaims {
            actions: vec![UiAction::Up],
            ..UiClaims::default()
        };
        let masked = mask_frame(
            &mut capture,
            &key_pressed_frame(Key::ArrowUp),
            &claims,
            None,
        );
        assert!(!masked.key_pressed(Key::ArrowUp));
        let other = mask_frame(
            &mut capture,
            &key_pressed_frame(Key::D),
            &UiClaims::default(),
            None,
        );
        assert!(other.key_pressed(Key::D));
    }

    #[test]
    fn pad_buttons_consumed_by_ui_are_hidden_on_the_ui_pad_only() {
        let mut capture = InputCapture::default();
        let mut input = ScriptInput::default();
        input.set_pad(0, true, 1 << PAD_DPAD_DOWN, [0.0; 6]);
        let claims = UiClaims {
            actions: vec![UiAction::Down],
            ..UiClaims::default()
        };
        let masked = mask_frame(&mut capture, &input, &claims, Some(0));
        assert_eq!(masked.gamepad_state(1).unwrap().pressed, 0);
        let free = mask_frame(&mut capture, &input, &UiClaims::default(), Some(0));
        assert_eq!(free.gamepad_state(1).unwrap().pressed, 1 << PAD_DPAD_DOWN);
    }

    #[test]
    fn left_stick_is_hidden_from_game_only_while_ui_has_focus_on_the_ui_pad() {
        let mut capture = InputCapture::default();
        let mut input = ScriptInput::default();
        // Порядок осей: LeftX, LeftY, RightX, RightY, LeftTrigger, RightTrigger.
        input.set_pad(0, true, 0, [0.8, -0.6, 0.3, 0.4, 0.0, 0.0]);
        let focused = UiClaims {
            focused: true,
            ..UiClaims::default()
        };
        let masked = mask_frame(&mut capture, &input, &focused, Some(0));
        let state = masked.gamepad_state(1).unwrap();
        assert_eq!(state.axes[..2], [0.0, 0.0], "левый стик скрыт");
        assert_eq!(state.axes[2..4], [0.3, 0.4], "правый стик не трогаем");
        let free = mask_frame(&mut capture, &input, &UiClaims::default(), Some(0));
        assert_eq!(free.gamepad_state(1).unwrap().axes[..2], [0.8, -0.6]);
        let other_pad = mask_frame(&mut capture, &input, &focused, None);
        assert_eq!(other_pad.gamepad_state(1).unwrap().axes[..2], [0.8, -0.6]);
    }

    #[test]
    fn ui_input_collects_pressed_navigation_text_and_held_direction() {
        let mut bridge = UiInputBridge::new(Arc::new(AtomicBool::new(false)));
        let mut input = ScriptInput::default();
        input.set_key(Key::ArrowDown, true);
        input.set_mouse_button(MouseButton::Left, true);
        let text = vec![TextInputEvent::Text("a".to_owned())];
        let ui = bridge.ui_input(&input, None, Some([12.0, 34.0]), text.clone());
        assert_eq!(ui.actions, vec![UiAction::Down]);
        assert_eq!(ui.held, Some(UiAction::Down));
        assert!(ui.primary_pressed);
        assert_eq!(ui.pointer, Some(Vec2::new(12.0, 34.0)));
        assert_eq!(ui.text, text);
    }

    #[test]
    fn stick_gives_one_action_per_new_direction() {
        let mut bridge = UiInputBridge::new(Arc::new(AtomicBool::new(false)));
        let mut input = ScriptInput::default();
        input.set_pad(0, true, 0, [0.9, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let first = bridge.ui_input(&input, Some(0), None, Vec::new());
        assert_eq!(first.actions, vec![UiAction::Right]);
        assert_eq!(first.held, Some(UiAction::Right));
        let second = bridge.ui_input(&input, Some(0), None, Vec::new());
        assert!(
            second.actions.is_empty(),
            "удержанный стик даёт повтор, а не новое действие"
        );
        assert_eq!(second.held, Some(UiAction::Right));
    }

    #[test]
    fn shift_tab_moves_to_previous_element() {
        let mut bridge = UiInputBridge::new(Arc::new(AtomicBool::new(false)));
        let mut input = ScriptInput::default();
        input.set_key(Key::ShiftLeft, true);
        input.set_key(Key::Tab, true);
        let ui = bridge.ui_input(&input, None, None, Vec::new());
        assert_eq!(ui.actions, vec![UiAction::Previous]);
    }

    #[test]
    fn pad_bit_constants_match_abi_buttons() {
        use rve_sdk::GamepadButton;
        assert_eq!(PAD_SOUTH, GamepadButton::South as u32);
        assert_eq!(PAD_EAST, GamepadButton::East as u32);
        assert_eq!(PAD_DPAD_UP, GamepadButton::DPadUp as u32);
        assert_eq!(PAD_DPAD_DOWN, GamepadButton::DPadDown as u32);
        assert_eq!(PAD_DPAD_LEFT, GamepadButton::DPadLeft as u32);
        assert_eq!(PAD_DPAD_RIGHT, GamepadButton::DPadRight as u32);
    }
}
