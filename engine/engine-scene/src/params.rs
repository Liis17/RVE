//! Схема параметров скрипта: объявленные имена, типы, значения по умолчанию и диапазоны.

use crate::document::ParamValue;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Наибольшая длина имени параметра в байтах.
pub const MAX_PARAM_NAME_BYTES: usize = 64;

/// Тип объявленного параметра. `Vec3` хранится в сцене как массив из трёх чисел.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ParamKind {
    Bool,
    Int,
    Float,
    String,
    Vec3,
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
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::String => "string",
            Self::Vec3 => "vec3",
        }
    }

    /// Допускает ли тип границы `min`/`max`.
    pub fn is_numeric(self) -> bool {
        matches!(self, Self::Int | Self::Float)
    }

    /// Значение по умолчанию, когда схема его не задаёт.
    pub fn zero(self) -> ParamValue {
        match self {
            Self::Bool => ParamValue::Bool(false),
            Self::Int => ParamValue::Int(0),
            Self::Float => ParamValue::Float(0.0),
            Self::String => ParamValue::String(String::new()),
            Self::Vec3 => ParamValue::Array(vec![ParamValue::Float(0.0); 3]),
        }
    }
}

impl fmt::Display for ParamKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
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
    /// Проверяет значение по типу и диапазону и приводит его к каноническому виду:
    /// целое число для `Float` и компонент `Vec3` становится вещественным.
    pub fn coerce(&self, value: &ParamValue) -> Result<ParamValue, String> {
        let value = match (self.kind, value) {
            (ParamKind::Bool, ParamValue::Bool(_))
            | (ParamKind::Int, ParamValue::Int(_))
            | (ParamKind::Float, ParamValue::Float(_))
            | (ParamKind::String, ParamValue::String(_)) => value.clone(),
            (ParamKind::Float, ParamValue::Int(value)) => ParamValue::Float(*value as f64),
            (ParamKind::Vec3, ParamValue::Array(values)) if values.len() == 3 => {
                let mut components = Vec::with_capacity(3);
                for value in values {
                    components.push(ParamValue::Float(match value {
                        ParamValue::Int(value) => *value as f64,
                        ParamValue::Float(value) => *value,
                        _ => return Err(self.mismatch()),
                    }));
                }
                ParamValue::Array(components)
            }
            _ => return Err(self.mismatch()),
        };
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
            assert_eq!(ParamKind::parse(kind.name()), Some(kind));
            let zero = kind.zero();
            assert_eq!(spec("value", kind, zero.clone()).coerce(&zero), Ok(zero));
        }
        assert_eq!(ParamKind::parse("double"), None);
    }
}
