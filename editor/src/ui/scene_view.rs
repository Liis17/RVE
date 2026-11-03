//! Панель Scene: изображение сцены, камера редактора, выделение и гизмо.

use crate::state::{AssetDrag, EditorState};
use crate::viewport::camera::{Ray, ground_point, project_point, screen_ray};
use crate::viewport::gizmo::{DragTarget, GizmoDrag, GizmoTool, gizmo_axes};
use crate::viewport::picking::{nearest_hit, ray_model};
use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use glam::{Mat4, Vec3};

const AXIS_COLORS: [Color32; 3] = [
    Color32::from_rgb(230, 70, 70),
    Color32::from_rgb(90, 200, 90),
    Color32::from_rgb(80, 130, 240),
];
const ACTIVE_COLOR: Color32 = Color32::from_rgb(250, 220, 90);
/// Длина ручки — доля расстояния до камеры: гизмо сохраняет размер на экране.
const HANDLE_SCREEN_FRACTION: f32 = 0.18;
const HANDLE_HIT_POINTS: f32 = 8.0;
const MARKER_HIT_POINTS: f32 = 12.0;
const RING_SEGMENTS: usize = 64;
const FLY_UNITS_PER_SECOND: f32 = 6.0;
/// Брошенная модель ложится на пол не дальше этого числа расстояний до цели камеры.
const MAX_DROP_DISTANCE_FACTOR: f32 = 10.0;

/// Точка на экране внутри прямоугольника панели.
fn to_screen(view_projection: Mat4, rect: Rect, point: Vec3) -> Option<Pos2> {
    let [u, v] = project_point(view_projection, point)?;
    Some(rect.min + Vec2::new(u * rect.width(), v * rect.height()))
}

fn pointer_ray(view_projection: Mat4, rect: Rect, pointer: Pos2) -> Option<Ray> {
    let local = pointer - rect.min;
    screen_ray(
        view_projection,
        local.x / rect.width().max(1.0),
        local.y / rect.height().max(1.0),
    )
}

fn distance_to_segment(point: Pos2, a: Pos2, b: Pos2) -> f32 {
    let segment = b - a;
    let length_squared = segment.length_sq();
    if length_squared < 1e-3 {
        return point.distance(a);
    }
    let t = ((point - a).dot(segment) / length_squared).clamp(0.0, 1.0);
    point.distance(a + segment * t)
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, Sense::click_and_drag());
    let pixels_per_point = ui.ctx().pixels_per_point();
    state.scene_view.visible = true;
    state.scene_view.hovered = response.contains_pointer();
    state.scene_view.size = [
        (rect.width() * pixels_per_point).round().max(0.0) as u32,
        (rect.height() * pixels_per_point).round().max(0.0) as u32,
    ];
    if rect.width() < 1.0 || rect.height() < 1.0 {
        state.scene_view.visible = false;
        return;
    }
    if let Some(texture) = state.viewport_texture {
        ui.painter().image(
            texture,
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    let [width, height] = state.scene_view.size;

    if state.is_playing() {
        feed_play_pointer(ui, state, rect, pixels_per_point);
        return;
    }

    handle_camera(ui, state, &response, rect);
    let view_projection = state.camera.camera().view_projection(width, height);
    if let Some(asset) = response.dnd_release_payload::<AssetDrag>() {
        let drop_point = ui
            .input(|input| input.pointer.latest_pos())
            .and_then(|pointer| pointer_ray(view_projection, rect, pointer))
            .and_then(|ray| ground_point(ray, state.camera.distance * MAX_DROP_DISTANCE_FACTOR))
            .unwrap_or(state.camera.target);
        state.create_from_asset(&asset.0, None, drop_point.to_array());
    }

    let Some(markers) = collect_markers(state) else {
        return;
    };
    draw_markers(ui, &markers, view_projection, rect);
    draw_selection_bounds(ui, state, &markers, view_projection, rect);
    let gizmo_taken = handle_gizmo(ui, state, &response, view_projection, rect);
    if !gizmo_taken
        && response.clicked()
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let modifiers = ui.input(|input| input.modifiers);
        pick(state, &markers, view_projection, rect, pointer, modifiers);
    }
}

/// Передаёт положение курсора и прокрутку над сценой в ввод скриптов.
pub(super) fn feed_play_pointer(
    ui: &egui::Ui,
    state: &mut EditorState,
    rect: Rect,
    pixels_per_point: f32,
) {
    let (pointer, delta, scroll) = ui.input(|input| {
        (
            input.pointer.latest_pos(),
            input.pointer.delta(),
            input.smooth_scroll_delta,
        )
    });
    let Some(play) = &mut state.play else {
        return;
    };
    let input = play.input_mut();
    if let Some(pointer) = pointer
        && rect.contains(pointer)
    {
        let local = (pointer - rect.min) * pixels_per_point;
        input.set_cursor(local.x, local.y);
        input.add_mouse_delta(delta.x * pixels_per_point, delta.y * pixels_per_point);
        input.add_wheel(scroll.x, scroll.y);
    }
}

fn handle_camera(ui: &egui::Ui, state: &mut EditorState, response: &egui::Response, rect: Rect) {
    let delta = response.drag_delta();
    let secondary = response.dragged_by(egui::PointerButton::Secondary);
    let middle = response.dragged_by(egui::PointerButton::Middle);
    let alt = ui.input(|input| input.modifiers.alt);
    let primary_alt = alt && response.dragged_by(egui::PointerButton::Primary);
    let (forward, right, up, dt) = ui.input(|input| {
        let axis = |positive, negative| {
            f32::from(u8::from(input.key_down(positive)))
                - f32::from(u8::from(input.key_down(negative)))
        };
        (
            axis(egui::Key::W, egui::Key::S),
            axis(egui::Key::D, egui::Key::A),
            axis(egui::Key::E, egui::Key::Q),
            input.stable_dt.min(0.1),
        )
    });
    let flying = secondary && (forward != 0.0 || right != 0.0 || up != 0.0);
    if flying {
        state.camera.look(delta.x, delta.y);
        let step = FLY_UNITS_PER_SECOND * dt;
        state.camera.fly(forward * step, right * step, up * step);
        ui.ctx().request_repaint();
    } else if secondary || primary_alt {
        state.camera.orbit(delta.x, delta.y);
    } else if middle {
        state.camera.pan(delta.x, delta.y, rect.height());
    }
    if response.contains_pointer() {
        let scroll = ui.input(|input| input.smooth_scroll_delta.y);
        if scroll != 0.0 {
            state.camera.zoom(scroll);
        }
    }
}

/// Объект сцены с данными, нужными для выбора и маркеров.
struct Marker {
    id: String,
    world: Mat4,
    label: Option<&'static str>,
    model: Option<String>,
}

fn collect_markers(state: &mut EditorState) -> Option<Vec<Marker>> {
    let world = state.world_mut()?;
    world.recalculate_world_matrices();
    Some(
        world
            .objects()
            .iter()
            .filter_map(|object| {
                let matrix = world.world_matrix(object.handle)?;
                let label = if object.camera.is_some() {
                    Some("CAM")
                } else if object.directional_light.is_some() {
                    Some("SUN")
                } else {
                    None
                };
                Some(Marker {
                    id: object.id.clone(),
                    world: matrix,
                    label,
                    model: object
                        .mesh_renderer
                        .as_ref()
                        .map(|mesh| mesh.model.as_str().to_owned()),
                })
            })
            .collect(),
    )
}

fn draw_markers(ui: &egui::Ui, markers: &[Marker], view_projection: Mat4, rect: Rect) {
    let painter = ui.painter_at(rect);
    for marker in markers {
        let Some(label) = marker.label else {
            continue;
        };
        if let Some(position) = to_screen(view_projection, rect, marker.world.w_axis.truncate()) {
            painter.circle_filled(position, 13.0, Color32::from_black_alpha(150));
            painter.text(
                position,
                egui::Align2::CENTER_CENTER,
                label,
                egui::FontId::proportional(9.0),
                Color32::WHITE,
            );
        }
    }
}

/// Пары вершин рёбер параллелепипеда из `box_corners`.
const BOX_EDGES: [(usize, usize); 12] = [
    (0, 1),
    (1, 3),
    (3, 2),
    (2, 0),
    (4, 5),
    (5, 7),
    (7, 6),
    (6, 4),
    (0, 4),
    (1, 5),
    (2, 6),
    (3, 7),
];

/// Мировые вершины границ модели; бит 0 индекса выбирает X, бит 1 — Y, бит 2 — Z.
fn box_corners(matrix: Mat4, (min, max): ([f32; 3], [f32; 3])) -> [Vec3; 8] {
    std::array::from_fn(|index| {
        let pick = |axis: usize| {
            if index & (1 << axis) == 0 {
                min[axis]
            } else {
                max[axis]
            }
        };
        matrix.transform_point3(Vec3::new(pick(0), pick(1), pick(2)))
    })
}

/// Рамки границ моделей выделенных объектов. Рёбра с вершиной за камерой пропускаются.
fn draw_selection_bounds(
    ui: &egui::Ui,
    state: &EditorState,
    markers: &[Marker],
    view_projection: Mat4,
    rect: Rect,
) {
    let Some(scene) = &state.scene else {
        return;
    };
    let painter = ui.painter_at(rect);
    let stroke = Stroke::new(1.5, ACTIVE_COLOR);
    for marker in markers
        .iter()
        .filter(|marker| scene.is_selected(&marker.id))
    {
        let Some(bounds) = marker
            .model
            .as_ref()
            .and_then(|model| state.models.get(model))
            .and_then(|model| model.bounds())
        else {
            continue;
        };
        let corners = box_corners(marker.world, bounds)
            .map(|corner| to_screen(view_projection, rect, corner));
        for (a, b) in BOX_EDGES {
            if let (Some(a), Some(b)) = (corners[a], corners[b]) {
                painter.line_segment([a, b], stroke);
            }
        }
    }
}

fn pick(
    state: &mut EditorState,
    markers: &[Marker],
    view_projection: Mat4,
    rect: Rect,
    pointer: Pos2,
    modifiers: egui::Modifiers,
) {
    // Маркеры камеры и света не имеют геометрии и имеют приоритет: они рисуются поверх сцены.
    let marker = markers
        .iter()
        .filter(|marker| marker.label.is_some())
        .filter_map(|marker| {
            let position = to_screen(view_projection, rect, marker.world.w_axis.truncate())?;
            let distance = position.distance(pointer);
            (distance <= MARKER_HIT_POINTS).then_some((marker.id.clone(), distance))
        });
    let selected = nearest_hit(marker).or_else(|| {
        let ray = pointer_ray(view_projection, rect, pointer)?;
        nearest_hit(markers.iter().filter_map(|marker| {
            let model = state.models.get(marker.model.as_ref()?)?;
            Some((marker.id.clone(), ray_model(ray, model, marker.world)?))
        }))
    });
    if let Some(scene) = &mut state.scene {
        apply_click_selection(scene, selected.as_deref(), modifiers);
    }
}

/// Щелчок по объекту: Ctrl/Cmd переключает его в выделении, Shift добавляет,
/// без модификаторов выделение заменяется. Щелчок мимо объектов с модификатором ничего не снимает.
pub(super) fn apply_click_selection(
    scene: &mut crate::scene_edit::EditorScene,
    id: Option<&str>,
    modifiers: egui::Modifiers,
) {
    match id {
        Some(id) if modifiers.command => scene.toggle_selection(id),
        Some(id) if modifiers.shift => scene.add_to_selection(id),
        _ if modifiers.command || modifiers.shift => {}
        id => scene.select(id),
    }
}

/// Мировая матрица объекта и его родителя.
fn world_matrices(world: &engine_scene::World, id: &str) -> Option<(Mat4, Mat4)> {
    let object = world.object_by_id(id)?;
    let matrix = world.world_matrix(object.handle)?;
    let parent = object
        .parent
        .and_then(|parent| world.world_matrix(parent))
        .unwrap_or(Mat4::IDENTITY);
    Some((matrix, parent))
}

/// Рисует гизмо выделения и обрабатывает перетаскивание ручек.
/// Оси берутся у активного объекта, центр группы — среднее положение выделенных поддеревьев.
/// Возвращает `true`, если указатель занят гизмо и щелчок не должен менять выделение.
fn handle_gizmo(
    ui: &egui::Ui,
    state: &mut EditorState,
    response: &egui::Response,
    view_projection: Mat4,
    rect: Rect,
) -> bool {
    let Some((selection, starts)) = state.scene.as_ref().and_then(|scene| {
        let active = scene.selection()?.to_owned();
        let starts = scene
            .selection_roots()
            .into_iter()
            .filter_map(|id| Some((scene.object(&id)?.transform, id)))
            .collect::<Vec<_>>();
        Some((active, starts))
    }) else {
        // Выделение исчезло посреди перетаскивания: следующая правка — новая запись истории.
        if state.gizmo_drag.take().is_some()
            && let Some(scene) = &mut state.scene
        {
            scene.end_continuous_edit();
        }
        return false;
    };
    let Some(world) = state.world_mut() else {
        return false;
    };
    let Some((active_world, _)) = world_matrices(world, &selection) else {
        return false;
    };
    let targets = starts
        .into_iter()
        .filter_map(|(start, id)| {
            let (matrix, parent_world) = world_matrices(world, &id)?;
            Some(DragTarget {
                id,
                start,
                parent_world,
                world_position: matrix.w_axis.truncate(),
            })
        })
        .collect::<Vec<_>>();
    if targets.is_empty() {
        return false;
    }
    let (_, axes) = gizmo_axes(active_world, state.tool, state.space);
    let origin = targets
        .iter()
        .map(|target| target.world_position)
        .sum::<Vec3>()
        / targets.len() as f32;
    let eye = state.camera.eye();
    let handle = origin.distance(eye) * HANDLE_SCREEN_FRACTION;
    let pointer = ui.input(|input| input.pointer.latest_pos());

    // Завершение или продолжение активного перетаскивания.
    if let Some(drag) = state.gizmo_drag.clone() {
        let snapping = state.snap.enabled || ui.input(|input| input.modifiers.command);
        let step = snapping.then(|| state.snap.step(drag.tool));
        if !ui.input(|input| input.pointer.primary_down()) {
            state.gizmo_drag = None;
            if let Some(scene) = &mut state.scene {
                scene.end_continuous_edit();
            }
        } else if let Some(pointer) = pointer
            && let Some(ray) = pointer_ray(view_projection, rect, pointer)
            && let Some(transforms) = drag.update(ray, step)
        {
            let key = format!("{selection}/gizmo");
            state.edit_scene(|scene| scene.set_transforms(&transforms, Some(&key)));
        }
    }

    // Перетаскивание начинается с порогом; ручка определяется по точке нажатия, а не по курсору.
    let drag_starting = response.drag_started_by(egui::PointerButton::Primary);
    let press_origin = ui.input(|input| input.pointer.press_origin());
    let probe = if drag_starting { press_origin } else { pointer };
    let hovered = match (&state.gizmo_drag, probe) {
        (Some(drag), _) => Some(drag.axis_index),
        (None, Some(pointer))
            if rect.contains(pointer) && (drag_starting || !response.dragged()) =>
        {
            hovered_axis(
                state.tool,
                pointer,
                origin,
                &axes,
                handle,
                view_projection,
                rect,
            )
        }
        _ => None,
    };
    draw_gizmo(
        ui,
        state.tool,
        origin,
        &axes,
        handle,
        hovered,
        view_projection,
        rect,
    );

    if state.gizmo_drag.is_none()
        && drag_starting
        && !ui.input(|input| input.modifiers.alt)
        && let Some(axis_index) = hovered
        && let Some(pointer) = press_origin
        && let Some(ray) = pointer_ray(view_projection, rect, pointer)
    {
        state.gizmo_drag = GizmoDrag::begin(
            state.tool,
            axis_index,
            ray,
            (origin, axes[axis_index]),
            handle,
            targets,
        );
    }
    state.gizmo_drag.is_some() || hovered.is_some()
}

#[allow(clippy::too_many_arguments)]
fn hovered_axis(
    tool: GizmoTool,
    pointer: Pos2,
    origin: Vec3,
    axes: &[Vec3; 3],
    handle: f32,
    view_projection: Mat4,
    rect: Rect,
) -> Option<usize> {
    let mut best = None;
    let mut best_distance = HANDLE_HIT_POINTS;
    for (index, axis) in axes.iter().enumerate() {
        let distance = match tool {
            GizmoTool::Move | GizmoTool::Scale => {
                let start = to_screen(view_projection, rect, origin);
                let end = to_screen(view_projection, rect, origin + *axis * handle);
                match (start, end) {
                    (Some(start), Some(end)) => distance_to_segment(pointer, start, end),
                    _ => f32::INFINITY,
                }
            }
            GizmoTool::Rotate => ring_points(origin, *axis, handle, view_projection, rect)
                .windows(2)
                .map(|segment| distance_to_segment(pointer, segment[0], segment[1]))
                .fold(f32::INFINITY, f32::min),
        };
        if distance < best_distance {
            best_distance = distance;
            best = Some(index);
        }
    }
    best
}

fn ring_points(
    origin: Vec3,
    axis: Vec3,
    radius: f32,
    view_projection: Mat4,
    rect: Rect,
) -> Vec<Pos2> {
    let (tangent, bitangent) = axis.any_orthonormal_pair();
    (0..=RING_SEGMENTS)
        .filter_map(|segment| {
            let angle = segment as f32 / RING_SEGMENTS as f32 * std::f32::consts::TAU;
            let point = origin + (tangent * angle.cos() + bitangent * angle.sin()) * radius;
            to_screen(view_projection, rect, point)
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn draw_gizmo(
    ui: &egui::Ui,
    tool: GizmoTool,
    origin: Vec3,
    axes: &[Vec3; 3],
    handle: f32,
    hovered: Option<usize>,
    view_projection: Mat4,
    rect: Rect,
) {
    let painter = ui.painter_at(rect);
    for (index, axis) in axes.iter().enumerate() {
        let color = if hovered == Some(index) {
            ACTIVE_COLOR
        } else {
            AXIS_COLORS[index]
        };
        let stroke = Stroke::new(2.5, color);
        match tool {
            GizmoTool::Rotate => {
                let points = ring_points(origin, *axis, handle, view_projection, rect);
                painter.add(egui::Shape::line(points, stroke));
            }
            GizmoTool::Move | GizmoTool::Scale => {
                let start = to_screen(view_projection, rect, origin);
                let end = to_screen(view_projection, rect, origin + *axis * handle);
                if let (Some(start), Some(end)) = (start, end) {
                    painter.line_segment([start, end], stroke);
                    if tool == GizmoTool::Scale {
                        painter.rect_filled(
                            Rect::from_center_size(end, Vec2::splat(9.0)),
                            0.0,
                            color,
                        );
                    } else {
                        painter.circle_filled(end, 5.0, color);
                    }
                }
            }
        }
    }
}

/// Наводит камеру на выделенный объект; размер берётся из границ его модели.
pub fn focus_selection(state: &mut EditorState) {
    let Some(selection) = state
        .scene
        .as_ref()
        .and_then(|scene| scene.selection())
        .map(str::to_owned)
    else {
        return;
    };
    let Some(world) = state.world_mut() else {
        return;
    };
    let Some(object) = world.object_by_id(&selection).cloned() else {
        return;
    };
    let Some(matrix) = world.world_matrix(object.handle) else {
        return;
    };
    let bounds = object
        .mesh_renderer
        .as_ref()
        .and_then(|mesh| state.models.get(mesh.model.as_str()))
        .and_then(|model| model.bounds());
    let (center, radius) = match bounds {
        Some(bounds) => bounding_sphere(matrix, bounds),
        None => (matrix.w_axis.truncate(), 1.0),
    };
    state.camera.focus(center, radius);
}

/// Мировая сфера вокруг границ модели, размещённой матрицей `matrix`.
pub fn bounding_sphere(matrix: Mat4, (min, max): ([f32; 3], [f32; 3])) -> (Vec3, f32) {
    let (min, max) = (Vec3::from_array(min), Vec3::from_array(max));
    let center = matrix.transform_point3((min + max) * 0.5);
    let corner = matrix.transform_vector3((max - min) * 0.5);
    (center, corner.length())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewport::camera::EditorCamera;

    #[test]
    fn segment_distance_clamps_to_endpoints() {
        let a = egui::pos2(0.0, 0.0);
        let b = egui::pos2(10.0, 0.0);
        assert_eq!(distance_to_segment(egui::pos2(5.0, 3.0), a, b), 3.0);
        assert_eq!(distance_to_segment(egui::pos2(-4.0, 3.0), a, b), 5.0);
        assert_eq!(
            distance_to_segment(egui::pos2(1.0, 1.0), a, a),
            2.0_f32.sqrt()
        );
    }

    #[test]
    fn hovering_an_axis_tip_selects_that_axis() {
        let camera = EditorCamera {
            yaw: 0.6,
            pitch: 0.5,
            ..EditorCamera::default()
        };
        let rect = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(800.0, 600.0));
        let view_projection = camera.camera().view_projection(800, 600);
        let axes = [Vec3::X, Vec3::Y, Vec3::Z];
        for (index, axis) in axes.iter().enumerate() {
            let tip = to_screen(view_projection, rect, *axis).unwrap();
            assert_eq!(
                hovered_axis(
                    GizmoTool::Move,
                    tip,
                    Vec3::ZERO,
                    &axes,
                    1.0,
                    view_projection,
                    rect
                ),
                Some(index)
            );
        }
        assert_eq!(
            hovered_axis(
                GizmoTool::Move,
                rect.min,
                Vec3::ZERO,
                &axes,
                1.0,
                view_projection,
                rect
            ),
            None
        );
    }

    #[test]
    fn box_corners_follow_the_world_matrix_and_edges_join_neighbours() {
        let matrix = Mat4::from_scale_rotation_translation(
            Vec3::new(2.0, 1.0, 1.0),
            glam::Quat::IDENTITY,
            Vec3::new(10.0, 0.0, 0.0),
        );
        let corners = box_corners(matrix, ([-1.0, 0.0, -1.0], [1.0, 2.0, 1.0]));
        assert_eq!(corners[0], Vec3::new(8.0, 0.0, -1.0));
        assert_eq!(corners[7], Vec3::new(12.0, 2.0, 1.0));
        // Каждое ребро меняет ровно одну координату, и все 12 рёбер различны.
        for (a, b) in BOX_EDGES {
            assert_eq!((a ^ b).count_ones(), 1, "{a}-{b}");
        }
        let unique = BOX_EDGES
            .iter()
            .map(|&(a, b)| (a.min(b), a.max(b)))
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 12);
    }

    #[test]
    fn pointer_ray_through_a_projected_point_hits_it() {
        let camera = EditorCamera::default();
        let rect = Rect::from_min_size(egui::pos2(50.0, 30.0), egui::vec2(640.0, 480.0));
        let view_projection = camera.camera().view_projection(640, 480);
        let point = Vec3::new(1.0, 0.5, -1.0);
        let screen = to_screen(view_projection, rect, point).unwrap();
        let ray = pointer_ray(view_projection, rect, screen).unwrap();
        let along = (point - ray.origin).dot(ray.direction);
        let closest = ray.origin + ray.direction * along;
        assert!(closest.distance(point) < 1e-2);
    }
}
