use crate::pipeline::GpuSceneTable;
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
use engine_scene::{CameraSettings, PostProcessSettings, resolve_post_process};
use engine_script::{PrefabCatalog, PrefabKind, PrefabSpec};
use glam::Vec3;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::Hash;
use std::path::Path;
use std::sync::Arc;

/// Ресурсы одного объекта с мешом: уровни LOD (модель и текстура) и замена материала.
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
    let mut world = World::from_document(&expanded).map_err(|error| error.to_string())?;
    world
        .resolve_physics_materials(&mut |path: &VirtualPath| {
            read_utf8(resources, &format!("/{}", path.as_str()))
        })
        .map_err(|error| error.to_string())?;
    prepare_scene(resources, assets, scene_path, world, prefabs, scene_hash)
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
        scene_hash,
        render_assets: PreparedRenderAssets {
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

struct GpuTextureCache<T> {
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

fn read_utf8(resources: &ResourceHandle, path: &str) -> Result<String, String> {
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
    Ok(PreparedTemplate {
        models,
        textures,
        material_assets,
        shader_assets,
        levels,
        particles,
        sources,
    })
}

pub(crate) fn upload_scene(
    renderer: &mut Renderer,
    assets: &PreparedRenderAssets,
) -> Result<GpuSceneTable, RenderError> {
    let mut gpu_models: HashMap<String, Vec<(MeshId, MaterialId)>> = HashMap::new();
    let mut texture_cache = GpuTextureCache::<TextureId>::default();
    let mut shader_cache = HashMap::<String, Result<ShaderId, String>>::new();
    let mut material_cache = HashMap::<String, MaterialId>::new();
    let mut error_material = None;
    // Модели, которые анимирует хотя бы один объект сцены, загружаются со скином.
    let animated = animated_model_paths(assets);

    for (path, model) in &assets.models {
        let skinned = animated.contains(path.as_str()) && model.skeleton.is_some();
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
            let resolved = resolve_material(&mesh.material, None, None);
            let material_id = upload_resolved_material(
                renderer,
                &resolved,
                path,
                model,
                &assets.textures,
                &mut texture_cache,
                &mut material_cache,
                None,
            )?;
            gpu_meshes.push((mesh_id, material_id));
        }
        gpu_models.insert(path.clone(), gpu_meshes);
    }

    let mut table = GpuSceneTable {
        environments: assets.environments.clone(),
        animation_clips: assets.clips.clone(),
        ..GpuSceneTable::default()
    };
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
            assets,
            &gpu_models,
            &mut texture_cache,
            &mut shader_cache,
            &mut material_cache,
            &mut error_material,
            object,
        )?;
        if object.animated
            && let Some((model_path, _)) = object.levels.first()
            && let Some(skeleton) = assets
                .models
                .get(model_path)
                .and_then(|model| model.skeleton.as_ref())
        {
            // Экземпляр skinning на каждую часть уровня 0: выходные вершины живут отдельно.
            let mut instances = Vec::with_capacity(levels[0].len());
            for (mesh_id, _) in &levels[0] {
                instances.push(renderer.create_skin_instance(*mesh_id)?);
            }
            table.skin_instances.insert(*handle, instances);
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
                    assets,
                    &gpu_models,
                    &mut texture_cache,
                    &mut shader_cache,
                    &mut material_cache,
                    &mut error_material,
                    object,
                )?,
                None => Vec::new(),
            });
        }
        table.prefabs.insert(path.clone(), templates);
    }
    for (handle, source) in &assets.particles {
        let desc = particle_desc(renderer, assets, &mut texture_cache, source)?;
        table
            .particles
            .insert(*handle, renderer.create_particle_emitter(desc)?);
    }
    for (path, sources) in &assets.prefab_particles {
        let mut descs = Vec::with_capacity(sources.len());
        for source in sources {
            descs.push(match source {
                Some(source) => Some(particle_desc(renderer, assets, &mut texture_cache, source)?),
                None => None,
            });
        }
        table.particle_templates.insert(path.clone(), descs);
    }
    Ok(table)
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
fn upload_object_levels(
    renderer: &mut Renderer,
    assets: &PreparedRenderAssets,
    gpu_models: &HashMap<String, Vec<(MeshId, MaterialId)>>,
    texture_cache: &mut GpuTextureCache<TextureId>,
    shader_cache: &mut HashMap<String, Result<ShaderId, String>>,
    material_cache: &mut HashMap<String, MaterialId>,
    error_material: &mut Option<MaterialId>,
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
        let model = assets.models.get(model_path).ok_or({
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
                        &assets.textures,
                        texture_cache,
                        material_cache,
                        None,
                    )?
                }
            };
            draws.push((*mesh_id, material_id));
        }
        levels.push(draws);
    }
    Ok(levels)
}

/// Создаёт GPU-материал или возвращает уже созданный для тех же входных данных. Материалы живут
/// до завершения рендера вместе со сценой, поэтому кэш владения не меняет.
#[allow(clippy::too_many_arguments)]
fn upload_resolved_material(
    renderer: &mut Renderer,
    resolved: &ResolvedMaterial,
    model_path: &str,
    model: &Model,
    scene_texture_data: &HashMap<String, TextureData>,
    texture_cache: &mut GpuTextureCache<TextureId>,
    material_cache: &mut HashMap<String, MaterialId>,
    shader: Option<ShaderId>,
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
        scene_texture_data,
        texture_cache,
    )?;
    let metallic_roughness_texture = upload_material_texture(
        renderer,
        resolved.metallic_roughness_texture.as_ref(),
        TextureColorSpace::Linear,
        model_path,
        model,
        scene_texture_data,
        texture_cache,
    )?;
    let normal_texture = upload_material_texture(
        renderer,
        resolved.normal_texture.as_ref(),
        TextureColorSpace::Linear,
        model_path,
        model,
        scene_texture_data,
        texture_cache,
    )?;
    let emissive_texture = upload_material_texture(
        renderer,
        resolved.emissive_texture.as_ref(),
        TextureColorSpace::Srgb,
        model_path,
        model,
        scene_texture_data,
        texture_cache,
    )?;
    let occlusion_texture = upload_material_texture(
        renderer,
        resolved.occlusion_texture.as_ref(),
        TextureColorSpace::Linear,
        model_path,
        model,
        scene_texture_data,
        texture_cache,
    )?;
    let mut user_textures = [None; 8];
    for (slot, source) in resolved.user_texture_sources.iter().enumerate() {
        let Some(source) = source else { continue };
        let key = (source.path.clone(), source.color_space);
        let data = scene_texture_data.get(&source.path).ok_or_else(|| {
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
    assets: &PreparedRenderAssets,
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
) -> Result<MaterialId, RenderError> {
    let resolved = assets
        .material_assets
        .get(path)
        .ok_or_else(|| format!("материал /{path} не был подготовлен"))
        .and_then(|result| result.as_ref().map_err(Clone::clone))
        .and_then(|material| {
            let shader = material
                .shader
                .as_ref()
                .map(|shader_path| {
                    assets
                        .shader_assets
                        .get(shader_path.as_str())
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
        &assets.textures,
        texture_cache,
        material_cache,
        shader,
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
    scene_texture_data: &HashMap<String, TextureData>,
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
            let texture = scene_texture_data.get(path).ok_or({
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
        GpuTextureCache, MaterialTextureSource, PreparedPrefabs, get_or_insert_gpu, load_scene,
        material_cache_key, prepare_scene_with_model_loader, resolve_material,
        resolve_material_asset,
    };
    use engine_assets::{Assets, Model, ModelAlphaMode, ModelMaterial};
    use engine_files::VirtualPath;
    use engine_physics::BodyKind;
    use engine_project::PhysicsSettings;
    use engine_render::{AlphaMode, EnvironmentSource, TextureColorSpace};
    use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
    use engine_scene::{
        MaterialAlphaMode, MaterialAsset, MaterialOverride, PrefabTemplate, SceneDocument,
        ShaderAsset, World,
    };
    use engine_script::{
        Key, MouseButton, PrefabCatalog, PrefabKind, PrefabRequest, PrefabSpec, ScriptEventLevel,
        ScriptHost, ScriptInput, ScriptLoadOptions, ScriptTime,
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
        engine_script::ScriptHost::load(
            &prepared.world,
            "/scenes/physics.rvescene",
            &prepared.sources,
            None,
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
        ScriptHost::load_with_options(
            &prepared.world,
            "/scenes/physics.rvescene",
            &prepared.sources,
            None,
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
        let mut host = ScriptHost::load_with_options(
            &prepared.world,
            &prepared.name,
            &prepared.sources,
            None,
            ScriptLoadOptions {
                input: project.input,
                prefabs: prepared.prefabs.catalog.clone(),
                ..ScriptLoadOptions::default()
            },
        )
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
