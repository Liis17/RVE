//! Математика гизмо Move/Rotate/Scale: от положения курсора к локальной трансформации.

use super::camera::Ray;
use engine_scene::SceneTransform;
use glam::{EulerRot, Mat4, Quat, Vec3};

const MIN_SCALE: f32 = 0.001;

/// Активный инструмент гизмо.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GizmoTool {
    Move,
    Rotate,
    Scale,
}

/// Система координат осей гизмо. На масштабирование не влияет: оно всегда локально.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GizmoSpace {
    Local,
    Global,
}

/// Мировое положение и единичные оси гизмо объекта.
pub fn gizmo_axes(world: Mat4, tool: GizmoTool, space: GizmoSpace) -> (Vec3, [Vec3; 3]) {
    let (_, rotation, translation) = world.to_scale_rotation_translation();
    let axes = if space == GizmoSpace::Global && tool != GizmoTool::Scale {
        [Vec3::X, Vec3::Y, Vec3::Z]
    } else {
        [rotation * Vec3::X, rotation * Vec3::Y, rotation * Vec3::Z]
    };
    (translation, axes)
}

/// Параметр на оси `origin + direction * t`, ближайший к лучу; ось вдоль взгляда даёт `None`.
pub fn closest_axis_parameter(ray: Ray, origin: Vec3, direction: Vec3) -> Option<f32> {
    let offset = origin - ray.origin;
    let cross = direction.cross(ray.direction);
    let denominator = cross.length_squared();
    if denominator < 1e-6 {
        return None;
    }
    Some(ray.direction.cross(offset).dot(cross) / denominator)
}

/// Подписанный угол в радианах вокруг `axis` для точки пересечения луча с плоскостью вращения,
/// отсчитанный от `reference`. `None`, если луч параллелен плоскости или идёт в центр.
pub fn ring_angle(ray: Ray, center: Vec3, axis: Vec3, reference: Vec3) -> Option<f32> {
    let denominator = ray.direction.dot(axis);
    if denominator.abs() < 1e-5 {
        return None;
    }
    let t = (center - ray.origin).dot(axis) / denominator;
    if t < 0.0 {
        return None;
    }
    let radial = ray.origin + ray.direction * t - center;
    if radial.length_squared() < 1e-10 {
        return None;
    }
    Some(
        reference
            .cross(radial)
            .dot(axis)
            .atan2(reference.dot(radial)),
    )
}

/// Перетаскивание гизмо: всё считается от начальной трансформации, ошибки не накапливаются.
#[derive(Clone, Debug)]
pub struct GizmoDrag {
    pub tool: GizmoTool,
    pub axis_index: usize,
    origin: Vec3,
    axis: Vec3,
    /// Параметр на оси или начальный вектор кольца относительно центра.
    start_parameter: f32,
    ring_reference: Vec3,
    /// Длина ручки в мире; по ней перевод смещения в масштаб.
    handle_length: f32,
    start: SceneTransform,
    parent_world: Mat4,
}

impl GizmoDrag {
    /// Начинает перетаскивание; `None`, если ось смотрит точно в камеру.
    #[allow(clippy::too_many_arguments)]
    pub fn begin(
        tool: GizmoTool,
        axis_index: usize,
        ray: Ray,
        origin: Vec3,
        axis: Vec3,
        handle_length: f32,
        start: SceneTransform,
        parent_world: Mat4,
    ) -> Option<Self> {
        let (start_parameter, ring_reference) = match tool {
            GizmoTool::Move | GizmoTool::Scale => {
                (closest_axis_parameter(ray, origin, axis)?, Vec3::ZERO)
            }
            GizmoTool::Rotate => {
                let denominator = ray.direction.dot(axis);
                if denominator.abs() < 1e-5 {
                    return None;
                }
                let t = (origin - ray.origin).dot(axis) / denominator;
                let reference = ray.origin + ray.direction * t - origin;
                if reference.length_squared() < 1e-10 {
                    return None;
                }
                (0.0, reference.normalize())
            }
        };
        Some(Self {
            tool,
            axis_index,
            origin,
            axis,
            start_parameter,
            ring_reference,
            handle_length,
            start,
            parent_world,
        })
    }

    /// Новая локальная трансформация для текущего луча курсора.
    pub fn update(&self, ray: Ray) -> Option<SceneTransform> {
        match self.tool {
            GizmoTool::Move => {
                let t = closest_axis_parameter(ray, self.origin, self.axis)?;
                let delta = self.axis * (t - self.start_parameter);
                Some(translate(self.start, self.parent_world, delta))
            }
            GizmoTool::Rotate => {
                let angle = ring_angle(ray, self.origin, self.axis, self.ring_reference)?;
                Some(rotate(
                    self.start,
                    self.parent_world,
                    Quat::from_axis_angle(self.axis, angle),
                ))
            }
            GizmoTool::Scale => {
                let t = closest_axis_parameter(ray, self.origin, self.axis)?;
                let factor = 1.0 + (t - self.start_parameter) / self.handle_length.max(1e-4);
                Some(scale(self.start, self.axis_index, factor))
            }
        }
    }
}

/// Переносит объект на смещение, заданное в мировом пространстве.
pub fn translate(start: SceneTransform, parent_world: Mat4, world_delta: Vec3) -> SceneTransform {
    let local_delta = parent_world.inverse().transform_vector3(world_delta);
    SceneTransform {
        position: (Vec3::from_array(start.position) + local_delta).to_array(),
        ..start
    }
}

/// Поворачивает объект на мировое вращение, сохраняя его локальные положение и масштаб.
pub fn rotate(start: SceneTransform, parent_world: Mat4, world_rotation: Quat) -> SceneTransform {
    let (_, parent_rotation, _) = parent_world.to_scale_rotation_translation();
    let radians = Vec3::from_array(start.rotation) * (std::f32::consts::PI / 180.0);
    let local = Quat::from_euler(EulerRot::YXZ, radians.y, radians.x, radians.z);
    let updated =
        (parent_rotation.inverse() * world_rotation * parent_rotation * local).normalize();
    let (y, x, z) = updated.to_euler(EulerRot::YXZ);
    SceneTransform {
        rotation: [x.to_degrees(), y.to_degrees(), z.to_degrees()],
        ..start
    }
}

/// Умножает локальный масштаб по оси; значение не проходит через ноль и не меняет знак.
pub fn scale(start: SceneTransform, axis_index: usize, factor: f32) -> SceneTransform {
    let mut result = start;
    let value = start.scale[axis_index]
        * factor.max(MIN_SCALE / start.scale[axis_index].abs().max(MIN_SCALE));
    result.scale[axis_index] = value;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_scene::world::LocalTransform;

    fn world_matrix(transform: SceneTransform, parent_world: Mat4) -> Mat4 {
        parent_world * LocalTransform::from(transform).matrix()
    }

    fn assert_near(a: Vec3, b: Vec3) {
        assert!((a - b).length() < 1e-3, "{a} != {b}");
    }

    fn ray_at(x: f32) -> Ray {
        Ray {
            origin: Vec3::new(x, 0.0, 10.0),
            direction: Vec3::NEG_Z,
        }
    }

    #[test]
    fn closest_parameter_is_where_the_ray_crosses_the_axis() {
        assert!(
            (closest_axis_parameter(ray_at(3.0), Vec3::ZERO, Vec3::X).unwrap() - 3.0).abs() < 1e-4
        );
        assert_eq!(
            closest_axis_parameter(ray_at(3.0), Vec3::ZERO, Vec3::Z),
            None
        );
    }

    #[test]
    fn ring_angle_is_signed_around_the_axis() {
        let from = |x: f32, y: f32| Ray {
            origin: Vec3::new(x, y, 10.0),
            direction: Vec3::NEG_Z,
        };
        let angle = ring_angle(from(0.0, 2.0), Vec3::ZERO, Vec3::Z, Vec3::X).unwrap();
        assert!((angle - std::f32::consts::FRAC_PI_2).abs() < 1e-4);
        let angle = ring_angle(from(0.0, -2.0), Vec3::ZERO, Vec3::Z, Vec3::X).unwrap();
        assert!((angle + std::f32::consts::FRAC_PI_2).abs() < 1e-4);
        assert_eq!(
            ring_angle(from(0.0, 0.0), Vec3::ZERO, Vec3::Z, Vec3::X),
            None
        );
        assert_eq!(
            ring_angle(from(1.0, 0.0), Vec3::ZERO, Vec3::X, Vec3::Y),
            None
        );
    }

    #[test]
    fn global_axes_ignore_rotation_and_local_axes_follow_it() {
        let world = Mat4::from_quat(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2));
        let (_, global) = gizmo_axes(world, GizmoTool::Move, GizmoSpace::Global);
        assert_near(global[0], Vec3::X);
        let (_, local) = gizmo_axes(world, GizmoTool::Move, GizmoSpace::Local);
        assert_near(local[0], Vec3::Y);
    }

    #[test]
    fn scale_axes_are_always_local() {
        let world = Mat4::from_quat(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2));
        let (_, axes) = gizmo_axes(world, GizmoTool::Scale, GizmoSpace::Global);
        assert_near(axes[0], Vec3::Y);
    }

    #[test]
    fn world_translation_under_a_rotated_scaled_parent_lands_at_the_world_delta() {
        let parent = Mat4::from_scale_rotation_translation(
            Vec3::splat(2.0),
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            Vec3::new(5.0, 0.0, 0.0),
        );
        let start = SceneTransform {
            position: [1.0, 0.0, 0.0],
            ..SceneTransform::default()
        };
        let before = world_matrix(start, parent).w_axis.truncate();
        let moved = translate(start, parent, Vec3::new(0.0, 0.0, 4.0));
        let after = world_matrix(moved, parent).w_axis.truncate();
        assert_near(after - before, Vec3::new(0.0, 0.0, 4.0));
    }

    #[test]
    fn world_rotation_under_a_rotated_parent_is_applied_in_world_space() {
        let parent = Mat4::from_quat(Quat::from_rotation_x(0.7));
        let start = SceneTransform {
            rotation: [10.0, 20.0, 30.0],
            ..SceneTransform::default()
        };
        let delta = Quat::from_rotation_y(0.5);
        let rotated = rotate(start, parent, delta);
        let expected = delta
            * world_matrix(start, parent)
                .to_scale_rotation_translation()
                .1;
        let actual = world_matrix(rotated, parent)
            .to_scale_rotation_translation()
            .1;
        assert!(actual.dot(expected).abs() > 0.9999);
    }

    #[test]
    fn rotation_preserves_position_and_scale() {
        let start = SceneTransform {
            position: [1.0, 2.0, 3.0],
            rotation: [0.0; 3],
            scale: [2.0, 3.0, 4.0],
        };
        let rotated = rotate(start, Mat4::IDENTITY, Quat::from_rotation_y(1.0));
        assert_eq!(rotated.position, start.position);
        assert_eq!(rotated.scale, start.scale);
    }

    #[test]
    fn scale_changes_one_axis_and_stays_nonzero() {
        let start = SceneTransform {
            scale: [2.0, 1.0, 1.0],
            ..SceneTransform::default()
        };
        assert_eq!(scale(start, 0, 1.5).scale, [3.0, 1.0, 1.0]);
        assert!(scale(start, 0, -10.0).scale[0] > 0.0);
        assert!(scale(start, 0, 0.0).scale[0] >= MIN_SCALE - 1e-6);
    }

    fn drag(tool: GizmoTool, axis: Vec3, ray: Ray) -> GizmoDrag {
        GizmoDrag::begin(
            tool,
            0,
            ray,
            Vec3::ZERO,
            axis,
            2.0,
            SceneTransform::default(),
            Mat4::IDENTITY,
        )
        .unwrap()
    }

    #[test]
    fn move_drag_follows_the_cursor_relative_to_the_grab_point() {
        let drag = drag(GizmoTool::Move, Vec3::X, ray_at(1.0));
        let moved = drag.update(ray_at(4.0)).unwrap();
        assert!((moved.position[0] - 3.0).abs() < 1e-4);
        assert_eq!(drag.update(ray_at(1.0)).unwrap().position, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn scale_drag_converts_handle_lengths_to_a_factor() {
        let drag = drag(GizmoTool::Scale, Vec3::X, ray_at(1.0));
        let scaled = drag.update(ray_at(3.0)).unwrap();
        assert!((scaled.scale[0] - 2.0).abs() < 1e-4);
    }

    #[test]
    fn rotate_drag_turns_by_the_cursor_angle() {
        let grab = Ray {
            origin: Vec3::new(1.0, 0.0, 10.0),
            direction: Vec3::NEG_Z,
        };
        let turned = Ray {
            origin: Vec3::new(0.0, 1.0, 10.0),
            direction: Vec3::NEG_Z,
        };
        let drag = drag(GizmoTool::Rotate, Vec3::Z, grab);
        let result = drag.update(turned).unwrap();
        assert!((result.rotation[2] - 90.0).abs() < 1e-2);
    }

    #[test]
    fn dragging_an_axis_pointing_at_the_camera_cannot_start() {
        let started = GizmoDrag::begin(
            GizmoTool::Move,
            2,
            ray_at(0.0),
            Vec3::ZERO,
            Vec3::Z,
            1.0,
            SceneTransform::default(),
            Mat4::IDENTITY,
        );
        assert!(started.is_none());
    }
}
