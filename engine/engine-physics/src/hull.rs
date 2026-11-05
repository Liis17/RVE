use std::collections::{BTreeMap, HashSet};

use glam::{Mat3, Vec3};

use crate::GeometryError;
use crate::shape::Aabb;

pub const MAX_HULL_INPUT_POINTS: usize = 4_096;
pub const MAX_HULL_VERTICES: usize = 256;
pub const MAX_HULL_ITERATIONS: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Plane {
    pub normal: Vec3,
    pub point: Vec3,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Hull {
    vertices: Vec<Vec3>,
    triangles: Vec<[u32; 3]>,
    planes: Vec<Plane>,
    bounds: Aabb,
    interior_point: Vec3,
}

#[derive(Clone, Debug)]
struct Face {
    vertices: [usize; 3],
    plane: Plane,
    outside: Vec<usize>,
}

impl Hull {
    pub fn from_points(input: &[Vec3]) -> Result<Self, GeometryError> {
        if input.len() > MAX_HULL_INPUT_POINTS {
            return Err(GeometryError::TooManyPoints);
        }
        if input.iter().any(|point| !point.is_finite()) {
            return Err(GeometryError::NonFiniteInput);
        }

        let mut unique = Vec::with_capacity(input.len());
        let mut seen = HashSet::with_capacity(input.len());
        for point in input {
            let mut bits = point.to_array().map(f32::to_bits);
            for component in &mut bits {
                if *component == (-0.0_f32).to_bits() {
                    *component = 0.0_f32.to_bits();
                }
            }
            if seen.insert(bits) {
                unique.push(*point);
            }
        }
        if unique.len() < 4 {
            return Err(GeometryError::TooFewPoints);
        }

        let bounds = Aabb::from_points(&unique)?;
        let extent = bounds.max - bounds.min;
        if !extent.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        let diagonal = length64(extent);
        let coordinate_scale = unique
            .iter()
            .map(|point| point.abs().max_element() as f64)
            .fold(0.0_f64, f64::max);
        let epsilon = (diagonal * 1.0e-6)
            .max(coordinate_scale * f32::EPSILON as f64 * 4.0)
            .max(f64::MIN_POSITIVE);

        let p0 = (0..unique.len())
            .min_by(|&left, &right| unique[left].x.total_cmp(&unique[right].x))
            .expect("at least four unique points");
        let p1 = (0..unique.len())
            .filter(|&index| index != p0)
            .max_by(|&left, &right| {
                distance_squared64(unique[p0], unique[left])
                    .total_cmp(&distance_squared64(unique[p0], unique[right]))
            })
            .expect("at least four unique points");
        if distance_squared64(unique[p0], unique[p1]).sqrt() <= epsilon {
            return Err(GeometryError::DegenerateHull);
        }

        let line = subtract64(unique[p1], unique[p0]);
        let line_length_squared = dot64(line, line);
        let p2 = (0..unique.len())
            .filter(|&index| index != p0 && index != p1)
            .max_by(|&left, &right| {
                line_distance_squared64(unique[p0], line, line_length_squared, unique[left])
                    .total_cmp(&line_distance_squared64(
                        unique[p0],
                        line,
                        line_length_squared,
                        unique[right],
                    ))
            })
            .expect("at least four unique points");
        if line_distance_squared64(unique[p0], line, line_length_squared, unique[p2]).sqrt()
            <= epsilon
        {
            return Err(GeometryError::DegenerateHull);
        }

        let normal64 = cross64(
            subtract64(unique[p1], unique[p0]),
            subtract64(unique[p2], unique[p0]),
        );
        let normal_length = dot64(normal64, normal64).sqrt();
        let p3 = (0..unique.len())
            .filter(|&index| index != p0 && index != p1 && index != p2)
            .max_by(|&left, &right| {
                point_plane_distance64(unique[p0], normal64, normal_length, unique[left]).total_cmp(
                    &point_plane_distance64(unique[p0], normal64, normal_length, unique[right]),
                )
            })
            .expect("at least four unique points");
        if point_plane_distance64(unique[p0], normal64, normal_length, unique[p3]) <= epsilon {
            return Err(GeometryError::DegenerateHull);
        }

        let interior_point = average_four(unique[p0], unique[p1], unique[p2], unique[p3]);
        let tetrahedron = [p0, p1, p2, p3];
        let mut faces = Vec::with_capacity(16);
        for [a, b, c] in [[p0, p1, p2], [p0, p3, p1], [p0, p2, p3], [p1, p3, p2]] {
            faces.push(make_face([a, b, c], &unique, interior_point, epsilon)?);
        }

        for (index, &point) in unique.iter().enumerate() {
            if tetrahedron.contains(&index) {
                continue;
            }
            let mut best_face = None;
            let mut best_distance = epsilon;
            for (face_index, face) in faces.iter().enumerate() {
                let distance = plane_distance(face.plane, point);
                if distance > best_distance {
                    best_distance = distance;
                    best_face = Some(face_index);
                }
            }
            if let Some(face_index) = best_face {
                faces[face_index].outside.push(index);
            }
        }

        let mut iterations = 0;
        while let Some((face_index, eye, _)) = farthest_outside_point(&faces, &unique) {
            if iterations >= MAX_HULL_ITERATIONS {
                return Err(GeometryError::IterationLimit);
            }
            if faces
                .iter()
                .flat_map(|face| face.vertices)
                .collect::<HashSet<_>>()
                .len()
                >= MAX_HULL_VERTICES
            {
                return Err(GeometryError::HullVertexLimit);
            }
            iterations += 1;

            let visible: Vec<bool> = faces
                .iter()
                .map(|face| plane_distance(face.plane, unique[eye]) > epsilon)
                .collect();
            if !visible[face_index] {
                return Err(GeometryError::DegenerateHull);
            }

            let mut horizon: BTreeMap<(usize, usize), (usize, (usize, usize))> = BTreeMap::new();
            let mut orphaned = Vec::new();
            for (index, face) in faces.iter().enumerate() {
                if !visible[index] {
                    continue;
                }
                orphaned.extend(face.outside.iter().copied().filter(|&point| point != eye));
                for [a, b] in [
                    [face.vertices[0], face.vertices[1]],
                    [face.vertices[1], face.vertices[2]],
                    [face.vertices[2], face.vertices[0]],
                ] {
                    let key = if a < b { (a, b) } else { (b, a) };
                    let entry = horizon.entry(key).or_insert((0, (a, b)));
                    entry.0 += 1;
                }
            }
            let boundary_edges: Vec<(usize, usize)> = horizon
                .values()
                .filter_map(|&(count, edge)| (count == 1).then_some(edge))
                .collect();
            if boundary_edges.len() < 3 {
                return Err(GeometryError::DegenerateHull);
            }

            let mut next_faces = Vec::with_capacity(faces.len() + boundary_edges.len());
            for (index, face) in faces.into_iter().enumerate() {
                if !visible[index] {
                    next_faces.push(face);
                }
            }
            let new_face_start = next_faces.len();
            for (a, b) in boundary_edges {
                next_faces.push(make_face([a, b, eye], &unique, interior_point, epsilon)?);
            }

            for point in orphaned {
                let mut best_face = None;
                let mut best_distance = epsilon;
                for (face_index, face) in next_faces.iter().enumerate().skip(new_face_start) {
                    let distance = plane_distance(face.plane, unique[point]);
                    if distance > best_distance {
                        best_distance = distance;
                        best_face = Some(face_index);
                    }
                }
                if let Some(face_index) = best_face {
                    next_faces[face_index].outside.push(point);
                }
            }
            faces = next_faces;
        }

        let mut remap = vec![usize::MAX; unique.len()];
        let mut vertices = Vec::new();
        let mut triangles = Vec::with_capacity(faces.len());
        let mut planes = Vec::with_capacity(faces.len());
        for face in faces {
            let mut triangle = [0; 3];
            for (slot, source_index) in face.vertices.into_iter().enumerate() {
                if remap[source_index] == usize::MAX {
                    remap[source_index] = vertices.len();
                    vertices.push(unique[source_index]);
                }
                triangle[slot] = remap[source_index] as u32;
            }
            triangles.push(triangle);
            planes.push(face.plane);
        }
        if vertices.len() < 4 || vertices.len() > MAX_HULL_VERTICES {
            return Err(if vertices.len() > MAX_HULL_VERTICES {
                GeometryError::HullVertexLimit
            } else {
                GeometryError::DegenerateHull
            });
        }
        let bounds = Aabb::from_points(&vertices)?;
        Ok(Self {
            vertices,
            triangles,
            planes,
            bounds,
            interior_point,
        })
    }

    pub fn vertices(&self) -> &[Vec3] {
        &self.vertices
    }

    pub fn triangles(&self) -> &[[u32; 3]] {
        &self.triangles
    }

    pub fn planes(&self) -> &[Plane] {
        &self.planes
    }

    pub fn bounds(&self) -> Aabb {
        self.bounds
    }

    pub fn interior_point(&self) -> Vec3 {
        self.interior_point
    }

    pub fn support(&self, direction: Vec3) -> Result<Vec3, GeometryError> {
        if !direction.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        Ok(self
            .vertices
            .iter()
            .copied()
            .max_by(|left, right| {
                dot_vec64(*left, direction).total_cmp(&dot_vec64(*right, direction))
            })
            .unwrap_or(self.interior_point))
    }

    pub fn mass_properties(&self, mass: f32) -> Result<crate::MassProperties, GeometryError> {
        if !mass.is_finite() {
            return Err(GeometryError::NonFiniteInput);
        }
        if mass <= 0.0 {
            return Err(GeometryError::InvalidMass);
        }

        let reference = self.interior_point;
        let mut volume = 0.0_f64;
        let mut first_moment = [0.0_f64; 3];
        let mut second_moment = [[0.0_f64; 3]; 3];
        for triangle in &self.triangles {
            let points = [
                reference,
                self.vertices[triangle[0] as usize],
                self.vertices[triangle[1] as usize],
                self.vertices[triangle[2] as usize],
            ];
            let relative = [
                [0.0; 3],
                subtract64(points[1], reference),
                subtract64(points[2], reference),
                subtract64(points[3], reference),
            ];
            let signed_volume = dot64(relative[1], cross64(relative[2], relative[3])) / 6.0;
            let tetra_volume = signed_volume.abs();
            volume += tetra_volume;
            let sums = [
                relative.iter().map(|point| point[0]).sum::<f64>(),
                relative.iter().map(|point| point[1]).sum::<f64>(),
                relative.iter().map(|point| point[2]).sum::<f64>(),
            ];
            for axis in 0..3 {
                first_moment[axis] += tetra_volume * sums[axis] / 4.0;
                for other in 0..3 {
                    let products = relative
                        .iter()
                        .map(|point| point[axis] * point[other])
                        .sum::<f64>();
                    second_moment[axis][other] +=
                        tetra_volume * (sums[axis] * sums[other] + products) / 20.0;
                }
            }
        }
        if !volume.is_finite() || volume <= f64::MIN_POSITIVE {
            return Err(GeometryError::DegenerateHull);
        }

        let density = mass as f64 / volume;
        let center_relative = first_moment.map(|moment| moment / volume);
        let center = reference
            + Vec3::new(
                center_relative[0] as f32,
                center_relative[1] as f32,
                center_relative[2] as f32,
            );
        let q = second_moment;
        let m = mass as f64;
        let cx = center_relative[0];
        let cy = center_relative[1];
        let cz = center_relative[2];
        let ixx = density * (q[1][1] + q[2][2]) - m * (cy * cy + cz * cz);
        let iyy = density * (q[0][0] + q[2][2]) - m * (cx * cx + cz * cz);
        let izz = density * (q[0][0] + q[1][1]) - m * (cx * cx + cy * cy);
        let ixy = -density * q[0][1] + m * cx * cy;
        let ixz = -density * q[0][2] + m * cx * cz;
        let iyz = -density * q[1][2] + m * cy * cz;
        let inertia = Mat3::from_cols(
            Vec3::new(ixx as f32, ixy as f32, ixz as f32),
            Vec3::new(ixy as f32, iyy as f32, iyz as f32),
            Vec3::new(ixz as f32, iyz as f32, izz as f32),
        );
        let volume = volume as f32;
        if !center.is_finite() || !inertia.is_finite() || !volume.is_finite() || volume <= 0.0 {
            return Err(GeometryError::NonFiniteInput);
        }
        Ok(crate::MassProperties {
            mass,
            volume,
            center_of_mass: center,
            inertia,
        })
    }
}

fn make_face(
    mut vertices: [usize; 3],
    points: &[Vec3],
    interior_point: Vec3,
    epsilon: f64,
) -> Result<Face, GeometryError> {
    let a = points[vertices[0]];
    let b = points[vertices[1]];
    let c = points[vertices[2]];
    let normal64 = cross64(subtract64(b, a), subtract64(c, a));
    let length = dot64(normal64, normal64).sqrt();
    if !length.is_finite() || length <= epsilon * epsilon {
        return Err(GeometryError::DegenerateHull);
    }
    let mut normal = Vec3::new(
        (normal64[0] / length) as f32,
        (normal64[1] / length) as f32,
        (normal64[2] / length) as f32,
    );
    if plane_distance(Plane { normal, point: a }, interior_point) > 0.0 {
        vertices.swap(1, 2);
        normal = -normal;
    }
    Ok(Face {
        vertices,
        plane: Plane {
            normal,
            point: points[vertices[0]],
        },
        outside: Vec::new(),
    })
}

fn farthest_outside_point(faces: &[Face], points: &[Vec3]) -> Option<(usize, usize, f64)> {
    let mut best = None;
    for (face_index, face) in faces.iter().enumerate() {
        for &point_index in &face.outside {
            let distance = plane_distance(face.plane, points[point_index]);
            if best.is_none_or(|(_, _, best_distance)| distance > best_distance) {
                best = Some((face_index, point_index, distance));
            }
        }
    }
    best
}

fn plane_distance(plane: Plane, point: Vec3) -> f64 {
    dot64(
        [
            plane.normal.x as f64,
            plane.normal.y as f64,
            plane.normal.z as f64,
        ],
        subtract64(point, plane.point),
    )
}

fn dot_vec64(left: Vec3, right: Vec3) -> f64 {
    dot64(
        [left.x as f64, left.y as f64, left.z as f64],
        [right.x as f64, right.y as f64, right.z as f64],
    )
}

fn subtract64(left: Vec3, right: Vec3) -> [f64; 3] {
    [
        left.x as f64 - right.x as f64,
        left.y as f64 - right.y as f64,
        left.z as f64 - right.z as f64,
    ]
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

fn distance_squared64(left: Vec3, right: Vec3) -> f64 {
    let delta = subtract64(left, right);
    dot64(delta, delta)
}

fn line_distance_squared64(
    origin: Vec3,
    line: [f64; 3],
    line_length_squared: f64,
    point: Vec3,
) -> f64 {
    let delta = subtract64(point, origin);
    let cross = cross64(line, delta);
    dot64(cross, cross) / line_length_squared
}

fn point_plane_distance64(origin: Vec3, normal: [f64; 3], normal_length: f64, point: Vec3) -> f64 {
    dot64(normal, subtract64(point, origin)).abs() / normal_length
}

fn length64(vector: Vec3) -> f64 {
    ((vector.x as f64).powi(2) + (vector.y as f64).powi(2) + (vector.z as f64).powi(2)).sqrt()
}

fn average_four(a: Vec3, b: Vec3, c: Vec3, d: Vec3) -> Vec3 {
    Vec3::new(
        ((a.x as f64 + b.x as f64 + c.x as f64 + d.x as f64) * 0.25) as f32,
        ((a.y as f64 + b.y as f64 + c.y as f64 + d.y as f64) * 0.25) as f32,
        ((a.z as f64 + b.z as f64 + c.z as f64 + d.z as f64) * 0.25) as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::{GeometryError, Hull, MAX_HULL_VERTICES};
    use glam::Vec3;

    #[test]
    fn quickhull_builds_a_tetrahedron_and_supports_real_extreme_points() {
        let hull =
            Hull::from_points(&[Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z, Vec3::splat(0.1)]).unwrap();
        assert_eq!(hull.vertices().len(), 4);
        assert_eq!(hull.triangles().len(), 4);
        let support = hull.support(Vec3::ONE).unwrap();
        assert!(support == Vec3::X || support == Vec3::Y || support == Vec3::Z);
        assert!(hull.planes().iter().all(|plane| plane.normal.is_finite()));
    }

    #[test]
    fn quickhull_handles_coplanar_cube_faces_and_interior_points() {
        let mut points = Vec::new();
        for x in [-1.0, 1.0] {
            for y in [-1.0, 1.0] {
                for z in [-1.0, 1.0] {
                    points.push(Vec3::new(x, y, z));
                }
            }
        }
        points.extend([
            Vec3::ZERO,
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(0.5, 0.5, 0.5),
        ]);
        let hull = Hull::from_points(&points).unwrap();
        assert_eq!(hull.vertices().len(), 8);
        assert!((hull.mass_properties(1.0).unwrap().volume - 8.0).abs() < 1e-5);
    }

    #[test]
    fn quickhull_rejects_too_few_and_degenerate_points() {
        assert_eq!(
            Hull::from_points(&[Vec3::ZERO, Vec3::X, Vec3::Y]),
            Err(GeometryError::TooFewPoints)
        );
        assert_eq!(
            Hull::from_points(&[Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::new(1.0, 1.0, 0.0)]),
            Err(GeometryError::DegenerateHull)
        );
        assert_eq!(
            Hull::from_points(&[
                Vec3::ZERO,
                Vec3::X,
                Vec3::splat(2.0),
                Vec3::new(3.0, 3.0, 3.0)
            ]),
            Err(GeometryError::DegenerateHull)
        );
    }

    #[test]
    fn quickhull_enforces_vertex_limit() {
        let points: Vec<Vec3> = (0..=MAX_HULL_VERTICES)
            .map(|index| {
                let y = 1.0 - 2.0 * (index as f32 + 0.5) / (MAX_HULL_VERTICES + 1) as f32;
                let radius = (1.0 - y * y).sqrt();
                let angle = index as f32 * 2.399_963_1;
                Vec3::new(radius * angle.cos(), y, radius * angle.sin())
            })
            .collect();
        assert_eq!(
            Hull::from_points(&points),
            Err(GeometryError::HullVertexLimit)
        );
    }
}
