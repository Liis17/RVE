use std::collections::{HashMap, HashSet};
use std::fmt;

use glam::{DVec3, Vec3};

use crate::bvh::TriMesh;
use crate::{Aabb, GeometryError, Pose, Shape};

pub const GJK_MAX_ITERATIONS: usize = 64;
pub const EPA_MAX_ITERATIONS: usize = 64;
pub const EPA_MAX_FACES: usize = 128;
pub const MAX_MANIFOLD_POINTS: usize = 4;
pub const MAX_MESH_CANDIDATES: usize = 16_384;

const BOX_AXIS_EPSILON: f32 = 1.0e-6;
const CONTACT_NORMAL_THRESHOLD: f32 = 0.8;

#[derive(Clone, Debug, PartialEq)]
pub enum NarrowphaseError {
    Geometry(GeometryError),
    NonConvexPair,
    GjkIterationLimit,
    EpaIterationLimit,
    EpaFaceLimit,
    DegenerateSimplex,
    CandidateLimit,
}

impl fmt::Display for NarrowphaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(error) => error.fmt(formatter),
            Self::NonConvexPair => {
                formatter.write_str("narrowphase requires at least one convex shape")
            }
            Self::GjkIterationLimit => formatter.write_str("GJK iteration limit exceeded"),
            Self::EpaIterationLimit => formatter.write_str("EPA iteration limit exceeded"),
            Self::EpaFaceLimit => formatter.write_str("EPA face limit exceeded"),
            Self::DegenerateSimplex => formatter.write_str("narrowphase simplex is degenerate"),
            Self::CandidateLimit => formatter.write_str("convex-mesh candidate limit exceeded"),
        }
    }
}

impl std::error::Error for NarrowphaseError {}

impl From<GeometryError> for NarrowphaseError {
    fn from(value: GeometryError) -> Self {
        Self::Geometry(value)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PairKey {
    pub first: u64,
    pub second: u64,
}

impl PairKey {
    pub fn new(first: u64, second: u64) -> Self {
        if first <= second {
            Self { first, second }
        } else {
            Self {
                first: second,
                second: first,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContactPoint {
    /// Surface point on the first shape in world coordinates.
    pub point_a: Vec3,
    /// Surface point on the second shape in world coordinates.
    pub point_b: Vec3,
    pub position: Vec3,
    pub penetration: f32,
    /// Stable primitive feature when available; zero requests geometric matching.
    pub feature_id: u64,
    pub normal_impulse: f32,
    /// Accumulated world-space tangential impulse for the next solver pass.
    pub tangent_impulse: Vec3,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ContactManifold {
    /// Unit normal from the first shape toward the second shape.
    pub normal: Vec3,
    pub contacts: Vec<ContactPoint>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DistanceResult {
    /// Nonnegative separation; zero means touching or overlapping.
    pub distance: f32,
    /// Unit direction from the first shape toward the second shape.
    pub normal: Vec3,
    pub point_a: Vec3,
    pub point_b: Vec3,
}

#[derive(Clone, Debug)]
struct CachedPoint {
    local_a: Vec3,
    local_b: Vec3,
    feature_id: u64,
    normal_impulse: f32,
    tangent_impulse: Vec3,
}

#[derive(Clone, Debug)]
struct CachedManifold {
    normal: Vec3,
    points: Vec<CachedPoint>,
}

#[derive(Default)]
pub struct ManifoldCache {
    entries: HashMap<PairKey, CachedManifold>,
}

impl ManifoldCache {
    pub fn update(
        &mut self,
        key: PairKey,
        pose_a: Pose,
        pose_b: Pose,
        mut generated: ContactManifold,
    ) -> Result<ContactManifold, NarrowphaseError> {
        let pose_a = Pose::new(pose_a.position, pose_a.rotation)?;
        let pose_b = Pose::new(pose_b.position, pose_b.rotation)?;
        let old = self.entries.get(&key);
        let preserve = old.is_some_and(|entry| entry.normal.dot(generated.normal) >= 0.9);
        let mut used = HashSet::new();
        let mut points = Vec::with_capacity(generated.contacts.len());

        for contact in &mut generated.contacts {
            let local_a = inverse_transform_point(pose_a, contact.point_a);
            let local_b = inverse_transform_point(pose_b, contact.point_b);
            let matching = preserve
                .then(|| {
                    old.and_then(|entry| {
                        entry
                            .points
                            .iter()
                            .enumerate()
                            .filter(|(index, cached)| {
                                !used.contains(index)
                                    && cached.feature_id == contact.feature_id
                                    && cached.local_a.distance_squared(local_a)
                                        + cached.local_b.distance_squared(local_b)
                                        <= 0.02_f32.powi(2)
                            })
                            .min_by(|(_, left), (_, right)| {
                                (left.local_a.distance_squared(local_a)
                                    + left.local_b.distance_squared(local_b))
                                .total_cmp(
                                    &(right.local_a.distance_squared(local_a)
                                        + right.local_b.distance_squared(local_b)),
                                )
                            })
                            .map(|(index, cached)| (index, cached.clone()))
                    })
                })
                .flatten();

            if let Some((index, cached)) = matching {
                used.insert(index);
                contact.normal_impulse = cached.normal_impulse;
                contact.tangent_impulse = cached.tangent_impulse
                    - generated.normal * cached.tangent_impulse.dot(generated.normal);
            }
            points.push(CachedPoint {
                local_a,
                local_b,
                feature_id: contact.feature_id,
                normal_impulse: contact.normal_impulse,
                tangent_impulse: contact.tangent_impulse,
            });
        }
        self.entries.insert(
            key,
            CachedManifold {
                normal: generated.normal,
                points,
            },
        );
        Ok(generated)
    }

    pub fn store_impulses(&mut self, key: PairKey, manifold: &ContactManifold) {
        let Some(cached) = self.entries.get_mut(&key) else {
            return;
        };
        for (point, contact) in cached.points.iter_mut().zip(&manifold.contacts) {
            point.normal_impulse = contact.normal_impulse;
            point.tangent_impulse = contact.tangent_impulse;
        }
    }

    pub fn remove(&mut self, key: PairKey) -> bool {
        self.entries.remove(&key).is_some()
    }

    pub fn retain(&mut self, active: &HashSet<PairKey>) {
        self.entries.retain(|key, _| active.contains(key));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Computes a contact manifold for two convex shapes or a convex shape against a static mesh.
pub fn collide(
    shape_a: &Shape,
    pose_a: Pose,
    shape_b: &Shape,
    pose_b: Pose,
) -> Result<Option<ContactManifold>, NarrowphaseError> {
    let pose_a = Pose::new(pose_a.position, pose_a.rotation)?;
    let pose_b = Pose::new(pose_b.position, pose_b.rotation)?;
    shape_a.local_aabb()?;
    shape_b.local_aabb()?;

    match (shape_a, shape_b) {
        (Shape::TriMesh(_), Shape::TriMesh(_)) => Err(NarrowphaseError::NonConvexPair),
        (Shape::TriMesh(mesh), convex) => collide_convex_mesh(convex, pose_b, mesh, pose_a)
            .map(|manifold| manifold.map(invert_manifold)),
        (convex, Shape::TriMesh(mesh)) => collide_convex_mesh(convex, pose_a, mesh, pose_b),
        (Shape::Sphere { radius: radius_a }, Shape::Sphere { radius: radius_b }) => Ok(
            sphere_sphere(pose_a.position, *radius_a, pose_b.position, *radius_b),
        ),
        (Shape::Sphere { radius }, Shape::Box { half_extents }) => {
            Ok(sphere_box(pose_a.position, *radius, pose_b, *half_extents))
        }
        (Shape::Box { half_extents }, Shape::Sphere { radius }) => {
            Ok(sphere_box(pose_b.position, *radius, pose_a, *half_extents).map(invert_manifold))
        }
        (
            Shape::Box {
                half_extents: half_a,
            },
            Shape::Box {
                half_extents: half_b,
            },
        ) => Ok(box_box(pose_a, *half_a, pose_b, *half_b)),
        (
            Shape::Sphere { radius },
            Shape::Capsule {
                radius: cap_radius,
                height,
            },
        ) => Ok(sphere_capsule(
            pose_a.position,
            *radius,
            pose_b,
            *cap_radius,
            *height,
        )),
        (
            Shape::Capsule {
                radius: cap_radius,
                height,
            },
            Shape::Sphere { radius },
        ) => Ok(
            sphere_capsule(pose_b.position, *radius, pose_a, *cap_radius, *height)
                .map(invert_manifold),
        ),
        (
            Shape::Capsule {
                radius: radius_a,
                height: height_a,
            },
            Shape::Capsule {
                radius: radius_b,
                height: height_b,
            },
        ) => Ok(capsule_capsule(
            pose_a, *radius_a, *height_a, pose_b, *radius_b, *height_b,
        )),
        _ => {
            let a = WorldShape::new(shape_a, pose_a)?;
            let b = WorldShape::new(shape_b, pose_b)?;
            generic_contact(&a, &b, 0).map(|contact| {
                contact.map(|(normal, point)| ContactManifold {
                    normal,
                    contacts: vec![point],
                })
            })
        }
    }
}

/// Returns closest witnesses for two convex shapes using bounded GJK.
pub fn distance(
    shape_a: &Shape,
    pose_a: Pose,
    shape_b: &Shape,
    pose_b: Pose,
) -> Result<DistanceResult, NarrowphaseError> {
    if matches!(shape_a, Shape::TriMesh(_)) || matches!(shape_b, Shape::TriMesh(_)) {
        return Err(NarrowphaseError::NonConvexPair);
    }
    let a = WorldShape::new(shape_a, Pose::new(pose_a.position, pose_a.rotation)?)?;
    let b = WorldShape::new(shape_b, Pose::new(pose_b.position, pose_b.rotation)?)?;
    let result = gjk(&a, &b)?;
    Ok(result.distance)
}

pub(crate) fn convex_triangle_distance(
    shape: &Shape,
    pose: Pose,
    vertices: [Vec3; 3],
) -> Result<DistanceResult, NarrowphaseError> {
    let convex = WorldShape::new(shape, Pose::new(pose.position, pose.rotation)?)?;
    Ok(gjk(&convex, &TriangleProxy { vertices })?.distance)
}

fn sphere_sphere(
    center_a: Vec3,
    radius_a: f32,
    center_b: Vec3,
    radius_b: f32,
) -> Option<ContactManifold> {
    let delta = center_b - center_a;
    let distance = delta.length();
    let depth = radius_a + radius_b - distance;
    let epsilon = linear_epsilon(radius_a.max(radius_b), center_a, center_b);
    if depth < -epsilon {
        return None;
    }
    let normal = if distance > epsilon {
        delta / distance
    } else {
        Vec3::X
    };
    Some(single_contact(
        normal,
        center_a + normal * radius_a,
        center_b - normal * radius_b,
        depth.max(0.0),
        0,
    ))
}

fn sphere_box(
    center: Vec3,
    radius: f32,
    box_pose: Pose,
    half_extents: Vec3,
) -> Option<ContactManifold> {
    let local = box_pose.rotation.inverse() * (center - box_pose.position);
    let closest = local.clamp(-half_extents, half_extents);
    let world_closest = transform_point(box_pose, closest);
    let delta = world_closest - center;
    let distance = delta.length();
    let epsilon = linear_epsilon(
        radius.max(half_extents.max_element()),
        center,
        box_pose.position,
    );
    let inside = local.abs().cmple(half_extents).all();
    if !inside && distance > radius + epsilon {
        return None;
    }

    if inside || distance <= epsilon {
        let (face_axis, face_sign, face_distance) = nearest_box_face(local, half_extents);
        let local_face_normal = axis_vector(face_axis) * face_sign;
        let world_exit = box_pose.rotation * local_face_normal;
        let normal = -world_exit;
        let mut face_point = local;
        face_point[face_axis] = face_sign * half_extents[face_axis];
        let point_b = transform_point(box_pose, face_point);
        let point_a = center + normal * radius;
        return Some(single_contact(
            normal,
            point_a,
            point_b,
            radius + face_distance,
            ((face_axis * 2 + usize::from(face_sign > 0.0)) + 1) as u64,
        ));
    }

    let normal = delta / distance;
    Some(single_contact(
        normal,
        center + normal * radius,
        world_closest,
        (radius - distance).max(0.0),
        0,
    ))
}

fn sphere_capsule(
    sphere_center: Vec3,
    sphere_radius: f32,
    capsule_pose: Pose,
    capsule_radius: f32,
    capsule_height: f32,
) -> Option<ContactManifold> {
    let axis = capsule_pose.rotation * Vec3::Y;
    let half_height = capsule_height * 0.5;
    let start = capsule_pose.position - axis * half_height;
    let end = capsule_pose.position + axis * half_height;
    let closest = closest_point_segment(sphere_center, start, end);
    let delta = closest - sphere_center;
    let distance = delta.length();
    let depth = sphere_radius + capsule_radius - distance;
    let epsilon = linear_epsilon(sphere_radius.max(capsule_radius), sphere_center, closest);
    if depth < -epsilon {
        return None;
    }
    let normal = if distance > epsilon {
        delta / distance
    } else {
        deterministic_axis(capsule_pose.position - sphere_center)
    };
    Some(single_contact(
        normal,
        sphere_center + normal * sphere_radius,
        closest - normal * capsule_radius,
        depth.max(0.0),
        0,
    ))
}

fn capsule_capsule(
    pose_a: Pose,
    radius_a: f32,
    height_a: f32,
    pose_b: Pose,
    radius_b: f32,
    height_b: f32,
) -> Option<ContactManifold> {
    let axis_a = pose_a.rotation * Vec3::Y;
    let axis_b = pose_b.rotation * Vec3::Y;
    let half_a = axis_a * (height_a * 0.5);
    let half_b = axis_b * (height_b * 0.5);
    let (point_a, point_b) = closest_segment_points(
        pose_a.position - half_a,
        pose_a.position + half_a,
        pose_b.position - half_b,
        pose_b.position + half_b,
    );
    let delta = point_b - point_a;
    let distance = delta.length();
    let depth = radius_a + radius_b - distance;
    let epsilon = linear_epsilon(radius_a.max(radius_b), point_a, point_b);
    if depth < -epsilon {
        return None;
    }
    let normal = if distance > epsilon {
        delta / distance
    } else {
        deterministic_axis(pose_b.position - pose_a.position)
    };
    Some(single_contact(
        normal,
        point_a + normal * radius_a,
        point_b - normal * radius_b,
        depth.max(0.0),
        0,
    ))
}

fn box_box(pose_a: Pose, half_a: Vec3, pose_b: Pose, half_b: Vec3) -> Option<ContactManifold> {
    let axes_a = [
        pose_a.rotation * Vec3::X,
        pose_a.rotation * Vec3::Y,
        pose_a.rotation * Vec3::Z,
    ];
    let axes_b = [
        pose_b.rotation * Vec3::X,
        pose_b.rotation * Vec3::Y,
        pose_b.rotation * Vec3::Z,
    ];
    let delta = pose_b.position - pose_a.position;
    let epsilon = linear_epsilon(
        half_a.max_element().max(half_b.max_element()),
        pose_a.position,
        pose_b.position,
    );
    let mut best_depth = f32::INFINITY;
    let mut best_normal = Vec3::X;
    for axis in axes_a.into_iter().chain(axes_b).chain(
        axes_a
            .into_iter()
            .flat_map(|axis_a| axes_b.map(|axis_b| axis_a.cross(axis_b))),
    ) {
        let length_squared = axis.length_squared();
        if length_squared <= BOX_AXIS_EPSILON * BOX_AXIS_EPSILON {
            continue;
        }
        let axis = axis / length_squared.sqrt();
        let radius_a = box_projection_radius(half_a, axes_a, axis);
        let radius_b = box_projection_radius(half_b, axes_b, axis);
        let signed_distance = delta.dot(axis);
        let depth = radius_a + radius_b - signed_distance.abs();
        if depth < -epsilon {
            return None;
        }
        if depth < best_depth {
            best_depth = depth;
            best_normal = if signed_distance >= 0.0 { axis } else { -axis };
        }
    }
    let contacts = box_box_contacts(
        pose_a,
        half_a,
        pose_b,
        half_b,
        best_normal,
        best_depth.max(0.0),
        epsilon,
    );
    Some(ContactManifold {
        normal: best_normal,
        contacts: if contacts.is_empty() {
            vec![support_contact_boxes(
                pose_a,
                half_a,
                pose_b,
                half_b,
                best_normal,
                best_depth.max(0.0),
            )]
        } else {
            reduce_contacts(contacts, epsilon)
        },
    })
}

fn box_box_contacts(
    pose_a: Pose,
    half_a: Vec3,
    pose_b: Pose,
    half_b: Vec3,
    normal: Vec3,
    penetration: f32,
    epsilon: f32,
) -> Vec<ContactPoint> {
    let axes_a = box_axes(pose_a);
    let axes_b = box_axes(pose_b);
    let face_a = (0..3)
        .max_by(|&left, &right| {
            axes_a[left]
                .dot(normal)
                .abs()
                .total_cmp(&axes_a[right].dot(normal).abs())
        })
        .unwrap_or(0);
    let face_b = (0..3)
        .max_by(|&left, &right| {
            axes_b[left]
                .dot(normal)
                .abs()
                .total_cmp(&axes_b[right].dot(normal).abs())
        })
        .unwrap_or(0);
    let alignment_a = axes_a[face_a].dot(normal).abs();
    let alignment_b = axes_b[face_b].dot(normal).abs();
    if alignment_a.max(alignment_b) < 0.98 {
        return vec![support_contact_boxes(
            pose_a,
            half_a,
            pose_b,
            half_b,
            normal,
            penetration,
        )];
    }

    let reference_a = alignment_a >= alignment_b;
    let (reference_pose, reference_half, reference_axes, reference_axis, reference_normal) =
        if reference_a {
            let sign = if axes_a[face_a].dot(normal) < 0.0 {
                -1.0
            } else {
                1.0
            };
            (pose_a, half_a, axes_a, face_a, axes_a[face_a] * sign)
        } else {
            let sign = if axes_b[face_b].dot(-normal) < 0.0 {
                -1.0
            } else {
                1.0
            };
            (pose_b, half_b, axes_b, face_b, axes_b[face_b] * sign)
        };
    let face_sign = if reference_axes[reference_axis].dot(reference_normal) < 0.0 {
        -1.0
    } else {
        1.0
    };
    let reference_center = reference_pose.position
        + reference_axes[reference_axis] * (face_sign * reference_half[reference_axis]);
    let (incident_pose, incident_half, incident_axes, incident_axis, incident_normal) =
        if reference_a {
            let axis = face_b;
            let sign = if axes_b[axis].dot(-normal) < 0.0 {
                -1.0
            } else {
                1.0
            };
            (pose_b, half_b, axes_b, axis, axes_b[axis] * sign)
        } else {
            let axis = face_a;
            let sign = if axes_a[axis].dot(normal) < 0.0 {
                -1.0
            } else {
                1.0
            };
            (pose_a, half_a, axes_a, axis, axes_a[axis] * sign)
        };
    let mut polygon = box_face_vertices(
        incident_pose,
        incident_half,
        incident_axes,
        incident_axis,
        incident_normal,
    );
    for axis in 0..3 {
        if axis == reference_axis {
            continue;
        }
        for sign in [-1.0, 1.0] {
            polygon = clip_polygon_to_face(
                polygon,
                reference_center,
                reference_axes[axis] * sign,
                reference_half[axis],
                epsilon,
            );
        }
    }

    let mut contacts = Vec::new();
    for point in polygon {
        let signed_distance = (point - reference_center).dot(reference_normal);
        if signed_distance > epsilon {
            continue;
        }
        let reference_point = point - reference_normal * signed_distance;
        let (point_a, point_b) = if reference_a {
            (reference_point, point)
        } else {
            (point, reference_point)
        };
        contacts.push(contact_point(
            point_a,
            point_b,
            (-signed_distance).max(0.0),
            0,
        ));
    }
    reduce_contacts(contacts, epsilon)
}

fn box_axes(pose: Pose) -> [Vec3; 3] {
    [
        pose.rotation * Vec3::X,
        pose.rotation * Vec3::Y,
        pose.rotation * Vec3::Z,
    ]
}

fn box_face_vertices(
    pose: Pose,
    half: Vec3,
    axes: [Vec3; 3],
    face_axis: usize,
    face_normal: Vec3,
) -> Vec<Vec3> {
    let sign = if axes[face_axis].dot(face_normal) < 0.0 {
        -1.0
    } else {
        1.0
    };
    let center = pose.position + axes[face_axis] * (sign * half[face_axis]);
    let tangent_axes: Vec<_> = (0..3).filter(|axis| *axis != face_axis).collect();
    let u = axes[tangent_axes[0]] * half[tangent_axes[0]];
    let v = axes[tangent_axes[1]] * half[tangent_axes[1]];
    vec![
        center - u - v,
        center + u - v,
        center + u + v,
        center - u + v,
    ]
}

fn clip_polygon_to_face(
    polygon: Vec<Vec3>,
    origin: Vec3,
    axis: Vec3,
    half_extent: f32,
    epsilon: f32,
) -> Vec<Vec3> {
    if polygon.is_empty() {
        return polygon;
    }
    let mut clipped = Vec::with_capacity(8);
    let mut previous = *polygon.last().expect("nonempty polygon");
    let mut previous_distance = (previous - origin).dot(axis) - half_extent;
    for current in polygon {
        let current_distance = (current - origin).dot(axis) - half_extent;
        let previous_inside = previous_distance <= epsilon;
        let current_inside = current_distance <= epsilon;
        if previous_inside != current_inside {
            let denominator = previous_distance - current_distance;
            if denominator.abs() > f32::MIN_POSITIVE {
                let fraction = previous_distance / denominator;
                clipped.push(previous.lerp(current, fraction.clamp(0.0, 1.0)));
            }
        }
        if current_inside {
            clipped.push(current);
        }
        previous = current;
        previous_distance = current_distance;
    }
    clipped
}

fn support_contact_boxes(
    pose_a: Pose,
    half_a: Vec3,
    pose_b: Pose,
    half_b: Vec3,
    normal: Vec3,
    penetration: f32,
) -> ContactPoint {
    let point_a = transform_point(
        pose_a,
        support_box(half_a, pose_a.rotation.inverse() * normal),
    );
    let point_b = transform_point(
        pose_b,
        support_box(half_b, pose_b.rotation.inverse() * -normal),
    );
    contact_point(point_a, point_b, penetration, 0)
}

fn collide_convex_mesh(
    convex: &Shape,
    convex_pose: Pose,
    mesh: &TriMesh,
    mesh_pose: Pose,
) -> Result<Option<ContactManifold>, NarrowphaseError> {
    let convex_proxy = WorldShape::new(convex, convex_pose)?;
    let mesh_inverse = mesh_pose.rotation.inverse();
    let world_bounds = convex.aabb(convex_pose)?;
    let mesh_local_corners = world_bounds
        .corners()
        .map(|point| mesh_inverse * (point - mesh_pose.position));
    let query = Aabb::from_points(&mesh_local_corners)?;
    let candidates = mesh.query_aabb(query);
    if candidates.len() > MAX_MESH_CANDIDATES {
        return Err(NarrowphaseError::CandidateLimit);
    }

    let mut contacts = Vec::new();
    let mut normal = None;
    for triangle_index in candidates {
        let indices = mesh.triangles()[triangle_index as usize];
        let vertices =
            indices.map(|index| transform_point(mesh_pose, mesh.vertices()[index as usize]));
        let triangle = TriangleProxy { vertices };
        if let Some((triangle_normal, mut contact)) =
            generic_contact(&convex_proxy, &triangle, triangle_index as u64 + 1)?
        {
            if normal.is_none_or(|best: Vec3| best.dot(triangle_normal) < CONTACT_NORMAL_THRESHOLD)
            {
                if normal.is_none_or(|_| {
                    contact.penetration
                        > contacts
                            .first()
                            .map_or(-1.0, |old: &ContactPoint| old.penetration)
                }) {
                    normal = Some(triangle_normal);
                    contacts.clear();
                } else {
                    continue;
                }
            }
            contact.feature_id = (triangle_index as u64 + 1) << 32;
            contacts.push(contact);
        }
    }
    let Some(normal) = normal else {
        return Ok(None);
    };
    let epsilon = linear_epsilon(
        convex_proxy.scale(),
        convex_proxy.center(),
        mesh_pose.position,
    );
    Ok(Some(ContactManifold {
        normal,
        contacts: reduce_contacts(contacts, epsilon),
    }))
}

fn invert_manifold(mut manifold: ContactManifold) -> ContactManifold {
    manifold.normal = -manifold.normal;
    for contact in &mut manifold.contacts {
        std::mem::swap(&mut contact.point_a, &mut contact.point_b);
        contact.tangent_impulse = -contact.tangent_impulse;
    }
    manifold
}

fn single_contact(
    normal: Vec3,
    point_a: Vec3,
    point_b: Vec3,
    penetration: f32,
    feature_id: u64,
) -> ContactManifold {
    ContactManifold {
        normal: normal.normalize_or_zero(),
        contacts: vec![contact_point(point_a, point_b, penetration, feature_id)],
    }
}

fn contact_point(point_a: Vec3, point_b: Vec3, penetration: f32, feature_id: u64) -> ContactPoint {
    ContactPoint {
        point_a,
        point_b,
        position: (point_a + point_b) * 0.5,
        penetration: penetration.max(0.0),
        feature_id,
        normal_impulse: 0.0,
        tangent_impulse: Vec3::ZERO,
    }
}

fn reduce_contacts(contacts: Vec<ContactPoint>, epsilon: f32) -> Vec<ContactPoint> {
    if contacts.is_empty() {
        return Vec::new();
    }
    let center = contacts
        .iter()
        .map(|contact| contact.position)
        .sum::<Vec3>()
        / contacts.len() as f32;
    let mut selected = Vec::with_capacity(MAX_MANIFOLD_POINTS);
    while selected.len() < MAX_MANIFOLD_POINTS {
        let mut best: Option<(usize, f32)> = None;
        for (index, contact) in contacts.iter().enumerate() {
            if selected.contains(&index) {
                continue;
            }
            let distance = if selected.is_empty() {
                contact.position.distance_squared(center)
            } else {
                selected
                    .iter()
                    .map(|&chosen| contact.position.distance_squared(contacts[chosen].position))
                    .fold(f32::INFINITY, f32::min)
            };
            if !selected.is_empty() && distance <= epsilon * epsilon {
                continue;
            }
            if best.is_none_or(|(_, best_distance)| distance > best_distance) {
                best = Some((index, distance));
            }
        }
        let Some((index, _)) = best else { break };
        selected.push(index);
    }
    selected.into_iter().map(|index| contacts[index]).collect()
}

fn box_projection_radius(half: Vec3, axes: [Vec3; 3], axis: Vec3) -> f32 {
    half.x * axis.dot(axes[0]).abs()
        + half.y * axis.dot(axes[1]).abs()
        + half.z * axis.dot(axes[2]).abs()
}

fn nearest_box_face(point: Vec3, half: Vec3) -> (usize, f32, f32) {
    let margins = half - point.abs();
    let axis = if margins.x <= margins.y && margins.x <= margins.z {
        0
    } else if margins.y <= margins.z {
        1
    } else {
        2
    };
    let sign = if point[axis] < 0.0 { -1.0 } else { 1.0 };
    (axis, sign, margins[axis].max(0.0))
}

fn support_box(half: Vec3, direction: Vec3) -> Vec3 {
    Vec3::new(
        if direction.x < 0.0 { -half.x } else { half.x },
        if direction.y < 0.0 { -half.y } else { half.y },
        if direction.z < 0.0 { -half.z } else { half.z },
    )
}

fn axis_vector(axis: usize) -> Vec3 {
    match axis {
        0 => Vec3::X,
        1 => Vec3::Y,
        _ => Vec3::Z,
    }
}

fn closest_point_segment(point: Vec3, start: Vec3, end: Vec3) -> Vec3 {
    let segment = end - start;
    let length_squared = segment.length_squared();
    if length_squared <= f32::MIN_POSITIVE {
        return start;
    }
    start + segment * ((point - start).dot(segment) / length_squared).clamp(0.0, 1.0)
}

fn closest_segment_points(p1: Vec3, q1: Vec3, p2: Vec3, q2: Vec3) -> (Vec3, Vec3) {
    let d1 = q1 - p1;
    let d2 = q2 - p2;
    let r = p1 - p2;
    let a = d1.dot(d1);
    let e = d2.dot(d2);
    let f = d2.dot(r);
    let epsilon = 1.0e-8;
    let (mut s, mut t);
    if a <= epsilon && e <= epsilon {
        return (p1, p2);
    } else if a <= epsilon {
        s = 0.0;
        t = (f / e).clamp(0.0, 1.0);
    } else {
        let c = d1.dot(r);
        if e <= epsilon {
            t = 0.0;
            s = (-c / a).clamp(0.0, 1.0);
        } else {
            let b = d1.dot(d2);
            let denominator = a * e - b * b;
            s = if denominator.abs() > epsilon {
                ((b * f - c * e) / denominator).clamp(0.0, 1.0)
            } else {
                0.0
            };
            t = (b * s + f) / e;
            if t < 0.0 {
                t = 0.0;
                s = (-c / a).clamp(0.0, 1.0);
            } else if t > 1.0 {
                t = 1.0;
                s = ((b - c) / a).clamp(0.0, 1.0);
            }
        }
    }
    (p1 + d1 * s, p2 + d2 * t)
}

fn deterministic_axis(delta: Vec3) -> Vec3 {
    delta.try_normalize().unwrap_or(Vec3::X)
}

fn transform_point(pose: Pose, point: Vec3) -> Vec3 {
    pose.position + pose.rotation * point
}

fn inverse_transform_point(pose: Pose, point: Vec3) -> Vec3 {
    pose.rotation.inverse() * (point - pose.position)
}

fn linear_epsilon(scale: f32, a: Vec3, b: Vec3) -> f32 {
    (scale.abs() * 1.0e-6)
        .max(a.abs().max_element().max(b.abs().max_element()) * f32::EPSILON * 4.0)
        .max(f32::MIN_POSITIVE)
}

trait SupportMap {
    fn support(&self, direction: Vec3) -> Result<Vec3, GeometryError>;
    fn center(&self) -> Vec3;
    fn scale(&self) -> f32;
}

struct WorldShape<'a> {
    shape: &'a Shape,
    pose: Pose,
    scale: f32,
}

impl<'a> WorldShape<'a> {
    fn new(shape: &'a Shape, pose: Pose) -> Result<Self, GeometryError> {
        let bounds = shape.local_aabb()?;
        if matches!(shape, Shape::TriMesh(_)) {
            return Err(GeometryError::InvalidDimensions);
        }
        Ok(Self {
            shape,
            pose,
            scale: bounds.half_extents().max_element().max(f32::MIN_POSITIVE),
        })
    }
}

impl SupportMap for WorldShape<'_> {
    fn support(&self, direction: Vec3) -> Result<Vec3, GeometryError> {
        let local = self.pose.rotation.inverse() * direction;
        let point = self
            .shape
            .support(local)?
            .ok_or(GeometryError::InvalidDimensions)?;
        Ok(transform_point(self.pose, point))
    }

    fn center(&self) -> Vec3 {
        self.pose.position
    }

    fn scale(&self) -> f32 {
        self.scale
    }
}

#[derive(Clone, Copy)]
struct TriangleProxy {
    vertices: [Vec3; 3],
}

impl SupportMap for TriangleProxy {
    fn support(&self, direction: Vec3) -> Result<Vec3, GeometryError> {
        Ok(*self
            .vertices
            .iter()
            .max_by(|left, right| left.dot(direction).total_cmp(&right.dot(direction)))
            .expect("triangle has three vertices"))
    }

    fn center(&self) -> Vec3 {
        self.vertices.iter().copied().sum::<Vec3>() / 3.0
    }

    fn scale(&self) -> f32 {
        self.vertices
            .iter()
            .map(|point| point.distance(self.center()))
            .fold(0.0, f32::max)
            .max(f32::MIN_POSITIVE)
    }
}

#[derive(Clone, Copy, Debug)]
struct SupportPoint {
    difference: Vec3,
    point_a: Vec3,
    point_b: Vec3,
}

impl SupportPoint {
    fn new<A: SupportMap, B: SupportMap>(
        a: &A,
        b: &B,
        direction: Vec3,
    ) -> Result<Self, NarrowphaseError> {
        let point_a = a.support(direction)?;
        let point_b = b.support(-direction)?;
        Ok(Self {
            difference: point_a - point_b,
            point_a,
            point_b,
        })
    }
}

#[derive(Clone)]
struct ClosestSimplex {
    points: Vec<SupportPoint>,
    weights: Vec<f64>,
    difference: DVec3,
}

struct GjkResult {
    distance: DistanceResult,
    simplex: Vec<SupportPoint>,
    intersects: bool,
}

fn gjk<A: SupportMap, B: SupportMap>(a: &A, b: &B) -> Result<GjkResult, NarrowphaseError> {
    let epsilon = linear_epsilon(a.scale().max(b.scale()), a.center(), b.center());
    let mut direction = b.center() - a.center();
    if direction.length_squared() <= epsilon * epsilon {
        direction = Vec3::X;
    }
    let first = SupportPoint::new(a, b, direction)?;
    let mut closest = closest_simplex(&[first])?;

    for _ in 0..GJK_MAX_ITERATIONS {
        let distance_squared = closest.difference.length_squared();
        let epsilon_squared = (epsilon as f64).powi(2);
        if distance_squared <= epsilon_squared {
            let witnesses = witnesses(&closest);
            return Ok(GjkResult {
                distance: DistanceResult {
                    distance: 0.0,
                    normal: (b.center() - a.center()).try_normalize().unwrap_or(Vec3::X),
                    point_a: witnesses.0,
                    point_b: witnesses.1,
                },
                simplex: closest.points,
                intersects: true,
            });
        }

        let direction64 = -closest.difference;
        let direction = direction64.as_vec3();
        let next = SupportPoint::new(a, b, direction)?;
        let next_difference = next.difference.as_dvec3();
        let improvement = next_difference.dot(direction64) - closest.difference.dot(direction64);
        let duplicate = closest.points.iter().any(|point| {
            point
                .difference
                .as_dvec3()
                .distance_squared(next_difference)
                <= epsilon_squared
        });
        if improvement <= epsilon as f64 * direction64.length() || duplicate {
            let distance = distance_squared.sqrt();
            let (point_a, point_b) = witnesses(&closest);
            return Ok(GjkResult {
                distance: DistanceResult {
                    distance: distance as f32,
                    normal: (-closest.difference / distance)
                        .as_vec3()
                        .normalize_or_zero(),
                    point_a,
                    point_b,
                },
                simplex: closest.points,
                intersects: false,
            });
        }

        let previous_difference = closest.difference;
        let mut simplex = closest.points;
        simplex.push(next);
        closest = closest_simplex(&simplex)?;
        // Simplex reduction can discard a support point and repeat with a tiny false improvement.
        // Only treat that stagnation as separation when the support plane proves a positive gap.
        if closest.difference.length_squared() > epsilon_squared
            && closest.difference.distance_squared(previous_difference) <= epsilon_squared
            && next_difference.dot(direction64) < -(epsilon as f64) * direction64.length()
        {
            let distance = closest.difference.length();
            let (point_a, point_b) = witnesses(&closest);
            return Ok(GjkResult {
                distance: DistanceResult {
                    distance: distance as f32,
                    normal: (-closest.difference / distance)
                        .as_vec3()
                        .normalize_or_zero(),
                    point_a,
                    point_b,
                },
                simplex: closest.points,
                intersects: false,
            });
        }
    }
    Err(NarrowphaseError::GjkIterationLimit)
}

fn closest_simplex(points: &[SupportPoint]) -> Result<ClosestSimplex, NarrowphaseError> {
    if points.is_empty() || points.len() > 5 {
        return Err(NarrowphaseError::DegenerateSimplex);
    }
    let mut best: Option<ClosestSimplex> = None;
    let subset_count = 1usize << points.len();
    for mask in 1..subset_count {
        let indices: Vec<_> = (0..points.len())
            .filter(|index| mask & (1 << index) != 0)
            .collect();
        if indices.len() > 4 {
            continue;
        }
        let Some(mut weights) = closest_weights(points, &indices) else {
            continue;
        };
        if weights.iter().any(|weight| *weight < -1.0e-6) {
            continue;
        }
        for weight in &mut weights {
            *weight = weight.max(0.0);
        }
        let sum = weights.iter().sum::<f64>();
        if sum <= f64::MIN_POSITIVE {
            continue;
        }
        for weight in &mut weights {
            *weight /= sum;
        }
        // Keep cancellation in near-zero closest points out of f32 before the next search direction.
        let difference = indices
            .iter()
            .zip(&weights)
            .map(|(index, weight)| points[*index].difference.as_dvec3() * *weight)
            .sum::<DVec3>();
        let candidate = ClosestSimplex {
            points: indices.iter().map(|index| points[*index]).collect(),
            weights,
            difference,
        };
        let replace = best.as_ref().is_none_or(|current| {
            candidate.difference.length_squared() < current.difference.length_squared()
        });
        if replace {
            best = Some(candidate);
        }
    }
    best.ok_or(NarrowphaseError::DegenerateSimplex)
}

fn closest_weights(points: &[SupportPoint], indices: &[usize]) -> Option<Vec<f64>> {
    let count = indices.len();
    if count == 1 {
        return Some(vec![1.0]);
    }
    let dimension = count + 1;
    let mut matrix = [[0.0_f64; 6]; 5];
    for row in 0..count {
        for column in 0..count {
            matrix[row][column] = dot64(
                points[indices[row]].difference,
                points[indices[column]].difference,
            );
        }
        matrix[row][count] = 1.0;
        matrix[row][dimension] = 0.0;
    }
    matrix[count][..count].fill(1.0);
    matrix[count][count] = 0.0;
    matrix[count][dimension] = 1.0;
    let solution = solve_linear(matrix, dimension)?;
    Some(solution[..count].to_vec())
}

fn solve_linear(mut matrix: [[f64; 6]; 5], dimension: usize) -> Option<[f64; 5]> {
    for column in 0..dimension {
        let pivot = (column..dimension).max_by(|&left, &right| {
            matrix[left][column]
                .abs()
                .total_cmp(&matrix[right][column].abs())
        })?;
        if matrix[pivot][column].abs() <= 1.0e-18 {
            return None;
        }
        matrix.swap(column, pivot);
        let divisor = matrix[column][column];
        for value in &mut matrix[column][column..=dimension] {
            *value /= divisor;
        }
        let pivot_row = matrix[column];
        for (row_index, row) in matrix.iter_mut().take(dimension).enumerate() {
            if row_index == column {
                continue;
            }
            let factor = row[column];
            for (entry, value) in row.iter_mut().enumerate().take(dimension + 1).skip(column) {
                *value -= factor * pivot_row[entry];
            }
        }
    }
    let mut solution = [0.0; 5];
    for row in 0..dimension {
        solution[row] = matrix[row][dimension];
    }
    solution
        .iter()
        .take(dimension)
        .all(|value| value.is_finite())
        .then_some(solution)
}

fn witnesses(simplex: &ClosestSimplex) -> (Vec3, Vec3) {
    let point_a = simplex
        .points
        .iter()
        .zip(&simplex.weights)
        .map(|(point, weight)| point.point_a.as_dvec3() * *weight)
        .sum::<DVec3>()
        .as_vec3();
    let point_b = simplex
        .points
        .iter()
        .zip(&simplex.weights)
        .map(|(point, weight)| point.point_b.as_dvec3() * *weight)
        .sum::<DVec3>()
        .as_vec3();
    (point_a, point_b)
}

fn generic_contact<A: SupportMap, B: SupportMap>(
    a: &A,
    b: &B,
    feature_id: u64,
) -> Result<Option<(Vec3, ContactPoint)>, NarrowphaseError> {
    let result = gjk(a, b)?;
    if !result.intersects {
        return Ok(None);
    }
    let epsilon = linear_epsilon(a.scale().max(b.scale()), a.center(), b.center());
    if let Some((mut normal, mut point_a, mut point_b, penetration)) =
        epa(a, b, &result.simplex, epsilon)?
    {
        if (b.center() - a.center()).dot(normal) < 0.0 {
            normal = -normal;
            std::mem::swap(&mut point_a, &mut point_b);
        }
        return Ok(Some((
            normal,
            contact_point(point_a, point_b, penetration, feature_id),
        )));
    }
    Ok(Some((
        result.distance.normal,
        contact_point(
            result.distance.point_a,
            result.distance.point_b,
            0.0,
            feature_id,
        ),
    )))
}

#[derive(Clone, Copy)]
struct EpaFace {
    indices: [usize; 3],
    normal: Vec3,
    distance: f32,
}

fn epa<A: SupportMap, B: SupportMap>(
    a: &A,
    b: &B,
    simplex: &[SupportPoint],
    epsilon: f32,
) -> Result<Option<(Vec3, Vec3, Vec3, f32)>, NarrowphaseError> {
    const MAX_SEED_POINTS: usize = 18;
    const MAX_SEED_TETRAHEDRA: usize = 4_000;

    let mut candidates = simplex.to_vec();
    let directions = [
        Vec3::X,
        -Vec3::X,
        Vec3::Y,
        -Vec3::Y,
        Vec3::Z,
        -Vec3::Z,
        Vec3::new(1.0, 1.0, 1.0),
        Vec3::new(-1.0, 1.0, 1.0),
        Vec3::new(1.0, -1.0, 1.0),
        Vec3::new(1.0, 1.0, -1.0),
        Vec3::new(-1.0, -1.0, 1.0),
        Vec3::new(-1.0, 1.0, -1.0),
        Vec3::new(1.0, -1.0, -1.0),
        Vec3::new(-1.0, -1.0, -1.0),
    ];
    for direction in directions {
        if candidates.len() >= MAX_SEED_POINTS {
            break;
        }
        let point = SupportPoint::new(a, b, direction)?;
        if candidates
            .iter()
            .all(|old| old.difference.distance_squared(point.difference) > epsilon * epsilon)
        {
            candidates.push(point);
        }
    }

    let mut tetrahedron = None;
    let mut best_margin = -f32::INFINITY;
    let mut examined = 0;
    'search: for i in 0..candidates.len() {
        for j in i + 1..candidates.len() {
            for k in j + 1..candidates.len() {
                for l in k + 1..candidates.len() {
                    examined += 1;
                    if examined > MAX_SEED_TETRAHEDRA {
                        return Err(NarrowphaseError::DegenerateSimplex);
                    }
                    let indices = [i, j, k, l];
                    let Some(weights) = tetra_weights(&candidates, indices) else {
                        continue;
                    };
                    let margin = weights.iter().copied().fold(f32::INFINITY, f32::min);
                    if margin > best_margin && margin > 1.0e-6 {
                        best_margin = margin;
                        tetrahedron = Some(indices);
                        if margin > 0.05 {
                            break 'search;
                        }
                    }
                }
            }
        }
    }
    let Some(tetrahedron) = tetrahedron else {
        return Ok(None);
    };
    let mut points: Vec<_> = tetrahedron
        .into_iter()
        .map(|index| candidates[index])
        .collect();
    let tetrahedron = [0, 1, 2, 3];

    let mut faces = Vec::with_capacity(16);
    for (indices, opposite) in [
        (
            [tetrahedron[0], tetrahedron[1], tetrahedron[2]],
            tetrahedron[3],
        ),
        (
            [tetrahedron[0], tetrahedron[3], tetrahedron[1]],
            tetrahedron[2],
        ),
        (
            [tetrahedron[0], tetrahedron[2], tetrahedron[3]],
            tetrahedron[1],
        ),
        (
            [tetrahedron[1], tetrahedron[3], tetrahedron[2]],
            tetrahedron[0],
        ),
    ] {
        if let Some(face) = make_face(&points, indices, Some(opposite), epsilon) {
            faces.push(face);
        }
    }
    if faces.len() != 4 {
        return Ok(None);
    }

    for _ in 0..EPA_MAX_ITERATIONS {
        let Some(face) = faces
            .iter()
            .copied()
            .min_by(|left, right| left.distance.total_cmp(&right.distance))
        else {
            return Err(NarrowphaseError::DegenerateSimplex);
        };
        let support = SupportPoint::new(a, b, face.normal)?;
        let gap = support.difference.dot(face.normal) - face.distance;
        let duplicate = points.iter().any(|point| {
            point.difference.distance_squared(support.difference) <= epsilon * epsilon
        });
        if gap <= epsilon || duplicate {
            return face_contact(&points, face, epsilon).map(Some);
        }

        let new_index = points.len();
        points.push(support);
        let visible: Vec<_> = faces
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate
                    .normal
                    .dot(support.difference - points[candidate.indices[0]].difference)
                    > epsilon
            })
            .map(|(index, _)| index)
            .collect();
        if visible.is_empty() {
            return face_contact(&points, face, epsilon).map(Some);
        }

        let mut horizon = Vec::<[usize; 2]>::new();
        for &face_index in &visible {
            let indices = faces[face_index].indices;
            for edge in [
                [indices[0], indices[1]],
                [indices[1], indices[2]],
                [indices[2], indices[0]],
            ] {
                if let Some(reverse) = horizon.iter().position(|old| *old == [edge[1], edge[0]]) {
                    horizon.swap_remove(reverse);
                } else {
                    horizon.push(edge);
                }
            }
        }
        for index in visible.into_iter().rev() {
            faces.swap_remove(index);
        }
        for edge in horizon {
            let Some(new_face) = make_face(&points, [edge[0], edge[1], new_index], None, epsilon)
            else {
                continue;
            };
            faces.push(new_face);
            if faces.len() > EPA_MAX_FACES {
                return Err(NarrowphaseError::EpaFaceLimit);
            }
        }
        if faces.len() < 4 {
            return Err(NarrowphaseError::DegenerateSimplex);
        }
    }
    Err(NarrowphaseError::EpaIterationLimit)
}

fn tetra_weights(points: &[SupportPoint], indices: [usize; 4]) -> Option<[f32; 4]> {
    let mut matrix = [[0.0_f64; 6]; 5];
    for (row_index, row) in matrix.iter_mut().take(3).enumerate() {
        for (column, index) in indices.iter().enumerate() {
            row[column] = points[*index].difference.to_array()[row_index] as f64;
        }
    }
    matrix[3][..4].fill(1.0);
    matrix[3][4] = 1.0;
    let solution = solve_linear(matrix, 4)?;
    Some([
        solution[0] as f32,
        solution[1] as f32,
        solution[2] as f32,
        solution[3] as f32,
    ])
}

fn make_face(
    points: &[SupportPoint],
    mut indices: [usize; 3],
    opposite: Option<usize>,
    epsilon: f32,
) -> Option<EpaFace> {
    let a = points[indices[0]].difference;
    let mut normal = (points[indices[1]].difference - a)
        .cross(points[indices[2]].difference - a)
        .try_normalize()?;
    if opposite.is_some_and(|opposite| normal.dot(points[opposite].difference - a) > 0.0) {
        indices.swap(1, 2);
        normal = -normal;
    }
    let mut distance = normal.dot(points[indices[0]].difference);
    if opposite.is_none() && distance < 0.0 {
        indices.swap(1, 2);
        normal = -normal;
        distance = -distance;
    }
    if distance < -epsilon {
        return None;
    }
    Some(EpaFace {
        indices,
        normal,
        distance: distance.max(0.0),
    })
}

fn face_contact(
    points: &[SupportPoint],
    face: EpaFace,
    epsilon: f32,
) -> Result<(Vec3, Vec3, Vec3, f32), NarrowphaseError> {
    let a = points[face.indices[0]].difference;
    let b = points[face.indices[1]].difference;
    let c = points[face.indices[2]].difference;
    let projection = face.normal * face.distance;
    let v0 = b - a;
    let v1 = c - a;
    let v2 = projection - a;
    let d00 = v0.dot(v0);
    let d01 = v0.dot(v1);
    let d11 = v1.dot(v1);
    let d20 = v2.dot(v0);
    let d21 = v2.dot(v1);
    let denominator = d00 * d11 - d01 * d01;
    if denominator.abs() <= epsilon * epsilon {
        return Err(NarrowphaseError::DegenerateSimplex);
    }
    let mut weight_b = (d11 * d20 - d01 * d21) / denominator;
    let mut weight_c = (d00 * d21 - d01 * d20) / denominator;
    let mut weight_a = 1.0 - weight_b - weight_c;
    weight_a = weight_a.max(0.0);
    weight_b = weight_b.max(0.0);
    weight_c = weight_c.max(0.0);
    let sum = weight_a + weight_b + weight_c;
    if sum <= f32::MIN_POSITIVE {
        return Err(NarrowphaseError::DegenerateSimplex);
    }
    weight_a /= sum;
    weight_b /= sum;
    weight_c /= sum;
    let weights = [weight_a, weight_b, weight_c];
    let selected = face.indices.map(|index| points[index]);
    let point_a = selected
        .iter()
        .zip(weights)
        .map(|(point, weight)| point.point_a * weight)
        .sum();
    let point_b = selected
        .iter()
        .zip(weights)
        .map(|(point, weight)| point.point_b * weight)
        .sum();
    Ok((face.normal, point_a, point_b, face.distance.max(0.0)))
}

fn dot64(left: Vec3, right: Vec3) -> f64 {
    left.x as f64 * right.x as f64 + left.y as f64 * right.y as f64 + left.z as f64 * right.z as f64
}

#[cfg(test)]
mod tests {
    use super::{ManifoldCache, NarrowphaseError, PairKey, collide, distance};
    use crate::{Hull, Pose, Shape, TriMesh};
    use glam::{EulerRot, Quat, Vec3};
    use std::sync::Arc;

    fn cube() -> Shape {
        let points = [
            Vec3::new(-0.5, -0.5, -0.5),
            Vec3::new(0.5, -0.5, -0.5),
            Vec3::new(-0.5, 0.5, -0.5),
            Vec3::new(0.5, 0.5, -0.5),
            Vec3::new(-0.5, -0.5, 0.5),
            Vec3::new(0.5, -0.5, 0.5),
            Vec3::new(-0.5, 0.5, 0.5),
            Vec3::new(0.5, 0.5, 0.5),
        ];
        Shape::ConvexHull(Arc::new(Hull::from_points(&points).unwrap()))
    }

    fn assert_unit_normal(normal: Vec3) {
        assert!(normal.is_finite());
        assert!((normal.length() - 1.0).abs() < 1.0e-3, "{normal:?}");
    }

    #[test]
    fn every_convex_shape_pair_has_ordered_contacts_and_reversed_normals() {
        let shapes = [
            Shape::sphere(0.5).unwrap(),
            Shape::cuboid(Vec3::splat(0.5)).unwrap(),
            Shape::capsule(0.4, 0.6).unwrap(),
            cube(),
        ];
        let pose_a = Pose::new(Vec3::ZERO, Quat::from_rotation_z(0.13)).unwrap();
        let pose_b = Pose::new(Vec3::new(0.52, 0.03, -0.02), Quat::from_rotation_y(-0.21)).unwrap();

        for (index_a, shape_a) in shapes.iter().enumerate() {
            for (index_b, shape_b) in shapes.iter().enumerate() {
                let forward = collide(shape_a, pose_a, shape_b, pose_b)
                    .unwrap_or_else(|error| panic!("pair ({index_a}, {index_b}): {error}"))
                    .unwrap_or_else(|| panic!("expected overlap for pair ({index_a}, {index_b})"));
                let reverse = collide(shape_b, pose_b, shape_a, pose_a)
                    .unwrap_or_else(|error| panic!("reverse pair ({index_b}, {index_a}): {error}"))
                    .unwrap_or_else(|| {
                        panic!("expected reverse overlap for pair ({index_a}, {index_b})")
                    });
                assert_unit_normal(forward.normal);
                assert_unit_normal(reverse.normal);
                assert!(
                    forward
                        .normal
                        .dot((pose_b.position - pose_a.position).normalize())
                        > 0.0,
                    "{index_a}/{index_b}: {forward:?}"
                );
                assert!(forward.normal.dot(-reverse.normal) > 0.98);
                assert!(!forward.contacts.is_empty() && forward.contacts.len() <= 4);
                assert!(
                    forward
                        .contacts
                        .iter()
                        .all(|contact| contact.penetration > 0.0)
                );
            }
        }
    }

    #[test]
    fn primitive_pairs_handle_touching_inside_coincident_and_rotated_boxes() {
        let sphere = Shape::sphere(0.25).unwrap();
        let box_shape = Shape::cuboid(Vec3::splat(0.5)).unwrap();
        let touching = collide(
            &sphere,
            Pose::new(Vec3::new(0.75, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            &box_shape,
            Pose::IDENTITY,
        )
        .unwrap()
        .unwrap();
        assert!(touching.contacts[0].penetration.abs() < 1.0e-5);
        assert!(touching.normal.dot(-Vec3::X) > 0.99);

        let inside = collide(&sphere, Pose::IDENTITY, &box_shape, Pose::IDENTITY)
            .unwrap()
            .unwrap();
        assert!(inside.contacts[0].penetration > 0.7);
        assert!(inside.normal.dot(-Vec3::X) > 0.99);

        let coincident = collide(
            &sphere,
            Pose::IDENTITY,
            &Shape::sphere(0.5).unwrap(),
            Pose::IDENTITY,
        )
        .unwrap()
        .unwrap();
        assert_unit_normal(coincident.normal);
        assert!((coincident.contacts[0].penetration - 0.75).abs() < 1.0e-5);

        let rotated = collide(
            &box_shape,
            Pose::IDENTITY,
            &box_shape,
            Pose::new(
                Vec3::new(0.6, 0.0, 0.0),
                Quat::from_rotation_z(std::f32::consts::FRAC_PI_4),
            )
            .unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(rotated.normal.dot(Vec3::X) > 0.8);
        assert!(rotated.contacts.len() <= 4);
    }

    #[test]
    fn convex_distance_returns_world_witnesses_and_separated_shapes_miss() {
        let sphere = Shape::sphere(0.5).unwrap();
        let cube_shape = Shape::cuboid(Vec3::splat(0.5)).unwrap();
        let pose_a = Pose::IDENTITY;
        let pose_b = Pose::new(Vec3::new(2.0, 0.0, 0.0), Quat::IDENTITY).unwrap();
        let result = distance(&sphere, pose_a, &cube_shape, pose_b).unwrap();
        assert!((result.distance - 1.0).abs() < 1.0e-4, "{result:?}");
        assert!(result.normal.dot(Vec3::X) > 0.99);
        assert!(((result.point_b - result.point_a).length() - result.distance).abs() < 1.0e-4);
        assert!(
            collide(&sphere, pose_a, &cube_shape, pose_b)
                .unwrap()
                .is_none()
        );
        assert!(
            collide(&cube(), pose_a, &cube_shape, pose_b)
                .unwrap()
                .is_none()
        );

        let mesh = Shape::TriMesh(Arc::new(
            TriMesh::new(vec![Vec3::ZERO, Vec3::X, Vec3::Y], vec![[0, 1, 2]]).unwrap(),
        ));
        assert_eq!(
            distance(&sphere, pose_a, &mesh, pose_b),
            Err(NarrowphaseError::NonConvexPair)
        );
    }

    #[test]
    fn convex_mesh_uses_triangle_geometry_and_bvh_candidates() {
        let mesh = Shape::TriMesh(Arc::new(
            TriMesh::new(
                vec![
                    Vec3::ZERO,
                    Vec3::new(2.0, 0.0, 0.0),
                    Vec3::new(0.0, 2.0, 0.0),
                ],
                vec![[0, 1, 2]],
            )
            .unwrap(),
        ));
        let sphere = Shape::sphere(0.15).unwrap();
        let hit = collide(
            &sphere,
            Pose::new(Vec3::new(0.25, 0.25, -0.1), Quat::IDENTITY).unwrap(),
            &mesh,
            Pose::IDENTITY,
        )
        .unwrap()
        .unwrap();
        assert!(hit.contacts[0].penetration > 0.0);
        assert!(hit.normal.dot(-Vec3::Z).abs() > 0.5);

        let outside_triangle = collide(
            &sphere,
            Pose::new(Vec3::new(1.8, 1.8, 0.0), Quat::IDENTITY).unwrap(),
            &mesh,
            Pose::IDENTITY,
        )
        .unwrap();
        assert!(
            outside_triangle.is_none(),
            "AABB overlap must not create contact"
        );

        let reversed = collide(
            &mesh,
            Pose::IDENTITY,
            &sphere,
            Pose::new(Vec3::new(0.25, 0.25, -0.1), Quat::IDENTITY).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(hit.normal.dot(-reversed.normal) > 0.98);
    }

    #[test]
    fn degenerate_simplex_cases_terminate_for_touching_parallel_and_coincident_hulls() {
        let hull = cube();
        let box_shape = Shape::cuboid(Vec3::splat(0.5)).unwrap();
        let touching = collide(
            &hull,
            Pose::IDENTITY,
            &box_shape,
            Pose::new(Vec3::X, Quat::IDENTITY).unwrap(),
        )
        .unwrap();
        assert!(touching.is_some());
        assert!(touching.unwrap().contacts[0].penetration < 1.0e-3);

        let parallel = collide(
            &hull,
            Pose::IDENTITY,
            &box_shape,
            Pose::new(Vec3::new(1.2, 0.0, 0.0), Quat::IDENTITY).unwrap(),
        )
        .unwrap();
        assert!(parallel.is_none());

        let overlap = collide(
            &hull,
            Pose::IDENTITY,
            &hull,
            Pose::new(Vec3::new(0.2, 0.0, 0.0), Quat::IDENTITY).unwrap(),
        )
        .unwrap()
        .unwrap();
        assert!(overlap.contacts[0].penetration > 0.0);
        assert!(overlap.normal.dot(Vec3::X) > 0.8, "{overlap:?}");
    }

    #[test]
    fn manifold_cache_preserves_local_anchors_and_impulses() {
        let sphere = Shape::sphere(0.5).unwrap();
        let pose_a = Pose::IDENTITY;
        let pose_b = Pose::new(Vec3::new(0.9, 0.0, 0.0), Quat::IDENTITY).unwrap();
        let initial = collide(&sphere, pose_a, &sphere, pose_b).unwrap().unwrap();
        let key = PairKey::new(4, 9);
        let mut cache = ManifoldCache::default();
        let mut first = cache.update(key, pose_a, pose_b, initial).unwrap();
        first.contacts[0].normal_impulse = 1.25;
        first.contacts[0].tangent_impulse = Vec3::Y * 0.2;
        cache.store_impulses(key, &first);

        let moved_a = Pose::new(Vec3::new(0.01, 0.0, 0.0), Quat::IDENTITY).unwrap();
        let moved_b = Pose::new(Vec3::new(0.91, 0.0, 0.0), Quat::IDENTITY).unwrap();
        let second = cache
            .update(
                key,
                moved_a,
                moved_b,
                collide(&sphere, moved_a, &sphere, moved_b)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(second.contacts[0].normal_impulse, 1.25);
        assert!(
            second.contacts[0]
                .tangent_impulse
                .abs_diff_eq(Vec3::Y * 0.2, 1.0e-5)
        );
        assert!((second.contacts[0].point_a.x - 0.51).abs() < 1.0e-5);
    }

    #[test]
    fn rotated_cube_hulls_resolve_nearby_separation_and_overlap() {
        let hull = cube();
        let separated_pose = Pose::new(
            Vec3::new(-0.804_056_2, -0.969_654_3, -0.700_818_3),
            Quat::from_xyzw(0.040_864_84, 0.080_302_73, 0.444_840_82, 0.891_065_8),
        )
        .unwrap();
        let box_shape = Shape::cuboid(Vec3::splat(0.5)).unwrap();

        assert!(
            hull.aabb(Pose::IDENTITY)
                .unwrap()
                .overlaps(hull.aabb(separated_pose).unwrap())
        );
        assert!(
            collide(&box_shape, Pose::IDENTITY, &box_shape, separated_pose)
                .unwrap()
                .is_none()
        );
        let hull_collision = collide(&hull, Pose::IDENTITY, &hull, separated_pose);
        let hull_distance = distance(&hull, Pose::IDENTITY, &hull, separated_pose);
        assert!(matches!(&hull_collision, Ok(None)), "{hull_collision:?}");
        assert!(hull_distance.is_ok(), "{hull_distance:?}");
        let separated = hull_distance.unwrap();
        assert!(separated.distance > 0.0, "{separated:?}");
        assert!(
            (separated.distance - 0.006_76).abs() < 1.0e-4,
            "{separated:?}"
        );
        assert!(
            ((separated.point_b - separated.point_a).length() - separated.distance).abs() < 1.0e-4,
            "{separated:?}"
        );

        for factor in [0.9, 0.5, 0.1] {
            let nearby_pose =
                Pose::new(separated_pose.position * factor, separated_pose.rotation).unwrap();
            let manifold = collide(&hull, Pose::IDENTITY, &hull, nearby_pose)
                .unwrap()
                .unwrap_or_else(|| panic!("expected rotated hull overlap at {factor}"));
            assert!(!manifold.contacts.is_empty());
            assert!(
                manifold
                    .contacts
                    .iter()
                    .all(|contact| contact.penetration > 0.0)
            );
        }
    }

    #[test]
    fn near_degenerate_rotated_cube_hulls_return_accurate_separation() {
        let hull = cube();
        let pose = Pose::new(
            Vec3::new(-0.746_299_9, -0.981_468_4, -0.648_807_1),
            Quat::from_xyzw(0.050_731_134, 0.061_450_034, 0.442_501, 0.893_220_66),
        )
        .unwrap();
        let box_shape = Shape::cuboid(Vec3::splat(0.5)).unwrap();

        assert!(
            hull.aabb(Pose::IDENTITY)
                .unwrap()
                .overlaps(hull.aabb(pose).unwrap())
        );
        assert!(
            collide(&box_shape, Pose::IDENTITY, &box_shape, pose)
                .unwrap()
                .is_none()
        );
        let collision = collide(&hull, Pose::IDENTITY, &hull, pose);
        let separation = distance(&hull, Pose::IDENTITY, &hull, pose);
        assert!(matches!(&collision, Ok(None)), "{collision:?}");
        assert!(separation.is_ok(), "{separation:?}");
        let separation = separation.unwrap();
        assert!(
            (separation.distance - 0.000_088_199).abs() < 1.0e-6,
            "{separation:?}"
        );
        assert!(
            ((separation.point_b - separation.point_a).length() - separation.distance).abs()
                < 1.0e-6,
            "{separation:?}"
        );

        let near_touch_pose = Pose::new(
            pose.position - separation.normal * (separation.distance - 1.0e-5),
            pose.rotation,
        )
        .unwrap();
        assert!(
            collide(&box_shape, Pose::IDENTITY, &box_shape, near_touch_pose)
                .unwrap()
                .is_none()
        );
        assert!(
            collide(&hull, Pose::IDENTITY, &hull, near_touch_pose)
                .unwrap()
                .is_none()
        );

        let touching_pose = Pose::new(
            pose.position - separation.normal * separation.distance,
            pose.rotation,
        )
        .unwrap();
        assert!(
            collide(&box_shape, Pose::IDENTITY, &box_shape, touching_pose)
                .unwrap()
                .is_some()
        );
        assert!(
            collide(&hull, Pose::IDENTITY, &hull, touching_pose)
                .unwrap()
                .is_some()
        );

        let nearby_overlap_pose = Pose::new(
            pose.position - separation.normal * (separation.distance + 1.0e-4),
            pose.rotation,
        )
        .unwrap();
        assert!(
            collide(&box_shape, Pose::IDENTITY, &box_shape, nearby_overlap_pose)
                .unwrap()
                .is_some()
        );
        assert!(
            collide(&hull, Pose::IDENTITY, &hull, nearby_overlap_pose)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn rotated_cube_hull_gjk_matches_box_sat_for_deterministic_poses() {
        let hull = cube();
        let box_shape = Shape::cuboid(Vec3::splat(0.5)).unwrap();
        let mut state = 0x8f31_4a27_u32;
        let mut overlaps = 0;
        let mut separations = 0;

        for _ in 0..96 {
            let mut sample = || {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 8) as f32 / (1_u32 << 24) as f32
            };
            let position = Vec3::new(
                (sample() * 2.0 - 1.0) * 1.7,
                (sample() * 2.0 - 1.0) * 1.7,
                (sample() * 2.0 - 1.0) * 1.7,
            );
            let rotation = Quat::from_euler(
                EulerRot::XYZ,
                (sample() * 2.0 - 1.0) * std::f32::consts::PI,
                (sample() * 2.0 - 1.0) * std::f32::consts::PI,
                (sample() * 2.0 - 1.0) * std::f32::consts::PI,
            );
            let pose = Pose::new(position, rotation).unwrap();
            let sat_overlap = collide(&box_shape, Pose::IDENTITY, &box_shape, pose)
                .unwrap()
                .is_some();
            let hull_overlap = collide(&hull, Pose::IDENTITY, &hull, pose)
                .unwrap_or_else(|error| panic!("pose {pose:?}: {error}"))
                .is_some();
            assert_eq!(hull_overlap, sat_overlap, "pose {pose:?}");
            if sat_overlap {
                overlaps += 1;
            } else {
                separations += 1;
                let result = distance(&hull, Pose::IDENTITY, &hull, pose)
                    .unwrap_or_else(|error| panic!("pose {pose:?}: {error}"));
                assert!(
                    result.distance.is_finite() && result.distance > 0.0,
                    "{result:?}"
                );
                assert!(
                    ((result.point_b - result.point_a).length() - result.distance).abs() < 1.0e-5,
                    "{result:?}"
                );
            }
        }

        assert!(overlaps > 20, "only {overlaps} overlap poses");
        assert!(separations > 20, "only {separations} separated poses");
    }

    #[test]
    fn manifold_cache_keeps_fresh_witnesses_on_a_shared_mesh_triangle() {
        let sphere = Shape::sphere(0.5).unwrap();
        let mesh = Shape::TriMesh(Arc::new(
            TriMesh::new(
                vec![
                    Vec3::new(-100.0, 0.0, -100.0),
                    Vec3::new(0.0, 0.0, 100.0),
                    Vec3::new(100.0, 0.0, -100.0),
                ],
                vec![[0, 1, 2]],
            )
            .unwrap(),
        ));
        let key = PairKey::new(4, 9);
        let mut cache = ManifoldCache::default();

        for (index, x) in [0.0, 0.01, 1.0, 5.0].into_iter().enumerate() {
            let sphere_pose = Pose::new(Vec3::new(x, 0.49, 0.0), Quat::IDENTITY).unwrap();
            let fresh = collide(&sphere, sphere_pose, &mesh, Pose::IDENTITY)
                .unwrap()
                .unwrap();
            let fresh_witness = fresh.contacts[0].point_b;
            assert!((fresh_witness.x - x).abs() < 1.0e-3, "{fresh:?}");

            let updated = cache
                .update(key, sphere_pose, Pose::IDENTITY, fresh)
                .unwrap();
            let cached_witness = updated.contacts[0].point_b;
            assert!(
                (cached_witness.x - x).abs() < 1.0e-3,
                "x={x}, witness={cached_witness:?}"
            );
            if index == 0 {
                let mut with_impulses = updated;
                with_impulses.contacts[0].normal_impulse = 1.25;
                with_impulses.contacts[0].tangent_impulse = Vec3::X * 0.2;
                cache.store_impulses(key, &with_impulses);
            } else if index == 1 {
                assert_eq!(updated.contacts[0].normal_impulse, 1.25);
                assert!(
                    updated.contacts[0]
                        .tangent_impulse
                        .abs_diff_eq(Vec3::X * 0.2, 1e-5)
                );
            } else {
                assert_eq!(updated.contacts[0].normal_impulse, 0.0);
                assert_eq!(updated.contacts[0].tangent_impulse, Vec3::ZERO);
            }
        }
    }
}
