//! Геометрия и трансформации демонстрационной сцены sandbox.

use engine_assets::TextureData;
use engine_render::Vertex;
use engine_scene::camera::OrbitCamera;
use engine_scene::light::{OrbitingSun, shadow_view_projection};
use glam::camera::rh::proj::directx;
use glam::{Mat4, Vec3};

/// Демонстрационная сцена, выбираемая флагом `--scene`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scene {
    Triangle,
    Cube,
    #[default]
    Shadows,
}

const SHADOWS_CAMERA: OrbitCamera = OrbitCamera {
    target: Vec3::ZERO,
    radius: 4.2,
    height: 1.5,
    angular_speed: std::f32::consts::TAU / 20.0,
    fov_narrow: std::f32::consts::FRAC_PI_4,
    near: 0.1,
    far: 100.0,
};

const SHADOWS_SUN: OrbitingSun = OrbitingSun {
    elevation: std::f32::consts::PI * 55.0 / 180.0,
    start_azimuth: 0.0,
    angular_speed: -std::f32::consts::TAU / 12.0,
};

impl Scene {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "triangle" => Some(Self::Triangle),
            "cube" => Some(Self::Cube),
            "shadows" => Some(Self::Shadows),
            _ => None,
        }
    }

    pub fn mesh(self) -> (Vec<Vertex>, Vec<u32>) {
        match self {
            Self::Triangle => (TRIANGLE_VERTICES.to_vec(), TRIANGLE_INDICES.to_vec()),
            Self::Cube | Self::Shadows => cube_mesh(),
        }
    }

    /// Текстура сцены; треугольник рисуется цветом вершин без текстуры.
    pub fn texture(self) -> Option<TextureData> {
        match self {
            Self::Triangle => None,
            Self::Cube | Self::Shadows => Some(checker_texture()),
        }
    }

    pub fn view_projection(self, elapsed: f32, width: u32, height: u32) -> Mat4 {
        match self {
            Self::Triangle => triangle_view_projection(width, height),
            Self::Cube => cube_view_projection(width, height),
            Self::Shadows => SHADOWS_CAMERA
                .camera_at(elapsed)
                .view_projection(width, height),
        }
    }

    pub fn model(self, elapsed: f32) -> Mat4 {
        match self {
            Self::Triangle => Mat4::from_rotation_z(elapsed),
            Self::Cube => cube_model(elapsed),
            Self::Shadows => Mat4::from_rotation_z(elapsed),
        }
    }
}

pub fn shadows_light_direction(elapsed: f32) -> Vec3 {
    SHADOWS_SUN.direction_at(elapsed)
}

pub fn shadows_light_view_projection(elapsed: f32) -> Mat4 {
    shadow_view_projection(shadows_light_direction(elapsed), Vec3::ZERO, 1.8)
}

/// Квадратная плоскость 2×2 на высоте −1, с обходом CCW со стороны +Y.
pub fn plane_mesh() -> (Vec<Vertex>, Vec<u32>) {
    let positions = [
        [-1.0, -1.0, -1.0],
        [-1.0, -1.0, 1.0],
        [1.0, -1.0, 1.0],
        [1.0, -1.0, -1.0],
    ];
    let vertices = positions
        .into_iter()
        .zip([[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]])
        .map(|(position, uv)| Vertex {
            position,
            normal: [0.0, 1.0, 0.0],
            uv,
            color: [0.78, 0.78, 0.78],
        })
        .collect();
    (vertices, vec![0, 1, 2, 0, 2, 3])
}

const TRIANGLE_NORMAL: [f32; 3] = [0.0, 0.0, 1.0];

pub const TRIANGLE_VERTICES: [Vertex; 3] = [
    Vertex {
        position: [0.0, 0.6, 0.0],
        normal: TRIANGLE_NORMAL,
        uv: [0.5, 0.0],
        color: [1.0, 0.0, 0.0],
    },
    Vertex {
        position: [-0.52, -0.3, 0.0],
        normal: TRIANGLE_NORMAL,
        uv: [0.0, 1.0],
        color: [0.0, 1.0, 0.0],
    },
    Vertex {
        position: [0.52, -0.3, 0.0],
        normal: TRIANGLE_NORMAL,
        uv: [1.0, 1.0],
        color: [0.0, 0.0, 1.0],
    },
];
pub const TRIANGLE_INDICES: [u32; 3] = [0, 1, 2];

/// Грани куба: внешняя нормаль и оси `u`, `v` текстуры, причём `u × v` равно нормали.
const CUBE_FACES: [[[f32; 3]; 3]; 6] = [
    [[1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]],
    [[-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]],
    [[0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]],
    [[0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]],
    [[0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
    [[0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
];

/// Куб со стороной 1 и центром в начале координат: по 4 вершины на грань,
/// чтобы у каждой грани были свои нормаль и развёртка текстуры на весь квадрат.
pub fn cube_mesh() -> (Vec<Vertex>, Vec<u32>) {
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for [normal, u, v] in CUBE_FACES {
        let first = vertices.len() as u32;
        for (s, t) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            let position =
                Vec3::from(normal) * 0.5 + Vec3::from(u) * (s - 0.5) + Vec3::from(v) * (t - 0.5);
            vertices.push(Vertex {
                position: position.to_array(),
                normal,
                // Ось v текстуры направлена вниз, а ось v грани — вверх.
                uv: [s, 1.0 - t],
                color: [1.0, 1.0, 1.0],
            });
        }
        indices.extend([0, 1, 2, 0, 2, 3].map(|offset| first + offset));
    }
    (vertices, indices)
}

/// Шахматная текстура 256x256 из клеток 32x32.
pub fn checker_texture() -> TextureData {
    const SIZE: u32 = 256;
    const CELL: u32 = 32;
    let rgba = (0..SIZE * SIZE)
        .flat_map(|index| {
            let (x, y) = (index % SIZE, index / SIZE);
            if (x / CELL + y / CELL).is_multiple_of(2) {
                [235, 235, 235, 255]
            } else {
                [40, 110, 200, 255]
            }
        })
        .collect();
    TextureData {
        width: SIZE,
        height: SIZE,
        rgba,
    }
}

/// Сохраняет пропорции треугольника при любом соотношении сторон окна.
pub fn triangle_view_projection(width: u32, height: u32) -> Mat4 {
    let aspect = aspect_ratio(width, height);
    let fit = if aspect >= 1.0 {
        Vec3::new(1.0 / aspect, 1.0, 1.0)
    } else {
        Vec3::new(1.0, aspect, 1.0)
    };
    Mat4::from_scale(fit)
}

/// Камера в точке z = 3 смотрит вдоль -Z.
///
/// Угол обзора 45° относится к более узкой стороне окна, поэтому куб виден целиком
/// и в горизонтальном, и в вертикальном окне.
pub fn cube_view_projection(width: u32, height: u32) -> Mat4 {
    let aspect = aspect_ratio(width, height);
    let half_fov_tan = 22.5_f32.to_radians().tan();
    let vertical_fov = 2.0 * (half_fov_tan / aspect.min(1.0)).atan();
    // Соглашение DirectX совпадает с контрактом renderer: Y вверх, глубина в [0, 1].
    let projection = directx::perspective(vertical_fov, aspect, 0.1, 100.0);
    let view = Mat4::from_translation(Vec3::new(0.0, 0.0, -3.0));
    projection * view
}

/// Вращает куб вокруг осей Y и X.
pub fn cube_model(elapsed: f32) -> Mat4 {
    Mat4::from_rotation_y(elapsed) * Mat4::from_rotation_x(elapsed * 0.7)
}

/// Отношение ширины к высоте; нулевые размеры свёрнутого окна считаются единицей.
fn aspect_ratio(width: u32, height: u32) -> f32 {
    width.max(1) as f32 / height.max(1) as f32
}

#[cfg(test)]
mod tests {
    use super::{
        Scene, TRIANGLE_INDICES, TRIANGLE_VERTICES, checker_texture, cube_mesh, cube_model,
        cube_view_projection, plane_mesh, shadows_light_direction, shadows_light_view_projection,
        triangle_view_projection,
    };
    use glam::{Mat4, Vec3, Vec4};

    fn clip(transform: Mat4, position: [f32; 3]) -> Vec4 {
        transform * Vec3::from(position).extend(1.0)
    }

    fn triangle_transform(elapsed: f32, width: u32, height: u32) -> Mat4 {
        triangle_view_projection(width, height) * Scene::Triangle.model(elapsed)
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
    fn scenes_parse_by_name_and_default_to_shadows() {
        assert_eq!(Scene::parse("triangle"), Some(Scene::Triangle));
        assert_eq!(Scene::parse("cube"), Some(Scene::Cube));
        assert_eq!(Scene::parse("shadows"), Some(Scene::Shadows));
        assert_eq!(Scene::parse("Cube"), None);
        assert_eq!(Scene::parse(""), None);
        assert_eq!(Scene::default(), Scene::Shadows);
        assert_eq!(Scene::Triangle.mesh().0, TRIANGLE_VERTICES);
        assert_eq!(Scene::Cube.mesh(), cube_mesh());
        assert_eq!(Scene::Shadows.mesh(), cube_mesh());
        assert!(Scene::Triangle.texture().is_none());
        assert!(Scene::Cube.texture().is_some());
        assert!(Scene::Shadows.texture().is_some());
    }

    #[test]
    fn cube_faces_have_outward_normals_matching_winding_and_full_uv_squares() {
        let (vertices, indices) = cube_mesh();
        assert_eq!((vertices.len(), indices.len()), (24, 36));
        assert!(
            indices
                .iter()
                .all(|&index| (index as usize) < vertices.len())
        );
        for triangle in indices.as_chunks::<3>().0 {
            let corners = triangle.map(|index| vertices[index as usize]);
            let points = corners.map(|vertex| Vec3::from(vertex.position));
            let winding = (points[1] - points[0]).cross(points[2] - points[0]);
            let normal = Vec3::from(corners[0].normal);
            // Нормаль единичная, общая для треугольника, смотрит от центра и совпадает с обходом CCW.
            assert!((normal.length() - 1.0).abs() < 1e-6);
            assert!(
                corners
                    .iter()
                    .all(|vertex| vertex.normal == corners[0].normal)
            );
            assert!(normal.dot(points[0]) > 0.0, "{triangle:?}");
            assert!(winding.normalize().dot(normal) > 0.999, "{triangle:?}");
        }
        for face in vertices.chunks(4) {
            let mut uvs = face.iter().map(|vertex| vertex.uv).collect::<Vec<_>>();
            uvs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            assert_eq!(uvs, [[0.0, 0.0], [0.0, 1.0], [1.0, 0.0], [1.0, 1.0]]);
        }
    }

    #[test]
    fn cube_texture_top_row_matches_upward_face_axis() {
        let (vertices, _) = cube_mesh();
        // Передняя грань (+Z): верхние вершины имеют v = 0, то есть первую строку текстуры.
        let front = &vertices[16..20];
        assert!(front.iter().all(|vertex| vertex.normal == [0.0, 0.0, 1.0]));
        for vertex in front {
            assert_eq!(vertex.uv[1] == 0.0, vertex.position[1] > 0.0, "{vertex:?}");
        }
    }

    #[test]
    fn checker_texture_alternates_cells_and_has_expected_size() {
        let texture = checker_texture();
        assert_eq!((texture.width, texture.height), (256, 256));
        assert_eq!(texture.rgba.len(), 256 * 256 * 4);
        let texel = |x: usize, y: usize| &texture.rgba[(y * 256 + x) * 4..][..4];
        assert_eq!(texel(0, 0), texel(31, 31));
        assert_ne!(texel(0, 0), texel(32, 0));
        assert_ne!(texel(0, 0), texel(0, 32));
        assert_eq!(texel(0, 0), texel(32, 32));
        assert!(texture.rgba.chunks(4).all(|pixel| pixel[3] == 255));
    }

    #[test]
    fn cube_projects_inside_view_with_depth_in_vulkan_range() {
        let (vertices, _) = cube_mesh();
        for (width, height) in [(1280, 720), (720, 1280), (0, 0), (1000, 1000)] {
            for step in 0..64 {
                let transform = cube_view_projection(width, height) * cube_model(step as f32 * 0.1);
                for vertex in &vertices {
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
        let transform = cube_view_projection(1280, 720) * cube_model(0.0);
        let front = clip(transform, [0.0, 0.0, 0.5]);
        let back = clip(transform, [0.0, 0.0, -0.5]);
        assert!(front.z / front.w < back.z / back.w);
    }

    #[test]
    fn plane_is_two_meters_square_at_y_minus_one_with_upward_winding() {
        let (vertices, indices) = plane_mesh();
        assert_eq!(vertices.len(), 4);
        assert_eq!(indices, [0, 1, 2, 0, 2, 3]);
        assert!(vertices.iter().all(|vertex| {
            vertex.position[1] == -1.0
                && (-1.0..=1.0).contains(&vertex.position[0])
                && (-1.0..=1.0).contains(&vertex.position[2])
                && vertex.normal == [0.0, 1.0, 0.0]
        }));
        for triangle in indices.as_chunks::<3>().0 {
            let points = triangle.map(|index| Vec3::from(vertices[index as usize].position));
            let winding = (points[1] - points[0]).cross(points[2] - points[0]);
            assert!(winding.dot(Vec3::Y) > 0.0);
        }
        assert_eq!(
            vertices.iter().map(|vertex| vertex.uv).collect::<Vec<_>>(),
            [[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.0]]
        );
    }

    #[test]
    fn shadow_cube_rotates_around_z_and_stays_above_the_plane() {
        let (vertices, _) = cube_mesh();
        assert!(
            Scene::Shadows
                .model(1.73)
                .transform_point3(Vec3::Z)
                .abs_diff_eq(Vec3::Z, 1e-6)
        );
        for step in 0..64 {
            let model = Scene::Shadows.model(step as f32 * 0.13);
            for vertex in &vertices {
                assert!(model.transform_point3(Vec3::from(vertex.position)).y > -1.0);
            }
        }
    }

    #[test]
    fn shadow_scene_projects_cube_and_plane_inside_all_window_shapes() {
        let (cube_vertices, _) = cube_mesh();
        let (plane_vertices, _) = plane_mesh();
        for (width, height) in [(1280, 720), (720, 1280), (1000, 1000), (0, 0)] {
            for step in 0..64 {
                // Равномерные моменты проходят также через диагональные азимуты камеры.
                let elapsed = step as f32 * 20.0 / 64.0;
                let transform = Scene::Shadows.view_projection(elapsed, width, height);
                let model = Scene::Shadows.model(elapsed);
                for position in cube_vertices
                    .iter()
                    .map(|vertex| model.transform_point3(Vec3::from(vertex.position)))
                    .chain(
                        plane_vertices
                            .iter()
                            .map(|vertex| Vec3::from(vertex.position)),
                    )
                {
                    let point = transform * position.extend(1.0);
                    let ndc = point.truncate() / point.w;
                    assert!(point.w > 0.0, "{width}x{height} t={elapsed}: {point:?}");
                    assert!(
                        ndc.x.abs() <= 1.0 && ndc.y.abs() <= 1.0,
                        "{width}x{height} t={elapsed}: {ndc:?}"
                    );
                    assert!(
                        ndc.z > 0.0 && ndc.z < 1.0,
                        "{width}x{height} t={elapsed}: {ndc:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn camera_and_sun_move_in_opposite_azimuth_directions() {
        let start = super::SHADOWS_CAMERA.camera_at(0.0);
        let next = super::SHADOWS_CAMERA.camera_at(0.001);
        let right = start
            .view()
            .inverse()
            .transform_vector3(Vec3::X)
            .normalize();
        assert!((next.position - start.position).dot(right) > 0.0);
        assert!(shadows_light_direction(0.25).x < 0.0);
    }

    #[test]
    fn sun_projection_of_cube_center_hits_the_plane_square_at_every_azimuth() {
        for step in 0..64 {
            let elapsed = step as f32 * 12.0 / 64.0;
            let direction = shadows_light_direction(elapsed);
            assert!(direction.y < 0.0);
            let distance = -1.0 / direction.y;
            let projected = direction * distance;
            assert!((projected.y + 1.0).abs() < 1e-6);
            assert!(
                projected.x.abs() <= 1.0 && projected.z.abs() <= 1.0,
                "{projected:?}"
            );

            let clip = shadows_light_view_projection(elapsed) * Vec3::ZERO.extend(1.0);
            assert!(clip.w.is_finite());
        }
    }
}
