use engine_script::ScriptCursor;
use engine_window::{CursorState, WindowProxy};
use std::sync::{Arc, Mutex};

/// Получатель эффективного состояния курсора; в игре его реализует окно.
pub(crate) trait CursorSink: Send + Sync {
    fn apply(&self, state: CursorState);
}

struct WindowCursor(WindowProxy);

impl CursorSink for WindowCursor {
    fn apply(&self, state: CursorState) {
        self.0.set_cursor(state);
    }
}

#[derive(Default)]
struct CursorStateData {
    script: ScriptCursor,
    console_open: bool,
    sink: Option<Arc<dyn CursorSink>>,
    last_sent: Option<CursorState>,
}

impl CursorStateData {
    /// Открытая консоль требует обычный курсор независимо от запроса скриптов.
    fn effective(&self) -> CursorState {
        if self.console_open {
            return CursorState::default();
        }
        CursorState {
            visible: self.script.visible,
            locked: self.script.locked,
            lock_position: None,
        }
    }

    /// Возвращает получателя и состояние, если эффективное значение отличается от отправленного.
    fn outgoing(&mut self) -> Option<(Arc<dyn CursorSink>, CursorState)> {
        let sink = self.sink.clone()?;
        let state = self.effective();
        if self.last_sent == Some(state) {
            return None;
        }
        self.last_sent = Some(state);
        Some((sink, state))
    }
}

/// Передаёт желаемый курсор скриптов и состояние консоли в окно игрока.
///
/// Состояние хранится до подключения окна и отправляется при подключении. Отправка идёт вне
/// замка данных, но под отдельным замком порядка, чтобы окно получало состояния по порядку.
#[derive(Clone, Default)]
pub(crate) struct CursorControl {
    state: Arc<Mutex<CursorStateData>>,
    delivery: Arc<Mutex<()>>,
}

impl CursorControl {
    pub fn attach(&self, proxy: WindowProxy) {
        self.attach_sink(Arc::new(WindowCursor(proxy)));
    }

    pub fn attach_sink(&self, sink: Arc<dyn CursorSink>) {
        self.change(|data| data.sink = Some(sink));
    }

    pub fn set_script_cursor(&self, cursor: ScriptCursor) {
        self.change(|data| data.script = cursor);
    }

    /// Вызывается только из dev-консоли; в сборке без неё не используется.
    #[cfg_attr(not(feature = "dev-console"), allow(dead_code))]
    pub fn set_console_open(&self, open: bool) {
        self.change(|data| data.console_open = open);
    }

    fn change(&self, update: impl FnOnce(&mut CursorStateData)) {
        let _order = self.delivery.lock().expect("cursor delivery poisoned");
        let outgoing = {
            let mut data = self.state.lock().expect("cursor state poisoned");
            update(&mut data);
            data.outgoing()
        };
        if let Some((sink, state)) = outgoing {
            sink.apply(state);
        }
    }
}

/// Запоминает все отправленные состояния курсора.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct RecordingCursorSink(pub(crate) Mutex<Vec<CursorState>>);

#[cfg(test)]
impl CursorSink for RecordingCursorSink {
    fn apply(&self, state: CursorState) {
        self.0.lock().unwrap().push(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type RecordingSink = RecordingCursorSink;

    fn locked() -> ScriptCursor {
        ScriptCursor {
            visible: false,
            locked: true,
        }
    }

    fn window(visible: bool, locked: bool) -> CursorState {
        CursorState {
            visible,
            locked,
            lock_position: None,
        }
    }

    #[test]
    fn repeated_script_state_is_sent_once() {
        let control = CursorControl::default();
        let sink = Arc::new(RecordingSink::default());
        control.attach_sink(sink.clone());
        control.set_script_cursor(locked());
        control.set_script_cursor(locked());
        assert_eq!(
            *sink.0.lock().unwrap(),
            vec![window(true, false), window(false, true)]
        );
    }

    #[test]
    fn open_console_overrides_script_cursor_and_restores_it_on_close() {
        let control = CursorControl::default();
        let sink = Arc::new(RecordingSink::default());
        control.attach_sink(sink.clone());
        control.set_script_cursor(locked());
        control.set_console_open(true);
        control.set_console_open(true);
        control.set_console_open(false);
        assert_eq!(
            *sink.0.lock().unwrap(),
            vec![
                window(true, false),
                window(false, true),
                window(true, false),
                window(false, true),
            ]
        );
    }

    #[test]
    fn state_written_before_attach_is_sent_when_the_window_connects() {
        let control = CursorControl::default();
        control.set_script_cursor(locked());
        let sink = Arc::new(RecordingSink::default());
        control.attach_sink(sink.clone());
        assert_eq!(*sink.0.lock().unwrap(), vec![window(false, true)]);
    }

    #[test]
    fn console_state_before_attach_is_part_of_the_first_delivery() {
        let control = CursorControl::default();
        control.set_script_cursor(locked());
        control.set_console_open(true);
        let sink = Arc::new(RecordingSink::default());
        control.attach_sink(sink.clone());
        assert_eq!(*sink.0.lock().unwrap(), vec![window(true, false)]);
    }
}
