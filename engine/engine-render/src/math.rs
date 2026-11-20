//! Матричная и векторная арифметика renderer, масштабирование размеров.

use crate::*;

pub(crate) fn transform_4x4(matrix: [[f32; 4]; 4], vector: [f32; 4]) -> [f32; 4] {
    std::array::from_fn(|row| {
        (0..4)
            .map(|column| f64::from(matrix[column][row]) * f64::from(vector[column]))
            .sum::<f64>() as f32
    })
}

pub(crate) fn view_position(
    view: [[f32; 4]; 4],
    position: [f32; 3],
) -> Result<[f32; 3], RenderError> {
    let transformed = transform_4x4(view, [position[0], position[1], position[2], 1.0]);
    if !transformed.into_iter().all(f32::is_finite) || transformed[3].abs() <= f32::EPSILON {
        return Err(RenderError::InvalidConfig(
            "camera view transform produced an invalid local light position",
        ));
    }
    let result = std::array::from_fn(|axis| transformed[axis] / transformed[3]);
    if !result.into_iter().all(f32::is_finite) {
        return Err(RenderError::InvalidConfig(
            "camera view transform produced an invalid local light position",
        ));
    }
    Ok(result)
}

pub(crate) fn view_direction(
    view: [[f32; 4]; 4],
    direction: [f32; 3],
) -> Result<[f32; 3], RenderError> {
    let transformed = transform_4x4(view, [direction[0], direction[1], direction[2], 0.0]);
    let length = transformed[..3]
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if !length.is_finite() || length <= f64::EPSILON {
        return Err(RenderError::InvalidConfig(
            "camera view transform produced an invalid spot direction",
        ));
    }
    let result = std::array::from_fn(|axis| (f64::from(transformed[axis]) / length) as f32);
    if !result.into_iter().all(f32::is_finite) {
        return Err(RenderError::InvalidConfig(
            "camera view transform produced an invalid spot direction",
        ));
    }
    Ok(result)
}

pub(crate) fn multiply_4x4(left: [[f32; 4]; 4], right: [[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let mut result = [[0.0; 4]; 4];
    for column in 0..4 {
        for row in 0..4 {
            result[column][row] = (0..4)
                .map(|index| left[index][row] * right[column][index])
                .sum();
        }
    }
    result
}

pub(crate) fn float_bytes<const N: usize>(values: &[f32]) -> [u8; N] {
    let mut bytes = [0; N];
    for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(values) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

/// Матрица нормалей: присоединённая к блоку 3x3 модели со знаком определителя.
///
/// Она пропорциональна обратной транспонированной матрице, а длину нормали
/// восстанавливает нормализация в шейдере. Знак сохраняет внешние нормали
/// у зеркальных моделей.
pub(crate) fn normal_matrix(model: &[[f32; 4]; 4]) -> [[f32; 4]; 4] {
    let [a, b, c] = [0, 1, 2].map(|column| {
        let [x, y, z, _] = model[column];
        [x, y, z]
    });
    let cross = |u: [f32; 3], v: [f32; 3]| {
        [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ]
    };
    let columns = [cross(b, c), cross(c, a), cross(a, b)];
    let determinant: f32 = (0..3).map(|i| a[i] * columns[0][i]).sum();
    let sign = if determinant < 0.0 { -1.0 } else { 1.0 };
    let mut result = [[0.0; 4]; 4];
    for (target, column) in result.iter_mut().zip(columns) {
        *target = [column[0] * sign, column[1] * sign, column[2] * sign, 0.0];
    }
    result[3][3] = 1.0;
    result
}

#[cfg(test)]
pub(crate) fn derivative_tangent_basis(
    dp1: [f32; 3],
    dp2: [f32; 3],
    duv1: [f32; 2],
    duv2: [f32; 2],
    normal: [f32; 3],
) -> Option<([f32; 3], [f32; 3])> {
    let determinant = duv1[0] * duv2[1] - duv1[1] * duv2[0];
    if !determinant.is_finite()
        || determinant.abs() <= 1e-8
        || !dp1.into_iter().chain(dp2).all(f32::is_finite)
    {
        return None;
    }
    let normal = try_normalize3(normal).unwrap_or([0.0, 0.0, 1.0]);
    let mut tangent = scale3(
        sub3(scale3(dp1, duv2[1]), scale3(dp2, duv1[1])),
        determinant.recip(),
    );
    let mut bitangent = scale3(
        sub3(scale3(dp2, duv1[0]), scale3(dp1, duv2[0])),
        -determinant.recip(),
    );
    tangent = sub3(tangent, scale3(normal, dot3(tangent, normal)));
    bitangent = sub3(bitangent, scale3(normal, dot3(bitangent, normal)));
    Some((try_normalize3(tangent)?, try_normalize3(bitangent)?))
}

#[cfg(test)]
pub(crate) fn transform_tangent_normal(
    tangent: [f32; 3],
    bitangent: [f32; 3],
    normal: [f32; 3],
    tangent_normal: [f32; 3],
) -> [f32; 3] {
    let mapped = add3(
        add3(
            scale3(tangent, tangent_normal[0]),
            scale3(bitangent, tangent_normal[1]),
        ),
        scale3(normal, tangent_normal[2]),
    );
    try_normalize3(mapped).unwrap_or(normal)
}

/// CPU-зеркало выбора базиса в `mesh.frag::rve_perturb_normal` по вершинному tangent.
///
/// Возвращает `None`, когда шейдер уходит на производные экрана: sentinel `[0, 0, 0, 0]`,
/// нефинитный tangent, дробный `w` (`|w| <= 0.99`: шов или вырожденный экземпляр) или tangent,
/// вырожденный после проекции на нормаль.
#[cfg(test)]
pub(crate) fn vertex_tangent_basis(
    tangent: [f32; 4],
    normal: [f32; 3],
) -> Option<([f32; 3], [f32; 3])> {
    if !tangent.into_iter().all(f32::is_finite) || tangent[3].abs() <= 0.99 {
        return None;
    }
    let normal = try_normalize3(normal).unwrap_or([0.0, 0.0, 1.0]);
    let direction = [tangent[0], tangent[1], tangent[2]];
    let projected = sub3(direction, scale3(normal, dot3(direction, normal)));
    let length = dot3(projected, projected).sqrt();
    if !length.is_finite() || length <= 1e-6 {
        return None;
    }
    let tangent_axis = scale3(projected, length.recip());
    let bitangent = scale3(cross3(normal, tangent_axis), glsl_sign(tangent[3]));
    Some((tangent_axis, bitangent))
}

/// CPU-зеркало `mesh.vert`: `mat3(model) * t.xyz` и `w * sign(determinant(mat3(model)))`.
/// Столбцы `linear` — первые три столбца матрицы модели экземпляра.
#[cfg(test)]
pub(crate) fn transform_vertex_tangent(tangent: [f32; 4], linear: [[f32; 3]; 3]) -> [f32; 4] {
    let direction = mat3_mul_vector(linear, [tangent[0], tangent[1], tangent[2]]);
    [
        direction[0],
        direction[1],
        direction[2],
        tangent[3] * glsl_sign(determinant3(linear)),
    ]
}

/// CPU-зеркало `skin.comp` для tangent: `Σ wᵢ·mat3(Pᵢ)·t` нормализуется, знак `w` умножается
/// на `sign(det(Σ wᵢ·mat3(Pᵢ)))`. Влияния с весом `<= 0` пропускаются, как в шейдере.
/// При нулевой сумме весов tangent возвращается без изменений.
#[cfg(test)]
pub(crate) fn skinned_vertex_tangent(
    tangent: [f32; 4],
    influences: &[(f32, [[f32; 3]; 3])],
) -> [f32; 4] {
    let direction = [tangent[0], tangent[1], tangent[2]];
    let mut total_weight = 0.0;
    let mut skinned = [0.0; 3];
    let mut blended = [[0.0; 3]; 3];
    for &(weight, linear) in influences.iter().filter(|(weight, _)| *weight > 0.0) {
        skinned = add3(skinned, scale3(mat3_mul_vector(linear, direction), weight));
        for (target, column) in blended.iter_mut().zip(linear) {
            *target = add3(*target, scale3(column, weight));
        }
        total_weight += weight;
    }
    if total_weight <= 0.0 {
        return tangent;
    }
    let length = dot3(skinned, skinned).sqrt();
    if length > 0.0 {
        skinned = scale3(skinned, length.recip());
    }
    [
        skinned[0],
        skinned[1],
        skinned[2],
        tangent[3] * glsl_sign(determinant3(blended)),
    ]
}

#[cfg(test)]
pub(crate) fn mat3_mul_vector(linear: [[f32; 3]; 3], vector: [f32; 3]) -> [f32; 3] {
    add3(
        add3(scale3(linear[0], vector[0]), scale3(linear[1], vector[1])),
        scale3(linear[2], vector[2]),
    )
}

#[cfg(test)]
pub(crate) fn determinant3(linear: [[f32; 3]; 3]) -> f32 {
    dot3(linear[0], cross3(linear[1], linear[2]))
}

#[cfg(test)]
pub(crate) fn cross3(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

/// `sign` из GLSL: для нуля возвращает 0, в отличие от `f32::signum`.
#[cfg(test)]
pub(crate) fn glsl_sign(value: f32) -> f32 {
    if value > 0.0 {
        1.0
    } else if value < 0.0 {
        -1.0
    } else {
        0.0
    }
}

#[cfg(test)]
pub(crate) fn try_normalize3(value: [f32; 3]) -> Option<[f32; 3]> {
    let length_squared = dot3(value, value);
    if !length_squared.is_finite() || length_squared <= 1e-20 {
        None
    } else {
        Some(scale3(value, length_squared.sqrt().recip()))
    }
}

#[cfg(test)]
pub(crate) fn dot3(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

#[cfg(test)]
pub(crate) fn scale3(value: [f32; 3], scale: f32) -> [f32; 3] {
    [value[0] * scale, value[1] * scale, value[2] * scale]
}

#[cfg(test)]
pub(crate) fn sub3(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

#[cfg(test)]
pub(crate) fn add3(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

pub(crate) fn scaled_extent(output: vk::Extent2D, scale: f32) -> vk::Extent2D {
    let scale = f64::from(scale);
    vk::Extent2D {
        width: ((f64::from(output.width) * scale).floor() as u32).max(1),
        height: ((f64::from(output.height) * scale).floor() as u32).max(1),
    }
}

pub(crate) fn inverse_extent(extent: vk::Extent2D) -> [f32; 2] {
    [1.0 / extent.width as f32, 1.0 / extent.height as f32]
}
