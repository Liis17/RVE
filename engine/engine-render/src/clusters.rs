#[cfg(test)]
use crate::MAX_CLUSTER_LIGHTS;
use crate::{
    CLUSTER_COUNT, CLUSTER_GRID_X, CLUSTER_GRID_Y, CLUSTER_GRID_Z, RenderError,
    environment::inverse4x4,
};
#[cfg(test)]
use ash::vk;

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ClusterBounds {
    pub min: [f32; 4],
    pub max: [f32; 4],
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct GpuLocalLight {
    pub position_range: [f32; 4],
    pub direction_type: [f32; 4],
    pub color_intensity: [f32; 4],
    pub cone_params: [f32; 4],
    pub view_position_range: [f32; 4],
    pub view_direction: [f32; 4],
    pub shadow_params: [u32; 4],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DepthRange {
    pub near: f32,
    pub far: f32,
}

pub(crate) fn camera_cluster_bounds(
    projection: [[f32; 4]; 4],
) -> Result<(DepthRange, Vec<ClusterBounds>), RenderError> {
    let inverse = inverse4x4(projection).ok_or(RenderError::InvalidConfig(
        "camera projection matrix must be invertible",
    ))?;
    let near_point = unproject(inverse, [0.0, 0.0, 0.0])?;
    let far_point = unproject(inverse, [0.0, 0.0, 1.0])?;
    let range = DepthRange {
        near: -near_point[2],
        far: -far_point[2],
    };
    if !range.near.is_finite()
        || !range.far.is_finite()
        || range.near <= 0.0
        || range.far <= range.near
        || !(range.far / range.near).is_finite()
    {
        return Err(RenderError::InvalidConfig(
            "camera projection must have finite 0 < near < far view depths",
        ));
    }
    for x in [-1.0, 1.0] {
        for y in [-1.0, 1.0] {
            for (clip_depth, expected) in [(0.0, range.near), (1.0, range.far)] {
                let point = unproject(inverse, [x, y, clip_depth])?;
                let actual = -point[2];
                if (actual - expected).abs() > expected.max(1.0) * 1e-4 {
                    return Err(RenderError::InvalidConfig(
                        "camera projection must use parallel Vulkan 0..1 near/far planes",
                    ));
                }
            }
        }
    }

    let mut bounds = Vec::with_capacity(CLUSTER_COUNT);
    for z in 0..CLUSTER_GRID_Z {
        let depth_min = depth_boundary(range, z);
        let depth_max = depth_boundary(range, z + 1);
        for y in 0..CLUSTER_GRID_Y {
            let ndc_top = 1.0 - 2.0 * y as f32 / CLUSTER_GRID_Y as f32;
            let ndc_bottom = 1.0 - 2.0 * (y + 1) as f32 / CLUSTER_GRID_Y as f32;
            for x in 0..CLUSTER_GRID_X {
                let ndc_left = 2.0 * x as f32 / CLUSTER_GRID_X as f32 - 1.0;
                let ndc_right = 2.0 * (x + 1) as f32 / CLUSTER_GRID_X as f32 - 1.0;
                let mut current = ClusterBounds {
                    min: [f32::INFINITY; 4],
                    max: [f32::NEG_INFINITY; 4],
                };
                for depth in [depth_min, depth_max] {
                    for ndc_x in [ndc_left, ndc_right] {
                        for ndc_y in [ndc_top, ndc_bottom] {
                            let point = unproject_at_view_depth(inverse, ndc_x, ndc_y, depth)?;
                            for (axis, value) in point.into_iter().enumerate() {
                                current.min[axis] = current.min[axis].min(value);
                                current.max[axis] = current.max[axis].max(value);
                            }
                        }
                    }
                }
                let epsilon = depth_max.max(1.0) * 1e-5;
                for axis in 0..3 {
                    current.min[axis] -= epsilon;
                    current.max[axis] += epsilon;
                }
                current.min[3] = 0.0;
                current.max[3] = 0.0;
                bounds.push(current);
            }
        }
    }
    debug_assert_eq!(bounds.len(), CLUSTER_COUNT);
    Ok((range, bounds))
}

pub(crate) fn depth_boundary(range: DepthRange, slice: usize) -> f32 {
    match slice {
        0 => range.near,
        slice if slice >= CLUSTER_GRID_Z => range.far,
        slice => range.near * (range.far / range.near).powf(slice as f32 / CLUSTER_GRID_Z as f32),
    }
}

#[cfg(test)]
pub(crate) fn depth_slice(view_depth: f32, range: DepthRange) -> usize {
    if view_depth <= range.near {
        return 0;
    }
    if view_depth >= range.far {
        return CLUSTER_GRID_Z - 1;
    }
    let scaled =
        (view_depth / range.near).ln() / (range.far / range.near).ln() * CLUSTER_GRID_Z as f32;
    let mut slice = (scaled.floor() as usize).min(CLUSTER_GRID_Z - 1);
    if slice > 0 && view_depth < depth_boundary(range, slice) {
        slice -= 1;
    }
    if slice + 1 < CLUSTER_GRID_Z && view_depth >= depth_boundary(range, slice + 1) {
        slice += 1;
    }
    slice
}

#[cfg(test)]
pub(crate) fn fragment_cluster_index(
    view_depth: f32,
    frag_coord: [f32; 2],
    extent: vk::Extent2D,
    range: DepthRange,
) -> usize {
    let x = ((frag_coord[0] / extent.width.max(1) as f32 * CLUSTER_GRID_X as f32).floor() as usize)
        .min(CLUSTER_GRID_X - 1);
    let y = ((frag_coord[1] / extent.height.max(1) as f32 * CLUSTER_GRID_Y as f32).floor()
        as usize)
        .min(CLUSTER_GRID_Y - 1);
    (depth_slice(view_depth, range) * CLUSTER_GRID_Y + y) * CLUSTER_GRID_X + x
}

#[cfg(test)]
pub(crate) fn cluster_light_indices(bounds: ClusterBounds, lights: &[GpuLocalLight]) -> Vec<usize> {
    lights
        .iter()
        .enumerate()
        .filter_map(|(index, light)| {
            let intersects = if light.direction_type[3] < 0.5 {
                sphere_intersects_aabb(
                    [
                        light.view_position_range[0],
                        light.view_position_range[1],
                        light.view_position_range[2],
                    ],
                    light.view_position_range[3],
                    bounds,
                )
            } else {
                spot_bound_intersects_aabb(*light, bounds)
            };
            intersects.then_some(index)
        })
        .take(MAX_CLUSTER_LIGHTS)
        .collect()
}

#[cfg(test)]
fn sphere_intersects_aabb(center: [f32; 3], radius: f32, bounds: ClusterBounds) -> bool {
    let delta = std::array::from_fn::<_, 3, _>(|axis| {
        center[axis] - center[axis].clamp(bounds.min[axis], bounds.max[axis])
    });
    delta
        .iter()
        .map(|component| component * component)
        .sum::<f32>()
        <= radius * radius
}

#[cfg(test)]
fn spot_bound_intersects_aabb(light: GpuLocalLight, bounds: ClusterBounds) -> bool {
    let direction = [
        light.view_direction[0],
        light.view_direction[1],
        light.view_direction[2],
    ];
    let direction_length = direction
        .iter()
        .map(|value| value * value)
        .sum::<f32>()
        .sqrt();
    if direction_length <= f32::EPSILON || !direction_length.is_finite() {
        return true;
    }
    let direction = direction.map(|value| value / direction_length);
    let range = light.view_position_range[3];
    let center =
        std::array::from_fn(|axis| light.view_position_range[axis] + direction[axis] * range * 0.5);
    let radius = range * (1.25 - light.cone_params[1].clamp(0.0, 1.0)).sqrt();
    sphere_intersects_aabb(center, radius, bounds)
}

fn unproject(inverse_projection: [[f32; 4]; 4], ndc: [f32; 3]) -> Result<[f32; 3], RenderError> {
    let homogeneous = transform(inverse_projection, [ndc[0], ndc[1], ndc[2], 1.0]);
    if !homogeneous.into_iter().all(f32::is_finite) || homogeneous[3].abs() <= 1e-8 {
        return Err(RenderError::InvalidConfig(
            "camera projection has non-finite or infinite cluster corners",
        ));
    }
    Ok(std::array::from_fn(|axis| {
        homogeneous[axis] / homogeneous[3]
    }))
}

fn unproject_at_view_depth(
    inverse_projection: [[f32; 4]; 4],
    ndc_x: f32,
    ndc_y: f32,
    view_depth: f32,
) -> Result<[f32; 3], RenderError> {
    let base = transform(inverse_projection, [ndc_x, ndc_y, 0.0, 1.0]);
    let depth_axis = transform(inverse_projection, [0.0, 0.0, 1.0, 0.0]);
    let denominator = depth_axis[2] + view_depth * depth_axis[3];
    let clip_depth = (-view_depth * base[3] - base[2]) / denominator;
    if !clip_depth.is_finite() || !(-1e-4..=1.0001).contains(&clip_depth) {
        return Err(RenderError::InvalidConfig(
            "camera projection cannot represent an exponential cluster depth",
        ));
    }
    unproject(
        inverse_projection,
        [ndc_x, ndc_y, clip_depth.clamp(0.0, 1.0)],
    )
}

fn transform(matrix: [[f32; 4]; 4], vector: [f32; 4]) -> [f32; 4] {
    std::array::from_fn(|row| {
        matrix[0][row] * vector[0]
            + matrix[1][row] * vector[1]
            + matrix[2][row] * vector[2]
            + matrix[3][row] * vector[3]
    })
}

#[cfg(test)]
mod tests {
    use super::{
        ClusterBounds, DepthRange, GpuLocalLight, camera_cluster_bounds, cluster_light_indices,
        depth_boundary, depth_slice, fragment_cluster_index,
    };
    use crate::{
        CLUSTER_COUNT, CLUSTER_GRID_X, CLUSTER_GRID_Y, CLUSTER_GRID_Z, MAX_CLUSTER_LIGHTS,
        environment::inverse4x4,
    };
    use ash::vk;

    fn projection(aspect: f32) -> [[f32; 4]; 4] {
        let near = 0.1;
        let far = 20.0;
        [
            [1.0 / aspect.min(1.0), 0.0, 0.0, 0.0],
            [0.0, aspect.max(1.0), 0.0, 0.0],
            [0.0, 0.0, far / (near - far), -1.0],
            [0.0, 0.0, near * far / (near - far), 0.0],
        ]
    }

    fn transform(matrix: [[f32; 4]; 4], vector: [f32; 4]) -> [f32; 4] {
        std::array::from_fn(|row| {
            matrix[0][row] * vector[0]
                + matrix[1][row] * vector[1]
                + matrix[2][row] * vector[2]
                + matrix[3][row] * vector[3]
        })
    }

    #[test]
    fn exponential_depth_slices_clamp_near_far_and_match_boundaries() {
        let range = DepthRange {
            near: 0.1,
            far: 500.0,
        };
        assert_eq!(CLUSTER_GRID_Z, 24);
        assert_eq!(depth_slice(0.0, range), 0);
        assert_eq!(depth_slice(range.near, range), 0);
        assert_eq!(depth_slice(range.far, range), CLUSTER_GRID_Z - 1);
        assert_eq!(depth_slice(range.far * 2.0, range), CLUSTER_GRID_Z - 1);
        for slice in 1..CLUSTER_GRID_Z {
            let boundary = depth_boundary(range, slice);
            assert_eq!(depth_slice(boundary, range), slice);
        }
    }

    #[test]
    fn inverse_projection_bounds_every_cluster_for_landscape_and_portrait_views() {
        assert_eq!(
            (CLUSTER_GRID_X, CLUSTER_GRID_Y, CLUSTER_GRID_Z),
            (16, 9, 24)
        );
        assert_eq!(CLUSTER_COUNT, 16 * 9 * 24);
        for aspect in [16.0 / 9.0, 9.0 / 16.0] {
            let projection = projection(aspect);
            let inverse = inverse4x4(projection).unwrap();
            let (range, bounds) = camera_cluster_bounds(projection).unwrap();
            assert!((range.near - 0.1).abs() < 1e-5);
            assert!((range.far - 20.0).abs() < 1e-3);
            assert_eq!(bounds.len(), CLUSTER_COUNT);
            for z in 0..CLUSTER_GRID_Z {
                let depths = [depth_boundary(range, z), depth_boundary(range, z + 1)];
                for y in 0..CLUSTER_GRID_Y {
                    let ndc_y = [
                        1.0 - 2.0 * y as f32 / CLUSTER_GRID_Y as f32,
                        1.0 - 2.0 * (y + 1) as f32 / CLUSTER_GRID_Y as f32,
                    ];
                    for x in 0..CLUSTER_GRID_X {
                        let ndc_x = [
                            2.0 * x as f32 / CLUSTER_GRID_X as f32 - 1.0,
                            2.0 * (x + 1) as f32 / CLUSTER_GRID_X as f32 - 1.0,
                        ];
                        let cluster_index = (z * CLUSTER_GRID_Y + y) * CLUSTER_GRID_X + x;
                        let cluster = bounds[cluster_index];
                        for depth in depths {
                            let scale = depth / range.near;
                            for clip_x in ndc_x {
                                for clip_y in ndc_y {
                                    let near = transform(inverse, [clip_x, clip_y, 0.0, 1.0]);
                                    let view: [f32; 3] =
                                        std::array::from_fn(|axis| near[axis] / near[3] * scale);
                                    for (axis, value) in view.into_iter().enumerate() {
                                        assert!(value >= cluster.min[axis]);
                                        assert!(value <= cluster.max[axis]);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn negative_viewport_maps_positive_ndc_y_to_the_first_cluster_row() {
        for (aspect, extent) in [
            (
                16.0 / 9.0,
                vk::Extent2D {
                    width: 1600,
                    height: 900,
                },
            ),
            (
                9.0 / 16.0,
                vk::Extent2D {
                    width: 900,
                    height: 1600,
                },
            ),
        ] {
            let range = camera_cluster_bounds(projection(aspect)).unwrap().0;
            for (ndc_x, ndc_y, expected_x, expected_y) in [
                (-0.9, 0.9, 0, 0),
                (0.9, 0.9, CLUSTER_GRID_X - 1, 0),
                (-0.9, -0.9, 0, CLUSTER_GRID_Y - 1),
                (0.9, -0.9, CLUSTER_GRID_X - 1, CLUSTER_GRID_Y - 1),
            ] {
                let pixel_x = (ndc_x + 1.0) * 0.5 * extent.width as f32;
                let pixel_y = (1.0 - ndc_y) * 0.5 * extent.height as f32;
                let index = fragment_cluster_index(1.0, [pixel_x, pixel_y], extent, range);
                assert_eq!(index % CLUSTER_GRID_X, expected_x);
                assert_eq!((index / CLUSTER_GRID_X) % CLUSTER_GRID_Y, expected_y);
            }
        }
    }

    #[test]
    fn sphere_and_ninety_degree_spot_bounds_keep_boundary_intersections() {
        let bounds = ClusterBounds {
            min: [0.0, 0.0, 0.0, 0.0],
            max: [1.0, 1.0, 1.0, 0.0],
        };
        let light =
            |position: [f32; 3], direction: [f32; 3], kind: f32, outer: f32| -> GpuLocalLight {
                GpuLocalLight {
                    direction_type: [direction[0], direction[1], direction[2], kind],
                    cone_params: [outer, outer, 0.0, 0.0],
                    view_position_range: [position[0], position[1], position[2], 1.0],
                    ..GpuLocalLight::default()
                }
            };
        let point_on_face = light([-0.5, 0.5, 0.5], [0.0; 3], 0.0, 0.0);
        let hemisphere_reaches_box = light([-0.5, 0.5, 0.5], [1.0, 0.0, 0.0], 1.0, 0.0);
        let hemisphere_points_away = light([-0.5, 0.5, 0.5], [-1.0, 0.0, 0.0], 1.0, 0.0);
        let equal_angle_touches = light([-0.5, 0.5, 0.5], [1.0, 0.0, 0.0], 1.0, 1.0);
        let apex_inside = light([0.5, 0.5, 0.5], [0.0, 0.0, -1.0], 1.0, 0.5);
        let near_camera_slice = ClusterBounds {
            min: [-0.1, -0.1, -0.2, 0.0],
            max: [0.1, 0.1, -0.1, 0.0],
        };
        let cone_contains_camera_slice = light([0.0, 0.0, -1.0], [0.0, 0.0, 1.0], 1.0, 0.5);
        assert_eq!(cluster_light_indices(bounds, &[point_on_face]), [0]);
        assert_eq!(
            cluster_light_indices(bounds, &[hemisphere_reaches_box]),
            [0]
        );
        assert_eq!(cluster_light_indices(bounds, &[equal_angle_touches]), [0]);
        assert_eq!(cluster_light_indices(bounds, &[apex_inside]), [0]);
        assert_eq!(
            cluster_light_indices(near_camera_slice, &[cone_contains_camera_slice]),
            [0]
        );
        // The enclosing sphere can retain a false positive at 90 degrees, which is conservative.
        assert_eq!(
            cluster_light_indices(bounds, &[hemisphere_points_away]),
            [0]
        );
    }

    #[test]
    fn one_cluster_keeps_the_first_128_intersections_in_source_order() {
        assert_eq!(std::mem::size_of::<GpuLocalLight>(), 112);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, view_position_range), 64);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, view_direction), 80);
        assert_eq!(std::mem::offset_of!(GpuLocalLight, shadow_params), 96);
        let bounds = ClusterBounds {
            min: [-1.0; 4],
            max: [1.0; 4],
        };
        let lights = (0..MAX_CLUSTER_LIGHTS + 7)
            .map(|index| GpuLocalLight {
                position_range: [index as f32, 0.0, 0.0, 1.0],
                view_position_range: [0.0, 0.0, 0.0, 1.0],
                ..GpuLocalLight::default()
            })
            .collect::<Vec<_>>();
        let selected = cluster_light_indices(bounds, &lights);
        assert_eq!(selected, (0..MAX_CLUSTER_LIGHTS).collect::<Vec<_>>());
    }
}
