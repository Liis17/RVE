//! Захват курсора панелью Game во время Play: условия, Esc и состояние, которое уходит в окно.

use crate::layout::Tab;
use crate::state::EditorState;
use egui_dock::DockState;
use engine_script::ScriptCursor;
use engine_window::CursorState;
use winit::dpi::PhysicalPosition;

/// Условия захвата на текущем кадре.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureInputs {
    pub playing: bool,
    pub paused: bool,
    pub window_focused: bool,
    /// Открыта консоль или модальное окно.
    pub overlay_open: bool,
    /// Панель Game видна и является активной вкладкой фокусного листа.
    pub game_active: bool,
    /// Esc снял захват; вернуть его может только клик ЛКМ в панели Game.
    pub released: bool,
}

/// Панель Game захватывает курсор только в Play, без паузы, в фокусе окна и без оверлеев.
pub fn play_cursor_captured(inputs: CaptureInputs) -> bool {
    inputs.playing
        && !inputs.paused
        && inputs.window_focused
        && !inputs.overlay_open
        && inputs.game_active
        && !inputs.released
}

/// Источник дельты движения мыши для ввода Play: ровно один.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MotionSource {
    /// Raw-события окна (`DeviceEvent::MouseMotion`) при захвате с удержанием.
    Raw,
    /// Дельта egui над панелью Game.
    Egui,
}

/// Состояние захвата: признак Esc, решение текущего кадра и последний запрос, отправленный в окно.
#[derive(Debug, Default)]
pub struct PlayCursor {
    released: bool,
    captured: bool,
    locked: bool,
    sent: Option<CursorState>,
}

impl PlayCursor {
    /// Esc снимает захват. Клавиша при этом всё равно передаётся скриптам.
    pub fn escape(&mut self) {
        self.released = true;
    }

    /// Клик ЛКМ в панели Game возвращает захват.
    pub fn game_clicked(&mut self) {
        self.released = false;
    }

    /// Старт Play сбрасывает признак Esc.
    pub fn reset(&mut self) {
        self.released = false;
    }

    pub fn captured(&self) -> bool {
        self.captured
    }

    pub fn released(&self) -> bool {
        self.released
    }

    /// Источник дельты мыши для ввода Play. Raw-события окна действуют только при захвате с удержанием
    /// и указателе над видом; иначе действует дельта egui. Оба пути ввода спрашивают этот ответ,
    /// поэтому дельта не считается дважды.
    pub fn motion_source(&self, pointer_over_view: bool) -> MotionSource {
        if self.locked && pointer_over_view {
            MotionSource::Raw
        } else {
            MotionSource::Egui
        }
    }

    /// Пересчитывает захват на кадре и возвращает состояние для окна, если оно изменилось.
    ///
    /// Удержание включается, только если скрипт заблокировал курсор и известен центр панели.
    /// Видимость всегда `true`: скрытие курсора внутри панели делает egui иконкой `None`.
    pub fn update(
        &mut self,
        captured: bool,
        script: ScriptCursor,
        lock_center: Option<PhysicalPosition<f64>>,
    ) -> Option<CursorState> {
        self.captured = captured;
        let request = match lock_center {
            Some(center) if captured && script.locked => CursorState {
                visible: true,
                locked: true,
                lock_position: Some(center),
            },
            _ => CursorState::default(),
        };
        self.locked = request.locked;
        if self.sent == Some(request) {
            return None;
        }
        self.sent = Some(request);
        Some(request)
    }
}

/// Делает вкладку Game активной и фокусной, чтобы Play сразу захватил курсор без клика.
pub fn focus_game_tab(dock: &mut DockState<Tab>) {
    if let Some(path) = dock.find_tab(&Tab::Game) {
        let _ = dock.set_active_tab(path);
        dock.set_focused_node_and_surface(path.node_path());
    }
}

/// Пересчитывает захват на кадре, выставляет иконку `None` над скрытым курсором и возвращает
/// состояние для окна, если оно изменилось.
pub fn sync(
    ctx: &egui::Context,
    state: &mut EditorState,
    dock: &mut DockState<Tab>,
    window_focused: bool,
) -> Option<CursorState> {
    let game_focused = dock
        .find_active_focused()
        .is_some_and(|(_, tab)| *tab == Tab::Game);
    let inputs = CaptureInputs {
        playing: state.is_playing(),
        paused: state.play.as_ref().is_some_and(|play| play.is_paused()),
        window_focused,
        overlay_open: state.console_open || crate::app::console_modal_open(state),
        game_active: game_focused && state.game_view.visible,
        released: state.play_cursor.released(),
    };
    let captured = play_cursor_captured(inputs);
    let script = state
        .play
        .as_ref()
        .map(|play| play.cursor())
        .unwrap_or_default();
    let pixels_per_point = ctx.pixels_per_point();
    let lock_center = state.game_view.rect.map(|rect| {
        let center = rect.center();
        PhysicalPosition::new(
            f64::from(center.x * pixels_per_point),
            f64::from(center.y * pixels_per_point),
        )
    });
    let request = state.play_cursor.update(captured, script, lock_center);
    if captured && !script.visible && state.game_view.hovered {
        ctx.set_cursor_icon(egui::CursorIcon::None);
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_game() -> CaptureInputs {
        CaptureInputs {
            playing: true,
            paused: false,
            window_focused: true,
            overlay_open: false,
            game_active: true,
            released: false,
        }
    }

    fn locked() -> ScriptCursor {
        ScriptCursor {
            visible: false,
            locked: true,
        }
    }

    fn center() -> Option<PhysicalPosition<f64>> {
        Some(PhysicalPosition::new(400.0, 300.0))
    }

    #[test]
    fn capture_needs_play_focus_an_active_game_tab_and_no_overlay_or_pause() {
        assert!(play_cursor_captured(open_game()));
        let cases = [
            CaptureInputs {
                playing: false,
                ..open_game()
            },
            CaptureInputs {
                paused: true,
                ..open_game()
            },
            CaptureInputs {
                window_focused: false,
                ..open_game()
            },
            CaptureInputs {
                overlay_open: true,
                ..open_game()
            },
            CaptureInputs {
                game_active: false,
                ..open_game()
            },
            CaptureInputs {
                released: true,
                ..open_game()
            },
        ];
        for inputs in cases {
            assert!(!play_cursor_captured(inputs), "{inputs:?}");
        }
    }

    #[test]
    fn escape_releases_until_the_next_click_in_game() {
        let mut cursor = PlayCursor::default();
        cursor.escape();
        assert!(cursor.released());
        assert!(!play_cursor_captured(CaptureInputs {
            released: cursor.released(),
            ..open_game()
        }));
        cursor.game_clicked();
        assert!(!cursor.released());
    }

    #[test]
    fn starting_play_clears_a_previous_escape() {
        let mut cursor = PlayCursor::default();
        cursor.escape();
        cursor.reset();
        assert!(!cursor.released());
    }

    #[test]
    fn locked_capture_centers_on_game_and_sends_once() {
        let mut cursor = PlayCursor::default();
        let request = cursor.update(true, locked(), center());
        assert_eq!(
            request,
            Some(CursorState {
                visible: true,
                locked: true,
                lock_position: center(),
            })
        );
        assert_eq!(cursor.motion_source(true), MotionSource::Raw);
        assert_eq!(cursor.update(true, locked(), center()), None);
    }

    #[test]
    fn captured_and_locked_takes_raw_motion_only_over_the_view() {
        let mut cursor = PlayCursor::default();
        cursor.update(true, locked(), center());
        assert_eq!(cursor.motion_source(true), MotionSource::Raw);
        assert_eq!(
            cursor.motion_source(false),
            MotionSource::Egui,
            "указатель над другой панелью: raw-дельта в Play не идёт"
        );
    }

    #[test]
    fn hidden_only_or_uncaptured_cursor_takes_egui_motion() {
        let mut cursor = PlayCursor::default();
        cursor.update(
            true,
            ScriptCursor {
                visible: false,
                locked: false,
            },
            center(),
        );
        assert_eq!(
            cursor.motion_source(true),
            MotionSource::Egui,
            "скрыт, но не заблокирован: egui-дельта"
        );
        cursor.update(false, locked(), center());
        assert_eq!(
            cursor.motion_source(true),
            MotionSource::Egui,
            "без захвата"
        );
        cursor.update(true, locked(), None);
        assert_eq!(
            cursor.motion_source(true),
            MotionSource::Egui,
            "без панели нет точки удержания"
        );
    }

    #[test]
    fn leaving_capture_releases_the_cursor_once() {
        let mut cursor = PlayCursor::default();
        cursor.update(true, locked(), center());
        assert_eq!(
            cursor.update(false, locked(), center()),
            Some(CursorState::default())
        );
        assert_eq!(cursor.update(false, locked(), center()), None);
    }

    #[test]
    fn game_tab_becomes_active_and_focused_for_play() {
        let mut dock = DockState::new(vec![Tab::Scene, Tab::Game]);
        assert!(
            dock.find_active_focused()
                .is_none_or(|(_, tab)| *tab != Tab::Game)
        );
        focus_game_tab(&mut dock);
        assert!(
            dock.find_active_focused()
                .is_some_and(|(_, tab)| *tab == Tab::Game)
        );
    }
}
