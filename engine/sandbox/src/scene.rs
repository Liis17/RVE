//! Геометрия и трансформации демонстрационной сцены sandbox.

use engine_render::Vertex;
use glam::{Mat4, Vec3};

pub const TRIANGLE_VERTICES: [Vertex; 3] = [
    Vertex {
        position: [0.0, 0.6, 0.0],
        color: [1.0, 0.0, 0.0],
    },
    Vertex {
        position: [-0.52, -0.3, 0.0],
        color: [0.0, 1.0, 0.0],
    },
    Vertex {
        position: [0.52, -0.3, 0.0],
        color: [0.0, 0.0, 1.0],
    },
];
pub const TRIANGLE_INDICES: [u32; 3] = [0, 1, 2];

/// Поворачивает треугольник вокруг оси Z на `elapsed` радиан и сохраняет его пропорции в окне.
pub fn triangle_transform(elapsed: f32, width: u32, height: u32) -> Mat4 {
    let aspect = aspect_ratio(width, height);
    let fit = if aspect >= 1.0 {
        Vec3::new(1.0 / aspect, 1.0, 1.0)
    } else {
        Vec3::new(1.0, aspect, 1.0)
    };
    Mat4::from_scale(fit) * Mat4::from_rotation_z(elapsed)
}

/// Отношение ширины к высоте; нулевые размеры свёрнутого окна считаются единицей.
fn aspect_ratio(width: u32, height: u32) -> f32 {
    width.max(1) as f32 / height.max(1) as f32
}

#[cfg(test)]
mod tests {
    use super::{TRIANGLE_INDICES, TRIANGLE_VERTICES, triangle_transform};
    use glam::{Vec3, Vec4};

    fn clip(transform: glam::Mat4, position: [f32; 3]) -> Vec4 {
        transform * Vec3::from(position).extend(1.0)
    }

    #[test]
    fn triangle_indices_reference_existing_vertices() {
        assert!(
            TRIANGLE_INDICES
                .iter()
                .all(|&index| (index as usize) < TRIANGLE_VERTICES.len())
        );
    }

    #[test]
    fn triangle_stays_inside_clip_space_for_any_angle_and_window_shape() {
        for (width, height) in [(1280, 720), (720, 1280), (1, 1), (0, 0), (4000, 10)] {
            for step in 0..64 {
                let transform = triangle_transform(step as f32 * 0.1, width, height);
                for vertex in TRIANGLE_VERTICES {
                    let point = clip(transform, vertex.position);
                    assert!(point.x.abs() <= 1.0 && point.y.abs() <= 1.0, "{point:?}");
                    assert_eq!(point.w, 1.0);
                }
            }
        }
    }

    #[test]
    fn triangle_rotates_counter_clockwise_and_keeps_square_pixels() {
        let top = TRIANGLE_VERTICES[0].position;
        let quarter = clip(
            triangle_transform(std::f32::consts::FRAC_PI_2, 1280, 720),
            top,
        );
        assert!(
            (quarter.x - -0.6 * 720.0 / 1280.0).abs() < 1e-6,
            "{quarter:?}"
        );
        assert!(quarter.y.abs() < 1e-6, "{quarter:?}");

        let tall = clip(triangle_transform(0.0, 720, 1280), top);
        assert!((tall.y - 0.6 * 720.0 / 1280.0).abs() < 1e-6, "{tall:?}");
    }
}
