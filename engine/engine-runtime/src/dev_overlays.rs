//! Состояние dev-оверлеев окна: консоль (Backquote) и профайлер (F3).
//!
//! Оба оверлея рисуются одним egui-контекстом окна. Решение «нужен ли egui»,
//! «виден ли кадр» и «блокирует ли консоль ввод игры» вычисляется здесь, а
//! `PlayerWindow` только применяет [`OverlayPlan`]. Так сценарии переключения
//! проверяются без окна и GPU.

/// Открытые dev-оверлеи. Значение не хранит egui-состояние: его создаёт и освобождает окно.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DevOverlays {
    console: bool,
    profile: bool,
}

/// Что окно должно сделать с egui и кадром оверлеев после изменения состояния.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OverlayPlan {
    /// Нужен живой egui-контекст: создать, если его нет, иначе освободить.
    pub egui_context: bool,
    /// Публиковать кадр с геометрией. Иначе публикуется пустой кадр, чтобы render снял оверлей.
    pub frame_visible: bool,
    /// Ввод игры блокируется только открытой консолью.
    pub blocks_game_input: bool,
}

impl DevOverlays {
    pub(crate) fn console_open(self) -> bool {
        self.console
    }

    pub(crate) fn profile_open(self) -> bool {
        self.profile
    }

    pub(crate) fn with_console(self, open: bool) -> Self {
        Self {
            console: open,
            ..self
        }
    }

    pub(crate) fn toggled_profile(self) -> Self {
        Self {
            profile: !self.profile,
            ..self
        }
    }

    pub(crate) fn visible(self) -> bool {
        self.console || self.profile
    }

    pub(crate) fn plan(self) -> OverlayPlan {
        OverlayPlan {
            egui_context: self.visible(),
            frame_visible: self.visible(),
            blocks_game_input: self.console,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DevOverlays, OverlayPlan};

    fn hidden() -> OverlayPlan {
        OverlayPlan {
            egui_context: false,
            frame_visible: false,
            blocks_game_input: false,
        }
    }

    #[test]
    fn f3_toggled_twice_hides_overlay_and_releases_egui() {
        let shown = DevOverlays::default().toggled_profile();
        assert_eq!(
            shown.plan(),
            OverlayPlan {
                egui_context: true,
                frame_visible: true,
                blocks_game_input: false,
            }
        );

        let hidden_again = shown.toggled_profile();
        assert_eq!(hidden_again.plan(), hidden());
    }

    #[test]
    fn profiler_opened_after_console_closed_keeps_egui_and_does_not_block_input() {
        let state = DevOverlays::default()
            .with_console(true)
            .with_console(false)
            .toggled_profile();

        assert_eq!(
            state.plan(),
            OverlayPlan {
                egui_context: true,
                frame_visible: true,
                blocks_game_input: false,
            }
        );
    }

    #[test]
    fn closing_console_keeps_profiler_visible_and_input_unblocked() {
        let state = DevOverlays::default()
            .toggled_profile()
            .with_console(true)
            .with_console(false);

        assert_eq!(
            state.plan(),
            OverlayPlan {
                egui_context: true,
                frame_visible: true,
                blocks_game_input: false,
            }
        );
        assert!(!state.console_open());
        assert!(state.profile_open());
    }

    #[test]
    fn open_console_blocks_game_input_even_with_profiler() {
        let state = DevOverlays::default().toggled_profile().with_console(true);

        assert!(state.plan().blocks_game_input);
        assert!(state.console_open());
    }

    #[test]
    fn closing_profiler_while_console_open_keeps_console_visible() {
        let state = DevOverlays::default()
            .with_console(true)
            .toggled_profile()
            .toggled_profile();

        assert_eq!(
            state.plan(),
            OverlayPlan {
                egui_context: true,
                frame_visible: true,
                blocks_game_input: true,
            }
        );
    }

    #[test]
    fn closing_last_overlay_releases_egui_and_publishes_empty_frame() {
        let state = DevOverlays::default()
            .with_console(true)
            .toggled_profile()
            .with_console(false)
            .toggled_profile();

        assert_eq!(state.plan(), hidden());
    }
}
