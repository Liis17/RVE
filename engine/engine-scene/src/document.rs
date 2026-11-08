//! TOML-документы сцен и их проверенная схема, независимая от renderer.

use crate::CameraSettings;
use crate::animation::{AnimatorComponent, validate_animator};
use crate::audio::{AudioSourceComponent, validate_audio_source};
use crate::particles::{ParticleEmitterComponent, validate_particle_emitter};
use crate::prefab::{PREFAB_EXTENSION, PrefabInstance};
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
    /// Экземпляры префабов; см. [`crate::prefab`]. Пустой список не сериализуется,
    /// поэтому сцены без префабов сохраняются как прежде.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instances: Vec<PrefabInstance>,
    #[serde(skip)]
    pub(crate) source_label: String,
    #[serde(skip)]
    pub(crate) kind: DocumentKind,
    /// Результат [`crate::prefab::expand`]: id объектов могут содержать точки.
    #[serde(skip)]
    pub(crate) expanded: bool,
}

/// Вид документа: сцена или префаб `.rveprefab`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DocumentKind {
    #[default]
    Scene,
    Prefab,
}

impl SceneDocument {
    /// Разбирает TOML и проверяет идентификаторы, ссылки, числа и мировые базисы.
    pub fn parse(source_label: impl Into<String>, input: &str) -> Result<Self, SceneError> {
        Self::parse_kind(source_label.into(), input, DocumentKind::Scene)
    }

    /// Разбирает префаб `.rveprefab`: схема та же, что у сцены, но ровно один корень
    /// (объект или экземпляр) и камера не обязательна.
    pub fn parse_prefab(source_label: impl Into<String>, input: &str) -> Result<Self, SceneError> {
        Self::parse_kind(source_label.into(), input, DocumentKind::Prefab)
    }

    fn parse_kind(
        source_label: String,
        input: &str,
        kind: DocumentKind,
    ) -> Result<Self, SceneError> {
        let value: toml::Value = toml::from_str(input)
            .map_err(|error| SceneError::new(&source_label, None, error.to_string()))?;
        validate_raw_virtual_paths(&value, &source_label)?;
        let mut document: Self = toml::from_str(input)
            .map_err(|error| SceneError::new(&source_label, None, error.to_string()))?;
        document.source_label = source_label;
        document.kind = kind;
        document.validate()?;
        Ok(document)
    }

    /// Сериализует документ в форматированный TOML.
    pub fn to_toml(&self) -> Result<String, SceneError> {
        if self.expanded {
            return Err(SceneError::new(
                &self.source_label,
                None,
                "раскрытый документ префабов не сохраняется; сохраняйте авторский документ",
            ));
        }
        toml::to_string_pretty(self)
            .map_err(|error| SceneError::new(&self.source_label, None, error.to_string()))
    }

    /// Документ разобран как префаб `.rveprefab`.
    pub fn is_prefab(&self) -> bool {
        self.kind == DocumentKind::Prefab
    }

    /// Документ получен из [`crate::prefab::expand`] и не содержит экземпляров.
    pub fn is_expanded(&self) -> bool {
        self.expanded
    }

    /// Тот же документ как префаб; editor создаёт так `.rveprefab` из поддерева сцены.
    pub fn into_prefab(mut self, source_label: impl Into<String>) -> Self {
        self.kind = DocumentKind::Prefab;
        self.source_label = source_label.into();
        self
    }

    /// Метка источника, переданная в [`SceneDocument::parse`].
    pub fn source_label(&self) -> &str {
        &self.source_label
    }

    /// Проверяет идентификаторы, ссылки, числа и мировые базисы уже изменённого документа.
    ///
    /// Документ с `instances` проверяется только локально: формат, ресурсы и overrides.
    /// Существование родителей, циклы, камеры, свет и базисы зависят от содержимого
    /// префабов и проверяются на результате [`crate::prefab::expand`].
    pub fn validate(&self) -> Result<(), SceneError> {
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
            if let Some(max_instances) = asset.max_instances {
                let is_prefab = Path::new(asset.path.as_str())
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|value| value.eq_ignore_ascii_case(PREFAB_EXTENSION));
                if !is_prefab || max_instances == 0 {
                    return Err(SceneError::new(
                        source,
                        None,
                        format!(
                            "max_instances ресурса /{} допустим только для .{PREFAB_EXTENSION} и должен быть больше 0",
                            asset.path.as_str()
                        ),
                    ));
                }
            }
        }

        let mut object_indices = HashMap::with_capacity(self.objects.len());
        for (index, object) in self.objects.iter().enumerate() {
            if self.expanded {
                if !crate::prefab::valid_expanded_id(&object.id) {
                    return Err(SceneError::new(
                        source,
                        Some(&object.id),
                        "раскрытый id должен состоять из сегментов [A-Za-z0-9_-]{1,64} через точку и быть не длиннее 255 байт",
                    ));
                }
            } else if !valid_ascii_name(&object.id, true) {
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
        crate::prefab::validate_instances(self, &asset_paths, &object_indices)?;
        let local_only = !self.instances.is_empty();
        if self.kind == DocumentKind::Prefab {
            let roots = self
                .objects
                .iter()
                .map(|object| &object.parent)
                .chain(self.instances.iter().map(|instance| &instance.parent))
                .filter(|parent| parent.is_none())
                .count();
            if roots != 1 {
                return Err(SceneError::new(
                    source,
                    None,
                    format!(
                        "префаб должен иметь ровно один корневой объект или экземпляр, найдено {roots}"
                    ),
                ));
            }
        }

        let mut camera_count = 0;
        let mut active_camera_count = 0;
        let mut light_count = 0;
        let mut environment_count = 0;
        for object in &self.objects {
            if let Some(parent) = &object.parent
                && !local_only
                && !object_indices.contains_key(parent.as_str())
            {
                return Err(SceneError::new(
                    source,
                    Some(&object.id),
                    format!("родитель {parent:?} не существует"),
                ));
            }
            validate_transform(source, object)?;
            validate_physics_components(source, object)?;
            if let Some(camera) = &object.camera {
                camera_count += 1;
                active_camera_count += usize::from(camera.active);
                if let Some(settings) = &camera.settings {
                    require_asset_path(
                        source,
                        &object.id,
                        &asset_paths,
                        settings,
                        "настройки камеры",
                    )?;
                    require_extension(
                        source,
                        &object.id,
                        settings,
                        "camerasettings",
                        "настройки камеры",
                    )?;
                }
                if let Some(post_process) = &camera.post_process {
                    require_asset_path(
                        source,
                        &object.id,
                        &asset_paths,
                        post_process,
                        "постобработка камеры",
                    )?;
                    require_extension(
                        source,
                        &object.id,
                        post_process,
                        "postprocess",
                        "постобработка камеры",
                    )?;
                }
            }
            if let Some(light) = &object.directional_light {
                light_count += 1;
                validate_light(source, object, light)?;
            }
            if let Some(light) = &object.point_light {
                validate_point_light(source, object, light)?;
            }
            if let Some(light) = &object.spot_light {
                validate_spot_light(source, object, light)?;
            }
            if let Some(environment) = &object.environment {
                environment_count += 1;
                validate_environment(source, &object.id, &asset_paths, environment)?;
            }
            if let Some(audio) = &object.audio_source {
                validate_audio_source(source, &object.id, &asset_paths, audio)?;
            }
            if let Some(animator) = &object.animator {
                validate_animator(source, object, &asset_paths, animator)?;
            }
            if let Some(emitter) = &object.particle_emitter {
                validate_particle_emitter(source, &object.id, &asset_paths, emitter)?;
            }
            if let Some(mesh) = &object.mesh_renderer {
                require_asset_path(source, &object.id, &asset_paths, &mesh.model, "модель")?;
                require_extension(source, &object.id, &mesh.model, "fbx", "модель")?;
                if let Some(texture) = &mesh.texture {
                    require_asset_path(source, &object.id, &asset_paths, texture, "текстура")?;
                    require_extension(source, &object.id, texture, "png", "текстура")?;
                }
                if let Some(material) = &mesh.material {
                    validate_material_override(source, &object.id, &asset_paths, material)?;
                }
            }
            if let Some(group) = &object.lod_group {
                if object.mesh_renderer.is_none() {
                    return Err(SceneError::new(
                        source,
                        Some(&object.id),
                        "lod_group требует mesh_renderer",
                    ));
                }
                if group.levels.is_empty() || group.levels.len() > MAX_LOD_LEVELS {
                    return Err(SceneError::new(
                        source,
                        Some(&object.id),
                        format!("lod_group должен содержать от 1 до {MAX_LOD_LEVELS} уровней"),
                    ));
                }
                let mut previous_distance = 0.0;
                for (index, level) in group.levels.iter().enumerate() {
                    if !level.distance.is_finite()
                        || level.distance <= 0.0
                        || level.distance <= previous_distance
                    {
                        return Err(SceneError::new(
                            source,
                            Some(&object.id),
                            format!(
                                "lod_group.levels[{index}].distance должен быть конечным, положительным и строго возрастать"
                            ),
                        ));
                    }
                    previous_distance = level.distance;
                    let model =
                        parse_lod_path(source, &object.id, &level.model, "lod_group.levels.model")?;
                    require_asset_path(source, &object.id, &asset_paths, &model, "модель LOD")?;
                    require_extension(source, &object.id, &model, "fbx", "модель LOD")?;
                    if let Some(texture) = &level.texture {
                        let texture = parse_lod_path(
                            source,
                            &object.id,
                            texture,
                            "lod_group.levels.texture",
                        )?;
                        require_asset_path(
                            source,
                            &object.id,
                            &asset_paths,
                            &texture,
                            "текстура LOD",
                        )?;
                        require_extension(source, &object.id, &texture, "png", "текстура LOD")?;
                    }
                }
                if let Some(cull_distance) = group.cull_distance
                    && (!cull_distance.is_finite() || cull_distance <= previous_distance)
                {
                    return Err(SceneError::new(
                        source,
                        Some(&object.id),
                        "lod_group.cull_distance должен быть конечным и больше последнего distance",
                    ));
                }
            }
            for script in &object.lite_scripts {
                require_asset_path(
                    source,
                    &object.id,
                    &asset_paths,
                    &script.path,
                    "lite-скрипт",
                )?;
                require_extension(source, &object.id, &script.path, "lua", "lite-скрипт")?;
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

        if camera_count == 0 && self.kind == DocumentKind::Scene && !local_only {
            return Err(SceneError::new(
                source,
                None,
                "сцена должна содержать хотя бы одну камеру",
            ));
        }
        if active_camera_count > 1 {
            return Err(SceneError::new(
                source,
                None,
                "в сцене допускается не более одной активной камеры",
            ));
        }
        if camera_count > 1 && active_camera_count == 0 {
            return Err(SceneError::new(
                source,
                None,
                "при нескольких камерах одна должна иметь active = true",
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
        if environment_count > 1 {
            return Err(SceneError::new(
                source,
                None,
                format!(
                    "в сцене допускается не более одного environment, найдено {environment_count}"
                ),
            ));
        }
        if local_only {
            return Ok(());
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
                && camera_from_matrix(matrix, CameraSettings::default()).is_none()
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
            if object.spot_light.is_some() && light_direction_from_matrix(matrix).is_none() {
                return Err(SceneError::new(
                    source,
                    Some(&object.id),
                    "мировое направление spot_light должно быть конечным и ненулевым",
                ));
            }
        }
        Ok(())
    }
}

/// Ресурс, объявленный для включения в архив сцены.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneAsset {
    #[serde(with = "virtual_path_serde")]
    pub path: VirtualPath,
    #[serde(default)]
    pub archive: Option<String>,
    /// Лимит живых spawn-экземпляров; допустим только для `.rveprefab` и больше нуля.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_instances: Option<u32>,
}

/// Объект сцены в порядке документа.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lod_group: Option<LodGroup>,
    #[serde(default)]
    pub directional_light: Option<DirectionalLightComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_light: Option<PointLightComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot_light: Option<SpotLightComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collider: Option<ColliderComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rigid_body: Option<RigidBodyComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character_controller: Option<CharacterControllerComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_source: Option<AudioSourceComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animator: Option<AnimatorComponent>,
    pub particle_emitter: Option<ParticleEmitterComponent>,
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

/// Shape used by a physics collider.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColliderShape {
    #[default]
    Box,
    Sphere,
    Capsule,
    ConvexMesh,
    TriangleMesh,
}

/// Collider geometry and contact material settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColliderComponent {
    pub shape: ColliderShape,
    pub half_extents: [f32; 3],
    pub radius: f32,
    pub height: f32,
    pub offset: [f32; 3],
    pub trigger: bool,
    pub friction: f32,
    pub restitution: f32,
}

impl Default for ColliderComponent {
    fn default() -> Self {
        Self {
            shape: ColliderShape::Box,
            half_extents: [0.5; 3],
            radius: 0.5,
            height: 1.0,
            offset: [0.0; 3],
            trigger: false,
            friction: 0.5,
            restitution: 0.0,
        }
    }
}

/// Motion type of a rigid body.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RigidBodyKind {
    Static,
    Kinematic,
    #[default]
    Dynamic,
}

/// Body settings attached to an object with a collider.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RigidBodyComponent {
    pub kind: RigidBodyKind,
    pub mass: f32,
    pub linear_damping: f32,
    pub angular_damping: f32,
    pub gravity_scale: f32,
    pub lock_rotation: bool,
}

impl Default for RigidBodyComponent {
    fn default() -> Self {
        Self {
            kind: RigidBodyKind::Dynamic,
            mass: 1.0,
            linear_damping: 0.0,
            angular_damping: 0.05,
            gravity_scale: 1.0,
            lock_rotation: false,
        }
    }
}

/// Character capsule movement settings.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CharacterControllerComponent {
    pub radius: f32,
    pub height: f32,
    pub step_height: f32,
    pub max_slope_deg: f32,
    pub snap_distance: f32,
    pub skin: f32,
    pub push_strength: f32,
}

impl Default for CharacterControllerComponent {
    fn default() -> Self {
        Self {
            radius: 0.35,
            height: 1.1,
            step_height: 0.3,
            max_slope_deg: 45.0,
            snap_distance: 0.2,
            skin: 0.02,
            push_strength: 1.0,
        }
    }
}

/// Локальная трансформация документа; углы вращения хранятся в градусах.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
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

/// Ссылка на настройки камеры и выбор активной камеры сцены.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraComponent {
    #[serde(default, with = "option_virtual_path_serde")]
    pub settings: Option<VirtualPath>,
    /// Файл `.postprocess`; без него используется постобработка по умолчанию.
    #[serde(default, with = "option_virtual_path_serde")]
    pub post_process: Option<VirtualPath>,
    pub active: bool,
    /// Состояние курсора при старте сцены; действует, только если камера активна.
    #[serde(default, skip_serializing_if = "CameraCursor::is_default")]
    pub cursor: CameraCursor,
}

/// Начальное состояние курсора, которое камера задаёт при старте сцены.
///
/// Флаги независимы: `hide_on_start` только скрывает курсор, `lock_on_start` только
/// удерживает его в окне. Значения по умолчанию не записываются в сцену.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraCursor {
    pub hide_on_start: bool,
    pub lock_on_start: bool,
}

impl CameraCursor {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Экземпляр FBX, необязательная замена base-color PNG и override материала.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshRenderer {
    #[serde(with = "virtual_path_serde")]
    pub model: VirtualPath,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_virtual_path_serde"
    )]
    pub texture: Option<VirtualPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<MaterialOverride>,
}

/// Модель, геометрия которой нужна мешевому коллайдеру: `model` меш-рендерера, если форма
/// коллайдера — выпуклый или треугольный меш. Одинакова для сцены, префабов и редактора.
pub fn mesh_collider_model(
    collider: Option<ColliderComponent>,
    mesh: Option<&MeshRenderer>,
) -> Option<&str> {
    let collider = collider?;
    if matches!(
        collider.shape,
        ColliderShape::ConvexMesh | ColliderShape::TriangleMesh
    ) {
        Some(mesh?.model.as_str())
    } else {
        None
    }
}

pub const MAX_LOD_LEVELS: usize = 6;

/// Дополнительные уровни модели, выбираемые по расстоянию до объекта.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LodGroup {
    #[serde(default)]
    pub levels: Vec<LodLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cull_distance: Option<f32>,
}

impl LodGroup {
    /// Возвращает индекс уровня: 0 — `mesh_renderer`, 1.. — записи `levels`.
    pub fn level_for_distance(&self, distance: f32) -> Option<usize> {
        if self.cull_distance.is_some_and(|limit| distance >= limit) {
            return None;
        }
        Some(
            self.levels
                .iter()
                .take_while(|level| distance >= level.distance)
                .count(),
        )
    }
}

/// Модель замены и её порог расстояния; `texture = None` наследует текстуру.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LodLevel {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub texture: Option<String>,
    pub distance: f32,
}

/// Режим обработки альфа-канала в переопределении материала.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialAlphaMode {
    Opaque,
    Mask,
    Blend,
}

/// Необязательные замены факторов, текстур и двусторонности FBX-материала.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaterialOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_color: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metallic: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roughness: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emissive: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opacity: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alpha_mode: Option<MaterialAlphaMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alpha_cutoff: Option<f32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_virtual_path_serde"
    )]
    pub metallic_roughness_texture: Option<VirtualPath>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_virtual_path_serde"
    )]
    pub normal_texture: Option<VirtualPath>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_virtual_path_serde"
    )]
    pub emissive_texture: Option<VirtualPath>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_virtual_path_serde"
    )]
    pub occlusion_texture: Option<VirtualPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normal_scale: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occlusion_strength: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub double_sided: Option<bool>,
}

/// Цвет направленного света, ambient и необязательная проекция теней.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectionalLightComponent {
    #[serde(default = "one3")]
    pub color: [f32; 3],
    #[serde(default = "default_ambient")]
    pub ambient: [f32; 3],
    #[serde(default)]
    pub shadows: bool,
    #[serde(default = "default_shadow_distance")]
    pub shadow_distance: f32,
    #[serde(default = "default_shadow_cascades")]
    pub shadow_cascades: u8,
}

/// Локальный точечный источник света.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PointLightComponent {
    pub color: [f32; 3],
    pub intensity: f32,
    pub range: f32,
    /// Запрашивает шестислойные point shadows; при range ниже минимума теней источник остаётся освещённым без тени.
    pub shadows: bool,
}

impl Default for PointLightComponent {
    fn default() -> Self {
        Self {
            color: [1.0; 3],
            intensity: 1.0,
            range: 10.0,
            shadows: false,
        }
    }
}

/// Локальный конический источник света, направленный вдоль мирового −Z объекта.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpotLightComponent {
    pub color: [f32; 3],
    pub intensity: f32,
    pub range: f32,
    pub inner_angle_deg: f32,
    pub outer_angle_deg: f32,
    /// Запрашивает spot shadow; при range ниже минимума теней источник остаётся освещённым без тени.
    pub shadows: bool,
}

impl Default for SpotLightComponent {
    fn default() -> Self {
        Self {
            color: [1.0; 3],
            intensity: 1.0,
            range: 10.0,
            inner_angle_deg: 20.0,
            outer_angle_deg: 30.0,
            shadows: false,
        }
    }
}

/// Окружение сцены: Radiance HDR panorama или процедурное небо.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EnvironmentComponent {
    #[serde(with = "option_virtual_path_serde")]
    pub hdr: Option<VirtualPath>,
    pub procedural: Option<ProceduralSkyComponent>,
    pub intensity: f32,
    pub skybox: bool,
}

impl Default for EnvironmentComponent {
    fn default() -> Self {
        Self {
            hdr: None,
            procedural: None,
            intensity: 1.0,
            skybox: true,
        }
    }
}

/// Параметры процедурного неба в линейном RGB.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProceduralSkyComponent {
    pub zenith_color: [f32; 3],
    pub horizon_color: [f32; 3],
    pub ground_color: [f32; 3],
    pub sun_size_deg: f32,
    pub sun_intensity: f32,
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiteScript {
    #[serde(with = "virtual_path_serde")]
    pub path: VirtualPath,
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
}

/// Фабрика скрипта, зарегистрированная игровым крейтом.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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

fn validate_physics_components(source: &str, object: &SceneObject) -> Result<(), SceneError> {
    if let Some(collider) = &object.collider {
        if !collider
            .half_extents
            .into_iter()
            .chain([collider.radius, collider.height])
            .all(f32::is_finite)
        {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "collider dimensions должны содержать конечные числа",
            ));
        }
        if !collider.offset.into_iter().all(f32::is_finite) {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "collider.offset должен содержать конечные числа",
            ));
        }
        if !collider.friction.is_finite() || collider.friction < 0.0 {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "collider.friction должен быть конечным и >= 0",
            ));
        }
        if !collider.restitution.is_finite() || !(0.0..=1.0).contains(&collider.restitution) {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "collider.restitution должен быть конечным и находиться в диапазоне [0, 1]",
            ));
        }
        let valid_dimensions = match collider.shape {
            ColliderShape::Box => collider.half_extents.into_iter().all(|value| value > 0.0),
            ColliderShape::Sphere => collider.radius > 0.0,
            ColliderShape::Capsule => collider.radius > 0.0 && collider.height > 0.0,
            ColliderShape::ConvexMesh | ColliderShape::TriangleMesh => true,
        };
        if !valid_dimensions {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "collider dimensions должны быть > 0 для выбранной формы",
            ));
        }
        if matches!(
            collider.shape,
            ColliderShape::ConvexMesh | ColliderShape::TriangleMesh
        ) && object.mesh_renderer.is_none()
        {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "collider mesh shape требует mesh_renderer",
            ));
        }
    }

    if let Some(body) = &object.rigid_body {
        if object.collider.is_none() {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "rigid_body требует collider",
            ));
        }
        if !body.mass.is_finite()
            || body.mass <= 0.0
            || !body.linear_damping.is_finite()
            || body.linear_damping < 0.0
            || !body.angular_damping.is_finite()
            || body.angular_damping < 0.0
            || !body.gravity_scale.is_finite()
        {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "rigid_body mass должен быть > 0, damping >= 0, а все числа конечными",
            ));
        }
        if body.kind == RigidBodyKind::Dynamic
            && object
                .collider
                .is_some_and(|collider| collider.shape == ColliderShape::TriangleMesh)
        {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "dynamic rigid_body несовместим с triangle_mesh",
            ));
        }
        if body.kind == RigidBodyKind::Dynamic && object.parent.is_some() {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "dynamic rigid_body должен быть корневым объектом",
            ));
        }
    }

    if let Some(controller) = &object.character_controller {
        if object.collider.is_some() || object.rigid_body.is_some() {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "character_controller несовместим с collider и rigid_body",
            ));
        }
        if ![
            controller.radius,
            controller.height,
            controller.step_height,
            controller.max_slope_deg,
            controller.snap_distance,
            controller.skin,
            controller.push_strength,
        ]
        .into_iter()
        .all(f32::is_finite)
        {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "character_controller должен содержать конечные числа",
            ));
        }
        if controller.radius <= 0.0
            || controller.height <= 0.0
            || controller.step_height < 0.0
            || !(0.0 < controller.max_slope_deg && controller.max_slope_deg < 90.0)
            || controller.snap_distance < 0.0
            || controller.skin < 0.0
            || controller.push_strength < 0.0
        {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "character_controller требует положительные radius/height, slope в (0, 90) и остальные значения >= 0",
            ));
        }
        if object.parent.is_some() {
            return Err(SceneError::new(
                source,
                Some(&object.id),
                "character_controller должен быть корневым объектом",
            ));
        }
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
    if !light.shadow_distance.is_finite() || light.shadow_distance <= 0.0 {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "directional_light.shadow_distance должен быть конечным и > 0",
        ));
    }
    if !(1..=crate::light::MAX_SHADOW_CASCADES).contains(&light.shadow_cascades) {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "directional_light.shadow_cascades должен быть в диапазоне 1..=4",
        ));
    }
    Ok(())
}

fn validate_point_light(
    source: &str,
    object: &SceneObject,
    light: &PointLightComponent,
) -> Result<(), SceneError> {
    if !light
        .color
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "point_light.color должен содержать конечные компоненты >= 0",
        ));
    }
    if !light.intensity.is_finite() || light.intensity < 0.0 {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "point_light.intensity должен быть конечным и >= 0",
        ));
    }
    if !light.range.is_finite() || light.range <= 0.0 {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "point_light.range должен быть конечным и > 0",
        ));
    }
    Ok(())
}

fn validate_spot_light(
    source: &str,
    object: &SceneObject,
    light: &SpotLightComponent,
) -> Result<(), SceneError> {
    if !light
        .color
        .into_iter()
        .all(|value| value.is_finite() && value >= 0.0)
    {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "spot_light.color должен содержать конечные компоненты >= 0",
        ));
    }
    if !light.intensity.is_finite() || light.intensity < 0.0 {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "spot_light.intensity должен быть конечным и >= 0",
        ));
    }
    if !light.range.is_finite() || light.range <= 0.0 {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "spot_light.range должен быть конечным и > 0",
        ));
    }
    if !light.inner_angle_deg.is_finite()
        || !light.outer_angle_deg.is_finite()
        || light.inner_angle_deg <= 0.0
        || light.inner_angle_deg > light.outer_angle_deg
        || light.outer_angle_deg > 90.0
    {
        return Err(SceneError::new(
            source,
            Some(&object.id),
            "spot_light angles должны удовлетворять 0 < inner_angle_deg <= outer_angle_deg <= 90",
        ));
    }
    Ok(())
}

pub(crate) fn validate_environment(
    source: &str,
    object_id: &str,
    assets: &HashSet<VirtualPath>,
    environment: &EnvironmentComponent,
) -> Result<(), SceneError> {
    if environment.hdr.is_some() == environment.procedural.is_some() {
        return Err(SceneError::new(
            source,
            Some(object_id),
            "environment должен задавать ровно один источник: hdr или procedural",
        ));
    }
    if !environment.intensity.is_finite() || environment.intensity < 0.0 {
        return Err(SceneError::new(
            source,
            Some(object_id),
            "environment.intensity должен быть конечным и >= 0",
        ));
    }
    if let Some(path) = &environment.hdr {
        require_asset_path(source, object_id, assets, path, "HDR environment")?;
        require_extension(source, object_id, path, "hdr", "HDR environment")?;
    }
    if let Some(sky) = environment.procedural {
        if [sky.zenith_color, sky.horizon_color, sky.ground_color]
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite() || value < 0.0)
        {
            return Err(SceneError::new(
                source,
                Some(object_id),
                "environment.procedural цвета должны содержать конечные компоненты >= 0",
            ));
        }
        if !sky.sun_size_deg.is_finite()
            || !(0.0..=180.0).contains(&sky.sun_size_deg)
            || sky.sun_size_deg == 0.0
        {
            return Err(SceneError::new(
                source,
                Some(object_id),
                "environment.procedural.sun_size_deg должен быть конечным и в диапазоне (0, 180]",
            ));
        }
        if !sky.sun_intensity.is_finite() || sky.sun_intensity < 0.0 {
            return Err(SceneError::new(
                source,
                Some(object_id),
                "environment.procedural.sun_intensity должен быть конечным и >= 0",
            ));
        }
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
    if let Some(instances) = value.get("instances").and_then(toml::Value::as_array) {
        paths.extend(instances.iter().filter_map(|instance| {
            instance
                .get("prefab")
                .and_then(toml::Value::as_str)
                .map(|path| {
                    (
                        instance.get("id").and_then(toml::Value::as_str),
                        "prefab",
                        path,
                    )
                })
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
            if let Some(material) = renderer.get("material").and_then(toml::Value::as_table) {
                paths.extend(
                    [
                        "metallic_roughness_texture",
                        "normal_texture",
                        "emissive_texture",
                        "occlusion_texture",
                    ]
                    .into_iter()
                    .filter_map(|key| {
                        material
                            .get(key)
                            .and_then(toml::Value::as_str)
                            .map(|path| (object_id, key, path))
                    }),
                );
            }
        }
        if let Some(camera) = object.get("camera").and_then(toml::Value::as_table) {
            paths.extend(
                camera
                    .get("settings")
                    .and_then(toml::Value::as_str)
                    .map(|path| (object_id, "camera.settings", path)),
            );
            paths.extend(
                camera
                    .get("post_process")
                    .and_then(toml::Value::as_str)
                    .map(|path| (object_id, "camera.post_process", path)),
            );
        }
        if let Some(environment) = object.get("environment").and_then(toml::Value::as_table) {
            paths.extend(
                environment
                    .get("hdr")
                    .and_then(toml::Value::as_str)
                    .map(|path| (object_id, "environment.hdr", path)),
            );
        }
        if let Some(group) = object.get("lod_group").and_then(toml::Value::as_table)
            && let Some(levels) = group.get("levels").and_then(toml::Value::as_array)
        {
            for level in levels {
                if let Some(level) = level.as_table() {
                    paths.extend(["model", "texture"].into_iter().filter_map(|key| {
                        level.get(key).and_then(toml::Value::as_str).map(|path| {
                            (
                                object_id,
                                if key == "model" {
                                    "lod_group.levels.model"
                                } else {
                                    "lod_group.levels.texture"
                                },
                                path,
                            )
                        })
                    }));
                }
            }
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

pub(crate) fn require_asset_path(
    source: &str,
    object_id: &str,
    assets: &HashSet<VirtualPath>,
    path: &VirtualPath,
    kind: &str,
) -> Result<(), SceneError> {
    if assets.contains(path) {
        Ok(())
    } else {
        Err(SceneError::new(
            source,
            Some(object_id),
            format!("{kind} /{} отсутствует в [[assets]]", path.as_str()),
        ))
    }
}

fn parse_lod_path(
    source: &str,
    object_id: &str,
    value: &str,
    field: &str,
) -> Result<VirtualPath, SceneError> {
    VirtualPath::parse(value).map_err(|_| {
        let path = value.strip_prefix('/').unwrap_or(value);
        SceneError::new(
            source,
            Some(object_id),
            format!("{field} /{path}: некорректный virtual path"),
        )
    })
}

pub(crate) fn validate_material_override(
    source: &str,
    object_id: &str,
    assets: &HashSet<VirtualPath>,
    material: &MaterialOverride,
) -> Result<(), SceneError> {
    let invalid_factor = |field: &str| {
        SceneError::new(
            source,
            Some(object_id),
            format!("{field} должен быть конечным и находиться в диапазоне [0, 1]"),
        )
    };
    if material.base_color.is_some_and(|color| {
        color
            .into_iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    }) {
        return Err(invalid_factor("material.base_color"));
    }
    if material
        .metallic
        .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(invalid_factor("material.metallic"));
    }
    if material
        .roughness
        .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(invalid_factor("material.roughness"));
    }
    if material.emissive.is_some_and(|color| {
        color
            .into_iter()
            .any(|value| !value.is_finite() || value < 0.0)
    }) {
        return Err(SceneError::new(
            source,
            Some(object_id),
            "material.emissive должен содержать конечные неотрицательные значения",
        ));
    }
    for (field, value) in [
        ("material.opacity", material.opacity),
        ("material.occlusion_strength", material.occlusion_strength),
    ] {
        if value.is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value)) {
            return Err(invalid_factor(field));
        }
    }
    if material
        .alpha_cutoff
        .is_some_and(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    {
        return Err(invalid_factor("material.alpha_cutoff"));
    }
    if material.alpha_cutoff.is_some() && material.alpha_mode != Some(MaterialAlphaMode::Mask) {
        return Err(SceneError::new(
            source,
            Some(object_id),
            "material.alpha_cutoff разрешён только при alpha_mode = \"mask\"",
        ));
    }
    if material
        .normal_scale
        .is_some_and(|value| !value.is_finite())
    {
        return Err(SceneError::new(
            source,
            Some(object_id),
            "material.normal_scale должен быть конечным",
        ));
    }
    for (path, kind) in [
        (
            material.metallic_roughness_texture.as_ref(),
            "metallic-roughness текстура",
        ),
        (material.normal_texture.as_ref(), "normal текстура"),
        (material.emissive_texture.as_ref(), "emissive текстура"),
        (material.occlusion_texture.as_ref(), "occlusion текстура"),
    ] {
        if let Some(path) = path {
            require_asset_path(source, object_id, assets, path, kind)?;
            require_extension(source, object_id, path, "png", kind)?;
        }
    }
    Ok(())
}

pub(crate) fn require_extension(
    source: &str,
    object_id: &str,
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
            Some(object_id),
            format!(
                "{kind} /{} должен иметь расширение .{extension}",
                path.as_str()
            ),
        ))
    }
}

pub(crate) fn valid_ascii_name(value: &str, hyphen: bool) -> bool {
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

fn default_shadow_distance() -> f32 {
    100.0
}

fn default_shadow_cascades() -> u8 {
    crate::light::MAX_SHADOW_CASCADES
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
    settings: CameraSettings,
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
        fov_narrow: settings.projection.fov_deg.to_radians(),
        near: settings.projection.near,
        far: settings.projection.far,
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

pub(crate) mod virtual_path_serde {
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

pub(crate) mod option_virtual_path_serde {
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
    use super::{
        CharacterControllerComponent, ColliderComponent, LodGroup, LodLevel, MAX_LOD_LEVELS,
        MaterialAlphaMode, ParamValue, RigidBodyComponent, SceneDocument,
    };
    use engine_files::VirtualPath;

    const VALID: &str = r#"
[[objects]]
id = "camera"
[objects.camera]

"#;

    #[test]
    fn validate_checks_a_document_changed_after_parsing() {
        let mut document = parse(VALID).unwrap();
        assert!(document.validate().is_ok());
        document.objects[0].camera.as_mut().unwrap().settings =
            Some(VirtualPath::parse("missing.camerasettings").unwrap());
        let error = document.validate().unwrap_err();
        assert_eq!(error.object_id.as_deref(), Some("camera"));
        assert!(error.message.contains("[[assets]]"));
        document.objects.clear();
        assert!(document.validate().unwrap_err().message.contains("камер"));
    }

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

    fn lod_assets() -> String {
        let mut assets = String::from("[[assets]]\npath=\"/models/base.fbx\"\n");
        for index in 0..MAX_LOD_LEVELS {
            assets.push_str(&format!("[[assets]]\npath=\"/models/lod{index}.fbx\"\n"));
        }
        assets.push_str(
            "[[assets]]\npath=\"/textures/base.png\"\n[[assets]]\npath=\"/textures/lod.png\"\n",
        );
        assets
    }

    fn lod_group(distances: &[&str], cull_distance: Option<&str>) -> String {
        let mut group = String::from("[objects.lod_group]\n");
        if let Some(distance) = cull_distance {
            group.push_str(&format!("cull_distance={distance}\n"));
        }
        for (index, distance) in distances.iter().enumerate() {
            group.push_str(&format!(
                "\n[[objects.lod_group.levels]]\nmodel=\"/models/lod{index}.fbx\"\ndistance={distance}\n"
            ));
        }
        group
    }

    fn lod_scene(group: &str, mesh_renderer: bool) -> String {
        let renderer = if mesh_renderer {
            "[objects.mesh_renderer]\nmodel=\"/models/base.fbx\"\ntexture=\"/textures/base.png\"\n"
        } else {
            ""
        };
        format!(
            "{}\n{}\n[[objects]]\nid=\"mesh\"\n{}{}",
            lod_assets(),
            VALID,
            renderer,
            group
        )
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
    fn integer_literals_are_accepted_for_transform_fields() {
        let input = VALID.replace(
            "[objects.camera]",
            "[objects.transform]\nposition = [0, 1, 0]\n[objects.camera]",
        );
        let document = parse(&input).unwrap();
        assert_eq!(document.objects[0].transform.position, [0.0, 1.0, 0.0]);
        assert!(!document.objects[0].camera.as_ref().unwrap().active);
    }

    #[test]
    fn lod_group_round_trips_and_absent_groups_keep_legacy_defaults() {
        let input = lod_scene(&lod_group(&["10.0", "20.0"], Some("40.0")), true);
        let document = parse(&input).unwrap();
        let group = document.objects[1].lod_group.as_ref().unwrap();
        assert_eq!(group.levels.len(), 2);
        assert_eq!(group.levels[0].model, "/models/lod0.fbx");
        assert_eq!(group.levels[0].texture, None);
        assert_eq!(group.cull_distance, Some(40.0));

        let serialized = document.to_toml().unwrap();
        let round_trip = parse(&serialized).unwrap();
        assert!(!serialized.contains("double_sided"), "{serialized}");
        assert_eq!(
            round_trip.objects[1].lod_group.as_ref().unwrap().levels[1].distance,
            20.0
        );
        assert!(parse(VALID).unwrap().objects[0].lod_group.is_none());
    }

    #[test]
    fn lod_group_selects_next_level_at_thresholds_and_culls_at_limit() {
        let group = LodGroup {
            levels: vec![
                LodLevel {
                    model: "/models/lod0.fbx".to_owned(),
                    texture: None,
                    distance: 10.0,
                },
                LodLevel {
                    model: "/models/lod1.fbx".to_owned(),
                    texture: None,
                    distance: 20.0,
                },
            ],
            cull_distance: Some(30.0),
        };
        assert_eq!(group.level_for_distance(0.0), Some(0));
        assert_eq!(group.level_for_distance(9.99), Some(0));
        assert_eq!(group.level_for_distance(10.0), Some(1));
        assert_eq!(group.level_for_distance(20.0), Some(2));
        assert_eq!(group.level_for_distance(29.99), Some(2));
        assert_eq!(group.level_for_distance(30.0), None);
    }

    #[test]
    fn lod_group_requires_a_mesh_and_one_to_six_levels() {
        error(
            &lod_scene(&lod_group(&["10.0"], None), false),
            "lod_group требует mesh_renderer",
        );
        error(&lod_scene(&lod_group(&[], None), true), "от 1 до 6 уровней");
        let seven = ["1.0", "2.0", "3.0", "4.0", "5.0", "6.0", "7.0"];
        error(
            &lod_scene(&lod_group(&seven, None), true),
            "от 1 до 6 уровней",
        );
    }

    #[test]
    fn lod_distances_must_be_finite_positive_and_strictly_ascending() {
        for distances in [
            vec!["0.0"],
            vec!["-1.0"],
            vec!["nan"],
            vec!["inf"],
            vec!["10.0", "10.0"],
            vec!["20.0", "10.0"],
        ] {
            error(
                &lod_scene(&lod_group(&distances, None), true),
                "distance должен быть конечным, положительным и строго возрастать",
            );
        }
        for cull_distance in ["10.0", "5.0", "nan", "inf"] {
            error(
                &lod_scene(&lod_group(&["10.0"], Some(cull_distance)), true),
                "cull_distance должен быть конечным и больше последнего distance",
            );
        }
    }

    #[test]
    fn lod_assets_require_declared_fbx_and_png_paths_and_reject_unknown_fields() {
        let group = lod_group(&["10.0"], None);
        let undeclared = group.replace("/models/lod0.fbx", "/models/missing.fbx");
        error(
            &lod_scene(&undeclared, true),
            "модель LOD /models/missing.fbx отсутствует в [[assets]]",
        );

        let wrong_model = group.replace("/models/lod0.fbx", "/models/lod0.obj");
        let wrong_model = format!("[[assets]]\npath=\"/models/lod0.obj\"\n{wrong_model}");
        error(&lod_scene(&wrong_model, true), "расширение .fbx");

        let wrong_texture = group.replace(
            "distance=10.0",
            "distance=10.0\ntexture=\"/textures/lod.jpg\"",
        );
        let wrong_texture = format!("[[assets]]\npath=\"/textures/lod.jpg\"\n{wrong_texture}");
        error(&lod_scene(&wrong_texture, true), "расширение .png");

        let undeclared_texture = group.replace(
            "distance=10.0",
            "distance=10.0\ntexture=\"/textures/missing.png\"",
        );
        error(
            &lod_scene(&undeclared_texture, true),
            "текстура LOD /textures/missing.png отсутствует в [[assets]]",
        );

        error(
            &lod_scene(
                &group.replace("[objects.lod_group]", "[objects.lod_group]\nextra=true"),
                true,
            ),
            "unknown field",
        );
        error(
            &lod_scene(
                &group.replace("distance=10.0", "distance=10.0\nextra=true"),
                true,
            ),
            "unknown field",
        );

        let invalid_path = group.replace("/models/lod0.fbx", "/models/../lod0.fbx");
        error(
            &lod_scene(&invalid_path, true),
            "lod_group.levels.model /models/../lod0.fbx: некорректный virtual path",
        );
    }

    #[test]
    fn unknown_fields_are_denied_at_every_schema_level() {
        let cases = [
            format!("unexpected = true\n{VALID}"),
            VALID.replace("id = \"camera\"", "id = \"camera\"\nunexpected = true"),
            format!("{VALID}\nunexpected = true"),
            format!("{VALID}\n[objects.transform]\nunexpected = true"),
            format!("{VALID}\n[objects.camera]\nunexpected = true"),
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
        let cycle = "[[objects]]\nid = \"camera\"\nparent = \"loop\"\n[objects.camera]\n[[objects]]\nid = \"loop\"\nparent = \"camera\"\n".to_owned();
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
    fn scene_requires_a_camera_and_selects_one_explicit_active_camera() {
        assert!(parse("[[objects]]\nid=\"empty\"\n").is_err());
        let two_without_active = format!("{VALID}\n[[objects]]\nid=\"second\"\n[objects.camera]\n");
        error(&two_without_active, "одна должна иметь active = true");
        let two_with_active =
            format!("{VALID}\n[[objects]]\nid=\"second\"\n[objects.camera]\nactive = true\n");
        assert!(parse(&two_with_active).is_ok());
        let two_active = format!(
            "{}\n[[objects]]\nid=\"second\"\n[objects.camera]\nactive = true\n",
            VALID.replace("[objects.camera]", "[objects.camera]\nactive = true")
        );
        error(&two_active, "не более одной активной камеры");
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
        let missing_settings = VALID.replace(
            "[objects.camera]",
            "[objects.camera]\nsettings = \"/camera/default.camerasettings\"",
        );
        error(&missing_settings, "отсутствует в [[assets]]");
        let wrong_settings_extension = format!(
            "[[assets]]\npath=\"/camera/default.toml\"\n{}",
            VALID.replace(
                "[objects.camera]",
                "[objects.camera]\nsettings=\"/camera/default.toml\"",
            )
        );
        error(&wrong_settings_extension, "расширение .camerasettings");
        let good = format!(
            "[[assets]]\npath=\"/models/A.FBX\"\n[[assets]]\npath=\"/textures/A.PNG\"\n[[assets]]\npath=\"/scripts/a.LUA\"\n[[assets]]\npath=\"/camera/Default.CAMERAsettings\"\n{}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/A.FBX\"\ntexture=\"/textures/A.PNG\"\n[[objects.lite_scripts]]\npath=\"/scripts/a.LUA\"\n",
            VALID.replace(
                "[objects.camera]",
                "[objects.camera]\nsettings=\"/camera/Default.CAMERAsettings\"",
            )
        );
        assert!(parse(&good).is_ok());
    }

    #[test]
    fn camera_post_process_must_be_a_declared_postprocess_asset() {
        let rejects = |input: &str, expected: &str| {
            let message = parse(input).unwrap_err().to_string();
            assert!(message.contains(expected), "{message}");
        };
        let with_post_process = |path: &str| {
            VALID.replace(
                "[objects.camera]",
                &format!("[objects.camera]\npost_process=\"{path}\""),
            )
        };
        rejects(
            &with_post_process("/post/default.postprocess"),
            "отсутствует в [[assets]]",
        );
        let wrong_extension = format!(
            "[[assets]]\npath=\"/post/default.toml\"\n{}",
            with_post_process("/post/default.toml")
        );
        rejects(&wrong_extension, "расширение .postprocess");
        let good = format!(
            "[[assets]]\npath=\"/post/Default.POSTprocess\"\n{}",
            with_post_process("/post/Default.POSTprocess")
        );
        let document = parse(&good).unwrap();
        assert!(
            document.objects[0]
                .camera
                .as_ref()
                .is_some_and(|camera| camera.post_process.is_some())
        );
    }

    #[test]
    fn material_override_round_trips_and_legacy_mesh_renderer_defaults() {
        let input = format!(
            "[[assets]]\npath=\"/models/a.fbx\"\n[[assets]]\npath=\"/textures/mr.png\"\n[[assets]]\npath=\"/textures/normal.png\"\n[[assets]]\npath=\"/textures/emissive.png\"\n[[assets]]\npath=\"/textures/occlusion.png\"\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.fbx\"\n[objects.mesh_renderer.material]\nbase_color=[0.2,0.4,0.6]\nmetallic=0.7\nroughness=0.3\nemissive=[2.0,0.5,4.0]\nopacity=0.65\nalpha_mode=\"mask\"\nalpha_cutoff=0.35\nocclusion_strength=0.6\nmetallic_roughness_texture=\"/textures/mr.png\"\nnormal_texture=\"/textures/normal.png\"\nemissive_texture=\"/textures/emissive.png\"\nocclusion_texture=\"/textures/occlusion.png\"\nnormal_scale=0.5\n"
        );
        let document = parse(&input).unwrap();
        let material = document.objects[1]
            .mesh_renderer
            .as_ref()
            .unwrap()
            .material
            .as_ref()
            .unwrap();
        assert_eq!(material.base_color, Some([0.2, 0.4, 0.6]));
        assert_eq!(material.metallic, Some(0.7));
        assert_eq!(material.roughness, Some(0.3));
        assert_eq!(material.emissive, Some([2.0, 0.5, 4.0]));
        assert_eq!(material.opacity, Some(0.65));
        assert_eq!(material.alpha_mode, Some(MaterialAlphaMode::Mask));
        assert_eq!(material.alpha_cutoff, Some(0.35));
        assert_eq!(material.occlusion_strength, Some(0.6));
        assert_eq!(
            material
                .metallic_roughness_texture
                .as_ref()
                .unwrap()
                .as_str(),
            "textures/mr.png"
        );
        assert_eq!(
            material.normal_texture.as_ref().unwrap().as_str(),
            "textures/normal.png"
        );
        assert_eq!(
            material.emissive_texture.as_ref().unwrap().as_str(),
            "textures/emissive.png"
        );
        assert_eq!(
            material.occlusion_texture.as_ref().unwrap().as_str(),
            "textures/occlusion.png"
        );
        assert_eq!(material.normal_scale, Some(0.5));
        assert_eq!(material.double_sided, None);

        let serialized = document.to_toml().unwrap();
        let round_trip = parse(&serialized).unwrap();
        assert_eq!(
            round_trip.objects[1]
                .mesh_renderer
                .as_ref()
                .unwrap()
                .material
                .as_ref()
                .unwrap()
                .metallic,
            Some(0.7)
        );
        assert!(!serialized.contains("material = \""), "{serialized}");

        let double_sided = parse(&format!(
            "[[assets]]\npath=\"/models/a.fbx\"\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.fbx\"\n[objects.mesh_renderer.material]\ndouble_sided=true\n"
        ))
        .unwrap();
        let double_sided_round_trip = parse(&double_sided.to_toml().unwrap()).unwrap();
        assert_eq!(
            double_sided_round_trip.objects[1]
                .mesh_renderer
                .as_ref()
                .unwrap()
                .material
                .as_ref()
                .unwrap()
                .double_sided,
            Some(true)
        );
        let double_sided_false = parse(&format!(
            "[[assets]]\npath=\"/models/a.fbx\"\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.fbx\"\n[objects.mesh_renderer.material]\ndouble_sided=false\n"
        ))
        .unwrap();
        let double_sided_false_round_trip = parse(&double_sided_false.to_toml().unwrap()).unwrap();
        assert_eq!(
            double_sided_false_round_trip.objects[1]
                .mesh_renderer
                .as_ref()
                .unwrap()
                .material
                .as_ref()
                .unwrap()
                .double_sided,
            Some(false)
        );

        let partial = parse(&format!(
            "[[assets]]\npath=\"/models/a.fbx\"\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.fbx\"\n[objects.mesh_renderer.material]\nroughness=0.4\n"
        ))
        .unwrap()
        .to_toml()
        .unwrap();
        assert!(partial.contains("roughness = 0.4"), "{partial}");
        assert!(!partial.contains("base_color"), "{partial}");
        assert!(!partial.contains("metallic ="), "{partial}");
        assert!(!partial.contains("normal_scale"), "{partial}");

        let legacy = parse(&format!(
            "[[assets]]\npath=\"/models/a.fbx\"\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.fbx\"\n"
        ))
        .unwrap();
        assert!(
            legacy.objects[1]
                .mesh_renderer
                .as_ref()
                .unwrap()
                .material
                .is_none()
        );
    }

    #[test]
    fn material_override_rejects_unknown_fields_out_of_range_factors_and_non_finite_scale() {
        let scene = |material: &str| {
            format!(
                "[[assets]]\npath=\"/models/a.fbx\"\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.fbx\"\n[objects.mesh_renderer.material]\n{material}\n"
            )
        };
        assert!(parse(&scene("unexpected=true")).is_err());
        for values in [
            "base_color=[-0.1,0.5,0.5]",
            "base_color=[0.5,1.1,0.5]",
            "base_color=[nan,0.5,0.5]",
            "metallic=-0.01",
            "metallic=1.01",
            "metallic=nan",
            "roughness=-0.01",
            "roughness=1.01",
            "roughness=inf",
            "opacity=-0.01",
            "opacity=1.01",
            "opacity=nan",
            "occlusion_strength=-0.01",
            "occlusion_strength=1.01",
            "emissive=[-0.1,0.0,0.0]",
            "emissive=[0.0,nan,0.0]",
            "normal_scale=nan",
            "normal_scale=inf",
            "alpha_mode=\"unknown\"",
            "alpha_cutoff=-0.01",
            "alpha_mode=\"blend\"\nalpha_cutoff=0.5",
            "alpha_cutoff=0.5",
        ] {
            assert!(parse(&scene(values)).is_err(), "accepted {values}");
        }
        assert!(
            parse(&scene(
                "base_color=[0,1,0.5]\nmetallic=0\nroughness=1\nnormal_scale=-2"
            ))
            .is_ok()
        );
        assert!(parse(&scene("alpha_mode=\"mask\"\nalpha_cutoff=1.0")).is_ok());
    }

    #[test]
    fn material_override_paths_must_be_valid_declared_png_assets() {
        let scene = |assets: &str, material: &str| {
            format!(
                "{assets}\n{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/a.fbx\"\n[objects.mesh_renderer.material]\n{material}\n"
            )
        };
        let model = "[[assets]]\npath=\"/models/a.fbx\"";
        error(
            &scene(model, "normal_texture=\"/textures/missing.png\""),
            "отсутствует в [[assets]]",
        );
        let wrong_extension = format!("{model}\n[[assets]]\npath=\"/textures/normal.jpg\"");
        error(
            &scene(&wrong_extension, "normal_texture=\"/textures/normal.jpg\""),
            "расширение .png",
        );
        for field in ["emissive_texture", "occlusion_texture"] {
            error(
                &scene(model, &format!("{field}=\"/textures/missing.png\"")),
                "отсутствует в [[assets]]",
            );
            let wrong_extension = format!("{model}\n[[assets]]\npath=\"/textures/map.jpg\"");
            error(
                &scene(&wrong_extension, &format!("{field}=\"/textures/map.jpg\"")),
                "расширение .png",
            );
        }
        let invalid_path = scene(model, "normal_texture=\"/textures/../normal.png\"");
        let error = parse(&invalid_path).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("normal_texture /textures/../normal.png: некорректный virtual path"),
            "{error}"
        );
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
        let old_camera_fields =
            VALID.replace("[objects.camera]", "[objects.camera]\nfov_deg = 60.0");
        error(&old_camera_fields, "unknown field `fov_deg`");
        let light_nan = format!(
            "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\ncolor=[nan,1.0,1.0]\n"
        );
        assert!(parse(&light_nan).is_err());
        let param_inf = format!(
            "{VALID}\n[[objects.lite_scripts]]\npath=\"/s.lua\"\nparams={{speed=inf}}\n[[assets]]\npath=\"/s.lua\"\n"
        );
        assert!(parse(&param_inf).is_err());
    }

    #[test]
    fn light_defaults_shadow_settings_and_nonnegative_colors_are_checked() {
        for distance in ["0.0", "-1.0", "nan", "inf", "-inf"] {
            let input = format!(
                "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\nshadow_distance={distance}\n"
            );
            assert!(
                parse(&input).is_err(),
                "accepted shadow distance {distance}"
            );
        }
        for cascades in ["0", "5"] {
            let input = format!(
                "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\nshadow_cascades={cascades}\n"
            );
            assert!(
                parse(&input).is_err(),
                "accepted shadow cascades {cascades}"
            );
        }
        for removed_field in ["shadow_center=[0.0,0.0,0.0]", "shadow_radius=5.0"] {
            let input = format!(
                "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\n{removed_field}\n"
            );
            assert!(
                parse(&input).is_err(),
                "accepted removed field {removed_field}"
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
        assert_eq!(light.shadow_distance, 100.0);
        assert_eq!(light.shadow_cascades, 4);

        let shadow_enabled = parse(&format!(
            "{VALID}\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\nshadows=true\n"
        ))
        .unwrap();
        assert!(shadow_enabled.objects[1].directional_light.unwrap().shadows);
    }

    #[test]
    fn local_light_defaults_round_trip_and_validate_all_factors() {
        let input = format!(
            "{VALID}\n[[objects]]\nid=\"point\"\n[objects.point_light]\n[[objects]]\nid=\"spot\"\n[objects.spot_light]\n"
        );
        let document = parse(&input).unwrap();
        let point = document.objects[1].point_light.unwrap();
        assert_eq!(point.color, [1.0; 3]);
        assert_eq!(point.intensity, 1.0);
        assert_eq!(point.range, 10.0);
        assert!(!point.shadows);
        let spot = document.objects[2].spot_light.unwrap();
        assert_eq!(spot.color, [1.0; 3]);
        assert_eq!(spot.intensity, 1.0);
        assert_eq!(spot.range, 10.0);
        assert_eq!(spot.inner_angle_deg, 20.0);
        assert_eq!(spot.outer_angle_deg, 30.0);
        assert!(!spot.shadows);
        assert!(parse(&document.to_toml().unwrap()).is_ok());

        for field in [
            "color=[-1.0,1.0,1.0]",
            "color=[nan,1.0,1.0]",
            "intensity=-1.0",
            "intensity=inf",
            "range=0.0",
            "range=nan",
            "unknown=true",
        ] {
            let input =
                format!("{VALID}\n[[objects]]\nid=\"point\"\n[objects.point_light]\n{field}\n");
            assert!(parse(&input).is_err(), "accepted point_light.{field}");
            let input =
                format!("{VALID}\n[[objects]]\nid=\"spot\"\n[objects.spot_light]\n{field}\n");
            assert!(parse(&input).is_err(), "accepted spot_light.{field}");
        }
        for (inner, outer) in [
            ("0.0", "30.0"),
            ("31.0", "30.0"),
            ("20.0", "91.0"),
            ("nan", "30.0"),
            ("20.0", "inf"),
        ] {
            let input = format!(
                "{VALID}\n[[objects]]\nid=\"spot\"\n[objects.spot_light]\ninner_angle_deg={inner}\nouter_angle_deg={outer}\n"
            );
            assert!(
                parse(&input).is_err(),
                "accepted spot cone {inner}..{outer}"
            );
        }
        assert!(parse(&format!(
            "{VALID}\n[[objects]]\nid=\"spot\"\n[objects.spot_light]\ninner_angle_deg=90.0\nouter_angle_deg=90.0\n"
        ))
        .is_ok());
    }

    #[test]
    fn local_lights_are_unlimited_and_independent_of_directional_light_limit() {
        let mut input = VALID.to_owned();
        for index in 0..40 {
            input.push_str(&format!(
                "\n[[objects]]\nid=\"point-{index}\"\n[objects.point_light]\n"
            ));
        }
        input.push_str("\n[[objects]]\nid=\"sun\"\n[objects.directional_light]\n");
        assert!(parse(&input).is_ok());
        input.push_str("\n[[objects]]\nid=\"sun2\"\n[objects.directional_light]\n");
        assert!(parse(&input).is_err());
    }

    #[test]
    fn physics_components_default_and_round_trip() {
        let document = parse(&format!(
            "{VALID}\n[[objects]]\nid=\"box\"\n[objects.collider]\n[objects.rigid_body]\n\
             [[objects]]\nid=\"player\"\n[objects.character_controller]\n"
        ))
        .unwrap();
        let box_object = &document.objects[1];
        assert_eq!(box_object.collider, Some(ColliderComponent::default()));
        assert_eq!(box_object.rigid_body, Some(RigidBodyComponent::default()));
        assert_eq!(
            box_object.character_controller, None,
            "components stay optional per object"
        );
        assert_eq!(
            document.objects[2].character_controller,
            Some(CharacterControllerComponent::default())
        );

        let round_trip = parse(&document.to_toml().unwrap()).unwrap();
        assert_eq!(round_trip.objects[1].collider, box_object.collider);
        assert_eq!(round_trip.objects[1].rigid_body, box_object.rigid_body);
        assert_eq!(
            round_trip.objects[2].character_controller,
            document.objects[2].character_controller
        );
    }

    #[test]
    fn physics_components_preserve_authored_values_in_round_trip() {
        let document = parse(&format!(
            r#"{VALID}
[[objects]]
id = "body"
[objects.collider]
shape = "capsule"
radius = 0.25
height = 0.75
offset = [1.0, 2.0, 3.0]
trigger = true
friction = 0.8
restitution = 0.5
[objects.rigid_body]
kind = "kinematic"
mass = 3.0
linear_damping = 0.1
angular_damping = 0.2
gravity_scale = -1.0
lock_rotation = true
[[objects]]
id = "player"
[objects.character_controller]
radius = 0.4
height = 1.4
step_height = 0.5
max_slope_deg = 60.0
snap_distance = 0.3
skin = 0.04
push_strength = 2.0
"#
        ))
        .unwrap();
        let round_trip = parse(&document.to_toml().unwrap()).unwrap();
        assert_eq!(round_trip.objects[1].collider, document.objects[1].collider);
        assert_eq!(
            round_trip.objects[1].rigid_body,
            document.objects[1].rigid_body
        );
        assert_eq!(
            round_trip.objects[2].character_controller,
            document.objects[2].character_controller
        );
    }

    #[test]
    fn physics_components_validate_shape_dependencies_and_body_relationships() {
        let missing_collider = format!("{VALID}\n[[objects]]\nid=\"body\"\n[objects.rigid_body]\n");
        assert!(
            parse(&missing_collider)
                .unwrap_err()
                .message
                .contains("требует collider")
        );

        for shape in ["convex_mesh", "triangle_mesh"] {
            let input = format!(
                "{VALID}\n[[objects]]\nid=\"mesh\"\n[objects.collider]\nshape=\"{shape}\"\n"
            );
            assert!(
                parse(&input)
                    .unwrap_err()
                    .message
                    .contains("требует mesh_renderer")
            );
        }

        let dynamic_triangle_mesh = format!(
            "[[assets]]\npath=\"/models/mesh.fbx\"\n{VALID}\n\
             [[objects]]\nid=\"mesh\"\n[objects.mesh_renderer]\nmodel=\"/models/mesh.fbx\"\n\
             [objects.collider]\nshape=\"triangle_mesh\"\n[objects.rigid_body]\nkind=\"dynamic\"\n"
        );
        assert!(
            parse(&dynamic_triangle_mesh)
                .unwrap_err()
                .message
                .contains("несовместим с triangle_mesh")
        );

        let character_with_collider = format!(
            "{VALID}\n[[objects]]\nid=\"player\"\n[objects.collider]\n\
             [objects.character_controller]\n"
        );
        assert!(
            parse(&character_with_collider)
                .unwrap_err()
                .message
                .contains("несовместим")
        );

        let character_with_body = format!(
            "{VALID}\n[[objects]]\nid=\"player\"\n[objects.collider]\n\
             [objects.rigid_body]\n[objects.character_controller]\n"
        );
        assert!(
            parse(&character_with_body)
                .unwrap_err()
                .message
                .contains("несовместим")
        );
    }

    #[test]
    fn physics_pose_writers_must_be_roots_but_kinematic_bodies_may_be_children() {
        let dynamic_child = format!(
            "{VALID}\n[[objects]]\nid=\"body\"\nparent=\"camera\"\n\
             [objects.collider]\n[objects.rigid_body]\nkind=\"dynamic\"\n"
        );
        assert!(
            parse(&dynamic_child)
                .unwrap_err()
                .message
                .contains("должен быть корневым")
        );

        let character_child = format!(
            "{VALID}\n[[objects]]\nid=\"player\"\nparent=\"camera\"\n\
             [objects.character_controller]\n"
        );
        assert!(
            parse(&character_child)
                .unwrap_err()
                .message
                .contains("должен быть корневым")
        );

        let kinematic_child = format!(
            "{VALID}\n[[objects]]\nid=\"platform\"\nparent=\"camera\"\n\
             [objects.collider]\n[objects.rigid_body]\nkind=\"kinematic\"\n"
        );
        assert!(parse(&kinematic_child).is_ok());
    }

    #[test]
    fn physics_components_reject_non_finite_and_out_of_range_values() {
        for collider in [
            "shape=\"box\"\nhalf_extents=[0.5, nan, 0.5]",
            "shape=\"box\"\nhalf_extents=[0.0, 0.5, 0.5]",
            "shape=\"sphere\"\nradius=0.0",
            "shape=\"capsule\"\nheight=0.0",
            "friction=inf",
            "friction=-0.1",
            "restitution=1.1",
            "offset=[0.0, 0.0, nan]",
            "unknown=true",
        ] {
            let input =
                format!("{VALID}\n[[objects]]\nid=\"shape\"\n[objects.collider]\n{collider}\n");
            assert!(
                parse(&input).is_err(),
                "accepted collider fields {collider}"
            );
        }

        for body in [
            "mass=0.0",
            "linear_damping=-1.0",
            "angular_damping=nan",
            "gravity_scale=inf",
        ] {
            let input = format!(
                "{VALID}\n[[objects]]\nid=\"body\"\n[objects.collider]\n\
                 [objects.rigid_body]\n{body}\n"
            );
            assert!(parse(&input).is_err(), "accepted rigid_body.{body}");
        }

        for field in [
            "radius=0.0",
            "height=nan",
            "step_height=-0.1",
            "max_slope_deg=0.0",
            "max_slope_deg=-0.1",
            "max_slope_deg=90.0",
            "snap_distance=-0.1",
            "skin=-0.1",
            "push_strength=inf",
            "unknown=true",
        ] {
            let input = format!(
                "{VALID}\n[[objects]]\nid=\"player\"\n[objects.character_controller]\n{field}\n"
            );
            assert!(
                parse(&input).is_err(),
                "accepted character_controller.{field}"
            );
        }
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

    #[test]
    fn environment_sources_are_exclusive_validated_and_round_trip() {
        let hdr = r#"
[[assets]]
path="/environment/sky.hdr"
[[objects]]
id="camera"
[objects.camera]
[[objects]]
id="environment"
[objects.environment]
hdr="/environment/sky.hdr"
intensity=1.5
skybox=false
"#;
        let document = SceneDocument::parse("environment.rvescene", hdr).unwrap();
        let saved = document.to_toml().unwrap();
        let reparsed = SceneDocument::parse("environment.rvescene", &saved).unwrap();
        assert_eq!(reparsed.to_toml().unwrap(), saved);

        let procedural = r#"
[[objects]]
id="camera"
[objects.camera]
[[objects]]
id="environment"
[objects.environment]
[objects.environment.procedural]
zenith_color=[0.1, 0.2, 0.4]
horizon_color=[0.8, 0.7, 0.5]
ground_color=[0.1, 0.1, 0.1]
sun_size_deg=2.0
sun_intensity=4.0
"#;
        assert!(SceneDocument::parse("procedural.rvescene", procedural).is_ok());

        let empty = format!("{VALID}\n[[objects]]\nid=\"environment\"\n[objects.environment]\n");
        let error = SceneDocument::parse("empty-environment.rvescene", &empty).unwrap_err();
        assert!(error.message.contains("ровно один источник"));

        let both = format!(
            "{hdr}\n[objects.environment.procedural]\nzenith_color=[1.0,1.0,1.0]\nhorizon_color=[1.0,1.0,1.0]\nground_color=[0.0,0.0,0.0]\nsun_size_deg=1.0\nsun_intensity=1.0\n"
        );
        let error = SceneDocument::parse("both-environment.rvescene", &both).unwrap_err();
        assert!(error.message.contains("ровно один источник"));

        let bad_extension = hdr.replace("/environment/sky.hdr", "/environment/sky.png");
        let error = SceneDocument::parse("bad-extension.rvescene", &bad_extension).unwrap_err();
        assert!(error.message.contains("расширение .hdr"));
        let negative_intensity = hdr.replace("intensity=1.5", "intensity=-1.0");
        let error =
            SceneDocument::parse("bad-intensity.rvescene", &negative_intensity).unwrap_err();
        assert!(error.message.contains("environment.intensity"));
        let invalid_sun = procedural.replace("sun_size_deg=2.0", "sun_size_deg=181.0");
        let error = SceneDocument::parse("bad-sun.rvescene", &invalid_sun).unwrap_err();
        assert!(error.message.contains("sun_size_deg"));

        let two = format!(
            "{hdr}\n[[objects]]\nid=\"second\"\n[objects.environment]\nhdr=\"/environment/sky.hdr\"\n"
        );
        let error = SceneDocument::parse("two-environments.rvescene", &two).unwrap_err();
        assert!(error.message.contains("не более одного environment"));
    }

    #[test]
    fn camera_cursor_defaults_to_false_for_existing_scenes() {
        let document = parse(VALID).unwrap();
        let camera = document.objects[0].camera.as_ref().unwrap();
        assert_eq!(camera.cursor, super::CameraCursor::default());
        assert!(!camera.cursor.hide_on_start);
        assert!(!camera.cursor.lock_on_start);
    }

    #[test]
    fn camera_cursor_flags_round_trip_and_default_table_is_not_written() {
        let scene = r#"
[[objects]]
id = "camera"
[objects.camera]
active = true
[objects.camera.cursor]
hide_on_start = true
lock_on_start = true
"#;
        let document = parse(scene).unwrap();
        let cursor = document.objects[0].camera.as_ref().unwrap().cursor;
        assert!(cursor.hide_on_start && cursor.lock_on_start);
        let written = document.to_toml().unwrap();
        assert!(written.contains("[objects.camera.cursor]"), "{written}");
        let reparsed = parse(&written).unwrap();
        assert_eq!(reparsed.objects[0].camera.as_ref().unwrap().cursor, cursor);

        let mut default_cursor = document;
        default_cursor.objects[0].camera.as_mut().unwrap().cursor = Default::default();
        let written = default_cursor.to_toml().unwrap();
        assert!(!written.contains("cursor"), "{written}");
    }

    #[test]
    fn camera_cursor_rejects_unknown_fields() {
        let scene = format!("{VALID}\n[objects.camera.cursor]\nunknown = true\n");
        assert!(parse(&scene).is_err());
    }

    #[test]
    fn environment_rejects_unknown_fields_and_undeclared_assets() {
        let unknown = format!(
            "{VALID}\n[[objects]]\nid=\"environment\"\n[objects.environment]\nhdr=\"/sky.hdr\"\nunknown=true\n"
        );
        assert!(SceneDocument::parse("unknown-environment.rvescene", &unknown).is_err());
        let missing = format!(
            "{VALID}\n[[objects]]\nid=\"environment\"\n[objects.environment]\nhdr=\"/sky.hdr\"\n"
        );
        let error = SceneDocument::parse("missing-environment.rvescene", &missing).unwrap_err();
        assert!(error.message.contains("отсутствует в [[assets]]"));
    }

    #[test]
    fn max_instances_round_trips_for_prefab_assets_and_stays_absent_otherwise() {
        let input = format!(
            "[[assets]]\npath = \"/prefabs/bullet.rveprefab\"\nmax_instances = 100\n[[assets]]\npath = \"/models/crate.fbx\"\n{VALID}"
        );
        let document = parse(&input).unwrap();
        assert_eq!(document.assets[0].max_instances, Some(100));
        assert_eq!(document.assets[1].max_instances, None);
        let text = document.to_toml().unwrap();
        assert!(text.contains("max_instances = 100"), "{text}");
        assert_eq!(parse(&text).unwrap().assets, document.assets);

        let plain = parse(&format!(
            "[[assets]]\npath = \"/models/crate.fbx\"\n{VALID}"
        ))
        .unwrap()
        .to_toml()
        .unwrap();
        assert!(!plain.contains("max_instances"), "{plain}");
        assert_eq!(parse(&plain).unwrap().to_toml().unwrap(), plain);
    }

    #[test]
    fn max_instances_requires_a_prefab_and_a_positive_value() {
        for asset in [
            "path = \"/models/crate.fbx\"\nmax_instances = 3",
            "path = \"/prefabs/bullet.rveprefab\"\nmax_instances = 0",
        ] {
            let error = parse(&format!("[[assets]]\n{asset}\n{VALID}")).unwrap_err();
            assert!(error.message.contains("max_instances"), "{error}");
        }
        assert!(
            parse(&format!(
                "[[assets]]\npath = \"/prefabs/bullet.rveprefab\"\nmax_instances = -1\n{VALID}"
            ))
            .is_err()
        );
    }
}
