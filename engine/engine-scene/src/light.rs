//! Направленный солнечный свет и матрица камеры для shadow map.

use glam::camera::rh::proj::directx;
use glam::camera::rh::view::look_at_mat4;
use glam::{Mat4, Vec3};

/// Направление лучей солнца в мировом пространстве.
///
/// Углы задаются в радианах. Результат единичный для конечных входных значений.
pub fn sun_direction(elevation: f32, azimuth: f32) -> Vec3 {
    let horizontal = elevation.cos();
    Vec3::new(
        horizontal * azimuth.sin(),
        -elevation.sin(),
        horizontal * azimuth.cos(),
    )
}

/// Солнце с постоянной высотой и вращением азимута со временем.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OrbitingSun {
    pub elevation: f32,
    pub start_azimuth: f32,
    pub angular_speed: f32,
}

impl OrbitingSun {
    /// Направление солнечных лучей для времени `elapsed` в секундах.
    pub fn direction_at(self, elapsed: f32) -> Vec3 {
        sun_direction(
            self.elevation,
            self.start_azimuth + self.angular_speed * elapsed,
        )
    }
}

/// Матрица мировой позиции в clip space ортографической камеры света.
///
/// `direction` — направление лучей, а `radius` — радиус покрываемой сферы. Для
/// определённого результата направление должно быть конечным и ненулевым, `center`
/// конечным, а `radius` конечным и положительным. Нарушение требований даёт NaN.
pub fn shadow_view_projection(direction: Vec3, center: Vec3, radius: f32) -> Mat4 {
    let direction_length_squared = direction.length_squared();
    if !direction.is_finite()
        || !center.is_finite()
        || !direction_length_squared.is_finite()
        || direction_length_squared == 0.0
        || !radius.is_finite()
        || radius <= 0.0
    {
        return undefined_matrix();
    }

    let direction = direction / direction_length_squared.sqrt();
    let eye = center - direction * (2.0 * radius);
    let up = if direction.dot(Vec3::Y).abs() > 0.99 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    let view = look_at_mat4(eye, center, up);
    let projection = directx::orthographic(-radius, radius, -radius, radius, radius, 3.0 * radius);
    projection * view
}

fn undefined_matrix() -> Mat4 {
    Mat4::from_cols_array(&[f32::NAN; 16])
}

#[cfg(test)]
mod tests {
    use super::{OrbitingSun, shadow_view_projection, sun_direction};
    use glam::{Mat4, Vec3};

    fn ndc(matrix: Mat4, point: Vec3) -> Vec3 {
        let clip = matrix * point.extend(1.0);
        clip.truncate() / clip.w
    }

    #[test]
    fn sun_direction_is_unit_length_and_points_downward() {
        let direction = sun_direction(55.0_f32.to_radians(), 0.8);
        assert!((direction.length() - 1.0).abs() < 1e-6);
        assert!(direction.y < 0.0);
    }

    #[test]
    fn opposite_angular_speeds_rotate_azimuth_in_opposite_directions() {
        let make_sun = |angular_speed| OrbitingSun {
            elevation: 55.0_f32.to_radians(),
            start_azimuth: 0.0,
            angular_speed,
        };
        let positive = make_sun(1.0).direction_at(0.25);
        let negative = make_sun(-1.0).direction_at(0.25);
        assert!(positive.x > 0.0);
        assert!(negative.x < 0.0);
    }

    #[test]
    fn shadow_projection_covers_sphere_and_orders_depth_toward_the_light() {
        let direction = Vec3::new(1.0, -2.0, 3.0);
        let center = Vec3::new(2.0, -1.0, 4.0);
        let radius = 1.8;
        let matrix = shadow_view_projection(direction, center, radius);
        for point in [
            center,
            center + Vec3::X * radius,
            center - Vec3::X * radius,
            center + Vec3::Y * radius,
            center - Vec3::Y * radius,
            center + Vec3::Z * radius,
            center - Vec3::Z * radius,
            center + Vec3::new(1.0, 1.0, 1.0).normalize() * radius,
        ] {
            let projected = ndc(matrix, point);
            assert!(projected.x.abs() <= 1.0 + 1e-5, "{projected:?}");
            assert!(projected.y.abs() <= 1.0 + 1e-5, "{projected:?}");
            assert!((0.0..=1.0).contains(&projected.z), "{projected:?}");
        }

        let normalized = shadow_view_projection(direction.normalize(), center, radius);
        assert!(matrix.abs_diff_eq(normalized, 1e-6));
        let toward_light = ndc(matrix, center - direction.normalize() * (radius * 0.5));
        let away_from_light = ndc(matrix, center + direction.normalize() * (radius * 0.5));
        assert!(toward_light.z < away_from_light.z);
    }

    #[test]
    fn vertical_sun_direction_has_finite_shadow_matrix() {
        let matrix = shadow_view_projection(Vec3::NEG_Y, Vec3::ZERO, 1.8);
        assert!(matrix.to_cols_array().into_iter().all(f32::is_finite));
    }

    #[test]
    fn invalid_shadow_inputs_produce_nan_without_panicking() {
        assert!(!shadow_view_projection(Vec3::ZERO, Vec3::ZERO, 1.0).is_finite());
        assert!(!shadow_view_projection(Vec3::Y, Vec3::ZERO, 0.0).is_finite());
    }
}
