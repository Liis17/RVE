//! Ввод UI: указатель, колесо, навигация по фокусу, текст InputField и повтор направления.
//! Вызывающая сторона передаёт сырой ввод в [`UiSystem::update`] и получает события и заявки
//! на поглощение ([`UiClaims`]); скрытие ввода от скриптов делает [`crate::capture`].

use crate::document::{Axis, ElementKind, accepts_text};
use crate::events::{UiEvent, UiEventKind, UiValue};
use crate::system::{
    UiCanvas, UiCanvasHandle, UiElementHandle, UiSystem, UiSystemError, clamp_slider, event_for,
    scroll_limit,
};
use epaint::{Pos2, Rect, Vec2, pos2, vec2};

/// Задержка перед первым повтором удержанного направления и интервал между повторами, секунды.
const REPEAT_DELAY: f64 = 0.4;
const REPEAT_INTERVAL: f64 = 0.1;
/// Шаг слайдера с клавиатуры: доля диапазона, если значения не целые.
const SLIDER_KEY_STEPS: f32 = 20.0;

/// Навигационное действие, уже переведённое вызывающей стороной из клавиш и кнопок геймпада.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiAction {
    Up,
    Down,
    Left,
    Right,
    /// Enter или кнопка A: нажимает кнопку, переключает флаг, открывает список.
    Confirm,
    /// Esc или кнопка B: закрывает список или шлёт `cancel`.
    Cancel,
    /// Tab: следующий элемент в порядке отрисовки.
    Next,
    /// Shift+Tab: предыдущий элемент.
    Previous,
}

impl UiAction {
    fn direction(self) -> Option<Vec2> {
        match self {
            Self::Up => Some(vec2(0.0, -1.0)),
            Self::Down => Some(vec2(0.0, 1.0)),
            Self::Left => Some(vec2(-1.0, 0.0)),
            Self::Right => Some(vec2(1.0, 0.0)),
            _ => None,
        }
    }
}

/// Текстовый ввод для InputField. `Text` — уже отфильтрованные печатаемые символы из IME или
/// `KeyEvent.text`; остальные — правка и подтверждение.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextInputEvent {
    Text(String),
    Backspace,
    Delete,
    Left,
    Right,
    Home,
    End,
    /// Подтверждает InputField (`submit`); для других элементов не используется.
    Enter,
}

/// Сырой ввод одного кадра. Координаты указателя — физические пиксели окна.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiInput {
    /// `None`, если курсор вне окна.
    pub pointer: Option<Vec2>,
    pub primary_pressed: bool,
    pub primary_down: bool,
    pub primary_released: bool,
    /// Колесо в строках: положительный `y` — прокрутка вверх.
    pub wheel: Vec2,
    /// Навигация, нажатая в этом кадре.
    pub actions: Vec<UiAction>,
    /// Направление, которое сейчас удержано (для повтора); остальные действия игнорируются.
    pub held: Option<UiAction>,
    pub text: Vec<TextInputEvent>,
}

/// Что UI поглотил в кадре. Вызывающая сторона переводит это в идентификаторы своих кнопок и
/// клавиш и передаёт в [`crate::capture::InputCapture`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiClaims {
    /// Основная кнопка нажата над элементом UI или пунктом открытого списка.
    pub pointer_primary: bool,
    /// Навигационные действия, которые обработал фокус или список.
    pub actions: Vec<UiAction>,
    /// Фокус на InputField: новые нажатия клавиатуры скрыты от скриптов.
    pub text_focused: bool,
    /// Колесо обработано над ScrollView или списком.
    pub wheel: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiUpdate {
    pub events: Vec<UiEvent>,
    pub claims: UiClaims,
}

/// Часть элемента под указателем: тело, пункт открытого списка или пустая область списка.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    Body,
    Item(usize),
    Popup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hit {
    pub(crate) element: UiElementHandle,
    pub(crate) part: Part,
}

/// Состояние повтора удержанного направления: следующий повтор в секундах внешнего времени.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Repeat {
    pub(crate) action: UiAction,
    pub(crate) next: f64,
}

impl UiSystem {
    /// Обрабатывает кадр: обновляет layout при необходимости, наведение, нажатия, колесо,
    /// навигацию и текст. `now` — время в секундах; повтор считается от него, без таймеров.
    /// `screen_px` — размер окна в физических пикселях.
    pub fn update(&mut self, input: &UiInput, now: f64, screen_px: Vec2) -> UiUpdate {
        let screen_changed = screen_px != self.screen;
        self.screen = screen_px;
        self.refresh_layouts(screen_changed);
        if screen_changed {
            self.paint_dirty = true;
        }

        let mut events = Vec::new();
        self.apply_default_focus(&mut events);
        self.sanitize(&mut events);

        let hit = input.pointer.and_then(|pointer| self.hit_test(pointer));
        self.pointer_over = hit.is_some();
        self.set_hover(hit.map(|hit| hit.element), &mut events);

        let mut claims = UiClaims::default();
        let pressed_hit = hit.zip(input.pointer);
        if input.primary_pressed {
            self.close_popups_except(pressed_hit.map(|(hit, _)| hit.element));
            if let Some((hit, pointer)) = pressed_hit {
                claims.pointer_primary = true;
                self.begin_press(hit, pointer, &mut events);
            }
        }
        if input.primary_down
            && let Some(pointer) = input.pointer
            && self.press.is_some()
        {
            self.drag(pointer, &mut events);
        }
        if input.primary_released
            && let Some(press) = self.press.take()
            && hit == Some(press)
        {
            self.activate_hit(press, &mut events);
        }

        if input.wheel != Vec2::ZERO
            && let Some(pointer) = input.pointer
        {
            match hit {
                Some(Hit {
                    part: Part::Popup | Part::Item(_),
                    ..
                }) => claims.wheel = true,
                _ => {
                    if let Some(view) = self.scroll_view_under(pointer) {
                        self.scroll_wheel(view, input.wheel);
                        claims.wheel = true;
                    }
                }
            }
        }

        self.tick_repeat(input.held, now, &mut events);
        for &action in &input.actions {
            if self.handle_action(action, &mut events) {
                claims.actions.push(action);
            }
        }
        for text in &input.text {
            self.edit_text(text, &mut events);
        }

        self.refresh_layouts(false);
        self.sanitize(&mut events);
        claims.text_focused = self.focused_is_input_field();
        // Тесселяция нужна только при изменении видимого состояния: события, ввод или повтор.
        let touched = !events.is_empty()
            || input.primary_pressed
            || input.primary_down
            || input.primary_released
            || input.wheel != Vec2::ZERO
            || !input.actions.is_empty()
            || !input.text.is_empty()
            || input.held.is_some();
        if touched {
            self.paint_dirty = true;
        }
        UiUpdate { events, claims }
    }

    /// Переводит фокус на элемент, если он может его получить: кнопка, переключатель, слайдер,
    /// поле ввода или список. Возвращает `blur`/`focus` для вызывающей стороны.
    pub fn focus(&mut self, handle: UiElementHandle) -> Result<Vec<UiEvent>, UiSystemError> {
        let (canvas, index) = self.element_ref(handle)?;
        if !is_focusable(&canvas.elements[index].kind) {
            return Err(UiSystemError::Unsupported {
                element: canvas.elements[index].id.clone(),
                property: "focus",
            });
        }
        let mut events = Vec::new();
        self.set_focus(Some(handle), &mut events);
        Ok(events)
    }

    /// Снимает фокус; возвращает `blur` для элемента, который его терял.
    pub fn clear_focus(&mut self) -> Vec<UiEvent> {
        let mut events = Vec::new();
        self.set_focus(None, &mut events);
        events
    }

    /// Закрывает открытые списки, кроме списка элемента `keep`.
    pub(crate) fn close_popups_except(&mut self, keep: Option<UiElementHandle>) {
        for (slot_index, slot) in self.slots.iter_mut().enumerate() {
            let Some(canvas) = slot.canvas.as_mut() else {
                continue;
            };
            let Some(open) = canvas.popup else {
                continue;
            };
            let keep_here = keep.is_some_and(|handle| {
                handle.canvas.index as usize == slot_index && handle.index() == open
            });
            if !keep_here {
                canvas.popup = None;
            }
        }
    }

    fn set_hover(&mut self, next: Option<UiElementHandle>, events: &mut Vec<UiEvent>) {
        if self.hover == next {
            return;
        }
        if let Some(old) = self.hover.take() {
            events.push(event_for(old, UiEventKind::HoverExit, None));
        }
        self.hover = next;
        if let Some(new) = next {
            events.push(event_for(new, UiEventKind::HoverEnter, None));
        }
    }

    fn set_focus(&mut self, next: Option<UiElementHandle>, events: &mut Vec<UiEvent>) {
        if self.focus == next {
            return;
        }
        self.paint_dirty = true;
        if let Some(old) = self.focus.take() {
            if let Ok(canvas) = self.canvas_mut(old.canvas)
                && canvas.popup == Some(old.index())
            {
                canvas.popup = None;
            }
            events.push(event_for(old, UiEventKind::Blur, None));
        }
        self.focus = next;
        if let Some(new) = next {
            events.push(event_for(new, UiEventKind::Focus, None));
        }
    }

    /// Фокус на канвасе, который открылся с `default_focus`, применяется при первом update.
    fn apply_default_focus(&mut self, events: &mut Vec<UiEvent>) {
        for canvas_handle in self.canvases_top_down() {
            let target = {
                if !self
                    .canvas_ref(canvas_handle)
                    .is_ok_and(|canvas| canvas.default_focus_pending)
                {
                    continue;
                }
                let Ok(canvas) = self.canvas_mut(canvas_handle) else {
                    continue;
                };
                canvas.default_focus_pending = false;
                canvas
                    .document
                    .canvas
                    .default_focus
                    .clone()
                    .and_then(|id| canvas.document.element_index(&id))
                    .map(|index| UiElementHandle {
                        canvas: canvas_handle,
                        index: index as u32,
                    })
            };
            if let Some(target) = target
                && self.is_focus_target(target)
            {
                self.set_focus(Some(target), events);
            }
        }
    }

    /// Сбрасывает наведение, фокус и нажатие, если элемент больше не виден или не интерактивен.
    fn sanitize(&mut self, events: &mut Vec<UiEvent>) {
        if let Some(hover) = self.hover
            && !self.is_live(hover)
        {
            self.hover = None;
            events.push(event_for(hover, UiEventKind::HoverExit, None));
        }
        if let Some(focus) = self.focus
            && !self.is_live(focus)
        {
            self.focus = None;
            events.push(event_for(focus, UiEventKind::Blur, None));
        }
        if let Some(press) = self.press
            && !self.is_live(press.element)
        {
            self.press = None;
        }
        for slot in &mut self.slots {
            let Some(canvas) = slot.canvas.as_mut() else {
                continue;
            };
            if let Some(open) = canvas.popup
                && !(canvas.layout.visible.get(open) == Some(&true)
                    && canvas.layout.interactive.get(open) == Some(&true))
            {
                canvas.popup = None;
            }
        }
    }

    fn is_live(&self, handle: UiElementHandle) -> bool {
        self.element_ref(handle).is_ok_and(|(canvas, index)| {
            canvas.visible
                && canvas.layout.visible.get(index) == Some(&true)
                && canvas.layout.interactive.get(index) == Some(&true)
        })
    }

    fn is_focus_target(&self, handle: UiElementHandle) -> bool {
        self.element_ref(handle)
            .is_ok_and(|(canvas, index)| focus_ready(canvas, index))
    }

    fn focused_is_input_field(&self) -> bool {
        self.focus.is_some_and(|focus| {
            self.element_ref(focus).is_ok_and(|(canvas, index)| {
                matches!(canvas.elements[index].kind, ElementKind::InputField(_))
            })
        })
    }

    /// Верхний элемент под указателем: сначала открытый список канваса, потом элементы в обратном
    /// порядке отрисовки. Невидимые и неинтерактивные элементы не перехватывают указатель.
    pub(crate) fn hit_test(&self, pointer: Vec2) -> Option<Hit> {
        for canvas_handle in self.canvases_top_down() {
            let Ok(canvas) = self.canvas_ref(canvas_handle) else {
                continue;
            };
            if !canvas.visible {
                continue;
            }
            let position = to_pos(pointer / canvas.scale);
            if let Some(open) = canvas.popup
                && canvas.layout.visible.get(open) == Some(&true)
            {
                let popup = popup_rect(canvas, open);
                if popup.contains(position) {
                    let item_height = canvas.layout.rects[open].height();
                    let count = dropdown_len(canvas, open);
                    let part = if item_height > 0.0 {
                        let item = ((position.y - popup.min.y) / item_height) as usize;
                        if item < count {
                            Part::Item(item)
                        } else {
                            Part::Popup
                        }
                    } else {
                        Part::Popup
                    };
                    return Some(Hit {
                        element: handle_of(canvas_handle, open),
                        part,
                    });
                }
            }
            for &index in canvas.layout.order.iter().rev() {
                if canvas.layout.interactive[index]
                    && canvas.elements[index].kind.is_interactive()
                    && canvas.layout.rects[index].contains(position)
                    && canvas.layout.clips[index].contains(position)
                {
                    return Some(Hit {
                        element: handle_of(canvas_handle, index),
                        part: Part::Body,
                    });
                }
            }
        }
        None
    }

    /// Верхний ScrollView под указателем; колесо над ним прокручивает содержимое.
    fn scroll_view_under(&self, pointer: Vec2) -> Option<UiElementHandle> {
        for canvas_handle in self.canvases_top_down() {
            let Ok(canvas) = self.canvas_ref(canvas_handle) else {
                continue;
            };
            if !canvas.visible {
                continue;
            }
            let position = to_pos(pointer / canvas.scale);
            for &index in canvas.layout.order.iter().rev() {
                if canvas.layout.interactive[index]
                    && matches!(canvas.elements[index].kind, ElementKind::ScrollView(_))
                    && canvas.layout.rects[index].contains(position)
                    && canvas.layout.clips[index].contains(position)
                {
                    return Some(handle_of(canvas_handle, index));
                }
            }
        }
        None
    }

    fn begin_press(&mut self, hit: Hit, pointer: Vec2, events: &mut Vec<UiEvent>) {
        if hit.part == Part::Popup {
            return;
        }
        self.press = Some(hit);
        self.drag_last = pointer;
        if self.kind_of(hit.element).is_some_and(is_focusable) {
            self.set_focus(Some(hit.element), events);
        }
        match self.kind_of(hit.element) {
            Some(ElementKind::Slider(_)) => {
                self.set_slider_from_pointer(hit.element, pointer, events)
            }
            Some(ElementKind::InputField(_)) => {
                if let Ok((canvas, index)) = self.element_mut(hit.element) {
                    let length = match &canvas.elements[index].kind {
                        ElementKind::InputField(spec) => spec.text.chars().count(),
                        _ => 0,
                    };
                    canvas.cursors[index] = length;
                }
            }
            _ => {}
        }
    }

    /// Перетаскивание: слайдер следует за указателем, ScrollView прокручивается на его смещение.
    fn drag(&mut self, pointer: Vec2, events: &mut Vec<UiEvent>) {
        let Some(press) = self.press else {
            return;
        };
        let delta = pointer - self.drag_last;
        self.drag_last = pointer;
        match self.kind_of(press.element) {
            Some(ElementKind::Slider(_)) if press.part == Part::Body => {
                self.set_slider_from_pointer(press.element, pointer, events);
            }
            Some(ElementKind::ScrollView(_)) => {
                let Ok((canvas, _)) = self.element_ref(press.element) else {
                    return;
                };
                let units = -delta / canvas.scale;
                self.scroll_by_units(press.element, units);
            }
            _ => {}
        }
    }

    /// Отпускание над тем же элементом и той же частью — нажатие.
    fn activate_hit(&mut self, hit: Hit, events: &mut Vec<UiEvent>) {
        match hit.part {
            Part::Body => self.activate_element(hit.element, events),
            Part::Item(item) => self.choose_item(hit.element, item, events),
            Part::Popup => {}
        }
    }

    /// Нажатие кнопки, переключение флага, открытие и закрытие списка.
    fn activate_element(&mut self, handle: UiElementHandle, events: &mut Vec<UiEvent>) {
        let Ok((canvas, index)) = self.element_mut(handle) else {
            return;
        };
        match &mut canvas.elements[index].kind {
            ElementKind::Button(_) => events.push(event_for(handle, UiEventKind::Click, None)),
            ElementKind::Toggle(spec) => {
                spec.is_on = !spec.is_on;
                let on = spec.is_on;
                events.push(event_for(handle, UiEventKind::Click, None));
                events.push(event_for(
                    handle,
                    UiEventKind::ValueChanged,
                    Some(UiValue::Bool(on)),
                ));
            }
            ElementKind::Dropdown(spec) => {
                if canvas.popup == Some(index) {
                    canvas.popup = None;
                } else {
                    canvas.popup = Some(index);
                    canvas.cursors[index] = spec.selected.unwrap_or(0);
                }
            }
            _ => {}
        }
    }

    fn choose_item(&mut self, handle: UiElementHandle, item: usize, events: &mut Vec<UiEvent>) {
        let Ok((canvas, index)) = self.element_mut(handle) else {
            return;
        };
        canvas.popup = None;
        if let ElementKind::Dropdown(spec) = &mut canvas.elements[index].kind
            && item < spec.options.len()
            && spec.selected != Some(item)
        {
            spec.selected = Some(item);
            events.push(event_for(
                handle,
                UiEventKind::ValueChanged,
                Some(UiValue::Index(item)),
            ));
        }
    }

    /// Значение по положению указателя на дорожке; вертикальный слайдер растёт снизу вверх.
    fn set_slider_from_pointer(
        &mut self,
        handle: UiElementHandle,
        pointer: Vec2,
        events: &mut Vec<UiEvent>,
    ) {
        let Ok((canvas, index)) = self.element_ref(handle) else {
            return;
        };
        let position = to_pos(pointer / canvas.scale);
        let rect = canvas.layout.rects[index];
        let ElementKind::Slider(spec) = &canvas.elements[index].kind else {
            return;
        };
        let fraction = match spec.direction {
            Axis::Horizontal if rect.width() > 0.0 => (position.x - rect.min.x) / rect.width(),
            Axis::Vertical if rect.height() > 0.0 => (rect.max.y - position.y) / rect.height(),
            _ => 0.0,
        };
        let value = clamp_slider(
            spec,
            spec.min + fraction.clamp(0.0, 1.0) * (spec.max - spec.min),
        );
        let current = spec.value;
        if value != current {
            self.set_slider_value(handle, value, events);
        }
    }

    fn set_slider_value(&mut self, handle: UiElementHandle, value: f32, events: &mut Vec<UiEvent>) {
        if let Ok((canvas, index)) = self.element_mut(handle)
            && let ElementKind::Slider(spec) = &mut canvas.elements[index].kind
        {
            spec.value = value;
            events.push(event_for(
                handle,
                UiEventKind::ValueChanged,
                Some(UiValue::Number(value)),
            ));
        }
    }

    fn adjust_slider(&mut self, handle: UiElementHandle, sign: f32, events: &mut Vec<UiEvent>) {
        let (value, current) = {
            let Ok((canvas, index)) = self.element_ref(handle) else {
                return;
            };
            let ElementKind::Slider(spec) = &canvas.elements[index].kind else {
                return;
            };
            let step = if spec.whole_numbers {
                1.0
            } else {
                (spec.max - spec.min) / SLIDER_KEY_STEPS
            };
            (clamp_slider(spec, spec.value + sign * step), spec.value)
        };
        if value != current {
            self.set_slider_value(handle, value, events);
        }
    }

    /// Смещает содержимое ScrollView на `units` единиц канваса, с учётом пределов прокрутки.
    fn scroll_by_units(&mut self, handle: UiElementHandle, units: Vec2) {
        let Ok((canvas, index)) = self.element_mut(handle) else {
            return;
        };
        let limit = scroll_limit(
            &canvas.layout,
            &canvas.elements,
            canvas.document.tree(),
            index,
        );
        let current = canvas.scroll[index];
        canvas.scroll[index] = Vec2::new(
            (current.x + units.x).clamp(0.0, limit.x),
            (current.y + units.y).clamp(0.0, limit.y),
        );
        canvas.layout_dirty = true;
    }

    /// Колесо в строках: вверх (положительный `y`) — содержимое опускается.
    fn scroll_wheel(&mut self, handle: UiElementHandle, wheel: Vec2) {
        let Some(ElementKind::ScrollView(spec)) = self.kind_of(handle) else {
            return;
        };
        let units = -wheel * spec.sensitivity;
        self.scroll_by_units(handle, units);
    }

    /// Навигационное действие: фокус, список, слайдер или текстовое поле.
    /// Возвращает `true`, если действие поглощено и не должно уйти скриптам.
    fn handle_action(&mut self, action: UiAction, events: &mut Vec<UiEvent>) -> bool {
        let Some(focus) = self.focus else {
            return self.start_focus_cycle(action, events);
        };
        if self.popup_open(focus) {
            self.popup_action(focus, action, events);
            return true;
        }
        match action {
            UiAction::Next | UiAction::Previous => {
                self.cycle_focus(focus, action == UiAction::Next, events);
                true
            }
            UiAction::Cancel => {
                events.push(event_for(focus, UiEventKind::Cancel, None));
                true
            }
            UiAction::Confirm => {
                self.activate_element(focus, events);
                true
            }
            UiAction::Left | UiAction::Right | UiAction::Up | UiAction::Down => {
                self.directional_action(focus, action, events);
                true
            }
        }
    }

    fn directional_action(
        &mut self,
        focus: UiElementHandle,
        action: UiAction,
        events: &mut Vec<UiEvent>,
    ) {
        // Курсор поля ввода двигают Left/Right через TextInputEvent; навигация отключена.
        if matches!(self.kind_of(focus), Some(ElementKind::InputField(_)))
            && matches!(action, UiAction::Left | UiAction::Right)
        {
            return;
        }
        let slider_axis = match self.kind_of(focus) {
            Some(ElementKind::Slider(spec)) => Some(spec.direction),
            _ => None,
        };
        if let Some(axis) = slider_axis
            && slider_takes(axis, action)
        {
            let sign = if matches!(action, UiAction::Right | UiAction::Up) {
                1.0
            } else {
                -1.0
            };
            self.adjust_slider(focus, sign, events);
            return;
        }
        if let Some(target) = self.neighbor(focus, action) {
            self.set_focus(Some(target), events);
        }
    }

    /// Без фокуса Tab выбирает первый элемент на самом верхнем канвасе, где есть цели фокуса.
    fn start_focus_cycle(&mut self, action: UiAction, events: &mut Vec<UiEvent>) -> bool {
        if !matches!(action, UiAction::Next | UiAction::Previous) {
            return false;
        }
        for canvas_handle in self.canvases_top_down() {
            let targets = self.focus_targets(canvas_handle);
            if let Some(&first) = targets.first() {
                let forward = action == UiAction::Next;
                let next = if forward {
                    first
                } else {
                    *targets.last().expect("список не пуст")
                };
                self.set_focus(Some(handle_of(canvas_handle, next)), events);
                return true;
            }
        }
        false
    }

    fn cycle_focus(&mut self, focus: UiElementHandle, forward: bool, events: &mut Vec<UiEvent>) {
        let targets = self.focus_targets(focus.canvas);
        if targets.is_empty() {
            return;
        }
        let position = targets.iter().position(|&index| index == focus.index());
        let next = match (position, forward) {
            (Some(position), true) => targets[(position + 1) % targets.len()],
            (Some(position), false) => targets[(position + targets.len() - 1) % targets.len()],
            (None, true) => targets[0],
            (None, false) => targets[targets.len() - 1],
        };
        self.set_focus(Some(handle_of(focus.canvas, next)), events);
    }

    /// Цели фокуса канваса в порядке отрисовки.
    fn focus_targets(&self, canvas_handle: UiCanvasHandle) -> Vec<usize> {
        let Ok(canvas) = self.canvas_ref(canvas_handle) else {
            return Vec::new();
        };
        canvas
            .layout
            .order
            .iter()
            .copied()
            .filter(|&index| focus_ready(canvas, index))
            .collect()
    }

    /// Ближайший элемент в направлении. Явный `nav` главнее: если он задан и цель годится, берётся он.
    fn neighbor(&self, from: UiElementHandle, action: UiAction) -> Option<UiElementHandle> {
        let direction = action.direction()?;
        let (canvas, index) = self.element_ref(from).ok()?;
        let nav = &canvas.elements[index].nav;
        let explicit = match action {
            UiAction::Up => nav.up.as_ref(),
            UiAction::Down => nav.down.as_ref(),
            UiAction::Left => nav.left.as_ref(),
            _ => nav.right.as_ref(),
        };
        if let Some(target) = explicit.and_then(|id| canvas.document.element_index(id))
            && self.is_focus_target(handle_of(from.canvas, target))
        {
            return Some(handle_of(from.canvas, target));
        }
        let origin = canvas.layout.rects[index].center();
        canvas
            .layout
            .order
            .iter()
            .copied()
            .filter(|&other| other != index && self.is_focus_target(handle_of(from.canvas, other)))
            .filter_map(|other| {
                let delta = canvas.layout.rects[other].center() - origin;
                let along = delta.dot(direction);
                let across = (delta.x * direction.y - delta.y * direction.x).abs();
                (along > 0.0).then_some((other, along + 2.0 * across))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(other, _)| handle_of(from.canvas, other))
    }

    fn popup_open(&self, handle: UiElementHandle) -> bool {
        self.canvas_ref(handle.canvas)
            .is_ok_and(|canvas| canvas.popup == Some(handle.index()))
    }

    /// Открытый список забирает все навигационные действия.
    fn popup_action(
        &mut self,
        handle: UiElementHandle,
        action: UiAction,
        events: &mut Vec<UiEvent>,
    ) {
        let Ok((canvas, index)) = self.element_mut(handle) else {
            return;
        };
        let count = dropdown_len(canvas, index);
        match action {
            UiAction::Up => canvas.cursors[index] = canvas.cursors[index].saturating_sub(1),
            UiAction::Down => {
                if count > 0 {
                    canvas.cursors[index] = (canvas.cursors[index] + 1).min(count - 1);
                }
            }
            UiAction::Confirm => {
                let item = canvas.cursors[index];
                self.choose_item(handle, item, events);
            }
            UiAction::Cancel => canvas.popup = None,
            UiAction::Left | UiAction::Right | UiAction::Next | UiAction::Previous => {}
        }
    }

    /// Повтор удержанного направления: первый повтор через [`REPEAT_DELAY`], затем каждые [`REPEAT_INTERVAL`].
    fn tick_repeat(&mut self, held: Option<UiAction>, now: f64, events: &mut Vec<UiEvent>) {
        match held.filter(|action| action.direction().is_some()) {
            None => self.repeat = None,
            Some(action) => {
                if self.repeat.is_none_or(|repeat| repeat.action != action) {
                    self.repeat = Some(Repeat {
                        action,
                        next: now + REPEAT_DELAY,
                    });
                }
            }
        }
        if let Some(repeat) = self.repeat
            && now >= repeat.next
        {
            self.repeat = Some(Repeat {
                next: now + REPEAT_INTERVAL,
                ..repeat
            });
            self.handle_action(repeat.action, events);
        }
    }

    /// Правка InputField: символы фильтруются по content и max_length, курсор считается в символах.
    fn edit_text(&mut self, event: &TextInputEvent, events: &mut Vec<UiEvent>) {
        let Some(focus) = self.focus else {
            return;
        };
        let Ok((canvas, index)) = self.element_mut(focus) else {
            return;
        };
        let ElementKind::InputField(spec) = &mut canvas.elements[index].kind else {
            return;
        };
        let cursor = &mut canvas.cursors[index];
        let mut changed = false;
        match event {
            TextInputEvent::Text(input) => {
                for character in input.chars().filter(|c| !c.is_control()) {
                    if spec
                        .max_length
                        .is_some_and(|max| spec.text.chars().count() >= max)
                    {
                        break;
                    }
                    let mut candidate = spec.text.clone();
                    candidate.insert(byte_index(&candidate, *cursor), character);
                    if accepts_text(spec.content, &candidate) {
                        spec.text = candidate;
                        *cursor += 1;
                        changed = true;
                    }
                }
            }
            TextInputEvent::Backspace => {
                if *cursor > 0 {
                    *cursor -= 1;
                    let at = byte_index(&spec.text, *cursor);
                    spec.text.remove(at);
                    changed = true;
                }
            }
            TextInputEvent::Delete => {
                if *cursor < spec.text.chars().count() {
                    let at = byte_index(&spec.text, *cursor);
                    spec.text.remove(at);
                    changed = true;
                }
            }
            TextInputEvent::Left => *cursor = cursor.saturating_sub(1),
            TextInputEvent::Right => *cursor = (*cursor + 1).min(spec.text.chars().count()),
            TextInputEvent::Home => *cursor = 0,
            TextInputEvent::End => *cursor = spec.text.chars().count(),
            TextInputEvent::Enter => {
                let value = UiValue::Text(spec.text.clone());
                events.push(event_for(focus, UiEventKind::Submit, Some(value)));
            }
        }
        if changed {
            let value = UiValue::Text(spec.text.clone());
            events.push(event_for(focus, UiEventKind::ValueChanged, Some(value)));
        }
    }

    fn kind_of(&self, handle: UiElementHandle) -> Option<&ElementKind> {
        self.element_ref(handle)
            .ok()
            .map(|(canvas, index)| &canvas.elements[index].kind)
    }
}

/// Элемент может получить фокус сейчас: видим, интерактивен, фокусируемого вида и не обрезан прокруткой.
fn focus_ready(canvas: &UiCanvas, index: usize) -> bool {
    let visible_part = canvas.layout.clips[index].intersect(canvas.layout.rects[index]);
    canvas.visible
        && canvas.layout.visible[index]
        && canvas.layout.interactive[index]
        && is_focusable(&canvas.elements[index].kind)
        && visible_part.area() > 0.0
}

fn is_focusable(kind: &ElementKind) -> bool {
    matches!(
        kind,
        ElementKind::Button(_)
            | ElementKind::Toggle(_)
            | ElementKind::Slider(_)
            | ElementKind::InputField(_)
            | ElementKind::Dropdown(_)
    )
}

fn slider_takes(direction: Axis, action: UiAction) -> bool {
    match direction {
        Axis::Horizontal => matches!(action, UiAction::Left | UiAction::Right),
        Axis::Vertical => matches!(action, UiAction::Up | UiAction::Down),
    }
}

fn handle_of(canvas: UiCanvasHandle, index: usize) -> UiElementHandle {
    UiElementHandle {
        canvas,
        index: index as u32,
    }
}

fn to_pos(point: Vec2) -> Pos2 {
    pos2(point.x, point.y)
}

/// Прямоугольник списка: под элементом, шириной с элемент, высотой `popup_height`.
fn popup_rect(canvas: &UiCanvas, index: usize) -> Rect {
    let rect = canvas.layout.rects[index];
    let height = match &canvas.elements[index].kind {
        ElementKind::Dropdown(spec) => spec.popup_height,
        _ => 0.0,
    };
    Rect::from_min_size(pos2(rect.min.x, rect.max.y), vec2(rect.width(), height))
}

fn dropdown_len(canvas: &UiCanvas, index: usize) -> usize {
    match &canvas.elements[index].kind {
        ElementKind::Dropdown(spec) => spec.options.len(),
        _ => 0,
    }
}

fn byte_index(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::UiDocument;
    use crate::events::UiEventKind as Kind;
    use crate::system::{UiCanvasHandle, UiSystem, UiSystemError};
    use std::sync::Arc;

    fn screen() -> Vec2 {
        vec2(1000.0, 1000.0)
    }

    /// Фрагмент rect с точечными якорями: позиция (x, y) и размер (w, h) в единицах канваса.
    fn at(x: f32, y: f32, w: f32, h: f32) -> String {
        format!(
            "rect = {{ anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_min = [{x}, {y}], offset_max = [{w}, {h}] }}"
        )
    }

    fn open(input: &str, owner: u64) -> (UiSystem, UiCanvasHandle) {
        let mut system = UiSystem::default();
        let document = Arc::new(UiDocument::parse("input-test.rveui", input).unwrap());
        let canvas = system.open(document, owner);
        (system, canvas)
    }

    fn step(system: &mut UiSystem, input: UiInput, now: f64) -> UiUpdate {
        system.update(&input, now, screen())
    }

    fn at_pointer(x: f32, y: f32) -> UiInput {
        UiInput {
            pointer: Some(vec2(x, y)),
            ..UiInput::default()
        }
    }

    fn press(x: f32, y: f32) -> UiInput {
        UiInput {
            primary_pressed: true,
            primary_down: true,
            ..at_pointer(x, y)
        }
    }

    fn release(x: f32, y: f32) -> UiInput {
        UiInput {
            primary_released: true,
            ..at_pointer(x, y)
        }
    }

    fn has(update: &UiUpdate, kind: Kind, element: UiElementHandle) -> bool {
        update
            .events
            .iter()
            .any(|event| event.kind == kind && event.element == element)
    }

    fn value_events(update: &UiUpdate) -> Vec<UiValue> {
        update
            .events
            .iter()
            .filter(|event| event.kind == Kind::ValueChanged)
            .filter_map(|event| event.value.clone())
            .collect()
    }

    const ROW: &str = r#"
[[elements]]
id = "a"
button = {}
RECT_A

[[elements]]
id = "b"
button = {}
RECT_B

[[elements]]
id = "c"
button = {}
RECT_C
"#;

    fn row() -> String {
        ROW.replace("RECT_A", &at(10.0, 10.0, 100.0, 40.0))
            .replace("RECT_B", &at(200.0, 10.0, 100.0, 40.0))
            .replace("RECT_C", &at(390.0, 10.0, 100.0, 40.0))
    }

    #[test]
    fn click_needs_press_and_release_over_same_element() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        let b = system.find(canvas, "b").unwrap();

        let entered = step(&mut system, at_pointer(50.0, 30.0), 0.0);
        assert!(has(&entered, Kind::HoverEnter, a));
        assert!(!entered.claims.pointer_primary);

        let pressed = step(&mut system, press(50.0, 30.0), 0.0);
        assert!(pressed.claims.pointer_primary, "нажатие над UI поглощено");
        assert!(has(&pressed, Kind::Focus, a));

        let moved = step(&mut system, release(250.0, 30.0), 0.0);
        assert!(
            !has(&moved, Kind::Click, a) && !has(&moved, Kind::Click, b),
            "отпускание в другом месте не даёт click"
        );

        step(&mut system, press(50.0, 30.0), 0.0);
        let clicked = step(&mut system, release(50.0, 30.0), 0.0);
        assert!(has(&clicked, Kind::Click, a));
    }

    fn button_document(id: &str, layer: i32) -> Arc<UiDocument> {
        let input = format!(
            "[canvas]\nlayer = {layer}\n[[elements]]\nid = \"{id}\"\nbutton = {{}}\n{}",
            at(10.0, 10.0, 100.0, 40.0)
        );
        Arc::new(UiDocument::parse("layer-test.rveui", &input).unwrap())
    }

    #[test]
    fn higher_layer_canvas_receives_pointer_first() {
        let mut system = UiSystem::default();
        let back = system.open(button_document("under", 0), 1);
        let front = system.open(button_document("over", 5), 2);
        let under = system.find(back, "under").unwrap();
        let over = system.find(front, "over").unwrap();

        let hovered = step(&mut system, at_pointer(50.0, 30.0), 0.0);
        assert!(has(&hovered, Kind::HoverEnter, over));
        assert!(!has(&hovered, Kind::HoverEnter, under));
    }

    #[test]
    fn equal_layers_put_later_opened_canvas_on_top() {
        let mut system = UiSystem::default();
        let _first = system.open(button_document("first", 0), 1);
        let second = system.open(button_document("second", 0), 2);
        let second_button = system.find(second, "second").unwrap();

        let hovered = step(&mut system, at_pointer(50.0, 30.0), 0.0);
        assert!(has(&hovered, Kind::HoverEnter, second_button));
    }

    #[test]
    fn non_interactive_elements_do_not_block_pointer() {
        let (mut system, canvas) = open(
            &format!(
                "[[elements]]\nid = \"button\"\nbutton = {{}}\n{}\n[[elements]]\nid = \"decor\"\npanel = {{}}\ninteractable = false\n{}",
                at(10.0, 10.0, 100.0, 40.0),
                at(0.0, 0.0, 300.0, 300.0)
            ),
            1,
        );
        let button = system.find(canvas, "button").unwrap();
        let update = step(&mut system, at_pointer(50.0, 30.0), 0.0);
        assert!(has(&update, Kind::HoverEnter, button));
    }

    #[test]
    fn slider_follows_press_and_drag() {
        let (mut system, canvas) = open(
            &format!(
                "[[elements]]\nid = \"slider\"\nslider = {{ min = 0.0, max = 100.0, value = 0.0 }}\n{}",
                at(10.0, 100.0, 200.0, 20.0)
            ),
            1,
        );
        let slider = system.find(canvas, "slider").unwrap();

        let pressed = step(&mut system, press(110.0, 110.0), 0.0);
        assert_eq!(value_events(&pressed), vec![UiValue::Number(50.0)]);
        assert_eq!(system.value(slider).unwrap(), Some(UiValue::Number(50.0)));

        let dragged = step(
            &mut system,
            UiInput {
                primary_down: true,
                ..at_pointer(160.0, 110.0)
            },
            0.0,
        );
        assert_eq!(value_events(&dragged), vec![UiValue::Number(75.0)]);

        let released = step(&mut system, release(160.0, 110.0), 0.0);
        assert!(!has(&released, Kind::Click, slider));
        assert_eq!(system.value(slider).unwrap(), Some(UiValue::Number(75.0)));
    }

    #[test]
    fn clear_focus_reports_blur_once_and_leaves_no_focus() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        system.focus(a).unwrap();

        let events = system.clear_focus();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Blur);
        assert_eq!(system.focused(), None);
        assert!(system.clear_focus().is_empty());
    }

    #[test]
    fn directional_navigation_picks_nearest_and_respects_explicit_nav() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        let b = system.find(canvas, "b").unwrap();
        let c = system.find(canvas, "c").unwrap();
        system.focus(a).unwrap();

        let right = step(
            &mut system,
            UiInput {
                actions: vec![UiAction::Right],
                ..UiInput::default()
            },
            0.0,
        );
        assert!(has(&right, Kind::Focus, b));
        assert_eq!(right.claims.actions, vec![UiAction::Right]);
        step(
            &mut system,
            UiInput {
                actions: vec![UiAction::Right],
                ..UiInput::default()
            },
            0.0,
        );
        assert_eq!(system.focused(), Some(c));
        step(
            &mut system,
            UiInput {
                actions: vec![UiAction::Left],
                ..UiInput::default()
            },
            0.0,
        );
        assert_eq!(system.focused(), Some(b));

        // Вниз соседей нет: фокус остаётся, действие всё равно поглощено.
        let down = step(
            &mut system,
            UiInput {
                actions: vec![UiAction::Down],
                ..UiInput::default()
            },
            0.0,
        );
        assert_eq!(system.focused(), Some(b));
        assert_eq!(down.claims.actions, vec![UiAction::Down]);
    }

    #[test]
    fn explicit_nav_overrides_geometry() {
        // Геометрически «c» справа от «a», а не ниже; явный nav.down всё равно выбирает её.
        let document = format!(
            r#"
[[elements]]
id = "a"
button = {{}}
nav = {{ down = "c" }}
{}

[[elements]]
id = "c"
button = {{}}
{}
"#,
            at(10.0, 10.0, 100.0, 40.0),
            at(390.0, 10.0, 100.0, 40.0)
        );
        let (mut system, canvas) = open(&document, 1);
        let a = system.find(canvas, "a").unwrap();
        let c = system.find(canvas, "c").unwrap();
        system.focus(a).unwrap();
        step(
            &mut system,
            UiInput {
                actions: vec![UiAction::Down],
                ..UiInput::default()
            },
            0.0,
        );
        assert_eq!(system.focused(), Some(c));
    }

    #[test]
    fn tab_cycles_in_draw_order_both_ways() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        let b = system.find(canvas, "b").unwrap();
        let c = system.find(canvas, "c").unwrap();
        let tab = |system: &mut UiSystem, action| {
            step(
                system,
                UiInput {
                    actions: vec![action],
                    ..UiInput::default()
                },
                0.0,
            )
        };

        tab(&mut system, UiAction::Next);
        assert_eq!(system.focused(), Some(a));
        tab(&mut system, UiAction::Next);
        assert_eq!(system.focused(), Some(b));
        tab(&mut system, UiAction::Next);
        assert_eq!(system.focused(), Some(c));
        tab(&mut system, UiAction::Next);
        assert_eq!(
            system.focused(),
            Some(a),
            "с конца список замыкается в начало"
        );
        tab(&mut system, UiAction::Previous);
        assert_eq!(system.focused(), Some(c));
        tab(&mut system, UiAction::Previous);
        assert_eq!(system.focused(), Some(b));
    }

    #[test]
    fn held_direction_repeats_after_delay_with_injected_time() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        let b = system.find(canvas, "b").unwrap();
        let c = system.find(canvas, "c").unwrap();
        system.focus(a).unwrap();
        let held = |actions: Vec<UiAction>| UiInput {
            actions,
            held: Some(UiAction::Right),
            ..UiInput::default()
        };

        step(&mut system, held(vec![UiAction::Right]), 0.0);
        assert_eq!(system.focused(), Some(b));
        step(&mut system, held(vec![]), 0.3);
        assert_eq!(system.focused(), Some(b), "до задержки повтора нет");
        step(&mut system, held(vec![]), 0.45);
        assert_eq!(system.focused(), Some(c), "первый повтор после 0.4 с");
        step(&mut system, held(vec![]), 0.5);
        assert_eq!(system.focused(), Some(c));

        step(&mut system, UiInput::default(), 0.6);
        step(&mut system, held(vec![]), 0.7);
        assert_eq!(system.focused(), Some(c), "без удержания повторов нет");
    }

    #[test]
    fn input_field_edits_by_character_and_respects_filter_and_length() {
        let (mut system, canvas) = open(
            &format!(
                "[[elements]]\nid = \"field\"\ninput_field = {{ text = \"\", content = \"integer\", max_length = 3 }}\n{}",
                at(10.0, 10.0, 200.0, 30.0)
            ),
            1,
        );
        let field = system.find(canvas, "field").unwrap();
        system.focus(field).unwrap();
        let typed = |system: &mut UiSystem, events: Vec<TextInputEvent>| {
            step(
                system,
                UiInput {
                    text: events,
                    ..UiInput::default()
                },
                0.0,
            )
        };

        let first = typed(&mut system, vec![TextInputEvent::Text("12x".into())]);
        assert_eq!(value_events(&first), vec![UiValue::Text("12".into())]);
        let capped = typed(&mut system, vec![TextInputEvent::Text("345".into())]);
        assert_eq!(value_events(&capped), vec![UiValue::Text("123".into())]);
        assert!(capped.claims.text_focused);

        typed(&mut system, vec![TextInputEvent::Backspace]);
        typed(
            &mut system,
            vec![TextInputEvent::Left, TextInputEvent::Delete],
        );
        assert_eq!(
            system.value(field).unwrap(),
            Some(UiValue::Text("1".into()))
        );
        typed(
            &mut system,
            vec![TextInputEvent::Home, TextInputEvent::Text("-".into())],
        );
        assert_eq!(
            system.value(field).unwrap(),
            Some(UiValue::Text("-1".into()))
        );
        let submitted = typed(
            &mut system,
            vec![TextInputEvent::End, TextInputEvent::Enter],
        );
        assert!(
            submitted
                .events
                .iter()
                .any(|event| event.kind == Kind::Submit
                    && event.value == Some(UiValue::Text("-1".into())))
        );
    }

    #[test]
    fn left_and_right_move_caret_not_focus_inside_input_field() {
        let (mut system, canvas) = open(
            &format!(
                "[[elements]]\nid = \"field\"\ninput_field = {{ text = \"ab\" }}\n{}\n[[elements]]\nid = \"other\"\nbutton = {{}}\n{}",
                at(10.0, 10.0, 200.0, 30.0),
                at(10.0, 60.0, 200.0, 30.0)
            ),
            1,
        );
        let field = system.find(canvas, "field").unwrap();
        system.focus(field).unwrap();
        let update = step(
            &mut system,
            UiInput {
                actions: vec![UiAction::Left],
                text: vec![TextInputEvent::Left],
                ..UiInput::default()
            },
            0.0,
        );
        assert_eq!(system.focused(), Some(field));
        assert_eq!(update.claims.actions, vec![UiAction::Left]);
    }

    #[test]
    fn password_content_keeps_text_in_value() {
        let (mut system, canvas) = open(
            "[[elements]]\nid = \"pw\"\ninput_field = { content = \"password\" }\n",
            1,
        );
        let pw = system.find(canvas, "pw").unwrap();
        system.focus(pw).unwrap();
        step(
            &mut system,
            UiInput {
                text: vec![TextInputEvent::Text("p@ss".into())],
                ..UiInput::default()
            },
            0.0,
        );
        assert_eq!(
            system.value(pw).unwrap(),
            Some(UiValue::Text("p@ss".into()))
        );
    }

    #[test]
    fn scroll_view_clamps_wheel_and_follows_drag() {
        let (mut system, canvas) = open(
            &format!(
                r#"
[[elements]]
id = "view"
scroll_view = {{ vertical = true, sensitivity = 40.0 }}
{}

[[elements]]
id = "content"
parent = "view"
vertical = {{}}
rect = {{ anchor_min = [0.0, 0.0], anchor_max = [1.0, 0.0], pivot = [0.0, 0.0], offset_min = [0.0, 0.0], offset_max = [0.0, 200.0] }}

[[elements]]
id = "row"
parent = "content"
panel = {{}}
{}
"#,
                at(10.0, 200.0, 100.0, 50.0),
                at(0.0, 0.0, 100.0, 20.0)
            ),
            1,
        );
        let content = system.find(canvas, "content").unwrap();
        let wheel = |y: f32| UiInput {
            wheel: vec2(0.0, y),
            ..at_pointer(50.0, 220.0)
        };

        let down = step(&mut system, wheel(-10.0), 0.0);
        assert!(down.claims.wheel);
        let top = system.canvas_ref(canvas).unwrap().layout.rects[content.index()]
            .min
            .y;
        assert_eq!(top, 200.0 - 150.0, "предел прокрутки 200 - 50");

        step(&mut system, wheel(1000.0), 0.0);
        let top = system.canvas_ref(canvas).unwrap().layout.rects[content.index()]
            .min
            .y;
        assert_eq!(top, 200.0, "вверху прокрутка ограничена нулём");

        step(&mut system, press(50.0, 220.0), 0.0);
        step(
            &mut system,
            UiInput {
                primary_down: true,
                ..at_pointer(50.0, 200.0)
            },
            0.0,
        );
        let top = system.canvas_ref(canvas).unwrap().layout.rects[content.index()]
            .min
            .y;
        assert_eq!(
            top, 180.0,
            "тянем вверх на 20: содержимое поднимается, прокрутка 20"
        );
    }

    #[test]
    fn focus_skips_elements_clipped_out_of_scroll_view() {
        let (mut system, canvas) = open(
            &format!(
                r#"
[[elements]]
id = "view"
scroll_view = {{ vertical = true }}
{}

[[elements]]
id = "content"
parent = "view"
panel = {{ color = [0, 0, 0, 0] }}
rect = {{ anchor_min = [0.0, 0.0], anchor_max = [1.0, 0.0], pivot = [0.0, 0.0], offset_min = [0.0, 0.0], offset_max = [0.0, 300.0] }}

[[elements]]
id = "visible"
parent = "content"
button = {{}}
{}

[[elements]]
id = "hidden"
parent = "content"
button = {{}}
{}
"#,
                at(10.0, 200.0, 100.0, 50.0),
                at(0.0, 0.0, 50.0, 20.0),
                at(0.0, 200.0, 50.0, 20.0)
            ),
            1,
        );
        let visible = system.find(canvas, "visible").unwrap();
        let hidden = system.find(canvas, "hidden").unwrap();
        let tab = |system: &mut UiSystem| {
            step(
                system,
                UiInput {
                    actions: vec![UiAction::Next],
                    ..UiInput::default()
                },
                0.0,
            )
        };
        tab(&mut system);
        assert_eq!(system.focused(), Some(visible));
        tab(&mut system);
        assert_eq!(
            system.focused(),
            Some(visible),
            "элемент за пределами окна прокрутки не получает фокус"
        );
        assert_ne!(system.focused(), Some(hidden));
    }

    #[test]
    fn wheel_over_plain_area_is_not_claimed() {
        let (mut system, _) = open(&row(), 1);
        let update = step(
            &mut system,
            UiInput {
                wheel: vec2(0.0, 1.0),
                ..at_pointer(900.0, 900.0)
            },
            0.0,
        );
        assert!(!update.claims.wheel);
    }

    #[test]
    fn dropdown_opens_selects_by_pointer_and_closes_on_outside_press() {
        let (mut system, canvas) = open(
            &format!(
                "[[elements]]\nid = \"drop\"\ndropdown = {{ options = [\"a\", \"b\", \"c\"], selected = 0, popup_height = 200.0 }}\n{}\n[[elements]]\nid = \"other\"\nbutton = {{}}\n{}",
                at(10.0, 300.0, 100.0, 30.0),
                at(400.0, 300.0, 100.0, 30.0)
            ),
            1,
        );
        let drop = system.find(canvas, "drop").unwrap();
        let other = system.find(canvas, "other").unwrap();

        step(&mut system, press(50.0, 310.0), 0.0);
        step(&mut system, release(50.0, 310.0), 0.0);
        assert_eq!(system.canvas_ref(canvas).unwrap().popup, Some(drop.index()));

        // Пункт 1: высота пункта 30, список начинается с 330, пункт 1 занимает 360..390.
        step(&mut system, press(50.0, 375.0), 0.0);
        let chosen = step(&mut system, release(50.0, 375.0), 0.0);
        assert_eq!(value_events(&chosen), vec![UiValue::Index(1)]);
        assert_eq!(system.value(drop).unwrap(), Some(UiValue::Index(1)));
        assert_eq!(system.canvas_ref(canvas).unwrap().popup, None);

        step(&mut system, release(50.0, 310.0), 0.0);
        step(&mut system, press(50.0, 310.0), 0.0);
        step(&mut system, release(50.0, 310.0), 0.0);
        assert!(system.canvas_ref(canvas).unwrap().popup.is_some());
        let outside = step(&mut system, press(450.0, 310.0), 0.0);
        assert!(outside.claims.pointer_primary);
        assert_eq!(
            system.canvas_ref(canvas).unwrap().popup,
            None,
            "нажатие в другой элемент закрывает список"
        );
        assert!(has(&outside, Kind::Focus, other));
    }

    #[test]
    fn dropdown_keyboard_moves_highlight_and_cancel_closes() {
        let (mut system, canvas) = open(
            "[[elements]]\nid = \"drop\"\ndropdown = { options = [\"a\", \"b\", \"c\"] }\n",
            1,
        );
        let drop = system.find(canvas, "drop").unwrap();
        system.focus(drop).unwrap();
        let key = |action| UiInput {
            actions: vec![action],
            ..UiInput::default()
        };

        step(&mut system, key(UiAction::Confirm), 0.0);
        assert!(system.canvas_ref(canvas).unwrap().popup.is_some());
        step(&mut system, key(UiAction::Down), 0.0);
        step(&mut system, key(UiAction::Down), 0.0);
        step(&mut system, key(UiAction::Down), 0.0);
        assert_eq!(
            system.canvas_ref(canvas).unwrap().cursors[drop.index()],
            2,
            "подсветка не выходит за список"
        );
        step(&mut system, key(UiAction::Confirm), 0.0);
        assert_eq!(system.value(drop).unwrap(), Some(UiValue::Index(2)));

        step(&mut system, key(UiAction::Confirm), 0.0);
        step(&mut system, key(UiAction::Cancel), 0.0);
        assert_eq!(
            system.canvas_ref(canvas).unwrap().popup,
            None,
            "Esc закрывает список, не отправляя cancel"
        );
    }

    #[test]
    fn cancel_event_goes_to_focused_element_without_open_list() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        system.focus(a).unwrap();
        let update = step(
            &mut system,
            UiInput {
                actions: vec![UiAction::Cancel],
                ..UiInput::default()
            },
            0.0,
        );
        assert!(has(&update, Kind::Cancel, a));
    }

    #[test]
    fn toggle_flips_and_reports_click_and_value() {
        let (mut system, canvas) = open(
            &format!(
                "[[elements]]\nid = \"t\"\ntoggle = {{}}\n{}",
                at(10.0, 10.0, 40.0, 40.0)
            ),
            1,
        );
        let toggle = system.find(canvas, "t").unwrap();
        step(&mut system, press(20.0, 20.0), 0.0);
        let update = step(&mut system, release(20.0, 20.0), 0.0);
        assert!(has(&update, Kind::Click, toggle));
        assert_eq!(value_events(&update), vec![UiValue::Bool(true)]);
        assert_eq!(system.value(toggle).unwrap(), Some(UiValue::Bool(true)));
    }

    #[test]
    fn default_focus_applies_on_first_update() {
        let (mut system, canvas) = open(&format!("[canvas]\ndefault_focus = \"b\"\n{}", row()), 1);
        let b = system.find(canvas, "b").unwrap();
        let update = step(&mut system, UiInput::default(), 0.0);
        assert!(has(&update, Kind::Focus, b));
        assert_eq!(system.focused(), Some(b));
    }

    #[test]
    fn hiding_focused_element_blurs_it_on_next_update() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        system.focus(a).unwrap();
        system.set_visible(a, false).unwrap();
        let update = step(&mut system, UiInput::default(), 0.0);
        assert!(has(&update, Kind::Blur, a));
        assert_eq!(system.focused(), None);
    }

    #[test]
    fn closing_canvas_reports_blur_and_hover_exit_once() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        step(&mut system, at_pointer(50.0, 30.0), 0.0);
        system.focus(a).unwrap();
        let events = system.close(canvas).unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.kind == Kind::HoverExit && event.element == a)
        );
        assert!(
            events
                .iter()
                .any(|event| event.kind == Kind::Blur && event.element == a)
        );
        assert_eq!(system.focused(), None);
        assert_eq!(system.focus(a), Err(UiSystemError::StaleHandle));
    }

    #[test]
    fn reset_interaction_clears_hover_and_focus_with_events() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        step(&mut system, at_pointer(50.0, 30.0), 0.0);
        system.focus(a).unwrap();
        let events = system.reset_interaction();
        assert!(events.iter().any(|event| event.kind == Kind::HoverExit));
        assert!(events.iter().any(|event| event.kind == Kind::Blur));
        assert!(!system.pointer_over());
    }

    #[test]
    fn stale_handles_are_rejected_for_focus_and_are_not_reused() {
        let (mut system, canvas) = open(&row(), 1);
        let a = system.find(canvas, "a").unwrap();
        system.close(canvas).unwrap();
        let reopened = system.open(
            Arc::new(UiDocument::parse("again.rveui", &row()).unwrap()),
            2,
        );
        assert_ne!(reopened, canvas);
        assert_eq!(system.focus(a), Err(UiSystemError::StaleHandle));
    }

    #[test]
    fn primary_press_outside_ui_is_not_claimed_and_actions_need_focus() {
        let (mut system, canvas) = open(&row(), 1);
        let empty = step(&mut system, press(900.0, 900.0), 0.0);
        assert!(!empty.claims.pointer_primary);

        let without_focus = step(
            &mut system,
            UiInput {
                actions: vec![UiAction::Right],
                ..UiInput::default()
            },
            0.0,
        );
        assert!(without_focus.claims.actions.is_empty());

        let a = system.find(canvas, "a").unwrap();
        system.focus(a).unwrap();
        let with_focus = step(
            &mut system,
            UiInput {
                actions: vec![UiAction::Right],
                ..UiInput::default()
            },
            0.0,
        );
        assert_eq!(with_focus.claims.actions, vec![UiAction::Right]);
    }
}
