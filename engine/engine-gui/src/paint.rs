//! Тесселяция UI в draw list физических пикселей через epaint.
//!
//! Геометрия строится в единицах канваса и переводится в пиксели умножением на масштаб канваса.
//! Порядок: канвасы по возрастанию слоя, внутри — порядок отрисовки; список Dropdown рисуется
//! поверх своего канваса. Атлас шрифтов общий для всех канвасов: его `Arc` и версия меняются
//! только при изменении содержимого. Тексты раскладываются до тесселяции, чтобы UV глифов
//! нормализовались по окончательному атласу.

use crate::document::{
    AssetKind, Axis, ContentType, ElementKind, HAlign, InputFieldSpec, StateValues, TextSpec,
    UiPath, VAlign, asset_kind,
};
use crate::input::Part;
use crate::layout::canvas_size;
use crate::system::{UiCanvas, UiCanvasHandle, UiSystem, UiSystemError};
use epaint::text::cursor::CCursor;
use epaint::text::{FontDefinitions, FontFamily, FontId, Fonts, FontsView, TextOptions};
use epaint::{
    Color32, CornerRadius, Galley, Mesh, Pos2, Rect, RectShape, Shape, Stroke, StrokeKind,
    TessellationOptions, Tessellator, TextShape, Vec2, pos2, vec2,
};
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

/// Пункт открытого Dropdown под курсором подсвечивается этим цветом.
const HIGHLIGHT: [u8; 4] = [70, 110, 180, 255];
/// Знак маски для пароля.
const MASK: char = '•';
/// Ширина каретки в единицах канваса; при масштабе 1 это один пиксель.
const CARET_WIDTH: f32 = 1.0;

/// Текстура батча: атлас шрифтов или картинка по каноническому пути ассета (без ведущего слеша).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiTexture {
    FontAtlas,
    Image(String),
}

/// Вершина в физических пикселях окна; цвет — premultiplied sRGB.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiVertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub color: [u8; 4],
}

/// Подряд идущие индексы одной текстуры и одной области отсечения.
#[derive(Clone, Debug, PartialEq)]
pub struct UiBatch {
    pub texture: UiTexture,
    /// Минимум и максимум по осям в физических пикселях: `[min_x, min_y, max_x, max_y]`.
    pub clip_rect: [f32; 4],
    pub indices: Range<u32>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UiDrawList {
    pub vertices: Vec<UiVertex>,
    pub indices: Vec<u32>,
    pub batches: Vec<UiBatch>,
}

/// Атлас глифов в premultiplied sRGB RGBA. `version` растёт при каждом изменении пикселей.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FontAtlas {
    pub version: u64,
    pub size: [usize; 2],
    pub pixels: Vec<[u8; 4]>,
}

/// Результат тесселяции: список и атлас, которые render thread загружает по версии.
#[derive(Clone, Debug)]
pub struct UiPaint {
    pub draw_list: Arc<UiDrawList>,
    pub atlas: Arc<FontAtlas>,
}

/// Геометрия до тесселяции: фигуры тесселируются с финальным атласом, меши уже готовы.
enum Piece {
    Shape(Shape),
    Mesh(Mesh),
}

struct Item {
    texture: UiTexture,
    clip: Rect,
    piece: Piece,
}

/// Элементы одного канваса вместе с его масштабом.
struct CanvasItems {
    scale: f32,
    items: Vec<Item>,
}

/// Состояние кнопки для выбора оттенка и картинки.
struct ButtonState {
    disabled: bool,
    pressed: bool,
    hovered: bool,
    focused: bool,
}

impl UiSystem {
    /// Регистрирует шрифт `.ttf`/`.otf` по пути ассета. Байты, которые не читает skrifa, дают
    /// ошибку до сборки атласа; шрифт не сохраняется.
    pub fn register_font(&mut self, path: &str, bytes: Vec<u8>) -> Result<(), UiSystemError> {
        let path = canonical_asset(path, AssetKind::Font)?;
        // epaint паникует при нечитаемом шрифте (`Fonts::new`), поэтому проверка идёт раньше.
        if skrifa::FontRef::new(&bytes).is_err() {
            return Err(UiSystemError::InvalidValue(format!(
                "шрифт /{path} не читается как TTF/OTF"
            )));
        }
        self.font_data.insert(path, bytes);
        self.fonts = Some(Fonts::new(
            TextOptions::default(),
            font_definitions(&self.font_data),
        ));
        self.paint_dirty = true;
        Ok(())
    }

    /// Регистрирует размер картинки `.png`: нужен для 9-slice и `preserve_aspect`.
    /// Картинка без зарегистрированного размера не рисуется.
    pub fn register_image(&mut self, path: &str, size: [u32; 2]) -> Result<(), UiSystemError> {
        let path = canonical_asset(path, AssetKind::Image)?;
        if size.contains(&0) {
            return Err(UiSystemError::InvalidValue(format!(
                "картинка /{path}: размер должен быть больше 0"
            )));
        }
        self.images.insert(path, size);
        self.paint_dirty = true;
        Ok(())
    }

    /// Текущая тесселяция. Пока состояние не менялось, возвращает те же `Arc`, без пересчёта.
    pub fn paint(&mut self) -> UiPaint {
        if !self.paint_dirty
            && let Some(paint) = &self.last_paint
        {
            return paint.clone();
        }
        let mut fonts = self.fonts.take().unwrap_or_else(|| {
            Fonts::new(TextOptions::default(), font_definitions(&self.font_data))
        });
        fonts.begin_pass(TextOptions::default());
        let groups = if self.screen.x > 0.0 && self.screen.y > 0.0 {
            self.layout_items(&mut fonts)
        } else {
            Vec::new()
        };
        self.fonts = Some(fonts);
        self.sync_atlas();

        let atlas_size = self.atlas.size;
        let mut list = UiDrawList::default();
        for group in groups {
            let mut tessellator = Tessellator::new(
                group.scale,
                TessellationOptions::default(),
                atlas_size,
                Vec::new(),
            );
            for item in group.items {
                let mut mesh = Mesh::default();
                match item.piece {
                    Piece::Shape(shape) => tessellator.tessellate_shape(shape, &mut mesh),
                    Piece::Mesh(built) => mesh = built,
                }
                push_mesh(&mut list, &mesh, item.texture, item.clip, group.scale);
            }
        }
        let paint = UiPaint {
            draw_list: Arc::new(list),
            atlas: Arc::clone(&self.atlas),
        };
        self.last_paint = Some(paint.clone());
        self.paint_dirty = false;
        paint
    }

    /// Переносит изменения атласа из epaint в копию. Полная замена (`pos = None`) задаёт новую
    /// текстуру; частичная — прямоугольник внутри текущей. Каждое изменение — новая версия.
    fn sync_atlas(&mut self) {
        let Some(fonts) = self.fonts.as_mut() else {
            return;
        };
        while let Some(delta) = fonts.font_image_delta() {
            let epaint::ImageData::Color(image) = delta.image;
            let [width, height] = image.size;
            let atlas = Arc::make_mut(&mut self.atlas);
            match delta.pos {
                None => {
                    atlas.size = [width, height];
                    atlas.pixels = image.pixels.iter().map(|pixel| pixel.to_array()).collect();
                }
                Some([x, y]) => {
                    let [atlas_width, atlas_height] = atlas.size;
                    if x + width > atlas_width || y + height > atlas_height {
                        continue;
                    }
                    for row in 0..height {
                        for column in 0..width {
                            atlas.pixels[(y + row) * atlas_width + x + column] =
                                image.pixels[row * width + column].to_array();
                        }
                    }
                }
            }
            atlas.version += 1;
        }
    }

    /// Геометрия всех видимых канвасов от нижнего к верхнему.
    fn layout_items(&self, fonts: &mut Fonts) -> Vec<CanvasItems> {
        let mut groups = Vec::new();
        for canvas_handle in self.canvases_top_down().into_iter().rev() {
            let Ok(canvas) = self.canvas_ref(canvas_handle) else {
                continue;
            };
            if !canvas.visible {
                continue;
            }
            let scale = canvas.scale;
            let canvas_rect = Rect::from_min_size(pos2(0.0, 0.0), canvas_size(self.screen, scale));
            let mut view = fonts.with_pixels_per_point(scale);
            let mut items = Vec::new();
            for &index in &canvas.layout.order {
                let state = self.button_state(canvas_handle, canvas, index);
                element_items(&mut view, self, canvas, index, state, &mut items);
            }
            if let Some(open) = canvas.popup {
                popup_items(&mut view, canvas, open, canvas_rect, &mut items);
            }
            groups.push(CanvasItems { scale, items });
        }
        groups
    }

    fn button_state(
        &self,
        canvas_handle: UiCanvasHandle,
        canvas: &UiCanvas,
        index: usize,
    ) -> ButtonState {
        let is = |handle: Option<crate::system::UiElementHandle>| {
            handle.is_some_and(|handle| handle.canvas == canvas_handle && handle.index() == index)
        };
        ButtonState {
            disabled: !canvas.elements[index].interactable,
            pressed: self
                .press
                .is_some_and(|press| press.part == Part::Body && is(Some(press.element))),
            hovered: is(self.hover),
            focused: is(self.focus),
        }
    }
}

fn element_items(
    view: &mut FontsView<'_>,
    system: &UiSystem,
    canvas: &UiCanvas,
    index: usize,
    state: ButtonState,
    items: &mut Vec<Item>,
) {
    let element = &canvas.elements[index];
    let rect = canvas.layout.rects[index];
    let clip = canvas.layout.clips[index];
    match &element.kind {
        ElementKind::Panel(spec) => {
            let stroke = Stroke::new(spec.border_width, color(spec.border_color));
            let shape = rect_shape(rect, corner(spec.corner_radius), color(spec.color), stroke);
            push_shape(items, shape, clip);
        }
        ElementKind::Image(spec) => {
            push_image(
                items,
                system,
                spec.path.as_str(),
                rect,
                clip,
                color(spec.tint),
                spec.slice,
                spec.preserve_aspect,
            );
        }
        ElementKind::Text(spec) => {
            push_text(
                view,
                items,
                spec,
                spec.string.clone(),
                rect,
                clip,
                spec.color,
            );
        }
        ElementKind::Button(spec) => match button_image(&spec.image, &state) {
            Some(path) => {
                let tint = button_tint(&spec.tint, &state);
                push_image(
                    items,
                    system,
                    path.as_str(),
                    rect,
                    clip,
                    color(tint),
                    None,
                    false,
                );
            }
            None => {
                let tint = button_tint(&spec.tint, &state);
                push_shape(items, rect_shape(rect, 0, color(tint), Stroke::NONE), clip);
            }
        },
        ElementKind::Toggle(spec) => {
            if spec.is_on {
                match &spec.checkmark_image {
                    Some(path) => push_image(
                        items,
                        system,
                        path.as_str(),
                        rect,
                        clip,
                        color(spec.checkmark_color),
                        None,
                        false,
                    ),
                    None => {
                        let mark = rect.shrink(rect.width().min(rect.height()) * 0.25);
                        push_shape(
                            items,
                            rect_shape(mark, 0, color(spec.checkmark_color), Stroke::NONE),
                            clip,
                        );
                    }
                }
            }
        }
        ElementKind::Slider(spec) => {
            let fraction = if spec.max > spec.min {
                ((spec.value - spec.min) / (spec.max - spec.min)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let thickness = (spec.handle_size / 3.0).max(2.0);
            let (track, fill, handle_center) = match spec.direction {
                Axis::Horizontal => {
                    let cy = rect.center().y;
                    let track = Rect::from_min_max(
                        pos2(rect.min.x, cy - thickness / 2.0),
                        pos2(rect.max.x, cy + thickness / 2.0),
                    );
                    let x = rect.min.x + rect.width() * fraction;
                    (
                        track,
                        Rect::from_min_max(track.min, pos2(x, track.max.y)),
                        pos2(x, cy),
                    )
                }
                Axis::Vertical => {
                    let cx = rect.center().x;
                    let track = Rect::from_min_max(
                        pos2(cx - thickness / 2.0, rect.min.y),
                        pos2(cx + thickness / 2.0, rect.max.y),
                    );
                    let y = rect.max.y - rect.height() * fraction;
                    (
                        track,
                        Rect::from_min_max(pos2(track.min.x, y), track.max),
                        pos2(cx, y),
                    )
                }
            };
            let handle =
                Rect::from_center_size(handle_center, vec2(spec.handle_size, spec.handle_size));
            push_shape(
                items,
                rect_shape(track, 0, color(spec.track_color), Stroke::NONE),
                clip,
            );
            push_shape(
                items,
                rect_shape(fill, 0, color(spec.fill_color), Stroke::NONE),
                clip,
            );
            push_shape(
                items,
                rect_shape(handle, 0, color(spec.handle_color), Stroke::NONE),
                clip,
            );
        }
        ElementKind::ProgressBar(spec) => {
            let value = spec.value.clamp(0.0, 1.0);
            let fill = match spec.direction {
                Axis::Horizontal => Rect::from_min_max(
                    rect.min,
                    pos2(rect.min.x + rect.width() * value, rect.max.y),
                ),
                Axis::Vertical => Rect::from_min_max(
                    pos2(rect.min.x, rect.max.y - rect.height() * value),
                    rect.max,
                ),
            };
            push_shape(
                items,
                rect_shape(rect, 0, color(spec.background_color), Stroke::NONE),
                clip,
            );
            push_shape(
                items,
                rect_shape(fill, 0, color(spec.fill_color), Stroke::NONE),
                clip,
            );
        }
        ElementKind::InputField(spec) => {
            input_items(
                view,
                items,
                spec,
                rect,
                clip,
                canvas.cursors[index],
                state.focused,
            );
        }
        ElementKind::Dropdown(spec) => {
            push_shape(
                items,
                rect_shape(rect, 0, color(spec.background_color), Stroke::NONE),
                clip,
            );
            if let Some(label) = spec
                .selected
                .and_then(|selected| spec.options.get(selected))
            {
                let style = TextSpec {
                    font: spec.font.clone(),
                    size: spec.size,
                    color: spec.color,
                    v_align: VAlign::Middle,
                    ..TextSpec::default()
                };
                push_text(view, items, &style, label.clone(), rect, clip, spec.color);
            }
        }
        ElementKind::ScrollView(_)
        | ElementKind::Grid(_)
        | ElementKind::Horizontal(_)
        | ElementKind::Vertical(_) => {}
    }
}

/// Список Dropdown: панель под элементом, пункты высотой с элемент, подсвечен курсор списка.
fn popup_items(
    view: &mut FontsView<'_>,
    canvas: &UiCanvas,
    index: usize,
    canvas_rect: Rect,
    items: &mut Vec<Item>,
) {
    let ElementKind::Dropdown(spec) = &canvas.elements[index].kind else {
        return;
    };
    let rect = canvas.layout.rects[index];
    let popup = Rect::from_min_size(
        pos2(rect.min.x, rect.max.y),
        vec2(rect.width(), spec.popup_height),
    );
    push_shape(
        items,
        rect_shape(popup, 0, color(spec.background_color), Stroke::NONE),
        canvas_rect,
    );
    let highlight = canvas.cursors[index];
    let style = TextSpec {
        font: spec.font.clone(),
        size: spec.size,
        color: spec.color,
        v_align: VAlign::Middle,
        ..TextSpec::default()
    };
    for (option_index, option) in spec.options.iter().enumerate() {
        let top = popup.min.y + option_index as f32 * rect.height();
        if top >= popup.max.y {
            break;
        }
        let row = Rect::from_min_max(
            pos2(popup.min.x, top),
            pos2(popup.max.x, top + rect.height()),
        );
        if option_index == highlight {
            push_shape(
                items,
                rect_shape(row, 0, color(HIGHLIGHT), Stroke::NONE),
                canvas_rect,
            );
        }
        push_text(
            view,
            items,
            &style,
            option.clone(),
            row,
            canvas_rect,
            spec.color,
        );
    }
}

fn input_items(
    view: &mut FontsView<'_>,
    items: &mut Vec<Item>,
    spec: &InputFieldSpec,
    rect: Rect,
    clip: Rect,
    cursor: usize,
    focused: bool,
) {
    let empty = spec.text.is_empty();
    let (text, text_color) = if empty {
        (spec.placeholder.clone(), spec.placeholder_color)
    } else {
        (display_text(spec.content, &spec.text), spec.color)
    };
    let style = TextSpec {
        font: spec.font.clone(),
        size: spec.size,
        color: text_color,
        v_align: VAlign::Middle,
        ..TextSpec::default()
    };
    let galley = layout_text(view, &style, text, f32::INFINITY);
    let origin = aligned(rect, galley.size(), HAlign::Left, VAlign::Middle);
    if focused {
        let caret = galley.pos_from_cursor(CCursor::new(cursor.min(spec.text.chars().count())));
        let x = origin.x + caret.min.x;
        let caret_rect = Rect::from_min_max(
            pos2(x, origin.y + caret.min.y),
            pos2(x + CARET_WIDTH, origin.y + caret.max.y),
        );
        push_shape(
            items,
            rect_shape(caret_rect, 0, color(spec.color), Stroke::NONE),
            clip,
        );
    }
    push_galley(items, origin, galley, color(text_color), clip);
}

/// Текст, выровненный внутри прямоугольника; перенос по ширине, если `wrap`.
fn push_text(
    view: &mut FontsView<'_>,
    items: &mut Vec<Item>,
    style: &TextSpec,
    text: String,
    rect: Rect,
    clip: Rect,
    text_color: [u8; 4],
) {
    let wrap = if style.wrap {
        rect.width()
    } else {
        f32::INFINITY
    };
    let galley = layout_text(view, style, text, wrap);
    let origin = aligned(rect, galley.size(), style.h_align, style.v_align);
    push_galley(items, origin, galley, color(text_color), clip);
}

fn layout_text(view: &mut FontsView<'_>, style: &TextSpec, text: String, wrap: f32) -> Arc<Galley> {
    let family = match &style.font {
        Some(path) => FontFamily::Name(path.as_str().into()),
        None => FontFamily::Proportional,
    };
    view.layout(
        text,
        FontId::new(style.size, family),
        color(style.color),
        wrap,
    )
}

fn push_galley(
    items: &mut Vec<Item>,
    origin: Pos2,
    galley: Arc<Galley>,
    fallback: Color32,
    clip: Rect,
) {
    push_shape(
        items,
        Shape::Text(TextShape::new(origin, galley, fallback)),
        clip,
    );
}

/// Текст для показа: пароль маскируется по одному знаку на символ.
pub(crate) fn display_text(content: ContentType, text: &str) -> String {
    match content {
        ContentType::Password => std::iter::repeat_n(MASK, text.chars().count()).collect(),
        ContentType::Standard | ContentType::Integer | ContentType::Decimal => text.to_owned(),
    }
}

fn aligned(rect: Rect, size: Vec2, h: HAlign, v: VAlign) -> Pos2 {
    let x = match h {
        HAlign::Left => rect.min.x,
        HAlign::Center => rect.center().x - size.x / 2.0,
        HAlign::Right => rect.max.x - size.x,
    };
    let y = match v {
        VAlign::Top => rect.min.y,
        VAlign::Middle => rect.center().y - size.y / 2.0,
        VAlign::Bottom => rect.max.y - size.y,
    };
    pos2(x, y)
}

/// Картинка без зарегистрированного размера пропускается: 9-slice и пропорции без него не посчитать.
#[allow(clippy::too_many_arguments)]
fn push_image(
    items: &mut Vec<Item>,
    system: &UiSystem,
    path: &str,
    rect: Rect,
    clip: Rect,
    tint: Color32,
    slice: Option<[f32; 4]>,
    preserve_aspect: bool,
) {
    let Some(&[width, height]) = system.images.get(path) else {
        return;
    };
    let size = [width as f32, height as f32];
    let target = if preserve_aspect {
        let scale = (rect.width() / size[0]).min(rect.height() / size[1]);
        Rect::from_center_size(rect.center(), vec2(size[0] * scale, size[1] * scale))
    } else {
        rect
    };
    let mut mesh = Mesh::default();
    match slice {
        Some(slice) => {
            for (dest, uv) in nine_slice_quads(target, size, slice) {
                mesh.add_rect_with_uv(dest, uv, tint);
            }
        }
        None => mesh.add_rect_with_uv(
            target,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            tint,
        ),
    }
    items.push(Item {
        texture: UiTexture::Image(path.to_owned()),
        clip,
        piece: Piece::Mesh(mesh),
    });
}

/// 9-slice: девять прямоугольников назначения и их UV в долях картинки. Отступы `[слева, сверху,
/// справа, снизу]` — в пикселях картинки. Если область меньше суммы отступов, средняя полоса
/// схлопывается в линию посередине.
pub(crate) fn nine_slice_quads(dest: Rect, size: [f32; 2], slice: [f32; 4]) -> Vec<(Rect, Rect)> {
    let [left, top, right, bottom] = slice;
    let left = left.min(size[0]);
    let top = top.min(size[1]);
    let xs_src = [0.0, left, (size[0] - right).max(left), size[0]];
    let ys_src = [0.0, top, (size[1] - bottom).max(top), size[1]];
    let mut xs_dst = [
        dest.min.x,
        dest.min.x + left,
        dest.max.x - right,
        dest.max.x,
    ];
    let mut ys_dst = [
        dest.min.y,
        dest.min.y + top,
        dest.max.y - bottom,
        dest.max.y,
    ];
    collapse_middle(&mut xs_dst);
    collapse_middle(&mut ys_dst);

    let mut quads = Vec::with_capacity(9);
    for row in 0..3 {
        for column in 0..3 {
            let target = Rect::from_min_max(
                pos2(xs_dst[column], ys_dst[row]),
                pos2(xs_dst[column + 1], ys_dst[row + 1]),
            );
            let uv = Rect::from_min_max(
                pos2(xs_src[column] / size[0], ys_src[row] / size[1]),
                pos2(xs_src[column + 1] / size[0], ys_src[row + 1] / size[1]),
            );
            quads.push((target, uv));
        }
    }
    quads
}

fn collapse_middle(edges: &mut [f32; 4]) {
    if edges[1] > edges[2] {
        let middle = (edges[1] + edges[2]) / 2.0;
        edges[1] = middle;
        edges[2] = middle;
    }
}

fn push_shape(items: &mut Vec<Item>, shape: Shape, clip: Rect) {
    items.push(Item {
        texture: UiTexture::FontAtlas,
        clip,
        piece: Piece::Shape(shape),
    });
}

fn rect_shape(rect: Rect, radius: u8, fill: Color32, stroke: Stroke) -> Shape {
    Shape::Rect(RectShape::new(
        rect,
        CornerRadius::same(radius),
        fill,
        stroke,
        StrokeKind::Inside,
    ))
}

/// Радиус в epaint — целое число; дробные значения округляются.
fn corner(radius: f32) -> u8 {
    radius.round().clamp(0.0, 255.0) as u8
}

/// Цвет документа задан прямой альфой; epaint работает с premultiplied.
fn color(value: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(value[0], value[1], value[2], value[3])
}

/// Оттенок по состоянию: отключено > нажато > наведено > фокус, иначе обычный (или белый).
fn button_tint(tints: &StateValues<[u8; 4]>, state: &ButtonState) -> [u8; 4] {
    let chosen = if state.disabled {
        tints.disabled
    } else if state.pressed {
        tints.pressed
    } else if state.hovered {
        tints.hover
    } else if state.focused {
        tints.focused
    } else {
        None
    };
    chosen.or(tints.normal).unwrap_or([255; 4])
}

fn button_image<'a>(images: &'a StateValues<UiPath>, state: &ButtonState) -> Option<&'a UiPath> {
    let chosen = if state.disabled {
        images.disabled.as_ref()
    } else if state.pressed {
        images.pressed.as_ref()
    } else if state.hovered {
        images.hover.as_ref()
    } else if state.focused {
        images.focused.as_ref()
    } else {
        None
    };
    chosen.or(images.normal.as_ref())
}

/// Добавляет меш канваса в общий список: вершины переводятся в пиксели, соседние батчи с одной
/// текстурой и клипом объединяются.
fn push_mesh(list: &mut UiDrawList, mesh: &Mesh, texture: UiTexture, clip: Rect, scale: f32) {
    if mesh.indices.is_empty() {
        return;
    }
    let base = list.vertices.len() as u32;
    list.vertices
        .extend(mesh.vertices.iter().map(|vertex| UiVertex {
            pos: [vertex.pos.x * scale, vertex.pos.y * scale],
            uv: [vertex.uv.x, vertex.uv.y],
            color: vertex.color.to_array(),
        }));
    let start = list.indices.len() as u32;
    list.indices
        .extend(mesh.indices.iter().map(|index| base + index));
    let end = list.indices.len() as u32;
    let clip_rect = [
        clip.min.x * scale,
        clip.min.y * scale,
        clip.max.x * scale,
        clip.max.y * scale,
    ];
    if let Some(last) = list.batches.last_mut()
        && last.texture == texture
        && last.clip_rect == clip_rect
        && last.indices.end == start
    {
        last.indices.end = end;
        return;
    }
    list.batches.push(UiBatch {
        texture,
        clip_rect,
        indices: start..end,
    });
}

/// Стандартные шрифты epaint плюс зарегистрированные; свой шрифт — первым в семействе.
fn font_definitions(font_data: &BTreeMap<String, Vec<u8>>) -> FontDefinitions {
    let mut definitions = FontDefinitions::default();
    let defaults = definitions
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for (name, bytes) in font_data {
        definitions.font_data.insert(
            name.clone(),
            Arc::new(epaint::text::FontData::from_owned(bytes.clone())),
        );
        let mut family = vec![name.clone()];
        family.extend(defaults.iter().cloned());
        definitions
            .families
            .insert(FontFamily::Name(name.as_str().into()), family);
    }
    definitions
}

fn canonical_asset(path: &str, expected: AssetKind) -> Result<String, UiSystemError> {
    let parsed = UiPath::parse(path).map_err(UiSystemError::InvalidValue)?;
    match asset_kind(parsed.as_str()) {
        Some(kind) if kind == expected => Ok(parsed.as_str().to_owned()),
        _ => Err(UiSystemError::InvalidValue(format!(
            "ресурс /{} имеет неподходящее расширение",
            parsed.as_str()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::UiDocument;
    use crate::events::UiValue;
    use crate::input::UiInput;
    use crate::system::UiSystem;

    fn screen() -> Vec2 {
        vec2(1000.0, 1000.0)
    }

    /// Система с одним канвасом, уже прошедшим update: без него экран нулевой и тесселяции нет.
    fn system_with(input: &str) -> (UiSystem, UiCanvasHandle) {
        let mut system = UiSystem::default();
        let document = Arc::new(UiDocument::parse("paint-test.rveui", input).unwrap());
        let canvas = system.open(document, 1);
        system.update(&UiInput::default(), 0.0, screen());
        (system, canvas)
    }

    const PANEL_IMAGE_TEXT: &str = r#"
[[assets]]
path = "/ui/icon.png"

[[elements]]
id = "bg"
panel = { color = [10, 20, 30, 255] }
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_min = [0.0, 0.0], offset_max = [100.0, 100.0] }

[[elements]]
id = "icon"
image = { path = "/ui/icon.png" }
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_min = [0.0, 0.0], offset_max = [40.0, 40.0] }

[[elements]]
id = "label"
text = { string = "Привет" }
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_min = [0.0, 50.0], offset_max = [200.0, 30.0] }
"#;

    #[test]
    fn nine_slice_produces_nine_quads_with_corner_uvs() {
        let dest = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 60.0));
        let quads = nine_slice_quads(dest, [40.0, 30.0], [10.0, 5.0, 10.0, 5.0]);
        assert_eq!(quads.len(), 9);
        let (corner_rect, corner_uv) = quads[0];
        assert_eq!(
            corner_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(10.0, 5.0))
        );
        assert_eq!(
            corner_uv,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(0.25, 5.0 / 30.0))
        );
        let (center_rect, center_uv) = quads[4];
        assert_eq!(
            center_rect,
            Rect::from_min_max(pos2(10.0, 5.0), pos2(90.0, 55.0))
        );
        assert_eq!(
            center_uv,
            Rect::from_min_max(pos2(0.25, 5.0 / 30.0), pos2(0.75, 25.0 / 30.0))
        );
        let (last_rect, last_uv) = quads[8];
        assert_eq!(last_rect.max, pos2(100.0, 60.0));
        assert_eq!(last_uv.max, pos2(1.0, 1.0));
    }

    #[test]
    fn nine_slice_collapses_middle_when_area_is_smaller_than_borders() {
        let dest = Rect::from_min_max(pos2(0.0, 0.0), pos2(10.0, 10.0));
        let quads = nine_slice_quads(dest, [40.0, 40.0], [8.0, 8.0, 8.0, 8.0]);
        let (middle, _) = quads[4];
        assert!(middle.width().abs() < 1e-4 && middle.height().abs() < 1e-4);
        assert_eq!(quads[8].0.max, pos2(10.0, 10.0));
    }

    #[test]
    fn password_content_is_masked_per_character() {
        assert_eq!(display_text(ContentType::Password, "p@s"), "•••");
        assert_eq!(display_text(ContentType::Standard, "p@s"), "p@s");
    }

    #[test]
    fn batches_split_by_texture_in_draw_order() {
        let (mut system, _) = system_with(PANEL_IMAGE_TEXT);
        system.register_image("/ui/icon.png", [64, 64]).unwrap();
        let paint = system.paint();
        let textures: Vec<&UiTexture> = paint
            .draw_list
            .batches
            .iter()
            .map(|batch| &batch.texture)
            .collect();
        assert_eq!(
            textures.first(),
            Some(&&UiTexture::FontAtlas),
            "панель рисуется первой"
        );
        assert!(textures.contains(&&UiTexture::Image("ui/icon.png".to_owned())));
        assert_eq!(
            textures.last(),
            Some(&&UiTexture::FontAtlas),
            "текст поверх картинки"
        );
        let mut expected_start = 0;
        for batch in &paint.draw_list.batches {
            assert_eq!(
                batch.indices.start, expected_start,
                "батчи покрывают индексы без пропусков"
            );
            expected_start = batch.indices.end;
        }
        assert_eq!(expected_start as usize, paint.draw_list.indices.len());
    }

    #[test]
    fn image_is_skipped_until_its_size_is_registered() {
        let (mut system, _) = system_with(PANEL_IMAGE_TEXT);
        let paint = system.paint();
        assert!(
            !paint
                .draw_list
                .batches
                .iter()
                .any(|batch| matches!(batch.texture, UiTexture::Image(_)))
        );
    }

    #[test]
    fn nested_scroll_view_clip_is_intersection_in_physical_pixels() {
        // Масштаб 2: окно (10,10)-(60,50) в единицах канваса → (20,20)-(120,100) в пикселях.
        let mut system = UiSystem::default();
        let document = Arc::new(
            UiDocument::parse(
                "scale.rveui",
                r#"
[canvas]
scale = 2.0

[[elements]]
id = "view"
scroll_view = { vertical = true }
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_min = [10.0, 10.0], offset_max = [50.0, 40.0] }

[[elements]]
id = "content"
parent = "view"
panel = { color = [255, 255, 255, 255] }
rect = { anchor_min = [0.0, 0.0], anchor_max = [1.0, 0.0], pivot = [0.0, 0.0], offset_min = [0.0, 0.0], offset_max = [0.0, 200.0] }
"#,
            )
            .unwrap(),
        );
        system.open(document, 1);
        system.update(&UiInput::default(), 0.0, screen());
        let paint = system.paint();
        let clips: Vec<[f32; 4]> = paint
            .draw_list
            .batches
            .iter()
            .map(|batch| batch.clip_rect)
            .collect();
        assert!(
            clips.contains(&[20.0, 20.0, 120.0, 100.0]),
            "клип: {clips:?}"
        );
    }

    #[test]
    fn unchanged_state_returns_same_draw_list_and_atlas() {
        let (mut system, _) = system_with(PANEL_IMAGE_TEXT);
        let first = system.paint();
        let second = system.paint();
        assert!(Arc::ptr_eq(&first.draw_list, &second.draw_list));
        assert!(Arc::ptr_eq(&first.atlas, &second.atlas));
    }

    #[test]
    fn same_text_rebuilds_draw_list_but_keeps_atlas() {
        let (mut system, canvas) = system_with(PANEL_IMAGE_TEXT);
        let label = system.find(canvas, "label").unwrap();
        let first = system.paint();
        // Элемент помечен изменённым, но глифы уже в кэше: атлас не меняется.
        system.set_text(label, "Привет").unwrap();
        let second = system.paint();
        assert!(
            !Arc::ptr_eq(&first.draw_list, &second.draw_list),
            "пересчитан список"
        );
        assert!(Arc::ptr_eq(&first.atlas, &second.atlas), "атлас тот же Arc");
        assert_eq!(first.atlas.version, second.atlas.version);
    }

    #[test]
    fn new_glyphs_bump_atlas_version() {
        let (mut system, canvas) = system_with(PANEL_IMAGE_TEXT);
        let label = system.find(canvas, "label").unwrap();
        let first = system.paint();
        system.set_text(label, "QWXZ").unwrap();
        let second = system.paint();
        assert!(second.atlas.version > first.atlas.version);
        assert_ne!(first.atlas.pixels, second.atlas.pixels);
    }

    #[test]
    fn invalid_font_is_rejected_without_panic_and_not_kept() {
        let mut system = UiSystem::default();
        let error = system
            .register_font("/fonts/broken.ttf", vec![1, 2, 3, 4])
            .unwrap_err();
        assert!(matches!(error, UiSystemError::InvalidValue(_)));
        assert!(system.font_data.is_empty());
    }

    #[test]
    fn font_and_image_extensions_and_sizes_are_checked() {
        let mut system = UiSystem::default();
        assert!(system.register_font("/ui/hud.png", vec![]).is_err());
        assert!(system.register_image("/ui/hud.ttf", [1, 1]).is_err());
        assert!(system.register_image("/ui/hud.png", [0, 1]).is_err());
    }

    #[test]
    fn focused_input_adds_caret_and_password_is_still_stored_plain() {
        let (mut system, canvas) = system_with(
            r#"
[[elements]]
id = "pw"
input_field = { text = "secret", content = "password" }
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_min = [0.0, 0.0], offset_max = [200.0, 30.0] }
"#,
        );
        let pw = system.find(canvas, "pw").unwrap();
        let unfocused = system.paint();
        system.focus(pw).unwrap();
        let focused = system.paint();
        assert!(
            focused.draw_list.vertices.len() > unfocused.draw_list.vertices.len(),
            "каретка добавляет геометрию"
        );
        assert_eq!(
            system.value(pw).unwrap(),
            Some(UiValue::Text("secret".into()))
        );
    }
}
