//! Layout элементов: масштаб канваса, якоря/pivot/offsets, группы раскладки и clip ScrollView.
//! Функции чистые: результат зависит только от документа, размера экрана и смещений прокрутки.

use crate::document::{
    CanvasSettings, ChildAlign, Corner, ElementKind, GridSpec, RectSpec, ScaleMode, StackSpec,
    Tree, UiElement,
};
use epaint::{Pos2, Rect, Vec2, pos2, vec2};

/// Физических пикселей на единицу канваса. Нулевой или отрицательный экран считается 1 пикселем,
/// чтобы масштаб оставался конечным; рисовать такой кадр не нужно.
pub fn canvas_scale(settings: &CanvasSettings, screen_px: Vec2) -> f32 {
    match settings.scale_mode {
        ScaleMode::Constant => settings.scale,
        ScaleMode::Screen => {
            let screen = screen_px.max(Vec2::splat(1.0));
            let [reference_width, reference_height] = settings.reference_resolution;
            let width = (screen.x / reference_width).log2();
            let height = (screen.y / reference_height).log2();
            let match_factor = settings.match_factor;
            2f32.powf((1.0 - match_factor) * width + match_factor * height)
        }
    }
}

/// Размер канваса в его единицах при заданном масштабе.
pub fn canvas_size(screen_px: Vec2, scale: f32) -> Vec2 {
    screen_px / scale
}

/// Прямоугольник элемента в координатах родителя по его [`RectSpec`].
/// Pivot на результат не влияет: границы всегда `якорь + offset` на каждой оси.
pub fn resolve_rect(spec: &RectSpec, parent: Rect) -> Rect {
    let (min_x, max_x) = resolve_axis(
        [spec.anchor_min[0], spec.anchor_max[0]],
        [spec.offset_min[0], spec.offset_max[0]],
        [parent.min.x, parent.max.x],
    );
    let (min_y, max_y) = resolve_axis(
        [spec.anchor_min[1], spec.anchor_max[1]],
        [spec.offset_min[1], spec.offset_max[1]],
        [parent.min.y, parent.max.y],
    );
    Rect::from_min_max(pos2(min_x, min_y), pos2(max_x, max_y))
}

fn resolve_axis(anchors: [f32; 2], offsets: [f32; 2], parent: [f32; 2]) -> (f32, f32) {
    let size = parent[1] - parent[0];
    (
        parent[0] + anchors[0] * size + offsets[0],
        parent[0] + anchors[1] * size + offsets[1],
    )
}

fn anchor_point(parent: Rect, anchor: [f32; 2]) -> Pos2 {
    parent.min + parent.size() * vec2(anchor[0], anchor[1])
}

/// Помощники редактора: позиция pivot и размер вместо offsets. Обратны друг другу.
impl RectSpec {
    /// Позиция pivot и размер прямоугольника в координатах `parent`.
    pub fn position_size(&self, parent: Rect) -> (Pos2, Vec2) {
        let rect = resolve_rect(self, parent);
        (
            rect.min + rect.size() * vec2(self.pivot[0], self.pivot[1]),
            rect.size(),
        )
    }

    /// Пересчитывает offsets так, чтобы pivot оказался в `pos`, а размер стал `size` (не меньше нуля).
    /// Якоря не меняются; на растянутой оси размер задаёт отступы этой оси.
    pub fn set_position_size(&mut self, parent: Rect, pos: Pos2, size: Vec2) {
        let size = size.max(Vec2::ZERO);
        let min = pos - size * vec2(self.pivot[0], self.pivot[1]);
        let max = min + size;
        let offset_min = min - anchor_point(parent, self.anchor_min);
        let offset_max = max - anchor_point(parent, self.anchor_max);
        self.offset_min = [offset_min.x, offset_min.y];
        self.offset_max = [offset_max.x, offset_max.y];
    }
}

/// Размер прокрутки: на сколько содержимое больше окна ScrollView, не меньше нуля.
pub fn max_scroll(view: Rect, content: Rect) -> Vec2 {
    (content.size() - view.size()).max(Vec2::ZERO)
}

/// Результат layout одного канваса; все массивы индексируются индексом элемента документа.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Layout {
    /// Прямоугольники видимых элементов в координатах канваса; у скрытых — нулевые.
    pub rects: Vec<Rect>,
    /// Область отсечения: пересечение окон всех предков ScrollView, по умолчанию — канвас.
    pub clips: Vec<Rect>,
    /// Видимость с учётом предков.
    pub visible: Vec<bool>,
    /// Интерактивность с учётом предков; скрытый элемент не интерактивен.
    pub interactive: Vec<bool>,
    /// Видимые элементы в порядке отрисовки: предок раньше потомков, соседи — по порядку документа.
    pub order: Vec<usize>,
}

/// Строит layout канваса. `scroll` — смещение содержимого каждого ScrollView по индексу элемента
/// (для остальных элементов значение не используется); длина равна числу элементов.
pub fn layout(tree: &Tree, elements: &[UiElement], canvas_size: Vec2, scroll: &[Vec2]) -> Layout {
    debug_assert_eq!(scroll.len(), elements.len());
    let count = elements.len();
    let canvas = Rect::from_min_size(pos2(0.0, 0.0), canvas_size);
    let mut pass = Pass {
        tree,
        elements,
        scroll,
        out: Layout {
            rects: vec![Rect::ZERO; count],
            clips: vec![canvas; count],
            visible: vec![false; count],
            interactive: vec![false; count],
            order: Vec::new(),
        },
    };
    for &root in &tree.roots {
        let rect = resolve_rect(&elements[root].rect, canvas);
        pass.place(root, rect, canvas, true, true);
    }
    pass.out
}

struct Pass<'a> {
    tree: &'a Tree,
    elements: &'a [UiElement],
    scroll: &'a [Vec2],
    out: Layout,
}

impl Pass<'_> {
    fn place(
        &mut self,
        index: usize,
        rect: Rect,
        clip: Rect,
        parent_visible: bool,
        parent_interactive: bool,
    ) {
        let element = &self.elements[index];
        self.out.rects[index] = rect;
        self.out.clips[index] = clip;
        let visible = parent_visible && element.visible;
        self.out.visible[index] = visible;
        if !visible {
            return;
        }
        let interactive = parent_interactive && element.interactable;
        self.out.interactive[index] = interactive;
        self.out.order.push(index);

        let children = &self.tree.children[index];
        match &element.kind {
            ElementKind::ScrollView(_) => {
                // Единственный ребёнок — содержимое: его смещает прокрутка, обрезает окно.
                let content_clip = clip.intersect(rect);
                for &child in children {
                    let content = resolve_rect(&self.elements[child].rect, rect)
                        .translate(-self.scroll[index]);
                    self.place(child, content, content_clip, true, interactive);
                }
            }
            ElementKind::Grid(spec) => {
                let inner = rect.shrink(spec.padding);
                for (child, child_rect) in arrange_grid(children, self.elements, inner, spec) {
                    self.place(child, child_rect, clip, true, interactive);
                }
            }
            ElementKind::Horizontal(spec) => {
                let inner = rect.shrink(spec.padding);
                for (child, child_rect) in
                    arrange_stack(children, self.elements, inner, spec, false)
                {
                    self.place(child, child_rect, clip, true, interactive);
                }
            }
            ElementKind::Vertical(spec) => {
                let inner = rect.shrink(spec.padding);
                for (child, child_rect) in arrange_stack(children, self.elements, inner, spec, true)
                {
                    self.place(child, child_rect, clip, true, interactive);
                }
            }
            _ => {
                for &child in children {
                    let child_rect = resolve_rect(&self.elements[child].rect, rect);
                    self.place(child, child_rect, clip, true, interactive);
                }
            }
        }
    }
}

/// Раскладка по сетке: дети идут по строкам от угла `start_corner`; размер клетки — `cell_size`.
fn arrange_grid(
    children: &[usize],
    elements: &[UiElement],
    inner: Rect,
    spec: &GridSpec,
) -> Vec<(usize, Rect)> {
    let visible = children
        .iter()
        .copied()
        .filter(|&child| elements[child].visible);
    let [cell_width, cell_height] = spec.cell_size;
    let [spacing_x, spacing_y] = spec.spacing;
    let columns = spec.columns as usize;
    let from_left = matches!(spec.start_corner, Corner::TopLeft | Corner::BottomLeft);
    let from_top = matches!(spec.start_corner, Corner::TopLeft | Corner::TopRight);
    visible
        .enumerate()
        .map(|(slot, child)| {
            let column = (slot % columns) as f32;
            let row = (slot / columns) as f32;
            let x = if from_left {
                inner.min.x + column * (cell_width + spacing_x)
            } else {
                inner.max.x - cell_width - column * (cell_width + spacing_x)
            };
            let y = if from_top {
                inner.min.y + row * (cell_height + spacing_y)
            } else {
                inner.max.y - cell_height - row * (cell_height + spacing_y)
            };
            (
                child,
                Rect::from_min_size(pos2(x, y), vec2(cell_width, cell_height)),
            )
        })
        .collect()
}

/// Раскладка в строку (`vertical = false`) или столбец. Размер по главной оси — натуральный
/// размер ребёнка; по поперечной — `child_align`. Группа переопределяет rect детей.
fn arrange_stack(
    children: &[usize],
    elements: &[UiElement],
    inner: Rect,
    spec: &StackSpec,
    vertical: bool,
) -> Vec<(usize, Rect)> {
    let visible: Vec<usize> = children
        .iter()
        .copied()
        .filter(|&child| elements[child].visible)
        .collect();
    if visible.is_empty() {
        return Vec::new();
    }
    let natural: Vec<Rect> = visible
        .iter()
        .map(|&child| resolve_rect(&elements[child].rect, inner))
        .collect();
    let main = |size: Vec2| if vertical { size.y } else { size.x };
    let cross = |size: Vec2| if vertical { size.x } else { size.y };
    let main_available = if vertical {
        inner.height()
    } else {
        inner.width()
    };
    let cross_available = if vertical {
        inner.width()
    } else {
        inner.height()
    };
    let gaps = spec.spacing * (visible.len() - 1) as f32;
    let used: f32 = natural.iter().map(|rect| main(rect.size())).sum::<f32>() + gaps;
    let extra = main_available - used;
    let bonus = if spec.expand && extra > 0.0 {
        extra / visible.len() as f32
    } else {
        0.0
    };

    let mut cursor = if vertical { inner.min.y } else { inner.min.x };
    let cross_start = if vertical { inner.min.x } else { inner.min.y };
    visible
        .into_iter()
        .zip(natural)
        .map(|(child, rect)| {
            let main_size = main(rect.size()) + bonus;
            let (cross_min, cross_size) = match spec.child_align {
                ChildAlign::Stretch => (cross_start, cross_available),
                ChildAlign::Start => (cross_start, cross(rect.size())),
                ChildAlign::Center => (
                    cross_start + (cross_available - cross(rect.size())) / 2.0,
                    cross(rect.size()),
                ),
                ChildAlign::End => (
                    cross_start + cross_available - cross(rect.size()),
                    cross(rect.size()),
                ),
            };
            let placed = if vertical {
                Rect::from_min_size(pos2(cross_min, cursor), vec2(cross_size, main_size))
            } else {
                Rect::from_min_size(pos2(cursor, cross_min), vec2(main_size, cross_size))
            };
            cursor += main_size + spec.spacing;
            (child, placed)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::UiDocument;

    fn document(input: &str) -> UiDocument {
        UiDocument::parse("layout-test.rveui", input).unwrap()
    }

    fn run(document: &UiDocument, size: Vec2) -> Layout {
        let scroll = vec![Vec2::ZERO; document.elements().len()];
        layout(document.tree(), document.elements(), size, &scroll)
    }

    fn assert_rect(actual: Rect, min: [f32; 2], size: [f32; 2]) {
        let expected = Rect::from_min_size(pos2(min[0], min[1]), vec2(size[0], size[1]));
        assert!(
            (actual.min - expected.min).length() < 1e-3
                && (actual.size() - expected.size()).length() < 1e-3,
            "rect {actual:?} != {expected:?}"
        );
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 1e-3, "{actual} != {expected}");
    }

    #[test]
    fn constant_scale_divides_screen_into_canvas_units() {
        let settings = CanvasSettings {
            scale: 2.0,
            ..CanvasSettings::default()
        };
        let scale = canvas_scale(&settings, vec2(800.0, 600.0));
        assert_close(scale, 2.0);
        assert_eq!(canvas_size(vec2(800.0, 600.0), scale), vec2(400.0, 300.0));
    }

    #[test]
    fn screen_scale_matches_width_height_and_blend() {
        let mut settings = CanvasSettings {
            scale_mode: ScaleMode::Screen,
            reference_resolution: [800.0, 600.0],
            ..CanvasSettings::default()
        };
        let screen = vec2(1600.0, 600.0);
        settings.match_factor = 0.0;
        assert_close(canvas_scale(&settings, screen), 2.0);
        settings.match_factor = 1.0;
        assert_close(canvas_scale(&settings, screen), 1.0);
        settings.match_factor = 0.5;
        assert_close(canvas_scale(&settings, screen), 2f32.sqrt());
    }

    #[test]
    fn screen_scale_stays_finite_for_zero_screen() {
        let settings = CanvasSettings {
            scale_mode: ScaleMode::Screen,
            ..CanvasSettings::default()
        };
        let scale = canvas_scale(&settings, Vec2::ZERO);
        assert!(scale.is_finite() && scale > 0.0);
    }

    #[test]
    fn point_anchor_places_min_and_max_from_offsets() {
        let parent = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        let centered = RectSpec {
            anchor_min: [0.5, 0.5],
            anchor_max: [0.5, 0.5],
            pivot: [0.5, 0.5],
            offset_min: [-20.0, -10.0],
            offset_max: [20.0, 10.0],
        };
        assert_rect(resolve_rect(&centered, parent), [80.0, 40.0], [40.0, 20.0]);

        let bottom_right = RectSpec {
            anchor_min: [1.0, 1.0],
            anchor_max: [1.0, 1.0],
            pivot: [1.0, 1.0],
            offset_min: [-60.0, -40.0],
            offset_max: [-10.0, -10.0],
        };
        assert_rect(
            resolve_rect(&bottom_right, parent),
            [140.0, 60.0],
            [50.0, 30.0],
        );
    }

    #[test]
    fn pivot_does_not_change_resolved_rect() {
        let parent = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        let spec = |pivot| RectSpec {
            anchor_min: [0.5, 0.5],
            anchor_max: [0.5, 0.5],
            pivot,
            offset_min: [-20.0, -10.0],
            offset_max: [20.0, 10.0],
        };
        assert_eq!(
            resolve_rect(&spec([0.0, 0.0]), parent),
            resolve_rect(&spec([1.0, 1.0]), parent)
        );
    }

    #[test]
    fn position_size_reports_pivot_point_and_size() {
        let parent = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        let corner = RectSpec {
            anchor_min: [1.0, 1.0],
            anchor_max: [1.0, 1.0],
            pivot: [1.0, 1.0],
            offset_min: [-60.0, -40.0],
            offset_max: [-10.0, -10.0],
        };
        let (pos, size) = corner.position_size(parent);
        assert_eq!(pos, pos2(190.0, 90.0));
        assert_eq!(size, vec2(50.0, 30.0));
    }

    #[test]
    fn set_position_size_round_trips_on_point_anchor() {
        let parent = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        let mut spec = RectSpec {
            anchor_min: [0.5, 0.5],
            anchor_max: [0.5, 0.5],
            pivot: [0.5, 0.5],
            offset_min: [-20.0, -10.0],
            offset_max: [20.0, 10.0],
        };
        spec.set_position_size(parent, pos2(50.0, 30.0), vec2(60.0, 40.0));
        assert_eq!(
            spec.position_size(parent),
            (pos2(50.0, 30.0), vec2(60.0, 40.0))
        );
        assert_rect(resolve_rect(&spec, parent), [20.0, 10.0], [60.0, 40.0]);
        assert_eq!(spec.anchor_min, [0.5, 0.5]);

        spec.set_position_size(parent, pos2(50.0, 30.0), vec2(-5.0, 10.0));
        assert_eq!(spec.position_size(parent).1, vec2(0.0, 10.0));
    }

    #[test]
    fn set_position_size_on_stretched_axis_sets_insets() {
        let parent = Rect::from_min_size(pos2(0.0, 0.0), vec2(200.0, 100.0));
        let mut spec = RectSpec {
            anchor_min: [0.0, 0.0],
            anchor_max: [1.0, 0.0],
            pivot: [0.0, 0.0],
            offset_min: [10.0, 5.0],
            offset_max: [-10.0, 25.0],
        };
        let (pos, size) = spec.position_size(parent);
        assert_eq!((pos, size), (pos2(10.0, 5.0), vec2(180.0, 20.0)));

        spec.set_position_size(parent, pos2(30.0, 5.0), vec2(100.0, 20.0));
        assert_eq!(spec.offset_min, [30.0, 5.0]);
        assert_eq!(spec.offset_max, [-70.0, 25.0]);
        assert_rect(resolve_rect(&spec, parent), [30.0, 5.0], [100.0, 20.0]);
    }

    #[test]
    fn stretch_anchor_insets_parent() {
        let parent = Rect::from_min_size(pos2(10.0, 20.0), vec2(200.0, 100.0));
        let inset = RectSpec {
            anchor_min: [0.0, 0.0],
            anchor_max: [1.0, 1.0],
            pivot: [0.5, 0.5],
            offset_min: [5.0, 6.0],
            offset_max: [-15.0, -26.0],
        };
        assert_rect(resolve_rect(&inset, parent), [15.0, 26.0], [180.0, 68.0]);
    }

    #[test]
    fn mixed_axes_stretch_horizontally_and_pin_vertically() {
        let parent = Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 300.0));
        let bar = RectSpec {
            anchor_min: [0.0, 1.0],
            anchor_max: [1.0, 1.0],
            pivot: [0.0, 1.0],
            offset_min: [10.0, -40.0],
            offset_max: [-10.0, -10.0],
        };
        let rect = resolve_rect(&bar, parent);
        assert_rect(rect, [10.0, 260.0], [280.0, 30.0]);
    }

    #[test]
    fn layout_positions_nested_elements_and_skips_hidden() {
        let document = document(
            r#"
[[elements]]
id = "root"
panel = {}

[[elements]]
id = "child"
parent = "root"
panel = {}
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [15.0, 15.0], offset_min = [5.0, 5.0] }

[[elements]]
id = "hidden"
parent = "root"
panel = {}
visible = false

[[elements]]
id = "grandchild"
parent = "child"
panel = {}
rect = { anchor_min = [0.0, 0.0], anchor_max = [1.0, 1.0] }
"#,
        );
        let result = run(&document, vec2(100.0, 100.0));
        let child = document.element_index("child").unwrap();
        let hidden = document.element_index("hidden").unwrap();
        let grandchild = document.element_index("grandchild").unwrap();
        assert_rect(result.rects[child], [5.0, 5.0], [10.0, 10.0]);
        assert_rect(result.rects[grandchild], [5.0, 5.0], [10.0, 10.0]);
        assert!(!result.visible[hidden]);
        assert!(!result.order.contains(&hidden));
        assert_eq!(result.order, vec![0, child, grandchild]);
    }

    #[test]
    fn hidden_parent_hides_subtree_and_interaction() {
        let document = document(
            r#"
[[elements]]
id = "root"
panel = {}
visible = false

[[elements]]
id = "button"
parent = "root"
button = {}
"#,
        );
        let result = run(&document, vec2(100.0, 100.0));
        let button = document.element_index("button").unwrap();
        assert!(!result.visible[button]);
        assert!(!result.interactive[button]);
        assert!(result.order.is_empty());
    }

    #[test]
    fn non_interactable_parent_disables_children_interaction() {
        let document = document(
            r#"
[[elements]]
id = "panel"
panel = {}
interactable = false

[[elements]]
id = "button"
parent = "panel"
button = {}
"#,
        );
        let result = run(&document, vec2(100.0, 100.0));
        let button = document.element_index("button").unwrap();
        assert!(result.visible[button]);
        assert!(!result.interactive[button]);
    }

    #[test]
    fn horizontal_group_packs_children_and_expands_with_padding() {
        let document = document(
            r#"
[[elements]]
id = "bar"
horizontal = { spacing = 10.0, padding = 5.0, child_align = "center", expand = true }
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [200.0, 50.0] }

[[elements]]
id = "a"
parent = "bar"
panel = {}
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [20.0, 10.0] }

[[elements]]
id = "b"
parent = "bar"
panel = {}
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [30.0, 20.0] }
"#,
        );
        let result = run(&document, vec2(300.0, 300.0));
        let a = document.element_index("a").unwrap();
        let b = document.element_index("b").unwrap();
        // Внутренняя область 190 × 40 от (5, 5); лишнее место 190 - 50 - 10 = 130 делится пополам.
        assert_rect(
            result.rects[a],
            [5.0, 5.0 + (40.0 - 10.0) / 2.0],
            [20.0 + 65.0, 10.0],
        );
        assert_rect(
            result.rects[b],
            [5.0 + 85.0 + 10.0, 5.0 + (40.0 - 20.0) / 2.0],
            [30.0 + 65.0, 20.0],
        );
    }

    #[test]
    fn vertical_group_stretches_cross_axis_and_ignores_hidden_children() {
        let document = document(
            r#"
[[elements]]
id = "column"
vertical = { spacing = 4.0, child_align = "stretch" }
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [80.0, 100.0] }

[[elements]]
id = "first"
parent = "column"
panel = {}
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [10.0, 30.0] }

[[elements]]
id = "gone"
parent = "column"
panel = {}
visible = false

[[elements]]
id = "second"
parent = "column"
panel = {}
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [10.0, 20.0] }
"#,
        );
        let result = run(&document, vec2(300.0, 300.0));
        let first = document.element_index("first").unwrap();
        let second = document.element_index("second").unwrap();
        assert_rect(result.rects[first], [0.0, 0.0], [80.0, 30.0]);
        assert_rect(result.rects[second], [0.0, 34.0], [80.0, 20.0]);
    }

    #[test]
    fn grid_fills_rows_from_corner() {
        let document = document(
            r#"
[[elements]]
id = "grid"
grid = { cell_size = [10.0, 5.0], spacing = [2.0, 1.0], columns = 2, start_corner = "bottom_right" }
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [100.0, 100.0] }

[[elements]]
id = "c0"
parent = "grid"
panel = {}

[[elements]]
id = "c1"
parent = "grid"
panel = {}

[[elements]]
id = "c2"
parent = "grid"
panel = {}
"#,
        );
        let result = run(&document, vec2(300.0, 300.0));
        let c0 = document.element_index("c0").unwrap();
        let c1 = document.element_index("c1").unwrap();
        let c2 = document.element_index("c2").unwrap();
        // Первая клетка в правом нижнем углу: 100 - 10 = 90, 100 - 5 = 95.
        assert_rect(result.rects[c0], [90.0, 95.0], [10.0, 5.0]);
        assert_rect(result.rects[c1], [78.0, 95.0], [10.0, 5.0]);
        assert_rect(result.rects[c2], [90.0, 89.0], [10.0, 5.0]);
    }

    #[test]
    fn scroll_view_offsets_content_and_clips_children() {
        let document = document(
            r#"
[[elements]]
id = "view"
scroll_view = { vertical = true }
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [50.0, 40.0] }

[[elements]]
id = "content"
parent = "view"
vertical = {}
rect = { anchor_min = [0.0, 0.0], anchor_max = [1.0, 0.0], pivot = [0.0, 0.0], offset_max = [0.0, 100.0] }

[[elements]]
id = "row"
parent = "content"
panel = {}
rect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_max = [10.0, 10.0] }
"#,
        );
        let view = document.element_index("view").unwrap();
        let content = document.element_index("content").unwrap();
        let row = document.element_index("row").unwrap();
        let mut scroll = vec![Vec2::ZERO; document.elements().len()];
        scroll[view] = vec2(0.0, 25.0);
        let result = layout(
            document.tree(),
            document.elements(),
            vec2(300.0, 300.0),
            &scroll,
        );

        assert_rect(result.rects[content], [0.0, -25.0], [50.0, 100.0]);
        assert_rect(result.rects[row], [0.0, -25.0], [10.0, 10.0]);
        let view_rect = result.rects[view];
        assert_eq!(result.clips[content], view_rect);
        assert_eq!(result.clips[row], view_rect);
        assert_eq!(
            result.clips[view],
            Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 300.0))
        );

        let range = max_scroll(
            view_rect,
            resolve_rect(&document.elements()[content].rect, view_rect),
        );
        assert_eq!(range, vec2(0.0, 60.0));
    }

    #[test]
    fn demo_hud_places_title_top_left_and_fire_bottom_right() {
        let document = UiDocument::parse(
            "hud.rveui",
            include_str!("../../../examples/demo-game/Assets/ui/hud.rveui"),
        )
        .unwrap();
        let result = run(&document, vec2(1280.0, 720.0));
        let title = document.element_index("title").unwrap();
        let fire = document.element_index("fire").unwrap();
        assert_rect(result.rects[title], [24.0, 24.0], [360.0, 48.0]);
        assert_rect(result.rects[fire], [1056.0, 640.0], [200.0, 56.0]);
    }

    #[test]
    fn max_scroll_is_never_negative() {
        let view = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, 100.0));
        let small = Rect::from_min_size(pos2(0.0, 0.0), vec2(40.0, 40.0));
        assert_eq!(max_scroll(view, small), Vec2::ZERO);
    }
}
