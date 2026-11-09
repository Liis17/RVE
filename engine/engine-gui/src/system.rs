//! Состояние UI: открытые канвасы, хендлы с поколениями и свойства элементов.
//! Ввод, навигация и события — в [`crate::input`]; этот модуль не знает о клавишах и окне.

use crate::document::{
    AssetKind, ContentType, ElementKind, RectSpec, SliderSpec, Tree, UiDocument, UiElement, UiPath,
    accepts_text, asset_kind, check_rect,
};
use crate::events::{UiEvent, UiEventKind, UiValue};
use crate::layout::{
    Layout, canvas_scale, canvas_size, layout as compute_layout, max_scroll, resolve_rect,
};
use epaint::Vec2;
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// Число проходов layout при ограничении прокрутки: вложенные ScrollView могут сдвинуть друг друга.
const SCROLL_PASSES: usize = 4;

/// Хендл открытого канваса. Поколение меняется при закрытии, поэтому старый хендл отклоняется.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UiCanvasHandle {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

impl UiCanvasHandle {
    /// Хендл из пары «индекс слота, поколение» для внешнего кодирования (ABI скриптов).
    /// Чужой или устаревший хендл отклоняется при первом использовании, а не здесь.
    pub fn from_raw(index: u32, generation: u32) -> Self {
        Self { index, generation }
    }

    pub fn index(self) -> u32 {
        self.index
    }

    pub fn generation(self) -> u32 {
        self.generation
    }
}

/// Хендл элемента: канвас (с поколением) и индекс элемента в документе.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct UiElementHandle {
    pub(crate) canvas: UiCanvasHandle,
    pub(crate) index: u32,
}

impl UiElementHandle {
    pub fn canvas(self) -> UiCanvasHandle {
        self.canvas
    }

    /// Индекс элемента в порядке объявления документа.
    pub fn index(self) -> usize {
        self.index as usize
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiSystemError {
    /// Хендл закрытого канваса; слот мог быть занят новым канвасом.
    StaleHandle,
    UnknownElement(String),
    /// Индекс элемента вне документа канваса.
    UnknownIndex(usize),
    /// У элемента нет такого свойства или действия.
    Unsupported {
        element: String,
        property: &'static str,
    },
    InvalidValue(String),
}

impl fmt::Display for UiSystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleHandle => f.write_str("хендл UI устарел: канвас закрыт"),
            Self::UnknownElement(id) => write!(f, "элемент {id:?} не объявлен в документе"),
            Self::UnknownIndex(index) => {
                write!(f, "элемент с индексом {index} не объявлен в документе")
            }
            Self::Unsupported { element, property } => {
                write!(f, "элемент {element:?} не поддерживает свойство {property}")
            }
            Self::InvalidValue(message) => f.write_str(message),
        }
    }
}

impl Error for UiSystemError {}

/// Открытый канвас: документ, копия элементов с изменяемыми свойствами и кэш layout.
pub(crate) struct UiCanvas {
    pub(crate) document: Arc<UiDocument>,
    pub(crate) owner: u64,
    pub(crate) layer: i32,
    pub(crate) visible: bool,
    pub(crate) sequence: u64,
    pub(crate) elements: Vec<UiElement>,
    /// Курсор InputField в символах или подсвеченный пункт открытого Dropdown.
    pub(crate) cursors: Vec<usize>,
    pub(crate) scroll: Vec<Vec2>,
    pub(crate) layout: Layout,
    /// Физических пикселей на единицу канваса для последнего layout.
    pub(crate) scale: f32,
    pub(crate) layout_dirty: bool,
    /// Индекс открытого Dropdown.
    pub(crate) popup: Option<usize>,
    pub(crate) default_focus_pending: bool,
}

/// Runtime-состояние канваса, которое меняют записи: элементы, курсоры, прокрутка, слой и
/// открытый список. Документ и хендлы не входят — записи их не меняют.
#[derive(Clone)]
pub struct UiCanvasSnapshot {
    layer: i32,
    visible: bool,
    elements: Vec<UiElement>,
    cursors: Vec<usize>,
    scroll: Vec<Vec2>,
    popup: Option<usize>,
}

pub(crate) struct Slot {
    pub(crate) generation: u32,
    pub(crate) canvas: Option<UiCanvas>,
}

/// Все открытые канвасы и общее состояние указателя и фокуса.
#[derive(Default)]
pub struct UiSystem {
    pub(crate) slots: Vec<Slot>,
    free: Vec<u32>,
    pub(crate) sequence: u64,
    pub(crate) screen: Vec2,
    /// Шрифты по каноническому пути ассета: байты нужны для пересоздания epaint.
    pub(crate) font_data: BTreeMap<String, Vec<u8>>,
    pub(crate) fonts: Option<epaint::text::Fonts>,
    /// Размеры картинок по пути: нужны для 9-slice и пропорций.
    pub(crate) images: BTreeMap<String, [u32; 2]>,
    pub(crate) atlas: Arc<crate::paint::FontAtlas>,
    pub(crate) paint_dirty: bool,
    pub(crate) last_paint: Option<crate::paint::UiPaint>,
    pub(crate) hover: Option<UiElementHandle>,
    pub(crate) press: Option<crate::input::Hit>,
    pub(crate) drag_last: Vec2,
    pub(crate) focus: Option<UiElementHandle>,
    pub(crate) repeat: Option<crate::input::Repeat>,
    pub(crate) pointer_over: bool,
}

impl UiSystem {
    /// Открывает канвас документа, принадлежащий `owner` (непрозрачный ключ объекта вызывающей стороны).
    /// Канвас сразу получает layout; `default_focus` применяется при следующем [`UiSystem::update`].
    pub fn open(&mut self, document: Arc<UiDocument>, owner: u64) -> UiCanvasHandle {
        let count = document.elements().len();
        let index = match self.free.pop() {
            Some(index) => index,
            None => {
                self.slots.push(Slot {
                    generation: 0,
                    canvas: None,
                });
                (self.slots.len() - 1) as u32
            }
        };
        self.sequence += 1;
        let generation = self.slots[index as usize].generation;
        let mut canvas = UiCanvas {
            layer: document.canvas.layer,
            visible: true,
            sequence: self.sequence,
            owner,
            elements: document.elements().to_vec(),
            cursors: vec![0; count],
            scroll: vec![Vec2::ZERO; count],
            layout: Layout::default(),
            scale: 1.0,
            layout_dirty: true,
            popup: None,
            default_focus_pending: document.canvas.default_focus.is_some(),
            document,
        };
        refresh_layout(&mut canvas, self.screen);
        self.slots[index as usize].canvas = Some(canvas);
        self.paint_dirty = true;
        UiCanvasHandle { index, generation }
    }

    /// Закрывает канвас. Возвращает blur/hover_exit для элементов, которые были в фокусе или под курсором.
    pub fn close(&mut self, canvas: UiCanvasHandle) -> Result<Vec<UiEvent>, UiSystemError> {
        self.canvas_ref(canvas)?;
        let mut events = Vec::new();
        self.clear_interaction_in(canvas, &mut events);
        let slot = &mut self.slots[canvas.index as usize];
        slot.canvas = None;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(canvas.index);
        self.paint_dirty = true;
        Ok(events)
    }

    /// Закрывает все канвасы владельца; события возвращаются так же, как у [`UiSystem::close`].
    pub fn close_owned(&mut self, owner: u64) -> Vec<UiEvent> {
        let owned: Vec<UiCanvasHandle> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let canvas = slot.canvas.as_ref()?;
                (canvas.owner == owner).then_some(UiCanvasHandle {
                    index: index as u32,
                    generation: slot.generation,
                })
            })
            .collect();
        let mut events = Vec::new();
        for handle in owned {
            if let Ok(mut closed) = self.close(handle) {
                events.append(&mut closed);
            }
        }
        events
    }

    pub fn set_layer(&mut self, canvas: UiCanvasHandle, layer: i32) -> Result<(), UiSystemError> {
        self.canvas_mut(canvas)?.layer = layer;
        Ok(())
    }

    pub fn set_canvas_visible(
        &mut self,
        canvas: UiCanvasHandle,
        visible: bool,
    ) -> Result<(), UiSystemError> {
        self.canvas_mut(canvas)?.visible = visible;
        Ok(())
    }

    /// Элемент по id из документа канваса.
    pub fn find(&self, canvas: UiCanvasHandle, id: &str) -> Result<UiElementHandle, UiSystemError> {
        let index = self
            .canvas_ref(canvas)?
            .document
            .element_index(id)
            .ok_or_else(|| UiSystemError::UnknownElement(id.to_owned()))?;
        Ok(UiElementHandle {
            canvas,
            index: index as u32,
        })
    }

    /// Хендл элемента по индексу в порядке объявления документа; индекс вне документа — ошибка.
    pub fn element(
        &self,
        canvas: UiCanvasHandle,
        index: usize,
    ) -> Result<UiElementHandle, UiSystemError> {
        if index >= self.canvas_ref(canvas)?.elements.len() {
            return Err(UiSystemError::UnknownIndex(index));
        }
        Ok(UiElementHandle {
            canvas,
            index: index as u32,
        })
    }

    /// Идентификатор элемента из документа.
    pub fn element_id(&self, handle: UiElementHandle) -> Result<&str, UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        Ok(canvas.elements[index].id.as_str())
    }

    /// Слой канваса с учётом переопределения скриптом.
    pub fn layer(&self, canvas: UiCanvasHandle) -> Result<i32, UiSystemError> {
        Ok(self.canvas_ref(canvas)?.layer)
    }

    /// Ключ владельца, переданный в [`UiSystem::open`].
    pub fn owner(&self, canvas: UiCanvasHandle) -> Result<u64, UiSystemError> {
        Ok(self.canvas_ref(canvas)?.owner)
    }

    /// Видим ли канвас; скрытый канвас остаётся открытым и сохраняет состояние элементов.
    pub fn canvas_visible(&self, canvas: UiCanvasHandle) -> Result<bool, UiSystemError> {
        Ok(self.canvas_ref(canvas)?.visible)
    }

    /// Элемент в фокусе; фокус всегда на живом элементе после следующего update.
    pub fn focused(&self) -> Option<UiElementHandle> {
        self.focus
    }

    /// Курсор был над интерактивным элементом или открытым Dropdown на последнем update.
    pub fn pointer_over(&self) -> bool {
        self.pointer_over
    }

    /// Текст `Text` или `InputField`.
    pub fn text(&self, handle: UiElementHandle) -> Result<String, UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        match &canvas.elements[index].kind {
            ElementKind::Text(spec) => Ok(spec.string.clone()),
            ElementKind::InputField(spec) => Ok(spec.text.clone()),
            _ => Err(unsupported(&canvas.elements[index].id, "text")),
        }
    }

    /// Задаёт текст; для `InputField` значение проверяется по content и max_length.
    pub fn set_text(&mut self, handle: UiElementHandle, text: &str) -> Result<(), UiSystemError> {
        let (canvas, index) = self.element_mut(handle)?;
        let id = canvas.elements[index].id.clone();
        match &mut canvas.elements[index].kind {
            ElementKind::Text(spec) => spec.string = text.to_owned(),
            ElementKind::InputField(spec) => {
                check_input_text(spec.content, spec.max_length, text)
                    .map_err(UiSystemError::InvalidValue)?;
                spec.text = text.to_owned();
                let length = text.chars().count();
                let cursor = &mut canvas.cursors[index];
                *cursor = (*cursor).min(length);
            }
            _ => return Err(unsupported(&id, "text")),
        }
        Ok(())
    }

    /// Число для слайдера и прогресса, флаг для переключателя, индекс для Dropdown, текст для InputField.
    pub fn value(&self, handle: UiElementHandle) -> Result<Option<UiValue>, UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        Ok(match &canvas.elements[index].kind {
            ElementKind::Slider(spec) => Some(UiValue::Number(spec.value)),
            ElementKind::ProgressBar(spec) => Some(UiValue::Number(spec.value)),
            ElementKind::Toggle(spec) => Some(UiValue::Bool(spec.is_on)),
            ElementKind::Dropdown(spec) => spec.selected.map(UiValue::Index),
            ElementKind::InputField(spec) => Some(UiValue::Text(spec.text.clone())),
            _ => return Err(unsupported(&canvas.elements[index].id, "value")),
        })
    }

    /// Задаёт значение. Слайдер и прогресс ограничивают число диапазоном, остальные значения проверяются.
    pub fn set_value(
        &mut self,
        handle: UiElementHandle,
        value: UiValue,
    ) -> Result<(), UiSystemError> {
        let (canvas, index) = self.element_mut(handle)?;
        let id = canvas.elements[index].id.clone();
        let mismatch =
            || UiSystemError::InvalidValue(format!("элемент {id:?}: тип значения не подходит"));
        match (&mut canvas.elements[index].kind, value) {
            (ElementKind::Slider(spec), UiValue::Number(number)) => {
                spec.value = clamp_slider(spec, number);
            }
            (ElementKind::ProgressBar(spec), UiValue::Number(number)) => {
                spec.value = number.clamp(0.0, 1.0);
            }
            (ElementKind::Toggle(spec), UiValue::Bool(on)) => spec.is_on = on,
            (ElementKind::Dropdown(spec), UiValue::Index(selected)) => {
                if selected >= spec.options.len() {
                    return Err(UiSystemError::InvalidValue(format!(
                        "элемент {id:?}: индекс {selected} вне списка options"
                    )));
                }
                spec.selected = Some(selected);
            }
            (ElementKind::InputField(spec), UiValue::Text(text)) => {
                check_input_text(spec.content, spec.max_length, &text)
                    .map_err(UiSystemError::InvalidValue)?;
                let length = text.chars().count();
                spec.text = text;
                let cursor = &mut canvas.cursors[index];
                *cursor = (*cursor).min(length);
            }
            (ElementKind::Slider(_) | ElementKind::ProgressBar(_) | ElementKind::Toggle(_), _)
            | (ElementKind::Dropdown(_) | ElementKind::InputField(_), _) => return Err(mismatch()),
            _ => return Err(unsupported(&id, "value")),
        }
        Ok(())
    }

    pub fn visible(&self, handle: UiElementHandle) -> Result<bool, UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        Ok(canvas.elements[index].visible)
    }

    /// Скрытие меняет layout; видимость предков учитывается в layout.
    pub fn set_visible(
        &mut self,
        handle: UiElementHandle,
        visible: bool,
    ) -> Result<(), UiSystemError> {
        let (canvas, index) = self.element_mut(handle)?;
        canvas.elements[index].visible = visible;
        canvas.layout_dirty = true;
        Ok(())
    }

    pub fn interactable(&self, handle: UiElementHandle) -> Result<bool, UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        Ok(canvas.elements[index].interactable)
    }

    pub fn set_interactable(
        &mut self,
        handle: UiElementHandle,
        interactable: bool,
    ) -> Result<(), UiSystemError> {
        let (canvas, index) = self.element_mut(handle)?;
        canvas.elements[index].interactable = interactable;
        canvas.layout_dirty = true;
        Ok(())
    }

    /// Основной цвет: фон панели, цвет текста и поля, оттенок картинки, нормальный оттенок кнопки,
    /// цвет галочки, заливка слайдера и прогресса.
    pub fn color(&self, handle: UiElementHandle) -> Result<[u8; 4], UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        let element = &canvas.elements[index];
        Ok(match &element.kind {
            ElementKind::Panel(spec) => spec.color,
            ElementKind::Text(spec) => spec.color,
            ElementKind::Image(spec) => spec.tint,
            ElementKind::Button(spec) => spec.tint.normal.unwrap_or([255; 4]),
            ElementKind::InputField(spec) => spec.color,
            ElementKind::Dropdown(spec) => spec.color,
            ElementKind::Toggle(spec) => spec.checkmark_color,
            ElementKind::Slider(spec) => spec.fill_color,
            ElementKind::ProgressBar(spec) => spec.fill_color,
            _ => return Err(unsupported(&element.id, "color")),
        })
    }

    pub fn set_color(
        &mut self,
        handle: UiElementHandle,
        color: [u8; 4],
    ) -> Result<(), UiSystemError> {
        let (canvas, index) = self.element_mut(handle)?;
        let element = &mut canvas.elements[index];
        let slot = match &mut element.kind {
            ElementKind::Panel(spec) => &mut spec.color,
            ElementKind::Text(spec) => &mut spec.color,
            ElementKind::Image(spec) => &mut spec.tint,
            ElementKind::Button(spec) => spec.tint.normal.get_or_insert([255; 4]),
            ElementKind::InputField(spec) => &mut spec.color,
            ElementKind::Dropdown(spec) => &mut spec.color,
            ElementKind::Toggle(spec) => &mut spec.checkmark_color,
            ElementKind::Slider(spec) => &mut spec.fill_color,
            ElementKind::ProgressBar(spec) => &mut spec.fill_color,
            _ => return Err(unsupported(&element.id, "color")),
        };
        *slot = color;
        Ok(())
    }

    /// Путь картинки у `Image` или нормального состояния `Button`.
    pub fn image(&self, handle: UiElementHandle) -> Result<Option<String>, UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        let element = &canvas.elements[index];
        match &element.kind {
            ElementKind::Image(spec) => Ok(Some(spec.path.as_str().to_owned())),
            ElementKind::Button(spec) => Ok(spec
                .image
                .normal
                .as_ref()
                .map(|path| path.as_str().to_owned())),
            _ => Err(unsupported(&element.id, "image")),
        }
    }

    /// Путь должен быть объявлен в `[[assets]]` документа как картинка `.png`.
    pub fn set_image(&mut self, handle: UiElementHandle, path: &str) -> Result<(), UiSystemError> {
        let (canvas, index) = self.element_mut(handle)?;
        let id = canvas.elements[index].id.clone();
        let path = declared_image(&canvas.document, path)?;
        match &mut canvas.elements[index].kind {
            ElementKind::Image(spec) => spec.path = path,
            ElementKind::Button(spec) => spec.image.normal = Some(path),
            _ => return Err(unsupported(&id, "image")),
        }
        Ok(())
    }

    pub fn rect(&self, handle: UiElementHandle) -> Result<RectSpec, UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        Ok(canvas.elements[index].rect)
    }

    /// Заменяет прямоугольник элемента; изменение применяется при следующем update.
    pub fn set_rect(
        &mut self,
        handle: UiElementHandle,
        rect: RectSpec,
    ) -> Result<(), UiSystemError> {
        let (canvas, index) = self.element_mut(handle)?;
        check_rect(&rect).map_err(UiSystemError::InvalidValue)?;
        canvas.elements[index].rect = rect;
        canvas.layout_dirty = true;
        Ok(())
    }

    pub fn options(&self, handle: UiElementHandle) -> Result<Vec<String>, UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        match &canvas.elements[index].kind {
            ElementKind::Dropdown(spec) => Ok(spec.options.clone()),
            _ => Err(unsupported(&canvas.elements[index].id, "options")),
        }
    }

    /// Новый список; выбор сохраняется, если индекс остался в списке.
    pub fn set_options(
        &mut self,
        handle: UiElementHandle,
        options: Vec<String>,
    ) -> Result<(), UiSystemError> {
        let (canvas, index) = self.element_mut(handle)?;
        let id = canvas.elements[index].id.clone();
        match &mut canvas.elements[index].kind {
            ElementKind::Dropdown(spec) => {
                if spec
                    .selected
                    .is_some_and(|selected| selected >= options.len())
                {
                    spec.selected = None;
                }
                spec.options = options;
            }
            _ => return Err(unsupported(&id, "options")),
        }
        if canvas.popup == Some(index) {
            canvas.popup = None;
        }
        Ok(())
    }

    /// Копия runtime-состояния канваса; возвращается в [`UiSystem::restore_canvas`] при откате.
    pub fn snapshot_canvas(
        &self,
        canvas: UiCanvasHandle,
    ) -> Result<UiCanvasSnapshot, UiSystemError> {
        let canvas = self.canvas_ref(canvas)?;
        Ok(UiCanvasSnapshot {
            layer: canvas.layer,
            visible: canvas.visible,
            elements: canvas.elements.clone(),
            cursors: canvas.cursors.clone(),
            scroll: canvas.scroll.clone(),
            popup: canvas.popup,
        })
    }

    /// Возвращает канвас к снимку. Layout пересчитывается при следующем update.
    pub fn restore_canvas(
        &mut self,
        canvas: UiCanvasHandle,
        snapshot: UiCanvasSnapshot,
    ) -> Result<(), UiSystemError> {
        let target = self.canvas_mut(canvas)?;
        target.layer = snapshot.layer;
        target.visible = snapshot.visible;
        target.elements = snapshot.elements;
        target.cursors = snapshot.cursors;
        target.scroll = snapshot.scroll;
        target.popup = snapshot.popup;
        target.layout_dirty = true;
        Ok(())
    }

    /// Ставит фокус без событий `focus`/`blur`: откат не должен порождать новых событий.
    pub fn restore_focus(&mut self, focus: Option<UiElementHandle>) -> Result<(), UiSystemError> {
        if let Some(handle) = focus {
            self.element_ref(handle)?;
        }
        self.paint_dirty = true;
        self.focus = focus;
        Ok(())
    }

    /// Сбрасывает наведение, нажатие и фокус (потеря фокуса окна, открытие консоли).
    /// Уже поглощённые нажатия остаются скрытыми до отпускания — это решает [`crate::capture`].
    pub fn reset_interaction(&mut self) -> Vec<UiEvent> {
        self.paint_dirty = true;
        let mut events = Vec::new();
        self.clear_interaction(&mut events);
        events
    }

    fn clear_interaction(&mut self, events: &mut Vec<UiEvent>) {
        if let Some(hover) = self.hover.take() {
            events.push(event_for(hover, UiEventKind::HoverExit, None));
        }
        if let Some(focus) = self.focus.take() {
            events.push(event_for(focus, UiEventKind::Blur, None));
        }
        self.press = None;
        self.repeat = None;
        self.pointer_over = false;
        for slot in &mut self.slots {
            if let Some(canvas) = &mut slot.canvas {
                canvas.popup = None;
            }
        }
    }

    fn clear_interaction_in(&mut self, canvas: UiCanvasHandle, events: &mut Vec<UiEvent>) {
        if self.hover.is_some_and(|hover| hover.canvas == canvas) {
            let hover = self.hover.take().expect("hover проверен выше");
            events.push(event_for(hover, UiEventKind::HoverExit, None));
        }
        if self.focus.is_some_and(|focus| focus.canvas == canvas) {
            let focus = self.focus.take().expect("focus проверен выше");
            events.push(event_for(focus, UiEventKind::Blur, None));
        }
        if self
            .press
            .is_some_and(|press| press.element.canvas == canvas)
        {
            self.press = None;
        }
        self.pointer_over = false;
    }

    pub(crate) fn canvas_ref(&self, canvas: UiCanvasHandle) -> Result<&UiCanvas, UiSystemError> {
        self.slots
            .get(canvas.index as usize)
            .filter(|slot| slot.generation == canvas.generation)
            .and_then(|slot| slot.canvas.as_ref())
            .ok_or(UiSystemError::StaleHandle)
    }

    pub(crate) fn canvas_mut(
        &mut self,
        canvas: UiCanvasHandle,
    ) -> Result<&mut UiCanvas, UiSystemError> {
        self.paint_dirty = true;
        self.slots
            .get_mut(canvas.index as usize)
            .filter(|slot| slot.generation == canvas.generation)
            .and_then(|slot| slot.canvas.as_mut())
            .ok_or(UiSystemError::StaleHandle)
    }

    pub(crate) fn element_ref(
        &self,
        handle: UiElementHandle,
    ) -> Result<(&UiCanvas, usize), UiSystemError> {
        Ok((self.canvas_ref(handle.canvas)?, handle.index as usize))
    }

    pub(crate) fn element_mut(
        &mut self,
        handle: UiElementHandle,
    ) -> Result<(&mut UiCanvas, usize), UiSystemError> {
        Ok((self.canvas_mut(handle.canvas)?, handle.index as usize))
    }

    /// Хендлы всех открытых канвасов, сверху вниз: сначала больший слой, потом позднее открытый.
    pub(crate) fn canvases_top_down(&self) -> Vec<UiCanvasHandle> {
        let mut open: Vec<(i32, u64, UiCanvasHandle)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let canvas = slot.canvas.as_ref()?;
                let handle = UiCanvasHandle {
                    index: index as u32,
                    generation: slot.generation,
                };
                Some((canvas.layer, canvas.sequence, handle))
            })
            .collect();
        open.sort_by_key(|&(layer, sequence, _)| std::cmp::Reverse((layer, sequence)));
        open.into_iter().map(|(_, _, handle)| handle).collect()
    }

    /// Пересчитывает layout канвасов, помеченных как изменённые, и всех при смене экрана.
    pub(crate) fn refresh_layouts(&mut self, screen_changed: bool) {
        let screen = self.screen;
        for slot in &mut self.slots {
            if let Some(canvas) = &mut slot.canvas
                && (screen_changed || canvas.layout_dirty)
            {
                refresh_layout(canvas, screen);
            }
        }
    }
}

pub(crate) fn event_for(
    element: UiElementHandle,
    kind: UiEventKind,
    value: Option<UiValue>,
) -> UiEvent {
    UiEvent {
        canvas: element.canvas,
        element,
        kind,
        value,
    }
}

/// Предел прокрутки ScrollView по включённым осям: на сколько содержимое больше окна.
pub(crate) fn scroll_limit(
    layout: &Layout,
    elements: &[UiElement],
    tree: &Tree,
    index: usize,
) -> Vec2 {
    let ElementKind::ScrollView(spec) = &elements[index].kind else {
        return Vec2::ZERO;
    };
    let view = layout.rects[index];
    let child = tree.children[index][0];
    let content = resolve_rect(&elements[child].rect, view);
    let range = max_scroll(view, content);
    vec2_axes(range, spec.horizontal, spec.vertical)
}

fn vec2_axes(range: Vec2, horizontal: bool, vertical: bool) -> Vec2 {
    Vec2::new(
        if horizontal { range.x } else { 0.0 },
        if vertical { range.y } else { 0.0 },
    )
}

fn unsupported(element: &str, property: &'static str) -> UiSystemError {
    UiSystemError::Unsupported {
        element: element.to_owned(),
        property,
    }
}

/// Слайдер: значение в диапазоне; для целых чисел — округление.
pub(crate) fn clamp_slider(spec: &SliderSpec, value: f32) -> f32 {
    let clamped = value.clamp(spec.min, spec.max);
    if spec.whole_numbers {
        clamped.round().clamp(spec.min, spec.max)
    } else {
        clamped
    }
}

fn check_input_text(
    content: ContentType,
    max_length: Option<usize>,
    text: &str,
) -> Result<(), String> {
    if !accepts_text(content, text) {
        return Err("текст не соответствует content поля ввода".to_owned());
    }
    if max_length.is_some_and(|max| text.chars().count() > max) {
        return Err("текст длиннее max_length поля ввода".to_owned());
    }
    Ok(())
}

fn declared_image(document: &UiDocument, path: &str) -> Result<UiPath, UiSystemError> {
    let parsed = UiPath::parse(path).map_err(UiSystemError::InvalidValue)?;
    let declared = document
        .assets()
        .iter()
        .any(|asset| asset.path == parsed && asset_kind(parsed.as_str()) == Some(AssetKind::Image));
    if declared {
        Ok(parsed)
    } else {
        Err(UiSystemError::InvalidValue(format!(
            "картинка /{} не объявлена в [[assets]] как .png",
            parsed.as_str()
        )))
    }
}

/// Строит layout и ограничивает прокрутку. Ограничение может сдвинуть содержимое, поэтому
/// layout повторяется, пока пределы меняются (не больше [`SCROLL_PASSES`] раз).
fn refresh_layout(canvas: &mut UiCanvas, screen: Vec2) {
    let scale = canvas_scale(&canvas.document.canvas, screen);
    let size = canvas_size(screen, scale);
    let tree = canvas.document.tree();
    let mut result = compute_layout(tree, &canvas.elements, size, &canvas.scroll);
    for _ in 0..SCROLL_PASSES {
        let mut changed = false;
        for index in 0..canvas.elements.len() {
            if !result.visible[index]
                || !matches!(canvas.elements[index].kind, ElementKind::ScrollView(_))
            {
                continue;
            }
            let limit = scroll_limit(&result, &canvas.elements, tree, index);
            let current = canvas.scroll[index];
            let clamped = Vec2::new(current.x.clamp(0.0, limit.x), current.y.clamp(0.0, limit.y));
            if clamped != current {
                canvas.scroll[index] = clamped;
                changed = true;
            }
        }
        if !changed {
            break;
        }
        result = compute_layout(tree, &canvas.elements, size, &canvas.scroll);
    }
    canvas.scale = scale;
    canvas.layout = result;
    canvas.layout_dirty = false;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    pub(crate) fn document(input: &str) -> Arc<UiDocument> {
        Arc::new(UiDocument::parse("system-test.rveui", input).unwrap())
    }

    pub(crate) const BASIC: &str = r#"
[[assets]]
path = "/ui/icon.png"

[[elements]]
id = "label"
text = { string = "Hi" }

[[elements]]
id = "btn"
button = {}

[[elements]]
id = "field"
input_field = { text = "1", content = "integer", max_length = 3 }

[[elements]]
id = "slider"
slider = { min = 0.0, max = 10.0, value = 2.0, whole_numbers = true }

[[elements]]
id = "drop"
dropdown = { options = ["a", "b"], selected = 0 }

[[elements]]
id = "toggle"
toggle = {}

[[elements]]
id = "image"
image = { path = "/ui/icon.png" }
"#;

    #[test]
    fn stale_handles_are_rejected_after_close_and_slot_reuse() {
        let mut system = UiSystem::default();
        let first = system.open(document(BASIC), 1);
        let label = system.find(first, "label").unwrap();
        system.close(first).unwrap();

        assert_eq!(system.text(label), Err(UiSystemError::StaleHandle));
        assert_eq!(system.close(first), Err(UiSystemError::StaleHandle));

        let second = system.open(document(BASIC), 2);
        assert_eq!(second.index, first.index);
        assert_ne!(second.generation, first.generation);
        assert_eq!(system.text(label), Err(UiSystemError::StaleHandle));
        let fresh = system.find(second, "label").unwrap();
        assert_eq!(system.text(fresh).unwrap(), "Hi");
    }

    #[test]
    fn element_handles_check_index_and_getters_reflect_the_canvas() {
        let mut system = UiSystem::default();
        let canvas = system.open(document(BASIC), 1);
        let label = system.element(canvas, 0).unwrap();
        assert_eq!(system.element_id(label), Ok("label"));
        assert_eq!(
            system.element(canvas, 99),
            Err(UiSystemError::UnknownIndex(99))
        );

        assert_eq!(system.owner(canvas), Ok(1));
        assert_eq!(system.layer(canvas), Ok(0));
        assert_eq!(system.canvas_visible(canvas), Ok(true));
        system.set_layer(canvas, 4).unwrap();
        system.set_canvas_visible(canvas, false).unwrap();
        assert_eq!(system.layer(canvas), Ok(4));
        assert_eq!(system.canvas_visible(canvas), Ok(false));

        let raw = UiCanvasHandle::from_raw(canvas.index(), canvas.generation());
        assert_eq!(raw, canvas);
        system.close(canvas).unwrap();
        assert_eq!(system.layer(raw), Err(UiSystemError::StaleHandle));
    }

    #[test]
    fn close_owned_closes_only_owner_canvases() {
        let mut system = UiSystem::default();
        let mine = system.open(document(BASIC), 7);
        let other = system.open(document(BASIC), 8);
        let second_mine = system.open(document(BASIC), 7);
        system.close_owned(7);
        assert_eq!(
            system.canvas_ref(mine).err(),
            Some(UiSystemError::StaleHandle)
        );
        assert_eq!(
            system.canvas_ref(second_mine).err(),
            Some(UiSystemError::StaleHandle)
        );
        assert!(system.canvas_ref(other).is_ok());
    }

    #[test]
    fn text_value_and_visibility_round_trip_and_reject_wrong_kinds() {
        let mut system = UiSystem::default();
        let canvas = system.open(document(BASIC), 1);
        let label = system.find(canvas, "label").unwrap();
        let field = system.find(canvas, "field").unwrap();
        let slider = system.find(canvas, "slider").unwrap();

        system.set_text(label, "Bye").unwrap();
        assert_eq!(system.text(label).unwrap(), "Bye");
        assert!(matches!(
            system.set_text(slider, "x"),
            Err(UiSystemError::Unsupported { .. })
        ));

        system.set_text(field, "42").unwrap();
        assert!(matches!(
            system.set_text(field, "4x"),
            Err(UiSystemError::InvalidValue(_))
        ));
        assert!(matches!(
            system.set_text(field, "1234"),
            Err(UiSystemError::InvalidValue(_))
        ));

        system.set_visible(label, false).unwrap();
        assert!(!system.visible(label).unwrap());
    }

    #[test]
    fn values_are_clamped_and_checked_by_kind() {
        let mut system = UiSystem::default();
        let canvas = system.open(document(BASIC), 1);
        let slider = system.find(canvas, "slider").unwrap();
        let drop = system.find(canvas, "drop").unwrap();
        let toggle = system.find(canvas, "toggle").unwrap();

        system.set_value(slider, UiValue::Number(7.6)).unwrap();
        assert_eq!(system.value(slider).unwrap(), Some(UiValue::Number(8.0)));
        system.set_value(slider, UiValue::Number(99.0)).unwrap();
        assert_eq!(system.value(slider).unwrap(), Some(UiValue::Number(10.0)));
        assert!(matches!(
            system.set_value(slider, UiValue::Bool(true)),
            Err(UiSystemError::InvalidValue(_))
        ));

        assert!(matches!(
            system.set_value(drop, UiValue::Index(2)),
            Err(UiSystemError::InvalidValue(_))
        ));
        system.set_value(drop, UiValue::Index(1)).unwrap();
        assert_eq!(system.value(drop).unwrap(), Some(UiValue::Index(1)));

        system.set_value(toggle, UiValue::Bool(true)).unwrap();
        assert_eq!(system.value(toggle).unwrap(), Some(UiValue::Bool(true)));
    }

    #[test]
    fn options_keep_selection_only_when_index_remains() {
        let mut system = UiSystem::default();
        let canvas = system.open(document(BASIC), 1);
        let drop = system.find(canvas, "drop").unwrap();
        system.set_value(drop, UiValue::Index(1)).unwrap();
        system
            .set_options(drop, vec!["x".into(), "y".into()])
            .unwrap();
        assert_eq!(system.value(drop).unwrap(), Some(UiValue::Index(1)));
        system.set_options(drop, vec!["only".into()]).unwrap();
        assert_eq!(system.value(drop).unwrap(), None);
        assert_eq!(system.options(drop).unwrap(), vec!["only".to_owned()]);
    }

    #[test]
    fn image_must_be_declared_and_color_round_trips() {
        let mut system = UiSystem::default();
        let canvas = system.open(document(BASIC), 1);
        let image = system.find(canvas, "image").unwrap();
        let label = system.find(canvas, "label").unwrap();

        system.set_image(image, "/ui/icon.png").unwrap();
        assert_eq!(system.image(image).unwrap().as_deref(), Some("ui/icon.png"));
        assert!(matches!(
            system.set_image(image, "/ui/other.png"),
            Err(UiSystemError::InvalidValue(_))
        ));

        system.set_color(label, [1, 2, 3, 4]).unwrap();
        assert_eq!(system.color(label).unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    fn set_rect_validates_and_marks_layout() {
        let mut system = UiSystem::default();
        let canvas = system.open(document(BASIC), 1);
        let btn = system.find(canvas, "btn").unwrap();
        let bad = RectSpec {
            anchor_min: [2.0, 0.0],
            ..RectSpec::default()
        };
        assert!(matches!(
            system.set_rect(btn, bad),
            Err(UiSystemError::InvalidValue(_))
        ));
        system.set_rect(btn, RectSpec::default()).unwrap();
        assert!(system.canvas_ref(canvas).unwrap().layout_dirty);
    }

    #[test]
    fn element_handle_reports_canvas_and_index() {
        let mut system = UiSystem::default();
        let canvas = system.open(document(BASIC), 1);
        let label = system.find(canvas, "label").unwrap();
        assert_eq!(label.canvas(), canvas);
        assert_eq!(label.index(), 0);
        assert!(matches!(
            system.find(canvas, "missing"),
            Err(UiSystemError::UnknownElement(_))
        ));
    }

    #[test]
    fn snapshot_restores_caret_value_and_text_and_restore_focus_is_silent() {
        use crate::input::{TextInputEvent, UiInput};
        let screen = Vec2::new(800.0, 600.0);
        let mut system = UiSystem::default();
        let canvas = system.open(document(BASIC), 1);
        let field = system.find(canvas, "field").unwrap();
        let drop = system.find(canvas, "drop").unwrap();
        system.focus(field).unwrap();
        let type_text = |system: &mut UiSystem, text: TextInputEvent| {
            system.update(
                &UiInput {
                    text: vec![text],
                    ..UiInput::default()
                },
                0.0,
                screen,
            );
        };
        type_text(&mut system, TextInputEvent::Home);
        let snapshot = system.snapshot_canvas(canvas).unwrap();

        system.set_value(drop, UiValue::Index(1)).unwrap();
        type_text(&mut system, TextInputEvent::Text("7".to_owned()));
        assert_eq!(system.text(field), Ok("71".to_owned()));

        system.restore_canvas(canvas, snapshot).unwrap();
        assert_eq!(system.text(field), Ok("1".to_owned()));
        assert_eq!(system.value(drop), Ok(Some(UiValue::Index(0))));
        type_text(&mut system, TextInputEvent::Text("7".to_owned()));
        assert_eq!(
            system.text(field),
            Ok("71".to_owned()),
            "каретка возвращается в позицию снимка"
        );

        system.restore_focus(None).unwrap();
        assert_eq!(system.focused(), None);
        system.close(canvas).unwrap();
        assert!(system.restore_focus(Some(field)).is_err());
        assert!(system.snapshot_canvas(canvas).is_err());
    }
}
