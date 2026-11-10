//! События и значения UI: клики, изменения, фокус и наведение. Скрипты получают их в начале update.

use crate::system::{UiCanvasHandle, UiElementHandle};

/// Значение элемента: флаг переключателя, число слайдера/прогресса, индекс выпадающего списка или текст.
#[derive(Clone, Debug, PartialEq)]
pub enum UiValue {
    Bool(bool),
    Number(f32),
    Index(usize),
    Text(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiEventKind {
    Click,
    ValueChanged,
    Submit,
    Cancel,
    Focus,
    Blur,
    HoverEnter,
    HoverExit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UiEvent {
    pub canvas: UiCanvasHandle,
    pub element: UiElementHandle,
    pub kind: UiEventKind,
    pub value: Option<UiValue>,
}
