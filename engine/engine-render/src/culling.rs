use crate::{DrawCommand, Vertex};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct LocalBounds {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

impl LocalBounds {
    pub fn from_vertices(vertices: &[Vertex]) -> Option<Self> {
        let first = vertices.first()?;
        if !first.position.into_iter().all(f32::is_finite) {
            return None;
        }

        let mut bounds = Self {
            min: first.position,
            max: first.position,
        };
        for vertex in &vertices[1..] {
            if !vertex.position.into_iter().all(f32::is_finite) {
                return None;
            }
            for axis in 0..3 {
                bounds.min[axis] = bounds.min[axis].min(vertex.position[axis]);
                bounds.max[axis] = bounds.max[axis].max(vertex.position[axis]);
            }
        }
        Some(bounds)
    }

    fn is_valid(self) -> bool {
        (0..3).all(|axis| self.min[axis].is_finite() && self.max[axis].is_finite())
            && (0..3).all(|axis| self.min[axis] <= self.max[axis])
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Frustum {
    planes: Option<[[f32; 4]; 6]>,
}

impl Frustum {
    pub fn from_view_projection(columns: [[f32; 4]; 4]) -> Self {
        if !columns.into_iter().flatten().all(f32::is_finite) {
            return Self { planes: None };
        }

        let rows = std::array::from_fn::<_, 4, _>(|row| {
            [
                columns[0][row],
                columns[1][row],
                columns[2][row],
                columns[3][row],
            ]
        });
        let add = |left: [f32; 4], right: [f32; 4]| {
            std::array::from_fn(|index| left[index] + right[index])
        };
        let sub = |left: [f32; 4], right: [f32; 4]| {
            std::array::from_fn(|index| left[index] - right[index])
        };
        let planes = [
            add(rows[3], rows[0]),
            sub(rows[3], rows[0]),
            add(rows[3], rows[1]),
            sub(rows[3], rows[1]),
            rows[2],
            sub(rows[3], rows[2]),
        ];
        if !planes.into_iter().flatten().all(f32::is_finite) {
            return Self { planes: None };
        }
        Self {
            planes: Some(planes),
        }
    }

    /// Returns false only when all transformed bounds corners are outside one clip plane.
    pub fn intersects(self, model: [[f32; 4]; 4], bounds: LocalBounds) -> bool {
        let Some(planes) = self.planes else {
            return true;
        };
        if !bounds.is_valid() || !model.into_iter().flatten().all(f32::is_finite) {
            return true;
        }

        for plane in planes {
            let mut all_outside = true;
            for x in [bounds.min[0], bounds.max[0]] {
                for y in [bounds.min[1], bounds.max[1]] {
                    for z in [bounds.min[2], bounds.max[2]] {
                        let world = transform(model, [x, y, z, 1.0]);
                        let distance = dot(plane, world);
                        if !distance.is_finite() {
                            return true;
                        }
                        if distance >= 0.0 {
                            all_outside = false;
                            break;
                        }
                    }
                    if !all_outside {
                        break;
                    }
                }
                if !all_outside {
                    break;
                }
            }
            if all_outside {
                return false;
            }
        }
        true
    }
}

/// Indices ordered back-to-front by distance to each transformed world AABB center.
pub(crate) fn back_to_front_order(
    draws: &[DrawCommand],
    camera_position: [f32; 3],
    mut bounds_for_draw: impl FnMut(&DrawCommand) -> Option<LocalBounds>,
) -> Vec<usize> {
    let mut distances = draws
        .iter()
        .enumerate()
        .map(|(index, draw)| {
            let distance = bounds_for_draw(draw)
                .and_then(|bounds| world_aabb_center(draw.model, bounds))
                .filter(|_| camera_position.into_iter().all(f32::is_finite))
                .map_or(0.0, |center| {
                    (0..3)
                        .map(|axis| {
                            let delta = center[axis] - camera_position[axis];
                            delta * delta
                        })
                        .sum()
                });
            (index, distance)
        })
        .collect::<Vec<_>>();
    distances.sort_by(
        |(left_index, left_distance), (right_index, right_distance)| {
            right_distance
                .total_cmp(left_distance)
                .then_with(|| left_index.cmp(right_index))
        },
    );
    distances.into_iter().map(|(index, _)| index).collect()
}

fn world_aabb_center(model: [[f32; 4]; 4], bounds: LocalBounds) -> Option<[f32; 3]> {
    if !bounds.is_valid() || !model.into_iter().flatten().all(f32::is_finite) {
        return None;
    }
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for x in [bounds.min[0], bounds.max[0]] {
        for y in [bounds.min[1], bounds.max[1]] {
            for z in [bounds.min[2], bounds.max[2]] {
                let world = transform(model, [x, y, z, 1.0]);
                if !world.into_iter().all(f32::is_finite) || world[3].abs() <= f32::EPSILON {
                    return None;
                }
                for axis in 0..3 {
                    let position = world[axis] / world[3];
                    if !position.is_finite() {
                        return None;
                    }
                    min[axis] = min[axis].min(position);
                    max[axis] = max[axis].max(position);
                }
            }
        }
    }
    Some(std::array::from_fn(|axis| (min[axis] + max[axis]) * 0.5))
}

fn transform(columns: [[f32; 4]; 4], vector: [f32; 4]) -> [f32; 4] {
    std::array::from_fn(|row| {
        columns[0][row] * vector[0]
            + columns[1][row] * vector[1]
            + columns[2][row] * vector[2]
            + columns[3][row] * vector[3]
    })
}

fn dot(left: [f32; 4], right: [f32; 4]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2] + left[3] * right[3]
}

#[cfg(test)]
mod tests {
    use super::{Frustum, LocalBounds, back_to_front_order};
    use crate::{DrawCommand, MeshId, Vertex};

    const IDENTITY: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    fn make_vertices(positions: &[[f32; 3]]) -> Vec<Vertex> {
        positions
            .iter()
            .map(|&position| Vertex {
                position,
                ..Vertex::default()
            })
            .collect()
    }

    fn bounds(min: [f32; 3], max: [f32; 3]) -> LocalBounds {
        LocalBounds { min, max }
    }

    fn perspective() -> [[f32; 4]; 4] {
        let near = 1.0;
        let far = 10.0;
        [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, far / (near - far), -1.0],
            [0.0, 0.0, near * far / (near - far), 0.0],
        ]
    }

    fn translated(x: f32, y: f32, z: f32) -> [[f32; 4]; 4] {
        let mut model = IDENTITY;
        model[3] = [x, y, z, 1.0];
        model
    }

    fn draw(mesh: usize, model: [[f32; 4]; 4]) -> DrawCommand {
        DrawCommand {
            mesh: MeshId(mesh),
            material: None,
            model,
            casts_shadow: true,
        }
    }

    #[test]
    fn transparent_order_is_back_to_front_and_equal_distances_keep_input_order() {
        let draws = [
            draw(0, translated(0.0, 0.0, 2.0)),
            draw(1, translated(0.0, 0.0, -4.0)),
            draw(2, translated(0.0, 0.0, 4.0)),
        ];
        let bounds = bounds([-0.5; 3], [0.5; 3]);
        assert_eq!(
            back_to_front_order(&draws, [0.0; 3], |_| Some(bounds)),
            [1, 2, 0]
        );
    }

    #[test]
    fn transparent_order_uses_world_aabb_centers_and_keeps_unknown_bounds_stable() {
        let draws = [
            draw(0, translated(0.0, 0.0, -5.0)),
            draw(1, translated(0.0, 0.0, -2.0)),
            draw(2, translated(0.0, 0.0, -2.0)),
        ];
        let bounds = [
            Some(bounds([-1.0, -1.0, -1.0], [1.0, 1.0, 1.0])),
            None,
            None,
        ];
        assert_eq!(
            back_to_front_order(&draws, [0.0; 3], |draw| bounds[draw.mesh.0]),
            [0, 1, 2]
        );
    }

    #[test]
    fn bounds_are_computed_from_all_vertices_and_allow_degenerate_axes() {
        let vertices = make_vertices(&[[-2.0, 0.0, -3.0], [1.0, 0.0, 4.0], [0.5, 0.0, 1.0]]);
        assert_eq!(
            LocalBounds::from_vertices(&vertices),
            Some(bounds([-2.0, 0.0, -3.0], [1.0, 0.0, 4.0]))
        );
        assert_eq!(LocalBounds::from_vertices(&[]), None);
        assert_eq!(
            LocalBounds::from_vertices(&make_vertices(&[[f32::NAN, 0.0, 0.0]])),
            None
        );
    }

    #[test]
    fn perspective_frustum_rejects_behind_left_and_far_bounds() {
        let frustum = Frustum::from_view_projection(perspective());
        let small = bounds([-0.1; 3], [0.1; 3]);
        assert!(frustum.intersects(translated(0.0, 0.0, -2.0), small));
        assert!(!frustum.intersects(translated(0.0, 0.0, 2.0), small));
        assert!(!frustum.intersects(translated(-5.0, 0.0, -2.0), small));
        assert!(!frustum.intersects(translated(0.0, 0.0, -20.0), small));
    }

    #[test]
    fn partial_bounds_and_bounds_enclosing_camera_are_kept() {
        let frustum = Frustum::from_view_projection(perspective());
        assert!(frustum.intersects(
            translated(-2.5, 0.0, -2.0),
            bounds([-1.0, -0.1, -0.1], [1.0, 0.1, 0.1])
        ));
        assert!(frustum.intersects(IDENTITY, bounds([-100.0; 3], [100.0; 3])));
    }

    #[test]
    fn scene_shadow_orthographic_matrix_culls_outside_the_shadow_volume() {
        // Matches shadow_view_projection(NEG_Z, ZERO, 1): a unit orthographic
        // volume looking from +Z, with DirectX [0, 1] clip depth.
        let light_view_projection = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, -0.5, 0.0],
            [0.0, 0.0, 0.5, 1.0],
        ];
        let frustum = Frustum::from_view_projection(light_view_projection);
        let small = bounds([-0.1; 3], [0.1; 3]);
        assert!(frustum.intersects(IDENTITY, small));
        assert!(!frustum.intersects(translated(2.0, 0.0, 0.0), small));
        assert!(!frustum.intersects(translated(0.0, 0.0, 2.0), small));
        assert!(!frustum.intersects(translated(0.0, 0.0, -2.0), small));
    }

    #[test]
    fn invalid_matrices_and_transforms_keep_bounds() {
        let mut invalid = perspective();
        invalid[0][0] = f32::NAN;
        let small = bounds([-0.1; 3], [0.1; 3]);
        assert!(
            Frustum::from_view_projection(invalid).intersects(translated(100.0, 0.0, 2.0), small)
        );

        let mut invalid_model = translated(100.0, 0.0, 2.0);
        invalid_model[0][0] = f32::NAN;
        assert!(Frustum::from_view_projection(perspective()).intersects(invalid_model, small));
    }
}
