//! Surface shader asset format shared by the editor, runtime, and project builder.

use engine_files::VirtualPath;
use serde::Deserialize;
use std::error::Error;
use std::fmt;

pub const MAX_NUMERIC_SHADER_PROPERTIES: usize = 16;
pub const MAX_TEXTURE_SHADER_PROPERTIES: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShaderLighting {
    #[default]
    Lit,
    Unlit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum ShaderPropertyKind {
    #[serde(rename = "float")]
    Float,
    #[serde(rename = "vec2")]
    Vec2,
    #[serde(rename = "vec3")]
    Vec3,
    #[serde(rename = "vec4")]
    Vec4,
    #[serde(rename = "color")]
    Color,
    #[serde(rename = "texture")]
    Texture,
}

impl ShaderPropertyKind {
    pub fn components(self) -> Option<usize> {
        match self {
            Self::Float => Some(1),
            Self::Vec2 => Some(2),
            Self::Vec3 | Self::Color => Some(3),
            Self::Vec4 => Some(4),
            Self::Texture => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShaderColorSpace {
    #[default]
    Srgb,
    Linear,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextureFallback {
    #[default]
    White,
    Black,
    Normal,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ShaderPropertyDefault {
    Numeric(Vec<f32>),
    Texture(Option<VirtualPath>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShaderProperty {
    pub name: String,
    pub label: String,
    pub kind: ShaderPropertyKind,
    pub default: ShaderPropertyDefault,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub color_space: Option<ShaderColorSpace>,
    pub fallback: Option<TextureFallback>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShaderPropertyLayout {
    pub property: ShaderProperty,
    /// Index into numeric vec4 uniforms or user texture descriptors, by property kind.
    pub slot: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShaderSchema {
    pub lighting: ShaderLighting,
    /// True only for the engine-provided Lit schema, whose values map to Material fields.
    pub builtin_lit: bool,
    pub properties: Vec<ShaderPropertyLayout>,
}

impl ShaderSchema {
    /// Built-in Lit schema. Values intentionally mirror engine_render::Material::default.
    pub fn lit() -> Self {
        let mut properties = Vec::new();
        let numeric = [
            ("base_color", ShaderPropertyKind::Color, vec![1.0, 1.0, 1.0]),
            ("metallic", ShaderPropertyKind::Float, vec![0.0]),
            ("roughness", ShaderPropertyKind::Float, vec![1.0]),
            ("emissive", ShaderPropertyKind::Color, vec![0.0, 0.0, 0.0]),
            ("opacity", ShaderPropertyKind::Float, vec![1.0]),
            ("normal_scale", ShaderPropertyKind::Float, vec![1.0]),
            ("occlusion_strength", ShaderPropertyKind::Float, vec![1.0]),
        ];
        for (slot, (name, kind, values)) in numeric.into_iter().enumerate() {
            properties.push(ShaderPropertyLayout {
                property: ShaderProperty {
                    name: name.to_owned(),
                    label: name.replace('_', " "),
                    kind,
                    default: ShaderPropertyDefault::Numeric(values),
                    min: None,
                    max: None,
                    color_space: None,
                    fallback: None,
                },
                slot,
            });
        }
        for (slot, (name, color_space, fallback)) in [
            (
                "base_color_texture",
                ShaderColorSpace::Srgb,
                TextureFallback::White,
            ),
            (
                "metallic_roughness_texture",
                ShaderColorSpace::Linear,
                TextureFallback::White,
            ),
            (
                "normal_texture",
                ShaderColorSpace::Linear,
                TextureFallback::Normal,
            ),
            (
                "emissive_texture",
                ShaderColorSpace::Srgb,
                TextureFallback::Black,
            ),
            (
                "occlusion_texture",
                ShaderColorSpace::Linear,
                TextureFallback::White,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            properties.push(ShaderPropertyLayout {
                property: ShaderProperty {
                    name: name.to_owned(),
                    label: name.replace('_', " "),
                    kind: ShaderPropertyKind::Texture,
                    default: ShaderPropertyDefault::Texture(None),
                    min: None,
                    max: None,
                    color_space: Some(color_space),
                    fallback: Some(fallback),
                },
                slot,
            });
        }
        Self {
            lighting: ShaderLighting::Lit,
            builtin_lit: true,
            properties,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShaderAsset {
    pub path: String,
    pub lighting: ShaderLighting,
    pub properties: Vec<ShaderProperty>,
    pub code: String,
    /// One-based source line at which the GLSL section starts.
    pub code_first_line: usize,
}

impl ShaderAsset {
    pub fn parse(path: impl Into<String>, source: &str) -> Result<Self, ShaderAssetError> {
        let path = path.into();
        let Some((header, code, code_first_line)) = split_shader_source(source) else {
            return Err(ShaderAssetError::new(
                &path,
                Some(1),
                "ожидалась отдельная строка ---",
            ));
        };
        let raw: RawShaderAsset = toml::from_str(header).map_err(|error| {
            ShaderAssetError::new(&path, toml_error_line(header, &error), error.to_string())
        })?;
        let mut properties = Vec::with_capacity(raw.properties.len());
        let mut names = std::collections::BTreeSet::new();
        let mut numeric_count = 0;
        let mut texture_count = 0;
        for (index, raw_property) in raw.properties.into_iter().enumerate() {
            let line = property_line(header, index);
            let invalid = |message: String| ShaderAssetError::new(&path, line, message);
            let name = raw_property.name.clone();
            let label = raw_property
                .label
                .clone()
                .unwrap_or_else(|| name.replace('_', " "));
            if !valid_identifier(&name) {
                return Err(invalid(format!("некорректный GLSL-идентификатор {name:?}")));
            }
            if name.starts_with("gl_") || name.starts_with("rve_") {
                return Err(invalid(format!(
                    "имя {name:?} использует зарезервированный префикс"
                )));
            }
            if is_reserved_identifier(&name) {
                return Err(invalid(format!(
                    "имя {name:?} зарезервировано GLSL или surface API"
                )));
            }
            if !names.insert(name.clone()) {
                return Err(invalid(format!("имя свойства {name:?} повторяется")));
            }
            let validated = validate_property(raw_property, &path, line)?;
            // Числовые свойства становятся макросами, поэтому не могут совпадать с именами,
            // которые пользовательский код видит рядом с Surface и шаблоном. У текстур макроса нет.
            if validated.kind != ShaderPropertyKind::Texture && is_macro_conflict(&name) {
                return Err(invalid(format!(
                    "имя числового свойства {name:?} совпадает с полем Surface, функцией или переменной шаблона; выберите другое"
                )));
            }
            if validated.color_space.is_some()
                != matches!(validated.default, ShaderPropertyDefault::Texture(_))
            {
                return Err(invalid(
                    "color_space/fallback допустимы только для texture".to_owned(),
                ));
            }
            if validated.kind == ShaderPropertyKind::Texture {
                texture_count += 1;
                if texture_count > MAX_TEXTURE_SHADER_PROPERTIES {
                    return Err(invalid(format!(
                        "допускается не больше {MAX_TEXTURE_SHADER_PROPERTIES} текстурных свойств"
                    )));
                }
            } else {
                numeric_count += 1;
                if numeric_count > MAX_NUMERIC_SHADER_PROPERTIES {
                    return Err(invalid(format!(
                        "допускается не больше {MAX_NUMERIC_SHADER_PROPERTIES} числовых свойств"
                    )));
                }
            }
            properties.push(ShaderProperty {
                name,
                label,
                kind: validated.kind,
                default: validated.default,
                min: validated.min,
                max: validated.max,
                color_space: validated.color_space,
                fallback: validated.fallback,
            });
        }
        Ok(Self {
            path,
            lighting: raw.shader.lighting,
            properties,
            code: code.to_owned(),
            code_first_line,
        })
    }

    /// Assigns independent numeric and texture slots in declaration order.
    pub fn layout(&self) -> ShaderSchema {
        let mut numeric_slot = 0;
        let mut texture_slot = 0;
        let properties = self
            .properties
            .iter()
            .cloned()
            .map(|property| {
                let slot = if property.kind == ShaderPropertyKind::Texture {
                    let slot = texture_slot;
                    texture_slot += 1;
                    slot
                } else {
                    let slot = numeric_slot;
                    numeric_slot += 1;
                    slot
                };
                ShaderPropertyLayout { property, slot }
            })
            .collect();
        ShaderSchema {
            lighting: self.lighting,
            builtin_lit: false,
            properties,
        }
    }

    /// Starter Lit shader source used by the editor's Create menu.
    pub fn template(name: &str) -> String {
        let name = name.replace(['\r', '\n'], " ");
        format!(
            "# {name}\n\
             [shader]\n\
             lighting = \"lit\"\n\
             \n\
             [[properties]]\n\
             name = \"tint\"\n\
             label = \"Tint\"\n\
             type = \"color\"\n\
             default = [1.0, 1.0, 1.0]\n\
             \n\
             [[properties]]\n\
             name = \"albedo\"\n\
             label = \"Albedo\"\n\
             type = \"texture\"\n\
             color_space = \"srgb\"\n\
             fallback = \"white\"\n\
             \n---\n\
             // Surface fields: albedo, emissive, metallic, roughness, opacity, normal, occlusion, uv, position.\n\
             void surface(inout Surface s) {{\n\
                 s.albedo = sample_albedo(s.uv).rgb * tint;\n\
             }}\n",
        )
    }
}

fn split_shader_source(source: &str) -> Option<(&str, &str, usize)> {
    let mut offset = 0;
    for (index, line) in source.split_inclusive('\n').enumerate() {
        let text = line.trim_end_matches(['\r', '\n']);
        if text == "---" {
            let code_start = offset + line.len();
            return Some((&source[..offset], &source[code_start..], index + 2));
        }
        offset += line.len();
    }
    None
}

fn toml_error_line(source: &str, error: &toml::de::Error) -> Option<usize> {
    error.span().map(|span| line_for_offset(source, span.start))
}

fn line_for_offset(source: &str, offset: usize) -> usize {
    source.as_bytes()[..offset.min(source.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn property_line(source: &str, property_index: usize) -> Option<usize> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| line.trim() == "[[properties]]")
        .nth(property_index)
        .map(|(line, _)| line + 1)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShaderAsset {
    shader: RawShaderHeader,
    #[serde(default)]
    properties: Vec<RawShaderProperty>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShaderHeader {
    #[serde(default)]
    lighting: ShaderLighting,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawShaderProperty {
    name: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(rename = "type")]
    kind: ShaderPropertyKind,
    #[serde(default)]
    default: Option<toml::Value>,
    #[serde(default)]
    min: Option<f64>,
    #[serde(default)]
    max: Option<f64>,
    #[serde(default)]
    color_space: Option<ShaderColorSpace>,
    #[serde(default)]
    fallback: Option<TextureFallback>,
}

struct ValidatedShaderProperty {
    kind: ShaderPropertyKind,
    default: ShaderPropertyDefault,
    min: Option<f32>,
    max: Option<f32>,
    color_space: Option<ShaderColorSpace>,
    fallback: Option<TextureFallback>,
}

fn validate_property(
    raw: RawShaderProperty,
    path: &str,
    line: Option<usize>,
) -> Result<ValidatedShaderProperty, ShaderAssetError> {
    let invalid = |message: String| ShaderAssetError::new(path, line, message);
    let RawShaderProperty {
        kind,
        default: raw_default,
        min: raw_min,
        max: raw_max,
        color_space: raw_color_space,
        fallback: raw_fallback,
        ..
    } = raw;
    let min = raw_min
        .map(as_f32)
        .transpose()
        .map_err(|_| invalid("min должен быть конечным f32".to_owned()))?;
    let max = raw_max
        .map(as_f32)
        .transpose()
        .map_err(|_| invalid("max должен быть конечным f32".to_owned()))?;
    if min.zip(max).is_some_and(|(min, max)| min > max) {
        return Err(invalid("min должен быть меньше или равен max".to_owned()));
    }
    if kind != ShaderPropertyKind::Float && (raw_min.is_some() || raw_max.is_some()) {
        return Err(invalid("min и max допустимы только для float".to_owned()));
    }
    let (default, color_space, fallback) = match kind {
        ShaderPropertyKind::Texture => {
            let default = match raw_default {
                None => None,
                Some(toml::Value::String(path)) => {
                    Some(VirtualPath::parse(&path).map_err(|error| invalid(error.to_string()))?)
                }
                Some(_) => {
                    return Err(invalid(
                        "default texture должен быть virtual path".to_owned(),
                    ));
                }
            };
            (
                ShaderPropertyDefault::Texture(default),
                Some(raw_color_space.unwrap_or_default()),
                Some(raw_fallback.unwrap_or_default()),
            )
        }
        kind => {
            if raw_color_space.is_some() || raw_fallback.is_some() {
                return Err(invalid(
                    "color_space/fallback допустимы только для texture".to_owned(),
                ));
            }
            let Some(default) = raw_default else {
                return Err(invalid(
                    "default обязателен для числового свойства".to_owned(),
                ));
            };
            let components = kind.components().expect("numeric property kind");
            let values = if components == 1 {
                vec![
                    toml_number(&default)
                        .ok_or_else(|| invalid("default должен быть числом".to_owned()))?,
                ]
            } else {
                let Some(values) = default.as_array() else {
                    return Err(invalid(format!(
                        "default должен содержать {components} числа"
                    )));
                };
                if values.len() != components {
                    return Err(invalid(format!(
                        "default должен содержать {components} числа"
                    )));
                }
                values
                    .iter()
                    .map(|value| {
                        toml_number(value).ok_or_else(|| {
                            invalid("default должен содержать только числа".to_owned())
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            };
            if values.iter().any(|value| !value.is_finite()) {
                return Err(invalid("default должен содержать конечные f32".to_owned()));
            }
            (ShaderPropertyDefault::Numeric(values), None, None)
        }
    };
    Ok(ValidatedShaderProperty {
        kind,
        default,
        min,
        max,
        color_space,
        fallback,
    })
}

fn as_f32(value: f64) -> Result<f32, ()> {
    let value = value as f32;
    value.is_finite().then_some(value).ok_or(())
}

fn toml_number(value: &toml::Value) -> Option<f32> {
    let value = match value {
        toml::Value::Integer(value) => *value as f64,
        toml::Value::Float(value) => *value,
        _ => return None,
    };
    as_f32(value).ok()
}

// The type is looked up separately to keep RawShaderProperty movable while validating defaults.
fn valid_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'A'..=b'Z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Имена, которые числовое свойство не может получить: оно превращается в `#define` в renderer,
/// и макрос подменил бы поле `s.opacity`, вызов `max(a, b)` или переменную шаблона `frame`.
/// Список повторяет `engine-render/shaders/mesh.frag`; префикс `sample_` занят хелперами текстур.
fn is_macro_conflict(name: &str) -> bool {
    const CONFLICTS: &[&str] = &[
        // Поля `struct Surface`.
        "uv",
        "position",
        "albedo",
        "opacity",
        "geometric_normal",
        "normal",
        "metallic",
        "roughness",
        "occlusion",
        "emissive",
        // Глобальные имена шаблона, видимые пользовательскому коду.
        "PI",
        "frame",
        "push_constants",
        "material_sampler",
        "shadow_map",
        "shadow_sampler",
        "local_shadow_map",
        "ao_image",
        "environment_map",
        "environment_sampler",
        "irradiance_map",
        "prefiltered_map",
        "brdf_lut",
        "brdf_sampler",
        "base_color_texture",
        "metallic_roughness_texture",
        "normal_texture",
        "emissive_texture",
        "occlusion_texture",
        "safe_normalize",
        "distribution_ggx",
        "visibility_smith_ggx_correlated",
        "fresnel_schlick",
        "direct_brdf",
        "range_attenuation",
        "spot_attenuation",
        "local_shadow_visibility",
        "lod_color",
        "cascade_color",
        "LocalLightData",
        "ClusterRecord",
        // Встроенные функции GLSL, которые пользователь вызывает в surface().
        "min",
        "max",
        "clamp",
        "mix",
        "step",
        "smoothstep",
        "pow",
        "exp",
        "exp2",
        "log",
        "log2",
        "sqrt",
        "inversesqrt",
        "abs",
        "sign",
        "floor",
        "ceil",
        "fract",
        "mod",
        "round",
        "trunc",
        "dot",
        "cross",
        "length",
        "distance",
        "normalize",
        "reflect",
        "refract",
        "texture",
        "textureLod",
        "texelFetch",
        "sin",
        "cos",
        "tan",
        "any",
        "all",
        "not",
        "transpose",
        "inverse",
        "determinant",
    ];
    name.starts_with("sample_") || CONFLICTS.contains(&name)
}

fn is_reserved_identifier(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "attribute",
        "const",
        "uniform",
        "varying",
        "buffer",
        "shared",
        "coherent",
        "volatile",
        "restrict",
        "readonly",
        "writeonly",
        "atomic_uint",
        "layout",
        "centroid",
        "flat",
        "smooth",
        "noperspective",
        "patch",
        "sample",
        "in",
        "out",
        "inout",
        "bool",
        "int",
        "uint",
        "float",
        "double",
        "bvec2",
        "bvec3",
        "bvec4",
        "ivec2",
        "ivec3",
        "ivec4",
        "uvec2",
        "uvec3",
        "uvec4",
        "dvec2",
        "dvec3",
        "dvec4",
        "vec2",
        "vec3",
        "vec4",
        "mat2",
        "mat3",
        "mat4",
        "mat2x2",
        "mat2x3",
        "mat2x4",
        "mat3x2",
        "mat3x3",
        "mat3x4",
        "mat4x2",
        "mat4x3",
        "mat4x4",
        "sampler1D",
        "sampler2D",
        "sampler3D",
        "samplerCube",
        "sampler2DRect",
        "samplerBuffer",
        "sampler2DMS",
        "isampler2D",
        "usampler2D",
        "image2D",
        "iimage2D",
        "uimage2D",
        "struct",
        "void",
        "while",
        "for",
        "do",
        "if",
        "else",
        "switch",
        "case",
        "default",
        "break",
        "continue",
        "return",
        "discard",
        "precise",
        "invariant",
        "true",
        "false",
        "precision",
        "highp",
        "mediump",
        "lowp",
        "subroutine",
        "common",
        "partition",
        "active",
        "main",
        "surface",
        "Surface",
        "rve_user",
        "rve_default_surface",
        "rve_perturb_normal",
        "sample_albedo",
        "sample_user_texture",
    ];
    RESERVED.contains(&name)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShaderAssetError {
    pub path: String,
    pub line: Option<usize>,
    pub message: String,
}

impl ShaderAssetError {
    fn new(path: &str, line: Option<usize>, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for ShaderAssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(f, "{}:{line}: {}", self.path, self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl Error for ShaderAssetError {}

#[cfg(test)]
mod tests {
    use super::*;

    const SHADER: &str = "[shader]\nlighting=\"lit\"\n[[properties]]\nname=\"tint\"\ntype=\"color\"\ndefault=[1.0,0.5,0.2]\n[[properties]]\nname=\"albedo\"\ntype=\"texture\"\ncolor_space=\"srgb\"\nfallback=\"white\"\n---\nvoid surface(inout Surface s) { s.albedo = sample_albedo(s.uv).rgb * tint; }\n";

    #[test]
    fn parses_header_and_assigns_declaration_order_slots() {
        let shader = ShaderAsset::parse("/shaders/test.rveshader", SHADER).unwrap();
        let layout = shader.layout();
        assert_eq!(shader.code_first_line, 13);
        assert_eq!(layout.properties[0].property.name, "tint");
        assert_eq!(layout.properties[0].slot, 0);
        assert_eq!(layout.properties[1].property.name, "albedo");
        assert_eq!(layout.properties[1].slot, 0);
    }

    #[test]
    fn parses_every_numeric_kind_and_float_ranges() {
        let source = "[shader]\n[[properties]]\nname=\"a\"\ntype=\"float\"\ndefault=0.5\nmin=0.0\nmax=1.0\n[[properties]]\nname=\"b\"\ntype=\"vec2\"\ndefault=[1,2]\n[[properties]]\nname=\"c\"\ntype=\"vec3\"\ndefault=[1,2,3]\n[[properties]]\nname=\"d\"\ntype=\"vec4\"\ndefault=[1,2,3,4]\n[[properties]]\nname=\"e\"\ntype=\"color\"\ndefault=[0.1,0.2,0.3]\n---\n";
        let shader = ShaderAsset::parse("types.rveshader", source).unwrap();
        assert_eq!(
            shader
                .properties
                .iter()
                .map(|property| property.kind)
                .collect::<Vec<_>>(),
            [
                ShaderPropertyKind::Float,
                ShaderPropertyKind::Vec2,
                ShaderPropertyKind::Vec3,
                ShaderPropertyKind::Vec4,
                ShaderPropertyKind::Color,
            ]
        );
        assert_eq!(shader.properties[0].min, Some(0.0));
        assert_eq!(shader.properties[0].max, Some(1.0));
        assert_eq!(shader.layout().properties[4].slot, 4);
    }

    #[test]
    fn rejects_invalid_defaults_ranges_duplicates_reserved_names_and_limits() {
        for source in [
            "[shader]\n[[properties]]\nname=\"x\"\ntype=\"vec3\"\ndefault=[1,2]\n---\n",
            "[shader]\n[[properties]]\nname=\"x\"\ntype=\"float\"\ndefault=1\nmin=2\nmax=1\n---\n",
            "[shader]\n[[properties]]\nname=\"x\"\ntype=\"float\"\ndefault=1\n[[properties]]\nname=\"x\"\ntype=\"float\"\ndefault=1\n---\n",
            "[shader]\n[[properties]]\nname=\"gl_bad\"\ntype=\"float\"\ndefault=1\n---\n",
        ] {
            assert!(
                ShaderAsset::parse("bad.rveshader", source).is_err(),
                "{source}"
            );
        }
        let numeric = (0..17)
            .map(|index| format!("[[properties]]\nname=\"p{index}\"\ntype=\"float\"\ndefault=0\n"))
            .collect::<String>();
        assert!(
            ShaderAsset::parse("many.rveshader", &format!("[shader]\n{numeric}---\n")).is_err()
        );
        let textures = (0..9)
            .map(|index| format!("[[properties]]\nname=\"t{index}\"\ntype=\"texture\"\n"))
            .collect::<String>();
        assert!(
            ShaderAsset::parse("many.rveshader", &format!("[shader]\n{textures}---\n")).is_err()
        );
    }

    #[test]
    fn denies_unknown_fields_and_reports_shader_code_line() {
        let unknown = "[shader]\nlighting=\"lit\"\nwat=true\n---\n";
        let error = ShaderAsset::parse("bad.rveshader", unknown).unwrap_err();
        assert!(error.to_string().contains("bad.rveshader:3"));
        assert!(ShaderAsset::parse("bad.rveshader", "[shader]\n---\n").is_ok());
        let unknown_property =
            "[shader]\n[[properties]]\nname=\"x\"\ntype=\"float\"\ndefault=1\nextra=true\n---\n";
        assert!(ShaderAsset::parse("bad.rveshader", unknown_property).is_err());
        let no_separator = ShaderAsset::parse("bad.rveshader", "[shader]\n").unwrap_err();
        assert!(no_separator.to_string().contains("bad.rveshader:1"));
    }

    #[test]
    fn template_has_valid_format_and_texture_may_be_named_like_surface_field() {
        let template = ShaderAsset::template("example");
        assert!(ShaderAsset::parse("example.rveshader", &template).is_ok());
        // Текстура `albedo` генерирует только хелпер `sample_albedo`, макроса нет.
        let source = "[shader]\n[[properties]]\nname=\"albedo\"\ntype=\"texture\"\n---\nvoid surface(inout Surface s) { s.albedo = sample_albedo(s.uv).rgb; }\n";
        assert!(ShaderAsset::parse("field.rveshader", source).is_ok());
    }

    #[test]
    fn numeric_property_named_like_surface_field_or_template_symbol_is_rejected() {
        let numeric = |name: &str, kind: &str| {
            format!(
                "[shader]\n[[properties]]\nname=\"{name}\"\ntype=\"{kind}\"\ndefault={}\n---\n",
                if kind == "float" { "1" } else { "[1,1,1]" }
            )
        };
        for name in [
            "opacity", "albedo", "uv", "position", "normal", "max", "sample_x",
        ] {
            let error =
                ShaderAsset::parse("bad.rveshader", &numeric(name, "float")).expect_err(name);
            assert!(error.message.contains("совпадает"), "{error}");
            assert_eq!(error.line, Some(2), "{error}");
        }
        assert!(ShaderAsset::parse("ok.rveshader", &numeric("tint", "color")).is_ok());
        assert!(ShaderAsset::parse("ok.rveshader", &numeric("roughness_scale", "float")).is_ok());
    }
}
