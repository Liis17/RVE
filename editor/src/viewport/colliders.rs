//! Рёбра коллайдеров для отрисовки в Scene. Формы и позы берутся из `engine-physics` теми же
//! функциями, что и сцена физики, поэтому линии совпадают с тем, что считает симуляция.

use std::collections::{BTreeSet, HashMap};
use std::f32::consts::{PI, TAU};
use std::sync::Arc;

use engine_assets::Model;
use engine_physics::{
    Hull, ObjectWorldTransform, Pose, Shape, TriMesh, character_capsule_shape,
    primitive_collider_shape,
};
use engine_scene::{CharacterControllerComponent, ColliderComponent, ColliderShape};
use glam::Vec3;

/// Отрезок в мировых координатах.
pub type Segment = [Vec3; 2];

/// Предел рёбер trimesh: сложная геометрия рисуется своим AABB, чтобы не плодить тысячи линий.
pub const MAX_MESH_WIRE_EDGES: usize = 4096;

const CIRCLE_SEGMENTS: usize = 24;
const ARC_SEGMENTS: usize = 12;

/// Какой компонент рисуется; от этого зависит цвет линий.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireStyle {
    Body,
    Trigger,
    Character,
}

/// Рёбра одного коллайдера или капсулы контроллера.
#[derive(Clone, Debug, PartialEq)]
pub struct ColliderWire {
    pub style: WireStyle,
    pub segments: Vec<Segment>,
}

/// Физические компоненты объекта мира, из которых строятся рёбра.
#[derive(Clone)]
pub struct WireSource {
    pub transform: ObjectWorldTransform,
    pub collider: Option<ColliderComponent>,
    pub character: Option<CharacterControllerComponent>,
    /// Модель `mesh_renderer`: из неё строятся выпуклая оболочка и trimesh.
    pub model: Option<String>,
}

/// Поза тела и смещение коллайдера в системе тела; `origin` — `offset × scale`, как при
/// построении сцены физики.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pose: Pose,
    origin: Vec3,
}

impl Placement {
    pub fn new(transform: ObjectWorldTransform, offset: Vec3) -> Self {
        Self {
            pose: transform.pose,
            origin: offset * transform.scale,
        }
    }

    fn point(&self, local: Vec3) -> Vec3 {
        self.pose.position + self.pose.rotation * (self.origin + local)
    }
}

/// Кэш выпуклых оболочек и trimesh по пути модели. Запись действует, пока у модели тот же `Arc`.
#[derive(Default)]
pub struct ColliderCache {
    hulls: HashMap<String, CacheEntry<Hull>>,
    meshes: HashMap<String, CacheEntry<TriMesh>>,
}

struct CacheEntry<T> {
    model: Arc<Model>,
    geometry: Result<Arc<T>, String>,
}

impl ColliderCache {
    /// Выпуклая оболочка модели в её системе координат; масштаб объекта применяется позже.
    pub fn hull(&mut self, path: &str, model: &Arc<Model>) -> Result<Arc<Hull>, String> {
        cached(&mut self.hulls, path, model, |model| {
            Hull::from_points(&model_positions(model)).map_err(|error| error.to_string())
        })
    }

    pub fn mesh(&mut self, path: &str, model: &Arc<Model>) -> Result<Arc<TriMesh>, String> {
        cached(&mut self.meshes, path, model, |model| {
            let (vertices, triangles) = model_triangles(model)?;
            TriMesh::new(vertices, triangles).map_err(|error| error.to_string())
        })
    }
}

fn cached<T>(
    entries: &mut HashMap<String, CacheEntry<T>>,
    path: &str,
    model: &Arc<Model>,
    build: impl FnOnce(&Model) -> Result<T, String>,
) -> Result<Arc<T>, String> {
    if let Some(entry) = entries.get(path)
        && Arc::ptr_eq(&entry.model, model)
    {
        return entry.geometry.clone();
    }
    let geometry = build(model).map(Arc::new);
    entries.insert(
        path.to_owned(),
        CacheEntry {
            model: Arc::clone(model),
            geometry: geometry.clone(),
        },
    );
    geometry
}

fn model_positions(model: &Model) -> Vec<Vec3> {
    model
        .meshes
        .iter()
        .flat_map(|mesh| {
            mesh.vertices
                .iter()
                .map(|vertex| Vec3::from_array(vertex.position))
        })
        .collect()
}

/// Объединяет меши модели так же, как `PhysicsSceneDesc::from_world` для trimesh.
fn model_triangles(model: &Model) -> Result<(Vec<Vec3>, Vec<[u32; 3]>), String> {
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    for mesh in &model.meshes {
        let (chunks, remainder) = mesh.indices.as_chunks::<3>();
        if !remainder.is_empty() {
            return Err("индексы модели не образуют треугольники".to_owned());
        }
        let base =
            u32::try_from(vertices.len()).map_err(|_| "модель слишком большая".to_owned())?;
        vertices.extend(
            mesh.vertices
                .iter()
                .map(|vertex| Vec3::from_array(vertex.position)),
        );
        triangles.extend(
            chunks
                .iter()
                .map(|[a, b, c]| [base + a, base + b, base + c]),
        );
    }
    Ok((vertices, triangles))
}

/// Рёбра всех коллайдеров источника: форма, при неудаче — ошибка для инспектора. Меш, чья модель
/// ещё не загружена, рёбер не даёт.
pub fn source_wires(
    cache: &mut ColliderCache,
    models: &HashMap<String, Arc<Model>>,
    source: &WireSource,
) -> Result<Vec<ColliderWire>, String> {
    let mut wires = Vec::new();
    if let Some(wire) = collider_wire(cache, models, source)? {
        wires.push(wire);
    }
    if let Some(controller) = source.character {
        let shape = character_capsule_shape(controller, source.transform.scale)
            .map_err(|error| error.to_string())?;
        let placement = Placement::new(source.transform, Vec3::ZERO);
        wires.push(ColliderWire {
            style: WireStyle::Character,
            segments: primitive_segments(&shape, &placement),
        });
    }
    Ok(wires)
}

/// Рёбра коллайдера; `None`, если модель меша ещё не загружена.
fn collider_wire(
    cache: &mut ColliderCache,
    models: &HashMap<String, Arc<Model>>,
    source: &WireSource,
) -> Result<Option<ColliderWire>, String> {
    let Some(collider) = source.collider.clone() else {
        return Ok(None);
    };
    let placement = Placement::new(source.transform, Vec3::from_array(collider.offset));
    let scale = source.transform.scale;
    let segments = match collider.shape {
        ColliderShape::ConvexMesh | ColliderShape::TriangleMesh => {
            let Some(path) = source.model.as_deref() else {
                return Err("для формы из модели нужен mesh_renderer".to_owned());
            };
            let Some(model) = models.get(path) else {
                return Ok(None);
            };
            if collider.shape == ColliderShape::ConvexMesh {
                let hull = cache.hull(path, model)?;
                hull_segments(&hull, scale, &placement)
            } else {
                let mesh = cache.mesh(path, model)?;
                mesh_segments(&mesh, scale, &placement, MAX_MESH_WIRE_EDGES)
            }
        }
        ColliderShape::Box | ColliderShape::Sphere | ColliderShape::Capsule => {
            let shape = primitive_collider_shape(collider.clone(), scale)
                .map_err(|error| error.to_string())?;
            primitive_segments(&shape, &placement)
        }
    };
    let style = if collider.trigger {
        WireStyle::Trigger
    } else {
        WireStyle::Body
    };
    Ok(Some(ColliderWire { style, segments }))
}

/// Рёбра примитива (`box`, `sphere`, `capsule`) в системе тела с позицией коллайдера.
pub fn primitive_segments(shape: &Shape, placement: &Placement) -> Vec<Segment> {
    match *shape {
        Shape::Box { half_extents } => {
            box_segments(corners(-half_extents, half_extents), placement)
        }
        Shape::Sphere { radius } => {
            let mut segments = Vec::new();
            for (a, b) in [(Vec3::X, Vec3::Y), (Vec3::Y, Vec3::Z), (Vec3::Z, Vec3::X)] {
                let points = (0..=CIRCLE_SEGMENTS).map(|step| {
                    let angle = TAU * step as f32 / CIRCLE_SEGMENTS as f32;
                    (a * angle.cos() + b * angle.sin()) * radius
                });
                chain(points, placement, &mut segments);
            }
            segments
        }
        Shape::Capsule { radius, height } => capsule_segments(radius, height / 2.0, placement),
        Shape::ConvexHull(_) | Shape::TriMesh(_) => Vec::new(),
    }
}

/// Рёбра выпуклой оболочки, масштабированной как вершины в сцене физики.
pub fn hull_segments(hull: &Hull, scale: Vec3, placement: &Placement) -> Vec<Segment> {
    let points: Vec<Vec3> = hull
        .vertices()
        .iter()
        .map(|vertex| placement.point(*vertex * scale))
        .collect();
    edge_segments(hull.triangles(), &points)
}

/// Рёбра треугольной сетки. Если рёбер больше `max_edges`, возвращается AABB её границ.
pub fn mesh_segments(
    mesh: &TriMesh,
    scale: Vec3,
    placement: &Placement,
    max_edges: usize,
) -> Vec<Segment> {
    let edges = unique_edges(mesh.triangles());
    if edges.len() > max_edges {
        let bounds = mesh.bounds();
        return box_segments(
            corners(bounds.min, bounds.max).map(|corner| corner * scale),
            placement,
        );
    }
    let points: Vec<Vec3> = mesh
        .vertices()
        .iter()
        .map(|vertex| placement.point(*vertex * scale))
        .collect();
    edges
        .into_iter()
        .map(|(a, b)| [points[a as usize], points[b as usize]])
        .collect()
}

/// Восемь углов бокса; бит 0 индекса выбирает X, бит 1 — Y, бит 2 — Z.
fn corners(min: Vec3, max: Vec3) -> [Vec3; 8] {
    std::array::from_fn(|index| {
        let pick = |axis: usize| {
            if index & (1 << axis) == 0 {
                min[axis]
            } else {
                max[axis]
            }
        };
        Vec3::new(pick(0), pick(1), pick(2))
    })
}

fn box_segments(corners: [Vec3; 8], placement: &Placement) -> Vec<Segment> {
    let points = corners.map(|corner| placement.point(corner));
    box_edge_pairs()
        .into_iter()
        .map(|(a, b)| [points[a], points[b]])
        .collect()
}

/// Двенадцать рёбер бокса: вершины отличаются ровно одним битом.
fn box_edge_pairs() -> Vec<(usize, usize)> {
    (0..8)
        .flat_map(|index| (0..3).map(move |axis| (index, index | (1 << axis))))
        .filter(|&(index, other)| index != other)
        .collect()
}

fn capsule_segments(radius: f32, half_height: f32, placement: &Placement) -> Vec<Segment> {
    let mut segments = Vec::new();
    for y in [half_height, -half_height] {
        let points = (0..=CIRCLE_SEGMENTS).map(|step| {
            let angle = TAU * step as f32 / CIRCLE_SEGMENTS as f32;
            Vec3::new(radius * angle.cos(), y, radius * angle.sin())
        });
        chain(points, placement, &mut segments);
    }
    for quarter in 0..4 {
        let angle = quarter as f32 * PI / 2.0;
        let x = radius * angle.cos();
        let z = radius * angle.sin();
        segments.push([
            placement.point(Vec3::new(x, -half_height, z)),
            placement.point(Vec3::new(x, half_height, z)),
        ]);
    }
    // Полусферы: дуги в плоскостях XY и ZY от каждой торцевой окружности наружу.
    for sign in [1.0_f32, -1.0] {
        let arc = |to_point: fn(f32, f32) -> Vec3| {
            (0..=ARC_SEGMENTS).map(move |step| {
                let angle = PI * step as f32 / ARC_SEGMENTS as f32;
                let y = sign * (half_height + radius * angle.sin());
                to_point(radius * angle.cos(), y)
            })
        };
        chain(arc(|x, y| Vec3::new(x, y, 0.0)), placement, &mut segments);
        chain(arc(|z, y| Vec3::new(0.0, y, z)), placement, &mut segments);
    }
    segments
}

fn chain(points: impl Iterator<Item = Vec3>, placement: &Placement, out: &mut Vec<Segment>) {
    let mut previous: Option<Vec3> = None;
    for local in points {
        let point = placement.point(local);
        if let Some(before) = previous {
            out.push([before, point]);
        }
        previous = Some(point);
    }
}

fn unique_edges(triangles: &[[u32; 3]]) -> BTreeSet<(u32, u32)> {
    let mut edges = BTreeSet::new();
    for [a, b, c] in triangles {
        for (from, to) in [(*a, *b), (*b, *c), (*c, *a)] {
            edges.insert((from.min(to), from.max(to)));
        }
    }
    edges
}

fn edge_segments(triangles: &[[u32; 3]], points: &[Vec3]) -> Vec<Segment> {
    unique_edges(triangles)
        .into_iter()
        .map(|(a, b)| [points[a as usize], points[b as usize]])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_assets::{ModelMaterial, ModelMesh, ModelVertex};
    use glam::Quat;

    fn identity() -> Placement {
        Placement {
            pose: Pose::IDENTITY,
            origin: Vec3::ZERO,
        }
    }

    fn distance_to_axis(point: Vec3, half_height: f32) -> f32 {
        let clamped = Vec3::new(0.0, point.y.clamp(-half_height, half_height), 0.0);
        point.distance(clamped)
    }

    #[test]
    fn box_has_twelve_edges_that_follow_pose_and_offset() {
        let placement = Placement {
            pose: Pose::new(
                Vec3::new(1.0, 2.0, 3.0),
                Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            )
            .unwrap(),
            origin: Vec3::new(0.5, 0.0, 0.0),
        };
        let shape = Shape::Box {
            half_extents: Vec3::new(1.0, 2.0, 3.0),
        };
        let segments = primitive_segments(&shape, &placement);
        assert_eq!(segments.len(), 12);
        let half = Vec3::new(1.0, 2.0, 3.0);
        for [a, b] in &segments {
            for point in [a, b] {
                // Обратное преобразование позы и смещения должно дать угол бокса: |x|,|y|,|z| = half.
                let local = placement.pose.rotation.inverse() * (*point - placement.pose.position)
                    - placement.origin;
                assert!((local.abs() - half).abs().max_element() < 1e-5, "{local:?}");
            }
        }
    }

    #[test]
    fn capsule_ends_reach_half_height_plus_radius_and_stay_on_its_axis() {
        let radius = 0.5;
        let half_height = 0.75;
        let segments = capsule_segments(radius, half_height, &identity());
        let top = segments
            .iter()
            .flat_map(|[a, b]| [*a, *b])
            .map(|point| point.y)
            .fold(f32::MIN, f32::max);
        let bottom = segments
            .iter()
            .flat_map(|[a, b]| [*a, *b])
            .map(|point| point.y)
            .fold(f32::MAX, f32::min);
        assert!((top - (half_height + radius)).abs() < 1e-5, "top {top}");
        assert!(
            (bottom + (half_height + radius)).abs() < 1e-5,
            "bottom {bottom}"
        );
        for [a, b] in &segments {
            for point in [a, b] {
                let distance = distance_to_axis(*point, half_height);
                assert!((distance - radius).abs() < 1e-5, "{point:?} at {distance}");
            }
        }
    }

    #[test]
    fn sphere_circles_lie_on_the_radius_in_world_space() {
        let placement = Placement {
            pose: Pose::new(Vec3::new(4.0, 0.0, -1.0), Quat::IDENTITY).unwrap(),
            origin: Vec3::ZERO,
        };
        let segments = primitive_segments(&Shape::Sphere { radius: 2.0 }, &placement);
        assert_eq!(segments.len(), 3 * CIRCLE_SEGMENTS);
        for [a, b] in &segments {
            assert!((a.distance(placement.pose.position) - 2.0).abs() < 1e-5);
            assert!((b.distance(placement.pose.position) - 2.0).abs() < 1e-5);
        }
    }

    #[test]
    fn hull_edges_use_scaled_vertices_in_world_space() {
        let hull = Hull::from_points(&[
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
        ])
        .unwrap();
        let placement = Placement {
            pose: Pose::new(Vec3::new(10.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            origin: Vec3::ZERO,
        };
        let segments = hull_segments(&hull, Vec3::new(2.0, 1.0, 1.0), &placement);
        assert_eq!(segments.len(), 6);
        let max_x = segments
            .iter()
            .flat_map(|[a, b]| [a.x, b.x])
            .fold(f32::MIN, f32::max);
        assert!((max_x - 12.0).abs() < 1e-5, "{max_x}");
    }

    #[test]
    fn mesh_over_the_edge_limit_becomes_its_scaled_aabb() {
        let mesh = TriMesh::new(
            vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(0.0, 1.0, 0.0),
                Vec3::new(0.0, 0.0, 1.0),
            ],
            vec![[0, 1, 2], [0, 2, 3], [0, 3, 1], [1, 3, 2]],
        )
        .unwrap();
        let scale = Vec3::new(2.0, 3.0, 1.0);
        let full = mesh_segments(&mesh, scale, &identity(), MAX_MESH_WIRE_EDGES);
        assert_eq!(full.len(), 6, "tetrahedron has six unique edges");

        let boxed = mesh_segments(&mesh, scale, &identity(), 5);
        assert_eq!(boxed.len(), 12);
        let max = boxed
            .iter()
            .flat_map(|[a, b]| [*a, *b])
            .fold(Vec3::MIN, Vec3::max);
        assert_eq!(max, Vec3::new(2.0, 3.0, 1.0));
    }

    fn model_with_points(points: &[[f32; 3]]) -> Arc<Model> {
        let vertices = points
            .iter()
            .map(|position| ModelVertex {
                position: *position,
                normal: [0.0, 1.0, 0.0],
                uv: [0.0, 0.0],
            })
            .collect();
        Arc::new(Model {
            meshes: vec![ModelMesh {
                vertices,
                indices: Vec::new(),
                material: ModelMaterial::default(),
                skin: None,
            }],
            textures: Vec::new(),
            skeleton: None,
            animations: Vec::new(),
        })
    }

    #[test]
    fn hull_cache_rebuilds_when_the_model_changes() {
        let mut cache = ColliderCache::default();
        let flat = model_with_points(&[[0.0; 3], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]);
        assert!(cache.hull("crate.fbx", &flat).is_err());
        assert!(cache.hull("crate.fbx", &flat).is_err());

        let tetrahedron = model_with_points(&[
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
        ]);
        let hull = cache.hull("crate.fbx", &tetrahedron).unwrap();
        assert_eq!(hull.vertices().len(), 4);
    }

    #[test]
    fn mesh_collider_of_a_model_that_is_not_loaded_draws_nothing() {
        let source = WireSource {
            transform: ObjectWorldTransform {
                pose: Pose::IDENTITY,
                scale: Vec3::ONE,
            },
            collider: Some(ColliderComponent {
                shape: ColliderShape::ConvexMesh,
                ..ColliderComponent::default()
            }),
            character: None,
            model: Some("crate.fbx".to_owned()),
        };
        let mut cache = ColliderCache::default();
        let wires = source_wires(&mut cache, &HashMap::new(), &source).unwrap();
        assert!(wires.is_empty());
    }

    #[test]
    fn source_without_model_for_mesh_shape_reports_missing_renderer() {
        let source = WireSource {
            transform: ObjectWorldTransform {
                pose: Pose::IDENTITY,
                scale: Vec3::ONE,
            },
            collider: Some(ColliderComponent {
                shape: ColliderShape::TriangleMesh,
                ..ColliderComponent::default()
            }),
            character: None,
            model: None,
        };
        let mut cache = ColliderCache::default();
        assert!(source_wires(&mut cache, &HashMap::new(), &source).is_err());
    }
}
