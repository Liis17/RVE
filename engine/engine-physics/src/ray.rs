use glam::Vec3;

use crate::shape::{Aabb, Pose, Shape};
use crate::{GeometryError, Hull, Plane, TriMesh};

#[derive(Clone, Copy, Debug, PartialEq)]
/// A normalized world-space ray with a finite maximum distance.
pub struct Ray {
    pub origin: Vec3,
    pub direction: Vec3,
    pub max_distance: f32,
}

impl Ray {
    pub fn new(origin: Vec3, direction: Vec3, max_distance: f32) -> Result<Self, GeometryError> {
        if !origin.is_finite() || !direction.is_finite() || !max_distance.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        if max_distance < 0.0 {
            return Err(GeometryError::InvalidRay);
        }
        let Some(direction) = normalize64(direction) else {
            return Err(GeometryError::InvalidRay);
        };
        let ray = Self {
            origin,
            direction,
            max_distance,
        };
        if !ray.is_valid() {
            return Err(GeometryError::InvalidRay);
        }
        Ok(ray)
    }

    pub fn point_at(self, distance: f32) -> Vec3 {
        self.origin + self.direction * distance
    }

    pub(crate) fn is_valid(self) -> bool {
        self.origin.is_finite()
            && self.direction.is_finite()
            && self.max_distance.is_finite()
            && self.max_distance >= 0.0
            && (self.direction.length_squared() - 1.0).abs() <= 1.0e-4
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
/// Closest ray intersection; mesh hits include the source triangle index.
pub struct RayHit {
    pub point: Vec3,
    pub normal: Vec3,
    pub distance: f32,
    pub triangle_index: Option<u32>,
}

/// Returns the nearest intersection in `[0, ray.max_distance]`.
pub fn raycast(ray: &Ray, shape: &Shape, pose: Pose) -> Result<Option<RayHit>, GeometryError> {
    if !ray.is_valid() {
        return Err(GeometryError::InvalidRay);
    }
    let pose = Pose::new(pose.position, pose.rotation)?;
    shape.local_aabb()?;
    let inverse = pose.rotation.inverse();
    let local_origin = inverse * (ray.origin - pose.position);
    let local_direction = (inverse * ray.direction)
        .try_normalize()
        .ok_or(GeometryError::InvalidRay)?;
    if !local_origin.is_finite() {
        return Err(GeometryError::NonFiniteInput);
    }
    let local_ray = Ray {
        origin: local_origin,
        direction: local_direction,
        max_distance: ray.max_distance,
    };
    let local_hit = match shape {
        Shape::Sphere { radius } => ray_sphere(&local_ray, *radius),
        Shape::Box { half_extents } => ray_box(&local_ray, *half_extents),
        Shape::Capsule { radius, height } => ray_capsule(&local_ray, *radius, *height),
        Shape::ConvexHull(hull) => ray_hull(&local_ray, hull),
        Shape::TriMesh(mesh) => ray_mesh(&local_ray, mesh),
    };
    Ok(local_hit.map(|mut hit| {
        hit.point = ray.point_at(hit.distance);
        hit.normal = (pose.rotation * hit.normal).normalize_or_zero();
        hit
    }))
}

pub(crate) fn ray_aabb(ray: &Ray, bounds: Aabb, max_distance: f32) -> Option<(f32, f32)> {
    let epsilon = geometry_epsilon(bounds.half_extents(), ray.origin);
    let mut enter = 0.0_f64;
    let mut exit = max_distance as f64;
    for axis in 0..3 {
        let origin = ray.origin[axis] as f64;
        let direction = ray.direction[axis] as f64;
        let min = bounds.min[axis] as f64;
        let max = bounds.max[axis] as f64;
        if direction == 0.0 {
            if origin < min - epsilon || origin > max + epsilon {
                return None;
            }
            continue;
        }
        let mut near = (min - origin) / direction;
        let mut far = (max - origin) / direction;
        if near > far {
            std::mem::swap(&mut near, &mut far);
        }
        enter = enter.max(near);
        exit = exit.min(far);
        if enter > exit + epsilon {
            return None;
        }
    }
    (exit >= 0.0 && enter <= max_distance as f64).then_some((enter as f32, exit as f32))
}

pub(crate) fn ray_triangle(
    ray: &Ray,
    vertices: [Vec3; 3],
    triangle_index: Option<u32>,
) -> Option<RayHit> {
    let [a, b, c] = vertices;
    let edge1 = subtract64(b, a);
    let edge2 = subtract64(c, a);
    let direction = vec64(ray.direction);
    let origin = vec64(ray.origin);
    let p = cross64(direction, edge2);
    let determinant = dot64(edge1, p);
    let determinant_epsilon = length64(edge1) * length64(edge2) * 1.0e-8;
    if determinant.abs() <= determinant_epsilon {
        return None;
    }
    let inverse_determinant = 1.0 / determinant;
    let translated = subtract64(ray.origin, a);
    let u = dot64(translated, p) * inverse_determinant;
    if !(-1.0e-8..=1.0 + 1.0e-8).contains(&u) {
        return None;
    }
    let q = cross64(translated, edge1);
    let v = dot64(direction, q) * inverse_determinant;
    if v < -1.0e-8 || u + v > 1.0 + 1.0e-8 {
        return None;
    }
    let distance = dot64(edge2, q) * inverse_determinant;
    let scale = Vec3::splat(length64(edge1).max(length64(edge2)) as f32);
    if distance < -geometry_epsilon(scale, ray.origin) || distance > ray.max_distance as f64 {
        return None;
    }
    let normal = cross64(edge1, edge2);
    let normal_length = length64(normal);
    if normal_length <= f64::MIN_POSITIVE || !normal_length.is_finite() {
        return None;
    }
    let normal = Vec3::new(
        (normal[0] / normal_length) as f32,
        (normal[1] / normal_length) as f32,
        (normal[2] / normal_length) as f32,
    );
    let distance = distance.max(0.0) as f32;
    Some(RayHit {
        point: Vec3::new(
            (origin[0] + direction[0] * distance as f64) as f32,
            (origin[1] + direction[1] * distance as f64) as f32,
            (origin[2] + direction[2] * distance as f64) as f32,
        ),
        normal,
        distance,
        triangle_index,
    })
}

fn ray_sphere(ray: &Ray, radius: f32) -> Option<RayHit> {
    if !radius.is_finite() || radius <= 0.0 {
        return None;
    }
    let origin = vec64(ray.origin);
    let direction = vec64(ray.direction);
    let radius = radius as f64;
    let b = dot64(origin, direction);
    let c = dot64(origin, origin) - radius * radius;
    let epsilon = geometry_epsilon(Vec3::splat(radius as f32), ray.origin);
    if (length64(origin) - radius).abs() <= epsilon {
        return make_hit(ray, 0.0, normalize64(ray.origin).unwrap_or(Vec3::X), None);
    }
    let (near, far) = quadratic_roots(dot64(direction, direction), 2.0 * b, c)?;
    let distance = if c < 0.0 {
        far
    } else if near >= 0.0 {
        near
    } else {
        far
    };
    if distance < 0.0 || distance > ray.max_distance as f64 {
        return None;
    }
    let distance = distance as f32;
    let point = ray.point_at(distance);
    make_hit(ray, distance, normalize64(point)?, None)
}

fn ray_box(ray: &Ray, half_extents: Vec3) -> Option<RayHit> {
    if !half_extents.is_finite() || half_extents.cmple(Vec3::ZERO).any() {
        return None;
    }
    let epsilon = geometry_epsilon(half_extents, ray.origin);
    let mut surface_axis = None;
    for axis in 0..3 {
        if ((ray.origin[axis].abs() as f64) - half_extents[axis] as f64).abs() <= epsilon
            && (0..3).all(|other| {
                other == axis
                    || ray.origin[other].abs() as f64 <= half_extents[other] as f64 + epsilon
            })
            && surface_axis
                .is_none_or(|best_axis| ray.direction[axis].abs() > ray.direction[best_axis].abs())
        {
            surface_axis = Some(axis);
        }
    }
    if let Some(axis) = surface_axis {
        let sign = ray.origin[axis].signum();
        let normal = match axis {
            0 => Vec3::X * sign,
            1 => Vec3::Y * sign,
            _ => Vec3::Z * sign,
        };
        return make_hit(ray, 0.0, normal, None);
    }
    let mut enter = f64::NEG_INFINITY;
    let mut exit = f64::INFINITY;
    let mut enter_normal = Vec3::ZERO;
    let mut exit_normal = Vec3::ZERO;
    for axis in 0..3 {
        let origin = ray.origin[axis] as f64;
        let direction = ray.direction[axis] as f64;
        let half = half_extents[axis] as f64;
        if direction == 0.0 {
            if origin < -half - epsilon || origin > half + epsilon {
                return None;
            }
            continue;
        }
        let near_t = (-half - origin) / direction;
        let far_t = (half - origin) / direction;
        let axis_normal = match axis {
            0 => Vec3::X,
            1 => Vec3::Y,
            _ => Vec3::Z,
        };
        let (near_normal, far_normal) = if direction > 0.0 {
            (-axis_normal, axis_normal)
        } else {
            (axis_normal, -axis_normal)
        };
        let (near_t, far_t) = if near_t <= far_t {
            (near_t, far_t)
        } else {
            (far_t, near_t)
        };
        if near_t > enter {
            enter = near_t;
            enter_normal = near_normal;
        }
        if far_t < exit {
            exit = far_t;
            exit_normal = far_normal;
        }
        if enter > exit + epsilon {
            return None;
        }
    }
    let (distance, normal) = if enter >= 0.0 {
        (enter, enter_normal)
    } else {
        (exit, exit_normal)
    };
    if distance < -epsilon || distance > ray.max_distance as f64 || normal == Vec3::ZERO {
        return None;
    }
    make_hit(ray, distance.max(0.0) as f32, normal, None)
}

fn ray_capsule(ray: &Ray, radius: f32, height: f32) -> Option<RayHit> {
    if !radius.is_finite() || !height.is_finite() || radius <= 0.0 || height < 0.0 {
        return None;
    }
    if height == 0.0 {
        return ray_sphere(ray, radius);
    }
    let half_height = height * 0.5;
    let epsilon = geometry_epsilon(Vec3::new(radius, radius + half_height, radius), ray.origin);
    let closest = Vec3::new(0.0, ray.origin.y.clamp(-half_height, half_height), 0.0);
    let surface_delta = ray.origin - closest;
    if (length64(vec64(surface_delta)) - radius as f64).abs() <= epsilon {
        return make_hit(
            ray,
            0.0,
            normalize64(surface_delta).unwrap_or(Vec3::X),
            None,
        );
    }

    let mut closest_hit: Option<RayHit> = None;
    let dx = ray.direction.x as f64;
    let dz = ray.direction.z as f64;
    let ox = ray.origin.x as f64;
    let oz = ray.origin.z as f64;
    let a = dx * dx + dz * dz;
    if a > f64::MIN_POSITIVE {
        let b = ox * dx + oz * dz;
        let c = ox * ox + oz * oz - (radius as f64).powi(2);
        if let Some((near, far)) = quadratic_roots(a, 2.0 * b, c) {
            for distance in [near, far] {
                if distance < 0.0 || distance > ray.max_distance as f64 {
                    continue;
                }
                let point = ray.point_at(distance as f32);
                if point.y as f64 >= -half_height as f64 - epsilon
                    && point.y as f64 <= half_height as f64 + epsilon
                {
                    let normal = normalize64(Vec3::new(point.x, 0.0, point.z))?;
                    choose_closest(
                        &mut closest_hit,
                        make_hit(ray, distance as f32, normal, None)?,
                    );
                }
            }
        }
    }

    for center_y in [-half_height, half_height] {
        let center = Vec3::new(0.0, center_y, 0.0);
        let offset = ray.origin - center;
        let b = offset.dot(ray.direction) as f64;
        let c = offset.length_squared() as f64 - (radius as f64).powi(2);
        if let Some((near, far)) = quadratic_roots(1.0, 2.0 * b, c) {
            for distance in [near, far] {
                if distance < 0.0 || distance > ray.max_distance as f64 {
                    continue;
                }
                let point = ray.point_at(distance as f32);
                let is_top = center_y > 0.0 || height == 0.0;
                if (is_top && point.y as f64 >= half_height as f64 - epsilon)
                    || (!is_top && point.y as f64 <= -half_height as f64 + epsilon)
                {
                    let normal = normalize64(point - center)?;
                    choose_closest(
                        &mut closest_hit,
                        make_hit(ray, distance as f32, normal, None)?,
                    );
                }
            }
        }
    }
    closest_hit
}

fn ray_hull(ray: &Ray, hull: &Hull) -> Option<RayHit> {
    let epsilon = geometry_epsilon(hull.bounds().half_extents(), ray.origin);
    let mut surface_normal = None;
    let mut inside = true;
    for &Plane { normal, point } in hull.planes() {
        let signed = dot64(vec64(normal), subtract64(ray.origin, point));
        if signed > epsilon {
            inside = false;
            break;
        }
        if signed.abs() <= epsilon
            && surface_normal.is_none_or(|best: Vec3| {
                dot64(vec64(normal), vec64(ray.direction)).abs()
                    > dot64(vec64(best), vec64(ray.direction)).abs()
            })
        {
            surface_normal = Some(normal);
        }
    }
    if inside && let Some(normal) = surface_normal {
        return make_hit(ray, 0.0, normal, None);
    }
    let mut enter = f64::NEG_INFINITY;
    let mut exit = f64::INFINITY;
    let mut enter_normal = Vec3::ZERO;
    let mut exit_normal = Vec3::ZERO;
    for &Plane { normal, point } in hull.planes() {
        let signed = dot64(vec64(normal), subtract64(ray.origin, point));
        let denominator = dot64(vec64(normal), vec64(ray.direction));
        if denominator == 0.0 {
            if signed > epsilon {
                return None;
            }
            continue;
        }
        let distance = -signed / denominator;
        if denominator < 0.0 {
            if distance > enter {
                enter = distance;
                enter_normal = normal;
            }
        } else if distance < exit {
            exit = distance;
            exit_normal = normal;
        }
        if enter > exit + epsilon {
            return None;
        }
    }
    let (distance, normal) = if enter >= 0.0 {
        (enter, enter_normal)
    } else {
        (exit, exit_normal)
    };
    if !distance.is_finite()
        || distance < 0.0
        || distance > ray.max_distance as f64
        || normal == Vec3::ZERO
    {
        return None;
    }
    make_hit(ray, distance as f32, normal, None)
}

fn ray_mesh(ray: &Ray, mesh: &TriMesh) -> Option<RayHit> {
    mesh.raycast(ray)
}

fn make_hit(ray: &Ray, distance: f32, normal: Vec3, triangle_index: Option<u32>) -> Option<RayHit> {
    if !distance.is_finite() || distance < 0.0 || distance > ray.max_distance || !normal.is_finite()
    {
        return None;
    }
    Some(RayHit {
        point: ray.point_at(distance),
        normal: normal.try_normalize()?,
        distance,
        triangle_index,
    })
}

fn choose_closest(current: &mut Option<RayHit>, candidate: RayHit) {
    if current.is_none_or(|best| candidate.distance < best.distance) {
        *current = Some(candidate);
    }
}

fn quadratic_roots(a: f64, b: f64, c: f64) -> Option<(f64, f64)> {
    if a <= f64::MIN_POSITIVE {
        return None;
    }
    let discriminant = b * b - 4.0 * a * c;
    if discriminant < 0.0 || !discriminant.is_finite() {
        return None;
    }
    let root = discriminant.sqrt();
    let first = (-b - root) / (2.0 * a);
    let second = (-b + root) / (2.0 * a);
    Some((first.min(second), first.max(second)))
}

fn subtract64(left: Vec3, right: Vec3) -> [f64; 3] {
    [
        left.x as f64 - right.x as f64,
        left.y as f64 - right.y as f64,
        left.z as f64 - right.z as f64,
    ]
}

fn vec64(vector: Vec3) -> [f64; 3] {
    [vector.x as f64, vector.y as f64, vector.z as f64]
}

fn dot64(left: [f64; 3], right: [f64; 3]) -> f64 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn cross64(left: [f64; 3], right: [f64; 3]) -> [f64; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

fn length64(vector: [f64; 3]) -> f64 {
    dot64(vector, vector).sqrt()
}

fn geometry_epsilon(scale: Vec3, origin: Vec3) -> f64 {
    let scale = scale.abs().max_element() as f64;
    let coordinate_precision = origin.abs().max_element() as f64 * f32::EPSILON as f64 * 4.0;
    (scale * 1.0e-6)
        .max(coordinate_precision)
        .max(f64::MIN_POSITIVE)
}

fn normalize64(vector: Vec3) -> Option<Vec3> {
    let vector = vec64(vector);
    let length = length64(vector);
    if !length.is_finite() || length <= f64::MIN_POSITIVE {
        return None;
    }
    Some(Vec3::new(
        (vector[0] / length) as f32,
        (vector[1] / length) as f32,
        (vector[2] / length) as f32,
    ))
}

#[cfg(test)]
mod tests {
    use crate::{GeometryError, Hull, Pose, Ray, Shape, TriMesh, raycast};
    use glam::{Quat, Vec3};
    use std::sync::Arc;

    fn close(left: f32, right: f32) {
        assert!((left - right).abs() < 1e-5, "{left} != {right}");
    }

    #[test]
    fn sphere_rays_handle_hits_misses_inside_and_surface_origins() {
        let sphere = Shape::sphere(1.0).unwrap();
        let ray = Ray::new(Vec3::new(0.0, 0.0, 3.0), -Vec3::Z, 5.0).unwrap();
        let hit = raycast(&ray, &sphere, Pose::IDENTITY).unwrap().unwrap();
        close(hit.distance, 2.0);
        assert!(hit.normal.abs_diff_eq(Vec3::Z, 1e-5));

        let inside = Ray::new(Vec3::ZERO, Vec3::X, 5.0).unwrap();
        close(
            raycast(&inside, &sphere, Pose::IDENTITY)
                .unwrap()
                .unwrap()
                .distance,
            1.0,
        );
        let surface = Ray::new(Vec3::X, Vec3::X, 5.0).unwrap();
        close(
            raycast(&surface, &sphere, Pose::IDENTITY)
                .unwrap()
                .unwrap()
                .distance,
            0.0,
        );
        let miss = Ray::new(Vec3::new(2.0, 2.0, 3.0), -Vec3::Z, 5.0).unwrap();
        assert!(raycast(&miss, &sphere, Pose::IDENTITY).unwrap().is_none());
        let short = Ray::new(Vec3::new(0.0, 0.0, 3.0), -Vec3::Z, 1.0).unwrap();
        assert!(raycast(&short, &sphere, Pose::IDENTITY).unwrap().is_none());
    }

    #[test]
    fn ray_tolerances_scale_with_small_shapes() {
        let sphere = Shape::sphere(1.0e-9).unwrap();
        let ray = Ray::new(Vec3::new(0.0, 0.0, 3.0e-9), -Vec3::Z, 5.0e-9).unwrap();
        close(
            raycast(&ray, &sphere, Pose::IDENTITY)
                .unwrap()
                .unwrap()
                .distance,
            2.0e-9,
        );

        let cube = Shape::cuboid(Vec3::splat(1.0e-9)).unwrap();
        let ray = Ray::new(Vec3::new(2.0e-9, 0.0, 0.0), -Vec3::X, 5.0e-9).unwrap();
        close(
            raycast(&ray, &cube, Pose::IDENTITY)
                .unwrap()
                .unwrap()
                .distance,
            1.0e-9,
        );
    }

    #[test]
    fn box_rays_handle_parallel_inside_surface_and_rotation_cases() {
        let cube = Shape::cuboid(Vec3::ONE).unwrap();
        let hit = Ray::new(Vec3::new(2.0, 0.0, 0.0), -Vec3::X, 5.0).unwrap();
        let hit = raycast(&hit, &cube, Pose::IDENTITY).unwrap().unwrap();
        close(hit.distance, 1.0);
        assert!(hit.normal.abs_diff_eq(Vec3::X, 1e-5));

        let inside = Ray::new(Vec3::ZERO, Vec3::Y, 5.0).unwrap();
        let inside_hit = raycast(&inside, &cube, Pose::IDENTITY).unwrap().unwrap();
        close(inside_hit.distance, 1.0);
        assert!(inside_hit.normal.abs_diff_eq(Vec3::Y, 1e-5));
        let surface = Ray::new(Vec3::X, Vec3::X, 5.0).unwrap();
        close(
            raycast(&surface, &cube, Pose::IDENTITY)
                .unwrap()
                .unwrap()
                .distance,
            0.0,
        );
        let surface_parallel = Ray::new(Vec3::X, Vec3::Y, 5.0).unwrap();
        let parallel_surface_hit = raycast(&surface_parallel, &cube, Pose::IDENTITY)
            .unwrap()
            .unwrap();
        close(parallel_surface_hit.distance, 0.0);
        assert!(parallel_surface_hit.normal.abs_diff_eq(Vec3::X, 1e-5));

        let parallel_miss = Ray::new(Vec3::new(-2.0, 2.0, 0.0), Vec3::X, 5.0).unwrap();
        assert!(
            raycast(&parallel_miss, &cube, Pose::IDENTITY)
                .unwrap()
                .is_none()
        );
        let pose = Pose::new(
            Vec3::ZERO,
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        )
        .unwrap();
        let rotated = Shape::cuboid(Vec3::new(1.0, 0.5, 0.25)).unwrap();
        let ray = Ray::new(Vec3::new(2.0, 0.0, 0.0), -Vec3::X, 5.0).unwrap();
        let rotated_hit = raycast(&ray, &rotated, pose).unwrap().unwrap();
        close(rotated_hit.distance, 1.75);
        assert!(rotated_hit.normal.abs_diff_eq(Vec3::X, 1e-5));
    }

    #[test]
    fn capsule_rays_hit_cylinder_and_caps_and_exit_from_inside() {
        let capsule = Shape::capsule(1.0, 2.0).unwrap();
        let side = Ray::new(Vec3::new(2.0, 0.0, 0.0), -Vec3::X, 5.0).unwrap();
        let side_hit = raycast(&side, &capsule, Pose::IDENTITY).unwrap().unwrap();
        close(side_hit.distance, 1.0);
        assert!(side_hit.normal.abs_diff_eq(Vec3::X, 1e-5));
        let top = Ray::new(Vec3::new(0.0, 4.0, 0.0), -Vec3::Y, 5.0).unwrap();
        close(
            raycast(&top, &capsule, Pose::IDENTITY)
                .unwrap()
                .unwrap()
                .distance,
            2.0,
        );
        let inside = Ray::new(Vec3::ZERO, Vec3::X, 5.0).unwrap();
        close(
            raycast(&inside, &capsule, Pose::IDENTITY)
                .unwrap()
                .unwrap()
                .distance,
            1.0,
        );
        let surface = Ray::new(Vec3::X, Vec3::X, 5.0).unwrap();
        close(
            raycast(&surface, &capsule, Pose::IDENTITY)
                .unwrap()
                .unwrap()
                .distance,
            0.0,
        );
        let miss = Ray::new(Vec3::new(2.0, 0.0, 2.0), -Vec3::X, 5.0).unwrap();
        assert!(raycast(&miss, &capsule, Pose::IDENTITY).unwrap().is_none());
    }

    #[test]
    fn hull_plane_rays_hit_rotated_hull_and_do_not_fall_back_to_aabb() {
        let hull = Arc::new(
            Hull::from_points(&[
                Vec3::ZERO,
                Vec3::new(2.0, 0.0, 0.0),
                Vec3::new(0.0, 2.0, 0.0),
                Vec3::new(0.0, 0.0, 2.0),
            ])
            .unwrap(),
        );
        let shape = Shape::ConvexHull(hull);
        let miss = Ray::new(Vec3::new(1.5, 1.5, 3.0), -Vec3::Z, 5.0).unwrap();
        assert!(raycast(&miss, &shape, Pose::IDENTITY).unwrap().is_none());

        let hit_ray = Ray::new(Vec3::new(0.2, 0.2, 3.0), -Vec3::Z, 5.0).unwrap();
        let hit = raycast(&hit_ray, &shape, Pose::IDENTITY).unwrap().unwrap();
        close(hit.distance, 1.4);
        assert!(hit.normal.z > 0.0);
        let surface = Ray::new(Vec3::new(0.2, 0.2, 1.6), Vec3::X, 5.0).unwrap();
        close(
            raycast(&surface, &shape, Pose::IDENTITY)
                .unwrap()
                .unwrap()
                .distance,
            0.0,
        );

        let pose = Pose::new(
            Vec3::new(4.0, 0.0, 0.0),
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        )
        .unwrap();
        let rotated_ray = Ray::new(Vec3::new(7.0, 0.2, -0.2), -Vec3::X, 5.0).unwrap();
        let rotated_hit = raycast(&rotated_ray, &shape, pose).unwrap().unwrap();
        close(rotated_hit.distance, 1.4);
        assert!(rotated_hit.normal.x > 0.0);
    }

    #[test]
    fn triangle_mesh_ray_transforms_with_pose_and_preserves_winding_normal() {
        let mesh =
            Arc::new(TriMesh::new(vec![Vec3::ZERO, Vec3::X, Vec3::Y], vec![[0, 1, 2]]).unwrap());
        let shape = Shape::TriMesh(mesh);
        let pose = Pose::new(
            Vec3::new(0.0, 0.0, 2.0),
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        )
        .unwrap();
        let ray = Ray::new(Vec3::new(-2.0, 0.2, 1.75), Vec3::X, 5.0).unwrap();
        let hit = raycast(&ray, &shape, pose).unwrap().unwrap();
        close(hit.distance, 2.0);
        assert!(hit.normal.x > 0.99);
        assert_eq!(hit.triangle_index, Some(0));
    }

    #[test]
    fn ray_constructor_rejects_invalid_inputs() {
        assert_eq!(
            Ray::new(Vec3::ZERO, Vec3::ZERO, 1.0),
            Err(GeometryError::InvalidRay)
        );
        assert_eq!(
            Ray::new(Vec3::ZERO, Vec3::X, -1.0),
            Err(GeometryError::InvalidRay)
        );
        assert_eq!(
            Ray::new(Vec3::splat(f32::NAN), Vec3::X, 1.0),
            Err(GeometryError::NonFiniteInput)
        );
    }
}
