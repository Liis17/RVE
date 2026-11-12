//! Проверка входных данных публичного API: сетки, материалы, текстуры, окружение и настройки.

use crate::*;

pub(crate) fn validate_mesh(vertex_count: usize, indices: &[u32]) -> Result<(), RenderError> {
    if vertex_count == 0 {
        return Err(RenderError::InvalidMesh("нет вершин"));
    }
    if indices.is_empty() || !indices.len().is_multiple_of(3) {
        return Err(RenderError::InvalidMesh(
            "число индексов должно быть положительным и кратным трём",
        ));
    }
    if u32::try_from(indices.len()).is_err() {
        return Err(RenderError::InvalidMesh("слишком много индексов"));
    }
    if indices.iter().any(|&index| index as usize >= vertex_count) {
        return Err(RenderError::InvalidMesh("индекс выходит за пределы вершин"));
    }
    Ok(())
}

pub(crate) fn validate_draws<M, T>(
    draws: &[DrawCommand],
    meshes: &[Option<M>],
    materials: &[Option<T>],
) -> Result<(), RenderError> {
    if draws
        .iter()
        .any(|draw| !meshes.get(draw.mesh.0).is_some_and(Option::is_some))
    {
        return Err(RenderError::InvalidMesh("MeshId не принадлежит renderer"));
    }
    for draw in draws {
        let material = draw
            .material
            .map_or(DEFAULT_MATERIAL, |material| material.0);
        if !materials.get(material).is_some_and(Option::is_some) {
            return Err(RenderError::InvalidMaterial(
                "MaterialId не принадлежит renderer".to_owned(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_material(
    material: &Material,
    texture_spaces: &[Option<TextureColorSpace>],
) -> Result<(), RenderError> {
    let valid_factor = |value: f32| value.is_finite() && (0.0..=1.0).contains(&value);
    if !material.base_color.into_iter().all(valid_factor)
        || !valid_factor(material.metallic)
        || !valid_factor(material.roughness)
        || !material.normal_scale.is_finite()
        || !valid_factor(material.opacity)
        || !valid_factor(material.occlusion_strength)
    {
        return Err(RenderError::InvalidMaterial(
            "факторы должны быть конечными; base_color/metallic/roughness/opacity/occlusion_strength — в [0, 1]".to_owned(),
        ));
    }
    if !material
        .emissive
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    {
        return Err(RenderError::InvalidMaterial(
            "emissive должен содержать конечные компоненты >= 0".to_owned(),
        ));
    }
    if material
        .user_values
        .iter()
        .flatten()
        .any(|value| !value.is_finite())
    {
        return Err(RenderError::InvalidMaterial(
            "пользовательские значения должны быть конечными".to_owned(),
        ));
    }
    if let AlphaMode::Mask { cutoff } = material.alpha_mode
        && !valid_factor(cutoff)
    {
        return Err(RenderError::InvalidMaterial(
            "alpha cutoff должен быть конечным и находиться в диапазоне [0, 1]".to_owned(),
        ));
    }
    let check_texture = |texture: Option<TextureId>, expected: TextureColorSpace, name: &str| {
        let Some(texture) = texture else {
            return Ok(());
        };
        let Some(Some(actual)) = texture_spaces.get(texture.0) else {
            return Err(RenderError::InvalidMaterial(format!(
                "TextureId для {name} не принадлежит renderer"
            )));
        };
        if *actual != expected {
            return Err(RenderError::InvalidMaterial(format!(
                "{name} должен иметь цветовое пространство {expected:?}"
            )));
        }
        Ok(())
    };
    check_texture(
        material.base_color_texture,
        TextureColorSpace::Srgb,
        "base_color_texture",
    )?;
    check_texture(
        material.metallic_roughness_texture,
        TextureColorSpace::Linear,
        "metallic_roughness_texture",
    )?;
    check_texture(
        material.normal_texture,
        TextureColorSpace::Linear,
        "normal_texture",
    )?;
    check_texture(
        material.emissive_texture,
        TextureColorSpace::Srgb,
        "emissive_texture",
    )?;
    check_texture(
        material.occlusion_texture,
        TextureColorSpace::Linear,
        "occlusion_texture",
    )
}

pub(crate) fn validate_surface_shader_source(
    source: &SurfaceShaderSource,
) -> Result<(), RenderError> {
    let mut numeric_slots = HashSet::new();
    let mut texture_slots = HashSet::new();
    let mut names = HashSet::new();
    for property in &source.properties {
        let mut chars = property.name.chars();
        let valid_identifier = chars
            .next()
            .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
            && chars.all(|character| character == '_' || character.is_ascii_alphanumeric());
        if !valid_identifier
            || property.name.starts_with("gl_")
            || property.name.starts_with("rve_")
            || !names.insert(property.name.as_str())
        {
            return Err(RenderError::InvalidShader(format!(
                "недопустимое или повторное имя свойства {}",
                property.name
            )));
        }
        match property.kind {
            SurfacePropertyKind::Texture => {
                if property.slot >= 8
                    || !texture_slots.insert(property.slot)
                    || property.color_space.is_none()
                {
                    return Err(RenderError::InvalidShader(format!(
                        "недопустимый слот или цветовое пространство текстуры {}",
                        property.name
                    )));
                }
            }
            _ => {
                if property.slot >= 16 || !numeric_slots.insert(property.slot) {
                    return Err(RenderError::InvalidShader(format!(
                        "недопустимый слот свойства {}",
                        property.name
                    )));
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_shader_material(
    material: &Material,
    texture_spaces: &[Option<TextureColorSpace>],
    shaders: &[Option<GpuSurfaceShader>],
) -> Result<(), RenderError> {
    let Some(shader_id) = material.shader else {
        if material.user_textures.iter().any(Option::is_some) {
            return Err(RenderError::InvalidMaterial(
                "user textures require a surface shader".to_owned(),
            ));
        }
        return Ok(());
    };
    let properties = shaders
        .get(shader_id.0)
        .and_then(Option::as_ref)
        .map(|shader| shader.properties.as_slice());
    validate_shader_material_properties(material, texture_spaces, properties)
}

pub(crate) fn validate_shader_material_properties(
    material: &Material,
    texture_spaces: &[Option<TextureColorSpace>],
    properties: Option<&[SurfaceProperty]>,
) -> Result<(), RenderError> {
    if material.shader.is_some() && properties.is_none() {
        return Err(RenderError::InvalidShader(
            "ShaderId не принадлежит renderer".to_owned(),
        ));
    }
    let properties = properties.unwrap_or_default();
    for (slot, texture) in material.user_textures.iter().enumerate() {
        let Some(texture) = texture else { continue };
        let property = properties
            .iter()
            .find(|property| property.kind == SurfacePropertyKind::Texture && property.slot == slot)
            .ok_or_else(|| {
                RenderError::InvalidMaterial(format!(
                    "texture slot {slot} is not declared by the surface shader"
                ))
            })?;
        let actual = texture_spaces
            .get(texture.0)
            .copied()
            .flatten()
            .ok_or_else(|| {
                RenderError::InvalidMaterial(format!(
                    "TextureId для {} не принадлежит renderer",
                    property.name
                ))
            })?;
        if property.color_space != Some(actual) {
            return Err(RenderError::InvalidMaterial(format!(
                "{} требует цветовое пространство {:?}, передано {:?}",
                property.name, property.color_space, actual
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_shader_destroyable(references: usize) -> Result<(), RenderError> {
    if references == 0 {
        Ok(())
    } else {
        Err(RenderError::InvalidShader(format!(
            "shader используется {references} материалами"
        )))
    }
}

pub(crate) fn validate_texture(
    width: u32,
    height: u32,
    byte_count: usize,
    max_size: u32,
) -> Result<(), RenderError> {
    if width == 0 || height == 0 {
        return Err(RenderError::InvalidTexture(
            "размеры должны быть больше нуля".to_owned(),
        ));
    }
    if width > max_size || height > max_size {
        return Err(RenderError::InvalidTexture(format!(
            "{width}x{height} больше предела GPU {max_size}x{max_size}"
        )));
    }
    let expected = texture_staging_size(width, height);
    if expected != Some(byte_count) {
        return Err(RenderError::InvalidTexture(format!(
            "для RGBA8 {width}x{height} нужно {} байт, передано {byte_count}",
            expected.map_or_else(|| "больше usize::MAX".to_owned(), |bytes| bytes.to_string())
        )));
    }
    Ok(())
}

pub(crate) fn texture_staging_size(width: u32, height: u32) -> Option<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|texels| texels.checked_mul(4))
}

pub(crate) fn validate_overlay(vertices: &[OverlayVertex]) -> Result<(), RenderError> {
    if !vertices.len().is_multiple_of(3) {
        return Err(RenderError::InvalidMesh(
            "число вершин оверлея должно быть кратным трём",
        ));
    }
    if u32::try_from(vertices.len()).is_err() {
        return Err(RenderError::InvalidMesh("слишком много вершин оверлея"));
    }
    Ok(())
}

/// Байты `FrameUniforms` в порядке std140: девять матриц и девять `vec4`.
/// Viewport рисуется в кадре один раз: его цель и uniform-буфер принадлежат одной сцене.
pub(crate) fn validate_scene_viewports(
    viewports: impl Iterator<Item = ViewportId>,
) -> Result<(), RenderError> {
    let mut seen = Vec::new();
    for viewport in viewports {
        if seen.contains(&viewport) {
            return Err(RenderError::InvalidTexture(
                "viewport указан в кадре несколько раз".to_owned(),
            ));
        }
        seen.push(viewport);
    }
    Ok(())
}

pub(crate) fn validate_frame_environment(environment: FrameEnvironment) -> Result<(), RenderError> {
    if !environment.intensity.is_finite() || environment.intensity < 0.0 {
        return Err(RenderError::InvalidConfig(
            "environment intensity must be finite and non-negative",
        ));
    }
    if !environment
        .rotation
        .iter()
        .flatten()
        .all(|value| value.is_finite())
    {
        return Err(RenderError::InvalidConfig(
            "environment rotation must be finite",
        ));
    }
    let columns = environment.rotation;
    let dot = |left: [f32; 3], right: [f32; 3]| {
        left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
    };
    if columns
        .iter()
        .any(|column| (dot(*column, *column) - 1.0).abs() > 1e-3)
        || (0..3)
            .any(|left| (left + 1..3).any(|right| dot(columns[left], columns[right]).abs() > 1e-3))
    {
        return Err(RenderError::InvalidConfig(
            "environment rotation must be an orthonormal basis",
        ));
    }
    let cross = [
        columns[0][1] * columns[1][2] - columns[0][2] * columns[1][1],
        columns[0][2] * columns[1][0] - columns[0][0] * columns[1][2],
        columns[0][0] * columns[1][1] - columns[0][1] * columns[1][0],
    ];
    if dot(cross, columns[2]) < 0.999 {
        return Err(RenderError::InvalidConfig(
            "environment rotation must preserve orientation",
        ));
    }
    Ok(())
}

pub(crate) fn validate_render_scale(scale: f32) -> Result<(), RenderError> {
    if scale.is_finite() && scale > 0.0 && scale <= 1.0 {
        Ok(())
    } else {
        Err(RenderError::InvalidConfig(
            "render_scale должен быть конечным и находиться в (0, 1]",
        ))
    }
}

pub(crate) fn validate_view_settings(view: ViewSettings) -> Result<(), RenderError> {
    validate_render_scale(view.render_scale)?;
    view.post.validate().map_err(RenderError::InvalidConfig)
}
