//! Выбор объекта лучом по треугольникам CPU-моделей.

use super::camera::Ray;
use engine_assets::Model;
use glam::{Mat4, Vec3};

/// Расстояние вдоль луча до треугольника по алгоритму Мёллера — Трумбора.
///
/// Вырожденные треугольники и попадания позади начала луча дают `None`.
pub fn ray_triangle(ray: Ray, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let edge1 = b - a;
    let edge2 = c - a;
    let p = ray.direction.cross(edge2);
    let determinant = edge1.dot(p);
    if determinant.abs() < 1e-10 {
        return None;
    }
    let inverse = 1.0 / determinant;
    let offset = ray.origin - a;
    let u = offset.dot(p) * inverse;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = offset.cross(edge1);
    let v = ray.direction.dot(q) * inverse;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = edge2.dot(q) * inverse;
    (t >= 0.0).then_some(t)
}

/// Ближайшее попадание луча в модель, размещённую матрицей `model_matrix`.
pub fn ray_model(ray: Ray, model: &Model, model_matrix: Mat4) -> Option<f32> {
    let mut nearest: Option<f32> = None;
    for mesh in &model.meshes {
        for triangle in mesh.indices.chunks_exact(3) {
            let vertex = |index: u32| {
                model_matrix
                    .transform_point3(Vec3::from_array(mesh.vertices[index as usize].position))
            };
            if let Some(t) = ray_triangle(
                ray,
                vertex(triangle[0]),
                vertex(triangle[1]),
                vertex(triangle[2]),
            ) && nearest.is_none_or(|best| t < best)
            {
                nearest = Some(t);
            }
        }
    }
    nearest
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

    fn triangle_model() -> Model {
        let vertex = |position: Vec3| ModelVertex {
            position: position.to_array(),
            normal: [0.0, 0.0, 1.0],
            uv: [0.0, 0.0],
        };
        Model {
            meshes: vec![ModelMesh {
                vertices: vec![vertex(A), vertex(B), vertex(C)],
                indices: vec![0, 1, 2],
                texture: None,
            }],
            textures: Vec::new(),
        }
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
}
