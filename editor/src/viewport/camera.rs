//! Камера редактора: она не входит в документ сцены и не сохраняется.

use engine_scene::camera::Camera;
use glam::{Mat4, Vec3};

const MIN_DISTANCE: f32 = 0.05;
/// Меньшая сфера занимает на экране единицы пикселей, и её легко не заметить.
const MIN_VISIBLE_RADIUS_PER_DISTANCE: f32 = 0.02;
const MAX_DISTANCE: f32 = 5000.0;
const MAX_PITCH: f32 = 1.54;
const ORBIT_RADIANS_PER_POINT: f32 = 0.006;
const ZOOM_PER_WHEEL_POINT: f32 = 0.0025;
const FOV_NARROW: f32 = std::f32::consts::FRAC_PI_3;

/// Камера, вращающаяся вокруг цели. Горизонт всегда остаётся горизонтальным.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EditorCamera {
    pub target: Vec3,
    pub yaw: f32,
    pub pitch: f32,
    pub distance: f32,
}

impl Default for EditorCamera {
    fn default() -> Self {
        Self {
            target: Vec3::ZERO,
            yaw: 0.6,
            pitch: 0.4,
            distance: 8.0,
        }
    }
}

impl EditorCamera {
    /// Единичный вектор от цели к глазу.
    fn back(self) -> Vec3 {
        Vec3::new(
            self.pitch.cos() * self.yaw.sin(),
            self.pitch.sin(),
            self.pitch.cos() * self.yaw.cos(),
        )
    }

    pub fn eye(self) -> Vec3 {
        self.target + self.back() * self.distance
    }

    pub fn right(self) -> Vec3 {
        Vec3::new(self.yaw.cos(), 0.0, -self.yaw.sin())
    }

    pub fn up(self) -> Vec3 {
        self.right().cross(-self.back())
    }

    pub fn camera(self) -> Camera {
        Camera {
            position: self.eye(),
            target: self.target,
            up: Vec3::Y,
            fov_narrow: FOV_NARROW,
            near: (self.distance * 0.01).clamp(0.01, 1.0),
            far: (self.distance * 100.0).max(1000.0),
        }
    }

    /// Смещение курсора вправо поворачивает сцену вправо, вниз — поднимает камеру.
    pub fn orbit(&mut self, dx: f32, dy: f32) {
        self.yaw -= dx * ORBIT_RADIANS_PER_POINT;
        self.pitch = (self.pitch + dy * ORBIT_RADIANS_PER_POINT).clamp(-MAX_PITCH, MAX_PITCH);
    }

    /// Поворачивает взгляд на месте: глаз неподвижен, смещается цель.
    pub fn look(&mut self, dx: f32, dy: f32) {
        let eye = self.eye();
        self.orbit(dx, dy);
        self.target = eye - self.back() * self.distance;
    }

    /// Сцена следует за курсором: точка на глубине цели остаётся под ним.
    pub fn pan(&mut self, dx: f32, dy: f32, viewport_height: f32) {
        let world_per_point =
            2.0 * self.distance * (FOV_NARROW * 0.5).tan() / viewport_height.max(1.0);
        self.target += (self.up() * dy - self.right() * dx) * world_per_point;
    }

    pub fn zoom(&mut self, wheel_points: f32) {
        self.distance = (self.distance * (-wheel_points * ZOOM_PER_WHEEL_POINT).exp())
            .clamp(MIN_DISTANCE, MAX_DISTANCE);
    }

    /// Перемещает камеру целиком в её осях: вперёд, вправо и вверх по миру.
    pub fn fly(&mut self, forward: f32, right: f32, up: f32) {
        self.target += -self.back() * forward + self.right() * right + Vec3::Y * up;
    }

    /// Сфера почти не видна: камера внутри неё или сфера занимает лишь несколько пикселей.
    pub fn needs_framing(self, center: Vec3, radius: f32) -> bool {
        let distance = self.eye().distance(center);
        distance <= radius || radius < distance * MIN_VISIBLE_RADIUS_PER_DISTANCE
    }

    /// Показывает сферу целиком, сохраняя направление взгляда.
    pub fn focus(&mut self, center: Vec3, radius: f32) {
        self.target = center;
        let half_fov = FOV_NARROW * 0.5;
        self.distance = (radius.max(0.05) / half_fov.sin() * 1.2).clamp(MIN_DISTANCE, MAX_DISTANCE);
    }
}

/// Луч в мировом пространстве; `direction` единичный.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ray {
    pub origin: Vec3,
    pub direction: Vec3,
}

/// Луч через точку изображения с координатами `0..1` от левого верхнего угла.
///
/// Клип-пространство renderer: Y вверх, глубина `[0, 1]`.
pub fn screen_ray(view_projection: Mat4, u: f32, v: f32) -> Option<Ray> {
    let inverse = view_projection.inverse();
    let ndc_x = u * 2.0 - 1.0;
    let ndc_y = 1.0 - v * 2.0;
    let near = inverse.project_point3(Vec3::new(ndc_x, ndc_y, 0.0));
    let far = inverse.project_point3(Vec3::new(ndc_x, ndc_y, 1.0));
    let direction = (far - near).try_normalize()?;
    near.is_finite().then_some(Ray {
        origin: near,
        direction,
    })
}

/// Проекция точки мира в координаты `0..1` изображения; `None`, если точка за камерой.
pub fn project_point(view_projection: Mat4, point: Vec3) -> Option<[f32; 2]> {
    let clip = view_projection * point.extend(1.0);
    if clip.w <= 1e-5 {
        return None;
    }
    Some([(clip.x / clip.w + 1.0) * 0.5, (1.0 - clip.y / clip.w) * 0.5])
}

/// Точка луча на плоскости пола `y = 0`, если она впереди и не дальше `max_distance`.
pub fn ground_point(ray: Ray, max_distance: f32) -> Option<Vec3> {
    if ray.direction.y.abs() < 1e-5 {
        return None;
    }
    let t = -ray.origin.y / ray.direction.y;
    (t > 0.0 && t <= max_distance).then(|| ray.origin + ray.direction * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_near(a: Vec3, b: Vec3) {
        assert!((a - b).length() < 1e-3, "{a} != {b}");
    }

    #[test]
    fn only_tiny_or_surrounding_objects_need_framing() {
        let camera = EditorCamera::default();
        let center = camera.target;
        assert!(camera.needs_framing(center, 0.02));
        assert!(camera.needs_framing(center, camera.distance * 2.0));
        assert!(!camera.needs_framing(center, 1.0));
    }

    #[test]
    fn ground_point_is_where_the_ray_meets_the_floor_within_reach() {
        let ray = Ray {
            origin: Vec3::new(1.0, 2.0, 0.0),
            direction: Vec3::new(0.0, -1.0, 0.0),
        };
        assert_near(ground_point(ray, 10.0).unwrap(), Vec3::new(1.0, 0.0, 0.0));
        assert!(ground_point(ray, 1.0).is_none());
        let upward = Ray {
            direction: Vec3::Y,
            ..ray
        };
        assert!(ground_point(upward, 10.0).is_none());
    }

    #[test]
    fn eye_lies_at_distance_from_target() {
        let camera = EditorCamera {
            target: Vec3::new(1.0, 2.0, 3.0),
            yaw: 0.0,
            pitch: 0.0,
            distance: 5.0,
        };
        assert_near(camera.eye(), Vec3::new(1.0, 2.0, 8.0));
        assert_near(camera.right(), Vec3::X);
        assert_near(camera.up(), Vec3::Y);
    }

    #[test]
    fn orbit_clamps_pitch_and_keeps_the_target() {
        let mut camera = EditorCamera::default();
        let target = camera.target;
        camera.orbit(50.0, 100_000.0);
        assert_eq!(camera.pitch, MAX_PITCH);
        camera.orbit(0.0, -1_000_000.0);
        assert_eq!(camera.pitch, -MAX_PITCH);
        assert_eq!(camera.target, target);
        assert!(camera.camera().view().is_finite());
    }

    #[test]
    fn look_keeps_the_eye_in_place() {
        let mut camera = EditorCamera::default();
        let eye = camera.eye();
        camera.look(40.0, -25.0);
        assert_near(camera.eye(), eye);
    }

    #[test]
    fn pan_moves_scene_with_the_cursor() {
        let mut camera = EditorCamera {
            yaw: 0.0,
            pitch: 0.0,
            ..EditorCamera::default()
        };
        let before = camera.target;
        camera.pan(10.0, 0.0, 600.0);
        assert!(camera.target.x < before.x);
        let projected = |camera: EditorCamera| {
            project_point(camera.camera().view_projection(800, 600), Vec3::ZERO).unwrap()
        };
        let mut moved = EditorCamera {
            yaw: 0.0,
            pitch: 0.0,
            ..EditorCamera::default()
        };
        let start = projected(moved);
        moved.pan(30.0, 0.0, 600.0);
        let end = projected(moved);
        // 30 пунктов из 800 по ширине: при аспекте 4:3 FOV задан по высоте.
        assert!(((end[0] - start[0]) - 30.0 / 800.0).abs() < 1e-3);
    }

    #[test]
    fn zoom_is_multiplicative_and_bounded() {
        let mut camera = EditorCamera::default();
        let distance = camera.distance;
        camera.zoom(100.0);
        assert!(camera.distance < distance);
        camera.zoom(1e6);
        assert_eq!(camera.distance, MIN_DISTANCE);
        camera.zoom(-1e6);
        assert_eq!(camera.distance, MAX_DISTANCE);
    }

    #[test]
    fn fly_forward_moves_along_the_view_direction() {
        let mut camera = EditorCamera {
            yaw: 0.0,
            pitch: 0.0,
            ..EditorCamera::default()
        };
        camera.fly(2.0, 1.0, 3.0);
        assert_near(camera.target, Vec3::new(1.0, 3.0, -2.0));
    }

    #[test]
    fn focus_centers_the_target_and_fits_the_radius() {
        let mut camera = EditorCamera::default();
        camera.focus(Vec3::new(4.0, 0.0, 0.0), 2.0);
        assert_eq!(camera.target, Vec3::new(4.0, 0.0, 0.0));
        assert!(camera.distance * (FOV_NARROW * 0.5).sin() >= 2.0);
    }

    #[test]
    fn center_ray_points_at_the_target() {
        let camera = EditorCamera::default();
        let view_projection = camera.camera().view_projection(800, 600);
        let ray = screen_ray(view_projection, 0.5, 0.5).unwrap();
        let to_target = (camera.target - camera.eye()).normalize();
        assert_near(ray.direction, to_target);
    }

    #[test]
    fn projection_and_ray_are_consistent_and_image_y_points_down() {
        let camera = EditorCamera {
            yaw: 0.0,
            pitch: 0.0,
            ..EditorCamera::default()
        };
        let view_projection = camera.camera().view_projection(800, 600);
        let [u, v] = project_point(view_projection, Vec3::new(1.0, 1.0, 0.0)).unwrap();
        assert!(u > 0.5 && v < 0.5);
        let ray = screen_ray(view_projection, u, v).unwrap();
        let t = -ray.origin.z / ray.direction.z;
        assert_near(ray.origin + ray.direction * t, Vec3::new(1.0, 1.0, 0.0));
    }

    #[test]
    fn points_behind_the_camera_are_not_projected() {
        let camera = EditorCamera::default();
        let view_projection = camera.camera().view_projection(800, 600);
        let behind = camera.eye() + (camera.eye() - camera.target);
        assert_eq!(project_point(view_projection, behind), None);
    }
}
