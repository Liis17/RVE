//! Small CPU-only meshes used by the material preview viewport.

use crate::state::MaterialPreviewPrimitive;
use engine_render::Vertex;

pub fn mesh(primitive: MaterialPreviewPrimitive) -> (Vec<Vertex>, Vec<u32>) {
    match primitive {
        MaterialPreviewPrimitive::Plane => plane(),
        MaterialPreviewPrimitive::Cube => cube(),
        MaterialPreviewPrimitive::Sphere => sphere(24, 16),
    }
}

fn vertex(position: [f32; 3], normal: [f32; 3], uv: [f32; 2]) -> Vertex {
    Vertex {
        position,
        normal,
        uv,
        color: [1.0; 3],
    }
}

fn plane() -> (Vec<Vertex>, Vec<u32>) {
    (
        vec![
            vertex([-1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [0.0, 0.0]),
            vertex([1.0, 0.0, -1.0], [0.0, 1.0, 0.0], [1.0, 0.0]),
            vertex([1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [1.0, 1.0]),
            vertex([-1.0, 0.0, 1.0], [0.0, 1.0, 0.0], [0.0, 1.0]),
        ],
        vec![0, 2, 1, 0, 3, 2],
    )
}

fn cube() -> (Vec<Vertex>, Vec<u32>) {
    let faces = [
        // normal, top-left, top-right, bottom-right, bottom-left
        (
            [0.0, 0.0, 1.0],
            [-1.0, 1.0, 1.0],
            [1.0, 1.0, 1.0],
            [1.0, -1.0, 1.0],
            [-1.0, -1.0, 1.0],
        ),
        (
            [0.0, 0.0, -1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
        ),
        (
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 1.0],
            [1.0, 1.0, -1.0],
            [1.0, -1.0, -1.0],
            [1.0, -1.0, 1.0],
        ),
        (
            [-1.0, 0.0, 0.0],
            [-1.0, 1.0, -1.0],
            [-1.0, 1.0, 1.0],
            [-1.0, -1.0, 1.0],
            [-1.0, -1.0, -1.0],
        ),
        (
            [0.0, 1.0, 0.0],
            [-1.0, 1.0, -1.0],
            [1.0, 1.0, -1.0],
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ),
        (
            [0.0, -1.0, 0.0],
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, -1.0, -1.0],
            [-1.0, -1.0, -1.0],
        ),
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for (normal, a, b, c, d) in faces {
        let base = vertices.len() as u32;
        vertices.extend([
            vertex(a, normal, [0.0, 0.0]),
            vertex(b, normal, [1.0, 0.0]),
            vertex(c, normal, [1.0, 1.0]),
            vertex(d, normal, [0.0, 1.0]),
        ]);
        indices.extend([base, base + 2, base + 1, base, base + 3, base + 2]);
    }
    (vertices, indices)
}

fn sphere(segments: u32, rings: u32) -> (Vec<Vertex>, Vec<u32>) {
    let mut vertices = Vec::with_capacity(((segments + 1) * (rings - 1) + 2) as usize);
    let mut indices = Vec::with_capacity((segments * rings * 6) as usize);
    vertices.push(vertex([0.0, 1.0, 0.0], [0.0, 1.0, 0.0], [0.5, 0.0]));
    for ring in 1..rings {
        let v = ring as f32 / rings as f32;
        let theta = v * std::f32::consts::PI;
        for segment in 0..=segments {
            let u = segment as f32 / segments as f32;
            let phi = u * std::f32::consts::TAU;
            let normal = [
                theta.sin() * phi.sin(),
                theta.cos(),
                theta.sin() * phi.cos(),
            ];
            vertices.push(vertex(normal, normal, [u, v]));
        }
    }
    let south_pole = vertices.len() as u32;
    vertices.push(vertex([0.0, -1.0, 0.0], [0.0, -1.0, 0.0], [0.5, 1.0]));
    let first_ring = 1_u32;
    for segment in 0..segments {
        let a = first_ring + segment;
        indices.extend([0, a, a + 1]);
    }
    let interior_rings = rings - 2;
    for ring in 0..interior_rings {
        for segment in 0..segments {
            let a = first_ring + ring * (segments + 1) + segment;
            let b = a + segments + 1;
            indices.extend([a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    let last_ring = first_ring + interior_rings * (segments + 1);
    for segment in 0..segments {
        let a = last_ring + segment;
        indices.extend([a, south_pole, a + 1]);
    }
    (vertices, indices)
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;

    #[test]
    fn preview_mesh_indices_are_in_range_normals_are_unit_and_winding_is_outward() {
        for primitive in [
            MaterialPreviewPrimitive::Plane,
            MaterialPreviewPrimitive::Cube,
            MaterialPreviewPrimitive::Sphere,
        ] {
            let (vertices, indices) = mesh(primitive);
            assert!(!vertices.is_empty());
            assert!(!indices.is_empty());
            assert!(
                indices
                    .iter()
                    .all(|index| (*index as usize) < vertices.len())
            );
            for vertex in &vertices {
                assert!((Vec3::from_array(vertex.normal).length() - 1.0).abs() < 1e-5);
            }
            for triangle in indices.as_chunks::<3>().0 {
                let a = &vertices[triangle[0] as usize];
                let b = &vertices[triangle[1] as usize];
                let c = &vertices[triangle[2] as usize];
                let a = Vec3::from_array(a.position);
                let b = Vec3::from_array(b.position);
                let c = Vec3::from_array(c.position);
                let normal = Vec3::from_array(vertices[triangle[0] as usize].normal);
                assert!((b - a).cross(c - a).dot(normal) > 0.0, "{primitive:?}");
            }
        }
    }
}
