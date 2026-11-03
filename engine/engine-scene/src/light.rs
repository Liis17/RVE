//! Направленный солнечный свет и каскадные матрицы теней камеры.

use crate::camera::Camera;
use glam::camera::rh::proj::directx;
use glam::camera::rh::view::look_at_mat4;
use glam::{Mat4, Vec3};

pub const MAX_SHADOW_CASCADES: u8 = 4;
const PRACTICAL_SPLIT_LAMBDA: f32 = 0.75;

/// Настройки каскадных теней направленного света.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShadowSettings {
    pub distance: f32,
    pub cascades: u8,
}

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

/// Строит camera-relative view-projection матрицы каскадов directional shadow map.
///
/// Практические split-ы смешивают logarithmic и uniform интервалы с `lambda = 0.75`.
/// Каждый camera-frustum slice аппроксимируется описанной сферой; её центр привязывается
/// к сетке light-space texel-ов, чтобы небольшое движение камеры вдоль этой плоскости
/// не меняло матрицу до пересечения границы texel-а. `resolution` — размер карты в texel-ах.
///
/// Возвращается пустой список для невалидной камеры, направления, aspect, разрешения или
/// настроек, включая дистанцию, не доходящую до near plane камеры.
pub fn cascade_view_projections(
    camera: Camera,
    aspect: f32,
    direction: Vec3,
    settings: ShadowSettings,
    resolution: u32,
) -> Vec<Mat4> {
    if !valid_camera(camera)
        || !aspect.is_finite()
        || aspect <= 0.0
        || !direction.is_finite()
        || !direction.length_squared().is_finite()
        || direction.length_squared() <= 0.0
        || !settings.distance.is_finite()
        || settings.distance <= 0.0
        || !(1..=MAX_SHADOW_CASCADES).contains(&settings.cascades)
        || resolution == 0
    {
        return Vec::new();
    }

    let shadow_far = settings.distance.min(camera.far);
    if shadow_far <= camera.near {
        return Vec::new();
    }

    let direction = direction.normalize();
    let camera_forward = camera.target - camera.position;
    let forward = camera_forward.normalize();
    let right = forward.cross(camera.up).normalize();
    let up = right.cross(forward).normalize();
    if !direction.is_finite()
        || !forward.is_finite()
        || forward.length_squared() == 0.0
        || !right.is_finite()
        || right.length_squared() == 0.0
        || !up.is_finite()
        || up.length_squared() == 0.0
    {
        return Vec::new();
    }

    let narrow_slope = (camera.fov_narrow * 0.5).tan();
    let (width_slope, height_slope) = if aspect >= 1.0 {
        (narrow_slope * aspect, narrow_slope)
    } else {
        (narrow_slope, narrow_slope / aspect)
    };
    if !width_slope.is_finite() || !height_slope.is_finite() {
        return Vec::new();
    }

    let splits = practical_splits(camera.near, shadow_far, settings.cascades);
    let mut matrices = Vec::with_capacity(settings.cascades as usize);
    for interval in splits.windows(2) {
        let near = interval[0];
        let far = interval[1];
        let half_width = far * width_slope;
        let half_height = far * height_slope;
        let half_depth = (far - near) * 0.5;
        let radius =
            (half_width * half_width + half_height * half_height + half_depth * half_depth).sqrt();
        if !radius.is_finite() || radius <= 0.0 {
            return Vec::new();
        }
        let radius = radius + radius.max(1.0) * 1e-5;
        let center = camera.position + forward * (near * 0.5 + far * 0.5);
        let Some(matrix) = light_view_projection(center, radius, direction, shadow_far, resolution)
        else {
            return Vec::new();
        };
        matrices.push(matrix);
    }
    matrices
}

fn valid_camera(camera: Camera) -> bool {
    let forward = camera.target - camera.position;
    let cross = forward.cross(camera.up);
    camera.position.is_finite()
        && camera.target.is_finite()
        && camera.up.is_finite()
        && camera.fov_narrow.is_finite()
        && camera.fov_narrow > 0.0
        && camera.fov_narrow < std::f32::consts::PI
        && camera.near.is_finite()
        && camera.far.is_finite()
        && camera.near > 0.0
        && camera.far > camera.near
        && forward.length_squared().is_finite()
        && forward.length_squared() > 0.0
        && cross.length_squared().is_finite()
        && cross.length_squared() > 0.0
}

fn practical_splits(near: f32, far: f32, cascades: u8) -> Vec<f32> {
    let mut splits = Vec::with_capacity(cascades as usize + 1);
    splits.push(near);
    for index in 1..=cascades {
        if index == cascades {
            splits.push(far);
            continue;
        }
        let fraction = f32::from(index) / f32::from(cascades);
        let logarithmic = (near.ln() * (1.0 - fraction) + far.ln() * fraction).exp();
        let uniform = near + (far - near) * fraction;
        splits
            .push(PRACTICAL_SPLIT_LAMBDA * logarithmic + (1.0 - PRACTICAL_SPLIT_LAMBDA) * uniform);
    }
    splits
}

fn light_view_projection(
    center: Vec3,
    radius: f32,
    direction: Vec3,
    caster_distance: f32,
    resolution: u32,
) -> Option<Mat4> {
    let reference_up = if direction.dot(Vec3::Y).abs() > 0.99 {
        Vec3::Z
    } else {
        Vec3::Y
    };
    let light_right = direction.cross(reference_up).normalize();
    let light_up = light_right.cross(direction).normalize();
    // Snapping moves each coordinate by at most extent / resolution. Reserve
    // that margin using the final texel size: extent >= radius + extent / resolution.
    // A single texel cannot satisfy this bound, so leave its center unsnapped.
    let extent = if resolution > 1 {
        radius / (1.0 - 1.0 / resolution as f32)
    } else {
        radius
    };
    let texel_size = 2.0 * extent / resolution as f32;
    if !center.is_finite()
        || !light_right.is_finite()
        || !light_up.is_finite()
        || !texel_size.is_finite()
        || texel_size <= 0.0
    {
        return None;
    }

    let center_right = center.dot(light_right);
    let center_up = center.dot(light_up);
    let snapped_center = if resolution > 1 {
        center
            + light_right * ((center_right / texel_size).round() * texel_size - center_right)
            + light_up * ((center_up / texel_size).round() * texel_size - center_up)
    } else {
        center
    };

    // Keep the entire receiver sphere in depth and allow casters up to the effective
    // shadow distance toward the light, opposite the ray direction.
    let depth_margin = caster_distance.max(radius);
    let near = 0.01;
    let eye = snapped_center - direction * (depth_margin + near);
    let view = look_at_mat4(eye, snapped_center, reference_up);
    let projection = directx::orthographic(
        -extent,
        extent,
        -extent,
        extent,
        near,
        depth_margin + 2.0 * radius + near,
    );
    let matrix = projection * view;
    matrix.is_finite().then_some(matrix)
}

#[cfg(test)]
mod tests {
    use super::{
        OrbitingSun, ShadowSettings, cascade_view_projections, practical_splits, sun_direction,
    };
    use crate::camera::Camera;
    use glam::{Mat4, Vec3};

    fn camera() -> Camera {
        Camera {
            position: Vec3::new(2.0, 1.0, 4.0),
            target: Vec3::new(2.0, 1.0, 3.0),
            up: Vec3::Y,
            fov_narrow: std::f32::consts::FRAC_PI_4,
            near: 0.1,
            far: 100.0,
        }
    }

    fn settings(cascades: u8, distance: f32) -> ShadowSettings {
        ShadowSettings { distance, cascades }
    }

    fn ndc(matrix: Mat4, point: Vec3) -> Vec3 {
        let clip = matrix * point.extend(1.0);
        clip.truncate() / clip.w
    }

    fn slice_corners(
        mut camera: Camera,
        width: u32,
        height: u32,
        near: f32,
        far: f32,
    ) -> [Vec3; 8] {
        camera.near = near;
        camera.far = far;
        let inverse = camera.view_projection(width, height).inverse();
        let mut corners = [Vec3::ZERO; 8];
        let mut index = 0;
        for z in [0.0, 1.0] {
            for y in [-1.0, 1.0] {
                for x in [-1.0, 1.0] {
                    corners[index] = inverse.project_point3(Vec3::new(x, y, z));
                    index += 1;
                }
            }
        }
        corners
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
    fn practical_splits_are_monotonic_and_cover_the_camera_shadow_interval() {
        let camera = camera();
        let far = 72.0_f32.min(camera.far);
        let splits = practical_splits(camera.near, far, 4);
        assert_eq!(splits.len(), 5);
        assert_eq!(splits[0], camera.near);
        assert_eq!(*splits.last().unwrap(), far);
        assert!(splits.windows(2).all(|pair| pair[0] < pair[1]));

        let matrices = cascade_view_projections(
            camera,
            16.0 / 9.0,
            Vec3::new(1.0, -2.0, 3.0),
            settings(4, 72.0),
            2048,
        );
        assert_eq!(matrices.len(), 4);
    }

    #[test]
    fn each_cascade_contains_all_eight_corners_of_its_frustum_slice() {
        let camera = camera();
        let aspect = 4.0 / 3.0;
        let splits = practical_splits(camera.near, 80.0, 3);
        let matrices = cascade_view_projections(
            camera,
            aspect,
            Vec3::new(1.0, -2.0, 3.0),
            settings(3, 80.0),
            1024,
        );
        assert_eq!(matrices.len(), 3);
        for (matrix, interval) in matrices.iter().zip(splits.windows(2)) {
            for corner in slice_corners(camera, 1200, 900, interval[0], interval[1]) {
                let projected = ndc(*matrix, corner);
                assert!(projected.x.abs() <= 1.0 + 1e-5, "{projected:?}");
                assert!(projected.y.abs() <= 1.0 + 1e-5, "{projected:?}");
                assert!((0.0..=1.0).contains(&projected.z), "{projected:?}");
            }
        }
    }

    #[test]
    fn cascades_cover_actual_landscape_and_portrait_projection_corners() {
        let camera = Camera {
            position: Vec3::ZERO,
            target: Vec3::NEG_Z,
            ..camera()
        };
        for (width, height) in [(1600, 900), (900, 1600)] {
            let matrices = cascade_view_projections(
                camera,
                width as f32 / height as f32,
                Vec3::NEG_Z,
                settings(1, 100.0),
                2048,
            );
            for corner in slice_corners(camera, width, height, camera.near, camera.far) {
                let projected = ndc(matrices[0], corner);
                assert!(projected.x.abs() <= 1.0 + 1e-5, "{projected:?}");
                assert!(projected.y.abs() <= 1.0 + 1e-5, "{projected:?}");
            }
        }
    }

    #[test]
    fn snapped_cascades_keep_boundary_corners_inside_at_all_resolutions() {
        let position = Vec3::new(-0.0304498, -0.0304498, 0.0152097);
        let camera = Camera {
            position,
            target: position - Vec3::Z,
            fov_narrow: std::f32::consts::FRAC_PI_2,
            ..camera()
        };
        let direction = Vec3::new(-0.5964703, 0.7452898, 0.2979369);
        for resolution in [2048, 16, 2, 1] {
            let matrices =
                cascade_view_projections(camera, 1.0, direction, settings(1, 100.0), resolution);
            for corner in slice_corners(camera, 1000, 1000, camera.near, camera.far) {
                let projected = ndc(matrices[0], corner);
                assert!(
                    projected.x.abs() <= 1.0 + 1e-5,
                    "{resolution}: {projected:?}"
                );
                assert!(
                    projected.y.abs() <= 1.0 + 1e-5,
                    "{resolution}: {projected:?}"
                );
            }
        }
    }

    #[test]
    fn cascade_depth_contains_casters_between_the_light_and_receivers() {
        let camera = camera();
        let settings = settings(4, 80.0);
        let direction = Vec3::new(1.0, -2.0, 3.0).normalize();
        let splits = practical_splits(camera.near, settings.distance, settings.cascades);
        let matrices = cascade_view_projections(camera, 16.0 / 9.0, direction, settings, 2048);
        let forward = (camera.target - camera.position).normalize();
        let receiver = camera.position + forward * ((splits[0] + splits[1]) * 0.5);
        let caster = receiver - direction * (settings.distance * 0.9);
        let projected = ndc(matrices[0], caster);

        assert!(projected.x.abs() <= 1.0 + 1e-5, "{projected:?}");
        assert!(projected.y.abs() <= 1.0 + 1e-5, "{projected:?}");
        assert!((0.0..=1.0).contains(&projected.z), "{projected:?}");
    }

    #[test]
    fn subtexel_camera_translation_in_light_plane_keeps_cascade_matrices_stable() {
        let direction = Vec3::NEG_Z;
        let mut camera = Camera {
            position: Vec3::ZERO,
            target: direction,
            up: Vec3::Y,
            fov_narrow: std::f32::consts::FRAC_PI_4,
            near: 0.1,
            far: 100.0,
        };
        let settings = settings(4, 80.0);
        let base = cascade_view_projections(camera, 16.0 / 9.0, direction, settings, 2048);
        let texel_size = 2.0 / (base[0].x_axis.x.abs() * 2048.0);
        camera.position += Vec3::X * (0.2 * texel_size);
        camera.target += Vec3::X * (0.2 * texel_size);
        let moved = cascade_view_projections(camera, 16.0 / 9.0, direction, settings, 2048);
        assert_eq!(base.len(), 4);
        assert_eq!(moved.len(), 4);
        assert!(base.iter().zip(moved).all(|(a, b)| a.abs_diff_eq(b, 1e-6)));
    }

    #[test]
    fn invalid_cascade_settings_and_camera_intervals_return_no_matrices() {
        let camera = camera();
        for settings in [
            settings(0, 20.0),
            settings(5, 20.0),
            settings(1, 0.0),
            settings(1, f32::INFINITY),
        ] {
            assert!(cascade_view_projections(camera, 1.0, Vec3::NEG_Y, settings, 2048).is_empty());
        }
        assert!(
            cascade_view_projections(camera, 0.0, Vec3::NEG_Y, settings(1, 20.0), 2048).is_empty()
        );
        assert!(
            cascade_view_projections(camera, 1.0, Vec3::ZERO, settings(1, 20.0), 2048).is_empty()
        );
        assert!(
            cascade_view_projections(camera, 1.0, Vec3::NEG_Y, settings(1, 20.0), 0).is_empty()
        );
        assert!(
            cascade_view_projections(camera, 1.0, Vec3::NEG_Y, settings(1, 0.05), 2048).is_empty()
        );
        assert_eq!(
            cascade_view_projections(camera, 1.0, Vec3::NEG_Y, settings(1, f32::MAX), 2048).len(),
            1
        );
    }
}
