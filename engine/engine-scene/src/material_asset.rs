//! Material asset format and schema-based value resolution for .rvemat files.

use crate::document::{MaterialAlphaMode, option_virtual_path_serde};
use crate::shader_asset::{
    ShaderColorSpace, ShaderPropertyDefault, ShaderPropertyKind, ShaderSchema,
};
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialAsset {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_virtual_path_serde"
    )]
    pub shader: Option<VirtualPath>,
    #[serde(default = "default_alpha_mode", skip_serializing_if = "is_opaque")]
    pub alpha_mode: MaterialAlphaMode,
    #[serde(
        default = "default_alpha_cutoff",
        skip_serializing_if = "is_default_cutoff"
    )]
    pub alpha_cutoff: f32,
    #[serde(default, skip_serializing_if = "is_false")]
    pub double_sided: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<String, toml::Value>,
}

impl Default for MaterialAsset {
    fn default() -> Self {
        Self {
            shader: None,
            alpha_mode: MaterialAlphaMode::Opaque,
            alpha_cutoff: 0.5,
            double_sided: false,
            properties: BTreeMap::new(),
        }
    }
}

impl MaterialAsset {
    pub fn parse(path: impl Into<String>, source: &str) -> Result<Self, MaterialAssetError> {
        let path = path.into();
        let material: Self = toml::from_str(source).map_err(|error| {
            MaterialAssetError::new(
                &path,
                error.span().map(|span| line_for_offset(source, span.start)),
                error.to_string(),
            )
        })?;
        material.validate(&path, source)?;
        Ok(material)
    }

    pub fn to_toml(&self) -> Result<String, MaterialAssetError> {
        self.validate("material", "")?;
        toml::to_string_pretty(self)
            .map_err(|error| MaterialAssetError::new("material", None, error.to_string()))
    }

    fn validate(&self, path: &str, source: &str) -> Result<(), MaterialAssetError> {
        let invalid = |key: &str, message: &str| {
            MaterialAssetError::new(
                path,
                property_line(source, key),
                format!("properties.{key}: {message}"),
            )
        };
        if !self.alpha_cutoff.is_finite() || !(0.0..=1.0).contains(&self.alpha_cutoff) {
            return Err(MaterialAssetError::new(
                path,
                field_line(source, "alpha_cutoff"),
                "alpha_cutoff должен быть конечным и находиться в диапазоне [0, 1]",
            ));
        }
        for (key, value) in &self.properties {
            match value {
                toml::Value::Integer(_) | toml::Value::Float(_) => {
                    if !numeric(value).is_some_and(f32::is_finite) {
                        return Err(invalid(key, "число должно быть конечным f32"));
                    }
                }
                toml::Value::Array(values) => {
                    if !(2..=4).contains(&values.len())
                        || values
                            .iter()
                            .any(|value| !numeric(value).is_some_and(f32::is_finite))
                    {
                        return Err(invalid(
                            key,
                            "массив должен содержать от 2 до 4 конечных чисел",
                        ));
                    }
                }
                toml::Value::String(path_value) => {
                    if VirtualPath::parse(path_value).is_err() {
                        return Err(invalid(key, "строковое значение должно быть virtual path"));
                    }
                }
                _ => {
                    return Err(invalid(
                        key,
                        "допустимы только число, массив чисел или virtual path",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Returns all shader and texture paths referenced by this material, without duplicates.
    pub fn dependencies(&self) -> Vec<VirtualPath> {
        let mut paths = BTreeSet::new();
        if let Some(shader) = &self.shader {
            paths.insert(shader.clone());
        }
        for value in self.properties.values() {
            if let toml::Value::String(path) = value
                && let Ok(path) = VirtualPath::parse(path)
            {
                paths.insert(path);
            }
        }
        paths.into_iter().collect()
    }

    /// Resolves known properties against defaults; incompatible/missing values are warned about.
    /// Unknown keys remain in the source document but do not affect the resolved result.
    pub fn resolve(&self, schema: &ShaderSchema) -> ResolvedMaterialAsset {
        let mut uniforms = [[0.0; 4]; 16];
        let mut textures: [Option<(VirtualPath, ShaderColorSpace)>; 8] =
            std::array::from_fn(|_| None);
        let mut warnings = Vec::new();
        let mut lit = schema.builtin_lit.then_some(LitValues::default());
        let mut used = BTreeSet::new();

        for layout in &schema.properties {
            let property = &layout.property;
            // Отсутствующее свойство тихо берёт default: предупреждения только о несовпадении типа.
            let supplied = self.properties.get(&property.name);
            if supplied.is_some() {
                used.insert(property.name.as_str());
            }
            match property.kind {
                ShaderPropertyKind::Texture => {
                    let (default, color_space) = match &property.default {
                        ShaderPropertyDefault::Texture(default) => {
                            (default.clone(), property.color_space.unwrap_or_default())
                        }
                        ShaderPropertyDefault::Numeric(_) => {
                            (None, property.color_space.unwrap_or_default())
                        }
                    };
                    let supplied_path = supplied.and_then(|value| match value {
                        toml::Value::String(path) => VirtualPath::parse(path).ok(),
                        _ => None,
                    });
                    if supplied.is_some() && supplied_path.is_none() {
                        warnings.push(format!(
                            "properties.{} имеет неверный тип; используется default",
                            property.name
                        ));
                    }
                    let resolved = supplied_path.or(default);
                    if let Some(slot) = textures.get_mut(layout.slot) {
                        *slot = resolved.map(|path| (path, color_space));
                    }
                }
                kind => {
                    let defaults = match &property.default {
                        ShaderPropertyDefault::Numeric(values) => values.as_slice(),
                        ShaderPropertyDefault::Texture(_) => &[],
                    };
                    let mut values = supplied.and_then(|value| numeric_values(value, kind));
                    if values.is_none() {
                        if supplied.is_some() {
                            warnings.push(format!(
                                "properties.{} имеет неверный тип; используется default",
                                property.name
                            ));
                        }
                        values = Some(defaults.to_vec());
                    }
                    if let Some(slot) = uniforms.get_mut(layout.slot)
                        && let Some(values) = values
                    {
                        slot[..values.len()].copy_from_slice(&values);
                        if let Some(lit) = &mut lit {
                            lit.set(&property.name, &values);
                        }
                    }
                }
            }
        }
        for key in self.properties.keys() {
            if !used.contains(key.as_str()) {
                warnings.push(format!("properties.{key} не используется выбранной схемой"));
            }
        }

        ResolvedMaterialAsset {
            uniforms,
            textures,
            lit,
            alpha: ResolvedAlpha {
                mode: self.alpha_mode,
                cutoff: self.alpha_cutoff,
            },
            double_sided: self.double_sided,
            warnings,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedAlpha {
    pub mode: MaterialAlphaMode,
    pub cutoff: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LitValues {
    pub base_color: [f32; 3],
    pub metallic: f32,
    pub roughness: f32,
    pub emissive: [f32; 3],
    pub opacity: f32,
    pub normal_scale: f32,
    pub occlusion_strength: f32,
}

impl Default for LitValues {
    fn default() -> Self {
        Self {
            base_color: [1.0; 3],
            metallic: 0.0,
            roughness: 1.0,
            emissive: [0.0; 3],
            opacity: 1.0,
            normal_scale: 1.0,
            occlusion_strength: 1.0,
        }
    }
}

impl LitValues {
    fn set(&mut self, name: &str, values: &[f32]) {
        match name {
            "base_color" if values.len() == 3 => self.base_color.copy_from_slice(values),
            "metallic" if values.len() == 1 => self.metallic = values[0],
            "roughness" if values.len() == 1 => self.roughness = values[0],
            "emissive" if values.len() == 3 => self.emissive.copy_from_slice(values),
            "opacity" if values.len() == 1 => self.opacity = values[0],
            "normal_scale" if values.len() == 1 => self.normal_scale = values[0],
            "occlusion_strength" if values.len() == 1 => self.occlusion_strength = values[0],
            _ => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedMaterialAsset {
    pub uniforms: [[f32; 4]; 16],
    pub textures: [Option<(VirtualPath, ShaderColorSpace)>; 8],
    pub lit: Option<LitValues>,
    pub alpha: ResolvedAlpha,
    pub double_sided: bool,
    pub warnings: Vec<String>,
}

fn default_alpha_mode() -> MaterialAlphaMode {
    MaterialAlphaMode::Opaque
}

fn is_opaque(mode: &MaterialAlphaMode) -> bool {
    *mode == MaterialAlphaMode::Opaque
}

fn default_alpha_cutoff() -> f32 {
    0.5
}

fn is_default_cutoff(value: &f32) -> bool {
    *value == 0.5
}

fn is_false(value: &bool) -> bool {
    !value
}

fn numeric(value: &toml::Value) -> Option<f32> {
    match value {
        toml::Value::Integer(value) => Some(*value as f32),
        toml::Value::Float(value) => Some(*value as f32),
        _ => None,
    }
}

fn numeric_values(value: &toml::Value, kind: ShaderPropertyKind) -> Option<Vec<f32>> {
    let expected = kind.components()?;
    let values = if expected == 1 {
        vec![numeric(value)?]
    } else {
        value
            .as_array()?
            .iter()
            .map(numeric)
            .collect::<Option<Vec<_>>>()?
    };
    (values.len() == expected && values.iter().all(|value| value.is_finite())).then_some(values)
}

fn line_for_offset(source: &str, offset: usize) -> usize {
    source.as_bytes()[..offset.min(source.len())]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn property_line(source: &str, key: &str) -> Option<usize> {
    source.lines().enumerate().find_map(|(line, text)| {
        let (name, _) = text.split_once('=')?;
        (name.trim() == key).then_some(line + 1)
    })
}

fn field_line(source: &str, key: &str) -> Option<usize> {
    source.lines().enumerate().find_map(|(line, text)| {
        let (name, _) = text.split_once('=')?;
        (name.trim() == key).then_some(line + 1)
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialAssetError {
    pub path: String,
    pub line: Option<usize>,
    pub message: String,
}

impl MaterialAssetError {
    fn new(path: &str, line: Option<usize>, message: impl Into<String>) -> Self {
        Self {
            path: path.to_owned(),
            line,
            message: message.into(),
        }
    }
}

impl fmt::Display for MaterialAssetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(line) = self.line {
            write!(f, "{}:{line}: {}", self.path, self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

impl Error for MaterialAssetError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shader_asset::ShaderSchema;

    #[test]
    fn defaults_serialize_and_round_trip() {
        let material = MaterialAsset::default();
        let text = material.to_toml().unwrap();
        assert_eq!(
            MaterialAsset::parse("empty.rvemat", &text).unwrap(),
            material
        );
        let resolved = material.resolve(&ShaderSchema::lit());
        let lit = resolved.lit.unwrap();
        assert_eq!(lit.base_color, [1.0, 1.0, 1.0]);
        assert_eq!(lit.metallic, 0.0);
        assert_eq!(lit.roughness, 1.0);
        assert_eq!(lit.emissive, [0.0, 0.0, 0.0]);
        assert_eq!(lit.opacity, 1.0);
        assert_eq!(lit.normal_scale, 1.0);
        assert_eq!(lit.occlusion_strength, 1.0);
        assert!(resolved.textures.iter().all(Option::is_none));
    }

    #[test]
    fn resolves_lit_defaults_values_textures_and_preserves_unknown_keys() {
        let source = "shader=\"/shaders/custom.rveshader\"\nalpha_mode=\"mask\"\nalpha_cutoff=0.25\n[properties]\nbase_color=[0.2,0.4,0.6]\nmetallic=0.7\nbase_color_texture=\"/textures/albedo.png\"\nunknown=\"/textures/unused.png\"\n";
        let material = MaterialAsset::parse("/materials/test.rvemat", source).unwrap();
        let resolved = material.resolve(&ShaderSchema::lit());
        let lit = resolved.lit.unwrap();
        assert_eq!(lit.base_color, [0.2, 0.4, 0.6]);
        assert_eq!(lit.metallic, 0.7);
        assert_eq!(
            resolved.textures[0].as_ref().unwrap().0.as_str(),
            "textures/albedo.png"
        );
        assert_eq!(resolved.alpha.mode, MaterialAlphaMode::Mask);
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains("unknown"))
        );
        assert!(material.to_toml().unwrap().contains("unknown"));
        assert!(
            material
                .dependencies()
                .iter()
                .any(|path| path.as_str() == "textures/unused.png")
        );
    }

    #[test]
    fn absent_properties_use_defaults_without_warnings() {
        let material = MaterialAsset::default();
        let resolved = material.resolve(&ShaderSchema::lit());
        assert!(resolved.warnings.is_empty(), "{:?}", resolved.warnings);
        assert_eq!(resolved.lit.unwrap().metallic, 0.0);
    }

    #[test]
    fn mismatched_type_uses_default_and_warns() {
        let material =
            MaterialAsset::parse("bad.rvemat", "[properties]\nmetallic=[0.2,0.4]\n").unwrap();
        let resolved = material.resolve(&ShaderSchema::lit());
        assert_eq!(resolved.lit.unwrap().metallic, 0.0);
        assert!(
            resolved
                .warnings
                .iter()
                .any(|warning| warning.contains("metallic имеет неверный тип"))
        );
    }

    #[test]
    fn rejects_invalid_alpha_values_and_property_values() {
        assert!(MaterialAsset::parse("bad.rvemat", "alpha_mode=\"invalid\"\n").is_err());
        assert!(MaterialAsset::parse("bad.rvemat", "alpha_cutoff=1.2\n").is_err());
        assert!(MaterialAsset::parse("bad.rvemat", "[properties]\nx=true\n").is_err());
        assert!(MaterialAsset::parse("bad.rvemat", "[properties]\nx=\"/../bad.png\"\n").is_err());
        assert!(MaterialAsset::parse("bad.rvemat", "extra=true\n").is_err());
        for alpha_mode in ["opaque", "mask", "blend"] {
            assert!(
                MaterialAsset::parse("valid.rvemat", &format!("alpha_mode=\"{alpha_mode}\"\n"))
                    .is_ok()
            );
        }
    }
}
