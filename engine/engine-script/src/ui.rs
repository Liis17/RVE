//! Игровой UI в скриптах: общее состояние `UiSystem`, журнал отката и транзакционные операции.
//!
//! Записи UI применяются к `UiSystem` сразу, чтобы чтения в том же вызове видели их без отдельного
//! overlay, и откатываются журналом при ошибке или panic callback. Закрытие канваса откладывается до
//! успешного commit: до него канвас остаётся видим, но любой доступ к нему в этом вызове дает
//! ошибку устаревшего хендла. Снаружи UI не читается во время callback, поэтому различие не видно.

use crate::transaction::ScriptTransaction;
use engine_gui::document::{RectSpec, UiDocument};
use engine_gui::events::{UiEventKind, UiValue};
use engine_gui::system::{UiCanvasHandle, UiElementHandle, UiSystem, UiSystemError};
use engine_scene::ObjectHandle;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Состояние UI, общее хосту и транзакциям (как физика и сеть).
#[derive(Default)]
pub(crate) struct UiScriptState {
    pub(crate) system: UiSystem,
    /// Предзагруженные документы по каноническому пути; `open` принимает только их.
    pub(crate) documents: BTreeMap<String, Arc<UiDocument>>,
    /// Канвас компонента `ui_canvas` по ключу владельца.
    pub(crate) component_canvases: HashMap<u64, UiCanvasHandle>,
}

/// Ключ владельца канваса: ABI-кодирование хендла объекта с поколением.
pub(crate) fn owner_key(object: ObjectHandle) -> u64 {
    object.to_abi().unwrap_or(0)
}

/// Событие UI, которое получает экземпляр владельца канваса.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UiDelivery {
    pub(crate) canvas: UiCanvasHandle,
    pub(crate) element: UiElementHandle,
    pub(crate) element_id: String,
    pub(crate) kind: UiEventKind,
    pub(crate) value: Option<UiValue>,
}

/// Предыдущее состояние, которое восстанавливает откат. Восстановление считается успешным:
/// значения были допустимы до записи.
#[derive(Debug)]
pub(crate) enum UiUndo {
    Opened(UiCanvasHandle),
    CanvasVisible(UiCanvasHandle, bool),
    CanvasLayer(UiCanvasHandle, i32),
    Text(UiElementHandle, String),
    /// `None` — у элемента не было значения (dropdown без выбора); такой откат не восстанавливает
    /// пустой выбор, потому что публичного API очистки значения нет.
    Value(UiElementHandle, Option<UiValue>),
    Options(UiElementHandle, Vec<String>, Option<UiValue>),
    Visible(UiElementHandle, bool),
    Interactable(UiElementHandle, bool),
    Color(UiElementHandle, [u8; 4]),
    /// `None` — у кнопки не было картинки; её нельзя снять публичным API.
    Image(UiElementHandle, Option<String>),
    Rect(UiElementHandle, RectSpec),
    Focus(Option<UiElementHandle>),
}

impl UiUndo {
    fn revert(self, system: &mut UiSystem) {
        // Ошибки отката не возникают: восстанавливаются значения, которые система уже принимала.
        let _ = match self {
            Self::Opened(canvas) => system.close(canvas).map(drop),
            Self::CanvasVisible(canvas, visible) => system.set_canvas_visible(canvas, visible),
            Self::CanvasLayer(canvas, layer) => system.set_layer(canvas, layer),
            Self::Text(element, text) => system.set_text(element, &text),
            Self::Value(element, value) => match value {
                Some(value) => system.set_value(element, value),
                None => Ok(()),
            },
            Self::Options(element, options, selected) => system
                .set_options(element, options)
                .and_then(|()| match selected {
                    Some(value) => system.set_value(element, value),
                    None => Ok(()),
                }),
            Self::Visible(element, visible) => system.set_visible(element, visible),
            Self::Interactable(element, interactable) => {
                system.set_interactable(element, interactable)
            }
            Self::Color(element, color) => system.set_color(element, color),
            Self::Image(element, Some(path)) => system.set_image(element, &path),
            Self::Image(_, None) => Ok(()),
            Self::Rect(element, rect) => system.set_rect(element, rect),
            Self::Focus(Some(element)) => system.focus(element).map(drop),
            Self::Focus(None) => {
                system.clear_focus();
                Ok(())
            }
        };
    }
}

/// Журнал UI-записей одного callback: откат в обратном порядке и отложенные закрытия.
#[derive(Default)]
pub(crate) struct UiJournal {
    undo: Vec<UiUndo>,
    closes: Vec<UiCanvasHandle>,
}

impl UiJournal {
    fn record(&mut self, entry: UiUndo) {
        self.undo.push(entry);
    }

    fn closing(&self, canvas: UiCanvasHandle) -> bool {
        self.closes.contains(&canvas)
    }

    /// Применяет отложенные закрытия. События закрытия не доставляются: канвас больше не существует.
    pub(crate) fn commit(&mut self, system: &mut UiSystem) {
        self.undo.clear();
        for canvas in self.closes.drain(..) {
            let _ = system.close(canvas);
        }
    }

    pub(crate) fn discard(&mut self, system: &mut UiSystem) {
        self.closes.clear();
        while let Some(entry) = self.undo.pop() {
            entry.revert(system);
        }
    }
}

fn stale() -> String {
    UiSystemError::StaleHandle.to_string()
}

fn ui_error(error: UiSystemError) -> String {
    error.to_string()
}

impl ScriptTransaction {
    /// UI-записи разрешены только в lifecycle-вызовах; чтения доступны везде.
    fn ui_write_guard(&self) -> Result<(), String> {
        if self.writable() {
            Ok(())
        } else {
            Err("UI-записи запрещены в create и в top-level chunk".to_owned())
        }
    }

    /// Канвас открыт и не закрыт в этом вызове.
    fn ui_live(&self, canvas: UiCanvasHandle) -> Result<(), String> {
        if self.ui_journal.closing(canvas) {
            return Err(stale());
        }
        self.ui
            .borrow()
            .system
            .layer(canvas)
            .map(drop)
            .map_err(ui_error)
    }

    pub(crate) fn ui_open(&mut self, path: &str) -> Result<UiCanvasHandle, String> {
        self.ui_write_guard()?;
        let owner = self
            .self_object
            .ok_or_else(|| "rve.ui.open нужен вызов из скрипта объекта".to_owned())?;
        let mut state = self.ui.borrow_mut();
        let document = state
            .documents
            .get(path)
            .cloned()
            .ok_or_else(|| format!("UI-документ {path:?} не зарегистрирован"))?;
        let canvas = state.system.open(document, owner_key(owner));
        self.ui_journal.record(UiUndo::Opened(canvas));
        Ok(canvas)
    }

    pub(crate) fn ui_close(&mut self, canvas: UiCanvasHandle) -> Result<(), String> {
        self.ui_write_guard()?;
        self.ui_live(canvas)?;
        self.ui_journal.closes.push(canvas);
        Ok(())
    }

    /// Канвас компонента `ui_canvas` объекта, если он открыт и не закрыт в этом вызове.
    pub(crate) fn ui_component_canvas(&self, object: ObjectHandle) -> Option<UiCanvasHandle> {
        let canvas = *self
            .ui
            .borrow()
            .component_canvases
            .get(&owner_key(object))?;
        self.ui_live(canvas).ok().map(|()| canvas)
    }

    pub(crate) fn ui_find(
        &self,
        canvas: UiCanvasHandle,
        id: &str,
    ) -> Result<Option<UiElementHandle>, String> {
        self.ui_live(canvas)?;
        match self.ui.borrow().system.find(canvas, id) {
            Ok(element) => Ok(Some(element)),
            Err(UiSystemError::UnknownElement(_)) => Ok(None),
            Err(error) => Err(ui_error(error)),
        }
    }

    pub(crate) fn ui_canvas_visible(&self, canvas: UiCanvasHandle) -> Result<bool, String> {
        self.ui_live(canvas)?;
        self.ui
            .borrow()
            .system
            .canvas_visible(canvas)
            .map_err(ui_error)
    }

    pub(crate) fn ui_canvas_layer(&self, canvas: UiCanvasHandle) -> Result<i32, String> {
        self.ui_live(canvas)?;
        self.ui.borrow().system.layer(canvas).map_err(ui_error)
    }

    pub(crate) fn ui_set_canvas_visible(
        &mut self,
        canvas: UiCanvasHandle,
        visible: bool,
    ) -> Result<(), String> {
        self.ui_write_guard()?;
        self.ui_live(canvas)?;
        let mut state = self.ui.borrow_mut();
        let previous = state.system.canvas_visible(canvas).map_err(ui_error)?;
        state
            .system
            .set_canvas_visible(canvas, visible)
            .map_err(ui_error)?;
        self.ui_journal
            .record(UiUndo::CanvasVisible(canvas, previous));
        Ok(())
    }

    pub(crate) fn ui_set_canvas_layer(
        &mut self,
        canvas: UiCanvasHandle,
        layer: i32,
    ) -> Result<(), String> {
        self.ui_write_guard()?;
        self.ui_live(canvas)?;
        let mut state = self.ui.borrow_mut();
        let previous = state.system.layer(canvas).map_err(ui_error)?;
        state.system.set_layer(canvas, layer).map_err(ui_error)?;
        self.ui_journal
            .record(UiUndo::CanvasLayer(canvas, previous));
        Ok(())
    }

    pub(crate) fn ui_focused(&self) -> Option<UiElementHandle> {
        let focused = self.ui.borrow().system.focused()?;
        self.ui_live(focused.canvas()).ok().map(|()| focused)
    }

    pub(crate) fn ui_pointer_over(&self) -> bool {
        self.ui.borrow().system.pointer_over()
    }

    fn ui_element_live(&self, element: UiElementHandle) -> Result<(), String> {
        self.ui_live(element.canvas())
    }

    /// Чтение свойства элемента: сначала проверка живости канваса, затем значение `UiSystem`.
    pub(crate) fn ui_read<T>(
        &self,
        element: UiElementHandle,
        read: impl FnOnce(&UiSystem, UiElementHandle) -> Result<T, UiSystemError>,
    ) -> Result<T, String> {
        self.ui_element_live(element)?;
        read(&self.ui.borrow().system, element).map_err(ui_error)
    }

    /// Запись свойства элемента: `capture` сохраняет прежнее значение, `apply` его меняет.
    fn ui_write(
        &mut self,
        element: UiElementHandle,
        capture: impl FnOnce(&UiSystem, UiElementHandle) -> Result<UiUndo, UiSystemError>,
        apply: impl FnOnce(&mut UiSystem, UiElementHandle) -> Result<(), UiSystemError>,
    ) -> Result<(), String> {
        self.ui_write_guard()?;
        self.ui_element_live(element)?;
        let mut state = self.ui.borrow_mut();
        let undo = capture(&state.system, element).map_err(ui_error)?;
        apply(&mut state.system, element).map_err(ui_error)?;
        self.ui_journal.record(undo);
        Ok(())
    }

    pub(crate) fn ui_element_id(&self, element: UiElementHandle) -> Result<String, String> {
        self.ui_read(element, |system, element| {
            system.element_id(element).map(str::to_owned)
        })
    }

    pub(crate) fn ui_text(&self, element: UiElementHandle) -> Result<String, String> {
        self.ui_read(element, |system, element| system.text(element))
    }

    pub(crate) fn ui_set_text(
        &mut self,
        element: UiElementHandle,
        text: &str,
    ) -> Result<(), String> {
        self.ui_write(
            element,
            |system, element| Ok(UiUndo::Text(element, system.text(element)?)),
            |system, element| system.set_text(element, text),
        )
    }

    pub(crate) fn ui_value(&self, element: UiElementHandle) -> Result<Option<UiValue>, String> {
        self.ui_read(element, |system, element| system.value(element))
    }

    pub(crate) fn ui_set_value(
        &mut self,
        element: UiElementHandle,
        value: UiValue,
    ) -> Result<(), String> {
        self.ui_write(
            element,
            |system, element| Ok(UiUndo::Value(element, system.value(element)?)),
            |system, element| system.set_value(element, value),
        )
    }

    pub(crate) fn ui_visible(&self, element: UiElementHandle) -> Result<bool, String> {
        self.ui_read(element, |system, element| system.visible(element))
    }

    pub(crate) fn ui_set_visible(
        &mut self,
        element: UiElementHandle,
        visible: bool,
    ) -> Result<(), String> {
        self.ui_write(
            element,
            |system, element| Ok(UiUndo::Visible(element, system.visible(element)?)),
            |system, element| system.set_visible(element, visible),
        )
    }

    pub(crate) fn ui_interactable(&self, element: UiElementHandle) -> Result<bool, String> {
        self.ui_read(element, |system, element| system.interactable(element))
    }

    pub(crate) fn ui_set_interactable(
        &mut self,
        element: UiElementHandle,
        interactable: bool,
    ) -> Result<(), String> {
        self.ui_write(
            element,
            |system, element| Ok(UiUndo::Interactable(element, system.interactable(element)?)),
            |system, element| system.set_interactable(element, interactable),
        )
    }

    pub(crate) fn ui_color(&self, element: UiElementHandle) -> Result<[u8; 4], String> {
        self.ui_read(element, |system, element| system.color(element))
    }

    pub(crate) fn ui_set_color(
        &mut self,
        element: UiElementHandle,
        color: [u8; 4],
    ) -> Result<(), String> {
        self.ui_write(
            element,
            |system, element| Ok(UiUndo::Color(element, system.color(element)?)),
            |system, element| system.set_color(element, color),
        )
    }

    pub(crate) fn ui_set_image(
        &mut self,
        element: UiElementHandle,
        path: &str,
    ) -> Result<(), String> {
        self.ui_write(
            element,
            |system, element| Ok(UiUndo::Image(element, system.image(element)?)),
            |system, element| system.set_image(element, path),
        )
    }

    pub(crate) fn ui_rect(&self, element: UiElementHandle) -> Result<RectSpec, String> {
        self.ui_read(element, |system, element| system.rect(element))
    }

    pub(crate) fn ui_set_rect(
        &mut self,
        element: UiElementHandle,
        rect: RectSpec,
    ) -> Result<(), String> {
        self.ui_write(
            element,
            |system, element| Ok(UiUndo::Rect(element, system.rect(element)?)),
            |system, element| system.set_rect(element, rect),
        )
    }

    pub(crate) fn ui_set_options(
        &mut self,
        element: UiElementHandle,
        options: Vec<String>,
    ) -> Result<(), String> {
        self.ui_write(
            element,
            |system, element| {
                Ok(UiUndo::Options(
                    element,
                    system.options(element)?,
                    system.value(element)?,
                ))
            },
            |system, element| system.set_options(element, options),
        )
    }

    pub(crate) fn ui_focus(&mut self, element: UiElementHandle) -> Result<(), String> {
        self.ui_write(
            element,
            |system, _| Ok(UiUndo::Focus(system.focused())),
            |system, element| system.focus(element).map(drop),
        )
    }

    /// Фиксирует отложенные закрытия после успешного callback.
    pub(crate) fn commit_ui(&mut self) {
        self.ui_journal.commit(&mut self.ui.borrow_mut().system);
    }

    /// Откатывает UI-записи вызова и отменяет закрытия.
    pub(crate) fn discard_ui(&mut self) {
        self.ui_journal.discard(&mut self.ui.borrow_mut().system);
    }
}
