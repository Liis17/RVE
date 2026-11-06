use glam::Vec3;

use crate::GeometryError;
use crate::ray::{Ray, RayHit, ray_aabb, ray_triangle};
use crate::shape::Aabb;

pub const MAX_TRIANGLE_COUNT: usize = 262_144;
pub const MAX_MESH_VERTEX_COUNT: usize = 1_000_000;
const BVH_LEAF_TRIANGLES: usize = 4;
const MAX_BVH_DEPTH: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub struct TriMesh {
    vertices: Vec<Vec3>,
    triangles: Vec<[u32; 3]>,
    triangle_bounds: Vec<Aabb>,
    nodes: Vec<BvhNode>,
    triangle_order: Vec<u32>,
    bounds: Aabb,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BvhNode {
    bounds: Aabb,
    left: Option<usize>,
    right: Option<usize>,
    start: usize,
    count: usize,
}

impl TriMesh {
    pub fn new(vertices: Vec<Vec3>, triangles: Vec<[u32; 3]>) -> Result<Self, GeometryError> {
        if vertices.len() > MAX_MESH_VERTEX_COUNT || triangles.len() > MAX_TRIANGLE_COUNT {
            return Err(GeometryError::TooManyPoints);
        }
        if triangles.is_empty() {
            return Err(GeometryError::EmptyTriangleMesh);
        }
        if vertices.iter().any(|vertex| !vertex.is_finite()) {
            return Err(GeometryError::NonFiniteInput);
        }
        for triangle in &triangles {
            if triangle
                .iter()
                .any(|&index| index as usize >= vertices.len())
            {
                return Err(GeometryError::InvalidTriangleIndex);
            }
        }

        let mut triangle_bounds = Vec::with_capacity(triangles.len());
        for triangle in &triangles {
            let a = vertices[triangle[0] as usize];
            let b = vertices[triangle[1] as usize];
            let c = vertices[triangle[2] as usize];
            let edge1 = [
                b.x as f64 - a.x as f64,
                b.y as f64 - a.y as f64,
                b.z as f64 - a.z as f64,
            ];
            let edge2 = [
                c.x as f64 - a.x as f64,
                c.y as f64 - a.y as f64,
                c.z as f64 - a.z as f64,
            ];
            let cross = [
                edge1[1] * edge2[2] - edge1[2] * edge2[1],
                edge1[2] * edge2[0] - edge1[0] * edge2[2],
                edge1[0] * edge2[1] - edge1[1] * edge2[0],
            ];
            if cross.iter().all(|component| *component == 0.0) {
                return Err(GeometryError::DegenerateTriangle);
            }
            triangle_bounds.push(Aabb::from_points(&[a, b, c])?);
        }

        let bounds = triangle_bounds
            .iter()
            .copied()
            .reduce(Aabb::union)
            .ok_or(GeometryError::EmptyTriangleMesh)?;
        let triangle_order: Vec<u32> = (0..triangles.len() as u32).collect();
        let mut mesh = Self {
            vertices,
            triangles,
            triangle_bounds,
            nodes: Vec::with_capacity(mesh_node_capacity(triangle_order.len())),
            triangle_order,
            bounds,
        };
        let triangle_count = mesh.triangle_order.len();
        mesh.build_node(0, triangle_count, 0);
        Ok(mesh)
    }

    pub fn vertices(&self) -> &[Vec3] {
        &self.vertices
    }

    pub fn triangles(&self) -> &[[u32; 3]] {
        &self.triangles
    }

    pub fn bounds(&self) -> Aabb {
        self.bounds
    }

    pub fn query_aabb(&self, query: Aabb) -> Vec<u32> {
        let mut result = Vec::new();
        let mut stack = vec![0];
        while let Some(node_index) = stack.pop() {
            let node = self.nodes[node_index];
            if !node.bounds.overlaps(query) {
                continue;
            }
            if node.count > 0 {
                for &triangle_index in &self.triangle_order[node.start..node.start + node.count] {
                    if self.triangle_bounds[triangle_index as usize].overlaps(query) {
                        result.push(triangle_index);
                    }
                }
            } else {
                if let Some(right) = node.right {
                    stack.push(right);
                }
                if let Some(left) = node.left {
                    stack.push(left);
                }
            }
        }
        result.sort_unstable();
        result
    }

    pub fn raycast(&self, ray: &Ray) -> Option<RayHit> {
        if !ray.is_valid() {
            return None;
        }
        let mut closest = None;
        let mut stack = vec![0];
        while let Some(node_index) = stack.pop() {
            let node = self.nodes[node_index];
            let max_distance = closest.map_or(ray.max_distance, |hit: RayHit| hit.distance);
            if ray_aabb(ray, node.bounds, max_distance).is_none() {
                continue;
            }
            if node.count > 0 {
                for &triangle_index in &self.triangle_order[node.start..node.start + node.count] {
                    let vertices = self.triangles[triangle_index as usize]
                        .map(|index| self.vertices[index as usize]);
                    if let Some(hit) = ray_triangle(ray, vertices, Some(triangle_index))
                        && closest.is_none_or(|best: RayHit| hit.distance < best.distance)
                    {
                        closest = Some(hit);
                    }
                }
            } else {
                if let Some(right) = node.right {
                    stack.push(right);
                }
                if let Some(left) = node.left {
                    stack.push(left);
                }
            }
        }
        closest
    }

    fn build_node(&mut self, start: usize, end: usize, depth: usize) -> usize {
        let bounds = self.triangle_order[start..end]
            .iter()
            .map(|&triangle| self.triangle_bounds[triangle as usize])
            .reduce(Aabb::union)
            .expect("BVH node has at least one triangle");
        let index = self.nodes.len();
        self.nodes.push(BvhNode {
            bounds,
            left: None,
            right: None,
            start,
            count: end - start,
        });
        if end - start <= BVH_LEAF_TRIANGLES || depth >= MAX_BVH_DEPTH {
            return index;
        }

        let first_centroid = self.triangle_bounds[self.triangle_order[start] as usize].center();
        let mut centroid_bounds = Aabb {
            min: first_centroid,
            max: first_centroid,
        };
        for &triangle in &self.triangle_order[start + 1..end] {
            let centroid = self.triangle_bounds[triangle as usize].center();
            centroid_bounds.min = centroid_bounds.min.min(centroid);
            centroid_bounds.max = centroid_bounds.max.max(centroid);
        }
        let extent = centroid_bounds.max - centroid_bounds.min;
        let axis = if extent.x >= extent.y && extent.x >= extent.z {
            0
        } else if extent.y >= extent.z {
            1
        } else {
            2
        };
        self.triangle_order[start..end].sort_by(|left, right| {
            self.triangle_bounds[*left as usize].center()[axis]
                .total_cmp(&self.triangle_bounds[*right as usize].center()[axis])
                .then_with(|| left.cmp(right))
        });
        let middle = start + (end - start) / 2;
        let left = self.build_node(start, middle, depth + 1);
        let right = self.build_node(middle, end, depth + 1);
        self.nodes[index].left = Some(left);
        self.nodes[index].right = Some(right);
        self.nodes[index].count = 0;
        index
    }
}

fn mesh_node_capacity(triangle_count: usize) -> usize {
    triangle_count
        .saturating_mul(2)
        .min(MAX_TRIANGLE_COUNT.saturating_mul(2))
}

#[cfg(test)]
mod tests {
    use super::TriMesh;
    use crate::{Aabb, GeometryError, Ray};
    use glam::Vec3;

    fn sample_mesh() -> TriMesh {
        let mut vertices = Vec::new();
        let mut triangles = Vec::new();
        for index in 0..6 {
            let z = index as f32 * 10.0;
            let first = vertices.len() as u32;
            vertices.extend([
                Vec3::new(0.0, 0.0, z),
                Vec3::new(1.0, 0.0, z),
                Vec3::new(0.0, 1.0, z),
            ]);
            triangles.push([first, first + 1, first + 2]);
        }
        vertices.push(Vec3::splat(100.0));
        TriMesh::new(vertices, triangles).unwrap()
    }

    #[test]
    fn triangle_bvh_queries_overlapping_triangles_and_raycast_hits_nearest() {
        let mesh = sample_mesh();
        assert!(
            mesh.bounds()
                .max
                .abs_diff_eq(Vec3::new(1.0, 1.0, 50.0), 1e-5)
        );
        let query = Aabb::new(Vec3::new(0.1, 0.1, -0.1), Vec3::new(0.4, 0.4, 0.1)).unwrap();
        assert_eq!(mesh.query_aabb(query), vec![0]);

        let ray = Ray::new(Vec3::new(0.25, 0.25, 52.0), -Vec3::Z, 60.0).unwrap();
        let hit = mesh.raycast(&ray).unwrap();
        assert_eq!(hit.triangle_index, Some(5));
        assert!((hit.distance - 2.0).abs() < 1e-5);
        assert!(hit.normal.abs_diff_eq(Vec3::Z, 1e-5));
    }

    #[test]
    fn triangle_bvh_ray_misses_outside_geometry_and_parallel_planes() {
        let mesh = sample_mesh();
        let miss = Ray::new(Vec3::new(2.0, 2.0, 2.0), -Vec3::Z, 60.0).unwrap();
        let parallel = Ray::new(Vec3::new(0.25, 0.25, 2.0), Vec3::X, 60.0).unwrap();
        assert!(mesh.raycast(&miss).is_none());
        assert!(mesh.raycast(&parallel).is_none());
        assert!(
            mesh.query_aabb(Aabb::new(Vec3::splat(20.0), Vec3::splat(21.0)).unwrap())
                .is_empty()
        );
    }

    #[test]
    fn triangle_bvh_rejects_invalid_indices_and_degenerate_geometry() {
        assert_eq!(
            TriMesh::new(vec![Vec3::ZERO, Vec3::X, Vec3::Y], vec![[0, 1, 4]]).unwrap_err(),
            GeometryError::InvalidTriangleIndex
        );
        assert_eq!(
            TriMesh::new(
                vec![Vec3::ZERO, Vec3::X, Vec3::new(2.0, 0.0, 0.0)],
                vec![[0, 1, 2]]
            )
            .unwrap_err(),
            GeometryError::DegenerateTriangle
        );
    }
}
