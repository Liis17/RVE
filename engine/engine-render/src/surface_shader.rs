//! Runtime compilation for user-authored surface shaders.

use std::fmt;

use super::TextureColorSpace;
use naga::back::spv;
use naga::front::glsl;
use naga::valid::{Capabilities, ValidationFlags, Validator};

const FRAGMENT_TEMPLATE: &str = include_str!("../shaders/mesh.frag");
const INSERTION_MARKER: &str = "// CUSTOM_SURFACE_INSERTION_POINT";

/// Lighting model used by the common mesh lighting pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceLighting {
    Lit,
    Unlit,
}

/// Data type of a user-declared surface property.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfacePropertyKind {
    Float,
    Vec2,
    Vec3,
    Vec4,
    Color,
    Texture,
}

/// Texture fallback used when a material leaves a shader texture property empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SurfaceTextureFallback {
    White,
    Black,
    Normal,
}

/// A property compiled into the renderer's fixed user-data bindings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceProperty {
    pub name: String,
    pub kind: SurfacePropertyKind,
    pub slot: usize,
    pub color_space: Option<TextureColorSpace>,
    pub fallback: SurfaceTextureFallback,
}

/// Renderer-independent shader input. Slots are assigned by `engine-scene`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SurfaceShaderSource {
    pub lighting: SurfaceLighting,
    pub properties: Vec<SurfaceProperty>,
    pub code: String,
}

/// Runtime SPIR-V variants. Opaque is also used for the post-prepass pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledSurfaceShader {
    pub opaque: Vec<u32>,
    pub mask: Vec<u32>,
    pub blend: Vec<u32>,
}

/// Shader compilation error, with a user-source line when Naga can locate it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShaderCompileError {
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for ShaderCompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(f, "строка {line}: {}", self.message)
        } else {
            f.write_str(&self.message)
        }
    }
}

impl std::error::Error for ShaderCompileError {}

/// Compiles a surface shader without a Vulkan device.
pub fn compile_surface_shader(
    source: &SurfaceShaderSource,
) -> Result<CompiledSurfaceShader, ShaderCompileError> {
    let (shader, user_start_line, user_end_line) = inject_surface_source(source)?;
    let lighting_define = match source.lighting {
        SurfaceLighting::Lit => None,
        SurfaceLighting::Unlit => Some(("UNLIT", "1")),
    };
    let opaque = compile_variant(
        &shader,
        user_start_line,
        user_end_line,
        lighting_define,
        None,
    )?;
    let mask = compile_variant(
        &shader,
        user_start_line,
        user_end_line,
        lighting_define,
        Some(("ALPHA_MASK", "1")),
    )?;
    let blend = compile_variant(
        &shader,
        user_start_line,
        user_end_line,
        lighting_define,
        Some(("ALPHA_BLEND", "1")),
    )?;
    Ok(CompiledSurfaceShader {
        opaque,
        mask,
        blend,
    })
}

fn inject_surface_source(
    source: &SurfaceShaderSource,
) -> Result<(String, usize, usize), ShaderCompileError> {
    let marker = FRAGMENT_TEMPLATE
        .find(INSERTION_MARKER)
        .ok_or_else(|| ShaderCompileError {
            line: None,
            message: "mesh.frag не содержит маркер custom surface".to_owned(),
        })?;
    let mut helpers = String::new();
    let mut defines = String::new();
    let mut undefs = String::new();
    for property in &source.properties {
        match property.kind {
            SurfacePropertyKind::Texture => {
                if property.slot >= 8 {
                    return Err(ShaderCompileError {
                        line: None,
                        message: format!("текстура {} имеет неверный слот", property.name),
                    });
                }
                helpers.push_str(&format!(
                    "vec4 sample_{}(vec2 uv) {{ return texture(sampler2D(rve_user_texture{}, material_sampler), uv, frame.render_params.z); }}\n",
                    property.name, property.slot
                ));
            }
            kind => {
                if property.slot >= 16 {
                    return Err(ShaderCompileError {
                        line: None,
                        message: format!("свойство {} имеет неверный слот", property.name),
                    });
                }
                let swizzle = match kind {
                    SurfacePropertyKind::Float => ".x",
                    SurfacePropertyKind::Vec2 => ".xy",
                    SurfacePropertyKind::Vec3 | SurfacePropertyKind::Color => ".rgb",
                    SurfacePropertyKind::Vec4 => "",
                    SurfacePropertyKind::Texture => unreachable!(),
                };
                defines.push_str(&format!(
                    "#define {} (rve_user.values[{}]{})\n",
                    property.name, property.slot, swizzle
                ));
            }
        }
        undefs.push_str(&format!("#undef {}\n", property.name));
    }
    let prefix = &FRAGMENT_TEMPLATE[..marker];
    let suffix = &FRAGMENT_TEMPLATE[marker + INSERTION_MARKER.len()..];
    let user_start_line = prefix.matches('\n').count()
        + helpers.matches('\n').count()
        + defines.matches('\n').count()
        + 1;
    let user_end_line = user_start_line + source.code.lines().count().saturating_sub(1);
    let mut generated = String::with_capacity(
        FRAGMENT_TEMPLATE.len() + helpers.len() + defines.len() + undefs.len() + source.code.len(),
    );
    generated.push_str(prefix);
    generated.push_str(&helpers);
    generated.push_str(&defines);
    generated.push_str(&source.code);
    if !source.code.ends_with('\n') {
        generated.push('\n');
    }
    generated.push_str(&undefs);
    generated.push_str(suffix);
    Ok((generated, user_start_line, user_end_line))
}

fn compile_variant(
    source: &str,
    user_start_line: usize,
    user_end_line: usize,
    lighting: Option<(&str, &str)>,
    alpha: Option<(&str, &str)>,
) -> Result<Vec<u32>, ShaderCompileError> {
    let mut options = glsl::Options::from(naga::ShaderStage::Fragment);
    options.defines.extend([
        ("CUSTOM_SURFACE".to_owned(), "1".to_owned()),
        (
            "MAX_CLUSTER_LIGHTS".to_owned(),
            super::MAX_CLUSTER_LIGHTS.to_string(),
        ),
        (
            "CLUSTER_GRID_X".to_owned(),
            super::CLUSTER_GRID_X.to_string(),
        ),
        (
            "CLUSTER_GRID_Y".to_owned(),
            super::CLUSTER_GRID_Y.to_string(),
        ),
        (
            "CLUSTER_GRID_Z".to_owned(),
            super::CLUSTER_GRID_Z.to_string(),
        ),
        (
            "MAX_LOCAL_SHADOW_LAYERS".to_owned(),
            super::MAX_LOCAL_SHADOW_LAYERS.to_string(),
        ),
        (
            "LOCAL_SHADOW_MAP_RESOLUTION".to_owned(),
            super::LOCAL_SHADOW_MAP_RESOLUTION.to_string(),
        ),
    ]);
    if let Some((name, value)) = lighting {
        options.defines.insert(name.to_owned(), value.to_owned());
    }
    if let Some((name, value)) = alpha {
        options.defines.insert(name.to_owned(), value.to_owned());
        options
            .defines
            .insert("NO_SCREEN_AO".to_owned(), "1".to_owned());
    }
    let mut frontend = glsl::Frontend::default();
    let module = frontend.parse(&options, source).map_err(|errors| {
        let first = errors.errors.first();
        let location = first.and_then(|error| error.location(source));
        let line = location.and_then(|location| {
            let line = location.line_number as usize;
            if (user_start_line..=user_end_line).contains(&line) {
                Some(line - user_start_line + 1)
            } else {
                None
            }
        });
        ShaderCompileError {
            line,
            message: errors.emit_to_string(source),
        }
    })?;
    let info = Validator::new(ValidationFlags::all(), Capabilities::IMMEDIATES)
        .validate(&module)
        .map_err(|error| ShaderCompileError {
            line: None,
            message: error.to_string(),
        })?;
    let options = spv::Options {
        flags: spv::Options::default().flags - spv::WriterFlags::ADJUST_COORDINATE_SPACE,
        ..spv::Options::default()
    };
    spv::write_vec(&module, &info, &options, None).map_err(|error| ShaderCompileError {
        line: None,
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(lighting: SurfaceLighting, code: &str) -> SurfaceShaderSource {
        SurfaceShaderSource {
            lighting,
            properties: vec![SurfaceProperty {
                name: "tint".to_owned(),
                kind: SurfacePropertyKind::Color,
                slot: 0,
                color_space: None,
                fallback: SurfaceTextureFallback::White,
            }],
            code: code.to_owned(),
        }
    }

    #[test]
    fn template_shader_compiles_all_lighting_and_alpha_variants() {
        let code = "void surface(inout Surface s) { s.albedo *= tint; s.opacity *= clamp(1.0 - s.position.y * 0.1, 0.0, 1.0); s.normal = rve_perturb_normal(vec3(0.0, 0.0, 1.0)); }";
        for lighting in [SurfaceLighting::Lit, SurfaceLighting::Unlit] {
            let variants = compile_surface_shader(&source(lighting, code)).unwrap();
            for words in [&variants.opaque, &variants.mask, &variants.blend] {
                assert_eq!(words.first(), Some(&0x0723_0203));
            }
        }
    }

    #[test]
    fn texture_accessors_and_name_colliding_with_template_compile() {
        let mut source = source(
            SurfaceLighting::Lit,
            "void surface(inout Surface s) { s.albedo *= base_color.rgb; s.albedo *= sample_albedo(s.uv).rgb; }",
        );
        source.properties = vec![
            SurfaceProperty {
                name: "base_color".to_owned(),
                kind: SurfacePropertyKind::Color,
                slot: 0,
                color_space: None,
                fallback: SurfaceTextureFallback::White,
            },
            SurfaceProperty {
                name: "albedo".to_owned(),
                kind: SurfacePropertyKind::Texture,
                slot: 0,
                color_space: Some(TextureColorSpace::Srgb),
                fallback: SurfaceTextureFallback::White,
            },
        ];
        compile_surface_shader(&source).unwrap();
    }

    #[test]
    fn syntax_error_reports_user_source_line() {
        let error = compile_surface_shader(&source(
            SurfaceLighting::Lit,
            "void surface(inout Surface s) {\n  s.albedo = vec3(;\n}",
        ))
        .unwrap_err();
        assert_eq!(error.line, Some(2));
    }
}
