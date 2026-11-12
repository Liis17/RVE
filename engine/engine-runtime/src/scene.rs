use crate::game_ui::{PreparedUi, UiImageData, load_ui};
use crate::pipeline::{GpuSceneTable, SkinLevel};
use engine_animation::PaletteRemap;
use engine_assets::{
    Assets, Model, ModelAlphaMode, ModelAnimation, ModelMaterial, ModelSkin, TextureData,
};
use engine_files::VirtualPath;
use engine_physics::{
    ModelGeometry, ModelMesh as PhysicsModelMesh, PhysicsSceneBinding, PhysicsSceneDesc, StepDriver,
};
use engine_project::PhysicsSettings;
use engine_render::{
    AlphaMode, EnvironmentSource, Material, MaterialId, MeshId, ParticleBlend, ParticleBurst,
    ParticleCollision, ParticleColorKey, ParticleEmitterDesc, ParticleFlipbook, ParticleShape,
    ParticleSizeKey, RenderError, Renderer, ShaderId, SkinVertex, SurfaceLighting, SurfaceProperty,
    SurfacePropertyKind, SurfaceShaderSource, SurfaceTextureFallback, TextureColorSpace, TextureId,
    Vertex,
};
use engine_resources::ResourceHandle;
use engine_scene::prefab::PREFAB_EXTENSION;
use engine_scene::{
    AnimatorComponent, EnvironmentComponent, LiteScript, LodGroup, MaterialAlphaMode,
    MaterialAsset, MaterialOverride, MeshRenderer, ObjectHandle, ParticleBlendMode,
    ParticleEmitterComponent, ParticleShapeComponent, PrefabTemplate, SceneDocument, SceneError,
    ShaderAsset, ShaderColorSpace, ShaderLighting, ShaderPropertyKind, TextureFallback, World,
    expand,
};
use engine_scene::{CameraSettings, CubeLut, PostProcessSettings, resolve_post_process};
use engine_script::{PrefabCatalog, PrefabKind, PrefabSpec};
use glam::Vec3;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;
use std::sync::Arc;

/// Ресурсы одного объекта с мешом: уровни LOD (модель и текстура) и замена материала.
#[derive(Clone, Debug)]
pub(crate) struct ObjectModel {
    pub levels: Vec<(String, Option<String>)>,
    pub material: Option<MaterialOverride>,
    /// Per-FBX-part `.rvemat` paths; `None` retains the FBX material.
    pub material_slots: Vec<Option<String>>,
    /// Объект имеет `animator`: его модель получает скиннинг, если у модели есть скелет.
    pub animated: bool,
}

pub(crate) struct PreparedRenderAssets {
    pub models: HashMap<String, Model>,
    pub textures: HashMap<String, TextureData>,
    pub material_assets: HashMap<String, Result<MaterialAsset, String>>,
    pub shader_assets: HashMap<String, Result<ShaderAsset, String>>,
    pub environments: HashMap<String, Arc<EnvironmentSource>>,
    /// Клипы из `animator.clips`, по виртуальному пути файла.
    pub clips: HashMap<String, Arc<Vec<ModelAnimation>>>,
    /// Объекты сцены с мешами и их хендлы.
    pub objects: Vec<(ObjectHandle, ObjectModel)>,
    /// Быстрые префабы по пути. Элемент `i` относится к объекту шаблона `i`; `None` — у объекта
    /// нет меша.
    pub prefabs: BTreeMap<String, Vec<Option<ObjectModel>>>,
    /// Эмиттеры частиц объектов сцены; создаются при загрузке сцены.
    pub particles: Vec<(ObjectHandle, ParticleSource)>,
    /// Эмиттеры быстрых префабов по пути. Элемент `i` относится к объекту шаблона `i`.
    pub prefab_particles: BTreeMap<String, Vec<Option<ParticleSource>>>,
    /// Картинки UI сцены (включая быстрые префабы) для загрузки в GPU при старте.
    pub ui_images: BTreeMap<String, Arc<UiImageData>>,
    /// Исходники `.glsl` экранных шейдеров камер по пути. Заполняются только для игрового клиента:
    /// headless-сервер шейдеры не читает.
    pub screen_shaders: BTreeMap<String, String>,
    /// 3D LUT цветокоррекции камер по каноническому пути `.cube`. Как и экранные шейдеры,
    /// читаются только игровым клиентом.
    pub color_luts: BTreeMap<String, CubeLut>,
}

#[derive(Default)]
pub(crate) struct PreparedMaterialAssets {
    pub models: HashMap<String, Model>,
    pub textures: HashMap<String, TextureData>,
    pub material_assets: HashMap<String, Result<MaterialAsset, String>>,
    pub shader_assets: HashMap<String, Result<ShaderAsset, String>>,
}

#[derive(Clone, Copy)]
pub(crate) struct MaterialAssetView<'a> {
    pub models: &'a HashMap<String, Model>,
    pub textures: &'a HashMap<String, TextureData>,
    pub material_assets: &'a HashMap<String, Result<MaterialAsset, String>>,
    pub shader_assets: &'a HashMap<String, Result<ShaderAsset, String>>,
    fallback: Option<&'a PreparedRenderAssets>,
}

impl<'a> MaterialAssetView<'a> {
    pub(crate) fn from_scene(assets: &'a PreparedRenderAssets) -> Self {
        Self {
            models: &assets.models,
            textures: &assets.textures,
            material_assets: &assets.material_assets,
            shader_assets: &assets.shader_assets,
            fallback: None,
        }
    }

    pub(crate) fn from_prefab(
        assets: &'a PreparedMaterialAssets,
        fallback: Option<&'a PreparedRenderAssets>,
    ) -> Self {
        Self {
            models: &assets.models,
            textures: &assets.textures,
            material_assets: &assets.material_assets,
            shader_assets: &assets.shader_assets,
            fallback,
        }
    }

    fn model(&self, path: &str) -> Option<&'a Model> {
        self.models
            .get(path)
            .or_else(|| self.fallback.and_then(|assets| assets.models.get(path)))
    }

    fn texture(&self, path: &str) -> Option<&'a TextureData> {
        self.textures
            .get(path)
            .or_else(|| self.fallback.and_then(|assets| assets.textures.get(path)))
    }

    fn material_asset(&self, path: &str) -> Option<&'a Result<MaterialAsset, String>> {
        self.material_assets.get(path).or_else(|| {
            self.fallback
                .and_then(|assets| assets.material_assets.get(path))
        })
    }

    fn shader_asset(&self, path: &str) -> Option<&'a Result<ShaderAsset, String>> {
        self.shader_assets.get(path).or_else(|| {
            self.fallback
                .and_then(|assets| assets.shader_assets.get(path))
        })
    }
}

/// Эмиттер частиц объекта: параметры сцены и путь текстуры, уже загруженной в `textures`.
pub(crate) struct ParticleSource {
    pub emitter: ParticleEmitterComponent,
    pub texture: Option<String>,
}

/// Быстрые префабы сцены: шаблоны для регистрации в скриптах и лимиты создания.
#[derive(Default)]
pub(crate) struct PreparedPrefabs {
    pub templates: BTreeMap<String, Arc<PrefabTemplate>>,
    pub catalog: PrefabCatalog,
}

pub(crate) struct PreparedScene {
    pub world: World,
    pub name: String,
    pub sources: BTreeMap<String, String>,
    pub render_assets: PreparedRenderAssets,
    pub prefabs: PreparedPrefabs,
    /// Документы `.rveui` сцены и быстрых префабов с их ассетами.
    pub ui: PreparedUi,
    /// Хэш раскрытого документа сцены для проверки совместимости клиента и сервера.
    pub scene_hash: [u8; 32],
}

impl PreparedScene {
    /// Геометрия моделей для коллайдеров мешей. Её используют и построение физики сцены, и
    /// вставка тел spawn-объектов, поэтому строится она один раз.
    pub(crate) fn physics_geometry(&self) -> Vec<ModelGeometry> {
        self.render_assets
            .models
            .iter()
            .map(|(path, model)| model_geometry(path, model))
            .collect()
    }

    pub(crate) fn build_physics(
        &self,
        geometry: &[ModelGeometry],
        settings: &PhysicsSettings,
    ) -> Result<(StepDriver, Vec<PhysicsSceneBinding>), String> {
        PhysicsSceneDesc::from_world(
            &self.world,
            geometry,
            Vec3::from_array(settings.gravity),
            settings.fixed_hz,
            settings.max_substeps as usize,
        )
        .map(|mut desc| {
            desc.collision_matrix = settings.collision_matrix.masks;
            desc
        })
        .and_then(|desc| desc.build())
        .map_err(|error| error.to_string())
    }
}

pub(crate) fn load_scene(
    resources: &ResourceHandle,
    assets: &Assets,
    scene_path: &str,
) -> Result<PreparedScene, String> {
    let contents = read_utf8(resources, scene_path)?;
    let document =
        SceneDocument::parse(scene_path, &contents).map_err(|error| error.to_string())?;
    let expansion = expand(&document, &mut |path| load_prefab(resources, path))
        .map_err(|error| error.to_string())?;
    let instanced = expansion.prefabs;
    let mut expanded = expansion.document;
    let scene_hash = engine_network::replication::scene_hash(&expanded);
    let prefabs = load_fast_prefabs(resources, &mut expanded, &instanced)?;
    let ui = load_ui(
        resources,
        assets,
        expanded.assets.iter().map(|asset| &asset.path),
    )?;
    let mut world = World::from_document(&expanded).map_err(|error| error.to_string())?;
    world
        .resolve_physics_materials(&mut |path: &VirtualPath| {
            read_utf8(resources, &format!("/{}", path.as_str()))
        })
        .map_err(|error| error.to_string())?;
    let mut prepared = prepare_scene(resources, assets, scene_path, world, prefabs, scene_hash)?;
    prepared.render_assets.ui_images = ui.images.clone();
    prepared.ui = ui;
    Ok(prepared)
}

/// Раскрывает каждый `.rveprefab` из `[[assets]]` сцены в шаблон для spawn. Ассеты шаблонов
/// добавляются в список ассетов сцены: звук и сборка видят их так же, как ассеты сцены.
///
/// Префаб с камерой, светом или окружением не регистрируется для spawn, но это не ошибка
/// загрузки: он может быть экземпляром сцены. Если такой префаб объявлен только в `[[assets]]`,
/// пишется один Warn. Ошибки чтения и разбора шаблона остаются ошибками загрузки.
fn load_fast_prefabs(
    resources: &ResourceHandle,
    document: &mut SceneDocument,
    instanced: &[VirtualPath],
) -> Result<PreparedPrefabs, String> {
    let fast = document
        .assets
        .iter()
        .filter(|asset| is_prefab_path(&asset.path))
        .map(|asset| (asset.path.clone(), asset.max_instances))
        .collect::<Vec<_>>();
    let mut prefabs = PreparedPrefabs::default();
    for (path, max_instances) in fast {
        let template = load_prefab_template(resources, &path)?;
        if let Err(error) = template.check_spawnable() {
            if !instanced.contains(&path) {
                log::warn!(
                    "префаб {} объявлен в [[assets]], не используется экземплярами и не может быть заспавнен: {error}",
                    path.as_str()
                );
            }
            continue;
        }
        for asset in template.assets() {
            if !document.assets.iter().any(|known| known.path == asset.path) {
                document.assets.push(asset.clone());
            }
        }
        prefabs.catalog.insert(
            path.as_str(),
            PrefabSpec {
                kind: PrefabKind::Fast,
                max_instances,
            },
        );
        prefabs
            .templates
            .insert(path.as_str().to_owned(), Arc::new(template));
    }
    Ok(prefabs)
}

fn is_prefab_path(path: &VirtualPath) -> bool {
    Path::new(path.as_str())
        .extension()
        .is_some_and(|extension| extension == PREFAB_EXTENSION)
}

/// Префаб читается через тот же `ResourceHandle`, что и сцена, поэтому dev и release
/// загружают его одинаково.
fn load_prefab(
    resources: &ResourceHandle,
    path: &VirtualPath,
) -> Result<SceneDocument, SceneError> {
    let label = format!("/{}", path.as_str());
    let text = read_utf8(resources, &label).map_err(|message| SceneError {
        source: label.clone(),
        object_id: None,
        message,
    })?;
    SceneDocument::parse_prefab(label, &text)
}

fn prepare_scene(
    resources: &ResourceHandle,
    assets: &Assets,
    scene_path: &str,
    world: World,
    prefabs: PreparedPrefabs,
    scene_hash: [u8; 32],
) -> Result<PreparedScene, String> {
    prepare_scene_with_model_loader(
        resources,
        assets,
        scene_path,
        world,
        prefabs,
        scene_hash,
        |path| assets.load_model(path).map_err(|error| error.to_string()),
    )
}

/// Ресурсы объектов сцены и шаблонов префабов, собранные за один проход. Модели и текстуры
/// общие: объект сцены и префаб с одним FBX загружают его один раз.
#[derive(Default)]
struct RenderAssetSet {
    models: HashMap<String, Model>,
    textures: HashMap<String, TextureData>,
    material_paths: BTreeSet<String>,
    material_assets: HashMap<String, Result<MaterialAsset, String>>,
    shader_assets: HashMap<String, Result<ShaderAsset, String>>,
    environments: HashMap<String, Arc<EnvironmentSource>>,
    clips: HashMap<String, Arc<Vec<ModelAnimation>>>,
    lite_paths: BTreeSet<String>,
}

/// Компоненты объекта, из которых готовятся его ресурсы. Одинаковы для объекта мира и для
/// объекта шаблона префаба.
struct ObjectParts<'a> {
    environment: Option<&'a EnvironmentComponent>,
    mesh_renderer: Option<&'a MeshRenderer>,
    lod_group: Option<&'a LodGroup>,
    animator: Option<&'a AnimatorComponent>,
    lite_scripts: &'a [LiteScript],
}

fn prepare_scene_with_model_loader(
    resources: &ResourceHandle,
    assets: &Assets,
    scene_path: &str,
    mut world: World,
    prefabs: PreparedPrefabs,
    scene_hash: [u8; 32],
    mut load_model: impl FnMut(&str) -> Result<Model, String>,
) -> Result<PreparedScene, String> {
    // Пути файлов камер собираются до изменения мира; каждый файл читается один раз.
    let cameras = world
        .objects()
        .iter()
        .filter_map(|object| {
            let camera = object.camera.as_ref()?;
            Some((
                object.handle,
                camera
                    .settings
                    .as_ref()
                    .map(|path| path.as_str().to_owned()),
                camera
                    .post_process
                    .as_ref()
                    .map(|path| path.as_str().to_owned()),
            ))
        })
        .collect::<Vec<_>>();
    let mut loaded_settings = HashMap::<String, CameraSettings>::new();
    for path in cameras
        .iter()
        .filter_map(|(_, settings, _)| settings.as_ref())
    {
        if !loaded_settings.contains_key(path) {
            let source = read_utf8(resources, path)?;
            let settings =
                CameraSettings::parse(path, &source).map_err(|error| error.to_string())?;
            loaded_settings.insert(path.clone(), settings);
        }
    }
    let mut loaded_post_process = HashMap::<String, PostProcessSettings>::new();
    for path in cameras.iter().filter_map(|(_, _, post)| post.as_ref()) {
        if !loaded_post_process.contains_key(path) {
            let source = read_utf8(resources, path)?;
            let settings =
                PostProcessSettings::parse(path, &source).map_err(|error| error.to_string())?;
            loaded_post_process.insert(path.clone(), settings);
        }
    }
    for (handle, settings_path, _) in &cameras {
        if let Some(path) = settings_path {
            world
                .set_camera_settings(*handle, loaded_settings[path])
                .map_err(|error| error.to_string())?;
        }
    }
    for (handle, _, post_path) in &cameras {
        // Файл постобработки побеждает; без него действует устаревший блок [color].
        let camera_settings = world.camera_settings(*handle).copied().unwrap_or_default();
        let post_file = post_path.as_ref().map(|path| &loaded_post_process[path]);
        world
            .set_post_process(*handle, resolve_post_process(&camera_settings, post_file))
            .map_err(|error| error.to_string())?;
    }

    let mut set = RenderAssetSet::default();
    let mut objects = Vec::new();
    let mut particles = Vec::new();
    for object in world.objects() {
        let parts = ObjectParts {
            environment: object.environment.as_ref(),
            mesh_renderer: object.mesh_renderer.as_ref(),
            lod_group: object.lod_group.as_ref(),
            animator: object.animator.as_ref(),
            lite_scripts: &object.lite_scripts,
        };
        if let Some(model) = set.add_object(assets, scene_path, parts, &mut load_model)? {
            objects.push((object.handle, model));
        }
        if let Some(source) =
            set.add_particle(assets, scene_path, object.particle_emitter.as_ref())?
        {
            particles.push((object.handle, source));
        }
    }
    let mut prefab_objects = BTreeMap::new();
    let mut prefab_particles = BTreeMap::new();
    for (path, template) in &prefabs.templates {
        warn_unsupported_prefab_animators(path, template);
        let mut levels = Vec::with_capacity(template.objects().len());
        let mut emitters = Vec::with_capacity(template.objects().len());
        for object in template.objects() {
            let parts = ObjectParts {
                environment: object.environment.as_ref(),
                mesh_renderer: object.mesh_renderer.as_ref(),
                lod_group: object.lod_group.as_ref(),
                animator: None,
                lite_scripts: &object.lite_scripts,
            };
            levels.push(set.add_object(assets, scene_path, parts, &mut load_model)?);
            emitters.push(set.add_particle(
                assets,
                scene_path,
                object.particle_emitter.as_ref(),
            )?);
        }
        prefab_objects.insert(path.clone(), levels);
        prefab_particles.insert(path.clone(), emitters);
    }

    // Все объявленные `.rvemat` готовятся, а не только слоты: скрипт назначает их через set_material.
    for path in world.asset_paths() {
        if path.as_str().to_ascii_lowercase().ends_with(".rvemat") {
            set.material_paths.insert(path.as_str().to_owned());
        }
    }
    set.load_surface_assets(resources, assets, scene_path);

    let RenderAssetSet {
        models,
        textures,
        material_assets,
        shader_assets,
        environments,
        clips,
        lite_paths,
        material_paths: _,
    } = set;
    let mut sources = BTreeMap::new();
    for path in lite_paths {
        sources.insert(path.clone(), read_utf8(resources, &path)?);
    }

    Ok(PreparedScene {
        world,
        name: scene_path.to_owned(),
        sources,
        ui: PreparedUi::default(),
        scene_hash,
        render_assets: PreparedRenderAssets {
            screen_shaders: BTreeMap::new(),
            color_luts: BTreeMap::new(),
            models,
            textures,
            material_assets,
            shader_assets,
            environments,
            clips,
            objects,
            prefabs: prefab_objects,
            particles,
            prefab_particles,
            ui_images: BTreeMap::new(),
        },
        prefabs,
    })
}

impl RenderAssetSet {
    /// Готовит ресурсы одного объекта: HDR-окружение, модели уровней LOD, текстуры материалов и
    /// Lua-исходники. Возвращает модель только для объекта с мешом.
    fn add_object(
        &mut self,
        assets: &Assets,
        scene_path: &str,
        parts: ObjectParts<'_>,
        load_model: &mut impl FnMut(&str) -> Result<Model, String>,
    ) -> Result<Option<ObjectModel>, String> {
        if let Some(path) = parts
            .environment
            .and_then(|environment| environment.hdr.as_ref())
        {
            let key = path.as_str().to_owned();
            if let std::collections::hash_map::Entry::Vacant(entry) = self.environments.entry(key) {
                let image = assets
                    .load_hdr(entry.key())
                    .map_err(|error| format!("{scene_path}: {error}"))?;
                entry.insert(Arc::new(EnvironmentSource::Equirect {
                    width: image.width,
                    height: image.height,
                    rgb: image.rgb,
                }));
            }
        }
        if let Some(animator) = parts.animator {
            for path in &animator.clips {
                if !self.clips.contains_key(path.as_str()) {
                    let animations = assets
                        .load_animations(&format!("/{}", path.as_str()))
                        .map_err(|error| format!("{scene_path}: {error}"))?;
                    self.clips
                        .insert(path.as_str().to_owned(), Arc::new(animations));
                }
            }
        }
        let mut model_object = None;
        if let Some(mesh) = parts.mesh_renderer {
            let mut levels = vec![(
                mesh.model.as_str().to_owned(),
                mesh.texture.as_ref().map(|path| path.as_str().to_owned()),
            )];
            if let Some(group) = parts.lod_group {
                for level in &group.levels {
                    let model_path = VirtualPath::parse(&level.model)
                        .map_err(|error| format!("{scene_path}: {error}"))?
                        .as_str()
                        .to_owned();
                    let texture_path = level
                        .texture
                        .as_deref()
                        .or_else(|| mesh.texture.as_ref().map(|path| path.as_str()));
                    let replacement_texture = texture_path
                        .map(|path| {
                            VirtualPath::parse(path)
                                .map(|path| path.as_str().to_owned())
                                .map_err(|error| format!("{scene_path}: {error}"))
                        })
                        .transpose()?;
                    levels.push((model_path, replacement_texture));
                }
            }
            let material = mesh.material.clone();
            for path in mesh.materials.iter().flatten() {
                self.material_paths.insert(path.as_str().to_owned());
            }
            for (model_path, _) in &levels {
                if !self.models.contains_key(model_path) {
                    let model =
                        load_model(model_path).map_err(|error| format!("{scene_path}: {error}"))?;
                    self.models.insert(model_path.clone(), model);
                }
            }
            let scene_texture_paths = levels
                .iter()
                .filter_map(|(_, texture)| texture.clone())
                .chain(material.iter().flat_map(|material| {
                    [
                        material.metallic_roughness_texture.as_ref(),
                        material.normal_texture.as_ref(),
                        material.emissive_texture.as_ref(),
                        material.occlusion_texture.as_ref(),
                    ]
                    .into_iter()
                    .flatten()
                    .map(|path| path.as_str().to_owned())
                }))
                .collect::<Vec<_>>();
            for path in scene_texture_paths {
                if let std::collections::hash_map::Entry::Vacant(entry) = self.textures.entry(path)
                {
                    let texture = assets
                        .load_texture(entry.key())
                        .map_err(|error| format!("{scene_path}: {error}"))?;
                    entry.insert(texture);
                }
            }
            model_object = Some(ObjectModel {
                levels,
                material,
                material_slots: mesh
                    .materials
                    .iter()
                    .map(|path| path.as_ref().map(|path| path.as_str().to_owned()))
                    .collect(),
                animated: parts.animator.is_some(),
            });
        }
        for script in parts.lite_scripts {
            self.lite_paths.insert(script.path.as_str().to_owned());
        }
        Ok(model_object)
    }

    /// Загружает текстуру эмиттера частиц объекта, если он есть.
    fn add_particle(
        &mut self,
        assets: &Assets,
        scene_path: &str,
        emitter: Option<&ParticleEmitterComponent>,
    ) -> Result<Option<ParticleSource>, String> {
        let Some(emitter) = emitter else {
            return Ok(None);
        };
        let texture = emitter
            .texture
            .as_ref()
            .map(|path| path.as_str().to_owned());
        if let Some(path) = &texture
            && let std::collections::hash_map::Entry::Vacant(entry) =
                self.textures.entry(path.clone())
        {
            let data = assets
                .load_texture(entry.key())
                .map_err(|error| format!("{scene_path}: {error}"))?;
            entry.insert(data);
        }
        Ok(Some(ParticleSource {
            emitter: emitter.clone(),
            texture,
        }))
    }

    fn load_surface_assets(&mut self, resources: &ResourceHandle, assets: &Assets, owner: &str) {
        let mut shader_paths = BTreeSet::new();
        for path in &self.material_paths {
            let parsed = read_utf8(resources, &format!("/{path}"))
                .and_then(|source| {
                    MaterialAsset::parse(format!("/{path}"), &source)
                        .map_err(|error| error.to_string())
                })
                .map_err(|error| format!("{owner}: {error}"));
            if let Ok(material) = &parsed
                && let Some(shader) = &material.shader
            {
                shader_paths.insert(shader.as_str().to_owned());
            }
            self.material_assets.insert(path.clone(), parsed);
        }
        for path in &shader_paths {
            let parsed = read_utf8(resources, &format!("/{path}"))
                .and_then(|source| {
                    ShaderAsset::parse(format!("/{path}"), &source)
                        .map_err(|error| error.to_string())
                })
                .map_err(|error| format!("{owner}: {error}"));
            self.shader_assets.insert(path.clone(), parsed);
        }
        let mut texture_paths = BTreeSet::new();
        for material in self
            .material_assets
            .values()
            .filter_map(|result| result.as_ref().ok())
        {
            let schema = match &material.shader {
                Some(path) => match self
                    .shader_assets
                    .get(path.as_str())
                    .and_then(|result| result.as_ref().ok())
                {
                    Some(shader) => shader.layout(),
                    None => continue,
                },
                None => engine_scene::ShaderSchema::lit(),
            };
            let resolved = material.resolve(&schema);
            texture_paths.extend(
                resolved
                    .textures
                    .into_iter()
                    .flatten()
                    .map(|(path, _)| path.as_str().to_owned()),
            );
        }
        for path in texture_paths {
            if self.textures.contains_key(&path) {
                continue;
            }
            match assets.load_texture(&path) {
                Ok(texture) => {
                    self.textures.insert(path, texture);
                }
                Err(error) => {
                    log::error!("[material] {owner}: texture dependency /{path}: {error}");
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MaterialTextureSource {
    Model(usize),
    Scene(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ResolvedMaterial {
    pub(crate) base_color: [f32; 3],
    pub(crate) metallic: f32,
    pub(crate) roughness: f32,
    pub(crate) emissive: [f32; 3],
    pub(crate) opacity: f32,
    pub(crate) alpha_mode: AlphaMode,
    pub(crate) double_sided: bool,
    pub(crate) base_color_texture: Option<MaterialTextureSource>,
    pub(crate) metallic_roughness_texture: Option<MaterialTextureSource>,
    pub(crate) normal_texture: Option<MaterialTextureSource>,
    pub(crate) emissive_texture: Option<MaterialTextureSource>,
    pub(crate) occlusion_texture: Option<MaterialTextureSource>,
    pub(crate) normal_scale: f32,
    pub(crate) occlusion_strength: f32,
    pub(crate) shader_path: Option<String>,
    pub(crate) shader_source: Option<SurfaceShaderSource>,
    pub(crate) user_values: [[f32; 4]; 16],
    pub(crate) user_texture_sources: [Option<ResolvedUserTexture>; 8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedUserTexture {
    pub(crate) path: String,
    pub(crate) color_space: TextureColorSpace,
}

/// GPU-кэши сцены, которые переживают загрузку: текстуры, шейдеры и запасной материал. Render thread
/// забирает их, чтобы заменять материалы объектов без повторной загрузки текстур.
#[derive(Default)]
pub(crate) struct MaterialUploadCaches {
    pub(crate) textures: GpuTextureCache<TextureId>,
    pub(crate) shaders: HashMap<String, Result<ShaderId, String>>,
    pub(crate) error_material: Option<MaterialId>,
    ownership: MaterialResourceOwnership<MaterialId, TextureId, ShaderId>,
}

pub(crate) struct GpuTextureCache<T> {
    model: HashMap<(String, usize, TextureColorSpace), T>,
    scene: HashMap<(String, TextureColorSpace), T>,
}

impl<T> Default for GpuTextureCache<T> {
    fn default() -> Self {
        Self {
            model: HashMap::new(),
            scene: HashMap::new(),
        }
    }
}

pub(crate) struct MaterialResourceDependencies<T, S> {
    textures: HashSet<T>,
    shader: Option<S>,
}

pub(crate) type MaterialDependencyMap =
    HashMap<MaterialId, MaterialResourceDependencies<TextureId, ShaderId>>;

struct MaterialResourceOwnership<M, T, S> {
    materials: HashMap<M, MaterialResourceDependencies<T, S>>,
    texture_refs: HashMap<T, usize>,
    shader_refs: HashMap<S, usize>,
    scene_textures: HashMap<u64, HashSet<T>>,
    scene_texture_refs: HashMap<T, usize>,
}

impl<M, T, S> Default for MaterialResourceOwnership<M, T, S> {
    fn default() -> Self {
        Self {
            materials: HashMap::new(),
            texture_refs: HashMap::new(),
            shader_refs: HashMap::new(),
            scene_textures: HashMap::new(),
            scene_texture_refs: HashMap::new(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct MaterialResourceReleases<T: Eq + Hash, S: Eq + Hash> {
    textures: HashSet<T>,
    shaders: HashSet<S>,
}

impl<M, T, S> MaterialResourceOwnership<M, T, S>
where
    M: Copy + Eq + Hash,
    T: Copy + Eq + Hash,
    S: Copy + Eq + Hash,
{
    fn retain_material(&mut self, material: M, dependencies: MaterialResourceDependencies<T, S>) {
        if self.materials.contains_key(&material) {
            return;
        }
        for texture in &dependencies.textures {
            *self.texture_refs.entry(*texture).or_default() += 1;
        }
        if let Some(shader) = dependencies.shader {
            *self.shader_refs.entry(shader).or_default() += 1;
        }
        self.materials.insert(material, dependencies);
    }

    fn release_materials(
        &mut self,
        materials: impl IntoIterator<Item = M>,
    ) -> MaterialResourceReleases<T, S> {
        let mut released = MaterialResourceReleases::default();
        for material in materials {
            let Some(dependencies) = self.materials.remove(&material) else {
                continue;
            };
            for texture in dependencies.textures {
                let exhausted = if let Some(refs) = self.texture_refs.get_mut(&texture) {
                    *refs = refs.saturating_sub(1);
                    *refs == 0
                } else {
                    false
                };
                if exhausted {
                    self.texture_refs.remove(&texture);
                    if !self.scene_texture_refs.contains_key(&texture) {
                        released.textures.insert(texture);
                    }
                }
            }
            if let Some(shader) = dependencies.shader {
                let exhausted = if let Some(refs) = self.shader_refs.get_mut(&shader) {
                    *refs = refs.saturating_sub(1);
                    *refs == 0
                } else {
                    false
                };
                if exhausted {
                    self.shader_refs.remove(&shader);
                    released.shaders.insert(shader);
                }
            }
        }
        released
    }

    fn retain_scene_textures(&mut self, owner: u64, textures: HashSet<T>) {
        if textures.is_empty() || self.scene_textures.contains_key(&owner) {
            return;
        }
        for texture in &textures {
            *self.scene_texture_refs.entry(*texture).or_default() += 1;
        }
        self.scene_textures.insert(owner, textures);
    }

    fn release_scene_textures(
        &mut self,
        owners: impl IntoIterator<Item = u64>,
    ) -> MaterialResourceReleases<T, S> {
        let mut released = MaterialResourceReleases::default();
        for owner in owners {
            let Some(textures) = self.scene_textures.remove(&owner) else {
                continue;
            };
            for texture in textures {
                let exhausted = if let Some(refs) = self.scene_texture_refs.get_mut(&texture) {
                    *refs = refs.saturating_sub(1);
                    *refs == 0
                } else {
                    false
                };
                if exhausted {
                    self.scene_texture_refs.remove(&texture);
                    if !self.texture_refs.contains_key(&texture) {
                        released.textures.insert(texture);
                    }
                }
            }
        }
        released
    }

    fn keeps_texture(&self, texture: T) -> bool {
        self.texture_refs.contains_key(&texture) || self.scene_texture_refs.contains_key(&texture)
    }

    fn keeps_shader(&self, shader: S) -> bool {
        self.shader_refs.contains_key(&shader)
    }
}

impl<T> GpuTextureCache<T>
where
    T: Copy + Eq + Hash,
{
    fn ids(&self) -> HashSet<T> {
        self.model
            .values()
            .chain(self.scene.values())
            .copied()
            .collect()
    }

    fn remove_id(&mut self, id: T) {
        self.model.retain(|_, cached| *cached != id);
        self.scene.retain(|_, cached| *cached != id);
    }
}

impl<T: Eq + Hash, S: Eq + Hash> Default for MaterialResourceReleases<T, S> {
    fn default() -> Self {
        Self {
            textures: HashSet::new(),
            shaders: HashSet::new(),
        }
    }
}

impl MaterialUploadCaches {
    pub(crate) fn retain_materials(
        &mut self,
        dependencies: HashMap<MaterialId, MaterialResourceDependencies<TextureId, ShaderId>>,
    ) {
        for (material, dependencies) in dependencies {
            self.ownership.retain_material(material, dependencies);
        }
    }

    pub(crate) fn retain_scene_textures(&mut self, owner: u64, textures: HashSet<TextureId>) {
        self.ownership.retain_scene_textures(owner, textures);
    }

    pub(crate) fn release_materials(
        &mut self,
        renderer: &mut Renderer,
        materials: impl IntoIterator<Item = MaterialId>,
    ) {
        let released = self.ownership.release_materials(materials);
        self.destroy_released_resources(renderer, released);
    }

    pub(crate) fn release_scene_textures(
        &mut self,
        renderer: &mut Renderer,
        owners: impl IntoIterator<Item = u64>,
    ) {
        let released = self.ownership.release_scene_textures(owners);
        self.destroy_released_resources(renderer, released);
    }

    fn destroy_released_resources(
        &mut self,
        renderer: &mut Renderer,
        released: MaterialResourceReleases<TextureId, ShaderId>,
    ) {
        for texture in released.textures {
            match renderer.destroy_texture(texture) {
                Ok(()) => self.textures.remove_id(texture),
                Err(error) => log::error!("текстура материала не уничтожена: {error}"),
            }
        }
        for shader in released.shaders {
            let path = self.shaders.iter().find_map(|(path, result)| {
                result
                    .as_ref()
                    .ok()
                    .filter(|cached| **cached == shader)
                    .map(|_| path.clone())
            });
            let Some(path) = path else { continue };
            match renderer.destroy_surface_shader(shader) {
                Ok(()) => {
                    self.shaders.remove(&path);
                }
                Err(error) => log::error!("шейдер материала /{path} не уничтожен: {error}"),
            }
        }
    }

    pub(crate) fn prune_unowned(&mut self, renderer: &mut Renderer) {
        let textures = self
            .textures
            .ids()
            .into_iter()
            .filter(|texture| !self.ownership.keeps_texture(*texture))
            .collect::<Vec<_>>();
        for texture in textures {
            match renderer.destroy_texture(texture) {
                Ok(()) => self.textures.remove_id(texture),
                Err(error) => log::error!("текстура материала не уничтожена: {error}"),
            }
        }

        let shaders = self
            .shaders
            .iter()
            .filter_map(|(path, result)| match result {
                Ok(shader) if self.ownership.keeps_shader(*shader) => None,
                Ok(_) | Err(_) => Some(path.clone()),
            })
            .collect::<Vec<_>>();
        for path in shaders {
            let destroy = match self.shaders.get(&path) {
                Some(Ok(shader)) => match renderer.destroy_surface_shader(*shader) {
                    Ok(()) => true,
                    Err(error) => {
                        log::error!("шейдер материала /{path} не уничтожен: {error}");
                        false
                    }
                },
                Some(Err(_)) => true,
                None => false,
            };
            if destroy {
                self.shaders.remove(&path);
            }
        }
    }
}

/// Возвращает GPU-ресурс из кэша или создаёт его один раз.
fn get_or_insert_gpu<K, T>(
    cache: &mut HashMap<K, T>,
    key: K,
    create: impl FnOnce() -> Result<T, RenderError>,
) -> Result<T, RenderError>
where
    K: Eq + Hash,
    T: Copy,
{
    if let Some(&texture) = cache.get(&key) {
        return Ok(texture);
    }
    let texture = create()?;
    cache.insert(key, texture);
    Ok(texture)
}

pub(crate) fn resolve_material(
    model: &ModelMaterial,
    replacement_texture: Option<&str>,
    override_material: Option<&MaterialOverride>,
) -> ResolvedMaterial {
    let override_texture = |path: Option<&engine_files::VirtualPath>| {
        path.map(|path| MaterialTextureSource::Scene(path.as_str().to_owned()))
    };
    ResolvedMaterial {
        base_color: override_material
            .and_then(|material| material.base_color)
            .unwrap_or(model.base_color),
        metallic: override_material
            .and_then(|material| material.metallic)
            .unwrap_or(model.metallic),
        roughness: override_material
            .and_then(|material| material.roughness)
            .unwrap_or(model.roughness),
        emissive: override_material
            .and_then(|material| material.emissive)
            .unwrap_or(model.emissive),
        opacity: override_material
            .and_then(|material| material.opacity)
            .unwrap_or(model.opacity),
        alpha_mode: match override_material.and_then(|material| material.alpha_mode) {
            Some(MaterialAlphaMode::Opaque) => AlphaMode::Opaque,
            Some(MaterialAlphaMode::Mask) => AlphaMode::Mask {
                cutoff: override_material
                    .and_then(|material| material.alpha_cutoff)
                    .unwrap_or(0.5),
            },
            Some(MaterialAlphaMode::Blend) => AlphaMode::Blend,
            None => match model.alpha_mode {
                ModelAlphaMode::Opaque => AlphaMode::Opaque,
                ModelAlphaMode::Blend => AlphaMode::Blend,
            },
        },
        double_sided: override_material
            .and_then(|material| material.double_sided)
            .unwrap_or(model.double_sided),
        base_color_texture: replacement_texture
            .map(|path| MaterialTextureSource::Scene(path.to_owned()))
            .or_else(|| model.base_color_texture.map(MaterialTextureSource::Model)),
        metallic_roughness_texture: override_texture(
            override_material.and_then(|material| material.metallic_roughness_texture.as_ref()),
        )
        .or_else(|| {
            model
                .metallic_roughness_texture
                .map(MaterialTextureSource::Model)
        }),
        normal_texture: override_texture(
            override_material.and_then(|material| material.normal_texture.as_ref()),
        )
        .or_else(|| model.normal_texture.map(MaterialTextureSource::Model)),
        emissive_texture: override_texture(
            override_material.and_then(|material| material.emissive_texture.as_ref()),
        )
        .or_else(|| model.emissive_texture.map(MaterialTextureSource::Model)),
        occlusion_texture: override_texture(
            override_material.and_then(|material| material.occlusion_texture.as_ref()),
        )
        .or_else(|| model.occlusion_texture.map(MaterialTextureSource::Model)),
        normal_scale: override_material
            .and_then(|material| material.normal_scale)
            .unwrap_or(model.normal_scale),
        occlusion_strength: override_material
            .and_then(|material| material.occlusion_strength)
            .unwrap_or(model.occlusion_strength),
        shader_path: None,
        shader_source: None,
        user_values: [[0.0; 4]; 16],
        user_texture_sources: std::array::from_fn(|_| None),
    }
}

pub(crate) fn resolve_material_asset(
    _model: &ModelMaterial,
    material_path: &str,
    material: &MaterialAsset,
    shader_asset: Option<&ShaderAsset>,
    replacement_texture: Option<&str>,
    override_material: Option<&MaterialOverride>,
) -> Result<ResolvedMaterial, String> {
    if material.shader.is_some() && shader_asset.is_none() {
        return Err(format!(
            "surface shader для /{material_path} отсутствует или повреждён"
        ));
    }
    let schema = shader_asset
        .map(ShaderAsset::layout)
        .unwrap_or_else(engine_scene::ShaderSchema::lit);
    let values = material.resolve(&schema);
    for warning in &values.warnings {
        log::warn!("[material] /{material_path}: {warning}");
    }
    let alpha = match values.alpha.mode {
        MaterialAlphaMode::Opaque => AlphaMode::Opaque,
        MaterialAlphaMode::Mask => AlphaMode::Mask {
            cutoff: values.alpha.cutoff,
        },
        MaterialAlphaMode::Blend => AlphaMode::Blend,
    };
    let mut resolved = if schema.builtin_lit {
        let lit = values
            .lit
            .ok_or_else(|| "встроенная Lit-схема не разрешила значения".to_owned())?;
        let mut result = resolve_material(&ModelMaterial::default(), None, None);
        result.base_color = lit.base_color;
        result.metallic = lit.metallic;
        result.roughness = lit.roughness;
        result.emissive = lit.emissive;
        result.opacity = lit.opacity;
        result.normal_scale = lit.normal_scale;
        result.occlusion_strength = lit.occlusion_strength;
        result.alpha_mode = alpha;
        result.double_sided = values.double_sided;
        let source = |slot: usize| {
            values.textures[slot]
                .as_ref()
                .map(|(path, _)| MaterialTextureSource::Scene(path.as_str().to_owned()))
        };
        result.base_color_texture = replacement_texture
            .map(|path| MaterialTextureSource::Scene(path.to_owned()))
            .or_else(|| source(0));
        result.metallic_roughness_texture = source(1);
        result.normal_texture = source(2);
        result.emissive_texture = source(3);
        result.occlusion_texture = source(4);
        if let Some(override_material) = override_material {
            result.base_color = override_material.base_color.unwrap_or(result.base_color);
            result.metallic = override_material.metallic.unwrap_or(result.metallic);
            result.roughness = override_material.roughness.unwrap_or(result.roughness);
            result.emissive = override_material.emissive.unwrap_or(result.emissive);
            result.opacity = override_material.opacity.unwrap_or(result.opacity);
            result.alpha_mode = match override_material.alpha_mode {
                Some(MaterialAlphaMode::Opaque) => AlphaMode::Opaque,
                Some(MaterialAlphaMode::Mask) => AlphaMode::Mask {
                    cutoff: override_material
                        .alpha_cutoff
                        .unwrap_or(values.alpha.cutoff),
                },
                Some(MaterialAlphaMode::Blend) => AlphaMode::Blend,
                None => result.alpha_mode,
            };
            result.double_sided = override_material
                .double_sided
                .unwrap_or(result.double_sided);
            result.metallic_roughness_texture = override_material
                .metallic_roughness_texture
                .as_ref()
                .map(|path| MaterialTextureSource::Scene(path.as_str().to_owned()))
                .or(result.metallic_roughness_texture);
            result.normal_texture = override_material
                .normal_texture
                .as_ref()
                .map(|path| MaterialTextureSource::Scene(path.as_str().to_owned()))
                .or(result.normal_texture);
            result.emissive_texture = override_material
                .emissive_texture
                .as_ref()
                .map(|path| MaterialTextureSource::Scene(path.as_str().to_owned()))
                .or(result.emissive_texture);
            result.occlusion_texture = override_material
                .occlusion_texture
                .as_ref()
                .map(|path| MaterialTextureSource::Scene(path.as_str().to_owned()))
                .or(result.occlusion_texture);
            result.normal_scale = override_material
                .normal_scale
                .unwrap_or(result.normal_scale);
            result.occlusion_strength = override_material
                .occlusion_strength
                .unwrap_or(result.occlusion_strength);
        }
        result
    } else {
        let shader = shader_asset.expect("custom shader presence validated above");
        let source = surface_shader_source(shader);
        let mut result = resolve_material(&ModelMaterial::default(), None, None);
        result.alpha_mode = alpha;
        result.double_sided = values.double_sided;
        result.shader_path = material
            .shader
            .as_ref()
            .map(|path| path.as_str().to_owned());
        result.user_values = values.uniforms;
        result.user_texture_sources = std::array::from_fn(|slot| {
            values.textures[slot]
                .as_ref()
                .map(|(path, color_space)| ResolvedUserTexture {
                    path: path.as_str().to_owned(),
                    color_space: match color_space {
                        ShaderColorSpace::Srgb => TextureColorSpace::Srgb,
                        ShaderColorSpace::Linear => TextureColorSpace::Linear,
                    },
                })
        });
        result.shader_source = Some(source);
        result
    };
    // Lit material assets form the base; MeshRenderer.texture and the legacy override are applied
    // above. User surface shaders intentionally ignore both legacy mechanisms.
    if schema.builtin_lit {
        resolved.base_color_texture = replacement_texture
            .map(|path| MaterialTextureSource::Scene(path.to_owned()))
            .or(resolved.base_color_texture);
    }
    Ok(resolved)
}

pub(crate) fn surface_shader_source(shader: &ShaderAsset) -> SurfaceShaderSource {
    let schema = shader.layout();
    SurfaceShaderSource {
        lighting: match shader.lighting {
            ShaderLighting::Lit => SurfaceLighting::Lit,
            ShaderLighting::Unlit => SurfaceLighting::Unlit,
        },
        properties: schema
            .properties
            .into_iter()
            .map(|layout| {
                let property = layout.property;
                SurfaceProperty {
                    name: property.name,
                    kind: match property.kind {
                        ShaderPropertyKind::Float => SurfacePropertyKind::Float,
                        ShaderPropertyKind::Vec2 => SurfacePropertyKind::Vec2,
                        ShaderPropertyKind::Vec3 => SurfacePropertyKind::Vec3,
                        ShaderPropertyKind::Vec4 => SurfacePropertyKind::Vec4,
                        ShaderPropertyKind::Color => SurfacePropertyKind::Color,
                        ShaderPropertyKind::Texture => SurfacePropertyKind::Texture,
                    },
                    slot: layout.slot,
                    color_space: property.color_space.map(|space| match space {
                        ShaderColorSpace::Srgb => TextureColorSpace::Srgb,
                        ShaderColorSpace::Linear => TextureColorSpace::Linear,
                    }),
                    fallback: match property.fallback.unwrap_or_default() {
                        TextureFallback::White => SurfaceTextureFallback::White,
                        TextureFallback::Black => SurfaceTextureFallback::Black,
                        TextureFallback::Normal => SurfaceTextureFallback::Normal,
                    },
                }
            })
            .collect(),
        code: shader.code.clone(),
    }
}

pub(crate) fn read_utf8(resources: &ResourceHandle, path: &str) -> Result<String, String> {
    let bytes = resources
        .read_all(path)
        .map_err(|error| format!("{path}: {error}"))?
        .wait()
        .map_err(|error| format!("{path}: {error}"))?;
    String::from_utf8(bytes).map_err(|error| format!("{path}: исходник должен быть UTF-8: {error}"))
}

/// Геометрия модели для коллайдеров мешей; одинакова для сцены и для тяжёлых префабов.
pub(crate) fn model_geometry(path: &str, model: &Model) -> ModelGeometry {
    ModelGeometry {
        path: path.to_owned(),
        meshes: model
            .meshes
            .iter()
            .map(|mesh| PhysicsModelMesh {
                positions: mesh
                    .vertices
                    .iter()
                    .map(|vertex| Vec3::from_array(vertex.position))
                    .collect(),
                indices: mesh.indices.clone(),
            })
            .collect(),
    }
}

/// Шаблон префаба, прочитанный через ресурсы, с разрешёнными физическими материалами; ошибки
/// чтения и разбора остаются текстом.
pub(crate) fn load_prefab_template(
    resources: &ResourceHandle,
    path: &VirtualPath,
) -> Result<PrefabTemplate, String> {
    let mut template = PrefabTemplate::load(path, &mut |child: &VirtualPath| {
        read_utf8(resources, &format!("/{}", child.as_str()))
    })
    .map_err(|error| error.to_string())?;
    template
        .resolve_physics_materials(&mut |child: &VirtualPath| {
            read_utf8(resources, &format!("/{}", child.as_str()))
        })
        .map_err(|error| error.to_string())?;
    Ok(template)
}

/// CPU-ресурсы шаблона тяжёлого префаба: то же, что для объектов сцены.
pub(crate) struct PreparedTemplate {
    pub models: HashMap<String, Model>,
    pub textures: HashMap<String, TextureData>,
    pub material_assets: HashMap<String, Result<MaterialAsset, String>>,
    pub shader_assets: HashMap<String, Result<ShaderAsset, String>>,
    /// Элемент `i` относится к объекту шаблона `i`; `None` — у объекта нет меша.
    pub levels: Vec<Option<ObjectModel>>,
    /// Эмиттеры объектов шаблона; элемент `i` относится к объекту `i`.
    pub particles: Vec<Option<ParticleSource>>,
    pub sources: BTreeMap<String, String>,
    /// `.rveui` шаблона и их ассеты: регистрируются в скриптах и загружаются в GPU при `PrefabReady`.
    pub ui: PreparedUi,
}

/// Заспавненные объекты пока не получают анимацию: аниматор шаблона не готовится, и предупреждение
/// выводится один раз на шаблон при его подготовке.
fn warn_unsupported_prefab_animators(path: &str, template: &PrefabTemplate) {
    if template
        .objects()
        .iter()
        .any(|object| object.animator.is_some())
    {
        log::warn!(
            "[animation] {path}: аниматор объектов префаба не поддерживается при спавне; объекты рисуются без анимации"
        );
    }
}

/// Готовит модели, текстуры и Lua-исходники шаблона. `stop` проверяется перед каждым объектом,
/// моделью и исходником: прерванная подготовка возвращает ошибку, а не частичный результат.
pub(crate) fn prepare_template(
    resources: &ResourceHandle,
    assets: &Assets,
    label: &str,
    template: &PrefabTemplate,
    stop: &dyn Fn() -> bool,
) -> Result<PreparedTemplate, String> {
    const STOPPED: &str = "подготовка префаба прервана";
    warn_unsupported_prefab_animators(label, template);
    let mut set = RenderAssetSet::default();
    let mut levels = Vec::with_capacity(template.objects().len());
    let mut particles = Vec::with_capacity(template.objects().len());
    for object in template.objects() {
        if stop() {
            return Err(STOPPED.to_owned());
        }
        let parts = ObjectParts {
            environment: object.environment.as_ref(),
            mesh_renderer: object.mesh_renderer.as_ref(),
            lod_group: object.lod_group.as_ref(),
            animator: None,
            lite_scripts: &object.lite_scripts,
        };
        let model = set.add_object(assets, label, parts, &mut |path: &str| {
            if stop() {
                return Err(STOPPED.to_owned());
            }
            assets.load_model(path).map_err(|error| error.to_string())
        })?;
        levels.push(model);
        particles.push(set.add_particle(assets, label, object.particle_emitter.as_ref())?);
    }
    if stop() {
        return Err(STOPPED.to_owned());
    }
    for asset in template.assets() {
        if asset
            .path
            .as_str()
            .to_ascii_lowercase()
            .ends_with(".rvemat")
        {
            set.material_paths.insert(asset.path.as_str().to_owned());
        }
    }
    set.load_surface_assets(resources, assets, label);
    let RenderAssetSet {
        models,
        textures,
        material_assets,
        shader_assets,
        environments: _,
        clips: _,
        lite_paths,
        material_paths: _,
    } = set;
    let mut sources = BTreeMap::new();
    for path in lite_paths {
        if stop() {
            return Err(STOPPED.to_owned());
        }
        sources.insert(path.clone(), read_utf8(resources, &path)?);
    }
    let ui = load_ui(
        resources,
        assets,
        template.assets().iter().map(|asset| &asset.path),
    )?;
    Ok(PreparedTemplate {
        models,
        textures,
        material_assets,
        shader_assets,
        levels,
        particles,
        sources,
        ui,
    })
}

/// Создание экранных шейдеров; шов для тестов без GPU. Реализован для [`Renderer`].
pub(crate) trait ScreenShaderGpu {
    type Shader: Copy;

    fn create_screen_shader(&mut self, source: &str) -> Result<Self::Shader, RenderError>;
}

impl ScreenShaderGpu for Renderer {
    type Shader = engine_render::ScreenShaderId;

    fn create_screen_shader(&mut self, source: &str) -> Result<Self::Shader, RenderError> {
        Renderer::create_screen_shader(self, source)
    }
}

/// Создаёт экранные шейдеры по исходникам. Ошибка компиляции записывается в лог, шейдер
/// пропускается, сцена продолжает работать.
pub(crate) fn upload_screen_shaders<G: ScreenShaderGpu>(
    gpu: &mut G,
    sources: &BTreeMap<String, String>,
) -> HashMap<String, G::Shader> {
    let mut ids = HashMap::with_capacity(sources.len());
    for (path, source) in sources {
        match gpu.create_screen_shader(source) {
            Ok(id) => {
                ids.insert(path.clone(), id);
            }
            Err(error) => log::error!("экранный шейдер /{path} пропущен: {error}"),
        }
    }
    ids
}

/// Пути `.glsl` экранных шейдеров всех камер мира без повторов; префабные камеры раскрыты в мир.
pub(crate) fn screen_shader_paths(world: &World) -> BTreeSet<String> {
    world
        .objects()
        .iter()
        .filter_map(|object| object.camera.as_ref())
        .flat_map(|camera| &camera.screen_shaders)
        .map(|shader| shader.shader.as_str().to_owned())
        .collect()
}

/// Читает исходники экранных шейдеров камер. Файл, который не читается или не UTF-8, пропускается
/// с записью в лог, как и ошибка компиляции.
pub(crate) fn load_screen_shader_sources(
    resources: &ResourceHandle,
    world: &World,
) -> BTreeMap<String, String> {
    let mut sources = BTreeMap::new();
    for path in screen_shader_paths(world) {
        match read_utf8(resources, &path) {
            Ok(source) => {
                sources.insert(path, source);
            }
            Err(error) => log::error!("экранный шейдер не прочитан: {error}"),
        }
    }
    sources
}

/// Создание LUT цветокоррекции; шов для тестов без GPU. Реализован для [`Renderer`].
pub(crate) trait ColorLutGpu {
    type Lut: Copy;

    fn create_color_lut(&mut self, lut: &CubeLut) -> Result<Self::Lut, RenderError>;
}

impl ColorLutGpu for Renderer {
    type Lut = engine_render::ColorLutId;

    fn create_color_lut(&mut self, lut: &CubeLut) -> Result<Self::Lut, RenderError> {
        Renderer::create_color_lut(self, lut.size(), lut.texels())
    }
}

/// Создаёт LUT по разобранным файлам. Ошибка записывается в лог, LUT пропускается, и камера
/// рисуется без неё, как с пропущенным экранным шейдером.
pub(crate) fn upload_color_luts<G: ColorLutGpu>(
    gpu: &mut G,
    luts: &BTreeMap<String, CubeLut>,
) -> HashMap<String, G::Lut> {
    let mut ids = HashMap::with_capacity(luts.len());
    for (path, lut) in luts {
        match gpu.create_color_lut(lut) {
            Ok(id) => {
                ids.insert(path.clone(), id);
            }
            Err(error) => log::error!("LUT /{path} пропущена: {error}"),
        }
    }
    ids
}

/// Канонические пути LUT цветокоррекции всех камер мира без повторов.
pub(crate) fn color_lut_paths(world: &World) -> BTreeSet<String> {
    world
        .objects()
        .iter()
        .filter_map(|object| object.post_process.as_ref())
        .filter_map(|post| post.color_grading.lut_path())
        .map(|path| path.as_str().to_owned())
        .collect()
}

/// Читает и разбирает LUT камер. Файл, который не читается или не разбирается, пропускается с
/// записью в лог: `rve-build` проверяет LUT при сборке, а в разработке сцена продолжает работать.
pub(crate) fn load_color_luts(
    resources: &ResourceHandle,
    world: &World,
) -> BTreeMap<String, CubeLut> {
    let mut luts = BTreeMap::new();
    for path in color_lut_paths(world) {
        let parsed = read_utf8(resources, &path).and_then(|source| {
            CubeLut::parse(format!("/{path}"), &source).map_err(|error| error.to_string())
        });
        match parsed {
            Ok(lut) => {
                luts.insert(path, lut);
            }
            Err(error) => log::error!("LUT не загружена: {error}"),
        }
    }
    luts
}

/// Как загрузка сцены отдаёт частицы объектов: эмиттеры создаются сразу, как при старте, либо
/// отдаются описаниями, которые замена сцены создаёт по финальному хендлу объекта.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SceneParticles {
    Emitters,
    Descriptions,
}

trait SceneUploadCleanupGpu {
    type Mesh: Copy + Eq + Hash;
    type Material: Copy + Eq + Hash;
    type Skin: Copy + Eq + Hash;
    type ScreenShader: Copy + Eq + Hash;
    type ColorLut: Copy + Eq + Hash;
    type Particle: Copy + Eq + Hash;
    type UiTexture: Copy + Eq + Hash;
    type Texture: Copy + Eq + Hash;
    type SurfaceShader: Copy + Eq + Hash;

    fn destroy_mesh(&mut self, id: Self::Mesh) -> Result<(), String>;
    fn destroy_material(&mut self, id: Self::Material) -> Result<(), String>;
    fn destroy_skin(&mut self, id: Self::Skin) -> Result<(), String>;
    fn destroy_screen_shader(&mut self, id: Self::ScreenShader) -> Result<(), String>;
    fn destroy_color_lut(&mut self, id: Self::ColorLut) -> Result<(), String>;
    fn destroy_particle(&mut self, id: Self::Particle) -> Result<(), String>;
    fn destroy_ui_texture(&mut self, id: Self::UiTexture) -> Result<(), String>;
    fn destroy_texture(&mut self, id: Self::Texture) -> Result<(), String>;
    fn destroy_surface_shader(&mut self, id: Self::SurfaceShader) -> Result<(), String>;
}

impl SceneUploadCleanupGpu for Renderer {
    type Mesh = MeshId;
    type Material = MaterialId;
    type Skin = engine_render::SkinId;
    type ScreenShader = engine_render::ScreenShaderId;
    type ColorLut = engine_render::ColorLutId;
    type Particle = engine_render::ParticleEmitterId;
    type UiTexture = engine_render::UiTextureId;
    type Texture = TextureId;
    type SurfaceShader = ShaderId;

    fn destroy_mesh(&mut self, id: Self::Mesh) -> Result<(), String> {
        Renderer::destroy_mesh(self, id).map_err(|error| error.to_string())
    }

    fn destroy_material(&mut self, id: Self::Material) -> Result<(), String> {
        Renderer::destroy_material(self, id).map_err(|error| error.to_string())
    }

    fn destroy_skin(&mut self, id: Self::Skin) -> Result<(), String> {
        Renderer::destroy_skin_instance(self, id).map_err(|error| error.to_string())
    }

    fn destroy_screen_shader(&mut self, id: Self::ScreenShader) -> Result<(), String> {
        Renderer::destroy_screen_shader(self, id).map_err(|error| error.to_string())
    }

    fn destroy_color_lut(&mut self, id: Self::ColorLut) -> Result<(), String> {
        Renderer::destroy_color_lut(self, id).map_err(|error| error.to_string())
    }

    fn destroy_particle(&mut self, id: Self::Particle) -> Result<(), String> {
        Renderer::destroy_particle_emitter(self, id).map_err(|error| error.to_string())
    }

    fn destroy_ui_texture(&mut self, id: Self::UiTexture) -> Result<(), String> {
        Renderer::free_ui_texture(self, id).map_err(|error| error.to_string())
    }

    fn destroy_texture(&mut self, id: Self::Texture) -> Result<(), String> {
        Renderer::destroy_texture(self, id).map_err(|error| error.to_string())
    }

    fn destroy_surface_shader(&mut self, id: Self::SurfaceShader) -> Result<(), String> {
        Renderer::destroy_surface_shader(self, id).map_err(|error| error.to_string())
    }
}

struct SceneUploadResources<G: SceneUploadCleanupGpu> {
    meshes: HashSet<G::Mesh>,
    materials: HashSet<G::Material>,
    skins: HashSet<G::Skin>,
    screen_shaders: HashSet<G::ScreenShader>,
    color_luts: HashSet<G::ColorLut>,
    particles: HashSet<G::Particle>,
    ui_textures: HashSet<G::UiTexture>,
    textures: HashSet<G::Texture>,
    surface_shaders: HashSet<G::SurfaceShader>,
}

impl<G: SceneUploadCleanupGpu> Default for SceneUploadResources<G> {
    fn default() -> Self {
        Self {
            meshes: HashSet::new(),
            materials: HashSet::new(),
            skins: HashSet::new(),
            screen_shaders: HashSet::new(),
            color_luts: HashSet::new(),
            particles: HashSet::new(),
            ui_textures: HashSet::new(),
            textures: HashSet::new(),
            surface_shaders: HashSet::new(),
        }
    }
}

impl<G: SceneUploadCleanupGpu> SceneUploadResources<G> {
    fn rollback(&mut self, gpu: &mut G) {
        for id in self.particles.drain() {
            if let Err(error) = gpu.destroy_particle(id) {
                log::error!("failed to release partially uploaded scene particle: {error}");
            }
        }
        for id in self.skins.drain() {
            if let Err(error) = gpu.destroy_skin(id) {
                log::error!("failed to release partially uploaded scene skin: {error}");
            }
        }
        for id in self.materials.drain() {
            if let Err(error) = gpu.destroy_material(id) {
                log::error!("failed to release partially uploaded scene material: {error}");
            }
        }
        for id in self.meshes.drain() {
            if let Err(error) = gpu.destroy_mesh(id) {
                log::error!("failed to release partially uploaded scene mesh: {error}");
            }
        }
        for id in self.screen_shaders.drain() {
            if let Err(error) = gpu.destroy_screen_shader(id) {
                log::error!("failed to release partially uploaded scene screen shader: {error}");
            }
        }
        for id in self.color_luts.drain() {
            if let Err(error) = gpu.destroy_color_lut(id) {
                log::error!("failed to release partially uploaded scene color LUT: {error}");
            }
        }
        for id in self.ui_textures.drain() {
            if let Err(error) = gpu.destroy_ui_texture(id) {
                log::error!("failed to release partially uploaded scene UI texture: {error}");
            }
        }
        for id in self.surface_shaders.drain() {
            if let Err(error) = gpu.destroy_surface_shader(id) {
                log::error!("failed to release partially uploaded scene surface shader: {error}");
            }
        }
        for id in self.textures.drain() {
            if let Err(error) = gpu.destroy_texture(id) {
                log::error!("failed to release partially uploaded scene texture: {error}");
            }
        }
    }
}

fn remove_new_cached_textures<T: Copy + Eq + Hash>(
    cache: &mut GpuTextureCache<T>,
    original: &HashSet<T>,
) -> HashSet<T> {
    let created = cache
        .ids()
        .difference(original)
        .copied()
        .collect::<HashSet<_>>();
    for id in &created {
        cache.remove_id(*id);
    }
    created
}

fn restore_shader_cache<S: Copy + Eq + Hash>(
    cache: &mut HashMap<String, Result<S, String>>,
    original_paths: &HashSet<String>,
) -> HashSet<S> {
    let created = cache
        .values()
        .filter_map(|shader| shader.as_ref().ok())
        .copied()
        .collect::<HashSet<_>>();
    let original = cache
        .iter()
        .filter(|(path, _)| original_paths.contains(*path))
        .filter_map(|(_, shader)| shader.as_ref().ok())
        .copied()
        .collect::<HashSet<_>>();
    cache.retain(|path, _| original_paths.contains(path));
    created.difference(&original).copied().collect()
}

#[allow(clippy::too_many_arguments)]
fn rollback_scene_upload<G: SceneUploadCleanupGpu>(
    gpu: &mut G,
    original_textures: &HashSet<G::Texture>,
    original_shader_paths: &HashSet<String>,
    original_error_material: Option<G::Material>,
    texture_cache: &mut GpuTextureCache<G::Texture>,
    shader_cache: &mut HashMap<String, Result<G::SurfaceShader, String>>,
    material_cache: &HashMap<String, G::Material>,
    error_material: &mut Option<G::Material>,
    created: &mut SceneUploadResources<G>,
) {
    created.materials.extend(material_cache.values().copied());
    if let Some(material) = *error_material
        && Some(material) != original_error_material
    {
        created.materials.insert(material);
    }
    if let Some(material) = original_error_material {
        created.materials.remove(&material);
    }
    created.textures = remove_new_cached_textures(texture_cache, original_textures);
    created.surface_shaders = restore_shader_cache(shader_cache, original_shader_paths);
    created.rollback(gpu);
    *error_material = original_error_material;
}

pub(crate) fn upload_scene(
    renderer: &mut Renderer,
    assets: &PreparedRenderAssets,
    caches: &mut MaterialUploadCaches,
    particles: SceneParticles,
    cache_owner: u64,
) -> Result<GpuSceneTable, RenderError> {
    let original_textures = caches.textures.ids();
    let original_shader_paths = caches.shaders.keys().cloned().collect::<HashSet<_>>();
    let original_error_material = caches.error_material;
    let mut gpu_models: HashMap<String, Vec<(MeshId, MaterialId)>> = HashMap::new();
    let mut texture_cache = std::mem::take(&mut caches.textures);
    let mut shader_cache = std::mem::take(&mut caches.shaders);
    let mut material_cache = HashMap::<String, MaterialId>::new();
    let mut error_material = caches.error_material.take();
    let mut created = SceneUploadResources::<Renderer>::default();
    let mut material_dependencies = MaterialDependencyMap::new();
    let mut particle_texture_dependencies = HashSet::new();
    let material_assets = MaterialAssetView::from_scene(assets);
    // Модели уровней объектов с `animator` загружаются со скином; мосту нужны только модели уровня 0.
    let animated = animated_model_paths(assets);
    let skinned_paths = skinned_model_paths(assets);

    let result = (|| -> Result<GpuSceneTable, RenderError> {
        for (path, model) in &assets.models {
            let skinned = skinned_paths.contains(path.as_str()) && model.skeleton.is_some();
            let joint_count = model
                .skeleton
                .as_ref()
                .map_or(0, |skeleton| skeleton.joints.len() as u32);
            let mut gpu_meshes = Vec::with_capacity(model.meshes.len());
            for mesh in &model.meshes {
                let vertices = mesh
                    .vertices
                    .iter()
                    .map(|vertex| Vertex {
                        position: vertex.position,
                        normal: vertex.normal,
                        uv: vertex.uv,
                        color: [1.0; 3],
                    })
                    .collect::<Vec<_>>();
                let mesh_id = match &mesh.skin {
                    Some(skin) if skinned => renderer.create_skinned_mesh(
                        &vertices,
                        &skin_vertices(skin),
                        &mesh.indices,
                        joint_count,
                    )?,
                    _ => renderer.create_mesh(&vertices, &mesh.indices)?,
                };
                created.meshes.insert(mesh_id);
                let resolved = resolve_material(&mesh.material, None, None);
                let material_id = upload_resolved_material(
                    renderer,
                    &resolved,
                    path,
                    model,
                    material_assets,
                    &mut texture_cache,
                    &mut material_cache,
                    None,
                    Some(&mut material_dependencies),
                )?;
                gpu_meshes.push((mesh_id, material_id));
            }
            gpu_models.insert(path.clone(), gpu_meshes);
        }

        let mut table = GpuSceneTable {
            environments: assets.environments.clone(),
            animation_clips: assets.clips.clone(),
            ui_scene_owners: HashSet::from([0]),
            material_sources: assets
                .objects
                .iter()
                .map(|(handle, object)| (*handle, object.clone()))
                .collect(),
            material_templates: assets
                .prefabs
                .iter()
                .map(|(path, models)| (path.clone(), models.clone()))
                .collect(),
            ..GpuSceneTable::default()
        };
        for (path, image) in &assets.ui_images {
            let texture = renderer.create_ui_texture(image.size[0], image.size[1], &image.rgba)?;
            created.ui_textures.insert(texture);
            table.ui_images.insert(path.clone(), texture);
        }
        table.screen_shaders = upload_screen_shaders(renderer, &assets.screen_shaders);
        created
            .screen_shaders
            .extend(table.screen_shaders.values().copied());
        table.color_luts = upload_color_luts(renderer, &assets.color_luts);
        created
            .color_luts
            .extend(table.color_luts.values().copied());
        for path in &animated {
            // Мосту нужны скелет и клипы; вершины остаются только в GPU-таблице.
            if let Some(model) = assets.models.get(*path) {
                table.animation_models.insert(
                    (*path).to_owned(),
                    Arc::new(Model {
                        meshes: Vec::new(),
                        textures: Vec::new(),
                        skeleton: model.skeleton.clone(),
                        animations: model.animations.clone(),
                    }),
                );
            }
        }
        for (handle, object) in &assets.objects {
            let levels = upload_object_levels(
                renderer,
                material_assets,
                &gpu_models,
                &mut texture_cache,
                &mut shader_cache,
                &mut material_cache,
                &mut error_material,
                Some(&mut material_dependencies),
                object,
            )?;
            if object.animated
                && let Some((model_path, _)) = object.levels.first()
                && let Some(skeleton) = assets
                    .models
                    .get(model_path)
                    .and_then(|model| model.skeleton.as_ref())
            {
                // Экземпляр skinning на каждую часть каждого уровня: выходные вершины живут
                // отдельно. Уровень, скелет которого не переводится из палитры уровня 0,
                // рисуется в bind pose.
                let mut skins = Vec::with_capacity(levels.len());
                for (level, ((level_path, _), meshes)) in
                    object.levels.iter().zip(&levels).enumerate()
                {
                    let remap = if level == 0 {
                        None
                    } else {
                        let remap = assets
                            .models
                            .get(level_path)
                            .and_then(|model| model.skeleton.as_ref())
                            .and_then(|lod| PaletteRemap::new(skeleton, lod));
                        if remap.is_none() {
                            log::warn!(
                                "[animation] {level_path}: кости уровня LOD {level} не найдены в {model_path}; уровень рисуется без скиннинга"
                            );
                            skins.push(SkinLevel::default());
                            continue;
                        }
                        remap.map(Arc::new)
                    };
                    let mut instances = Vec::with_capacity(meshes.len());
                    for (mesh_id, _) in meshes {
                        let instance = renderer.create_skin_instance(*mesh_id)?;
                        created.skins.insert(instance);
                        instances.push(instance);
                    }
                    skins.push(SkinLevel { instances, remap });
                }
                table.skin_instances.insert(*handle, skins);
                table
                    .joint_counts
                    .insert(*handle, skeleton.joints.len() as u32);
            }
            table.by_object.insert(*handle, levels);
        }
        for (path, objects) in &assets.prefabs {
            let mut templates = Vec::with_capacity(objects.len());
            for object in objects {
                templates.push(match object {
                    Some(object) => upload_object_levels(
                        renderer,
                        material_assets,
                        &gpu_models,
                        &mut texture_cache,
                        &mut shader_cache,
                        &mut material_cache,
                        &mut error_material,
                        Some(&mut material_dependencies),
                        object,
                    )?,
                    None => Vec::new(),
                });
            }
            table.prefabs.insert(path.clone(), templates);
        }
        for (handle, source) in &assets.particles {
            let desc = particle_desc(renderer, assets, &mut texture_cache, source)?;
            if let Some(texture) = desc.texture {
                particle_texture_dependencies.insert(texture);
                table.particle_texture_owners.insert(*handle, cache_owner);
            }
            match particles {
                SceneParticles::Emitters => {
                    let emitter = renderer.create_particle_emitter(desc)?;
                    created.particles.insert(emitter);
                    table.particles.insert(*handle, emitter);
                }
                SceneParticles::Descriptions => {
                    table.particle_descs.insert(*handle, desc);
                }
            }
        }
        for (path, sources) in &assets.prefab_particles {
            let mut descs = Vec::with_capacity(sources.len());
            for source in sources {
                let desc = match source {
                    Some(source) => {
                        Some(particle_desc(renderer, assets, &mut texture_cache, source)?)
                    }
                    None => None,
                };
                if let Some(texture) = desc.as_ref().and_then(|desc| desc.texture) {
                    particle_texture_dependencies.insert(texture);
                    table
                        .particle_template_owners
                        .insert(path.clone(), cache_owner);
                }
                descs.push(desc);
            }
            table.particle_templates.insert(path.clone(), descs);
        }
        Ok(table)
    })();
    match result {
        Ok(table) => {
            let table_materials = table
                .by_object
                .values()
                .chain(table.prefabs.values().flatten())
                .flat_map(|levels| levels.iter().flatten().map(|(_, material)| *material))
                .collect::<HashSet<_>>();
            material_dependencies.retain(|material, _| table_materials.contains(material));
            caches.textures = texture_cache;
            caches.shaders = shader_cache;
            caches.error_material = error_material;
            caches.retain_materials(material_dependencies);
            caches.retain_scene_textures(cache_owner, particle_texture_dependencies);
            Ok(table)
        }
        Err(error) => {
            rollback_scene_upload(
                renderer,
                &original_textures,
                &original_shader_paths,
                original_error_material,
                &mut texture_cache,
                &mut shader_cache,
                &material_cache,
                &mut error_material,
                &mut created,
            );
            caches.textures = texture_cache;
            caches.shaders = shader_cache;
            caches.error_material = error_material;
            Err(error)
        }
    }
}

/// Пути моделей всех уровней LOD у объектов сцены с `animator`.
fn skinned_model_paths(assets: &PreparedRenderAssets) -> HashSet<&str> {
    assets
        .objects
        .iter()
        .map(|(_, object)| object)
        .filter(|object| object.animated)
        .flat_map(|object| object.levels.iter())
        .map(|(path, _)| path.as_str())
        .collect()
}

/// Пути моделей уровня 0 у объектов сцены с `animator`.
fn animated_model_paths(assets: &PreparedRenderAssets) -> HashSet<&str> {
    assets
        .objects
        .iter()
        .map(|(_, object)| object)
        .filter(|object| object.animated)
        .filter_map(|object| object.levels.first())
        .map(|(path, _)| path.as_str())
        .collect()
}

fn skin_vertices(skin: &ModelSkin) -> Vec<SkinVertex> {
    skin.joints
        .iter()
        .zip(&skin.weights)
        .map(|(joints, weights)| SkinVertex {
            joints: *joints,
            weights: *weights,
        })
        .collect()
}

/// Описание эмиттера с GPU-текстурой; текстура кэшируется вместе с текстурами сцены.
fn particle_desc(
    renderer: &mut Renderer,
    assets: &PreparedRenderAssets,
    texture_cache: &mut GpuTextureCache<TextureId>,
    source: &ParticleSource,
) -> Result<ParticleEmitterDesc, RenderError> {
    let texture = match &source.texture {
        Some(path) => {
            let data = assets.textures.get(path).ok_or(RenderError::InvalidState(
                "CPU-текстура частиц отсутствует в таблице GPU-ресурсов",
            ))?;
            Some(get_or_insert_gpu(
                &mut texture_cache.scene,
                (path.clone(), TextureColorSpace::Srgb),
                || {
                    renderer.create_texture_with_color_space(
                        data.width,
                        data.height,
                        &data.rgba,
                        TextureColorSpace::Srgb,
                    )
                },
            )?)
        }
        None => None,
    };
    Ok(particle_emitter_desc(&source.emitter, texture))
}

/// Переводит компонент сцены в описание эмиттера `engine-render`; проверка диапазонов — в engine-scene.
pub(crate) fn particle_emitter_desc(
    component: &ParticleEmitterComponent,
    texture: Option<TextureId>,
) -> ParticleEmitterDesc {
    ParticleEmitterDesc {
        capacity: component.capacity,
        blend: match component.blend {
            ParticleBlendMode::Additive => ParticleBlend::Additive,
            ParticleBlendMode::Alpha => ParticleBlend::Alpha,
        },
        shape: match component.shape {
            ParticleShapeComponent::Point {} => ParticleShape::Point,
            ParticleShapeComponent::Sphere { radius } => ParticleShape::Sphere { radius },
            ParticleShapeComponent::Cone { angle, radius } => ParticleShape::Cone { angle, radius },
            ParticleShapeComponent::Box { half_extents } => ParticleShape::Box { half_extents },
        },
        rate: component.rate,
        bursts: component
            .bursts
            .iter()
            .map(|burst| ParticleBurst {
                time: burst.time,
                count: burst.count,
            })
            .collect(),
        duration: component.duration,
        looping: component.looping,
        lifetime: component.lifetime,
        speed: component.speed,
        size: component.size,
        rotation: component.rotation,
        angular_velocity: component.angular_velocity,
        gravity: component.gravity,
        drag: component.drag,
        color_over_life: component
            .color_over_life
            .iter()
            .map(|key| ParticleColorKey {
                time: key.time,
                color: key.color,
            })
            .collect(),
        size_over_life: component
            .size_over_life
            .iter()
            .map(|key| ParticleSizeKey {
                time: key.time,
                size: key.size,
            })
            .collect(),
        texture,
        flipbook: ParticleFlipbook {
            columns: component.flipbook.columns,
            rows: component.flipbook.rows,
            frames_per_second: component.flipbook.frames_per_second,
        },
        soft_distance: component.soft_distance,
        collision: component.collision.map(|collision| ParticleCollision {
            restitution: collision.restitution,
            friction: collision.friction,
            thickness: collision.thickness,
        }),
        seed: component.seed,
    }
}

/// Уровни LOD объекта. Без замены материала и текстуры переиспользуются меши модели; иначе для
/// каждой части берётся материал с заменами, одинаковые материалы создаются один раз.
#[allow(clippy::too_many_arguments)]
pub(crate) fn upload_object_levels(
    renderer: &mut Renderer,
    assets: MaterialAssetView<'_>,
    gpu_models: &HashMap<String, Vec<(MeshId, MaterialId)>>,
    texture_cache: &mut GpuTextureCache<TextureId>,
    shader_cache: &mut HashMap<String, Result<ShaderId, String>>,
    material_cache: &mut HashMap<String, MaterialId>,
    error_material: &mut Option<MaterialId>,
    mut material_dependencies: Option<&mut MaterialDependencyMap>,
    object: &ObjectModel,
) -> Result<Vec<Vec<(MeshId, MaterialId)>>, RenderError> {
    let mut levels = Vec::with_capacity(object.levels.len());
    for (model_path, replacement_texture) in &object.levels {
        let model_meshes = gpu_models.get(model_path).ok_or({
            RenderError::InvalidState("CPU-модель отсутствует в таблице GPU-ресурсов")
        })?;
        if object.material.is_none()
            && replacement_texture.is_none()
            && !object.material_slots.iter().any(Option::is_some)
        {
            levels.push(model_meshes.clone());
            continue;
        }
        let model = assets.model(model_path).ok_or({
            RenderError::InvalidState("CPU-модель отсутствует в таблице подготовленных ресурсов")
        })?;
        let mut draws = Vec::with_capacity(model_meshes.len());
        for (part_index, (source_mesh, (mesh_id, default_material))) in
            model.meshes.iter().zip(model_meshes).enumerate()
        {
            let slot = object
                .material_slots
                .get(part_index)
                .and_then(Option::as_ref);
            if slot.is_none() && object.material.is_none() && replacement_texture.is_none() {
                draws.push((*mesh_id, *default_material));
                continue;
            }
            let material_id = match slot {
                Some(path) => upload_material_slot(
                    renderer,
                    assets,
                    path,
                    &source_mesh.material,
                    replacement_texture.as_deref(),
                    object.material.as_ref(),
                    model_path,
                    model,
                    texture_cache,
                    shader_cache,
                    material_cache,
                    error_material,
                    material_dependencies.as_deref_mut(),
                )?,
                None => {
                    let resolved = resolve_material(
                        &source_mesh.material,
                        replacement_texture.as_deref(),
                        object.material.as_ref(),
                    );
                    upload_resolved_material(
                        renderer,
                        &resolved,
                        model_path,
                        model,
                        assets,
                        texture_cache,
                        material_cache,
                        None,
                        material_dependencies.as_deref_mut(),
                    )?
                }
            };
            draws.push((*mesh_id, material_id));
        }
        levels.push(draws);
    }
    Ok(levels)
}

/// Создаёт GPU-материал или возвращает уже созданный для тех же входных данных. Короткоживущие
/// материалы записывают зависимости, чтобы вызывающий код мог освободить их вместе с материалом.
#[allow(clippy::too_many_arguments)]
fn upload_resolved_material(
    renderer: &mut Renderer,
    resolved: &ResolvedMaterial,
    model_path: &str,
    model: &Model,
    assets: MaterialAssetView<'_>,
    texture_cache: &mut GpuTextureCache<TextureId>,
    material_cache: &mut HashMap<String, MaterialId>,
    shader: Option<ShaderId>,
    material_dependencies: Option<&mut MaterialDependencyMap>,
) -> Result<MaterialId, RenderError> {
    let key = material_cache_key(model_path, resolved);
    if let Some(&material) = material_cache.get(&key) {
        return Ok(material);
    }
    let base_color_texture = upload_material_texture(
        renderer,
        resolved.base_color_texture.as_ref(),
        TextureColorSpace::Srgb,
        model_path,
        model,
        assets,
        texture_cache,
    )?;
    let metallic_roughness_texture = upload_material_texture(
        renderer,
        resolved.metallic_roughness_texture.as_ref(),
        TextureColorSpace::Linear,
        model_path,
        model,
        assets,
        texture_cache,
    )?;
    let normal_texture = upload_material_texture(
        renderer,
        resolved.normal_texture.as_ref(),
        TextureColorSpace::Linear,
        model_path,
        model,
        assets,
        texture_cache,
    )?;
    let emissive_texture = upload_material_texture(
        renderer,
        resolved.emissive_texture.as_ref(),
        TextureColorSpace::Srgb,
        model_path,
        model,
        assets,
        texture_cache,
    )?;
    let occlusion_texture = upload_material_texture(
        renderer,
        resolved.occlusion_texture.as_ref(),
        TextureColorSpace::Linear,
        model_path,
        model,
        assets,
        texture_cache,
    )?;
    let mut user_textures = [None; 8];
    for (slot, source) in resolved.user_texture_sources.iter().enumerate() {
        let Some(source) = source else { continue };
        let key = (source.path.clone(), source.color_space);
        let data = assets.texture(&source.path).ok_or_else(|| {
            RenderError::InvalidTexture(format!(
                "ресурсная текстура /{} отсутствует или не загружена",
                source.path
            ))
        })?;
        user_textures[slot] = Some(get_or_insert_gpu(&mut texture_cache.scene, key, || {
            renderer.create_texture_with_color_space(
                data.width,
                data.height,
                &data.rgba,
                source.color_space,
            )
        })?);
    }
    let material = renderer.create_material(&Material {
        shader,
        user_values: resolved.user_values,
        user_textures,
        base_color: resolved.base_color,
        metallic: resolved.metallic,
        roughness: resolved.roughness,
        emissive: resolved.emissive,
        opacity: resolved.opacity,
        alpha_mode: resolved.alpha_mode,
        double_sided: resolved.double_sided,
        base_color_texture,
        metallic_roughness_texture,
        normal_texture,
        emissive_texture,
        occlusion_texture,
        normal_scale: resolved.normal_scale,
        occlusion_strength: resolved.occlusion_strength,
    })?;
    if let Some(material_dependencies) = material_dependencies {
        let textures = [
            base_color_texture,
            metallic_roughness_texture,
            normal_texture,
            emissive_texture,
            occlusion_texture,
        ]
        .into_iter()
        .flatten()
        .chain(user_textures.iter().flatten().copied())
        .collect();
        material_dependencies.insert(material, MaterialResourceDependencies { textures, shader });
    }
    material_cache.insert(key, material);
    Ok(material)
}

/// Ключ дедупликации GPU-материала: всё, что меняет результат, кроме исходника шейдера, который
/// однозначно задаётся `shader_path`. Текстуры модели адресуются индексом внутри модели,
/// поэтому путь модели входит в ключ.
fn material_cache_key(model_path: &str, resolved: &ResolvedMaterial) -> String {
    let without_source = ResolvedMaterial {
        shader_source: None,
        ..resolved.clone()
    };
    format!("{model_path}\n{without_source:?}")
}

#[allow(clippy::too_many_arguments)]
fn upload_material_slot(
    renderer: &mut Renderer,
    assets: MaterialAssetView<'_>,
    path: &str,
    model_material: &ModelMaterial,
    replacement_texture: Option<&str>,
    override_material: Option<&MaterialOverride>,
    model_path: &str,
    model: &Model,
    texture_cache: &mut GpuTextureCache<TextureId>,
    shader_cache: &mut HashMap<String, Result<ShaderId, String>>,
    material_cache: &mut HashMap<String, MaterialId>,
    error_material: &mut Option<MaterialId>,
    material_dependencies: Option<&mut MaterialDependencyMap>,
) -> Result<MaterialId, RenderError> {
    let resolved = assets
        .material_asset(path)
        .ok_or_else(|| format!("материал /{path} не был подготовлен"))
        .and_then(|result| result.as_ref().map_err(Clone::clone))
        .and_then(|material| {
            let shader = material
                .shader
                .as_ref()
                .map(|shader_path| {
                    assets
                        .shader_asset(shader_path.as_str())
                        .ok_or_else(|| format!("шейдер /{} не был подготовлен", shader_path))?
                        .as_ref()
                        .map_err(Clone::clone)
                })
                .transpose()?;
            resolve_material_asset(
                model_material,
                path,
                material,
                shader,
                replacement_texture,
                override_material,
            )
        });
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(error) => {
            log::error!("[material] /{path}: {error}; используется материал ошибки");
            return upload_error_material(renderer, error_material);
        }
    };
    let shader = if let (Some(shader_path), Some(source)) =
        (&resolved.shader_path, &resolved.shader_source)
    {
        let result = shader_cache.entry(shader_path.clone()).or_insert_with(|| {
            renderer
                .create_surface_shader(source)
                .map_err(|error| error.to_string())
        });
        match result {
            Ok(shader) => Some(*shader),
            Err(error) => {
                log::error!(
                    "[material] /{path}: не удалось скомпилировать /{shader_path}: {error}; используется материал ошибки"
                );
                return upload_error_material(renderer, error_material);
            }
        }
    } else {
        None
    };
    match upload_resolved_material(
        renderer,
        &resolved,
        model_path,
        model,
        assets,
        texture_cache,
        material_cache,
        shader,
        material_dependencies,
    ) {
        Ok(material) => Ok(material),
        Err(error) => {
            log::error!("[material] /{path}: {error}; используется материал ошибки");
            upload_error_material(renderer, error_material)
        }
    }
}

fn upload_error_material(
    renderer: &mut Renderer,
    cached: &mut Option<MaterialId>,
) -> Result<MaterialId, RenderError> {
    if let Some(material) = cached {
        return Ok(*material);
    }
    let material = renderer.create_material(&Material {
        base_color: [1.0, 0.0, 1.0],
        ..Material::default()
    })?;
    *cached = Some(material);
    Ok(material)
}

fn upload_material_texture(
    renderer: &mut Renderer,
    source: Option<&MaterialTextureSource>,
    color_space: TextureColorSpace,
    model_path: &str,
    model: &Model,
    assets: MaterialAssetView<'_>,
    texture_cache: &mut GpuTextureCache<TextureId>,
) -> Result<Option<TextureId>, RenderError> {
    let Some(source) = source else {
        return Ok(None);
    };
    let texture_id = match source {
        MaterialTextureSource::Model(index) => {
            let key = (model_path.to_owned(), *index, color_space);
            let texture = model.textures.get(*index).ok_or_else(|| {
                RenderError::InvalidTexture(format!(
                    "модель {model_path:?} ссылается на отсутствующую текстуру {index}"
                ))
            })?;
            get_or_insert_gpu(&mut texture_cache.model, key, || {
                renderer.create_texture_with_color_space(
                    texture.width,
                    texture.height,
                    &texture.rgba,
                    color_space,
                )
            })?
        }
        MaterialTextureSource::Scene(path) => {
            let key = (path.clone(), color_space);
            let texture = assets.texture(path).ok_or({
                RenderError::InvalidState("CPU-текстура отсутствует в таблице GPU-ресурсов")
            })?;
            get_or_insert_gpu(&mut texture_cache.scene, key, || {
                renderer.create_texture_with_color_space(
                    texture.width,
                    texture.height,
                    &texture.rgba,
                    color_space,
                )
            })?
        }
    };
    Ok(Some(texture_id))
}

#[cfg(test)]
mod tests {
    use super::{
        ColorLutGpu, CubeLut, GpuTextureCache, MaterialAssetView, MaterialTextureSource,
        PreparedMaterialAssets, PreparedPrefabs, ScreenShaderGpu, color_lut_paths,
        get_or_insert_gpu, load_color_luts, load_scene, load_screen_shader_sources,
        material_cache_key, prepare_scene_with_model_loader, resolve_material,
        resolve_material_asset, screen_shader_paths, upload_color_luts, upload_screen_shaders,
    };
    use engine_assets::{Assets, Model, ModelAlphaMode, ModelMaterial};
    use engine_files::VirtualPath;
    use engine_physics::BodyKind;
    use engine_project::PhysicsSettings;
    use engine_render::{AlphaMode, EnvironmentSource, RenderError, TextureColorSpace};
    use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
    use engine_scene::{
        MaterialAlphaMode, MaterialAsset, MaterialOverride, PrefabTemplate, SceneDocument,
        ShaderAsset, World,
    };
    use engine_script::{
        Key, MouseButton, PrefabCatalog, PrefabKind, PrefabRequest, PrefabSpec, ScriptEventLevel,
        ScriptHost, ScriptInput, ScriptLibrary, ScriptLoadOptions, ScriptTime,
    };
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    struct TempProject(PathBuf);

    impl TempProject {
        fn new() -> Self {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "rve-runtime-scene-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn root(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct LooseResources {
        system: ResourceSystem,
        assets: Assets,
    }

    impl LooseResources {
        fn new(root: &Path) -> Self {
            let system = ResourceSystem::start(ResourceConfig {
                root: root.to_owned(),
                source_mode: SourceMode::Loose,
                worker_count: 1,
                ..ResourceConfig::default()
            })
            .unwrap();
            let assets = Assets::with_resources(system.handle());
            Self { system, assets }
        }
    }

    impl Drop for LooseResources {
        fn drop(&mut self) {
            let _ = self.system.shutdown();
        }
    }

    struct DemoSceneReferenceProbe;

    impl rve_sdk::Script for DemoSceneReferenceProbe {}

    fn demo_scene_reference_probe(_: &rve_sdk::Params) -> Result<Box<dyn rve_sdk::Script>, String> {
        Ok(Box::new(DemoSceneReferenceProbe))
    }

    fn demo_scene_script_library() -> ScriptLibrary {
        let module = rve_sdk::script_module! {
            "SceneReferenceProbe" => demo_scene_reference_probe,
        };
        // SAFETY: the SDK macro keeps the module and descriptor table in static storage.
        unsafe { ScriptLibrary::from_module(std::ptr::from_ref(module)) }.unwrap()
    }

    const CAMERA_SCENE: &str = r#"
[[objects]]
id = "camera"
[objects.camera]
"#;

    fn camera_settings_scene(two_cameras: bool) -> String {
        let second = if two_cameras {
            "\n[[objects]]\nid = \"second\"\n[objects.camera]\nsettings = \"/cameras/shared.camerasettings\"\n"
        } else {
            ""
        };
        format!(
            "[[assets]]\npath = \"/cameras/shared.camerasettings\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = {two_cameras}\nsettings = \"/cameras/shared.camerasettings\"\n{second}"
        )
    }

    fn scene_with_lite_script() -> String {
        format!(
            "[[assets]]\npath = \"scripts/main.lua\"\n{CAMERA_SCENE}\n[[objects]]\nid = \"scripted\"\n[[objects.lite_scripts]]\npath = \"scripts/main.lua\"\n"
        )
    }

    #[test]
    fn load_scene_accepts_a_scene_without_meshes_from_loose_resources() {
        let project = TempProject::new();
        project.write("scene.rvescene", CAMERA_SCENE.as_bytes());
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        assert!(prepared.render_assets.models.is_empty());
        assert!(prepared.render_assets.objects.is_empty());
        assert!(prepared.sources.is_empty());
    }

    #[test]
    fn physics_scene_is_built_from_loaded_world_before_runtime_startup() {
        let project = TempProject::new();
        project.write(
            "scene.rvescene",
            format!(
                "{CAMERA_SCENE}\n[[objects]]\nid = \"body\"\n[objects.transform]\nscale = [0.0, 0.0, 0.0]\n[objects.collider]\nshape = \"sphere\"\nradius = 0.5\n"
            )
            .as_bytes(),
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();

        let geometry = prepared.physics_geometry();
        let error = prepared
            .build_physics(&geometry, &PhysicsSettings::default())
            .err()
            .expect("zero-scale collider should fail physics prebuild");
        assert!(error.contains("scale"), "{error}");
    }

    #[test]
    fn demo_game_scenes_load_and_build_physics_without_gpu() {
        let assets = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/demo-game/Assets"
        ));
        let resources = LooseResources::new(assets);
        let handle = resources.system.handle();
        for scene in [
            "/scenes/physics.rvescene",
            "/scenes/main.rvescene",
            "/scenes/second.rvescene",
        ] {
            let prepared = load_scene(&handle, &resources.assets, scene)
                .unwrap_or_else(|error| panic!("{scene}: {error}"));
            let geometry = prepared.physics_geometry();
            prepared
                .build_physics(&geometry, &PhysicsSettings::default())
                .unwrap_or_else(|error| panic!("{scene}: {error}"));
        }

        let prepared = load_scene(&handle, &resources.assets, "/scenes/physics.rvescene").unwrap();
        let resolved = |id: &str| {
            prepared
                .world
                .object_by_id(id)
                .and_then(|object| object.collider.as_ref()?.resolved_material)
        };
        assert_eq!(resolved("ice-pad").map(|m| m.static_friction), Some(0.05));
        assert_eq!(resolved("rubber-ball").map(|m| m.bounciness), Some(0.9));
        let library = demo_scene_script_library();
        engine_script::ScriptHost::load(
            &prepared.world,
            "/scenes/physics.rvescene",
            &prepared.sources,
            Some(&library),
        )
        .unwrap_or_else(|error| panic!("physics scripts: {error}"));
        let geometry = prepared.physics_geometry();
        let (_, bindings) = prepared
            .build_physics(&geometry, &PhysicsSettings::default())
            .unwrap();
        // 11 статических коллайдеров (с ледяной площадкой), 4 монеты-триггера, 4 ящика, ящик на льду,
        // мяч и 1 персонаж.
        assert_eq!(bindings.len(), 22);
        assert_eq!(
            bindings
                .iter()
                .filter(|binding| binding.kind == BodyKind::Dynamic)
                .count(),
            6
        );
        assert_eq!(
            bindings
                .iter()
                .filter(|binding| binding.character_controller.is_some())
                .count(),
            1
        );
    }

    #[test]
    fn demo_game_fast_prefabs_register_for_spawning_and_their_scripts_load() {
        let assets = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/demo-game/Assets"
        ));
        let resources = LooseResources::new(assets);
        let handle = resources.system.handle();
        let prepared = load_scene(&handle, &resources.assets, "/scenes/physics.rvescene").unwrap();

        for path in ["prefabs/bullet.rveprefab", "prefabs/bullet_hole.rveprefab"] {
            assert!(prepared.prefabs.templates.contains_key(path), "{path}");
        }
        let hole = prepared
            .prefabs
            .catalog
            .get("prefabs/bullet_hole.rveprefab")
            .unwrap();
        assert_eq!(hole.kind, PrefabKind::Fast);
        assert_eq!(hole.max_instances, Some(100));
        let bullet = prepared
            .prefabs
            .catalog
            .get("prefabs/bullet.rveprefab")
            .unwrap();
        assert_eq!(bullet.max_instances, None);
        // Тяжёлый префаб не входит в быстрые: он грузится по rve.prefabs.load.
        assert!(
            prepared
                .prefabs
                .catalog
                .get("prefabs/tower.rveprefab")
                .is_none()
        );

        // Регистрация компилирует Lua пули и проверяет её params до старта сцены.
        let library = demo_scene_script_library();
        ScriptHost::load_with_options(
            &prepared.world,
            "/scenes/physics.rvescene",
            &prepared.sources,
            Some(&library),
            ScriptLoadOptions {
                prefabs: prepared.prefabs.catalog.clone(),
                ..ScriptLoadOptions::default()
            },
        )
        .unwrap_or_else(|error| panic!("demo prefab scripts: {error}"));
    }

    /// Стрельба и башня демо-игры как в runtime: быстрые префабы регистрируются у хоста, тяжёлые
    /// приходят из `prefabs.rveprefabs`, действия — из `project.rveproject`. Столкновение пули не
    /// моделируется: его проверяет тест скрипта пули в engine-script.
    #[test]
    fn demo_game_shooter_spawns_bullets_and_requests_the_tower_prefab() {
        let demo = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../examples/demo-game"
        ));
        let resources = LooseResources::new(&demo.join("Assets"));
        let handle = resources.system.handle();
        let mut prepared =
            load_scene(&handle, &resources.assets, "/scenes/physics.rvescene").unwrap();
        let project = engine_project::ProjectSettings::parse(
            &fs::read_to_string(demo.join("project.rveproject")).unwrap(),
        )
        .unwrap();
        let heavy =
            engine_project::PrefabCatalogFile::load(demo.join("prefabs.rveprefabs")).unwrap();
        crate::add_heavy_prefabs(&mut prepared.prefabs.catalog, &heavy).unwrap();
        let library = demo_scene_script_library();
        let mut host = ScriptHost::load_with_options(
            &prepared.world,
            &prepared.name,
            &prepared.sources,
            Some(&library),
            ScriptLoadOptions {
                input: project.input,
                prefabs: prepared.prefabs.catalog.clone(),
                ..ScriptLoadOptions::default()
            },
        )
        .unwrap();
        // Канвас HUD сцены открывается скриптами: документы регистрируются, как при старте runtime.
        crate::game_ui::UiRegistry::default()
            .register(&mut host, &prepared.ui)
            .unwrap();
        for (path, template) in &prepared.prefabs.templates {
            host.register_prefab(
                &prepared.world,
                path,
                Arc::clone(template),
                &prepared.sources,
            )
            .unwrap_or_else(|error| panic!("{path}: {error}"));
        }
        // Контроллер игрока читает физику, поэтому снимок и привязки строятся как при старте runtime.
        let geometry = prepared.physics_geometry();
        let (driver, bindings) = prepared.build_physics(&geometry, &project.physics).unwrap();
        let initialization =
            engine_physics::PhysicsInitialization::from_prebuilt(&driver, bindings).unwrap();
        let sync = engine_physics::PhysicsSceneSync::new(&prepared.world, initialization.bindings)
            .unwrap();
        host.set_physics_snapshot(initialization.result.snapshot, sync.bindings());
        let errors = |host: &mut ScriptHost| -> Vec<String> {
            host.drain_events()
                .into_iter()
                .filter(|event| event.level == ScriptEventLevel::Error)
                .map(|event| event.message)
                .collect()
        };
        let time = |frame: u64| ScriptTime {
            elapsed: frame as f64 / 60.0,
            frame,
        };

        // Кнопка fire из проекта: пуля вылетает из камеры, объект получает id `bullet@N`.
        let mut fire = ScriptInput::default();
        fire.set_mouse_button(MouseButton::Left, true);
        host.set_input(fire);
        host.update(&mut prepared.world, 1.0 / 60.0, time(1));
        let fired = errors(&mut host);
        assert!(fired.is_empty(), "{fired:?}");
        let bullets = prepared
            .world
            .objects()
            .iter()
            .filter(|object| object.id.starts_with("bullet@"))
            .count();
        assert_eq!(bullets, 1);

        // Кнопка spawn_heavy из проекта: башня запрашивает загрузку тяжёлого префаба.
        let mut tower = ScriptInput::default();
        tower.set_key(Key::T, true);
        host.set_input(tower);
        host.update(&mut prepared.world, 1.0 / 60.0, time(2));
        let towers = errors(&mut host);
        assert!(towers.is_empty(), "{towers:?}");
        assert_eq!(
            host.drain_world_changes().prefab_requests,
            [PrefabRequest::Load("prefabs/tower.rveprefab".to_owned())]
        );
    }

    #[test]
    fn load_scene_reports_a_missing_lua_source_from_loose_resources() {
        let project = TempProject::new();
        project.write("scene.rvescene", scene_with_lite_script().as_bytes());
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("scripts/main.lua"));
    }

    #[test]
    fn load_scene_rejects_a_lua_source_that_is_not_utf8() {
        let project = TempProject::new();
        project.write("scene.rvescene", scene_with_lite_script().as_bytes());
        project.write("scripts/main.lua", &[0xff, 0xfe]);
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("исходник должен быть UTF-8"));
    }

    #[test]
    fn load_scene_expands_prefab_instances_through_the_resource_handle() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath = \"/prefabs/spinner.rveprefab\"\n{CAMERA_SCENE}\n[[instances]]\nid = \"spinner1\"\nprefab = \"/prefabs/spinner.rveprefab\"\n[[instances.overrides]]\ntarget = \"blade\"\nlite_params = [{{ path = \"/scripts/spin.lua\", params = {{ speed = 2 }} }}]\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        project.write(
            "prefabs/spinner.rveprefab",
            b"[[assets]]\npath = \"/scripts/spin.lua\"\n[[objects]]\nid = \"hub\"\n[[objects]]\nid = \"blade\"\nparent = \"hub\"\n[[objects.lite_scripts]]\npath = \"/scripts/spin.lua\"\n",
        );
        project.write("scripts/spin.lua", b"return {}\n");
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        let blade = prepared.world.handle("spinner1.blade").unwrap();
        let object = prepared.world.object(blade).unwrap();
        assert_eq!(
            object.lite_scripts[0].params["speed"],
            engine_scene::ParamValue::Int(2)
        );
        assert!(prepared.world.handle("spinner1").is_some());
        assert!(prepared.sources.contains_key("scripts/spin.lua"));
    }

    #[test]
    fn load_scene_prepares_hdr_environment_assets_of_scene_objects() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath = \"/environment/sky.hdr\"\n{CAMERA_SCENE}\n[[objects]]\nid = \"sky\"\n[objects.environment]\nhdr = \"/environment/sky.hdr\"\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        project.write(
            "environment/sky.hdr",
            b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 2\n\x80\x80\x80\x81\x40\x60\x80\x82",
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        let EnvironmentSource::Equirect { width, height, rgb } = prepared
            .render_assets
            .environments
            .get("environment/sky.hdr")
            .unwrap()
            .as_ref()
        else {
            panic!("HDR environment should be prepared as an equirectangular image");
        };
        assert_eq!((*width, *height), (2, 1));
        assert_eq!(rgb.len(), 6);
        assert!(
            prepared
                .world
                .object_by_id("sky")
                .unwrap()
                .environment
                .is_some()
        );
    }

    #[test]
    fn load_scene_reports_a_missing_prefab() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath = \"/prefabs/gone.rveprefab\"\n{CAMERA_SCENE}\n[[instances]]\nid = \"x\"\nprefab = \"/prefabs/gone.rveprefab\"\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("gone.rveprefab"), "{error}");
    }

    #[test]
    fn load_scene_rejects_an_invalid_scene_from_loose_resources() {
        let project = TempProject::new();
        project.write("scene.rvescene", b"objects = [");
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("scene.rvescene"));
    }

    fn physics_material_scene() -> String {
        format!(
            "[[assets]]\npath = \"/materials/ice.rvephysmat\"\n{CAMERA_SCENE}\n[[objects]]\nid = \"ball\"\n[objects.collider]\nshape = \"sphere\"\nradius = 0.5\nmaterial = \"/materials/ice.rvephysmat\"\n"
        )
    }

    #[test]
    fn declared_rvemat_without_a_slot_is_prepared_so_set_material_can_use_it() {
        let project = TempProject::new();
        project.write(
            "scene.rvescene",
            format!(
                "[[assets]]\npath = \"/materials/spare.rvemat\"\n{CAMERA_SCENE}\n[[objects]]\nid = \"lamp\"\n[objects.point_light]\n"
            )
            .as_bytes(),
        );
        project.write("materials/spare.rvemat", b"");
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        assert!(
            prepared
                .render_assets
                .material_assets
                .get("materials/spare.rvemat")
                .is_some_and(Result::is_ok),
            "{:?}",
            prepared
                .render_assets
                .material_assets
                .keys()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn prefab_material_view_resolves_scene_declared_materials() {
        let project = TempProject::new();
        project.write(
            "scene.rvescene",
            format!("[[assets]]\npath = \"/materials/gold.rvemat\"\n{CAMERA_SCENE}").as_bytes(),
        );
        project.write("materials/gold.rvemat", b"");
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        let prefab_assets = PreparedMaterialAssets::default();
        let view = MaterialAssetView::from_prefab(&prefab_assets, Some(&prepared.render_assets));

        assert!(
            view.material_asset("materials/gold.rvemat")
                .is_some_and(Result::is_ok)
        );
    }

    #[test]
    fn load_scene_resolves_collider_physics_materials_from_assets() {
        let project = TempProject::new();
        project.write("scene.rvescene", physics_material_scene().as_bytes());
        project.write(
            "materials/ice.rvephysmat",
            b"static_friction = 0.05\nbounciness = 0.9\n",
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        let collider = prepared
            .world
            .object_by_id("ball")
            .and_then(|object| object.collider.as_ref())
            .unwrap();
        let resolved = collider.resolved_material.unwrap();
        assert_eq!(resolved.static_friction, 0.05);
        assert_eq!(resolved.bounciness, 0.9);
    }

    #[test]
    fn load_scene_rejects_a_missing_or_invalid_physics_material() {
        let project = TempProject::new();
        project.write("scene.rvescene", physics_material_scene().as_bytes());
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("materials/ice.rvephysmat"), "{error}");

        project.write("materials/ice.rvephysmat", b"static_friction = -1.0\n");
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("static_friction"), "{error}");
    }

    #[test]
    fn load_scene_rejects_a_missing_camera_settings_asset() {
        let project = TempProject::new();
        let source = camera_settings_scene(false);
        project.write("scene.rvescene", source.as_bytes());
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("cameras/shared.camerasettings"));
    }

    #[test]
    fn load_scene_rejects_camera_settings_that_are_not_utf8_or_valid_toml() {
        let project = TempProject::new();
        let source = camera_settings_scene(false);
        project.write("scene.rvescene", source.as_bytes());
        project.write("cameras/shared.camerasettings", &[0xff, 0xfe]);
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("cameras/shared.camerasettings"));
        assert!(error.contains("UTF-8"));
        let project = TempProject::new();
        project.write("scene.rvescene", source.as_bytes());
        project.write("cameras/shared.camerasettings", b"[projection\nfov = 45");
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("cameras/shared.camerasettings"));
    }

    #[test]
    fn cameras_sharing_a_settings_asset_receive_the_same_validated_settings() {
        let project = TempProject::new();
        let source = camera_settings_scene(true);
        project.write("scene.rvescene", source.as_bytes());
        project.write(
            "cameras/shared.camerasettings",
            b"[projection]\nfov_deg = 42.0\nnear = 0.2\nfar = 250.0\n",
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        for id in ["camera", "second"] {
            let world = &prepared.world;
            let handle = world.handle(id).unwrap();
            assert_eq!(
                world.camera_settings(handle).unwrap().projection.fov_deg,
                42.0
            );
            assert_eq!(world.camera_settings(handle).unwrap().projection.near, 0.2);
        }
    }

    /// Сцена с двумя камерами, которые ссылаются на общий шейдер; `broken` и `missing` — плохие пути.
    fn screen_shader_scene() -> &'static str {
        "[[assets]]\npath = \"/fx/vhs.glsl\"\n[[assets]]\npath = \"/fx/broken.glsl\"\n[[assets]]\npath = \"/fx/missing.glsl\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = true\n[[objects.camera.screen_shaders]]\nshader = \"/fx/vhs.glsl\"\nstage = \"after_post\"\n[[objects.camera.screen_shaders]]\nshader = \"/fx/broken.glsl\"\nstage = \"before_post\"\n[[objects]]\nid = \"second\"\n[objects.camera]\n[[objects.camera.screen_shaders]]\nshader = \"/fx/vhs.glsl\"\nstage = \"after_post\"\n[[objects.camera.screen_shaders]]\nshader = \"/fx/missing.glsl\"\nstage = \"after_post\"\n"
    }

    /// Создание LUT без GPU: LUT стороны 3 отклоняется, остальные получают номер создания.
    #[derive(Default)]
    struct FakeLutGpu {
        created: Vec<u32>,
    }

    impl ColorLutGpu for FakeLutGpu {
        type Lut = usize;

        fn create_color_lut(&mut self, lut: &CubeLut) -> Result<usize, RenderError> {
            self.created.push(lut.size());
            if lut.size() == 3 {
                return Err(RenderError::InvalidConfig("тестовая ошибка LUT"));
            }
            Ok(self.created.len())
        }
    }

    fn cube_source(size: u32) -> String {
        let mut source = format!("LUT_3D_SIZE {size}\n");
        for _ in 0..size.pow(3) {
            source.push_str("0.5 0.5 0.5\n");
        }
        source
    }

    #[test]
    fn color_luts_of_camera_post_processes_are_read_once_and_bad_luts_are_skipped() {
        let project = TempProject::new();
        let camera = |id: &str, post: &str| {
            let active = id == "a";
            format!(
                "[[objects]]\nid = \"{id}\"\n[objects.camera]\nactive = {active}\npost_process = \"/post/{post}.postprocess\"\n"
            )
        };
        let scene = format!(
            "[[assets]]\npath = \"/post/film.postprocess\"\n[[assets]]\npath = \"/post/odd.postprocess\"\n[[assets]]\npath = \"/post/broken.postprocess\"\n[[assets]]\npath = \"/post/missing.postprocess\"\n{}{}{}{}{}",
            camera("a", "film"),
            camera("b", "film"),
            camera("c", "odd"),
            camera("d", "broken"),
            camera("e", "missing"),
        );
        project.write("scene.rvescene", scene.as_bytes());
        let post =
            |lut: &str| format!("[color_grading]\nenabled = true\nlut = \"/luts/{lut}.cube\"\n");
        project.write("post/film.postprocess", post("film").as_bytes());
        project.write("post/odd.postprocess", post("odd").as_bytes());
        project.write("post/broken.postprocess", post("broken").as_bytes());
        project.write("post/missing.postprocess", post("missing").as_bytes());
        project.write("luts/film.cube", cube_source(2).as_bytes());
        project.write("luts/odd.cube", cube_source(3).as_bytes());
        project.write("luts/broken.cube", b"LUT_1D_SIZE 2\n");
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        assert_eq!(
            color_lut_paths(&prepared.world)
                .into_iter()
                .collect::<Vec<_>>(),
            [
                "luts/broken.cube",
                "luts/film.cube",
                "luts/missing.cube",
                "luts/odd.cube"
            ],
            "пути всех камер без повторов"
        );
        let luts = load_color_luts(&resources.system.handle(), &prepared.world);
        assert_eq!(
            luts.keys().map(String::as_str).collect::<Vec<_>>(),
            ["luts/film.cube", "luts/odd.cube"],
            "нечитаемая и неразборчивая LUT пропущены"
        );
        let mut gpu = FakeLutGpu::default();
        let ids = upload_color_luts(&mut gpu, &luts);
        assert_eq!(gpu.created.len(), 2, "каждый путь создаётся один раз");
        assert!(ids.contains_key("luts/film.cube"));
        assert!(
            !ids.contains_key("luts/odd.cube"),
            "ошибка GPU пропускает LUT"
        );
    }

    /// Создание шейдеров без GPU: `broken` отклоняется, остальные получают номер создания.
    #[derive(Default)]
    struct FakeScreenGpu {
        created: Vec<String>,
    }

    impl ScreenShaderGpu for FakeScreenGpu {
        type Shader = usize;

        fn create_screen_shader(&mut self, source: &str) -> Result<usize, RenderError> {
            self.created.push(source.to_owned());
            if source.contains("broken") {
                return Err(RenderError::InvalidConfig("тестовая ошибка компиляции"));
            }
            Ok(self.created.len())
        }
    }

    #[test]
    fn screen_shader_sources_are_read_once_per_path_and_bad_shaders_are_skipped() {
        let project = TempProject::new();
        project.write("scene.rvescene", screen_shader_scene().as_bytes());
        project.write(
            "fx/vhs.glsl",
            b"vec3 rve_shade(vec2 uv) { return rve_sample(uv).rgb; }",
        );
        project.write("fx/broken.glsl", b"broken");
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        assert_eq!(
            screen_shader_paths(&prepared.world)
                .into_iter()
                .collect::<Vec<_>>(),
            ["fx/broken.glsl", "fx/missing.glsl", "fx/vhs.glsl"],
            "пути всех камер без повторов"
        );
        let sources = load_screen_shader_sources(&resources.system.handle(), &prepared.world);
        assert_eq!(
            sources.keys().map(String::as_str).collect::<Vec<_>>(),
            ["fx/broken.glsl", "fx/vhs.glsl"],
            "нечитаемый файл пропущен"
        );
        let mut gpu = FakeScreenGpu::default();
        let ids = upload_screen_shaders(&mut gpu, &sources);
        assert_eq!(gpu.created.len(), 2, "каждый путь компилируется один раз");
        assert!(ids.contains_key("fx/vhs.glsl"));
        assert!(
            !ids.contains_key("fx/broken.glsl"),
            "ошибка компиляции пропускает шейдер"
        );
    }

    fn post_process_scene(second_camera: bool) -> String {
        let second = if second_camera {
            "\n[[objects]]\nid = \"second\"\n[objects.camera]\npost_process = \"/post/shared.postprocess\"\n"
        } else {
            ""
        };
        format!(
            "[[assets]]\npath = \"/post/shared.postprocess\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = true\npost_process = \"/post/shared.postprocess\"\n{second}"
        )
    }

    #[test]
    fn cameras_sharing_a_postprocess_asset_receive_its_settings_and_legacy_color_is_the_fallback() {
        let project = TempProject::new();
        project.write("scene.rvescene", post_process_scene(true).as_bytes());
        project.write(
            "post/shared.postprocess",
            b"[bloom]\nenabled = true\nintensity = 2.0\n",
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        for id in ["camera", "second"] {
            let handle = prepared.world.handle(id).unwrap();
            let post = prepared.world.post_process(handle).unwrap();
            assert!(post.bloom.enabled);
            assert_eq!(post.bloom.intensity, 2.0);
        }

        // Камера без файла постобработки берёт exposure из устаревшего блока [color].
        let legacy = TempProject::new();
        legacy.write("scene.rvescene", camera_settings_scene(false).as_bytes());
        legacy.write(
            "cameras/shared.camerasettings",
            b"[color]\nexposure_ev = 1.5\ntonemapper = \"none\"\n",
        );
        let resources = LooseResources::new(legacy.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        let handle = prepared.world.handle("camera").unwrap();
        let post = prepared.world.post_process(handle).unwrap();
        assert_eq!(post.exposure.ev, 1.5);
        assert_eq!(post.tonemapping.tonemapper, engine_scene::Tonemapper::None);
    }

    #[test]
    fn postprocess_file_errors_name_the_file() {
        for (bytes, expected) in [
            (&[0xff_u8, 0xfe][..], "not utf-8"),
            (&b"[bloom\nintensity = 2"[..], "bloom"),
        ] {
            let project = TempProject::new();
            project.write("scene.rvescene", post_process_scene(false).as_bytes());
            project.write("post/shared.postprocess", bytes);
            let resources = LooseResources::new(project.root());
            let error = load_scene(
                &resources.system.handle(),
                &resources.assets,
                "scene.rvescene",
            )
            .err()
            .unwrap();
            assert!(
                error.contains("post/shared.postprocess"),
                "{error} ({expected})"
            );
        }
    }

    #[test]
    fn two_objects_using_one_model_share_a_single_cached_load() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath = \"models/quad.fbx\"\n[[assets]]\npath = \"scripts/main.lua\"\n{CAMERA_SCENE}\n[[objects]]\nid = \"first\"\n[objects.mesh_renderer]\nmodel = \"models/quad.fbx\"\n[[objects]]\nid = \"second\"\n[objects.mesh_renderer]\nmodel = \"models/quad.fbx\"\n[[objects.lite_scripts]]\npath = \"scripts/main.lua\"\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        project.write("scripts/main.lua", b"return {}\n");
        let resources = LooseResources::new(project.root());
        let document = SceneDocument::parse("scene.rvescene", &scene_text).unwrap();
        let world = World::from_document(&document).unwrap();
        let load_count = Cell::new(0);
        let prepared = prepare_scene_with_model_loader(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
            world,
            PreparedPrefabs::default(),
            [0; 32],
            |path| {
                assert_eq!(path, "models/quad.fbx");
                load_count.set(load_count.get() + 1);
                Ok(Model {
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    skeleton: None,
                    animations: Vec::new(),
                })
            },
        )
        .unwrap();
        assert_eq!(load_count.get(), 1);
        assert_eq!(prepared.render_assets.models.len(), 1);
        assert_eq!(prepared.render_assets.objects.len(), 2);
        assert_eq!(
            prepared.sources.get("scripts/main.lua").unwrap(),
            "return {}\n"
        );
    }

    #[test]
    fn lod_models_load_once_and_keep_document_level_order() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath=\"models/base.fbx\"\n[[assets]]\npath=\"models/lod.fbx\"\n{CAMERA_SCENE}\n[[objects]]\nid=\"first\"\n[objects.mesh_renderer]\nmodel=\"models/base.fbx\"\n[objects.lod_group]\ncull_distance=50.0\n[[objects.lod_group.levels]]\nmodel=\"models/lod.fbx\"\ndistance=10.0\n[[objects]]\nid=\"second\"\n[objects.mesh_renderer]\nmodel=\"models/lod.fbx\"\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        let resources = LooseResources::new(project.root());
        let document = SceneDocument::parse("scene.rvescene", &scene_text).unwrap();
        let world = World::from_document(&document).unwrap();
        let mut loaded_paths = Vec::new();
        let prepared = prepare_scene_with_model_loader(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
            world,
            PreparedPrefabs::default(),
            [0; 32],
            |path| {
                loaded_paths.push(path.to_owned());
                Ok(Model {
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    skeleton: None,
                    animations: Vec::new(),
                })
            },
        )
        .unwrap();

        assert_eq!(loaded_paths, ["models/base.fbx", "models/lod.fbx"]);
        assert_eq!(prepared.render_assets.models.len(), 2);
        assert_eq!(
            prepared.render_assets.objects[0].1.levels,
            [
                ("models/base.fbx".to_owned(), None),
                ("models/lod.fbx".to_owned(), None),
            ]
        );
        assert_eq!(
            prepared.render_assets.objects[1].1.levels,
            [("models/lod.fbx".to_owned(), None)]
        );
    }

    #[test]
    fn material_resolution_keeps_fbx_defaults_and_applies_only_specified_overrides() {
        let model = ModelMaterial {
            base_color: [0.2, 0.4, 0.6],
            metallic: 0.7,
            roughness: 0.3,
            emissive: [0.5, 1.0, 2.0],
            opacity: 0.8,
            alpha_mode: ModelAlphaMode::Blend,
            base_color_texture: Some(0),
            metallic_roughness_texture: Some(1),
            normal_texture: Some(2),
            emissive_texture: Some(3),
            occlusion_texture: Some(4),
            normal_scale: 0.8,
            occlusion_strength: 0.6,
            double_sided: true,
        };
        let unchanged = resolve_material(&model, None, None);
        assert_eq!(unchanged.base_color, model.base_color);
        assert_eq!(unchanged.metallic, model.metallic);
        assert_eq!(unchanged.roughness, model.roughness);
        assert_eq!(unchanged.emissive, model.emissive);
        assert_eq!(unchanged.opacity, model.opacity);
        assert_eq!(unchanged.alpha_mode, AlphaMode::Blend);
        assert!(unchanged.double_sided);
        assert_eq!(
            unchanged.base_color_texture,
            Some(MaterialTextureSource::Model(0))
        );
        assert_eq!(
            unchanged.metallic_roughness_texture,
            Some(MaterialTextureSource::Model(1))
        );
        assert_eq!(
            unchanged.normal_texture,
            Some(MaterialTextureSource::Model(2))
        );
        assert_eq!(unchanged.normal_scale, 0.8);
        assert_eq!(unchanged.occlusion_strength, 0.6);
        assert_eq!(
            unchanged.emissive_texture,
            Some(MaterialTextureSource::Model(3))
        );
        assert_eq!(
            unchanged.occlusion_texture,
            Some(MaterialTextureSource::Model(4))
        );

        let override_material = MaterialOverride {
            base_color: Some([0.9, 0.8, 0.7]),
            metallic: Some(1.0),
            roughness: None,
            emissive: Some([3.0, 2.0, 1.0]),
            opacity: Some(0.4),
            alpha_mode: Some(MaterialAlphaMode::Mask),
            alpha_cutoff: Some(0.25),
            metallic_roughness_texture: Some(VirtualPath::parse("/textures/mr.png").unwrap()),
            normal_texture: Some(VirtualPath::parse("/textures/normal.png").unwrap()),
            emissive_texture: Some(VirtualPath::parse("/textures/emissive.png").unwrap()),
            occlusion_texture: Some(VirtualPath::parse("/textures/occlusion.png").unwrap()),
            normal_scale: Some(0.5),
            occlusion_strength: Some(0.2),
            double_sided: Some(false),
        };
        let replacement = resolve_material(
            &model,
            Some("textures/replacement.png"),
            Some(&override_material),
        );
        assert_eq!(replacement.base_color, [0.9, 0.8, 0.7]);
        assert_eq!(replacement.metallic, 1.0);
        assert_eq!(replacement.roughness, model.roughness);
        assert_eq!(replacement.emissive, [3.0, 2.0, 1.0]);
        assert_eq!(replacement.opacity, 0.4);
        assert_eq!(replacement.alpha_mode, AlphaMode::Mask { cutoff: 0.25 });
        assert!(!replacement.double_sided);
        assert_eq!(
            replacement.base_color_texture,
            Some(MaterialTextureSource::Scene(
                "textures/replacement.png".to_owned()
            ))
        );
        assert_eq!(
            replacement.metallic_roughness_texture,
            Some(MaterialTextureSource::Scene("textures/mr.png".to_owned()))
        );
        assert_eq!(
            replacement.normal_texture,
            Some(MaterialTextureSource::Scene(
                "textures/normal.png".to_owned()
            ))
        );
        assert_eq!(replacement.normal_scale, 0.5);
        assert_eq!(replacement.occlusion_strength, 0.2);
        assert_eq!(
            replacement.emissive_texture,
            Some(MaterialTextureSource::Scene(
                "textures/emissive.png".to_owned()
            ))
        );
        assert_eq!(
            replacement.occlusion_texture,
            Some(MaterialTextureSource::Scene(
                "textures/occlusion.png".to_owned()
            ))
        );
    }

    #[test]
    fn lit_material_slot_replaces_fbx_and_keeps_legacy_override_order() {
        let model = ModelMaterial {
            base_color: [0.1, 0.2, 0.3],
            metallic: 0.8,
            ..ModelMaterial::default()
        };
        let material = MaterialAsset::parse(
            "/materials/lit.rvemat",
            "[properties]\nbase_color = [0.4, 0.5, 0.6]\nmetallic = 0.35\nmetallic_roughness_texture = \"/textures/mr.png\"\n",
        )
        .unwrap();
        let override_material = MaterialOverride {
            base_color: Some([0.9, 0.8, 0.7]),
            ..MaterialOverride::default()
        };
        let resolved = resolve_material_asset(
            &model,
            "materials/lit.rvemat",
            &material,
            None,
            Some("textures/replacement.png"),
            Some(&override_material),
        )
        .unwrap();
        assert_eq!(resolved.base_color, [0.9, 0.8, 0.7]);
        assert_eq!(resolved.metallic, 0.35);
        assert_eq!(
            resolved.base_color_texture,
            Some(MaterialTextureSource::Scene(
                "textures/replacement.png".to_owned()
            ))
        );
        assert_eq!(
            resolved.metallic_roughness_texture,
            Some(MaterialTextureSource::Scene("textures/mr.png".to_owned()))
        );
        assert_eq!(resolved.shader_path, None);
    }

    #[test]
    fn custom_material_slot_uses_shader_values_and_ignores_legacy_overrides() {
        let shader = ShaderAsset::parse(
            "/shaders/custom.rveshader",
            &ShaderAsset::template("custom"),
        )
        .unwrap();
        let material = MaterialAsset::parse(
            "/materials/custom.rvemat",
            "shader = \"/shaders/custom.rveshader\"\n[properties]\ntint = [0.2, 0.4, 0.6]\nalbedo = \"/textures/custom.png\"\n",
        )
        .unwrap();
        let override_material = MaterialOverride {
            base_color: Some([0.9, 0.8, 0.7]),
            ..MaterialOverride::default()
        };
        let resolved = resolve_material_asset(
            &ModelMaterial::default(),
            "materials/custom.rvemat",
            &material,
            Some(&shader),
            Some("textures/replacement.png"),
            Some(&override_material),
        )
        .unwrap();
        assert_eq!(resolved.base_color, [1.0; 3]);
        assert_eq!(resolved.base_color_texture, None);
        assert_eq!(
            resolved.shader_path.as_deref(),
            Some("shaders/custom.rveshader")
        );
        assert_eq!(resolved.user_values[0], [0.2, 0.4, 0.6, 0.0]);
        assert_eq!(
            resolved.user_texture_sources[0]
                .as_ref()
                .map(|texture| texture.path.as_str()),
            Some("textures/custom.png")
        );
    }

    #[test]
    fn corrupt_material_parse_is_reported_as_a_resolvable_asset_error() {
        let error = MaterialAsset::parse("/materials/broken.rvemat", "shader = [\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("materials/broken.rvemat"), "{error}");
    }

    #[test]
    fn material_cache_key_matches_equal_inputs_and_separates_model_textures_and_shaders() {
        let model = ModelMaterial {
            base_color_texture: Some(0),
            ..ModelMaterial::default()
        };
        let resolved = resolve_material(&model, None, None);
        let key = material_cache_key("models/a.fbx", &resolved);
        assert_eq!(
            key,
            material_cache_key("models/a.fbx", &resolve_material(&model, None, None))
        );
        // Индекс текстуры модели имеет смысл только внутри своей модели.
        assert_ne!(key, material_cache_key("models/b.fbx", &resolved));
        let mut shaded = resolved.clone();
        shaded.shader_path = Some("/shaders/water.rveshader".to_owned());
        assert_ne!(key, material_cache_key("models/a.fbx", &shaded));
        let mut other_values = resolved.clone();
        other_values.user_values[0] = [0.5, 0.0, 0.0, 0.0];
        assert_ne!(key, material_cache_key("models/a.fbx", &other_values));
    }

    #[test]
    fn gpu_texture_cache_reuses_uploads_per_source_and_color_space() {
        let mut cache = GpuTextureCache::<u32>::default();
        let uploads = Cell::new(0_u32);

        let model_srgb_key = ("models/a.fbx".to_owned(), 0, TextureColorSpace::Srgb);
        let model_srgb = get_or_insert_gpu(&mut cache.model, model_srgb_key.clone(), || {
            uploads.set(uploads.get() + 1);
            Ok(uploads.get())
        })
        .unwrap();
        let model_srgb_reused = get_or_insert_gpu(&mut cache.model, model_srgb_key, || {
            uploads.set(uploads.get() + 1);
            Ok(uploads.get())
        })
        .unwrap();
        let model_linear = get_or_insert_gpu(
            &mut cache.model,
            ("models/a.fbx".to_owned(), 0, TextureColorSpace::Linear),
            || {
                uploads.set(uploads.get() + 1);
                Ok(uploads.get())
            },
        )
        .unwrap();
        assert_eq!(model_srgb, model_srgb_reused);
        assert_ne!(model_srgb, model_linear);

        let scene_srgb_key = ("textures/shared.png".to_owned(), TextureColorSpace::Srgb);
        let scene_srgb = get_or_insert_gpu(&mut cache.scene, scene_srgb_key.clone(), || {
            uploads.set(uploads.get() + 1);
            Ok(uploads.get())
        })
        .unwrap();
        let scene_srgb_reused = get_or_insert_gpu(&mut cache.scene, scene_srgb_key, || {
            uploads.set(uploads.get() + 1);
            Ok(uploads.get())
        })
        .unwrap();
        let scene_linear = get_or_insert_gpu(
            &mut cache.scene,
            ("textures/shared.png".to_owned(), TextureColorSpace::Linear),
            || {
                uploads.set(uploads.get() + 1);
                Ok(uploads.get())
            },
        )
        .unwrap();
        assert_eq!(scene_srgb, scene_srgb_reused);
        assert_ne!(scene_srgb, scene_linear);
        assert_eq!(uploads.get(), 4);
    }

    const SPAWN_BALL_PREFAB: &[u8] = b"[[assets]]\npath = \"/sounds/pop.wav\"\n[[objects]]\nid = \"root\"\n[objects.collider]\nshape = \"sphere\"\nradius = 0.1\n[objects.rigid_body]\nkind = \"dynamic\"\n";

    #[test]
    fn load_scene_prepares_fast_prefabs_listed_in_assets_without_instances() {
        let project = TempProject::new();
        project.write(
            "scene.rvescene",
            format!(
                "[[assets]]\npath = \"/prefabs/ball.rveprefab\"\nmax_instances = 8\n{CAMERA_SCENE}"
            )
            .as_bytes(),
        );
        project.write("prefabs/ball.rveprefab", SPAWN_BALL_PREFAB);
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();

        let path = "prefabs/ball.rveprefab";
        assert_eq!(
            prepared.prefabs.catalog.get(path),
            Some(PrefabSpec {
                kind: PrefabKind::Fast,
                max_instances: Some(8),
            })
        );
        assert!(prepared.prefabs.templates.contains_key(path));
        assert!(prepared.world.handle("root").is_none());
        assert_eq!(prepared.render_assets.prefabs[path].len(), 1);
        assert!(prepared.render_assets.prefabs[path][0].is_none());
        assert!(
            prepared
                .world
                .asset_paths()
                .iter()
                .any(|asset| asset.as_str() == "sounds/pop.wav"),
            "ассеты шаблона должны стать ассетами сцены"
        );
        assert!(prepared.sources.is_empty());
    }

    #[test]
    fn load_scene_skips_a_declared_prefab_with_a_camera_without_failing() {
        let project = TempProject::new();
        project.write(
            "scene.rvescene",
            format!("[[assets]]\npath = \"/prefabs/eye.rveprefab\"\n{CAMERA_SCENE}").as_bytes(),
        );
        project.write(
            "prefabs/eye.rveprefab",
            b"[[objects]]\nid = \"eye\"\n[objects.camera]\n",
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        assert!(
            prepared
                .prefabs
                .catalog
                .get("prefabs/eye.rveprefab")
                .is_none()
        );
        assert!(
            !prepared
                .prefabs
                .templates
                .contains_key("prefabs/eye.rveprefab")
        );
    }

    #[test]
    fn load_scene_reports_a_declared_prefab_that_does_not_parse() {
        let project = TempProject::new();
        project.write(
            "scene.rvescene",
            format!("[[assets]]\npath = \"/prefabs/broken.rveprefab\"\n{CAMERA_SCENE}").as_bytes(),
        );
        project.write(
            "prefabs/broken.rveprefab",
            b"[[objects]]\nid = \"a\"\nparent = \"missing\"\n",
        );
        let resources = LooseResources::new(project.root());
        let error = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .err()
        .unwrap();
        assert!(error.contains("broken.rveprefab"), "{error}");
    }

    /// Префаб с окружением не регистрируется для spawn. Экземпляр с таким префабом грузится, а
    /// spawn того же пути из скрипта даёт ошибку вызова, а не ошибку загрузки сцены.
    #[test]
    fn unspawnable_prefab_is_not_registered_and_spawning_its_path_is_a_script_error() {
        let project = TempProject::new();
        project.write(
            "scene.rvescene",
            format!(
                "[[assets]]\npath = \"/prefabs/sky.rveprefab\"\n[[assets]]\npath = \"/gun.lua\"\n{CAMERA_SCENE}[[objects]]\nid = \"gun\"\n[[objects.lite_scripts]]\npath = \"/gun.lua\"\n"
            )
            .as_bytes(),
        );
        project.write("prefabs/sky.rveprefab", SKY_PREFAB);
        project.write("gun.lua", GUN_SPAWNS_SKY);
        let resources = LooseResources::new(project.root());
        let mut prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        let path = "prefabs/sky.rveprefab";
        assert!(prepared.prefabs.catalog.get(path).is_none());
        assert!(!prepared.prefabs.templates.contains_key(path));

        let mut host = ScriptHost::load_with_options(
            &prepared.world,
            &prepared.name,
            &prepared.sources,
            None,
            ScriptLoadOptions {
                prefabs: prepared.prefabs.catalog.clone(),
                ..ScriptLoadOptions::default()
            },
        )
        .unwrap();
        host.update(
            &mut prepared.world,
            0.1,
            ScriptTime {
                elapsed: 0.0,
                frame: 1,
            },
        );
        let errors = host
            .drain_events()
            .into_iter()
            .filter(|event| event.level == ScriptEventLevel::Error)
            .map(|event| event.message)
            .collect::<Vec<_>>();
        assert!(
            errors
                .iter()
                .any(|message| message.contains("is not in the project prefab list")),
            "{errors:?}"
        );
        assert!(prepared.world.handle("sky").is_none());
    }

    const SKY_PREFAB: &[u8] = b"[[objects]]\nid = \"sky\"\n[objects.environment]\n[objects.environment.procedural]\nzenith_color = [0.2, 0.3, 0.5]\nhorizon_color = [0.8, 0.7, 0.5]\nground_color = [0.1, 0.1, 0.1]\nsun_size_deg = 3.0\nsun_intensity = 2.0\n";
    const GUN_SPAWNS_SKY: &[u8] = b"local Gun = {}\nfunction Gun:on_update(dt)\n  rve.spawn(\"prefabs/sky.rveprefab\", { id = \"sky\" })\nend\nreturn Gun\n";

    /// Префаб с окружением, который одновременно объявлен в `[[assets]]` и использован экземпляром,
    /// грузится: окружение попадает в сцену, а сам префаб не регистрируется для spawn.
    #[test]
    fn load_scene_prepares_hdr_environment_assets_introduced_by_prefab_instances() {
        let project = TempProject::new();
        let scene_text = format!(
            "[[assets]]\npath = \"/prefabs/sky.rveprefab\"\n{CAMERA_SCENE}\n[[instances]]\nid = \"sky\"\nprefab = \"/prefabs/sky.rveprefab\"\n"
        );
        project.write("scene.rvescene", scene_text.as_bytes());
        project.write(
            "prefabs/sky.rveprefab",
            b"[[assets]]\npath = \"/environment/sky.hdr\"\n[[objects]]\nid = \"environment\"\n[objects.environment]\nhdr = \"/environment/sky.hdr\"\n",
        );
        project.write(
            "environment/sky.hdr",
            b"#?RADIANCE\nFORMAT=32-bit_rle_rgbe\n\n-Y 1 +X 2\n\x80\x80\x80\x81\x40\x60\x80\x82",
        );
        let resources = LooseResources::new(project.root());
        let prepared = load_scene(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
        )
        .unwrap();
        let EnvironmentSource::Equirect { width, height, rgb } = prepared
            .render_assets
            .environments
            .get("environment/sky.hdr")
            .unwrap()
            .as_ref()
        else {
            panic!("HDR environment should be prepared as an equirectangular image");
        };
        assert_eq!((*width, *height), (2, 1));
        assert_eq!(rgb.len(), 6);
        assert!(
            prepared
                .world
                .object_by_id("sky")
                .unwrap()
                .environment
                .is_some()
        );
    }

    #[test]
    fn prefab_templates_prepare_their_mesh_levels_with_the_scene_model_loader() {
        let prefab_path = "prefabs/ball.rveprefab";
        let template = PrefabTemplate::load(
            &VirtualPath::parse(prefab_path).unwrap(),
            &mut |_| {
                Ok(
                    "[[assets]]\npath = \"/models/quad.fbx\"\n[[objects]]\nid = \"root\"\n[objects.mesh_renderer]\nmodel = \"models/quad.fbx\"\n[[objects]]\nid = \"empty\"\nparent = \"root\"\n"
                        .to_owned(),
                )
            },
        )
        .unwrap();
        let prefabs = PreparedPrefabs {
            templates: BTreeMap::from([(prefab_path.to_owned(), Arc::new(template))]),
            catalog: PrefabCatalog::default(),
        };
        let project = TempProject::new();
        let resources = LooseResources::new(project.root());
        let document = SceneDocument::parse("scene.rvescene", CAMERA_SCENE).unwrap();
        let world = World::from_document(&document).unwrap();
        let loaded_paths = std::cell::RefCell::new(Vec::new());
        let prepared = prepare_scene_with_model_loader(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
            world,
            prefabs,
            [0; 32],
            |path| {
                loaded_paths.borrow_mut().push(path.to_owned());
                Ok(Model {
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    skeleton: None,
                    animations: Vec::new(),
                })
            },
        )
        .unwrap();

        assert_eq!(loaded_paths.borrow().as_slice(), ["models/quad.fbx"]);
        let levels = &prepared.render_assets.prefabs[prefab_path];
        assert_eq!(levels.len(), 2);
        assert_eq!(
            levels[0].as_ref().unwrap().levels,
            [("models/quad.fbx".to_owned(), None)]
        );
        assert!(levels[1].is_none());
    }

    #[test]
    fn prefab_animators_are_not_prepared_because_spawned_animators_are_unsupported() {
        // Регрессия: аниматор шаблона не загружает клипы и не получает скиннинг, поэтому
        // отсутствующий клип не валит подготовку сцены.
        let prefab_path = "prefabs/hero.rveprefab";
        let template = PrefabTemplate::load(
            &VirtualPath::parse(prefab_path).unwrap(),
            &mut |_| {
                Ok(
                    "[[assets]]\npath = \"/models/hero.fbx\"\n[[assets]]\npath = \"/anims/missing.fbx\"\n[[objects]]\nid = \"hero\"\n[objects.mesh_renderer]\nmodel = \"models/hero.fbx\"\n[objects.animator]\nclips = [\"/anims/missing.fbx\"]\n"
                        .to_owned(),
                )
            },
        )
        .unwrap();
        let prefabs = PreparedPrefabs {
            templates: BTreeMap::from([(prefab_path.to_owned(), Arc::new(template))]),
            catalog: PrefabCatalog::default(),
        };
        let project = TempProject::new();
        let resources = LooseResources::new(project.root());
        let document = SceneDocument::parse("scene.rvescene", CAMERA_SCENE).unwrap();
        let world = World::from_document(&document).unwrap();
        let prepared = prepare_scene_with_model_loader(
            &resources.system.handle(),
            &resources.assets,
            "scene.rvescene",
            world,
            prefabs,
            [0; 32],
            |_| {
                Ok(Model {
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    skeleton: None,
                    animations: Vec::new(),
                })
            },
        )
        .unwrap();

        assert!(prepared.render_assets.clips.is_empty());
        let levels = &prepared.render_assets.prefabs[prefab_path];
        assert!(!levels[0].as_ref().unwrap().animated);
    }
}

#[cfg(test)]
mod particle_mapping_tests {
    use super::particle_emitter_desc;
    use engine_render::{
        MAX_ADDITIVE_PARTICLE_CAPACITY as RENDER_ADDITIVE,
        MAX_ALPHA_PARTICLE_CAPACITY as RENDER_ALPHA, ParticleBlend, ParticleShape,
    };
    use engine_scene::{
        MAX_ADDITIVE_PARTICLE_CAPACITY as SCENE_ADDITIVE,
        MAX_ALPHA_PARTICLE_CAPACITY as SCENE_ALPHA, ParticleBlendMode, ParticleBurstComponent,
        ParticleCollisionComponent, ParticleColorKeyComponent, ParticleEmitterComponent,
        ParticleShapeComponent,
    };

    #[test]
    fn capacity_limits_match_between_scene_and_render() {
        assert_eq!(SCENE_ADDITIVE, RENDER_ADDITIVE);
        assert_eq!(SCENE_ALPHA, RENDER_ALPHA);
    }

    #[test]
    fn component_maps_every_field_into_render_description() {
        let component = ParticleEmitterComponent {
            capacity: 1024,
            blend: ParticleBlendMode::Alpha,
            shape: ParticleShapeComponent::Cone {
                angle: 0.3,
                radius: 0.5,
            },
            rate: 45.0,
            bursts: vec![ParticleBurstComponent {
                time: 0.2,
                count: 9,
            }],
            duration: 2.0,
            looping: false,
            lifetime: [1.0, 3.0],
            speed: [0.5, 1.5],
            size: [0.2, 0.4],
            rotation: [0.0, 1.0],
            angular_velocity: [-1.0, 1.0],
            gravity: [0.0, -9.8, 0.0],
            drag: 0.25,
            color_over_life: vec![ParticleColorKeyComponent {
                time: 0.0,
                color: [1.0, 0.5, 0.0, 1.0],
            }],
            size_over_life: Vec::new(),
            texture: None,
            flipbook: engine_scene::ParticleFlipbookComponent {
                columns: 4,
                rows: 2,
                frames_per_second: 6.0,
            },
            soft_distance: 0.75,
            collision: Some(ParticleCollisionComponent {
                restitution: 0.3,
                friction: 0.1,
                thickness: 0.05,
            }),
            seed: 42,
        };
        let texture = None;
        let desc = particle_emitter_desc(&component, texture);
        assert_eq!(desc.capacity, 1024);
        assert_eq!(desc.blend, ParticleBlend::Alpha);
        assert_eq!(
            desc.shape,
            ParticleShape::Cone {
                angle: 0.3,
                radius: 0.5
            }
        );
        assert_eq!(desc.bursts.len(), 1);
        assert_eq!(desc.bursts[0].count, 9);
        assert!(!desc.looping);
        assert_eq!(desc.lifetime, [1.0, 3.0]);
        assert_eq!(desc.color_over_life[0].color, [1.0, 0.5, 0.0, 1.0]);
        assert_eq!(desc.flipbook.columns, 4);
        assert_eq!(desc.flipbook.frames_per_second, 6.0);
        assert_eq!(desc.soft_distance, 0.75);
        assert_eq!(
            desc.collision.map(|collision| collision.restitution),
            Some(0.3)
        );
        assert_eq!(desc.seed, 42);
        assert_eq!(desc.texture, texture);
    }

    #[test]
    fn point_shape_without_texture_maps_to_procedural_point() {
        let desc = particle_emitter_desc(&ParticleEmitterComponent::default(), None);
        assert_eq!(desc.shape, ParticleShape::Point);
        assert_eq!(desc.blend, ParticleBlend::Additive);
        assert!(desc.collision.is_none());
        assert!(desc.texture.is_none());
    }
}

#[cfg(test)]
mod material_resource_ownership_tests {
    use super::{
        MaterialResourceDependencies, MaterialResourceOwnership, MaterialResourceReleases,
    };
    use std::collections::HashSet;

    fn dependencies(
        textures: impl IntoIterator<Item = u32>,
        shader: Option<u32>,
    ) -> MaterialResourceDependencies<u32, u32> {
        MaterialResourceDependencies {
            textures: textures.into_iter().collect(),
            shader,
        }
    }

    #[test]
    fn dynamic_material_dependencies_are_shared_and_retired_with_the_last_material() {
        let mut ownership = MaterialResourceOwnership::<u32, u32, u32>::default();
        ownership.retain_material(100, dependencies([1, 2], Some(11)));
        ownership.retain_material(200, dependencies([2], Some(12)));

        let first = ownership.release_materials([100]);
        assert_eq!(first.textures, HashSet::from([1]));
        assert_eq!(first.shaders, HashSet::from([11]));
        assert!(!ownership.keeps_texture(1));
        assert!(ownership.keeps_texture(2));
        assert!(!ownership.keeps_shader(11));
        assert!(ownership.keeps_shader(12));

        let released = ownership.release_materials([200]);
        assert_eq!(released.textures, HashSet::from([2]));
        assert_eq!(released.shaders, HashSet::from([12]));
        assert_eq!(
            ownership.release_materials([200]),
            MaterialResourceReleases::default()
        );
    }

    #[test]
    fn scene_cache_owners_release_unique_resources_once_and_share_live_dependencies() {
        fn apply(
            resources: MaterialResourceReleases<u32, u32>,
            live: &mut HashSet<u32>,
            destroyed: &mut HashSet<(&'static str, u32)>,
        ) {
            for id in resources.textures {
                assert!(
                    live.remove(&id),
                    "texture {id} released twice or was not live"
                );
                assert!(destroyed.insert(("texture", id)));
            }
            for id in resources.shaders {
                assert!(
                    live.remove(&id),
                    "shader {id} released twice or was not live"
                );
                assert!(destroyed.insert(("shader", id)));
            }
        }

        let mut ownership = MaterialResourceOwnership::<u32, u32, u32>::default();
        ownership.retain_scene_textures(1, HashSet::from([1, 2]));
        ownership.retain_scene_textures(2, HashSet::from([2, 3]));
        ownership.retain_material(100, dependencies([2, 4], Some(10)));

        let mut live = HashSet::from([1, 2, 3, 4, 10]);
        let mut destroyed = HashSet::new();
        apply(
            ownership.release_scene_textures([2]),
            &mut live,
            &mut destroyed,
        );
        assert_eq!(live, HashSet::from([1, 2, 4, 10]));
        apply(
            ownership.release_scene_textures([1]),
            &mut live,
            &mut destroyed,
        );
        assert_eq!(live, HashSet::from([2, 4, 10]));
        apply(
            ownership.release_materials([100]),
            &mut live,
            &mut destroyed,
        );
        assert!(live.is_empty());
        apply(
            ownership.release_scene_textures([1, 2]),
            &mut live,
            &mut destroyed,
        );
        apply(
            ownership.release_materials([100]),
            &mut live,
            &mut destroyed,
        );
        assert_eq!(destroyed.len(), 5);
    }

    #[test]
    fn repeated_scene_replacements_keep_cache_ownership_bounded() {
        let mut ownership = MaterialResourceOwnership::<u32, u32, u32>::default();
        for generation in 1..=64 {
            ownership
                .retain_scene_textures(generation, HashSet::from([generation as u32 + 100, 42]));
            if generation > 1 {
                let released = ownership.release_scene_textures([generation - 1]);
                assert_eq!(released.textures, HashSet::from([generation as u32 + 99]));
            }
            assert!(ownership.scene_texture_refs.len() <= 2);
            assert!(ownership.keeps_texture(42));
        }
        assert_eq!(
            ownership.release_scene_textures([64]).textures,
            HashSet::from([164, 42])
        );
    }

    #[test]
    fn stale_success_releases_unique_material_dependencies_once_and_failed_uploads_do_not_pin_them()
    {
        let mut ownership = MaterialResourceOwnership::<u32, u32, u32>::default();
        ownership.retain_material(300, dependencies([3], Some(13)));
        let stale = ownership.release_materials([300]);
        assert_eq!(stale.textures, HashSet::from([3]));
        assert_eq!(stale.shaders, HashSet::from([13]));
        assert_eq!(
            ownership.release_materials([300]),
            MaterialResourceReleases::default(),
            "a stale successful upload's unique dependencies are released only once"
        );

        // A failed upload can cache dependencies before material creation, but no material owns them.
        assert!(!ownership.keeps_texture(4));
        assert!(!ownership.keeps_shader(14));
    }
}

#[cfg(test)]
mod scene_upload_rollback_tests {
    use super::{
        GpuTextureCache, SceneUploadCleanupGpu, SceneUploadResources, TextureColorSpace,
        remove_new_cached_textures, restore_shader_cache, rollback_scene_upload,
    };
    use std::collections::{HashMap, HashSet};

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    enum Kind {
        Mesh,
        Material,
        ErrorMaterial,
        Skin,
        ScreenShader,
        ColorLut,
        Particle,
        UiTexture,
        Texture,
        SurfaceShader,
    }

    #[derive(Debug)]
    struct MockGpu {
        fail_at: Kind,
        next_id: usize,
        live: HashSet<(Kind, usize)>,
        destroyed: Vec<(Kind, usize)>,
    }

    impl MockGpu {
        fn create(&mut self, kind: Kind) -> Result<usize, ()> {
            if self.fail_at == kind {
                return Err(());
            }
            let id = self.next_id;
            self.next_id += 1;
            self.live.insert((
                if kind == Kind::ErrorMaterial {
                    Kind::Material
                } else {
                    kind
                },
                id,
            ));
            Ok(id)
        }

        fn release(&mut self, kind: Kind, id: usize) -> Result<(), String> {
            if !self.live.remove(&(kind, id)) {
                return Err(format!("{kind:?} {id} was released more than once"));
            }
            self.destroyed.push((kind, id));
            Ok(())
        }
    }

    impl SceneUploadCleanupGpu for MockGpu {
        type Mesh = usize;
        type Material = usize;
        type Skin = usize;
        type ScreenShader = usize;
        type ColorLut = usize;
        type Particle = usize;
        type UiTexture = usize;
        type Texture = usize;
        type SurfaceShader = usize;

        fn destroy_mesh(&mut self, id: usize) -> Result<(), String> {
            self.release(Kind::Mesh, id)
        }
        fn destroy_material(&mut self, id: usize) -> Result<(), String> {
            self.release(Kind::Material, id)
        }
        fn destroy_skin(&mut self, id: usize) -> Result<(), String> {
            self.release(Kind::Skin, id)
        }
        fn destroy_screen_shader(&mut self, id: usize) -> Result<(), String> {
            self.release(Kind::ScreenShader, id)
        }
        fn destroy_color_lut(&mut self, id: usize) -> Result<(), String> {
            self.release(Kind::ColorLut, id)
        }
        fn destroy_particle(&mut self, id: usize) -> Result<(), String> {
            self.release(Kind::Particle, id)
        }
        fn destroy_ui_texture(&mut self, id: usize) -> Result<(), String> {
            self.release(Kind::UiTexture, id)
        }
        fn destroy_texture(&mut self, id: usize) -> Result<(), String> {
            self.release(Kind::Texture, id)
        }
        fn destroy_surface_shader(&mut self, id: usize) -> Result<(), String> {
            self.release(Kind::SurfaceShader, id)
        }
    }

    #[test]
    fn failures_at_each_upload_stage_release_only_created_ids_once() {
        let stages = [
            Kind::Mesh,
            Kind::Material,
            Kind::ErrorMaterial,
            Kind::Texture,
            Kind::SurfaceShader,
            Kind::UiTexture,
            Kind::ScreenShader,
            Kind::ColorLut,
            Kind::Skin,
            Kind::Particle,
        ];
        for fail_at in stages {
            let mut gpu = MockGpu {
                fail_at,
                next_id: 2,
                live: HashSet::from([
                    (Kind::Texture, 1),
                    (Kind::Material, 1),
                    (Kind::SurfaceShader, 1),
                ]),
                destroyed: Vec::new(),
            };
            let original_textures = HashSet::from([1]);
            let original_shader_paths = HashSet::from(["old.glsl".to_owned()]);
            let mut texture_cache = GpuTextureCache::<usize>::default();
            texture_cache
                .model
                .insert(("old.png".to_owned(), 0, TextureColorSpace::Srgb), 1);
            let mut shader_cache = HashMap::from([("old.glsl".to_owned(), Ok(1))]);
            let mut material_cache = HashMap::new();
            let original_error_material = Some(1);
            let mut error_material = original_error_material;
            let mut resources = SceneUploadResources::<MockGpu>::default();
            let mut failed = false;
            for kind in stages {
                let id = match gpu.create(kind) {
                    Ok(id) => id,
                    Err(()) => {
                        failed = true;
                        break;
                    }
                };
                match kind {
                    Kind::Mesh => {
                        resources.meshes.insert(id);
                    }
                    Kind::Material => {
                        material_cache.insert("uploaded".to_owned(), id);
                    }
                    Kind::ErrorMaterial => {
                        error_material = Some(id);
                    }
                    Kind::Texture => {
                        texture_cache
                            .scene
                            .insert(("new.png".to_owned(), TextureColorSpace::Srgb), id);
                    }
                    Kind::SurfaceShader => {
                        shader_cache.insert("new.glsl".to_owned(), Ok(id));
                    }
                    Kind::Skin => {
                        resources.skins.insert(id);
                    }
                    Kind::ScreenShader => {
                        resources.screen_shaders.insert(id);
                    }
                    Kind::ColorLut => {
                        resources.color_luts.insert(id);
                    }
                    Kind::Particle => {
                        resources.particles.insert(id);
                    }
                    Kind::UiTexture => {
                        resources.ui_textures.insert(id);
                    }
                }
            }
            assert!(failed, "mock failure stage {fail_at:?} was not reached");
            rollback_scene_upload(
                &mut gpu,
                &original_textures,
                &original_shader_paths,
                original_error_material,
                &mut texture_cache,
                &mut shader_cache,
                &material_cache,
                &mut error_material,
                &mut resources,
            );
            assert_eq!(
                gpu.live,
                HashSet::from([
                    (Kind::Texture, 1),
                    (Kind::Material, 1),
                    (Kind::SurfaceShader, 1),
                ])
            );
            assert_eq!(texture_cache.ids(), original_textures);
            assert_eq!(
                shader_cache,
                HashMap::from([("old.glsl".to_owned(), Ok(1))])
            );
            assert_eq!(error_material, original_error_material);
            let destroyed = gpu.destroyed.iter().copied().collect::<HashSet<_>>();
            assert_eq!(destroyed.len(), gpu.destroyed.len(), "{fail_at:?}: {gpu:?}");
            resources.rollback(&mut gpu);
            assert_eq!(
                gpu.destroyed.len(),
                destroyed.len(),
                "{fail_at:?}: double release"
            );
        }
    }

    #[test]
    fn failed_upload_restores_texture_and_shader_cache_entries() {
        let mut textures = GpuTextureCache::<usize>::default();
        textures
            .model
            .insert(("model.png".to_owned(), 0, TextureColorSpace::Srgb), 1);
        textures
            .scene
            .insert(("scene.png".to_owned(), TextureColorSpace::Linear), 2);
        textures
            .scene
            .insert(("new.png".to_owned(), TextureColorSpace::Srgb), 3);
        assert_eq!(
            remove_new_cached_textures(&mut textures, &HashSet::from([1, 2])),
            HashSet::from([3])
        );
        assert_eq!(textures.ids(), HashSet::from([1, 2]));

        let mut shaders = HashMap::from([
            ("old.glsl".to_owned(), Ok(10_usize)),
            ("broken-old.glsl".to_owned(), Err("old failure".to_owned())),
            ("new.glsl".to_owned(), Ok(11_usize)),
            ("broken-new.glsl".to_owned(), Err("new failure".to_owned())),
        ]);
        assert_eq!(
            restore_shader_cache(
                &mut shaders,
                &HashSet::from(["old.glsl".to_owned(), "broken-old.glsl".to_owned(),])
            ),
            HashSet::from([11])
        );
        assert_eq!(shaders.len(), 2);
        assert_eq!(shaders.get("old.glsl"), Some(&Ok(10)));
        assert_eq!(
            shaders.get("broken-old.glsl"),
            Some(&Err("old failure".to_owned()))
        );
    }
}
