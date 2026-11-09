//! Схема параметров скрипта: объявленные имена, типы, значения по умолчанию и диапазоны.

use crate::asset_kind::AssetKind;
use crate::document::ParamValue;
use engine_files::VirtualPath;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Наибольшая длина имени параметра в байтах.
pub const MAX_PARAM_NAME_BYTES: usize = 64;

/// A scene component that can be required by a script reference parameter.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ComponentKind {
    Transform,
    Camera,
    MeshRenderer,
    LodGroup,
    DirectionalLight,
    PointLight,
    SpotLight,
    Environment,
    Collider,
    RigidBody,
    CharacterController,
    AudioSource,
    Animator,
    ParticleEmitter,
    NetworkIdentity,
    NetworkTransform,
    Script(String),
}

impl ComponentKind {
    pub fn parse(name: &str) -> Option<Self> {
        if let Some(script) = name.strip_prefix("script:") {
            return (!script.is_empty()).then(|| Self::Script(script.to_owned()));
        }
        Some(match name {
            "transform" => Self::Transform,
            "camera" => Self::Camera,
            "mesh_renderer" => Self::MeshRenderer,
            "lod_group" => Self::LodGroup,
            "directional_light" => Self::DirectionalLight,
            "point_light" => Self::PointLight,
            "spot_light" => Self::SpotLight,
            "environment" => Self::Environment,
            "collider" => Self::Collider,
            "rigid_body" => Self::RigidBody,
            "character_controller" => Self::CharacterController,
            "audio_source" => Self::AudioSource,
            "animator" => Self::Animator,
            "particle_emitter" => Self::ParticleEmitter,
            "network_identity" => Self::NetworkIdentity,
            "network_transform" => Self::NetworkTransform,
            _ => return None,
        })
    }

    pub fn name(&self) -> String {
        match self {
            Self::Transform => "transform".to_owned(),
            Self::Camera => "camera".to_owned(),
            Self::MeshRenderer => "mesh_renderer".to_owned(),
            Self::LodGroup => "lod_group".to_owned(),
            Self::DirectionalLight => "directional_light".to_owned(),
            Self::PointLight => "point_light".to_owned(),
            Self::SpotLight => "spot_light".to_owned(),
            Self::Environment => "environment".to_owned(),
            Self::Collider => "collider".to_owned(),
            Self::RigidBody => "rigid_body".to_owned(),
            Self::CharacterController => "character_controller".to_owned(),
            Self::AudioSource => "audio_source".to_owned(),
            Self::Animator => "animator".to_owned(),
            Self::ParticleEmitter => "particle_emitter".to_owned(),
            Self::NetworkIdentity => "network_identity".to_owned(),
            Self::NetworkTransform => "network_transform".to_owned(),
            Self::Script(script) => format!("script:{script}"),
        }
    }
}

/// Declared script parameter kind. `Vec3` and `Color` use three numeric array entries.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ParamKind {
    Bool,
    Int,
    Float,
    String,
    Vec3,
    Object,
    Component(ComponentKind),
    Asset(Option<AssetKind>),
    Enum(Vec<String>),
    Color,
    Array(Box<ParamKind>),
}

impl ParamKind {
    /// Разбирает имя типа из схемы Lua: `bool`, `int`, `float`, `string`, `vec3`.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "bool" => Self::Bool,
            "int" => Self::Int,
            "float" => Self::Float,
            "string" => Self::String,
            "vec3" => Self::Vec3,
            "object" => Self::Object,
            "color" => Self::Color,
            "asset" => Self::Asset(None),
            _ => return None,
        })
    }

    pub fn name(&self) -> String {
        match self {
            Self::Bool => "bool".to_owned(),
            Self::Int => "int".to_owned(),
            Self::Float => "float".to_owned(),
            Self::String => "string".to_owned(),
            Self::Vec3 => "vec3".to_owned(),
            Self::Object => "object".to_owned(),
            Self::Component(component) => format!("component ({})", component.name()),
            Self::Asset(Some(kind)) => format!("asset ({})", kind.name()),
            Self::Asset(None) => "asset".to_owned(),
            Self::Enum(_) => "enum".to_owned(),
            Self::Color => "color".to_owned(),
            Self::Array(kind) => format!("array of {}", kind.name()),
        }
    }

    /// Допускает ли тип границы `min`/`max`.
    pub fn is_numeric(&self) -> bool {
        matches!(self, Self::Int | Self::Float)
    }

    /// Значение по умолчанию, когда схема его не задаёт.
    pub fn zero(&self) -> ParamValue {
        match self {
            Self::Bool => ParamValue::Bool(false),
            Self::Int => ParamValue::Int(0),
            Self::Float => ParamValue::Float(0.0),
            Self::String => ParamValue::String(String::new()),
            Self::Vec3 => ParamValue::Array(vec![ParamValue::Float(0.0); 3]),
            Self::Object | Self::Component(_) => ParamValue::Object(String::new()),
            // An empty string is the schema's `None` default for optional asset references.
            Self::Asset(_) => ParamValue::String(String::new()),
            Self::Enum(values) => ParamValue::String(values.first().cloned().unwrap_or_default()),
            Self::Color => ParamValue::Array(vec![ParamValue::Float(0.0); 3]),
            Self::Array(_) => ParamValue::Array(Vec::new()),
        }
    }
}

impl fmt::Display for ParamKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.name())
    }
}

/// Объявление одного параметра.
#[derive(Clone, Debug, PartialEq)]
pub struct ParamSpec {
    pub name: String,
    pub kind: ParamKind,
    pub default: ParamValue,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Подпись для Inspector; без неё показывается имя.
    pub label: Option<String>,
}

impl ParamSpec {
    /// Проверяет значение по типу и диапазону и приводит числа массивов к каноническому виду.
    pub fn coerce(&self, value: &ParamValue) -> Result<ParamValue, String> {
        let value = coerce_kind(&self.name, &self.kind, value).map_err(|()| self.mismatch())?;
        let number = match value {
            ParamValue::Int(value) => Some(value as f64),
            ParamValue::Float(value) => Some(value),
            _ => None,
        };
        if let Some(number) = number {
            if !number.is_finite() {
                return Err(format!("параметр {:?} должен быть конечным", self.name));
            }
            if self.min.is_some_and(|min| number < min) || self.max.is_some_and(|max| number > max)
            {
                return Err(format!(
                    "параметр {:?} = {number} вне диапазона {}..={}",
                    self.name,
                    self.min.map_or("-inf".to_owned(), |min| min.to_string()),
                    self.max.map_or("inf".to_owned(), |max| max.to_string()),
                ));
            }
        }
        Ok(value)
    }

    fn mismatch(&self) -> String {
        format!("параметр {:?} должен иметь тип {}", self.name, self.kind)
    }
}

fn coerce_kind(name: &str, kind: &ParamKind, value: &ParamValue) -> Result<ParamValue, ()> {
    match (kind, value) {
        (ParamKind::Bool, ParamValue::Bool(_))
        | (ParamKind::Int, ParamValue::Int(_))
        | (ParamKind::String, ParamValue::String(_))
        | (ParamKind::Object, ParamValue::Object(_))
        | (ParamKind::Component(_), ParamValue::Object(_)) => Ok(value.clone()),
        (ParamKind::Float, ParamValue::Float(_)) => Ok(value.clone()),
        (ParamKind::Float, ParamValue::Int(value)) => Ok(ParamValue::Float(*value as f64)),
        (ParamKind::Asset(_), ParamValue::String(value)) if value.is_empty() => {
            Ok(ParamValue::String(value.clone()))
        }
        (ParamKind::Asset(expected), ParamValue::String(value)) => {
            let path = VirtualPath::parse(value).map_err(|_| ())?;
            if expected.is_some_and(|expected| AssetKind::of(path.as_str()) != expected) {
                return Err(());
            }
            Ok(ParamValue::Asset(path))
        }
        (ParamKind::Asset(expected), ParamValue::Asset(path))
            if expected.is_none_or(|expected| AssetKind::of(path.as_str()) == expected) =>
        {
            Ok(value.clone())
        }
        (ParamKind::Enum(values), ParamValue::String(value)) if values.contains(value) => {
            Ok(ParamValue::String(value.clone()))
        }
        (ParamKind::Vec3 | ParamKind::Color, ParamValue::Array(values)) if values.len() == 3 => {
            let mut components = Vec::with_capacity(3);
            for value in values {
                let number = match value {
                    ParamValue::Int(value) => *value as f64,
                    ParamValue::Float(value) => *value,
                    _ => return Err(()),
                };
                if !number.is_finite()
                    || matches!(kind, ParamKind::Color) && !(0.0..=1.0).contains(&number)
                {
                    return Err(());
                }
                components.push(ParamValue::Float(number));
            }
            Ok(ParamValue::Array(components))
        }
        (ParamKind::Array(element), ParamValue::Array(values)) => values
            .iter()
            .map(|value| coerce_kind(name, element, value))
            .collect::<Result<Vec<_>, _>>()
            .map(ParamValue::Array),
        _ => {
            let _ = name;
            Err(())
        }
    }
}

/// Упорядоченная схема параметров скрипта; порядок объявлений сохраняется.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ParamSchema(Vec<ParamSpec>);

impl ParamSchema {
    /// Проверяет имена, отсутствие повторов, границы и значения по умолчанию;
    /// значения по умолчанию приводятся к каноническому виду.
    pub fn new(specs: Vec<ParamSpec>) -> Result<Self, String> {
        let mut names = BTreeSet::new();
        let mut checked = Vec::with_capacity(specs.len());
        for mut spec in specs {
            validate_name(&spec.name)?;
            if !names.insert(spec.name.clone()) {
                return Err(format!("параметр {:?} объявлен повторно", spec.name));
            }
            if (spec.min.is_some() || spec.max.is_some()) && !spec.kind.is_numeric() {
                return Err(format!(
                    "min/max допустимы только у int и float: параметр {:?}",
                    spec.name
                ));
            }
            if spec.min.is_some_and(|min| !min.is_finite())
                || spec.max.is_some_and(|max| !max.is_finite())
            {
                return Err(format!(
                    "min/max параметра {:?} должны быть конечными",
                    spec.name
                ));
            }
            if let (Some(min), Some(max)) = (spec.min, spec.max)
                && min > max
            {
                return Err(format!("у параметра {:?} min больше max", spec.name));
            }
            validate_kind(&spec.kind, &spec.name)?;
            spec.default = spec
                .coerce(&spec.default)
                .map_err(|error| format!("значение по умолчанию: {error}"))?;
            checked.push(spec);
        }
        Ok(Self(checked))
    }

    pub fn specs(&self) -> &[ParamSpec] {
        &self.0
    }

    pub fn get(&self, name: &str) -> Option<&ParamSpec> {
        self.0.iter().find(|spec| spec.name == name)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Значения для экземпляра скрипта: заданные в сцене после проверки и приведения,
    /// остальные — по умолчанию. Необъявленный ключ — ошибка.
    pub fn resolve(
        &self,
        params: &BTreeMap<String, ParamValue>,
    ) -> Result<BTreeMap<String, ParamValue>, String> {
        if let Some(unknown) = params.keys().find(|name| self.get(name).is_none()) {
            return Err(format!("параметр {unknown:?} не объявлен скриптом"));
        }
        self.0
            .iter()
            .map(|spec| {
                let value = match params.get(&spec.name) {
                    Some(value) => spec.coerce(value)?,
                    None => spec.default.clone(),
                };
                Ok((spec.name.clone(), value))
            })
            .collect()
    }
}

fn validate_kind(kind: &ParamKind, name: &str) -> Result<(), String> {
    match kind {
        ParamKind::Enum(values) => {
            let unique = values.iter().collect::<BTreeSet<_>>();
            if values.is_empty()
                || unique.len() != values.len()
                || values.iter().any(String::is_empty)
            {
                return Err(format!(
                    "enum параметра {name:?} должен содержать непустые уникальные значения"
                ));
            }
        }
        ParamKind::Array(element) => validate_kind(element, name)?,
        _ => {}
    }
    Ok(())
}

/// Имя параметра — идентификатор `[A-Za-z_][A-Za-z0-9_]*` не длиннее 64 байт.
fn validate_name(name: &str) -> Result<(), String> {
    let mut characters = name.chars();
    let valid = name.len() <= MAX_PARAM_NAME_BYTES
        && characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "имя параметра {name:?} должно быть идентификатором [A-Za-z_][A-Za-z0-9_]* до {MAX_PARAM_NAME_BYTES} байт"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, kind: ParamKind, default: ParamValue) -> ParamSpec {
        ParamSpec {
            name: name.to_owned(),
            kind,
            default,
            min: None,
            max: None,
            label: None,
        }
    }

    fn speed() -> ParamSpec {
        ParamSpec {
            min: Some(0.0),
            max: Some(360.0),
            ..spec("speed", ParamKind::Float, ParamValue::Int(30))
        }
    }

    fn params(entries: &[(&str, ParamValue)]) -> BTreeMap<String, ParamValue> {
        entries
            .iter()
            .map(|(name, value)| ((*name).to_owned(), value.clone()))
            .collect()
    }

    fn vec3(values: [f64; 3]) -> ParamValue {
        ParamValue::Array(values.map(ParamValue::Float).to_vec())
    }

    #[test]
    fn schema_keeps_declaration_order_and_normalizes_defaults() {
        let schema = ParamSchema::new(vec![
            speed(),
            spec("enabled", ParamKind::Bool, ParamValue::Bool(true)),
            spec(
                "offset",
                ParamKind::Vec3,
                ParamValue::Array(vec![
                    ParamValue::Int(1),
                    ParamValue::Float(2.5),
                    ParamValue::Int(0),
                ]),
            ),
        ])
        .unwrap();
        let names: Vec<_> = schema
            .specs()
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert_eq!(names, ["speed", "enabled", "offset"]);
        assert_eq!(
            schema.get("speed").unwrap().default,
            ParamValue::Float(30.0)
        );
        assert_eq!(schema.get("offset").unwrap().default, vec3([1.0, 2.5, 0.0]));
    }

    #[test]
    fn invalid_schemas_are_rejected() {
        let invalid = [
            vec![spec("1bad", ParamKind::Int, ParamValue::Int(0))],
            vec![spec("a.b", ParamKind::Int, ParamValue::Int(0))],
            vec![spec(&"x".repeat(65), ParamKind::Int, ParamValue::Int(0))],
            vec![speed(), speed()],
            vec![spec("flag", ParamKind::Bool, ParamValue::Int(1))],
            vec![ParamSpec {
                min: Some(0.0),
                ..spec("flag", ParamKind::Bool, ParamValue::Bool(true))
            }],
            vec![ParamSpec {
                default: ParamValue::Float(400.0),
                ..speed()
            }],
            vec![ParamSpec {
                min: Some(10.0),
                max: Some(1.0),
                ..speed()
            }],
            vec![ParamSpec {
                max: Some(f64::NAN),
                ..speed()
            }],
            vec![spec("count", ParamKind::Int, ParamValue::Float(1.0))],
            vec![spec(
                "offset",
                ParamKind::Vec3,
                ParamValue::Array(vec![ParamValue::Float(1.0); 2]),
            )],
        ];
        for specs in invalid {
            assert!(ParamSchema::new(specs.clone()).is_err(), "{specs:?}");
        }
    }

    #[test]
    fn resolve_fills_defaults_and_coerces_integers() {
        let schema = ParamSchema::new(vec![
            speed(),
            spec("count", ParamKind::Int, ParamValue::Int(2)),
            spec("offset", ParamKind::Vec3, ParamKind::Vec3.zero()),
        ])
        .unwrap();
        let resolved = schema
            .resolve(&params(&[
                ("speed", ParamValue::Int(90)),
                (
                    "offset",
                    ParamValue::Array(vec![
                        ParamValue::Int(1),
                        ParamValue::Int(2),
                        ParamValue::Int(3),
                    ]),
                ),
            ]))
            .unwrap();
        assert_eq!(
            resolved,
            params(&[
                ("speed", ParamValue::Float(90.0)),
                ("count", ParamValue::Int(2)),
                ("offset", vec3([1.0, 2.0, 3.0])),
            ])
        );
    }

    #[test]
    fn resolve_rejects_wrong_types_ranges_and_unknown_keys() {
        let schema = ParamSchema::new(vec![
            speed(),
            spec("count", ParamKind::Int, ParamValue::Int(2)),
            spec("offset", ParamKind::Vec3, ParamKind::Vec3.zero()),
        ])
        .unwrap();
        for wrong in [
            params(&[("speed", ParamValue::String("fast".into()))]),
            params(&[("speed", ParamValue::Float(-1.0))]),
            params(&[("speed", ParamValue::Int(361))]),
            params(&[("count", ParamValue::Float(1.0))]),
            params(&[("offset", ParamValue::Array(vec![ParamValue::Int(1); 4]))]),
            params(&[("offset", ParamValue::Array(vec![ParamValue::Bool(true); 3]))]),
            params(&[("unknown", ParamValue::Int(1))]),
        ] {
            assert!(schema.resolve(&wrong).is_err(), "{wrong:?}");
        }
        let error = schema
            .resolve(&params(&[("unknown", ParamValue::Int(1))]))
            .unwrap_err();
        assert!(error.contains("unknown"), "{error}");
    }

    #[test]
    fn kinds_round_trip_through_their_names() {
        for kind in [
            ParamKind::Bool,
            ParamKind::Int,
            ParamKind::Float,
            ParamKind::String,
            ParamKind::Vec3,
        ] {
            assert_eq!(ParamKind::parse(&kind.name()), Some(kind.clone()));
            let zero = kind.zero();
            assert_eq!(spec("value", kind, zero.clone()).coerce(&zero), Ok(zero));
        }
        assert_eq!(ParamKind::parse("double"), None);
    }

    #[test]
    fn references_enums_colors_and_typed_arrays_are_coerced() {
        let texture = VirtualPath::parse("textures/hero.PNG").unwrap();
        let ui_document = VirtualPath::parse("ui/hud.rveui").unwrap();
        let schema = ParamSchema::new(vec![
            spec("target", ParamKind::Object, ParamKind::Object.zero()),
            spec(
                "body",
                ParamKind::Component(ComponentKind::RigidBody),
                ParamKind::Component(ComponentKind::RigidBody).zero(),
            ),
            spec(
                "texture",
                ParamKind::Asset(Some(AssetKind::Texture)),
                ParamKind::Asset(Some(AssetKind::Texture)).zero(),
            ),
            spec(
                "hud",
                ParamKind::Asset(Some(AssetKind::Ui)),
                ParamKind::Asset(Some(AssetKind::Ui)).zero(),
            ),
            spec(
                "mode",
                ParamKind::Enum(vec!["walk".into(), "run".into()]),
                ParamValue::String("walk".into()),
            ),
            spec("color", ParamKind::Color, ParamKind::Color.zero()),
            spec(
                "weights",
                ParamKind::Array(Box::new(ParamKind::Float)),
                ParamValue::Array(vec![]),
            ),
        ])
        .unwrap();
        let resolved = schema
            .resolve(&params(&[
                ("target", ParamValue::Object("player".into())),
                ("body", ParamValue::Object("player".into())),
                ("texture", ParamValue::Asset(texture.clone())),
                ("hud", ParamValue::Asset(ui_document.clone())),
                ("mode", ParamValue::String("run".into())),
                ("color", vec3([0.1, 0.2, 1.0])),
                (
                    "weights",
                    ParamValue::Array(vec![ParamValue::Int(1), ParamValue::Float(2.5)]),
                ),
            ]))
            .unwrap();
        assert_eq!(resolved["texture"], ParamValue::Asset(texture));
        assert_eq!(resolved["hud"], ParamValue::Asset(ui_document));
        assert_eq!(resolved["color"], vec3([0.1, 0.2, 1.0]));
        assert_eq!(
            resolved["weights"],
            ParamValue::Array(vec![ParamValue::Float(1.0), ParamValue::Float(2.5)])
        );
        for wrong in [
            params(&[("mode", ParamValue::String("fly".into()))]),
            params(&[("color", vec3([0.1, 1.01, 0.0]))]),
            params(&[("weights", ParamValue::Array(vec![ParamValue::Bool(true)]))]),
            params(&[(
                "texture",
                ParamValue::Asset(VirtualPath::parse("x.fbx").unwrap()),
            )]),
            params(&[(
                "hud",
                ParamValue::Asset(VirtualPath::parse("ui/hud.png").unwrap()),
            )]),
            params(&[("target", ParamValue::String("player".into()))]),
        ] {
            assert!(schema.resolve(&wrong).is_err(), "{wrong:?}");
        }
    }
}
