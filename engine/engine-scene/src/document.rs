//! TOML-документы сцен и их проверенная схема, независимая от renderer.

use crate::world::{LocalTransform, calculate_world_matrices};
use engine_files::VirtualPath;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::path::Path;

/// Ошибка разбора или проверки сцены с меткой источника и необязательным id объекта.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SceneError {
    pub source: String,
    pub object_id: Option<String>,
    pub message: String,
}

impl SceneError {
    pub(crate) fn new(source: &str, object_id: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            source: source.to_owned(),
            object_id: object_id.map(str::to_owned),
            message: message.into(),
        }
    }
}

impl fmt::Display for SceneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.object_id {
            Some(id) => write!(f, "{}: объект {id:?}: {}", self.source, self.message),
            None => write!(f, "{}: {}", self.source, self.message),
        }
    }
}

impl Error for SceneError {}

/// Разобранный и проверенный документ `.rvescene`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneDocument {
    #[serde(default)]
    pub assets: Vec<SceneAsset>,
    #[serde(default)]
    pub objects: Vec<SceneObject>,
    #[serde(skip)]
    source_label: String,
}

impl SceneDocument {
    /// Разбирает TOML и проверяет идентификаторы, ссылки, числа и мировые базисы.
    pub fn parse(source_label: impl Into<String>, input: &str) -> Result<Self, SceneError> {
        let source_label = source_label.into();
        let value: toml::Value = toml::from_str(input)
            .map_err(|error| SceneError::new(&source_label, None, error.to_string()))?;
        validate_raw_virtual_paths(&value, &source_label)?;
        let mut document: Self = toml::from_str(input)
            .map_err(|error| SceneError::new(&source_label, None, error.to_string()))?;
        document.source_label = source_label;
        document.validate()?;
        Ok(document)
    }

    /// Сериализует документ в форматированный TOML.
    pub fn to_toml(&self) -> Result<String, SceneError> {
        toml::to_string_pretty(self)
            .map_err(|error| SceneError::new(&self.source_label, None, error.to_string()))
    }

    /// Метка источника, переданная в [`SceneDocument::parse`].
    pub fn source_label(&self) -> &str {
        &self.source_label
    }

    pub(crate) fn validate(&self) -> Result<(), SceneError> {
        let source = &self.source_label;
        let mut asset_paths = HashSet::with_capacity(self.assets.len());
        for asset in &self.assets {
            if !asset_paths.insert(asset.path.clone()) {
                return Err(SceneError::new(
                    source,
                    None,
                    format!("повторяется путь ресурса /{}", asset.path.as_str()),
                ));
            }
            if let Some(archive) = &asset.archive
                && (!valid_ascii_name(archive, true)
                    || ["base", "shared"]
                        .iter()
                        .any(|reserved| archive.eq_ignore_ascii_case(reserved)))
            {
                return Err(SceneError::new(
                    source,
                    None,
                    format!("некорректное имя архива {archive:?}"),
                ));
            }
        }

        let mut object_indices = HashMap::with_capacity(self.objects.len());
        for (index, object) in self.objects.iter().enumerate() {
            if !valid_ascii_name(&object.id, true) {
                return Err(SceneError::new(
                    source,
                    Some(&object.id),
                    "id должен соответствовать [A-Za-z0-9_-]{1,64}",
                ));
            }
            if object_indices.insert(object.id.as_str(), index).is_some() {
                return Err(SceneError::new(
                    source,
                    Some(&object.id),
                    "id объекта уже используется",
                ));
            }
        }

        let mut camera_count = 0;
        let mut light_count = 0;
        for object in &self.objects {
            if let Some(parent) = &object.parent
                && !object_indices.contains_key(parent.as_str())
            {
                return Err(SceneError::new(
                    source,
                    Some(&object.id),
                    format!("родитель {parent:?} не существует"),
                ));
            }
            validate_transform(source, object)?;
            if let Some(camera) = &object.camera {
                camera_count += 1;
                validate_camera(source, object, camera)?;
            }
            if let Some(light) = &object.directional_light {
                light_count += 1;
                validate_light(source, object, light)?;
            }
            if let Some(mesh) = &object.mesh_renderer {
                require_asset_path(source, object, &asset_paths, &mesh.model, "модель")?;
                require_extension(source, object, &mesh.model, "fbx", "модель")?;
                if let Some(texture) = &mesh.texture {
                    require_asset_path(source, object, &asset_paths, texture, "текстура")?;
                    require_extension(source, object, texture, "png", "текстура")?;
                }
            }
            for script in &object.lite_scripts {
                require_asset_path(source, object, &asset_paths, &script.path, "lite-скрипт")?;
                require_extension(source, object, &script.path, "lua", "lite-скрипт")?;
                validate_params(source, object, &script.params)?;
            }
            for script in &object.heavy_scripts {
                if !valid_ascii_name(&script.name, false) {
                    return Err(SceneError::new(
                        source,
                        Some(&object.id),
                        format!(
                            "имя heavy-скрипта {:?} должно соответствовать [A-Za-z0-9_]{{1,64}}",
                            script.name
                        ),
                    ));
                }
                validate_params(source, object, &script.params)?;
            }
        }

        if camera_count != 1 {
            return Err(SceneError::new(
                source,
                None,
                format!("сцена должна содержать ровно одну камеру, найдено {camera_count}"),
            ));
        }
        if light_count > 1 {
            return Err(SceneError::new(
                source,
                None,
                format!(
                    "в сцене допускается не более одного directional_light, найдено {light_count}"
                ),
            ));
        }

        let world_matrices = validated_world_matrices(self, &object_indices)?;
        for (index, object) in self.objects.iter().enumerate() {
            let matrix = world_matrices[index];
            if !matrix.is_finite() {
                return Err(SceneError::new(
                    source,
                    Some(&object.id),
                    "мировая матрица содержит не конечные значения",
                ));
            }
            if object.camera.is_some()
                && camera_from_matrix(matrix, object.camera.unwrap()).is_none()
            {
                return Err(SceneError::new(
                    source,
                    Some(&object.id),
                    "мировые базисы камеры должны быть конечными, ненулевыми и неколлинеарными",
                ));
            }
            if object.directional_light.is_some() && light_direction_from_matrix(matrix).is_none() {
                return Err(SceneError::new(
                    source,
                    Some(&object.id),
                    "мировое направление directional_light должно быть конечным и ненулевым",
                ));
            }
        }
        Ok(())
    }
}

/// Ресурс, объявленный для включения в архив сцены.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneAsset {
    #[serde(with = "virtual_path_serde")]
    pub path: VirtualPath,
    #[serde(default)]
    pub archive: Option<String>,
}

/// Объект сцены в порядке документа.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneObject {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default)]
    pub transform: SceneTransform,
    #[serde(default)]
    pub camera: Option<CameraComponent>,
    #[serde(default)]
    pub mesh_renderer: Option<MeshRenderer>,
    #[serde(default)]
    pub directional_light: Option<DirectionalLightComponent>,
    #[serde(default)]
    pub lite_scripts: Vec<LiteScript>,
    #[serde(default)]
    pub heavy_scripts: Vec<HeavyScript>,
}

impl SceneObject {
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

/// Локальная трансформация документа; углы вращения хранятся в градусах.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneTransform {
    #[serde(default = "zero3")]
    pub position: [f32; 3],
    #[serde(default = "zero3")]
    pub rotation: [f32; 3],
    #[serde(default = "one3")]
    pub scale: [f32; 3],
}

impl Default for SceneTransform {
    fn default() -> Self {
        Self {
            position: zero3(),
            rotation: zero3(),
            scale: one3(),
        }
    }
}

/// Параметры проекции камеры. Поле зрения измеряется по узкой стороне.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraComponent {
    pub fov_deg: f32,
    pub near: f32,
    pub far: f32,
}

/// Экземпляр FBX и необязательная замена PNG-текстуры.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshRenderer {
    #[serde(with = "virtual_path_serde")]
    pub model: VirtualPath,
    #[serde(default, with = "option_virtual_path_serde")]
    pub texture: Option<VirtualPath>,
}

/// Цвет направленного света, ambient и необязательная проекция теней.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectionalLightComponent {
    #[serde(default = "one3")]
    pub color: [f32; 3],
    #[serde(default = "default_ambient")]
    pub ambient: [f32; 3],
    #[serde(default)]
    pub shadows: bool,
    #[serde(default = "zero3")]
    pub shadow_center: [f32; 3],
    #[serde(default)]
    pub shadow_radius: Option<f32>,
}

/// Скаляр или массив значений параметра скрипта.
#[derive(Clone, Debug, PartialEq)]
pub enum ParamValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Array(Vec<ParamValue>),
}

impl Serialize for ParamValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Int(value) => serializer.serialize_i64(*value),
            Self::Float(value) => serializer.serialize_f64(*value),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ParamValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ParamVisitor;

        impl<'de> Visitor<'de> for ParamVisitor {
            type Value = ParamValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "логическое значение, целое число, вещественное число, строка или массив таких значений",
                )
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(ParamValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(ParamValue::Int(value))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                i64::try_from(value)
                    .map(ParamValue::Int)
                    .map_err(|_| E::custom("целое число превышает i64::MAX"))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
                Ok(ParamValue::Float(value))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(ParamValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(ParamValue::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(ParamValue::Array(values))
            }

            fn visit_map<A>(self, _map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                Err(de::Error::custom(
                    "вложенные таблицы параметров и даты/время TOML не поддерживаются",
                ))
            }
        }

        deserializer.deserialize_any(ParamVisitor)
    }
}

/// Файл скрипта для lightweight runtime.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiteScript {
    #[serde(with = "virtual_path_serde")]
    pub path: VirtualPath,
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
}

/// Фабрика скрипта, зарегистрированная игровым крейтом.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeavyScript {
    pub name: String,
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
}

fn validate_transform(source: &str, object: &SceneObject) -> Result<(), SceneError> {
    for (name, values) in [
        ("position", object.transform.position),
        ("rotation", object.transform.rotation),
        ("scale", object.transform.scale),
    ] {
        if !values.into_iter().all(f32::is_finite) {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                format!("transform.{name} должен содержать конечные числа"),
            ));
        }
    }
    Ok(())
}

fn validate_camera(
    source: &str,
    object: &SceneObject,
    camera: &CameraComponent,
) -> Result<(), SceneError> {
    if !camera.fov_deg.is_finite() || camera.fov_deg <= 0.0 || camera.fov_deg >= 180.0 {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "camera.fov_deg должен быть конечным и находиться между 0 и 180",
        ));
    }
    if !camera.near.is_finite()
        || !camera.far.is_finite()
        || camera.near <= 0.0
        || camera.far <= camera.near
    {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "camera требует конечные параметры 0 < near < far",
        ));
    }
    Ok(())
}

fn validate_light(
    source: &str,
    object: &SceneObject,
    light: &DirectionalLightComponent,
) -> Result<(), SceneError> {
    if !light
        .color
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "directional_light.color должен содержать конечные компоненты >= 0",
        ));
    }
    if !light
        .ambient
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "directional_light.ambient должен содержать конечные компоненты >= 0",
        ));
    }
    if !light.shadow_center.into_iter().all(f32::is_finite)
        || light
            .shadow_radius
            .is_some_and(|radius| !radius.is_finite())
    {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "параметры тени должны быть конечными",
        ));
    }
    if light.shadow_radius.is_some_and(|radius| radius <= 0.0)
        || (light.shadows && light.shadow_radius.is_none())
    {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "shadow_radius должен быть > 0 и обязателен при shadows = true",
        ));
    }
    Ok(())
}

fn validate_params(
    source: &str,
    object: &SceneObject,
    params: &BTreeMap<String, ParamValue>,
) -> Result<(), SceneError> {
    fn finite(value: &ParamValue) -> bool {
        match value {
            ParamValue::Float(value) => value.is_finite(),
            ParamValue::Array(values) => values.iter().all(finite),
            _ => true,
        }
    }
    if params.values().all(finite) {
        Ok(())
    } else {
        Err(SceneError::new(
            source,
            Some(&object.id),
            "параметры скриптов должны содержать только конечные числа",
        ))
    }
}

fn validate_raw_virtual_paths(value: &toml::Value, source: &str) -> Result<(), SceneError> {
    let mut paths = Vec::new();
    if let Some(assets) = value.get("assets").and_then(toml::Value::as_array) {
        paths.extend(assets.iter().filter_map(|asset| {
            asset
                .get("path")
                .and_then(toml::Value::as_str)
                .map(|path| (None, "assets.path", path))
        }));
    }
    let Some(objects) = value.get("objects").and_then(toml::Value::as_array) else {
        return validate_raw_paths(paths, source);
    };
    for object in objects {
        let object_id = object.get("id").and_then(toml::Value::as_str);
        if let Some(renderer) = object.get("mesh_renderer").and_then(toml::Value::as_table) {
            paths.extend(["model", "texture"].into_iter().filter_map(|key| {
                renderer
                    .get(key)
                    .and_then(toml::Value::as_str)
                    .map(|path| (object_id, key, path))
            }));
        }
        if let Some(scripts) = object.get("lite_scripts").and_then(toml::Value::as_array) {
            paths.extend(scripts.iter().filter_map(|script| {
                script
                    .get("path")
                    .and_then(toml::Value::as_str)
                    .map(|path| (object_id, "lite_script.path", path))
            }));
        }
    }
    validate_raw_paths(paths, source)
}

fn validate_raw_paths(
    paths: Vec<(Option<&str>, &str, &str)>,
    source: &str,
) -> Result<(), SceneError> {
    for (object_id, field, path) in paths {
        if VirtualPath::parse(path).is_err() {
            let path = path.strip_prefix('/').unwrap_or(path);
            return Err(SceneError::new(
                source,
                object_id,
                format!("{field} /{path}: некорректный virtual path"),
            ));
        }
    }
    Ok(())
}

fn require_asset_path(
    source: &str,
    object: &SceneObject,
    assets: &HashSet<VirtualPath>,
    path: &VirtualPath,
    kind: &str,
) -> Result<(), SceneError> {
    if assets.contains(path) {
        Ok(())
    } else {
        Err(SceneError::new(
            source,
            Some(&object.id),
            format!("{kind} /{} отсутствует в [[assets]]", path.as_str()),
        ))
    }
}

fn require_extension(
    source: &str,
    object: &SceneObject,
    path: &VirtualPath,
    extension: &str,
    kind: &str,
) -> Result<(), SceneError> {
    let matches = Path::new(path.as_str())
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(extension));
    if matches {
        Ok(())
    } else {
        Err(SceneError::new(
            source,
            Some(&object.id),
            format!(
                "{kind} /{} должен иметь расширение .{extension}",
                path.as_str()
            ),
        ))
    }
}

fn valid_ascii_name(value: &str, hyphen: bool) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || (hyphen && byte == b'-'))
}

fn zero3() -> [f32; 3] {
    [0.0; 3]
}

fn one3() -> [f32; 3] {
    [1.0; 3]
}

fn default_ambient() -> [f32; 3] {
    [0.15; 3]
}

fn validated_world_matrices(
    document: &SceneDocument,
    indices: &HashMap<&str, usize>,
) -> Result<Vec<glam::Mat4>, SceneError> {
    let transforms = document
        .objects
        .iter()
        .map(|object| LocalTransform::from(object.transform))
        .collect::<Vec<_>>();
    let parents = document
        .objects
        .iter()
        .map(|object| {
            object
                .parent
                .as_ref()
                .map(|parent| indices[parent.as_str()])
        })
        .collect::<Vec<_>>();
    let object_ids = document
        .objects
        .iter()
        .map(|object| object.id.as_str())
        .collect::<Vec<_>>();
    calculate_world_matrices(&transforms, &parents, &object_ids, document.source_label())
        .map(|(matrices, _)| matrices)
}

pub(crate) fn camera_from_matrix(
    matrix: glam::Mat4,
    settings: CameraComponent,
) -> Option<crate::camera::Camera> {
    use glam::Vec3;

    let position = matrix.transform_point3(Vec3::ZERO);
    let forward = matrix.transform_vector3(Vec3::NEG_Z);
    let up = matrix.transform_vector3(Vec3::Y);
    let forward_length = forward.length();
    let up_length = up.length();
    if !position.is_finite()
        || !forward.is_finite()
        || !up.is_finite()
        || !forward_length.is_finite()
        || forward_length <= 0.0
        || !up_length.is_finite()
        || up_length <= 0.0
    {
        return None;
    }
    let forward = forward / forward_length;
    let up = up / up_length;
    let cross_length = forward.cross(up).length();
    if !cross_length.is_finite() || cross_length < 1e-4 {
        return None;
    }
    let target = position + forward * position.length().max(1.0);
    if !target.is_finite() {
        return None;
    }
    Some(crate::camera::Camera {
        position,
        target,
        up,
        fov_narrow: settings.fov_deg.to_radians(),
        near: settings.near,
        far: settings.far,
    })
}

pub(crate) fn light_direction_from_matrix(matrix: glam::Mat4) -> Option<[f32; 3]> {
    let direction = matrix.transform_vector3(glam::Vec3::NEG_Z);
    let length = direction.length();
    if !direction.is_finite() || !length.is_finite() || length == 0.0 {
        None
    } else {
        Some((direction / length).to_array())
    }
}

mod virtual_path_serde {
    use engine_files::VirtualPath;
    use serde::{Deserialize, Deserializer, Serializer};

    fn parse<E>(value: String) -> Result<VirtualPath, E>
    where
        E: serde::de::Error,
    {
        let display_path = format!("/{}", value.trim_start_matches('/'));
        VirtualPath::parse(&value)
            .map_err(|_| E::custom(format!("некорректный virtual path {display_path}")))
    }

    pub fn serialize<S>(value: &VirtualPath, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("/{}", value.as_str()))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<VirtualPath, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        parse(value)
    }
}

mod option_virtual_path_serde {
    use engine_files::VirtualPath;
    use serde::{Deserialize, Deserializer, Serializer};

    fn parse<E>(value: String) -> Result<VirtualPath, E>
    where
        E: serde::de::Error,
    {
        let display_path = format!("/{}", value.trim_start_matches('/'));
        VirtualPath::parse(&value)
            .map_err(|_| E::custom(format!("некорректный virtual path {display_path}")))
    }

    pub fn serialize<S>(value: &Option<VirtualPath>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match value {
            Some(path) => serializer.serialize_some(&format!("/{}", path.as_str())),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<VirtualPath>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<String>::deserialize(deserializer)?
            .map(parse)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::{ParamValue, SceneDocument};

    const VALID: &str = r#"
[[objects]]
id = "camera"
[objects.camera]
fov_deg = 60.0
near = 0.1
far = 100.0
"#;

    fn parse(input: &str) -> Result<SceneDocument, super::SceneError> {
        SceneDocument::parse("scenes/test.rvescene", input)
    }

    fn error(input: &str, contains: &str) {
        let error = parse(input).unwrap_err();
        assert!(
            error.to_string().contains("scenes/test.rvescene"),
            "{error}"
        );
        assert!(error.to_string().contains(contains), "{error}");
    }

    #[test]
    fn valid_document_applies_defaults_and_round_trips() {
        let document = parse(VALID).unwrap();
        let camera = &document.objects[0];
        assert_eq!(camera.name.as_deref(), None);
        assert_eq!(camera.display_name(), "camera");
        assert_eq!(camera.transform.position, [0.0; 3]);
        assert_eq!(camera.transform.rotation, [0.0; 3]);
        assert_eq!(camera.transform.scale, [1.0; 3]);
        let serialized = document.to_toml().unwrap();
        assert!(parse(&serialized).is_ok(), "{serialized}");
    }

    #[test]
    fn integer_literals_are_accepted_for_vector_and_camera_float_fields() {
        let input = VALID
            .replace(
                "[objects.camera]",
                "[objects.transform]\nposition = [0, 1, 0]\n[objects.camera]",
            )
            .replace("fov_deg = 60.0", "fov_deg = 60");
        let document = parse(&input).unwrap();
        assert_eq!(document.objects[0].transform.position, [0.0, 1.0, 0.0]);
        assert_eq!(document.objects[0].camera.unwrap().fov_deg, 60.0);
    }

    #[test]
    fn unknown_fields_are_denied_at_every_schema_level() {
        let cases = [
            format!("unexpected = true\n{VALID}"),
            VALID.replace("id = \"camera\"", "id = \"camera\"\nunexpected = true"),
            format!("{VALID}\nunexpected = true"),
            format!("{VALID}\n[objects.transform]\nunexpected = true"),
            format!("{VALID}\n[[assets]]\npath = \"/a.fbx\"\nunexpected = true"),
            format!("{VALID}\n[[objects.lite_scripts]]\npath = \"/s.lua\"\nunexpected = true"),
            format!("{VALID}\n[[objects.heavy_scripts]]\nname = \"Spin\"\nunexpected = true"),
        ];
        for input in cases {
            assert!(
                parse(&input).is_err(),
                "accepted unknown field in:\n{input}"
            );
        }
    }

    #[test]
    fn object_ids_are_unique_and_match_the_ascii_contract() {
        let duplicate = format!("{VALID}\n[[objects]]\nid = \"camera\"\n");
        error(&duplicate, "id объекта уже используется");
        for id in ["", "bad id", "a/b", "é", &"x".repeat(65)] {
            let input = VALID.replace("camera", id);
            assert!(parse(&input).is_err(), "accepted id {id:?}");
        }
    }

    #[test]
    fn parent_must_exist_and_hierarchy_must_be_acyclic() {
        let missing = VALID.replace("id = \"camera\"", "id = \"camera\"\nparent = \"lost\"");
        error(&missing, "родитель \"lost\" не существует");
        let cycle =
            "[[objects]]\nid = \"camera\"\nparent = \"loop\"\n[objects.camera]\nfov_deg=60.0\nnear=0.1\nfar=10.0\n[[objects]]\nid = \"loop\"\nparent = \"camera\"\n"
                .to_owned();
        error(&cycle, "цикл");
    }

    #[test]
    fn camera_basis_rejects_collinearity_after_parent_scale_and_rotation() {
        let scene = r#"
[[objects]]
id="camera"
parent="rig"
[objects.transform]
rotation=[45.0,0.0,0.0]
[objects.camera]
fov_deg=60.0
near=0.1
far=10.0
[[objects]]
id="rig"
[objects.transform]
scale=[1.0,1.0,0.0]
"#;
        let error = parse(scene).unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("camera"));
        assert!(error.message.contains("неколлинеарными"));
    }

    #[test]
    fn asset_paths_are_unique_and_archive_names_are_restricted() {
        let duplicate = format!("[[assets]]\npath=\"/a.fbx\"\n[[assets]]\npath=\"a.fbx\"\n{VALID}");
        error(&duplicate, "повторяется путь ресурса");
        for archive in [
            "base",
            "Base",
            "shared",
            "SHARED",
            "bad name",
            "",
            &"a".repeat(65),
        ] {
            let input = format!("[[assets]]\npath=\"/a.fbx\"\narchive={archive:?}\n{VALID}");
            assert!(parse(&input).is_err(), "accepted archive {archive:?}");
        }
        let accepted = format!("[[assets]]\npath=\"/a.fbx\"\narchive=\"weapons_2-X\"\n{VALID}");
        assert!(parse(&accepted).is_ok());
    }

    #[test]
    fn scene_requires_exactly_one_camera_and_at_most_one_light() {
        assert!(parse("[[objects]]\nid=\"empty\"\n").is_err());
        let two_cameras = format!(
            "{VALID}\n[[objects]]\nid=\"second\"\n[objects.camera]\nfov_deg=60.0\nnear=0.1\nfar=20.0\n"
        );
        assert!(parse(&two_cameras).is_err());
        let two_lights = format!(
            "{VALID}\n[[objects]]\nid=\"sun1\"\n[objects.directional_light]\n[[objects]]\nid=\"sun2\"\n[objects.directional_light]\n"
        );
        assert!(parse(&two_lights).is_err());
    }

    #[test]
    fn component_paths_must_be_assets_with_expected_case_insensitive_extensions() {
        let missing = format!(
            "{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.fbx\"\n"
        );
        error(&missing, "отсутствует в [[assets]]");
        let wrong_extension = format!(
            "[[assets]]\npath=\"/models/a.obj\"\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.obj\"\n"
        );
        error(&wrong_extension, "расширение .fbx");
        let wrong_texture = format!(
            "[[assets]]\npath=\"/models/a.fbx\"\n[[assets]]\npath=\"/textures/a.jpg\"\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.fbx\"\ntexture=\"/textures/a.jpg\"\n"
        );
        error(&wrong_texture, "расширение .png");
        let wrong_script = format!(
            "[[assets]]\npath=\"/scripts/a.txt\"\n{VALID}\n[[objects.lite_scripts]]\npath=\"/scripts/a.txt\"\n"
        );
        error(&wrong_script, "расширение .lua");
        let good = format!(
            "[[assets]]\npath=\"/models/A.FBX\"\n[[assets]]\npath=\"/textures/A.PNG\"\n[[assets]]\npath=\"/scripts/a.LUA\"\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/A.FBX\"\ntexture=\"/textures/A.PNG\"\n[[objects.lite_scripts]]\npath=\"/scripts/a.LUA\"\n"
        );
        assert!(parse(&good).is_ok());
    }

    #[test]
    fn transform_camera_light_and_parameter_numbers_must_be_finite() {
        for values in ["nan, 0.0, 0.0", "inf, 0.0, 0.0", "-inf,0.0,0.0"] {
            let input = VALID.replace(
                "id = \"camera\"",
                &format!("id = \"camera\"\n[objects.transform]\nposition=[{values}]"),
            );
            assert!(parse(&input).is_err(), "accepted transform {values}");
        }
        let camera_nan = VALID.replace("fov_deg = 60.0", "fov_deg = nan");
        assert!(parse(&camera_nan).is_err());
        let light_nan = format!(
            "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\ncolor=[nan,1.0,1.0]\n"
        );
        assert!(parse(&light_nan).is_err());
        let shadow_center_nan = format!(
            "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\nshadow_center=[0.0,inf,0.0]\n"
        );
        assert!(parse(&shadow_center_nan).is_err());
        let param_inf = format!(
            "{VALID}\n[[objects.lite_scripts]]\npath=\"/s.lua\"\nparams={{speed=inf}}\n[[assets]]\npath=\"/s.lua\"\n"
        );
        assert!(parse(&param_inf).is_err());
    }

    #[test]
    fn fov_and_clip_planes_are_validated() {
        for fov in ["0.0", "180.0", "nan", "inf"] {
            let input = VALID.replace("60.0", fov);
            assert!(parse(&input).is_err(), "accepted fov {fov}");
        }
        for (near, far) in [
            ("0.0", "100.0"),
            ("2.0", "2.0"),
            ("3.0", "2.0"),
            ("nan", "10.0"),
            ("0.1", "inf"),
        ] {
            let input = VALID
                .replace("near = 0.1", &format!("near = {near}"))
                .replace("far = 100.0", &format!("far = {far}"));
            assert!(parse(&input).is_err(), "accepted near/far {near}/{far}");
        }
    }

    #[test]
    fn light_defaults_shadow_requirements_and_nonnegative_colors_are_checked() {
        let no_shadow_radius = format!(
            "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\nshadows=true\n"
        );
        assert!(parse(&no_shadow_radius).is_err());
        for radius in ["0.0", "-1.0", "nan", "inf"] {
            let input = format!(
                "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\nshadows=true\nshadow_radius={radius}\n"
            );
            assert!(parse(&input).is_err(), "accepted radius {radius}");
        }
        for radius in ["0.0", "-1.0"] {
            let input = format!(
                "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\nshadows=false\nshadow_radius={radius}\n"
            );
            assert!(
                parse(&input).is_err(),
                "accepted shadow_radius {radius} without shadows"
            );
        }
        let negative_color = format!(
            "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\ncolor=[1.0,-1.0,1.0]\n"
        );
        assert!(parse(&negative_color).is_err());
        let valid = parse(&format!(
            "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\n"
        ))
        .unwrap();
        let light = valid.objects[1].directional_light.unwrap();
        assert_eq!(light.color, [1.0; 3]);
        assert_eq!(light.ambient, [0.15; 3]);
        assert!(!light.shadows);
        assert_eq!(light.shadow_center, [0.0; 3]);
    }

    #[test]
    fn script_names_and_parameter_values_follow_their_schema() {
        let params = format!(
            "{VALID}\n[[objects.lite_scripts]]\npath=\"/s.lua\"\nparams={{bool=true, int=-2, float=1.5, text=\"x\", array=[1, false, \"y\"]}}\n[[assets]]\npath=\"/s.lua\"\n[[objects.heavy_scripts]]\nname=\"Spin_2\"\nparams={{items=[1,2]}}\n"
        );
        let document = parse(&params).unwrap();
        assert_eq!(
            document.objects[0].lite_scripts[0].params["array"],
            ParamValue::Array(vec![
                ParamValue::Int(1),
                ParamValue::Bool(false),
                ParamValue::String("y".into())
            ])
        );
        let serialized = document.to_toml().unwrap();
        let round_trip = parse(&serialized).unwrap();
        assert_eq!(
            round_trip.objects[0].lite_scripts[0].params,
            document.objects[0].lite_scripts[0].params,
            "{serialized}"
        );
        for name in ["", "bad-name", "bad name", &"x".repeat(65)] {
            let input = format!("{VALID}\n[[objects.heavy_scripts]]\nname={name:?}\n");
            assert!(
                parse(&input).is_err(),
                "accepted heavy script name {name:?}"
            );
        }
        let nested_table = format!(
            "{VALID}\n[[objects.lite_scripts]]\npath=\"/s.lua\"\nparams={{nested={{value=1}}}}\n[[assets]]\npath=\"/s.lua\"\n"
        );
        let nested_table_error = parse(&nested_table).unwrap_err();
        assert!(
            nested_table_error
                .message
                .contains("вложенные таблицы параметров")
        );
        let date = format!(
            "{VALID}\n[[objects.lite_scripts]]\npath=\"/s.lua\"\nparams={{date=1979-05-27}}\n[[assets]]\npath=\"/s.lua\"\n"
        );
        let date_error = parse(&date).unwrap_err();
        assert!(
            date_error.message.contains("даты/время TOML"),
            "{date_error}"
        );
    }

    #[test]
    fn path_errors_include_source_and_object_id() {
        let input = format!(
            "{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/missing.fbx\"\n"
        );
        let error = parse(&input).unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("mesh"));
        assert!(error.to_string().contains("scenes/test.rvescene"));
        assert!(error.to_string().contains("/missing.fbx"));
        assert!(!error.to_string().contains("VirtualPath"));

        let invalid_path = format!(
            "{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/../outside.fbx\"\n"
        );
        let error = parse(&invalid_path).unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("mesh"));
        assert!(error.to_string().contains("/../outside.fbx"));
        assert!(!error.to_string().contains("\"/../outside.fbx\""));

        let invalid_asset_path = format!("[[assets]]\npath=\"assets/../bad.fbx\"\n{VALID}");
        let error = parse(&invalid_asset_path).unwrap_err();
        assert!(error.to_string().contains("/assets/../bad.fbx"));
        assert!(!error.to_string().contains("VirtualPath"));
    }

    #[test]
    fn direct_virtual_path_deserialization_uses_slash_paths_and_russian_errors() {
        let asset_error = toml::from_str::<super::SceneAsset>("path = \"a/../b\"")
            .unwrap_err()
            .to_string();
        assert!(
            asset_error.contains("некорректный virtual path /a/../b"),
            "{asset_error}"
        );
        assert!(
            !asset_error.contains("invalid virtual path"),
            "{asset_error}"
        );

        let renderer_error = toml::from_str::<super::MeshRenderer>(
            "model = \"/models/a.fbx\"\ntexture = \"textures/../a.png\"",
        )
        .unwrap_err()
        .to_string();
        assert!(
            renderer_error.contains("некорректный virtual path /textures/../a.png"),
            "{renderer_error}"
        );
        assert!(
            !renderer_error.contains("invalid virtual path"),
            "{renderer_error}"
        );
    }
}
