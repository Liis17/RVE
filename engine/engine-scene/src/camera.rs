//! Камеры и орбитальное движение камеры сцены.

use glam::camera::rh::proj::directx;
use glam::camera::rh::view::look_at_mat4;
use glam::{Mat4, Vec3};

/// Камера в правой системе координат с осью Y вверх и глубиной DirectX `[0, 1]`.
///
/// Для определённого результата требуется `0 < fov_narrow < π`, `0 < near < far`,
/// конечные параметры, `position != target` и ненулевой `up`, не параллельный взгляду.
/// При нарушении этих требований матрица результата содержит NaN; методы не паникуют.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub position: Vec3,
    pub target: Vec3,
    pub up: Vec3,
    /// Угол обзора в радианах по более узкой стороне окна.
    pub fov_narrow: f32,
    pub near: f32,
    pub far: f32,
}

impl Camera {
    /// Матрица вида из мирового пространства в пространство камеры.
    pub fn view(self) -> Mat4 {
        let direction = self.target - self.position;
        let direction_length_squared = direction.length_squared();
        let up_length_squared = self.up.length_squared();
        if !self.position.is_finite()
            || !self.target.is_finite()
            || !self.up.is_finite()
            || !direction.is_finite()
            || !direction_length_squared.is_finite()
            || direction_length_squared == 0.0
            || !up_length_squared.is_finite()
            || up_length_squared == 0.0
            || direction.cross(self.up).length_squared() == 0.0
        {
            return undefined_matrix();
        }
        look_at_mat4(self.position, self.target, self.up.normalize())
    }

    /// Перспективная проекция с FOV по узкой стороне, Y вверх и глубиной `[0, 1]`.
    /// Нулевые ширина или высота считаются равными единице.
    pub fn projection(self, width: u32, height: u32) -> Mat4 {
        if !self.fov_narrow.is_finite()
            || self.fov_narrow <= 0.0
            || self.fov_narrow >= std::f32::consts::PI
            || !self.near.is_finite()
            || !self.far.is_finite()
            || self.near <= 0.0
            || self.far <= self.near
        {
            return undefined_matrix();
        }

        let aspect = width.max(1) as f32 / height.max(1) as f32;
        let vertical_fov = 2.0 * ((0.5 * self.fov_narrow).tan() / aspect.min(1.0)).atan();
        directx::perspective(vertical_fov, aspect, self.near, self.far)
    }

    /// Матрица из мирового пространства в clip space камеры.
    pub fn view_projection(self, width: u32, height: u32) -> Mat4 {
        self.projection(width, height) * self.view()
    }
}

/// Камера, движущаяся по горизонтальной орбите вокруг фиксированной цели.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OrbitCamera {
    pub target: Vec3,
    pub radius: f32,
    pub height: f32,
    pub angular_speed: f32,
    pub fov_narrow: f32,
    pub near: f32,
    pub far: f32,
}

impl OrbitCamera {
    /// Создаёт состояние камеры для времени `elapsed` в секундах.
    /// При нулевом времени позиция находится на `+Z`; положительная скорость ведёт к `+X`.
    pub fn camera_at(self, elapsed: f32) -> Camera {
        let angle = self.angular_speed * elapsed;
        Camera {
            position: self.target
                + Vec3::new(
                    self.radius * angle.sin(),
                    self.height,
                    self.radius * angle.cos(),
                ),
            target: self.target,
            up: Vec3::Y,
            fov_narrow: self.fov_narrow,
            near: self.near,
            far: self.far,
        }
    }
}

fn undefined_matrix() -> Mat4 {
    Mat4::from_cols_array(&[f32::NAN; 16])
}

#[cfg(test)]
mod tests {
    use super::{Camera, OrbitCamera};
    use glam::{Mat4, Vec3, Vec4};

    fn test_camera() -> Camera {
        Camera {
            position: Vec3::new(0.0, 0.0, 4.0),
            target: Vec3::ZERO,
            up: Vec3::Y,
            fov_narrow: std::f32::consts::FRAC_PI_4,
            near: 0.1,
            far: 100.0,
        }
    }

    fn clip(matrix: Mat4, point: Vec3) -> Vec3 {
        let clip = matrix * point.extend(1.0);
        clip.truncate() / clip.w
    }

    #[test]
    fn target_projects_to_center_and_perspective_depth_is_ordered() {
        let camera = test_camera();
        let view_projection = camera.view_projection(1280, 720);
        let target = clip(view_projection, camera.target);
        assert!(target.x.abs() < 1e-6 && target.y.abs() < 1e-6);
        assert!((0.0..1.0).contains(&target.z));

        let near = clip(view_projection, Vec3::new(0.0, 0.0, 2.0));
        let far = clip(view_projection, Vec3::new(0.0, 0.0, -2.0));
        assert!(near.z < far.z);
    }

    #[test]
    fn narrow_side_fov_is_stable_for_horizontal_vertical_and_zero_sizes() {
        let camera = test_camera();
        let horizontal = camera.projection(1280, 720);
        let vertical = camera.projection(720, 1280);
        let square = camera.projection(1000, 1000);
        let zero = camera.projection(0, 0);

        assert!(horizontal.y_axis.y > horizontal.x_axis.x);
        assert!(vertical.x_axis.x > vertical.y_axis.y);
        assert!((square.x_axis.x - square.y_axis.y).abs() < 1e-6);
        assert_eq!(zero, square);
    }

    #[test]
    fn orbit_preserves_radius_and_height_and_moves_right_for_positive_speed() {
        let orbit = OrbitCamera {
            target: Vec3::new(1.0, -2.0, 3.0),
            radius: 4.2,
            height: 1.5,
            angular_speed: 1.0,
            fov_narrow: std::f32::consts::FRAC_PI_4,
            near: 0.1,
            far: 100.0,
        };
        let start = orbit.camera_at(0.0);
        let next = orbit.camera_at(0.001);
        let right = start
            .view()
            .inverse()
            .transform_vector3(Vec3::X)
            .normalize();
        let derivative = (next.position - start.position) / 0.001;

        let expected_radius = (orbit.radius.powi(2) + orbit.height.powi(2)).sqrt();
        assert!((start.position.distance(orbit.target) - expected_radius).abs() < 1e-5);
        assert!((start.position.y - (orbit.target.y + orbit.height)).abs() < 1e-6);
        assert!(derivative.dot(right) > 0.0);
    }

    #[test]
    fn invalid_camera_parameters_produce_nan_without_panicking() {
        let mut camera = test_camera();
        camera.position = camera.target;
        assert!((camera.view() * Vec4::W).is_nan());
        camera = test_camera();
        camera.fov_narrow = 0.0;
        assert!((camera.projection(1, 1) * Vec4::W).is_nan());
    }
}
