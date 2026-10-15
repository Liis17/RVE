//! Геометрия и трансформации демонстрационной сцены sandbox.

use engine_render::Vertex;
use glam::camera::rh::proj::directx;
use glam::{Mat4, Vec3};

/// Демонстрационная сцена, выбираемая флагом `--scene`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scene {
    Triangle,
    #[default]
    Cube,
}

impl Scene {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "triangle" => Some(Self::Triangle),
            "cube" => Some(Self::Cube),
            _ => None,
        }
    }

    pub fn mesh(self) -> (&'static [Vertex], &'static [u32]) {
        match self {
            Self::Triangle => (&TRIANGLE_VERTICES, &TRIANGLE_INDICES),
            Self::Cube => (&CUBE_VERTICES, &CUBE_INDICES),
        }
    }

    pub fn transform(self, elapsed: f32, width: u32, height: u32) -> Mat4 {
        match self {
            Self::Triangle => triangle_transform(elapsed, width, height),
            Self::Cube => cube_transform(elapsed, width, height),
        }
    }
}

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

/// Вершины куба со стороной 1; цвет каждой вершины совпадает с её координатами в `[0, 1]`.
pub const CUBE_VERTICES: [Vertex; 8] = [
    cube_vertex(0.0, 0.0, 0.0),
    cube_vertex(1.0, 0.0, 0.0),
    cube_vertex(1.0, 1.0, 0.0),
    cube_vertex(0.0, 1.0, 0.0),
    cube_vertex(0.0, 0.0, 1.0),
    cube_vertex(1.0, 0.0, 1.0),
    cube_vertex(1.0, 1.0, 1.0),
    cube_vertex(0.0, 1.0, 1.0),
];
/// Шесть граней по два треугольника.
pub const CUBE_INDICES: [u32; 36] = [
    0, 2, 1, 0, 3, 2, // z = 0
    4, 5, 6, 4, 6, 7, // z = 1
    0, 4, 7, 0, 7, 3, // x = 0
    1, 2, 6, 1, 6, 5, // x = 1
    0, 1, 5, 0, 5, 4, // y = 0
    3, 7, 6, 3, 6, 2, // y = 1
];

const fn cube_vertex(x: f32, y: f32, z: f32) -> Vertex {
    Vertex {
        position: [x - 0.5, y - 0.5, z - 0.5],
        color: [x, y, z],
    }
}

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

/// Вращает куб вокруг осей Y и X и проецирует его камерой, смотрящей вдоль -Z.
///
/// Угол обзора 45° относится к более узкой стороне окна, поэтому куб виден целиком
/// и в горизонтальном, и в вертикальном окне.
pub fn cube_transform(elapsed: f32, width: u32, height: u32) -> Mat4 {
    let aspect = aspect_ratio(width, height);
    let half_fov_tan = 22.5_f32.to_radians().tan();
    let vertical_fov = 2.0 * (half_fov_tan / aspect.min(1.0)).atan();
    // Соглашение DirectX совпадает с контрактом renderer: Y вверх, глубина в [0, 1].
    let projection = directx::perspective(vertical_fov, aspect, 0.1, 100.0);
    let view = Mat4::from_translation(Vec3::new(0.0, 0.0, -3.0));
    let model = Mat4::from_rotation_y(elapsed) * Mat4::from_rotation_x(elapsed * 0.7);
    projection * view * model
}

/// Отношение ширины к высоте; нулевые размеры свёрнутого окна считаются единицей.
fn aspect_ratio(width: u32, height: u32) -> f32 {
    width.max(1) as f32 / height.max(1) as f32
}

#[cfg(test)]
mod tests {
    use super::{
        CUBE_INDICES, CUBE_VERTICES, Scene, TRIANGLE_INDICES, TRIANGLE_VERTICES, cube_transform,
        triangle_transform,
    };
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

    #[test]
    fn scenes_parse_by_name_and_default_to_cube() {
        assert_eq!(Scene::parse("triangle"), Some(Scene::Triangle));
        assert_eq!(Scene::parse("cube"), Some(Scene::Cube));
        assert_eq!(Scene::parse("Cube"), None);
        assert_eq!(Scene::parse(""), None);
        assert_eq!(Scene::default(), Scene::Cube);
        assert_eq!(Scene::Triangle.mesh().0, &TRIANGLE_VERTICES);
        assert_eq!(Scene::Cube.mesh().1, &CUBE_INDICES);
    }

    #[test]
    fn cube_triangles_are_non_degenerate_and_face_outward() {
        assert_eq!(CUBE_INDICES.len(), 36);
        assert!(
            CUBE_INDICES
                .iter()
                .all(|&index| (index as usize) < CUBE_VERTICES.len())
        );
        for triangle in CUBE_INDICES.as_chunks::<3>().0 {
            let points = triangle.map(|index| Vec3::from(CUBE_VERTICES[index as usize].position));
            let normal = (points[1] - points[0]).cross(points[2] - points[0]);
            // Центр куба в начале координат, поэтому внешняя нормаль грани смотрит от него.
            assert!(normal.length() > 0.0);
            assert!(normal.dot(points[0]) > 0.0, "{triangle:?}");
        }
    }

    #[test]
    fn cube_projects_inside_view_with_depth_in_vulkan_range() {
        for (width, height) in [(1280, 720), (720, 1280), (0, 0), (1000, 1000)] {
            for step in 0..64 {
                let transform = cube_transform(step as f32 * 0.1, width, height);
                for vertex in CUBE_VERTICES {
                    let point = clip(transform, vertex.position);
                    let ndc = point.truncate() / point.w;
                    assert!(point.w > 0.0, "{point:?}");
                    assert!(ndc.x.abs() <= 1.0 && ndc.y.abs() <= 1.0, "{ndc:?}");
                    assert!(ndc.z > 0.0 && ndc.z < 1.0, "{ndc:?}");
                }
            }
        }
    }

    #[test]
    fn nearer_cube_points_get_smaller_depth() {
        let transform = cube_transform(0.0, 1280, 720);
        let front = clip(transform, [0.0, 0.0, 0.5]);
        let back = clip(transform, [0.0, 0.0, -0.5]);
        assert!(front.z / front.w < back.z / back.w);
    }
}
