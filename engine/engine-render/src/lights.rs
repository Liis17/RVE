//! Отбор локальных источников света, слоты их теней и упаковка в GPU-буферы.

use crate::*;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SelectedLocalLight {
    pub(crate) source_index: usize,
    pub(crate) light: LocalLight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalShadowKind {
    Point,
    Spot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LocalShadowSlot {
    pub(crate) layer_base: u32,
    pub(crate) kind: LocalShadowKind,
}

pub(crate) struct LocalShadowPlan {
    pub(crate) assignments: Vec<Option<LocalShadowSlot>>,
    pub(crate) matrices: Vec<[[f32; 4]; 4]>,
}

#[cfg(test)]
pub(crate) fn select_local_lights(
    camera: FrameCamera,
    lights: &[LocalLight],
) -> Result<Vec<LocalLight>, RenderError> {
    Ok(select_local_light_entries(camera, lights, [0.0; 2])?
        .into_iter()
        .map(|selected| selected.light)
        .collect())
}

pub(crate) fn select_local_light_entries(
    camera: FrameCamera,
    lights: &[LocalLight],
    culling_padding: [f32; 2],
) -> Result<Vec<SelectedLocalLight>, RenderError> {
    if !camera.position.into_iter().all(f32::is_finite)
        || !camera.view.iter().flatten().all(|value| value.is_finite())
        || !camera
            .projection
            .iter()
            .flatten()
            .all(|value| value.is_finite())
    {
        return Err(RenderError::InvalidConfig(
            "camera matrices and position must be finite",
        ));
    }
    let view_projection = multiply_4x4(camera.projection, camera.view);
    let frustum = Frustum::from_view_projection_with_padding(view_projection, culling_padding);
    let mut visible = Vec::with_capacity(lights.len().min(MAX_RENDERED_LIGHTS));
    for (index, light) in lights.iter().copied().enumerate() {
        let light = validate_local_light(light)?;
        if local_light_visible(frustum, light) {
            visible.push((
                index,
                local_light_contribution(camera.position, light),
                light,
            ));
        }
    }
    visible.sort_by(
        |(left_index, left_score, _), (right_index, right_score, _)| {
            right_score
                .total_cmp(left_score)
                .then_with(|| left_index.cmp(right_index))
        },
    );
    Ok(visible
        .into_iter()
        .take(MAX_RENDERED_LIGHTS)
        .map(|(source_index, _, light)| SelectedLocalLight {
            source_index,
            light,
        })
        .collect())
}

pub(crate) fn index_local_light_shadows<'a>(
    lights: &[LocalLight],
    shadows: &'a [LocalLightShadow],
) -> Result<Vec<Option<&'a LocalLightShadow>>, RenderError> {
    let mut indexed = vec![None; lights.len()];
    for shadow in shadows {
        let Some(light) = lights.get(shadow.light_index) else {
            return Err(RenderError::InvalidConfig(
                "local light shadow index must refer to an input light",
            ));
        };
        if indexed[shadow.light_index].is_some() {
            return Err(RenderError::InvalidConfig(
                "local light shadow indices must be unique",
            ));
        }
        let (matches_light, matrices_finite) = match (light, shadow.matrices) {
            (LocalLight::Point { .. }, LocalLightShadowMatrices::Point { matrices }) => (
                true,
                matrices
                    .iter()
                    .all(|matrix| matrix.iter().flatten().all(|value| value.is_finite())),
            ),
            (LocalLight::Spot { .. }, LocalLightShadowMatrices::Spot { matrix }) => {
                (true, matrix.iter().flatten().all(|value| value.is_finite()))
            }
            _ => (false, false),
        };
        if !matches_light || !matrices_finite {
            return Err(RenderError::InvalidConfig(
                "local light shadow matrices must be finite and match the light type",
            ));
        }
        indexed[shadow.light_index] = Some(shadow);
    }
    Ok(indexed)
}

pub(crate) fn select_local_shadow_slots(
    lights: &[SelectedLocalLight],
    requests: &[Option<&LocalLightShadow>],
    camera_position: [f32; 3],
) -> LocalShadowPlan {
    let mut candidates = lights
        .iter()
        .enumerate()
        .filter_map(|(selected_index, selected)| {
            let shadow = requests.get(selected.source_index).copied().flatten()?;
            let (kind, matrix_count) = match shadow.matrices {
                LocalLightShadowMatrices::Point { .. } => (LocalShadowKind::Point, 6),
                LocalLightShadowMatrices::Spot { .. } => (LocalShadowKind::Spot, 1),
            };
            Some((
                selected_index,
                selected.source_index,
                local_light_camera_distance_squared(selected.light, camera_position),
                kind,
                matrix_count,
                shadow.matrices,
            ))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        left.2
            .total_cmp(&right.2)
            .then_with(|| left.1.cmp(&right.1))
    });

    let mut assignments = vec![None; lights.len()];
    let mut matrices = Vec::with_capacity(MAX_LOCAL_SHADOW_LAYERS);
    for (selected_index, _, _, kind, matrix_count, source) in candidates {
        if matrices.len() + matrix_count > MAX_LOCAL_SHADOW_LAYERS {
            continue;
        }
        let layer_base = matrices.len() as u32;
        match source {
            LocalLightShadowMatrices::Point { matrices: faces } => matrices.extend(faces),
            LocalLightShadowMatrices::Spot { matrix } => matrices.push(matrix),
        }
        assignments[selected_index] = Some(LocalShadowSlot { layer_base, kind });
    }
    LocalShadowPlan {
        assignments,
        matrices,
    }
}

pub(crate) fn local_light_camera_distance_squared(light: LocalLight, camera: [f32; 3]) -> f64 {
    let position = match light {
        LocalLight::Point { position, .. } | LocalLight::Spot { position, .. } => position,
    };
    (0..3)
        .map(|axis| (f64::from(position[axis]) - f64::from(camera[axis])).powi(2))
        .sum()
}

#[cfg(test)]
pub(crate) fn point_shadow_face_index(direction: [f32; 3]) -> u32 {
    let magnitude = direction.map(f32::abs);
    if magnitude[0] >= magnitude[1] && magnitude[0] >= magnitude[2] {
        return u32::from(direction[0] < 0.0);
    }
    if magnitude[1] >= magnitude[2] {
        return if direction[1] >= 0.0 { 2 } else { 3 };
    }
    if direction[2] >= 0.0 { 4 } else { 5 }
}

pub(crate) fn validate_local_light(light: LocalLight) -> Result<LocalLight, RenderError> {
    let validate_common = |position: [f32; 3], color: [f32; 3], range: f32| {
        if !position.into_iter().all(f32::is_finite) {
            return Err(RenderError::InvalidConfig(
                "local light position must contain finite values",
            ));
        }
        if !color
            .into_iter()
            .all(|value| value.is_finite() && value >= 0.0)
        {
            return Err(RenderError::InvalidConfig(
                "local light color_intensity must contain finite non-negative values",
            ));
        }
        if !range.is_finite() || range <= 0.0 {
            return Err(RenderError::InvalidConfig(
                "local light range must be finite and positive",
            ));
        }
        Ok(())
    };
    match light {
        LocalLight::Point {
            position,
            color_intensity,
            range,
        } => {
            validate_common(position, color_intensity, range)?;
            Ok(light)
        }
        LocalLight::Spot {
            position,
            direction,
            color_intensity,
            range,
            cos_inner_angle,
            cos_outer_angle,
        } => {
            validate_common(position, color_intensity, range)?;
            if !direction.into_iter().all(f32::is_finite) {
                return Err(RenderError::InvalidConfig(
                    "spot light direction must contain finite values",
                ));
            }
            let length = direction
                .into_iter()
                .map(|value| f64::from(value) * f64::from(value))
                .sum::<f64>()
                .sqrt();
            if !length.is_finite() || length <= f64::EPSILON {
                return Err(RenderError::InvalidConfig(
                    "spot light direction must be non-zero",
                ));
            }
            let normalize_angle_cosine = |value: f32| {
                if (-f32::EPSILON..0.0).contains(&value) {
                    0.0
                } else {
                    value
                }
            };
            let cos_inner_angle = normalize_angle_cosine(cos_inner_angle);
            let cos_outer_angle = normalize_angle_cosine(cos_outer_angle);
            if !cos_inner_angle.is_finite()
                || !cos_outer_angle.is_finite()
                || cos_inner_angle < 0.0
                || cos_inner_angle > 1.0
                || cos_outer_angle < 0.0
                || cos_outer_angle > cos_inner_angle
            {
                return Err(RenderError::InvalidConfig(
                    "spot light cos angles must satisfy 0 <= cos_outer <= cos_inner <= 1",
                ));
            }
            Ok(LocalLight::Spot {
                position,
                direction: direction.map(|value| (f64::from(value) / length) as f32),
                color_intensity,
                range,
                cos_inner_angle,
                cos_outer_angle,
            })
        }
    }
}

pub(crate) fn local_light_visible(frustum: Frustum, light: LocalLight) -> bool {
    match light {
        LocalLight::Point {
            position, range, ..
        } => frustum.intersects_sphere(position, range),
        LocalLight::Spot {
            position,
            direction,
            range,
            cos_outer_angle,
            ..
        } => frustum.intersects_spot_sector(position, direction, range, cos_outer_angle),
    }
}

pub(crate) fn local_light_contribution(camera: [f32; 3], light: LocalLight) -> f64 {
    let (position, color, range) = match light {
        LocalLight::Point {
            position,
            color_intensity,
            range,
        }
        | LocalLight::Spot {
            position,
            color_intensity,
            range,
            ..
        } => (position, color_intensity, range),
    };
    let distance_squared = (0..3)
        .map(|axis| {
            let delta = f64::from(camera[axis]) - f64::from(position[axis]);
            delta * delta
        })
        .sum::<f64>();
    let range = f64::from(range);
    let normalized_distance_squared = distance_squared / (range * range);
    let range_window =
        (1.0 - normalized_distance_squared * normalized_distance_squared).clamp(0.0, 1.0);
    let spot_weight = match light {
        LocalLight::Point { .. } => 1.0,
        LocalLight::Spot {
            position,
            direction,
            cos_inner_angle,
            cos_outer_angle,
            ..
        } => {
            let delta = std::array::from_fn::<_, 3, _>(|axis| {
                f64::from(camera[axis]) - f64::from(position[axis])
            });
            if distance_squared <= 1e-12 {
                1.0
            } else {
                let inverse_distance = distance_squared.sqrt().recip();
                let ray_cosine = (0..3)
                    .map(|axis| f64::from(direction[axis]) * delta[axis])
                    .sum::<f64>()
                    * inverse_distance;
                let outer = f64::from(cos_outer_angle.max(0.0));
                let inner = f64::from(cos_inner_angle);
                if inner - outer <= 1e-7 {
                    f64::from(ray_cosine >= outer)
                } else {
                    let amount = ((ray_cosine - outer) / (inner - outer)).clamp(0.0, 1.0);
                    amount * amount * (3.0 - 2.0 * amount)
                }
            }
        }
    };
    let luminance =
        f64::from(color[0]) * 0.2126 + f64::from(color[1]) * 0.7152 + f64::from(color[2]) * 0.0722;
    luminance * range_window * range_window * spot_weight / distance_squared.max(1e-4)
}

pub(crate) fn local_light_bytes(
    lights: &[LocalLight],
    view: [[f32; 4]; 4],
) -> Result<Vec<u8>, RenderError> {
    let selected = lights
        .iter()
        .copied()
        .enumerate()
        .map(|(source_index, light)| SelectedLocalLight {
            source_index,
            light,
        })
        .collect::<Vec<_>>();
    local_light_bytes_for_selected(&selected, &[], view)
}

pub(crate) fn local_light_bytes_for_selected(
    lights: &[SelectedLocalLight],
    assignments: &[Option<LocalShadowSlot>],
    view: [[f32; 4]; 4],
) -> Result<Vec<u8>, RenderError> {
    if !lights.is_empty() {
        // Cluster spheres and cones retain world-space radii and angles, so
        // the camera transform must preserve distances and angles.
        let rigid = (0..3).all(|a| {
            (0..3).all(|b| {
                let dot = (0..3).map(|row| view[a][row] * view[b][row]).sum::<f32>();
                (dot - if a == b { 1.0 } else { 0.0 }).abs() <= 1e-4
            })
        }) && (0..3).all(|column| view[column][3] == 0.0)
            && view[3][3] == 1.0;
        if !rigid {
            return Err(RenderError::InvalidConfig(
                "camera view must be a rigid affine transform when clustering local lights",
            ));
        }
    }
    let mut packed = Vec::with_capacity(lights.len().max(1));
    for (selected_index, selected) in lights.iter().enumerate() {
        let shadow_params =
            assignments
                .get(selected_index)
                .copied()
                .flatten()
                .map_or([0; 4], |slot| {
                    [
                        slot.layer_base,
                        match slot.kind {
                            LocalShadowKind::Point => 1,
                            LocalShadowKind::Spot => 2,
                        },
                        1,
                        0,
                    ]
                });
        packed.push(match selected.light {
            LocalLight::Point {
                position,
                color_intensity,
                range,
            } => {
                let view_position = view_position(view, position)?;
                GpuLocalLight {
                    position_range: [position[0], position[1], position[2], range],
                    direction_type: [0.0; 4],
                    color_intensity: [
                        color_intensity[0],
                        color_intensity[1],
                        color_intensity[2],
                        0.0,
                    ],
                    cone_params: [0.0; 4],
                    view_position_range: [
                        view_position[0],
                        view_position[1],
                        view_position[2],
                        range,
                    ],
                    view_direction: [0.0; 4],
                    shadow_params,
                }
            }
            LocalLight::Spot {
                position,
                direction,
                color_intensity,
                range,
                cos_inner_angle,
                cos_outer_angle,
            } => {
                let view_position = view_position(view, position)?;
                let view_direction = view_direction(view, direction)?;
                GpuLocalLight {
                    position_range: [position[0], position[1], position[2], range],
                    direction_type: [direction[0], direction[1], direction[2], 1.0],
                    color_intensity: [
                        color_intensity[0],
                        color_intensity[1],
                        color_intensity[2],
                        0.0,
                    ],
                    cone_params: [cos_inner_angle, cos_outer_angle, 0.0, 0.0],
                    view_position_range: [
                        view_position[0],
                        view_position[1],
                        view_position[2],
                        range,
                    ],
                    view_direction: [view_direction[0], view_direction[1], view_direction[2], 0.0],
                    shadow_params,
                }
            }
        });
    }
    if packed.is_empty() {
        packed.push(GpuLocalLight::default());
    }
    // SAFETY: GpuLocalLight is repr(C), aligned, and contains initialized vec4/uvec4 fields.
    Ok(unsafe {
        std::slice::from_raw_parts(packed.as_ptr().cast(), size_of_val(packed.as_slice())).to_vec()
    })
}

pub(crate) fn local_shadow_matrix_bytes(
    matrices: &[[[f32; 4]; 4]],
) -> Result<[u8; LOCAL_SHADOW_MATRIX_BUFFER_SIZE], RenderError> {
    if matrices.len() > MAX_LOCAL_SHADOW_LAYERS
        || matrices
            .iter()
            .any(|matrix| !matrix.iter().flatten().all(|value| value.is_finite()))
    {
        return Err(RenderError::InvalidConfig(
            "local shadow matrices must be finite and fit the shared layer array",
        ));
    }
    let mut bytes = [0; LOCAL_SHADOW_MATRIX_BUFFER_SIZE];
    for layer in 0..MAX_LOCAL_SHADOW_LAYERS {
        let matrix = matrices.get(layer).unwrap_or(&IDENTITY_MATRIX);
        let matrix_bytes = unsafe {
            std::slice::from_raw_parts(matrix.as_ptr().cast::<u8>(), size_of::<[[f32; 4]; 4]>())
        };
        let start = layer * size_of::<[[f32; 4]; 4]>();
        bytes[start..start + matrix_bytes.len()].copy_from_slice(matrix_bytes);
    }
    Ok(bytes)
}

pub(crate) fn cluster_bounds_bytes(bounds: &[ClusterBounds]) -> &[u8] {
    debug_assert_eq!(bounds.len(), CLUSTER_COUNT);
    debug_assert_eq!(size_of::<ClusterBounds>(), CLUSTER_BOUNDS_GPU_SIZE);
    // SAFETY: ClusterBounds is repr(C), aligned to 16 bytes, and contains only initialized vec4s.
    unsafe { std::slice::from_raw_parts(bounds.as_ptr().cast(), size_of_val(bounds)) }
}
