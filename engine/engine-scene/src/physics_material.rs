//! Физический материал коллайдера, назначаемый через `.rvephysmat`.

use crate::document::{ColliderComponent, SceneError};
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

/// Правило совмещения значения двух материалов. Порядок приоритета: average < minimum < multiply <
/// maximum; при разных правилах в паре побеждает старшее.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CombineMode {
    #[default]
    Average,
    Minimum,
    Multiply,
    Maximum,
}

/// Поверхностные свойства тела: трение покоя и скольжения, отскок и правила совмещения.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PhysicsMaterialSettings {
    pub static_friction: f32,
    pub dynamic_friction: f32,
    pub bounciness: f32,
    pub friction_combine: CombineMode,
    pub bounce_combine: CombineMode,
}

impl Default for PhysicsMaterialSettings {
    fn default() -> Self {
        Self {
            static_friction: 0.6,
            dynamic_friction: 0.6,
            bounciness: 0.0,
            friction_combine: CombineMode::Average,
            bounce_combine: CombineMode::Average,
        }
    }
}

impl PhysicsMaterialSettings {
    /// Разбирает и проверяет TOML, добавляя имя файла к ошибкам.
    pub fn parse(label: impl Into<String>, input: &str) -> Result<Self, PhysicsMaterialError> {
        let label = label.into();
        let settings: Self = toml::from_str(input)
            .map_err(|error| PhysicsMaterialError::new(&label, error.to_string()))?;
        settings.validate_at(&label)?;
        Ok(settings)
    }

    /// Сериализует материал в форматированный TOML.
    pub fn to_toml(&self) -> Result<String, PhysicsMaterialError> {
        toml::to_string_pretty(self)
            .map_err(|error| PhysicsMaterialError::new("physics material", error.to_string()))
    }

    /// Проверяет диапазоны значений.
    pub fn validate(&self) -> Result<(), PhysicsMaterialError> {
        self.validate_at("physics material")
    }

    fn validate_at(&self, label: &str) -> Result<(), PhysicsMaterialError> {
        if !self.static_friction.is_finite() || self.static_friction < 0.0 {
            return Err(PhysicsMaterialError::new(
                label,
                "static_friction должен быть конечным и >= 0",
            ));
        }
        if !self.dynamic_friction.is_finite() || self.dynamic_friction < 0.0 {
            return Err(PhysicsMaterialError::new(
                label,
                "dynamic_friction должен быть конечным и >= 0",
            ));
        }
        if !self.bounciness.is_finite() || !(0.0..=1.0).contains(&self.bounciness) {
            return Err(PhysicsMaterialError::new(
                label,
                "bounciness должен быть конечным и находиться в диапазоне [0, 1]",
            ));
        }
        Ok(())
    }
}

/// Ошибка разбора или проверки `.rvephysmat`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PhysicsMaterialError {
    pub label: String,
    pub message: String,
}

impl PhysicsMaterialError {
    fn new(label: &str, message: impl Into<String>) -> Self {
        Self {
            label: label.to_owned(),
            message: message.into(),
        }
    }
}

impl fmt::Display for PhysicsMaterialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.label, self.message)
    }
}

impl Error for PhysicsMaterialError {}

/// Читает `.rvephysmat` по пути коллайдера и кладёт результат в `resolved_material`. Один путь
/// читается один раз за вызов; ошибка чтения или разбора — ошибка загрузки объекта.
pub(crate) fn resolve_collider_material(
    collider: &mut ColliderComponent,
    source: &str,
    object_id: &str,
    read: &mut dyn FnMut(&VirtualPath) -> Result<String, String>,
    cache: &mut BTreeMap<String, PhysicsMaterialSettings>,
) -> Result<(), SceneError> {
    let Some(path) = &collider.material else {
        collider.resolved_material = None;
        return Ok(());
    };
    let key = path.as_str().to_owned();
    if !cache.contains_key(&key) {
        let text = read(path).map_err(|message| {
            SceneError::new(
                source,
                Some(object_id),
                format!("физический материал /{key}: {message}"),
            )
        })?;
        let settings = PhysicsMaterialSettings::parse(format!("/{key}"), &text)
            .map_err(|error| SceneError::new(source, Some(object_id), error.to_string()))?;
        cache.insert(key.clone(), settings);
    }
    collider.resolved_material = Some(cache[&key]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CombineMode, PhysicsMaterialSettings};
    use crate::document::SceneDocument;
    use crate::world::World;

    #[test]
    fn empty_document_uses_unity_defaults() {
        let settings = PhysicsMaterialSettings::parse("default.rvephysmat", "").unwrap();
        assert_eq!(settings, PhysicsMaterialSettings::default());
        assert_eq!(settings.static_friction, 0.6);
        assert_eq!(settings.dynamic_friction, 0.6);
        assert_eq!(settings.bounciness, 0.0);
        assert_eq!(settings.friction_combine, CombineMode::Average);
        assert_eq!(settings.bounce_combine, CombineMode::Average);
    }

    #[test]
    fn explicit_fields_and_combine_modes_parse() {
        let input = "static_friction = 0.05\ndynamic_friction = 0.02\nbounciness = 0.9\n\
                     friction_combine = \"multiply\"\nbounce_combine = \"maximum\"\n";
        let settings = PhysicsMaterialSettings::parse("ice.rvephysmat", input).unwrap();
        assert_eq!(settings.static_friction, 0.05);
        assert_eq!(settings.dynamic_friction, 0.02);
        assert_eq!(settings.bounciness, 0.9);
        assert_eq!(settings.friction_combine, CombineMode::Multiply);
        assert_eq!(settings.bounce_combine, CombineMode::Maximum);
    }

    #[test]
    fn round_trip_keeps_every_value() {
        let settings = PhysicsMaterialSettings {
            static_friction: 0.3,
            dynamic_friction: 0.25,
            bounciness: 0.5,
            friction_combine: CombineMode::Minimum,
            bounce_combine: CombineMode::Multiply,
        };
        let serialized = settings.to_toml().unwrap();
        assert_eq!(
            PhysicsMaterialSettings::parse("round-trip.rvephysmat", &serialized).unwrap(),
            settings
        );
    }

    #[test]
    fn unknown_fields_and_bad_combine_modes_are_rejected() {
        for input in [
            "friction = 0.5",
            "friction_combine = \"geometric\"",
            "bounciness = \"high\"",
        ] {
            let error = PhysicsMaterialSettings::parse("bad.rvephysmat", input).unwrap_err();
            assert!(error.to_string().contains("bad.rvephysmat"), "{input}");
        }
    }

    #[test]
    fn negative_or_non_finite_friction_and_out_of_range_bounciness_are_rejected() {
        for input in [
            "static_friction = -0.1",
            "dynamic_friction = -1.0",
            "bounciness = 1.5",
            "bounciness = -0.1",
            "static_friction = nan",
        ] {
            assert!(
                PhysicsMaterialSettings::parse("invalid.rvephysmat", input).is_err(),
                "{input}"
            );
        }
    }

    #[test]
    fn scene_collider_materials_are_read_once_per_path_and_missing_files_fail() {
        let input = "[[assets]]\npath=\"/materials/ice.rvephysmat\"\n\
                     [[objects]]\nid=\"camera\"\n[objects.camera]\n\
                     [[objects]]\nid=\"ball\"\n[objects.collider]\nmaterial=\"/materials/ice.rvephysmat\"\n\
                     [[objects]]\nid=\"crate\"\n[objects.collider]\nmaterial=\"/materials/ice.rvephysmat\"\n";
        let document = SceneDocument::parse("scenes/test.rvescene", input).unwrap();
        let mut world = World::from_document(&document).unwrap();
        let mut reads = 0;
        world
            .resolve_physics_materials(&mut |path| {
                reads += 1;
                assert_eq!(path.as_str(), "materials/ice.rvephysmat");
                Ok("static_friction = 0.05\n".to_owned())
            })
            .unwrap();
        assert_eq!(reads, 1);
        let resolved = |id: &str| {
            world
                .objects()
                .iter()
                .find(|object| object.id == id)
                .and_then(|object| object.collider.as_ref()?.resolved_material)
        };
        assert_eq!(resolved("ball").map(|s| s.static_friction), Some(0.05));
        assert_eq!(resolved("crate").map(|s| s.static_friction), Some(0.05));

        let mut world = World::from_document(&document).unwrap();
        let error = world
            .resolve_physics_materials(&mut |_| Err("файл не найден".to_owned()))
            .unwrap_err();
        assert!(error.to_string().contains("файл не найден"), "{error}");
    }

    #[test]
    fn prefab_template_colliders_get_their_resolved_material() {
        let text = "[[assets]]\npath=\"/materials/rubber.rvephysmat\"\n\
                    [[objects]]\nid=\"ball\"\n[objects.collider]\nshape=\"sphere\"\nradius=0.5\n\
                    material=\"/materials/rubber.rvephysmat\"\n";
        let path = engine_files::VirtualPath::parse("prefabs/ball.rveprefab").unwrap();
        let mut template =
            crate::prefab::PrefabTemplate::load(&path, &mut |_| Ok(text.to_owned())).unwrap();
        assert!(
            template.objects()[0]
                .collider
                .as_ref()
                .unwrap()
                .resolved_material
                .is_none()
        );
        template
            .resolve_physics_materials(&mut |_| Ok("bounciness = 0.7\n".to_owned()))
            .unwrap();
        let collider = template.objects()[0].collider.as_ref().unwrap();
        assert_eq!(collider.resolved_material.unwrap().bounciness, 0.7);
    }

    #[test]
    fn zero_friction_and_full_bounce_are_valid() {
        let input = "static_friction = 0.0\ndynamic_friction = 0.0\nbounciness = 1.0\n";
        assert!(PhysicsMaterialSettings::parse("edge.rvephysmat", input).is_ok());
    }
}
