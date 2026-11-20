//! Выбор объекта лучом по треугольникам CPU-моделей.

use super::camera::Ray;
use engine_assets::Model;
use glam::{Mat3, Mat4, Vec3};
use std::collections::HashMap;
use std::sync::{Arc, Weak};

/// Порог определителя в мировых координатах: вырожденные треугольники отбрасываются.
const WORLD_DET_EPSILON: f32 = 1e-10;

/// Модуль определителя матрицы модели, ниже которого луч не переводится в локальные координаты.
/// Тогда используется перебор в мировых координатах.
const MIN_MATRIX_DET: f32 = 1e-12;

/// Максимум треугольников в листе BVH.
const LEAF_TRIANGLES: usize = 4;

/// Глубина дерева ограничена, чтобы рекурсия не зависела от данных.
const MAX_DEPTH: u32 = 32;

/// Относительный запас AABB: защищает от округления на границе коробки.
const BOUNDS_PADDING: f32 = 1e-6;

/// Расстояние вдоль луча до треугольника по алгоритму Мёллера — Трумбора.
///
/// Вырожденные треугольники и попадания позади начала луча дают `None`.
pub fn ray_triangle(ray: Ray, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    ray_triangle_eps(ray.origin, ray.direction, a, b, c, WORLD_DET_EPSILON)
}

/// Мёллер — Трумбор с порогом определителя `det_epsilon`.
fn ray_triangle_eps(
    origin: Vec3,
    direction: Vec3,
    a: Vec3,
    b: Vec3,
    c: Vec3,
    det_epsilon: f32,
) -> Option<f32> {
    let edge1 = b - a;
    let edge2 = c - a;
    let p = direction.cross(edge2);
    let determinant = edge1.dot(p);
    if determinant.abs() < det_epsilon {
        return None;
    }
    let inverse = 1.0 / determinant;
    let offset = origin - a;
    let u = offset.dot(p) * inverse;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = offset.cross(edge1);
    let v = direction.dot(q) * inverse;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = edge2.dot(q) * inverse;
    (t >= 0.0).then_some(t)
}

/// Треугольники всех мешей в локальных координатах модели.
///
/// Треугольник с индексом вне вершин пропускается.
fn local_triangles(model: &Model) -> impl Iterator<Item = [Vec3; 3]> + '_ {
    model.meshes.iter().flat_map(|mesh| {
        mesh.indices
            .as_chunks::<3>()
            .0
            .iter()
            .filter_map(move |triangle| {
                let position = |index: u32| {
                    mesh.vertices
                        .get(index as usize)
                        .map(|vertex| Vec3::from_array(vertex.position))
                };
                Some([
                    position(triangle[0])?,
                    position(triangle[1])?,
                    position(triangle[2])?,
                ])
            })
    })
}

/// Ближайшее попадание луча в модель, размещённую матрицей `model_matrix`, перебором всех треугольников.
pub fn ray_model(ray: Ray, model: &Model, model_matrix: Mat4) -> Option<f32> {
    let mut nearest: Option<f32> = None;
    for triangle in local_triangles(model) {
        let [a, b, c] = triangle.map(|point| model_matrix.transform_point3(point));
        if let Some(t) = ray_triangle(ray, a, b, c)
            && nearest.is_none_or(|best| t < best)
        {
            nearest = Some(t);
        }
    }
    nearest
}

/// Луч в локальных координатах модели.
struct LocalRay {
    origin: Vec3,
    direction: Vec3,
    det_epsilon: f32,
}

/// Переводит луч в локальные координаты. Направление не нормализуется, поэтому параметр `t`
/// совпадает с мировым. Порог пересчитывается: `det_world = det_local · det(M)`.
///
/// `None`, если матрица вырождена или результат нефинитен: тогда вызывается перебор.
fn to_local(ray: Ray, model_matrix: Mat4) -> Option<LocalRay> {
    let det = Mat3::from_mat4(model_matrix).determinant().abs();
    if !det.is_finite() || det < MIN_MATRIX_DET {
        return None;
    }
    let inverse = model_matrix.inverse();
    let origin = inverse.transform_point3(ray.origin);
    let direction = inverse.transform_vector3(ray.direction);
    let det_epsilon = WORLD_DET_EPSILON / det;
    (origin.is_finite() && direction.is_finite() && det_epsilon.is_finite()).then_some(LocalRay {
        origin,
        direction,
        det_epsilon,
    })
}

/// BVH по треугольникам модели в локальных координатах (bind pose).
pub struct ModelBvh {
    /// Треугольники, переставленные так, что каждый лист — непрерывный отрезок.
    triangles: Vec<[Vec3; 3]>,
    /// Корень — первый узел; пустой список значит пустую модель.
    nodes: Vec<Node>,
}

#[derive(Clone, Copy)]
struct Node {
    min: Vec3,
    max: Vec3,
    kind: NodeKind,
}

#[derive(Clone, Copy)]
enum NodeKind {
    Leaf { start: usize, len: usize },
    Branch { left: usize, right: usize },
}

impl ModelBvh {
    pub fn new(model: &Model) -> Self {
        let mut triangles: Vec<[Vec3; 3]> = local_triangles(model).collect();
        let mut nodes = Vec::new();
        if !triangles.is_empty() {
            build_node(&mut triangles, 0, 0, &mut nodes);
        }
        Self { triangles, nodes }
    }

    /// Ближайшее попадание луча в локальных координатах; `det_epsilon` — порог локального определителя.
    pub fn raycast(&self, origin: Vec3, direction: Vec3, det_epsilon: f32) -> Option<f32> {
        if self.nodes.is_empty() {
            return None;
        }
        let mut nearest: Option<f32> = None;
        let mut stack = vec![0usize];
        while let Some(index) = stack.pop() {
            let node = self.nodes[index];
            let limit = nearest.unwrap_or(f32::INFINITY);
            if !slab_hit(node.min, node.max, origin, direction, limit) {
                continue;
            }
            match node.kind {
                NodeKind::Leaf { start, len } => {
                    for &[a, b, c] in &self.triangles[start..start + len] {
                        if let Some(t) = ray_triangle_eps(origin, direction, a, b, c, det_epsilon)
                            && nearest.is_none_or(|best| t < best)
                        {
                            nearest = Some(t);
                        }
                    }
                }
                NodeKind::Branch { left, right } => {
                    stack.push(left);
                    stack.push(right);
                }
            }
        }
        nearest
    }
}

/// Строит узел для `triangles`, которые в общем массиве начинаются с `start`; возвращает индекс узла.
///
/// Делит по медиане центроидов вдоль самой длинной оси.
fn build_node(
    triangles: &mut [[Vec3; 3]],
    start: usize,
    depth: u32,
    nodes: &mut Vec<Node>,
) -> usize {
    let (min, max) = bounds(triangles);
    let index = nodes.len();
    nodes.push(Node {
        min,
        max,
        kind: NodeKind::Leaf {
            start,
            len: triangles.len(),
        },
    });
    if triangles.len() <= LEAF_TRIANGLES || depth >= MAX_DEPTH {
        return index;
    }
    let (centroid_min, centroid_max) = triangles.iter().map(centroid).fold(
        (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
        |(lo, hi), point| (lo.min(point), hi.max(point)),
    );
    let extent = centroid_max - centroid_min;
    let axis = (0..3)
        .max_by(|&a, &b| extent[a].total_cmp(&extent[b]))
        .unwrap_or(0);
    let mid = triangles.len() / 2;
    triangles.select_nth_unstable_by(mid, |a, b| centroid(a)[axis].total_cmp(&centroid(b)[axis]));
    let (left_part, right_part) = triangles.split_at_mut(mid);
    let left = build_node(left_part, start, depth + 1, nodes);
    let right = build_node(right_part, start + mid, depth + 1, nodes);
    nodes[index].kind = NodeKind::Branch { left, right };
    index
}

/// AABB треугольников с запасом `BOUNDS_PADDING`.
fn bounds(triangles: &[[Vec3; 3]]) -> (Vec3, Vec3) {
    let (min, max) = triangles.iter().flatten().fold(
        (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
        |(lo, hi), point| (lo.min(*point), hi.max(*point)),
    );
    let pad = Vec3::splat(BOUNDS_PADDING * (1.0 + min.abs().max(max.abs()).max_element()));
    (min - pad, max + pad)
}

fn centroid(triangle: &[Vec3; 3]) -> Vec3 {
    (triangle[0] + triangle[1] + triangle[2]) / 3.0
}

/// Пересекает ли луч AABB на отрезке `[0, limit]` (метод «слэбов»).
///
/// Компонента направления, равная нулю, не делит отрезок: луч параллелен слою и должен лежать внутри него.
fn slab_hit(min: Vec3, max: Vec3, origin: Vec3, direction: Vec3, limit: f32) -> bool {
    let mut near = 0.0_f32;
    let mut far = limit;
    for axis in 0..3 {
        let (lo, hi, o, d) = (min[axis], max[axis], origin[axis], direction[axis]);
        if d == 0.0 {
            if o < lo || o > hi {
                return false;
            }
            continue;
        }
        let (t0, t1) = ((lo - o) / d, (hi - o) / d);
        let (t0, t1) = if t0 <= t1 { (t0, t1) } else { (t1, t0) };
        near = near.max(t0);
        far = far.min(t1);
        if near > far {
            return false;
        }
    }
    true
}

/// Ближайшее попадание луча в модель через BVH; при вырожденной матрице — через `ray_model`.
///
/// `key` — путь модели в `EditorState::models`; `model` должен быть тем же `Arc`, что лежит по этому пути.
pub fn ray_model_cached(
    cache: &mut PickCache,
    key: &str,
    model: &Arc<Model>,
    ray: Ray,
    model_matrix: Mat4,
) -> Option<f32> {
    let Some(local) = to_local(ray, model_matrix) else {
        return ray_model(ray, model, model_matrix);
    };
    cache
        .get_or_build(key, model)
        .raycast(local.origin, local.direction, local.det_epsilon)
}

/// Кэш BVH по ключу модели.
///
/// Запись хранит `Weak<Model>`: BVH актуален, пока по ключу лежит тот же `Arc`. Перезагрузка даёт
/// новый `Arc`, и BVH пересобирается. Слабая ссылка держит аллокацию, поэтому адрес прежней модели
/// не может достаться новой (ABA исключён).
#[derive(Default)]
pub struct PickCache {
    entries: HashMap<String, (Weak<Model>, Arc<ModelBvh>)>,
}

impl PickCache {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn get_or_build(&mut self, key: &str, model: &Arc<Model>) -> Arc<ModelBvh> {
        if let Some((weak, bvh)) = self.entries.get(key)
            && std::ptr::eq(weak.as_ptr(), Arc::as_ptr(model))
        {
            return bvh.clone();
        }
        self.entries.retain(|_, (weak, _)| weak.strong_count() > 0);
        let bvh = Arc::new(ModelBvh::new(model));
        self.entries
            .insert(key.to_owned(), (Arc::downgrade(model), bvh.clone()));
        bvh
    }
}

/// Выбирает ключ ближайшего из попаданий.
pub fn nearest_hit<K>(hits: impl IntoIterator<Item = (K, f32)>) -> Option<K> {
    hits.into_iter()
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(key, _)| key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_assets::{ModelMesh, ModelVertex};

    fn ray(z: f32, x: f32) -> Ray {
        Ray {
            origin: Vec3::new(x, 0.0, z),
            direction: Vec3::NEG_Z,
        }
    }

    const A: Vec3 = Vec3::new(-1.0, -1.0, 0.0);
    const B: Vec3 = Vec3::new(1.0, -1.0, 0.0);
    const C: Vec3 = Vec3::new(0.0, 1.0, 0.0);

    #[test]
    fn ray_hits_triangle_at_expected_distance_from_either_side() {
        assert_eq!(ray_triangle(ray(5.0, 0.0), A, B, C), Some(5.0));
        assert_eq!(ray_triangle(ray(5.0, 0.0), A, C, B), Some(5.0));
    }

    #[test]
    fn ray_misses_outside_behind_parallel_and_degenerate() {
        assert_eq!(ray_triangle(ray(5.0, 3.0), A, B, C), None);
        assert_eq!(ray_triangle(ray(-5.0, 0.0), A, B, C), None);
        let parallel = Ray {
            origin: Vec3::new(0.0, 0.0, 1.0),
            direction: Vec3::X,
        };
        assert_eq!(ray_triangle(parallel, A, B, C), None);
        assert_eq!(ray_triangle(ray(5.0, 0.0), A, A, C), None);
    }

    fn vertex(position: [f32; 3]) -> ModelVertex {
        ModelVertex {
            position,
            normal: [0.0, 0.0, 1.0],
            uv: [0.0, 0.0],
            tangent: [0.0; 4],
        }
    }

    fn mesh(positions: &[[f32; 3]], indices: Vec<u32>) -> ModelMesh {
        ModelMesh {
            vertices: positions.iter().map(|&p| vertex(p)).collect(),
            indices,
            material: Default::default(),
            skin: None,
        }
    }

    fn model(meshes: Vec<ModelMesh>) -> Model {
        Model {
            meshes,
            textures: Vec::new(),
            skeleton: None,
            animations: Vec::new(),
        }
    }

    fn triangle_model() -> Model {
        model(vec![mesh(
            &[A.to_array(), B.to_array(), C.to_array()],
            vec![0, 1, 2],
        )])
    }

    /// Тот же треугольник, сдвинутый по z.
    fn triangle_at(z: f32) -> Model {
        model(vec![mesh(
            &[[A.x, A.y, z], [B.x, B.y, z], [C.x, C.y, z]],
            vec![0, 1, 2],
        )])
    }

    #[test]
    fn model_hit_uses_the_world_matrix() {
        let model = triangle_model();
        let moved = Mat4::from_translation(Vec3::new(10.0, 0.0, -2.0));
        assert_eq!(ray_model(ray(5.0, 0.0), &model, moved), None);
        assert_eq!(ray_model(ray(5.0, 10.0), &model, moved), Some(7.0));
    }

    #[test]
    fn nearest_hit_prefers_the_smallest_distance() {
        assert_eq!(
            nearest_hit([("far", 9.0), ("near", 1.0), ("mid", 3.0)]),
            Some("near")
        );
        assert_eq!(nearest_hit(Vec::<(&str, f32)>::new()), None);
    }

    /// Сетка 4×4 квадов в плоскости z = 0 и пирамида над ней: два меша, 42 треугольника.
    fn scene_model() -> Model {
        let mut grid = Vec::new();
        for j in 0..=4 {
            for i in 0..=4 {
                grid.push([i as f32, j as f32, 0.0]);
            }
        }
        let mut indices = Vec::new();
        for j in 0..4u32 {
            for i in 0..4u32 {
                let a = j * 5 + i;
                let (b, c) = (a + 1, a + 5);
                let d = c + 1;
                indices.extend([a, b, d, a, d, c]);
            }
        }
        let pyramid = [
            [-1.0, -1.0, -1.0],
            [5.0, -1.0, -1.0],
            [5.0, 5.0, -1.0],
            [-1.0, 5.0, -1.0],
            [2.0, 2.0, 3.0],
        ];
        model(vec![
            mesh(&grid, indices),
            mesh(
                &pyramid,
                vec![0, 1, 2, 0, 2, 3, 0, 1, 4, 1, 2, 4, 2, 3, 4, 3, 0, 4],
            ),
        ])
    }

    fn transforms() -> Vec<Mat4> {
        vec![
            Mat4::IDENTITY,
            Mat4::from_translation(Vec3::new(10.0, -2.0, 3.0)),
            Mat4::from_translation(Vec3::new(1.0, 2.0, -3.0)) * Mat4::from_rotation_y(0.7),
            Mat4::from_translation(Vec3::new(-4.0, 1.0, 0.5))
                * Mat4::from_rotation_x(0.4)
                * Mat4::from_rotation_z(-0.9)
                * Mat4::from_scale(Vec3::new(1.0, 3.0, 0.2)),
            Mat4::from_translation(Vec3::new(2.0, 0.0, 1.0))
                * Mat4::from_scale(Vec3::new(-1.0, 1.0, 1.0)),
            Mat4::from_translation(Vec3::new(0.5, -1.0, 2.0))
                * Mat4::from_rotation_y(-0.3)
                * Mat4::from_scale(Vec3::new(-0.5, 2.0, -1.5)),
        ]
    }

    fn directions() -> [Vec3; 5] {
        [
            Vec3::NEG_Z,
            Vec3::NEG_X,
            Vec3::new(0.0, 0.4, -1.0),
            Vec3::new(0.3, -0.2, -1.0),
            Vec3::new(-0.6, 0.5, 0.0),
        ]
    }

    /// Совпадение с перебором: оба промаха или оба попадания с близким расстоянием.
    fn assert_same_hit(ray: Ray, expected: Option<f32>, actual: Option<f32>) {
        match (expected, actual) {
            (None, None) => {}
            (Some(e), Some(a)) => assert!(
                (e - a).abs() <= 1e-4 * e.abs().max(1.0),
                "луч {ray:?}: перебор {e}, BVH {a}"
            ),
            _ => panic!("луч {ray:?}: перебор {expected:?}, BVH {actual:?}"),
        }
    }

    #[test]
    fn cached_pick_matches_brute_force_on_deterministic_rays() {
        let model = Arc::new(scene_model());
        let mut cache = PickCache::default();
        let (mut hits, mut misses) = (0, 0);
        for matrix in transforms() {
            let center = matrix.transform_point3(Vec3::new(2.0, 2.0, 1.0));
            for direction in directions() {
                for i in -3..=3 {
                    for j in -3..=3 {
                        for k in -3..=3 {
                            let offset = Vec3::new(i as f32, j as f32, k as f32) * 0.7
                                + Vec3::new(0.0317, 0.0523, 0.0711);
                            let ray = Ray {
                                origin: center + offset - direction * 12.0,
                                direction,
                            };
                            let expected = ray_model(ray, &model, matrix);
                            let actual = ray_model_cached(&mut cache, "scene", &model, ray, matrix);
                            assert_same_hit(ray, expected, actual);
                            if expected.is_some() {
                                hits += 1;
                            } else {
                                misses += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(
            hits > 0 && misses > 0,
            "набор лучей должен давать и попадания, и промахи"
        );
    }

    #[test]
    fn small_triangle_under_large_scale_is_hit_like_brute_force() {
        let model = Arc::new(model(vec![mesh(
            &[[0.0, 0.0, 0.0], [1e-5, 0.0, 0.0], [0.0, 1e-5, 0.0]],
            vec![0, 1, 2],
        )]));
        let matrix = Mat4::from_scale(Vec3::splat(100.0));
        let ray = Ray {
            origin: Vec3::new(2.5e-4, 2.5e-4, 1.0),
            direction: Vec3::NEG_Z,
        };
        let expected = ray_model(ray, &model, matrix);
        assert!(expected.is_some_and(|t| (t - 1.0).abs() < 1e-4));
        let actual = ray_model_cached(&mut PickCache::default(), "tiny", &model, ray, matrix);
        assert_same_hit(ray, expected, actual);
    }

    #[test]
    fn triangle_under_small_scale_is_hit_like_brute_force() {
        let model = Arc::new(triangle_model());
        let matrix = Mat4::from_scale(Vec3::splat(0.01));
        let ray = Ray {
            origin: Vec3::new(0.0, 0.0, 1.0),
            direction: Vec3::NEG_Z,
        };
        let expected = ray_model(ray, &model, matrix);
        assert!(expected.is_some_and(|t| (t - 1.0).abs() < 1e-4));
        let actual = ray_model_cached(&mut PickCache::default(), "small", &model, ray, matrix);
        assert_same_hit(ray, expected, actual);
    }

    #[test]
    fn degenerate_matrix_falls_back_to_brute_force() {
        // Масштаб 0 по z сплющивает треугольник в плоскости z = 0, но луч по −Z его всё ещё пересекает.
        let model = Arc::new(triangle_model());
        let matrix = Mat4::from_scale(Vec3::new(1.0, 1.0, 0.0));
        let ray = Ray {
            origin: Vec3::new(0.0, 0.0, 5.0),
            direction: Vec3::NEG_Z,
        };
        let expected = ray_model(ray, &model, matrix);
        assert!(expected.is_some_and(|t| (t - 5.0).abs() < 1e-4));
        let actual = ray_model_cached(&mut PickCache::default(), "flat", &model, ray, matrix);
        assert_same_hit(ray, expected, actual);
    }

    #[test]
    fn empty_model_never_hits() {
        let ray = Ray {
            origin: Vec3::new(0.0, 0.0, 5.0),
            direction: Vec3::NEG_Z,
        };
        let no_meshes = Arc::new(model(Vec::new()));
        let no_indices = Arc::new(model(vec![mesh(
            &[A.to_array(), B.to_array(), C.to_array()],
            Vec::new(),
        )]));
        let mut cache = PickCache::default();
        for empty in [no_meshes, no_indices] {
            assert_eq!(
                ray_model_cached(&mut cache, "empty", &empty, ray, Mat4::IDENTITY),
                None
            );
        }
        assert_eq!(
            ModelBvh::new(&model(Vec::new())).raycast(ray.origin, ray.direction, WORLD_DET_EPSILON),
            None
        );
    }

    #[test]
    fn triangles_with_out_of_range_indices_are_skipped() {
        // Первый треугольник ссылается за пределы вершин и пропускается; второй попадает.
        let model = Arc::new(model(vec![mesh(
            &[A.to_array(), B.to_array(), C.to_array()],
            vec![0, 1, 9, 0, 1, 2],
        )]));
        let ray = ray(5.0, 0.0);
        let actual = ray_model_cached(
            &mut PickCache::default(),
            "broken",
            &model,
            ray,
            Mat4::IDENTITY,
        );
        assert_eq!(actual, Some(5.0));
        assert_same_hit(ray, ray_model(ray, &model, Mat4::IDENTITY), actual);
    }

    #[test]
    fn nearest_instance_wins_between_two_placements_of_one_model() {
        let model = Arc::new(triangle_model());
        let mut cache = PickCache::default();
        let ray = ray(5.0, 0.0);
        let near = ray_model_cached(&mut cache, "tri", &model, ray, Mat4::IDENTITY);
        let far_matrix =
            Mat4::from_translation(Vec3::new(0.0, 0.0, -4.0)) * Mat4::from_scale(Vec3::splat(2.0));
        let far = ray_model_cached(&mut cache, "tri", &model, ray, far_matrix);
        assert_eq!(near, Some(5.0));
        assert_eq!(far, Some(9.0));
        let hits = [("far", far), ("near", near)]
            .into_iter()
            .filter_map(|(key, t)| Some((key, t?)));
        assert_eq!(nearest_hit(hits), Some("near"));
    }

    #[test]
    fn cache_reuses_bvh_for_the_same_model_arc() {
        let model = Arc::new(triangle_model());
        let mut cache = PickCache::default();
        let first = cache.get_or_build("tri", &model);
        let second = cache.get_or_build("tri", &model);
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn reloaded_model_under_same_key_rebuilds_bvh() {
        let old = Arc::new(triangle_model());
        let mut cache = PickCache::default();
        let before = cache.get_or_build("tri", &old);
        let ray = ray(5.0, 0.0);
        assert_eq!(
            ray_model_cached(&mut cache, "tri", &old, ray, Mat4::IDENTITY),
            Some(5.0)
        );

        let new = Arc::new(triangle_at(-2.0));
        let after = cache.get_or_build("tri", &new);
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(
            ray_model_cached(&mut cache, "tri", &new, ray, Mat4::IDENTITY),
            Some(7.0)
        );
    }

    #[test]
    fn cache_drops_entries_of_released_models_on_insert() {
        let mut cache = PickCache::default();
        let released = Arc::new(triangle_model());
        cache.get_or_build("released", &released);
        drop(released);

        let kept = Arc::new(triangle_at(-1.0));
        cache.get_or_build("kept", &kept);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.entries.contains_key("kept"));
    }
}
