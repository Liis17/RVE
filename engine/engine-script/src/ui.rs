//! Игровой UI в скриптах: общее состояние `UiSystem`, журнал отката и транзакционные операции.
//!
//! Записи UI применяются к `UiSystem` сразу, чтобы чтения в том же вызове видели их без отдельного
//! overlay. Перед первой записью в канвас журнал сохраняет его runtime-состояние; при ошибке или
//! panic callback снимки возвращаются, канвасы, открытые вызовом, закрываются, фокус восстанавливается.
//! Закрытие канваса откладывается до успешного commit: до него канвас остаётся видим, но любой доступ
//! к нему в этом вызове даёт ошибку устаревшего хендла. События фокуса и размытия, созданные вызовом,
//! попадают в очередь хоста только после успешного commit и доставляются в следующем update.

use crate::transaction::ScriptTransaction;
use engine_gui::document::{RectSpec, UiDocument};
use engine_gui::events::{UiEvent, UiEventKind, UiValue};
use engine_gui::system::{
    UiCanvasHandle, UiCanvasSnapshot, UiElementHandle, UiSystem, UiSystemError,
};
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
    /// События UI для доставки владельцам в следующем update.
    pub(crate) queued: Vec<QueuedUiEvent>,
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

/// Событие в очереди. Владелец и id элемента читаются при постановке: канвас может закрыться
/// до следующего update, а событие владельцу, который ещё жив, всё равно нужно доставить.
pub(crate) struct QueuedUiEvent {
    pub(crate) owner: ObjectHandle,
    pub(crate) delivery: UiDelivery,
}

/// Событие с живым канвасом; без владельца или id элемента его нельзя доставить.
pub(crate) fn queue_live(system: &UiSystem, event: UiEvent) -> Option<QueuedUiEvent> {
    let owner = ObjectHandle::from_abi(system.owner(event.canvas).ok()?)?;
    let element_id = system.element_id(event.element).ok()?.to_owned();
    Some(QueuedUiEvent {
        owner,
        delivery: UiDelivery {
            canvas: event.canvas,
            element: event.element,
            element_id,
            kind: event.kind,
            value: event.value,
        },
    })
}

/// Закрывает канвас при commit. Владелец и фокус читаются до закрытия. Blur элемента в фокусе
/// ставится в очередь, потому что владелец жив; hover_exit владельцу не доставляется.
fn close_queued(system: &mut UiSystem, canvas: UiCanvasHandle) -> Vec<QueuedUiEvent> {
    let owner = system.owner(canvas).ok().and_then(ObjectHandle::from_abi);
    let focus = system.focused().filter(|focus| focus.canvas() == canvas);
    let focus_id = focus.and_then(|handle| system.element_id(handle).ok().map(str::to_owned));
    let _ = system.close(canvas);
    match (owner, focus, focus_id) {
        (Some(owner), Some(element), Some(element_id)) => vec![QueuedUiEvent {
            owner,
            delivery: UiDelivery {
                canvas,
                element,
                element_id,
                kind: UiEventKind::Blur,
                value: None,
            },
        }],
        _ => Vec::new(),
    }
}

/// Журнал UI-записей одного callback: снимки канвасов, фокус, открытые и закрываемые канвасы.
#[derive(Default)]
pub(crate) struct UiJournal {
    /// Состояние канвасов, существовавших до первой записи в вызове.
    snapshots: Vec<(UiCanvasHandle, UiCanvasSnapshot)>,
    /// Фокус до первого изменения в вызове.
    focus_before: Option<Option<UiElementHandle>>,
    /// Канвасы, открытые вызовом: при откате закрываются.
    opened: Vec<UiCanvasHandle>,
    /// Отложенные закрытия: применяются при commit.
    closes: Vec<UiCanvasHandle>,
    /// События фокуса, созданные вызовом; попадают в очередь только после commit.
    events: Vec<QueuedUiEvent>,
}

impl UiJournal {
    /// Сохраняет состояние канваса перед первой записью в него в этом вызове.
    fn touch(&mut self, system: &UiSystem, canvas: UiCanvasHandle) -> Result<(), String> {
        if self.opened.contains(&canvas) || self.snapshots.iter().any(|(saved, _)| *saved == canvas)
        {
            return Ok(());
        }
        let snapshot = system.snapshot_canvas(canvas).map_err(ui_error)?;
        self.snapshots.push((canvas, snapshot));
        Ok(())
    }

    fn closing(&self, canvas: UiCanvasHandle) -> bool {
        self.closes.contains(&canvas)
    }

    /// Применяет отложенные закрытия и возвращает события для очереди хоста.
    pub(crate) fn commit(&mut self, system: &mut UiSystem) -> Vec<QueuedUiEvent> {
        self.snapshots.clear();
        self.focus_before = None;
        self.opened.clear();
        let mut queued = std::mem::take(&mut self.events);
        for canvas in self.closes.drain(..) {
            queued.extend(close_queued(system, canvas));
        }
        queued
    }

    /// Возвращает состояние до вызова: закрывает открытые канвасы, восстанавливает снимки и фокус.
    /// События вызова отбрасываются, потому что вызов не произошёл.
    pub(crate) fn discard(&mut self, system: &mut UiSystem) {
        self.closes.clear();
        self.events.clear();
        for canvas in self.opened.drain(..) {
            let _ = system.close(canvas);
        }
        for (canvas, snapshot) in self.snapshots.drain(..) {
            let _ = system.restore_canvas(canvas, snapshot);
        }
        if let Some(focus) = self.focus_before.take() {
            let _ = system.restore_focus(focus);
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
        self.ui_journal.opened.push(canvas);
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

    /// Элемент по индексу в документе канваса (ABI). Индекс вне документа — ошибка.
    pub(crate) fn ui_element_at(
        &self,
        canvas: UiCanvasHandle,
        index: usize,
    ) -> Result<UiElementHandle, String> {
        self.ui_live(canvas)?;
        self.ui
            .borrow()
            .system
            .element(canvas, index)
            .map_err(ui_error)
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
        self.ui_journal.touch(&state.system, canvas)?;
        state
            .system
            .set_canvas_visible(canvas, visible)
            .map_err(ui_error)
    }

    pub(crate) fn ui_set_canvas_layer(
        &mut self,
        canvas: UiCanvasHandle,
        layer: i32,
    ) -> Result<(), String> {
        self.ui_write_guard()?;
        self.ui_live(canvas)?;
        let mut state = self.ui.borrow_mut();
        self.ui_journal.touch(&state.system, canvas)?;
        state.system.set_layer(canvas, layer).map_err(ui_error)
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

    /// Запись в элемент: проверяет право записи и живость канваса, сохраняет его снимок
    /// при первой записи в вызове и затем применяет изменение.
    fn ui_write(
        &mut self,
        element: UiElementHandle,
        apply: impl FnOnce(&mut UiSystem, UiElementHandle) -> Result<(), UiSystemError>,
    ) -> Result<(), String> {
        self.ui_write_guard()?;
        self.ui_element_live(element)?;
        let mut state = self.ui.borrow_mut();
        self.ui_journal.touch(&state.system, element.canvas())?;
        apply(&mut state.system, element).map_err(ui_error)
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
        self.ui_write(element, |system, element| system.set_text(element, text))
    }

    pub(crate) fn ui_value(&self, element: UiElementHandle) -> Result<Option<UiValue>, String> {
        self.ui_read(element, |system, element| system.value(element))
    }

    pub(crate) fn ui_set_value(
        &mut self,
        element: UiElementHandle,
        value: UiValue,
    ) -> Result<(), String> {
        self.ui_write(element, |system, element| system.set_value(element, value))
    }

    pub(crate) fn ui_visible(&self, element: UiElementHandle) -> Result<bool, String> {
        self.ui_read(element, |system, element| system.visible(element))
    }

    pub(crate) fn ui_set_visible(
        &mut self,
        element: UiElementHandle,
        visible: bool,
    ) -> Result<(), String> {
        self.ui_write(element, |system, element| {
            system.set_visible(element, visible)
        })
    }

    pub(crate) fn ui_interactable(&self, element: UiElementHandle) -> Result<bool, String> {
        self.ui_read(element, |system, element| system.interactable(element))
    }

    pub(crate) fn ui_set_interactable(
        &mut self,
        element: UiElementHandle,
        interactable: bool,
    ) -> Result<(), String> {
        self.ui_write(element, |system, element| {
            system.set_interactable(element, interactable)
        })
    }

    pub(crate) fn ui_color(&self, element: UiElementHandle) -> Result<[u8; 4], String> {
        self.ui_read(element, |system, element| system.color(element))
    }

    pub(crate) fn ui_set_color(
        &mut self,
        element: UiElementHandle,
        color: [u8; 4],
    ) -> Result<(), String> {
        self.ui_write(element, |system, element| system.set_color(element, color))
    }

    pub(crate) fn ui_set_image(
        &mut self,
        element: UiElementHandle,
        path: &str,
    ) -> Result<(), String> {
        self.ui_write(element, |system, element| system.set_image(element, path))
    }

    pub(crate) fn ui_rect(&self, element: UiElementHandle) -> Result<RectSpec, String> {
        self.ui_read(element, |system, element| system.rect(element))
    }

    pub(crate) fn ui_set_rect(
        &mut self,
        element: UiElementHandle,
        rect: RectSpec,
    ) -> Result<(), String> {
        self.ui_write(element, |system, element| system.set_rect(element, rect))
    }

    pub(crate) fn ui_set_options(
        &mut self,
        element: UiElementHandle,
        options: Vec<String>,
    ) -> Result<(), String> {
        self.ui_write(element, |system, element| {
            system.set_options(element, options)
        })
    }

    /// Фокус записывается как изменение: до него сохраняется фокус вызова, а `focus`/`blur`
    /// уходят в очередь только после commit.
    pub(crate) fn ui_focus(&mut self, element: UiElementHandle) -> Result<(), String> {
        self.ui_write_guard()?;
        self.ui_element_live(element)?;
        let mut state = self.ui.borrow_mut();
        let previous = state.system.focused();
        self.ui_journal.touch(&state.system, element.canvas())?;
        if let Some(old) = previous {
            self.ui_journal.touch(&state.system, old.canvas())?;
        }
        if self.ui_journal.focus_before.is_none() {
            self.ui_journal.focus_before = Some(previous);
        }
        let events = state.system.focus(element).map_err(ui_error)?;
        let state = &*state;
        self.ui_journal.events.extend(
            events
                .into_iter()
                .filter_map(|event| queue_live(&state.system, event)),
        );
        Ok(())
    }

    /// Фиксирует отложенные закрытия и передаёт события фокуса в очередь хоста.
    pub(crate) fn commit_ui(&mut self) {
        let mut state = self.ui.borrow_mut();
        let queued = self.ui_journal.commit(&mut state.system);
        state.queued.extend(queued);
    }

    /// Откатывает UI-записи вызова и отменяет закрытия.
    pub(crate) fn discard_ui(&mut self) {
        self.ui_journal.discard(&mut self.ui.borrow_mut().system);
    }
}
