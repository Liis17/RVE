use std::sync::Arc;

use glam::{Mat3, Quat, Vec3};

use crate::{GeometryError, Hull, TriMesh};

#[derive(Clone, Copy, Debug, PartialEq)]
/// Local-space axis-aligned bounds for a shape or geometry node.
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    pub fn new(min: Vec3, max: Vec3) -> Result<Self, GeometryError> {
        if !min.is_finite() || !max.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        if min.cmpgt(max).any() {
            return Err(GeometryError::InvalidAabb);
        }
        Ok(Self { min, max })
    }

    pub fn from_points(points: &[Vec3]) -> Result<Self, GeometryError> {
        let Some(first) = points.first().copied() else {
            return Err(GeometryError::InvalidAabb);
        };
        if !first.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        let mut bounds = Self {
            min: first,
            max: first,
        };
        for point in &points[1..] {
            if !point.is_finite() {
                return Err(GeometryError::NonFiniteInput);
            }
            bounds.min = bounds.min.min(*point);
            bounds.max = bounds.max.max(*point);
        }
        Ok(bounds)
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.min.cmple(other.max).all() && self.max.cmpge(other.min).all()
    }

    pub fn contains(self, point: Vec3) -> bool {
        self.min.cmple(point).all() && self.max.cmpge(point).all()
    }

    pub fn center(self) -> Vec3 {
        Vec3::new(
            ((self.min.x as f64 + self.max.x as f64) * 0.5) as f32,
            ((self.min.y as f64 + self.max.y as f64) * 0.5) as f32,
            ((self.min.z as f64 + self.max.z as f64) * 0.5) as f32,
        )
    }

    pub fn half_extents(self) -> Vec3 {
        Vec3::new(
            ((self.max.x as f64 - self.min.x as f64) * 0.5) as f32,
            ((self.max.y as f64 - self.min.y as f64) * 0.5) as f32,
            ((self.max.z as f64 - self.min.z as f64) * 0.5) as f32,
        )
    }

    pub fn surface_area(self) -> f32 {
        let size = self.max - self.min;
        2.0 * (size.x * size.y + size.y * size.z + size.z * size.x)
    }

    pub fn corners(self) -> [Vec3; 8] {
        let min = self.min;
        let max = self.max;
        [
            Vec3::new(min.x, min.y, min.z),
            Vec3::new(max.x, min.y, min.z),
            Vec3::new(min.x, max.y, min.z),
            Vec3::new(max.x, max.y, min.z),
            Vec3::new(min.x, min.y, max.z),
            Vec3::new(max.x, min.y, max.z),
            Vec3::new(min.x, max.y, max.z),
            Vec3::new(max.x, max.y, max.z),
        ]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
/// Translation and rotation used by geometry queries; geometry has no scale.
pub struct Pose {
    pub position: Vec3,
    pub rotation: Quat,
}

impl Pose {
    pub const IDENTITY: Self = Self {
        position: Vec3::ZERO,
        rotation: Quat::IDENTITY,
    };

    pub fn new(position: Vec3, rotation: Quat) -> Result<Self, GeometryError> {
        if !position.is_finite() || !rotation.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        let length_squared = rotation.length_squared();
        if !length_squared.is_finite() || length_squared <= f32::MIN_POSITIVE {
            return Err(GeometryError::InvalidDimensions);
        }
        Ok(Self {
            position,
            rotation: rotation.normalize(),
        })
    }

    fn normalized(self) -> Result<Self, GeometryError> {
        Self::new(self.position, self.rotation)
    }
}

#[derive(Clone, Debug, PartialEq)]
/// Collision geometry stored in local coordinates.
pub enum Shape {
    Sphere {
        radius: f32,
    },
    Box {
        half_extents: Vec3,
    },
    /// `height` is the length of the cylindrical section between the hemispheres.
    Capsule {
        radius: f32,
        height: f32,
    },
    ConvexHull(Arc<Hull>),
    TriMesh(Arc<TriMesh>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
/// Uniform-density mass properties in the shape's local coordinate frame.
pub struct MassProperties {
    pub mass: f32,
    pub volume: f32,
    pub center_of_mass: Vec3,
    pub inertia: Mat3,
}

impl Shape {
    pub fn sphere(radius: f32) -> Result<Self, GeometryError> {
        if !radius.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        if radius <= 0.0 {
            return Err(GeometryError::InvalidDimensions);
        }
        Ok(Self::Sphere { radius })
    }

    pub fn cuboid(half_extents: Vec3) -> Result<Self, GeometryError> {
        if !half_extents.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        if half_extents.cmple(Vec3::ZERO).any() {
            return Err(GeometryError::InvalidDimensions);
        }
        Ok(Self::Box { half_extents })
    }

    pub fn capsule(radius: f32, height: f32) -> Result<Self, GeometryError> {
        if !radius.is_finite() || !height.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        if radius <= 0.0 || height < 0.0 {
            return Err(GeometryError::InvalidDimensions);
        }
        Ok(Self::Capsule { radius, height })
    }

    /// Returns a local-space support point; triangle meshes are not convex and return `None`.
    pub fn support(&self, direction: Vec3) -> Result<Option<Vec3>, GeometryError> {
        if !direction.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        let unit = normalize_direction(direction);
        let support = match self {
            Self::Sphere { radius } if radius.is_finite() && *radius > 0.0 => unit * *radius,
            Self::Box { half_extents }
                if half_extents.is_finite() && half_extents.cmpgt(Vec3::ZERO).all() =>
            {
                Vec3::new(
                    if direction.x < 0.0 {
                        -half_extents.x
                    } else {
                        half_extents.x
                    },
                    if direction.y < 0.0 {
                        -half_extents.y
                    } else {
                        half_extents.y
                    },
                    if direction.z < 0.0 {
                        -half_extents.z
                    } else {
                        half_extents.z
                    },
                )
            }
            Self::Capsule { radius, height }
                if radius.is_finite() && *radius > 0.0 && height.is_finite() && *height >= 0.0 =>
            {
                unit * *radius
                    + Vec3::Y
                        * if direction.y < 0.0 {
                            -*height * 0.5
                        } else {
                            *height * 0.5
                        }
            }
            Self::ConvexHull(hull) => return Ok(Some(hull.support(direction)?)),
            Self::TriMesh(_) => return Ok(None),
            _ => return Err(GeometryError::InvalidDimensions),
        };
        if !support.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        Ok(Some(support))
    }

    /// Returns bounds before applying a pose.
    pub fn local_aabb(&self) -> Result<Aabb, GeometryError> {
        match self {
            Self::Sphere { radius } if radius.is_finite() && *radius > 0.0 => {
                let extents = Vec3::splat(*radius);
                Aabb::new(-extents, extents)
            }
            Self::Box { half_extents }
                if half_extents.is_finite() && half_extents.cmpgt(Vec3::ZERO).all() =>
            {
                Aabb::new(-*half_extents, *half_extents)
            }
            Self::Capsule { radius, height }
                if radius.is_finite() && *radius > 0.0 && height.is_finite() && *height >= 0.0 =>
            {
                let extents = Vec3::new(*radius, *radius + *height * 0.5, *radius);
                Aabb::new(-extents, extents)
            }
            Self::ConvexHull(hull) => Ok(hull.bounds()),
            Self::TriMesh(mesh) => Ok(mesh.bounds()),
            _ => Err(GeometryError::InvalidDimensions),
        }
    }

    /// Returns a world-space AABB after rigid translation and rotation.
    pub fn aabb(&self, pose: Pose) -> Result<Aabb, GeometryError> {
        let pose = pose.normalized()?;
        let local = self.local_aabb()?;
        let corners = local
            .corners()
            .map(|point| pose.position + pose.rotation * point);
        Aabb::from_points(&corners)
    }

    /// Computes local uniform-density mass properties for a positive mass.
    pub fn mass_properties(&self, mass: f32) -> Result<MassProperties, GeometryError> {
        if !mass.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        if mass <= 0.0 {
            return Err(GeometryError::InvalidMass);
        }
        match self {
            Self::Sphere { radius } if radius.is_finite() && *radius > 0.0 => {
                let volume = 4.0 / 3.0 * std::f32::consts::PI * radius.powi(3);
                let moment = 0.4 * mass * radius * radius;
                checked_mass_properties(MassProperties {
                    mass,
                    volume,
                    center_of_mass: Vec3::ZERO,
                    inertia: Mat3::from_diagonal(Vec3::splat(moment)),
                })
            }
            Self::Box { half_extents }
                if half_extents.is_finite() && half_extents.cmpgt(Vec3::ZERO).all() =>
            {
                let volume = 8.0 * half_extents.x * half_extents.y * half_extents.z;
                let factors = mass / 3.0;
                let inertia = Vec3::new(
                    factors * (half_extents.y.powi(2) + half_extents.z.powi(2)),
                    factors * (half_extents.x.powi(2) + half_extents.z.powi(2)),
                    factors * (half_extents.x.powi(2) + half_extents.y.powi(2)),
                );
                checked_mass_properties(MassProperties {
                    mass,
                    volume,
                    center_of_mass: Vec3::ZERO,
                    inertia: Mat3::from_diagonal(inertia),
                })
            }
            Self::Capsule { radius, height }
                if radius.is_finite() && *radius > 0.0 && height.is_finite() && *height >= 0.0 =>
            {
                let sphere_volume = 4.0 / 3.0 * std::f32::consts::PI * radius.powi(3);
                let cylinder_volume = std::f32::consts::PI * radius.powi(2) * height;
                let volume = sphere_volume + cylinder_volume;
                let sphere_mass = mass * sphere_volume / volume;
                let cylinder_mass = mass - sphere_mass;
                let axial =
                    0.4 * sphere_mass * radius.powi(2) + 0.5 * cylinder_mass * radius.powi(2);
                let transverse = sphere_mass
                    * (0.4 * radius.powi(2) + height.powi(2) * 0.25 + 3.0 * radius * height / 8.0)
                    + cylinder_mass * (3.0 * radius.powi(2) + height.powi(2)) / 12.0;
                checked_mass_properties(MassProperties {
                    mass,
                    volume,
                    center_of_mass: Vec3::ZERO,
                    inertia: Mat3::from_diagonal(Vec3::new(transverse, axial, transverse)),
                })
            }
            Self::ConvexHull(hull) => hull.mass_properties(mass),
            Self::TriMesh(_) => Err(GeometryError::TriangleMeshMassUnsupported),
            _ => Err(GeometryError::InvalidDimensions),
        }
    }
}

fn checked_mass_properties(properties: MassProperties) -> Result<MassProperties, GeometryError> {
    if !properties.volume.is_finite()
        || properties.volume <= 0.0
        || !properties.center_of_mass.is_finite()
        || !properties.inertia.is_finite()
    {
        return Err(GeometryError::NonFiniteInput);
    }
    Ok(properties)
}

fn normalize_direction(direction: Vec3) -> Vec3 {
    let x = direction.x as f64;
    let y = direction.y as f64;
    let z = direction.z as f64;
    let length = (x * x + y * y + z * z).sqrt();
    if length <= f64::MIN_POSITIVE {
        return Vec3::X;
    }
    Vec3::new(
        (x / length) as f32,
        (y / length) as f32,
        (z / length) as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::{Aabb, Pose, Shape};
    use crate::{GeometryError, Hull, TriMesh};
    use glam::{Quat, Vec3};
    use std::sync::Arc;

    fn close(left: f32, right: f32) {
        assert!((left - right).abs() < 1e-5, "{left} != {right}");
    }

    #[test]
    fn support_points_and_rotated_aabb_match_shape_geometry() {
        let cuboid = Shape::cuboid(Vec3::new(1.0, 2.0, 0.5)).unwrap();
        assert_eq!(
            cuboid.support(Vec3::new(-1.0, 1.0, 0.0)).unwrap(),
            Some(Vec3::new(-1.0, 2.0, 0.5))
        );
        let pose = Pose::new(
            Vec3::new(3.0, 0.0, 0.0),
            Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
        )
        .unwrap();
        let bounds = cuboid.aabb(pose).unwrap();
        assert!(bounds.min.abs_diff_eq(Vec3::new(2.5, -2.0, -1.0), 1e-5));
        assert!(bounds.max.abs_diff_eq(Vec3::new(3.5, 2.0, 1.0), 1e-5));

        let sphere = Shape::sphere(2.0).unwrap();
        assert_eq!(sphere.support(-Vec3::Y).unwrap(), Some(-Vec3::Y * 2.0));
        assert_eq!(
            sphere.support(Vec3::new(0.0, -1.0e-30, 0.0)).unwrap(),
            Some(-Vec3::Y * 2.0)
        );
        let capsule = Shape::capsule(0.5, 2.0).unwrap();
        assert_eq!(capsule.support(Vec3::Y).unwrap(), Some(Vec3::Y * 1.5));
    }

    #[test]
    fn primitive_mass_properties_use_solid_volumes_and_inertia() {
        let sphere = Shape::sphere(2.0).unwrap().mass_properties(3.0).unwrap();
        close(sphere.volume, 4.0 / 3.0 * std::f32::consts::PI * 8.0);
        close(sphere.inertia.x_axis.x, 4.8);
        let cuboid = Shape::cuboid(Vec3::new(1.0, 2.0, 3.0))
            .unwrap()
            .mass_properties(6.0)
            .unwrap();
        close(cuboid.volume, 48.0);
        assert!(
            cuboid
                .inertia
                .diagonal()
                .abs_diff_eq(Vec3::new(26.0, 20.0, 10.0), 1e-5)
        );
        let capsule = Shape::capsule(1.0, 2.0)
            .unwrap()
            .mass_properties(1.0)
            .unwrap();
        assert!(capsule.inertia.diagonal().is_finite());
        assert!(capsule.inertia.y_axis.y < capsule.inertia.x_axis.x);
    }

    #[test]
    fn hull_mass_properties_use_triangle_mesh_geometry_and_meshes_are_static_only() {
        let hull = Arc::new(Hull::from_points(&[Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z]).unwrap());
        let properties = Shape::ConvexHull(hull).mass_properties(2.0).unwrap();
        close(properties.volume, 1.0 / 6.0);
        assert!(
            properties
                .center_of_mass
                .abs_diff_eq(Vec3::splat(0.25), 1e-5)
        );
        assert!(
            properties
                .inertia
                .diagonal()
                .abs_diff_eq(Vec3::splat(0.15), 1e-5)
        );
        close(properties.inertia.x_axis.y, 0.025);

        let mesh =
            Arc::new(TriMesh::new(vec![Vec3::ZERO, Vec3::X, Vec3::Y], vec![[0, 1, 2]]).unwrap());
        assert_eq!(
            Shape::TriMesh(mesh).mass_properties(1.0),
            Err(GeometryError::TriangleMeshMassUnsupported)
        );
    }

    #[test]
    fn aabb_queries_respect_closed_bounds() {
        let a = Aabb::new(Vec3::ZERO, Vec3::ONE).unwrap();
        let touching = Aabb::new(Vec3::X, Vec3::splat(2.0)).unwrap();
        assert!(a.overlaps(touching));
        assert!(a.contains(Vec3::ONE));
    }

    #[test]
    fn zero_cylinder_height_capsule_matches_sphere_bounds_and_inertia() {
        let capsule = Shape::capsule(1.0, 0.0).unwrap();
        let sphere = Shape::sphere(1.0).unwrap();
        assert_eq!(capsule.local_aabb().unwrap(), sphere.local_aabb().unwrap());
        assert_eq!(
            capsule.mass_properties(1.0).unwrap().inertia,
            sphere.mass_properties(1.0).unwrap().inertia
        );
    }
}
