//! Захват ввода: какие нажатия скрыты от скриптов, пока UI с ними работает.
//!
//! Правила: нажатие, поглощённое UI, скрыто во всех состояниях (нажато, удержано, отпущено)
//! до своего отпускания, даже если UI потом закрылся. Нажатие, начатое в игре, остаётся видимым
//! скриптам вместе с отпусканием, включая случай, когда UI захватил ввод во время удержания.
//! Колесо обнуляется, если UI его обработал.

use epaint::Vec2;
use std::collections::BTreeSet;

/// Абстрактный идентификатор кнопки мыши или клавиши: его выбирает вызывающая сторона.
pub type InputId = u32;

/// Состояние ввода одного кадра по абстрактным идентификаторам.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputFrame {
    pub pressed: BTreeSet<InputId>,
    pub down: BTreeSet<InputId>,
    pub released: BTreeSet<InputId>,
    pub wheel: Vec2,
}

/// Удерживает список поглощённых UI нажатий между кадрами.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputCapture {
    owned_by_ui: BTreeSet<InputId>,
}

impl InputCapture {
    /// Возвращает ввод, который видят скрипты. `claimed` — нажатия, поглощённые UI в этом кадре
    /// (включая клавиши, заблокированные фокусом InputField); `wheel_claimed` — колесо обработано UI.
    pub fn apply(
        &mut self,
        input: &InputFrame,
        claimed: &BTreeSet<InputId>,
        wheel_claimed: bool,
    ) -> InputFrame {
        let mut visible = InputFrame::default();
        for &id in &input.pressed {
            if claimed.contains(&id) {
                self.owned_by_ui.insert(id);
            } else {
                visible.pressed.insert(id);
            }
        }
        for &id in &input.down {
            if !self.owned_by_ui.contains(&id) {
                visible.down.insert(id);
            }
        }
        for &id in &input.released {
            if !self.owned_by_ui.remove(&id) {
                visible.released.insert(id);
            }
        }
        visible.wheel = if wheel_claimed {
            Vec2::ZERO
        } else {
            input.wheel
        };
        visible
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: InputId = 1;
    const CONFIRM: InputId = 2;
    const KEY_A: InputId = 3;

    fn frame(pressed: &[InputId], down: &[InputId], released: &[InputId]) -> InputFrame {
        InputFrame {
            pressed: pressed.iter().copied().collect(),
            down: down.iter().copied().collect(),
            released: released.iter().copied().collect(),
            wheel: Vec2::ZERO,
        }
    }

    fn claims(ids: &[InputId]) -> BTreeSet<InputId> {
        ids.iter().copied().collect()
    }

    #[test]
    fn claimed_press_is_hidden_until_release() {
        let mut capture = InputCapture::default();
        let first = capture.apply(&frame(&[LEFT], &[LEFT], &[]), &claims(&[LEFT]), false);
        assert!(first.pressed.is_empty() && first.down.is_empty());

        let held = capture.apply(&frame(&[], &[LEFT], &[]), &BTreeSet::new(), false);
        assert!(
            held.down.is_empty(),
            "удержание поглощённого нажатия скрыто"
        );

        let released = capture.apply(&frame(&[], &[], &[LEFT]), &BTreeSet::new(), false);
        assert!(
            released.released.is_empty(),
            "отпускание поглощённого нажатия скрыто"
        );

        let next = capture.apply(&frame(&[LEFT], &[LEFT], &[]), &BTreeSet::new(), false);
        assert_eq!(
            next.pressed,
            claims(&[LEFT]),
            "после отпускания нажатие снова видно"
        );
    }

    #[test]
    fn claimed_press_stays_hidden_after_ui_releases_capture() {
        let mut capture = InputCapture::default();
        capture.apply(
            &frame(&[CONFIRM], &[CONFIRM], &[]),
            &claims(&[CONFIRM]),
            false,
        );
        // UI закрыт во время удержания: claimed пустой, но нажатие всё ещё принадлежит UI.
        let held = capture.apply(&frame(&[], &[CONFIRM], &[]), &BTreeSet::new(), false);
        assert!(held.down.is_empty());
        let released = capture.apply(&frame(&[], &[], &[CONFIRM]), &BTreeSet::new(), false);
        assert!(released.released.is_empty());
    }

    #[test]
    fn game_press_held_into_capture_stays_visible_with_release() {
        let mut capture = InputCapture::default();
        let started = capture.apply(&frame(&[KEY_A], &[KEY_A], &[]), &BTreeSet::new(), false);
        assert_eq!(started.pressed, claims(&[KEY_A]));

        // InputField открылся во время удержания: клавиша уже у игры, захват её не трогает.
        let during = capture.apply(&frame(&[], &[KEY_A], &[]), &claims(&[KEY_A]), false);
        assert_eq!(during.down, claims(&[KEY_A]));

        let released = capture.apply(&frame(&[], &[], &[KEY_A]), &claims(&[KEY_A]), false);
        assert_eq!(
            released.released,
            claims(&[KEY_A]),
            "отпускание игрового нажатия видно скриптам"
        );
    }

    #[test]
    fn new_key_presses_are_hidden_while_text_is_focused() {
        let mut capture = InputCapture::default();
        let typed = capture.apply(&frame(&[KEY_A], &[KEY_A], &[]), &claims(&[KEY_A]), false);
        assert!(typed.pressed.is_empty() && typed.down.is_empty());
        let other = capture.apply(&frame(&[LEFT], &[LEFT], &[]), &BTreeSet::new(), false);
        assert_eq!(other.pressed, claims(&[LEFT]));
    }

    #[test]
    fn wheel_is_zeroed_only_when_ui_claims_it() {
        let mut capture = InputCapture::default();
        let mut input = frame(&[], &[], &[]);
        input.wheel = Vec2::new(0.0, 3.0);

        let over_scroll = capture.apply(&input, &BTreeSet::new(), true);
        assert_eq!(over_scroll.wheel, Vec2::ZERO);

        let elsewhere = capture.apply(&input, &BTreeSet::new(), false);
        assert_eq!(elsewhere.wheel, Vec2::new(0.0, 3.0));
    }
}
