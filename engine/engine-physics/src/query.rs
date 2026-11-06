use std::fmt;

use glam::Vec3;

use crate::broad::MAX_BROADPHASE_ITEMS;
use crate::narrow::{MAX_MESH_CANDIDATES, convex_triangle_distance};
use crate::ray::{Ray, raycast};
use crate::world::{BodyHandle, BodyKind};
use crate::{Aabb, DistanceResult, GeometryError, NarrowphaseError, Pose, Shape, TriMesh};

pub const MAX_QUERY_CANDIDATES: usize = MAX_BROADPHASE_ITEMS;
pub const MAX_SHAPE_CAST_ITERATIONS: usize = 32;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct QueryFilter {
    pub ignore: Option<BodyHandle>,
    pub include_triggers: bool,
}

/// Additional body exclusions and enabled-state overrides applied over a snapshot.
#[derive(Clone, Copy, Debug, Default)]
pub struct QueryOverrides<'a> {
    pub excluded: &'a [BodyHandle],
    /// Entries may be in any order; the last matching body entry wins.
    pub enabled: &'a [(BodyHandle, bool)],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueryHit {
    pub body: BodyHandle,
    pub point: Vec3,
    /// For raycasts this is the outward surface normal; for shape casts it points from the cast shape to the obstacle.
    pub normal: Vec3,
    /// Ray distance or cast travel distance in world units.
    pub distance: f32,
    /// Fraction of the requested cast displacement; raycasts use distance / max_distance.
    pub fraction: f32,
    pub triangle_index: Option<u32>,
    pub body_kind: BodyKind,
    pub body_velocity: Vec3,
    /// Dynamic body mass, or zero for static and kinematic bodies.
    pub mass: f32,
    pub trigger: bool,
}

/// Immutable body metadata captured with a query snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueryBodyState {
    pub pose: Pose,
    pub enabled: bool,
    pub trigger: bool,
    pub body_kind: BodyKind,
    pub body_velocity: Vec3,
    pub mass: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum QueryError {
    Geometry(GeometryError),
    Narrowphase(NarrowphaseError),
    CandidateLimit,
    ShapeCastIterationLimit,
    NonConvexCast,
    InvalidCast,
}

impl fmt::Display for QueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Geometry(error) => error.fmt(formatter),
            Self::Narrowphase(error) => error.fmt(formatter),
            Self::CandidateLimit => formatter.write_str("query candidate limit exceeded"),
            Self::ShapeCastIterationLimit => {
                formatter.write_str("shape cast iteration limit exceeded")
            }
            Self::NonConvexCast => formatter.write_str("shape casts require a convex shape"),
            Self::InvalidCast => formatter.write_str("shape cast input is invalid"),
        }
    }
}

impl std::error::Error for QueryError {}

impl From<GeometryError> for QueryError {
    fn from(value: GeometryError) -> Self {
        Self::Geometry(value)
    }
}

impl From<NarrowphaseError> for QueryError {
    fn from(value: NarrowphaseError) -> Self {
        Self::Narrowphase(value)
    }
}

#[derive(Clone)]
pub struct QuerySnapshot {
    colliders: Vec<QueryCollider>,
    x_order: Vec<usize>,
}

#[derive(Clone)]
pub(crate) struct QueryCollider {
    pub(crate) body: BodyHandle,
    pub(crate) shape: Shape,
    pub(crate) pose: Pose,
    bounds: Aabb,
    pub(crate) enabled: bool,
    pub(crate) trigger: bool,
    pub(crate) body_kind: BodyKind,
    pub(crate) body_velocity: Vec3,
    pub(crate) mass: f32,
}

pub(crate) struct QueryColliderDesc {
    pub(crate) body: BodyHandle,
    pub(crate) shape: Shape,
    pub(crate) pose: Pose,
    pub(crate) enabled: bool,
    pub(crate) trigger: bool,
    pub(crate) body_kind: BodyKind,
    pub(crate) body_velocity: Vec3,
    pub(crate) mass: f32,
}

impl QueryCollider {
    pub(crate) fn new(desc: QueryColliderDesc) -> Result<Self, GeometryError> {
        Ok(Self {
            body: desc.body,
            bounds: desc.shape.aabb(desc.pose)?,
            shape: desc.shape,
            pose: desc.pose,
            enabled: desc.enabled,
            trigger: desc.trigger,
            body_kind: desc.body_kind,
            body_velocity: desc.body_velocity,
            mass: desc.mass,
        })
    }
}

impl QuerySnapshot {
    pub(crate) fn from_colliders(colliders: Vec<QueryCollider>) -> Result<Self, QueryError> {
        if colliders.len() > MAX_QUERY_CANDIDATES {
            return Err(QueryError::CandidateLimit);
        }
        let mut x_order = (0..colliders.len()).collect::<Vec<_>>();
        x_order.sort_by(|&left, &right| {
            colliders[left]
                .bounds
                .min
                .x
                .total_cmp(&colliders[right].bounds.min.x)
                .then_with(|| colliders[left].body.cmp(&colliders[right].body))
        });
        Ok(Self { colliders, x_order })
    }

    pub fn raycast(&self, ray: &Ray, filter: QueryFilter) -> Result<Option<QueryHit>, QueryError> {
        self.raycast_excluding(ray, filter, &[])
    }

    /// Raycasts while excluding additional bodies, for transactional collider toggles.
    pub fn raycast_excluding(
        &self,
        ray: &Ray,
        filter: QueryFilter,
        excluded: &[BodyHandle],
    ) -> Result<Option<QueryHit>, QueryError> {
        self.raycast_with_enabled_overrides(ray, filter, excluded, &[])
    }

    /// Raycasts with script-staged collider enabled states applied over the snapshot.
    pub fn raycast_with_enabled_overrides(
        &self,
        ray: &Ray,
        filter: QueryFilter,
        excluded: &[BodyHandle],
        enabled_overrides: &[(BodyHandle, bool)],
    ) -> Result<Option<QueryHit>, QueryError> {
        if !ray.origin.is_finite()
            || !ray.direction.is_finite()
            || !ray.max_distance.is_finite()
            || ray.max_distance < 0.0
            || (ray.direction.length_squared() - 1.0).abs() > 1.0e-4
        {
            return Err(GeometryError::InvalidRay.into());
        }
        let end = ray.point_at(ray.max_distance);
        let broad_bounds = Aabb::from_points(&[ray.origin, end])?;
        let mut closest = None;
        let overrides = QueryOverrides {
            excluded,
            enabled: enabled_overrides,
        };
        for index in self.candidates(broad_bounds, filter, overrides)? {
            let collider = &self.colliders[index];
            let Some(hit) = raycast(ray, &collider.shape, collider.pose)? else {
                continue;
            };
            let fraction = if ray.max_distance > 0.0 {
                hit.distance / ray.max_distance
            } else {
                0.0
            };
            let candidate = QueryHit {
                body: collider.body,
                point: hit.point,
                normal: hit.normal,
                distance: hit.distance,
                fraction,
                triangle_index: hit.triangle_index,
                body_kind: collider.body_kind,
                body_velocity: collider.body_velocity,
                mass: collider.mass,
                trigger: collider.trigger,
            };
            if closest.is_none_or(|old: QueryHit| query_hit_precedes(candidate, old)) {
                closest = Some(candidate);
            }
        }
        Ok(closest)
    }

    pub fn shape_cast(
        &self,
        shape: &Shape,
        pose: Pose,
        displacement: Vec3,
        skin: f32,
        filter: QueryFilter,
    ) -> Result<Option<QueryHit>, QueryError> {
        self.shape_cast_excluding(shape, pose, displacement, skin, filter, &[])
    }

    /// Shape-casts while excluding additional bodies, for transactional collider toggles.
    pub fn shape_cast_excluding(
        &self,
        shape: &Shape,
        pose: Pose,
        displacement: Vec3,
        skin: f32,
        filter: QueryFilter,
        excluded: &[BodyHandle],
    ) -> Result<Option<QueryHit>, QueryError> {
        self.shape_cast_with_overrides(
            shape,
            pose,
            displacement,
            skin,
            filter,
            QueryOverrides {
                excluded,
                enabled: &[],
            },
        )
    }

    /// Shape-casts with caller-provided exclusions and enabled states applied over the snapshot.
    pub fn shape_cast_with_overrides(
        &self,
        shape: &Shape,
        pose: Pose,
        displacement: Vec3,
        skin: f32,
        filter: QueryFilter,
        overrides: QueryOverrides<'_>,
    ) -> Result<Option<QueryHit>, QueryError> {
        if !displacement.is_finite() || !skin.is_finite() || skin < 0.0 {
            return Err(QueryError::InvalidCast);
        }
        if matches!(shape, Shape::TriMesh(_)) {
            return Err(QueryError::NonConvexCast);
        }
        let pose = Pose::new(pose.position, pose.rotation)?;
        let start_bounds = shape.aabb(pose)?;
        let end_pose = Pose::new(pose.position + displacement, pose.rotation)?;
        let end_bounds = shape.aabb(end_pose)?;
        let sweep_bounds = expand_aabb(start_bounds.union(end_bounds), skin)?;
        let mut closest = None;

        for index in self.candidates(sweep_bounds, filter, overrides)? {
            let collider = &self.colliders[index];
            let cast = match &collider.shape {
                Shape::TriMesh(mesh) => cast_against_mesh(
                    shape,
                    pose,
                    displacement,
                    skin,
                    collider.pose,
                    mesh,
                    sweep_bounds,
                )?,
                other => conservative_advance(displacement, skin, |offset| {
                    let cast_pose = Pose::new(pose.position + offset, pose.rotation)?;
                    crate::distance(shape, cast_pose, other, collider.pose).map_err(Into::into)
                })?
                .map(|(fraction, distance)| (fraction, distance, None)),
            };
            let Some((fraction, distance, triangle_index)) = cast else {
                continue;
            };
            let candidate = QueryHit {
                body: collider.body,
                point: distance.point_b,
                normal: distance.normal,
                distance: displacement.length() * fraction,
                fraction,
                triangle_index,
                body_kind: collider.body_kind,
                body_velocity: collider.body_velocity,
                mass: collider.mass,
                trigger: collider.trigger,
            };
            if closest.is_none_or(|old: QueryHit| query_hit_precedes(candidate, old)) {
                closest = Some(candidate);
            }
        }
        Ok(closest)
    }

    pub fn body_state(&self, body: BodyHandle) -> Option<QueryBodyState> {
        self.colliders
            .iter()
            .find(|collider| collider.body == body)
            .map(|collider| QueryBodyState {
                pose: collider.pose,
                enabled: collider.enabled,
                trigger: collider.trigger,
                body_kind: collider.body_kind,
                body_velocity: collider.body_velocity,
                mass: collider.mass,
            })
    }

    fn candidates(
        &self,
        query: Aabb,
        filter: QueryFilter,
        overrides: QueryOverrides<'_>,
    ) -> Result<Vec<usize>, QueryError> {
        let mut candidates = Vec::new();
        for &index in &self.x_order {
            let collider = &self.colliders[index];
            if collider.bounds.min.x > query.max.x {
                break;
            }
            let enabled = overrides
                .enabled
                .iter()
                .rev()
                .find_map(|(body, enabled)| (*body == collider.body).then_some(*enabled))
                .unwrap_or(collider.enabled);
            if enabled
                && collider.bounds.max.x >= query.min.x
                && collider.bounds.overlaps(query)
                && filter.ignore != Some(collider.body)
                && !overrides.excluded.contains(&collider.body)
                && (filter.include_triggers || !collider.trigger)
            {
                if candidates.len() == MAX_QUERY_CANDIDATES {
                    return Err(QueryError::CandidateLimit);
                }
                candidates.push(index);
            }
        }
        Ok(candidates)
    }
}

fn cast_against_mesh(
    shape: &Shape,
    pose: Pose,
    displacement: Vec3,
    skin: f32,
    mesh_pose: Pose,
    mesh: &TriMesh,
    sweep_bounds: Aabb,
) -> Result<Option<(f32, DistanceResult, Option<u32>)>, QueryError> {
    let mesh_inverse = mesh_pose.rotation.inverse();
    let local_corners = sweep_bounds
        .corners()
        .map(|point| mesh_inverse * (point - mesh_pose.position));
    let local_bounds = Aabb::from_points(&local_corners)?;
    let candidates = mesh.query_aabb(local_bounds);
    if candidates.len() > MAX_MESH_CANDIDATES {
        return Err(QueryError::CandidateLimit);
    }
    let mut closest: Option<(f32, DistanceResult, Option<u32>)> = None;
    for triangle_index in candidates {
        let indices = mesh.triangles()[triangle_index as usize];
        let vertices = indices
            .map(|index| mesh_pose.position + mesh_pose.rotation * mesh.vertices()[index as usize]);
        let triangle_normal = (vertices[1] - vertices[0])
            .cross(vertices[2] - vertices[0])
            .try_normalize()
            .ok_or(GeometryError::DegenerateTriangle)?;
        let Some((fraction, distance)) = conservative_advance(displacement, skin, |offset| {
            let cast_pose = Pose::new(pose.position + offset, pose.rotation)?;
            let mut distance = convex_triangle_distance(shape, cast_pose, vertices)?;
            if distance.distance <= skin {
                let center = (vertices[0] + vertices[1] + vertices[2]) / 3.0;
                let outward = if (cast_pose.position - center).dot(triangle_normal) >= 0.0 {
                    triangle_normal
                } else {
                    -triangle_normal
                };
                distance.normal = -outward;
            }
            Ok(distance)
        })?
        else {
            continue;
        };
        if closest.is_none_or(
            |(old_fraction, _, old_triangle): (f32, DistanceResult, Option<u32>)| {
                fraction < old_fraction
                    || (fraction == old_fraction
                        && triangle_index < old_triangle.unwrap_or(u32::MAX))
            },
        ) {
            closest = Some((fraction, distance, Some(triangle_index)));
        }
    }
    Ok(closest)
}

fn conservative_advance(
    displacement: Vec3,
    skin: f32,
    mut distance_at: impl FnMut(Vec3) -> Result<DistanceResult, QueryError>,
) -> Result<Option<(f32, DistanceResult)>, QueryError> {
    let mut fraction = 0.0_f64;
    for _ in 0..MAX_SHAPE_CAST_ITERATIONS {
        let offset = displacement * fraction as f32;
        let distance = distance_at(offset)?;
        if !distance.distance.is_finite() || !distance.normal.is_finite() {
            return Err(QueryError::InvalidCast);
        }
        if distance.distance <= skin {
            return Ok(
                (displacement.dot(distance.normal) > 0.0).then_some((fraction as f32, distance))
            );
        }
        let closing_speed = displacement.dot(distance.normal) as f64;
        if closing_speed <= 0.0 {
            return Ok(None);
        }
        let advance = (distance.distance as f64 - skin as f64) / closing_speed;
        if !advance.is_finite() || advance <= 0.0 {
            return Err(QueryError::InvalidCast);
        }
        fraction += advance;
        if fraction > 1.0 {
            return Ok(None);
        }
    }
    Err(QueryError::ShapeCastIterationLimit)
}

fn expand_aabb(bounds: Aabb, amount: f32) -> Result<Aabb, GeometryError> {
    let expansion = Vec3::splat(amount);
    Aabb::new(bounds.min - expansion, bounds.max + expansion)
}

fn query_hit_precedes(candidate: QueryHit, current: QueryHit) -> bool {
    candidate
        .fraction
        .total_cmp(&current.fraction)
        .then_with(|| candidate.body.cmp(&current.body))
        .is_lt()
}

#[cfg(test)]
mod tests {
    use super::QueryFilter;
    use crate::world::{BodyDesc, PhysicsWorld};
    use crate::{Pose, Ray, Shape, TriMesh};
    use glam::{Quat, Vec3};
    use std::sync::Arc;

    fn floor_mesh() -> Shape {
        Shape::TriMesh(Arc::new(
            TriMesh::new(
                vec![
                    Vec3::new(-10.0, 0.0, -10.0),
                    Vec3::new(10.0, 0.0, -10.0),
                    Vec3::new(10.0, 0.0, 10.0),
                    Vec3::new(-10.0, 0.0, 10.0),
                ],
                vec![[0, 1, 2], [0, 2, 3]],
            )
            .unwrap(),
        ))
    }

    #[test]
    fn snapshot_raycast_filters_disabled_ignored_and_trigger_colliders() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let mut trigger = BodyDesc::static_collider(
            Shape::sphere(0.5).unwrap(),
            Pose::new(Vec3::new(1.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
        );
        trigger.collider.trigger = true;
        let trigger_handle = world.insert_body(trigger).unwrap();
        let mut disabled_desc = BodyDesc::static_collider(
            Shape::sphere(0.25).unwrap(),
            Pose::new(Vec3::new(2.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
        );
        disabled_desc.collider.enabled = false;
        world.insert_body(disabled_desc).unwrap();
        let wall = world
            .insert_body(BodyDesc::static_collider(
                Shape::cuboid(Vec3::splat(0.25)).unwrap(),
                Pose::new(Vec3::new(4.0, 0.0, 0.0), Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let ray = Ray::new(Vec3::ZERO, Vec3::X, 6.0).unwrap();

        let default_hit = snapshot
            .raycast(&ray, QueryFilter::default())
            .unwrap()
            .unwrap();
        assert_eq!(default_hit.body, wall);
        let trigger_hit = snapshot
            .raycast(
                &ray,
                QueryFilter {
                    ignore: None,
                    include_triggers: true,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(trigger_hit.body, trigger_handle);
        let ignored_hit = snapshot
            .raycast(
                &ray,
                QueryFilter {
                    ignore: Some(wall),
                    include_triggers: false,
                },
            )
            .unwrap();
        assert!(ignored_hit.is_none());
    }

    #[test]
    fn reversed_enabled_overrides_filter_rays_and_shape_casts() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let first = world
            .insert_body(BodyDesc::static_collider(
                Shape::sphere(0.25).unwrap(),
                Pose::new(Vec3::X, Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let second = world
            .insert_body(BodyDesc::static_collider(
                Shape::sphere(0.25).unwrap(),
                Pose::new(Vec3::X * 2.0, Quat::IDENTITY).unwrap(),
            ))
            .unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let enabled = [(second, false), (first, false)];
        let ray = Ray::new(Vec3::ZERO, Vec3::X, 3.0).unwrap();

        assert!(
            snapshot
                .raycast_with_enabled_overrides(&ray, QueryFilter::default(), &[], &enabled)
                .unwrap()
                .is_none()
        );
        assert!(
            snapshot
                .shape_cast_with_overrides(
                    &Shape::sphere(0.1).unwrap(),
                    Pose::IDENTITY,
                    Vec3::X * 3.0,
                    0.0,
                    QueryFilter::default(),
                    super::QueryOverrides {
                        excluded: &[],
                        enabled: &enabled,
                    },
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn shape_cast_crosses_mesh_floor_geometry_and_respects_movement_filters() {
        let mut world = PhysicsWorld::new(Vec3::ZERO).unwrap();
        let floor = world
            .insert_body(BodyDesc::static_collider(floor_mesh(), Pose::IDENTITY))
            .unwrap();
        let mut trigger = BodyDesc::static_collider(
            Shape::cuboid(Vec3::new(0.2, 0.4, 1.0)).unwrap(),
            Pose::new(Vec3::new(1.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
        );
        trigger.collider.trigger = true;
        world.insert_body(trigger).unwrap();
        let snapshot = world.query_snapshot().unwrap();
        let capsule = Shape::capsule(0.25, 0.5).unwrap();
        let pose = Pose::new(Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY).unwrap();
        let floor_hit = snapshot
            .shape_cast(&capsule, pose, -Vec3::Y, 0.01, QueryFilter::default())
            .unwrap()
            .unwrap();
        assert_eq!(floor_hit.body, floor);
        assert_eq!(floor_hit.triangle_index, Some(0));
        assert!(floor_hit.normal.dot(-Vec3::Y) > 0.99, "{floor_hit:?}");

        let horizontal = snapshot
            .shape_cast(
                &capsule,
                Pose::new(Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).unwrap(),
                Vec3::X * 2.0,
                0.0,
                QueryFilter::default(),
            )
            .unwrap();
        assert!(horizontal.is_none());
    }
}
