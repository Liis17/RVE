//! Префабы `.rveprefab`: экземпляры в документах, overrides и раскрытие в плоский документ.
//!
//! Префаб — документ той же схемы, что сцена, с ровно одним корнем. Экземпляр
//! `[[instances]]` вставляет раскрытое дерево префаба: корень получает id экземпляра,
//! остальные объекты — `<экземпляр>.<id в префабе>`, у вложенных — `a.b.c`.

use crate::audio::{AudioSourceComponent, validate_audio_source};
use crate::document::{
    CameraComponent, CharacterControllerComponent, ColliderComponent, DirectionalLightComponent,
    DocumentKind, EnvironmentComponent, MaterialAlphaMode, MaterialOverride, ParamValue,
    PointLightComponent, RigidBodyComponent, SceneAsset, SceneDocument, SceneError, SceneObject,
    SceneTransform, SpotLightComponent, mesh_collider_model, option_virtual_path_serde,
    require_asset_path, require_extension, valid_ascii_name, validate_environment,
    validate_material_override, virtual_path_serde,
};
use crate::particles::{ParticleEmitterComponent, validate_particle_emitter};
use crate::world::topological_order;
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

/// Наибольшая глубина вложенности префабов.
pub const MAX_PREFAB_DEPTH: usize = 16;
/// Наибольшая длина раскрытого id в байтах.
pub const MAX_EXPANDED_ID_BYTES: usize = 255;
/// Расширение файла префаба.
pub const PREFAB_EXTENSION: &str = "rveprefab";

/// Экземпляр префаба в сцене или другом префабе.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefabInstance {
    /// Id корня раскрытого префаба; авторский формат `[A-Za-z0-9_-]{1,64}`.
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Любой id раскрытого документа.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(with = "virtual_path_serde")]
    pub prefab: VirtualPath,
    /// Заменяет transform корня префаба целиком.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<SceneTransform>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<PrefabOverride>,
}

/// Разреженная замена полей одного объекта префаба. Компоненты не добавляются и не
/// удаляются: override поля отсутствующего компонента — ошибка.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrefabOverride {
    /// Id объекта в раскрытом префабе; у вложенных — через точку (`lock.bolt`).
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<TransformOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mesh_renderer: Option<MeshRendererOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<CameraComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directional_light: Option<DirectionalLightComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_light: Option<PointLightComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spot_light: Option<SpotLightComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collider: Option<ColliderComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rigid_body: Option<RigidBodyComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character_controller: Option<CharacterControllerComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_source: Option<AudioSourceComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub particle_emitter: Option<ParticleEmitterComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentComponent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lite_params: Vec<LiteParamsOverride>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heavy_params: Vec<HeavyParamsOverride>,
}

/// Частичная замена transform: заданные поля заменяются, остальные сохраняются.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransformOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<[f32; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<[f32; 3]>,
}

/// Замена модели и текстуры; материал сливается по заданным полям.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshRendererOverride {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_virtual_path_serde"
    )]
    pub model: Option<VirtualPath>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "option_virtual_path_serde"
    )]
    pub texture: Option<VirtualPath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<MaterialOverride>,
}

/// Слияние `params` lite-скриптов с этим путём по ключам.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiteParamsOverride {
    #[serde(with = "virtual_path_serde")]
    pub path: VirtualPath,
    pub params: BTreeMap<String, ParamValue>,
}

/// Слияние `params` heavy-скриптов с этим именем по ключам.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeavyParamsOverride {
    pub name: String,
    pub params: BTreeMap<String, ParamValue>,
}

/// Откуда взят объект раскрытого документа.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectOrigin {
    /// Id экземпляра верхнего уровня в исходном документе.
    pub instance: String,
    /// Id объекта в раскрытом префабе этого экземпляра — `target` его overrides.
    pub target: String,
}

/// Результат [`expand`].
#[derive(Clone, Debug)]
pub struct Expansion {
    /// Плоский проверенный документ без экземпляров; id объектов могут содержать точки.
    pub document: SceneDocument,
    /// Все использованные файлы префабов, включая вложенные, по возрастанию.
    pub prefabs: Vec<VirtualPath>,
    /// Происхождение объектов из экземпляров верхнего уровня; собственных объектов
    /// документа здесь нет.
    pub origins: BTreeMap<String, ObjectOrigin>,
}

/// Загрузчик префаба по пути; результат должен быть разобран [`SceneDocument::parse_prefab`].
pub type PrefabLoader<'a> = dyn FnMut(&VirtualPath) -> Result<SceneDocument, SceneError> + 'a;

/// Раскрывает экземпляры документа рекурсивно и проверяет результат целиком: родителей,
/// циклы, камеры, свет и базисы. Документ без экземпляров возвращается как есть, но
/// с признаком раскрытия.
pub fn expand(
    document: &SceneDocument,
    load: &mut PrefabLoader<'_>,
) -> Result<Expansion, SceneError> {
    expand_with(document, load, None)
}

/// Как [`expand`], но overrides экземпляра `instance` не применяются. По разнице с
/// обычным раскрытием редактор вычисляет overrides этого экземпляра.
pub fn expand_without_overrides(
    document: &SceneDocument,
    instance: &str,
    load: &mut PrefabLoader<'_>,
) -> Result<Expansion, SceneError> {
    expand_with(document, load, Some(instance))
}

fn expand_with(
    document: &SceneDocument,
    load: &mut PrefabLoader<'_>,
    skip_overrides: Option<&str>,
) -> Result<Expansion, SceneError> {
    document.validate()?;
    let mut expander = Expander {
        load,
        cache: HashMap::new(),
        stack: Vec::new(),
        used: BTreeSet::new(),
    };
    let flat = expander.flatten(document, skip_overrides)?;
    let expanded = SceneDocument {
        assets: flat.assets,
        objects: flat.objects,
        instances: Vec::new(),
        source_label: document.source_label.clone(),
        kind: document.kind,
        expanded: true,
    };
    expanded.validate()?;
    Ok(Expansion {
        document: expanded,
        prefabs: expander.used.into_iter().collect(),
        origins: flat.origins,
    })
}

/// Применяет override к объекту. Ошибка — override поля компонента, которого нет.
pub fn apply_override(object: &mut SceneObject, change: &PrefabOverride) -> Result<(), String> {
    if let Some(name) = &change.name {
        object.name = (!name.is_empty()).then(|| name.clone());
    }
    if let Some(transform) = change.transform {
        let target = &mut object.transform;
        target.position = transform.position.unwrap_or(target.position);
        target.rotation = transform.rotation.unwrap_or(target.rotation);
        target.scale = transform.scale.unwrap_or(target.scale);
    }
    if let Some(mesh) = &change.mesh_renderer {
        let target = object
            .mesh_renderer
            .as_mut()
            .ok_or("override mesh_renderer: у объекта нет mesh_renderer")?;
        if let Some(model) = &mesh.model {
            target.model = model.clone();
        }
        if let Some(texture) = &mesh.texture {
            target.texture = Some(texture.clone());
        }
        if let Some(material) = &mesh.material {
            let merged = target
                .material
                .get_or_insert_with(MaterialOverride::default);
            merge_material(merged, material);
        }
    }
    if let Some(camera) = &change.camera {
        let target = object
            .camera
            .as_mut()
            .ok_or("override camera: у объекта нет camera")?;
        *target = camera.clone();
    }
    if let Some(light) = change.directional_light {
        let target = object
            .directional_light
            .as_mut()
            .ok_or("override directional_light: у объекта нет directional_light")?;
        *target = light;
    }
    if let Some(light) = change.point_light {
        let target = object
            .point_light
            .as_mut()
            .ok_or("override point_light: у объекта нет point_light")?;
        *target = light;
    }
    if let Some(light) = change.spot_light {
        let target = object
            .spot_light
            .as_mut()
            .ok_or("override spot_light: у объекта нет spot_light")?;
        *target = light;
    }
    if let Some(collider) = change.collider {
        let target = object
            .collider
            .as_mut()
            .ok_or("override collider: у объекта нет collider")?;
        *target = collider;
    }
    if let Some(body) = change.rigid_body {
        let target = object
            .rigid_body
            .as_mut()
            .ok_or("override rigid_body: у объекта нет rigid_body")?;
        *target = body;
    }
    if let Some(controller) = change.character_controller {
        let target = object
            .character_controller
            .as_mut()
            .ok_or("override character_controller: у объекта нет character_controller")?;
        *target = controller;
    }
    if let Some(audio) = &change.audio_source {
        let target = object
            .audio_source
            .as_mut()
            .ok_or("override audio_source: у объекта нет audio_source")?;
        *target = audio.clone();
    }
    if let Some(emitter) = &change.particle_emitter {
        let target = object
            .particle_emitter
            .as_mut()
            .ok_or("override particle_emitter: у объекта нет particle_emitter")?;
        *target = emitter.clone();
    }
    if let Some(environment) = &change.environment {
        let target = object
            .environment
            .as_mut()
            .ok_or("override environment: у объекта нет environment")?;
        *target = environment.clone();
    }
    for params in &change.lite_params {
        let mut found = false;
        for script in &mut object.lite_scripts {
            if script.path == params.path {
                script.params.extend(params.params.clone());
                found = true;
            }
        }
        if !found {
            return Err(format!(
                "override lite_params: у объекта нет lite-скрипта /{}",
                params.path.as_str()
            ));
        }
    }
    for params in &change.heavy_params {
        let mut found = false;
        for script in &mut object.heavy_scripts {
            if script.name == params.name {
                script.params.extend(params.params.clone());
                found = true;
            }
        }
        if !found {
            return Err(format!(
                "override heavy_params: у объекта нет heavy-скрипта {:?}",
                params.name
            ));
        }
    }
    Ok(())
}

fn merge_material(target: &mut MaterialOverride, change: &MaterialOverride) {
    if change.base_color.is_some() {
        target.base_color = change.base_color;
    }
    if change.metallic.is_some() {
        target.metallic = change.metallic;
    }
    if change.roughness.is_some() {
        target.roughness = change.roughness;
    }
    if change.emissive.is_some() {
        target.emissive = change.emissive;
    }
    if change.opacity.is_some() {
        target.opacity = change.opacity;
    }
    if change.alpha_mode.is_some() {
        target.alpha_mode = change.alpha_mode;
        if change.alpha_mode != Some(MaterialAlphaMode::Mask) {
            target.alpha_cutoff = None;
        }
    }
    if change.alpha_cutoff.is_some() {
        target.alpha_cutoff = change.alpha_cutoff;
    }
    if change.metallic_roughness_texture.is_some() {
        target.metallic_roughness_texture = change.metallic_roughness_texture.clone();
    }
    if change.normal_texture.is_some() {
        target.normal_texture = change.normal_texture.clone();
    }
    if change.emissive_texture.is_some() {
        target.emissive_texture = change.emissive_texture.clone();
    }
    if change.occlusion_texture.is_some() {
        target.occlusion_texture = change.occlusion_texture.clone();
    }
    if change.normal_scale.is_some() {
        target.normal_scale = change.normal_scale;
    }
    if change.occlusion_strength.is_some() {
        target.occlusion_strength = change.occlusion_strength;
    }
    if change.double_sided.is_some() {
        target.double_sided = change.double_sided;
    }
}

/// Синтетический экземпляр, через который раскрывается шаблон spawn.
const TEMPLATE_INSTANCE: &str = "template";

/// Префаб, готовый к [`crate::World::spawn`]. Объекты хранятся с id относительно корня:
/// корень — пустой id, дети — `a.b`.
#[derive(Clone, Debug)]
pub struct PrefabTemplate {
    pub(crate) stem: String,
    assets: Vec<SceneAsset>,
    pub(crate) objects: Vec<SceneObject>,
    /// Индекс родителя каждого объекта в `objects`.
    pub(crate) parents: Vec<Option<usize>>,
    /// Индексы `objects` в порядке «родитель раньше потомка».
    pub(crate) order: Vec<usize>,
    pub(crate) root: usize,
    /// Причина, по которой префаб нельзя заспавнить; `None`, если ограничений нет.
    spawn_rejection: Option<SceneError>,
}

impl PrefabTemplate {
    /// Раскрывает префаб `path` и проверяет его структуру. `read_text` читает текст
    /// `.rveprefab` по виртуальному пути, в том числе вложенных префабов.
    ///
    /// Ошибка возвращается только для префаба, который не читается, не разбирается или
    /// структурно неверен. Ограничения spawn-префаба проверяет [`Self::check_spawnable`]:
    /// префаб с камерой или окружением допустим как экземпляр сцены.
    pub fn load(
        path: &VirtualPath,
        read_text: &mut dyn FnMut(&VirtualPath) -> Result<String, String>,
    ) -> Result<Self, SceneError> {
        let document = SceneDocument {
            assets: vec![SceneAsset {
                path: path.clone(),
                archive: None,
                max_instances: None,
            }],
            objects: Vec::new(),
            instances: vec![PrefabInstance {
                id: TEMPLATE_INSTANCE.to_owned(),
                name: None,
                parent: None,
                prefab: path.clone(),
                transform: None,
                overrides: Vec::new(),
            }],
            source_label: format!("/{}", path.as_str()),
            kind: DocumentKind::Prefab,
            expanded: false,
        };
        let mut load = |child: &VirtualPath| -> Result<SceneDocument, SceneError> {
            let label = format!("/{}", child.as_str());
            let text =
                read_text(child).map_err(|message| SceneError::new(&label, None, message))?;
            SceneDocument::parse_prefab(label, &text)
        };
        let expanded = expand(&document, &mut load)?.document;
        let label = expanded.source_label().to_owned();
        let spawn_rejection = spawn_rejection(&expanded.objects, &label);

        let index_of = expanded
            .objects
            .iter()
            .enumerate()
            .map(|(index, object)| (object.id.as_str(), index))
            .collect::<HashMap<_, _>>();
        let parents = expanded
            .objects
            .iter()
            .map(|object| object.parent.as_deref().map(|parent| index_of[parent]))
            .collect::<Vec<_>>();
        let ids = expanded
            .objects
            .iter()
            .map(|object| object.id.as_str())
            .collect::<Vec<_>>();
        let order = topological_order(&parents, &ids, &label)?;
        let root = parents
            .iter()
            .position(Option::is_none)
            .expect("проверенный префаб имеет корень");
        let objects = expanded
            .objects
            .iter()
            .map(|object| SceneObject {
                id: local_id(&object.id).to_owned(),
                parent: object
                    .parent
                    .as_deref()
                    .map(|parent| local_id(parent).to_owned()),
                ..object.clone()
            })
            .collect();
        let stem = Path::new(path.as_str())
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_owned();
        Ok(Self {
            stem,
            assets: expanded.assets,
            objects,
            parents,
            order,
            root,
            spawn_rejection,
        })
    }

    /// Проверяет, что префаб допустим для spawn: в нём нет camera, directional_light и
    /// environment. Dynamic rigid body и character controller допускаются только у корня:
    /// это уже проверяет валидация документа при [`Self::load`]. Ошибка не означает, что
    /// префаб нельзя использовать как экземпляр сцены.
    pub fn check_spawnable(&self) -> Result<(), SceneError> {
        self.spawn_rejection.clone().map_or(Ok(()), Err)
    }

    /// Ресурсы шаблона, включая ресурсы вложенных префабов.
    pub fn assets(&self) -> &[SceneAsset] {
        &self.assets
    }

    /// Объекты шаблона с id относительно корня; корень — объект без `parent`.
    pub fn objects(&self) -> &[SceneObject] {
        &self.objects
    }

    /// Пути моделей мешевых коллайдеров объектов шаблона: без повторов, по возрастанию пути.
    /// Остальные модели шаблона нужны только для отрисовки.
    pub fn mesh_collider_models(&self) -> BTreeSet<&str> {
        self.objects
            .iter()
            .filter_map(|object| {
                mesh_collider_model(object.collider, object.mesh_renderer.as_ref())
            })
            .collect()
    }

    /// Имя файла префаба без расширения: основа для генерируемых id.
    pub(crate) fn stem(&self) -> &str {
        &self.stem
    }
}

/// Id объекта относительно корня шаблона: корень — пустой, дети — `a.b`.
fn local_id(expanded: &str) -> &str {
    expanded
        .strip_prefix(TEMPLATE_INSTANCE)
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or("")
}

/// Первый объект раскрытого префаба, который запрещён для spawn: camera, directional_light или
/// environment.
fn spawn_rejection(objects: &[SceneObject], label: &str) -> Option<SceneError> {
    objects.iter().find_map(|object| {
        let forbidden = if object.camera.is_some() {
            "camera"
        } else if object.directional_light.is_some() {
            "directional_light"
        } else if object.environment.is_some() {
            "environment"
        } else {
            return None;
        };
        let local = local_id(&object.id);
        Some(SceneError::new(
            label,
            (!local.is_empty()).then_some(local),
            format!("spawn-префаб не может содержать {forbidden}"),
        ))
    })
}

/// Раскрытый id: сегменты `[A-Za-z0-9_-]{1,64}` через точку, всего до 255 байт.
pub fn valid_expanded_id(id: &str) -> bool {
    id.len() <= MAX_EXPANDED_ID_BYTES
        && id.split('.').all(|segment| valid_ascii_name(segment, true))
}

/// Локальные правила `[[instances]]`: формат, уникальность id, объявленный префаб и
/// корректность overrides. Глобальные правила проверяются на раскрытом документе.
pub(crate) fn validate_instances(
    document: &SceneDocument,
    assets: &HashSet<VirtualPath>,
    object_ids: &HashMap<&str, usize>,
) -> Result<(), SceneError> {
    let source = document.source_label();
    let mut ids = HashSet::with_capacity(document.instances.len());
    for instance in &document.instances {
        let id = instance.id.as_str();
        if !valid_ascii_name(id, true) {
            return Err(SceneError::new(
                source,
                Some(id),
                "id экземпляра должен соответствовать [A-Za-z0-9_-]{1,64}",
            ));
        }
        if object_ids.contains_key(id) || !ids.insert(id) {
            return Err(SceneError::new(
                source,
                Some(id),
                "id экземпляра уже используется",
            ));
        }
        require_asset_path(source, id, assets, &instance.prefab, "префаб")?;
        require_extension(source, id, &instance.prefab, PREFAB_EXTENSION, "префаб")?;
        let mut targets = HashSet::with_capacity(instance.overrides.len());
        for change in &instance.overrides {
            if !valid_expanded_id(&change.target) {
                return Err(SceneError::new(
                    source,
                    Some(id),
                    format!("некорректный target override {:?}", change.target),
                ));
            }
            if !targets.insert(change.target.as_str()) {
                return Err(SceneError::new(
                    source,
                    Some(id),
                    format!("override для {:?} повторяется", change.target),
                ));
            }
            if let Some(mesh) = &change.mesh_renderer {
                if let Some(model) = &mesh.model {
                    require_asset_path(source, id, assets, model, "модель override")?;
                    require_extension(source, id, model, "fbx", "модель override")?;
                }
                if let Some(texture) = &mesh.texture {
                    require_asset_path(source, id, assets, texture, "текстура override")?;
                    require_extension(source, id, texture, "png", "текстура override")?;
                }
                if let Some(material) = &mesh.material {
                    validate_material_override(source, id, assets, material)?;
                }
            }
            if let Some(settings) = change
                .camera
                .as_ref()
                .and_then(|camera| camera.settings.as_ref())
            {
                require_asset_path(source, id, assets, settings, "настройки камеры override")?;
                require_extension(
                    source,
                    id,
                    settings,
                    "camerasettings",
                    "настройки камеры override",
                )?;
            }
            if let Some(environment) = &change.environment {
                validate_environment(source, id, assets, environment)?;
            }
            if let Some(audio) = &change.audio_source {
                validate_audio_source(source, id, assets, audio)?;
            }
            if let Some(emitter) = &change.particle_emitter {
                validate_particle_emitter(source, id, assets, emitter)?;
            }
        }
    }
    Ok(())
}

/// Префаб, раскрытый в собственных локальных id.
struct LocalPrefab {
    objects: Vec<SceneObject>,
    assets: Vec<SceneAsset>,
    root: String,
}

struct Flat {
    objects: Vec<SceneObject>,
    assets: Vec<SceneAsset>,
    origins: BTreeMap<String, ObjectOrigin>,
}

struct Expander<'a, 'b> {
    load: &'a mut PrefabLoader<'b>,
    cache: HashMap<VirtualPath, Rc<LocalPrefab>>,
    stack: Vec<VirtualPath>,
    used: BTreeSet<VirtualPath>,
}

impl Expander<'_, '_> {
    /// Собственные объекты документа и раскрытые экземпляры в порядке объявления.
    fn flatten(
        &mut self,
        document: &SceneDocument,
        skip_overrides: Option<&str>,
    ) -> Result<Flat, SceneError> {
        let source = document.source_label();
        let mut objects = document.objects.clone();
        let mut assets = AssetMerge::new(&document.assets);
        let mut origins = BTreeMap::new();
        for instance in &document.instances {
            let prefab = self.prefab(&instance.prefab, source)?;
            assets.merge(&prefab.assets, source)?;
            let mut local = prefab.objects.clone();
            if skip_overrides != Some(instance.id.as_str()) {
                for change in &instance.overrides {
                    let object = local
                        .iter_mut()
                        .find(|object| object.id == change.target)
                        .ok_or_else(|| {
                            SceneError::new(
                                source,
                                Some(&instance.id),
                                format!(
                                    "override target {:?} не найден в префабе /{}",
                                    change.target,
                                    instance.prefab.as_str()
                                ),
                            )
                        })?;
                    apply_override(object, change)
                        .map_err(|message| SceneError::new(source, Some(&instance.id), message))?;
                }
            }
            for mut object in local {
                let target = object.id.clone();
                let is_root = target == prefab.root;
                object.id = map_id(&instance.id, &prefab.root, &target);
                if is_root {
                    object.parent = instance.parent.clone();
                    if let Some(name) = &instance.name {
                        object.name = (!name.is_empty()).then(|| name.clone());
                    }
                    if let Some(transform) = instance.transform {
                        object.transform = transform;
                    }
                } else {
                    object.parent = object
                        .parent
                        .as_deref()
                        .map(|parent| map_id(&instance.id, &prefab.root, parent));
                }
                if object.id.len() > MAX_EXPANDED_ID_BYTES {
                    return Err(SceneError::new(
                        source,
                        Some(&instance.id),
                        format!(
                            "раскрытый id {:?} длиннее {MAX_EXPANDED_ID_BYTES} байт",
                            object.id
                        ),
                    ));
                }
                origins.insert(
                    object.id.clone(),
                    ObjectOrigin {
                        instance: instance.id.clone(),
                        target,
                    },
                );
                objects.push(object);
            }
        }
        Ok(Flat {
            objects,
            assets: assets.finish(),
            origins,
        })
    }

    /// Раскрытый и полностью проверенный префаб; результат кэшируется по пути.
    fn prefab(&mut self, path: &VirtualPath, source: &str) -> Result<Rc<LocalPrefab>, SceneError> {
        if let Some(position) = self.stack.iter().position(|entry| entry == path) {
            let chain = self.stack[position..]
                .iter()
                .chain([path])
                .map(|path| format!("/{}", path.as_str()))
                .collect::<Vec<_>>()
                .join(" -> ");
            return Err(SceneError::new(
                source,
                None,
                format!("цикл префабов: {chain}"),
            ));
        }
        if let Some(prefab) = self.cache.get(path) {
            return Ok(Rc::clone(prefab));
        }
        if self.stack.len() >= MAX_PREFAB_DEPTH {
            return Err(SceneError::new(
                source,
                None,
                format!(
                    "вложенность префабов больше {MAX_PREFAB_DEPTH} на /{}",
                    path.as_str()
                ),
            ));
        }
        let document = (self.load)(path)?;
        if !document.is_prefab() {
            return Err(SceneError::new(
                document.source_label(),
                None,
                "документ загружен не как префаб",
            ));
        }
        self.used.insert(path.clone());
        self.stack.push(path.clone());
        let flat = self.flatten(&document, None);
        self.stack.pop();
        let flat = flat?;
        let checked = SceneDocument {
            assets: flat.assets,
            objects: flat.objects,
            instances: Vec::new(),
            source_label: document.source_label().to_owned(),
            kind: DocumentKind::Prefab,
            expanded: true,
        };
        checked.validate()?;
        let root = checked
            .objects
            .iter()
            .find(|object| object.parent.is_none())
            .map(|object| object.id.clone())
            .expect("проверенный префаб имеет один корень");
        let prefab = Rc::new(LocalPrefab {
            objects: checked.objects,
            assets: checked.assets,
            root,
        });
        self.cache.insert(path.clone(), Rc::clone(&prefab));
        Ok(prefab)
    }
}

/// Id объекта префаба в документе экземпляра: корень — id экземпляра, остальные —
/// `<экземпляр>.<локальный id>`.
fn map_id(instance: &str, root: &str, local: &str) -> String {
    if local == root {
        instance.to_owned()
    } else {
        format!("{instance}.{local}")
    }
}

/// Объединение `[[assets]]` по пути. Явный архив документа с экземпляром побеждает;
/// разные явные архивы из разных префабов — ошибка. Записи вложенных префабов не несут
/// `max_instances`: лимит берётся только из записи самого документа.
struct AssetMerge {
    entries: Vec<SceneAsset>,
    /// Индекс записи и признак, что её архив задан самим документом.
    index: HashMap<VirtualPath, (usize, bool)>,
}

impl AssetMerge {
    fn new(own: &[SceneAsset]) -> Self {
        let index = own
            .iter()
            .enumerate()
            .map(|(position, asset)| (asset.path.clone(), (position, asset.archive.is_some())))
            .collect();
        Self {
            entries: own.to_vec(),
            index,
        }
    }

    fn merge(&mut self, assets: &[SceneAsset], source: &str) -> Result<(), SceneError> {
        for asset in assets {
            match self.index.get(&asset.path).copied() {
                None => {
                    self.index
                        .insert(asset.path.clone(), (self.entries.len(), false));
                    // Лимит экземпляров задаёт только запись самого документа, не вложенного префаба.
                    self.entries.push(SceneAsset {
                        max_instances: None,
                        ..asset.clone()
                    });
                }
                Some((_, true)) => {}
                Some((position, false)) => {
                    let existing = &mut self.entries[position];
                    match (&existing.archive, &asset.archive) {
                        (Some(left), Some(right)) if left != right => {
                            return Err(SceneError::new(
                                source,
                                None,
                                format!(
                                    "ресурс /{} назначен префабами в разные архивы {left:?} и {right:?}",
                                    asset.path.as_str()
                                ),
                            ));
                        }
                        (None, Some(archive)) => existing.archive = Some(archive.clone()),
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(self) -> Vec<SceneAsset> {
        self.entries
    }
}

#[cfg(test)]
mod tests;
