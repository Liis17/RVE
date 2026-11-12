//! Подготовка кадра сцены: GPU-кэш ресурсов и команды отрисовки.

use crate::asset_watch::AssetChanges;
use crate::console::ConsoleLog;
use crate::materials::MaterialLibrary;
use crate::project::OpenProject;
use crate::state::MaterialPreviewPrimitive;
use crate::viewport::animation::{
    AnimationPreview, EditorAnimation, PreviewStatus, project_preview_bridge,
};
use crate::viewport::image_loader::{DecodedImage, ImageKind, ImageLoader};
use crate::viewport::model_loader::ModelLoader;
use engine_animation::{AnimationBridge, PaletteRemap};
use engine_assets::{Assets, Model, ModelAlphaMode, ModelSkeleton};
use engine_files::VirtualPath;
use engine_render::AlphaMode;
use engine_render::{
    AntiAliasing as RenderAntiAliasing, DebugView, DirectionalLight, DrawCommand, EnvironmentId,
    EnvironmentSource, Exposure, Frame, FrameCamera, FrameEnvironment, LocalLight,
    LocalLightShadow, LocalLightShadowMatrices, Material, MaterialId, MeshId, ParticleBlend,
    ParticleBurst, ParticleCollision, ParticleColorKey, ParticleEmitterDesc, ParticleEmitterFrame,
    ParticleEmitterId, ParticleFlipbook, ParticleShape, ParticleSizeKey, PostProcess,
    ProceduralSky, RenderError, Renderer, SHADOW_MAP_RESOLUTION, ShaderId, SkinBinding, SkinId,
    SkinVertex, SurfaceLighting, SurfaceShaderSource, TextureColorSpace, TextureId,
    Tonemapper as RenderTonemapper, Vertex, ViewSettings, ViewportId,
};
use engine_render::{
    ColorLutId, SCREEN_SHADER_PARAM_COUNT, ScreenShaderId, ScreenShaderPass, ScreenShaderStage,
    ScreenShaders,
};
use engine_scene::camera::Camera;
use engine_scene::document::{LodGroup, MaterialOverride, MeshRenderer};
use engine_scene::light::{
    cascade_view_projections, point_shadow_view_projections, spot_shadow_view_projection,
};
use engine_scene::world::ObjectHandle;
use engine_scene::world::ParticleCommand;
use engine_scene::{
    AntiAliasing, CameraSettings, EnvironmentState, LightState, LocalLightState, LodSelector,
    MaterialAlphaMode, MaterialAsset, MeshInstance, ParticleBlendMode, ParticleEmitterComponent,
    ParticleShapeComponent, ResolvedMaterialAsset, SceneFrame, ShaderAsset, ShaderColorSpace,
    ShaderPropertyKind, Tonemapper, World,
};
use engine_scene::{CubeLut, PostProcessSettings};
use engine_scene::{ScreenShaderRef, ScreenShaderStage as ScreenShaderStageDoc};
use glam::{Mat3, Mat4, Vec3};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;
use std::time::Instant;

const GRID_HALF_CELLS: i32 = 20;
const GRID_LINE_WIDTH: f32 = 0.02;

struct GpuModel {
    meshes: Vec<(MeshId, Material, MaterialId)>,
    textures: Vec<TextureId>,
    /// Меши созданы со скином: модель имеет скелет.
    skinned: bool,
    /// Число костей скелета; палитра моста должна иметь такую же длину.
    joint_count: u32,
    /// Скелет модели: по нему палитра уровня 0 переводится на модель уровня LOD.
    skeleton: Option<ModelSkeleton>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct AssetMaterialKey {
    material_revision: u64,
    shader_generation: u64,
    texture_revisions: Vec<(String, TextureColorSpace, u64)>,
}

struct GpuAssetMaterial {
    key: AssetMaterialKey,
    material: Material,
    id: MaterialId,
}

#[derive(Clone)]
struct GpuSurfaceShader {
    revision: u64,
    asset: ShaderAsset,
    id: ShaderId,
}

pub struct MaterialPreviewDraw {
    pub draw: DrawCommand,
    pub camera: Camera,
    pub environment: FrameEnvironment,
}

/// Экземпляры скиннинга объекта с `animator`: путь модели, её меши и по экземпляру на меш.
/// Типы параметризованы, чтобы планирование синхронизации тестировалось без GPU.
struct SkinnedObject<M = MeshId, S = SkinId> {
    model: String,
    joint_count: u32,
    mesh_ids: Vec<M>,
    instances: Vec<S>,
    /// Уровни LOD: элемент `i` относится к уровню `i + 1`; `None` — модель уровня без скелета
    /// или не загружена.
    lods: Vec<Option<LodSkin<M, S>>>,
}

/// Скиннинг уровня LOD: экземпляры его частей и перевод палитры уровня 0 на скелет уровня.
/// Без перевода (кости не совпали) экземпляров нет и уровень рисуется в bind pose.
struct LodSkin<M = MeshId, S = SkinId> {
    model: String,
    mesh_ids: Vec<M>,
    instances: Vec<S>,
    remap: Option<Arc<PaletteRemap>>,
}

/// Модель, меши и перевод палитры уровня LOD для создания его экземпляров skinning.
type NewLodSkin = (String, Vec<MeshId>, Option<Arc<PaletteRemap>>);

/// Желаемые модель и меши уровня 0 и уровней LOD объекта с аниматором.
type WantedSkin<M> = (String, Vec<M>, Vec<Option<(String, Vec<M>)>>);

/// GPU-ресурсы вьюпорта. Модели и текстуры кэшируются по virtual path; ошибка загрузки
/// запоминается и пишется в консоль один раз, пока кэш не будет сброшен.
pub struct SceneRenderer {
    pub viewport: ViewportId,
    pub game_viewport: ViewportId,
    /// Экранные шейдеры камер по пути `.glsl`; `id == None` — компиляция не удалась.
    screen_shaders: HashMap<String, EditorScreenShader>,
    /// Начало отсчёта времени для экранных шейдеров (`rve.time`).
    screen_shader_started: Instant,
    /// LUT цветокоррекции по каноническому пути `.cube`; `None` — LUT не загрузилась (ошибка уже в
    /// консоли), камера рисуется без неё.
    color_luts: HashMap<String, Option<ColorLutId>>,
    pub material_preview_viewport: Option<ViewportId>,
    models: HashMap<String, Option<GpuModel>>,
    model_dependencies: HashMap<String, Vec<VirtualPath>>,
    failed_reloads: HashSet<String>,
    /// Поколение каждой модели на момент последней загрузки или ошибки; мост превью
    /// пересобирается только при смене поколения модели аниматора.
    model_versions: HashMap<String, u64>,
    textures: HashMap<(String, TextureColorSpace), Option<TextureId>>,
    texture_revisions: HashMap<(String, TextureColorSpace), u64>,
    next_texture_revision: u64,
    asset_materials: HashMap<String, GpuAssetMaterial>,
    surface_shaders: HashMap<String, GpuSurfaceShader>,
    retired_surface_shaders: Vec<ShaderId>,
    failed_surface_shaders: HashSet<(String, u64)>,
    failed_asset_materials: HashSet<(String, AssetMaterialKey)>,
    error_material: Option<MaterialId>,
    preview_default_material: Option<MaterialId>,
    preview_asset_material: Option<(String, MaterialId)>,
    preview_shader_material: Option<(String, AssetMaterialKey, Material, MaterialId)>,
    preview_environment: Option<EnvironmentId>,
    preview_meshes: HashMap<MaterialPreviewPrimitive, MeshId>,
    reported_material_errors: HashSet<String>,
    reported_material_warnings: HashSet<String>,
    environment_sources: HashMap<String, Option<Arc<EnvironmentSource>>>,
    /// Фоновое декодирование PNG и HDR и корень проекта, для которого оно запущено.
    images: ImageLoader,
    images_root: Option<std::path::PathBuf>,
    /// Цветовые пространства, в которых ждут декодирования PNG по пути.
    texture_wants: HashMap<String, HashSet<TextureColorSpace>>,
    current_environment: Option<(EditorEnvironmentKey, EnvironmentId)>,
    failed_environment: Option<EditorEnvironmentKey>,
    object_materials: HashMap<(ObjectHandle, String, usize, usize), (Material, MaterialId)>,
    grid: Option<MeshId>,
    grid_material: Option<MaterialId>,
    grid_shader: Option<ShaderId>,
    scene_draws: Vec<DrawCommand>,
    game_draws: Vec<DrawCommand>,
    /// Viewport захвата с отдельной камерой; создаётся только на время `capture_view`.
    capture_viewport: Option<ViewportId>,
    /// Список захвата: тот же мир, что и панели, но отсечение и LOD по камере захвата.
    capture_draws: Vec<DrawCommand>,
    motion_history: MotionHistory,
    /// Скиннинг аниматоров сцены по объектам.
    skins: HashMap<ObjectHandle, SkinnedObject>,
    /// Палитры уровней LOD кадра; в `joint_palette_refs` идут после палитр моста.
    lod_palettes: Vec<Vec<[[f32; 4]; 4]>>,
    /// История выбора LOD панелей Scene, Game и камеры захвата для гистерезиса.
    scene_lod: LodSelector,
    game_lod: LodSelector,
    capture_lod: LodSelector,
    /// Мост редактора для превью и отпечаток его аниматоров.
    animation: EditorAnimation,
    /// Счётчик загрузок моделей и сбросов кэша; по нему заново читаются файлы клипов.
    model_generation: u64,
    /// Объекты, для которых уже выведено предупреждение о несовпадении палитры и меша.
    skin_mismatch_reported: HashSet<ObjectHandle>,
    particle_emitters: HashMap<ObjectHandle, EditorParticleEmitter>,
    particle_frames: Vec<ParticleEmitterFrame>,
    particle_step: f32,
    particle_clock: Option<Instant>,
    particles_playing: Option<bool>,
    playing: bool,
}

#[derive(Default)]
struct MotionHistory {
    previous_models: HashMap<ObjectHandle, [[f32; 4]; 4]>,
    current_models: HashMap<ObjectHandle, [[f32; 4]; 4]>,
    game_camera: Option<ObjectHandle>,
    playing: Option<bool>,
    reset_pending: bool,
}

impl MotionHistory {
    fn begin_frame(&mut self, game_camera: ObjectHandle, playing: bool) {
        let reset = self
            .game_camera
            .is_some_and(|previous| previous != game_camera)
            || self.playing.is_some_and(|previous| previous != playing);
        if reset {
            self.previous_models.clear();
            self.reset_pending = true;
        }
        self.game_camera = Some(game_camera);
        self.playing = Some(playing);
        self.current_models.clear();
    }

    fn previous_model(&self, object: ObjectHandle) -> Option<[[f32; 4]; 4]> {
        self.previous_models.get(&object).copied()
    }

    fn record_current(&mut self, object: ObjectHandle, model: [[f32; 4]; 4]) {
        self.current_models.insert(object, model);
    }

    fn commit_frame(&mut self) {
        self.previous_models = std::mem::take(&mut self.current_models);
        self.reset_pending = false;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum EditorEnvironmentKey {
    Hdr(String),
    Procedural {
        colors_and_sun: [u32; 11],
        sun_direction: [i32; 3],
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvironmentCreation {
    Success,
    MissingSource,
    ResourceError,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvironmentTransition {
    Keep,
    Replace,
    DropToDefault,
}

fn environment_transition(
    current: Option<&EditorEnvironmentKey>,
    requested: &EditorEnvironmentKey,
    creation: EnvironmentCreation,
) -> EnvironmentTransition {
    match creation {
        EnvironmentCreation::Success => EnvironmentTransition::Replace,
        EnvironmentCreation::MissingSource | EnvironmentCreation::ResourceError
            if current == Some(requested) =>
        {
            EnvironmentTransition::Keep
        }
        EnvironmentCreation::MissingSource | EnvironmentCreation::ResourceError => {
            EnvironmentTransition::DropToDefault
        }
    }
}

/// GPU-эмиттер редактора: id в renderer и компонент, по которому он создан.
struct EditorParticleEmitter {
    id: ParticleEmitterId,
    component: ParticleEmitterComponent,
    texture: Option<String>,
    texture_id: Option<TextureId>,
}

/// Создаёт значение через `create` и вставляет его в `entries` вместо прежнего. Возвращает прежнее значение,
/// которое вызывающий уничтожает уже после успешного создания; при ошибке `entries` не меняется.
fn replace_entry<K: Eq + Hash, V, E>(
    entries: &mut HashMap<K, V>,
    key: K,
    create: impl FnOnce() -> Result<V, E>,
) -> Result<Option<V>, E> {
    let value = create()?;
    Ok(entries.insert(key, value))
}

/// Переводит компонент сцены в описание эмиттера `engine-render`. Совпадает с сопоставлением в engine-runtime.
fn editor_particle_desc(
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

impl SceneRenderer {
    pub fn new(renderer: &mut Renderer) -> Result<Self, RenderError> {
        Ok(Self {
            viewport: renderer.create_viewport(1, 1)?,
            game_viewport: renderer.create_viewport(1, 1)?,
            material_preview_viewport: None,
            models: HashMap::new(),
            model_dependencies: HashMap::new(),
            failed_reloads: HashSet::new(),
            model_versions: HashMap::new(),
            textures: HashMap::new(),
            images: ImageLoader::default(),
            images_root: None,
            texture_wants: HashMap::new(),
            texture_revisions: HashMap::new(),
            next_texture_revision: 0,
            asset_materials: HashMap::new(),
            surface_shaders: HashMap::new(),
            retired_surface_shaders: Vec::new(),
            failed_surface_shaders: HashSet::new(),
            failed_asset_materials: HashSet::new(),
            error_material: None,
            preview_default_material: None,
            preview_asset_material: None,
            preview_shader_material: None,
            preview_environment: None,
            preview_meshes: HashMap::new(),
            reported_material_errors: HashSet::new(),
            reported_material_warnings: HashSet::new(),
            particle_emitters: HashMap::new(),
            particle_frames: Vec::new(),
            particle_step: 0.0,
            particle_clock: None,
            particles_playing: None,
            screen_shaders: HashMap::new(),
            screen_shader_started: Instant::now(),
            color_luts: HashMap::new(),
            playing: false,
            environment_sources: HashMap::new(),
            current_environment: None,
            failed_environment: None,
            object_materials: HashMap::new(),
            grid: None,
            grid_material: None,
            grid_shader: None,
            scene_draws: Vec::new(),
            game_draws: Vec::new(),
            capture_viewport: None,
            capture_draws: Vec::new(),
            motion_history: MotionHistory::default(),
            skins: HashMap::new(),
            lod_palettes: Vec::new(),
            scene_lod: LodSelector::default(),
            game_lod: LodSelector::default(),
            capture_lod: LodSelector::default(),
            animation: EditorAnimation::default(),
            model_generation: 0,
            skin_mismatch_reported: HashSet::new(),
        })
    }

    /// Удаляет кэшированные ресурсы: файлы на диске могли измениться.
    pub fn clear_cache(
        &mut self,
        renderer: &mut Renderer,
        cpu_models: &mut HashMap<String, Arc<Model>>,
    ) -> Result<(), RenderError> {
        if let Some((_, environment)) = self.current_environment.take() {
            renderer.destroy_environment(environment)?;
        }
        if let Some(environment) = self.preview_environment.take() {
            renderer.destroy_environment(environment)?;
        }
        self.environment_sources.clear();
        self.images_root = None;
        self.texture_wants.clear();
        self.model_dependencies.clear();
        self.failed_reloads.clear();
        self.failed_environment = None;
        self.model_generation += 1;
        self.model_versions.clear();
        self.skin_mismatch_reported.clear();
        self.release_skins(renderer)?;
        for model in self.models.drain().filter_map(|(_, model)| model) {
            destroy_model(renderer, model)?;
        }
        for (_, (_, material)) in self.object_materials.drain() {
            renderer.destroy_material(material)?;
        }
        for (_, material) in self.asset_materials.drain() {
            renderer.destroy_material(material.id)?;
        }
        self.preview_asset_material = None;
        if let Some((_, _, _, material)) = self.preview_shader_material.take() {
            renderer.destroy_material(material)?;
        }
        if let Some(material) = self.preview_default_material.take() {
            renderer.destroy_material(material)?;
        }
        if let Some(material) = self.error_material.take() {
            renderer.destroy_material(material)?;
        }
        for (_, shader) in self.surface_shaders.drain() {
            renderer.destroy_surface_shader(shader.id)?;
        }
        for shader in self.retired_surface_shaders.drain(..) {
            renderer.destroy_surface_shader(shader)?;
        }
        for texture in self.textures.drain().filter_map(|(_, texture)| texture) {
            renderer.destroy_texture(texture)?;
        }
        for shader in self
            .screen_shaders
            .drain()
            .filter_map(|(_, entry)| entry.id)
        {
            renderer.destroy_screen_shader(shader)?;
        }
        for lut in self.color_luts.drain().filter_map(|(_, lut)| lut) {
            renderer.destroy_color_lut(lut)?;
        }
        for mesh in self.preview_meshes.drain().map(|(_, mesh)| mesh) {
            renderer.destroy_mesh(mesh)?;
        }
        self.texture_revisions.clear();
        self.reported_material_errors.clear();
        self.reported_material_warnings.clear();
        self.failed_surface_shaders.clear();
        self.failed_asset_materials.clear();
        cpu_models.clear();
        Ok(())
    }

    /// Загружает ресурсы объектов мира, которых ещё нет в кэше. Модели разбираются в фоне:
    /// до готовности объект не рисуется, а готовые модели загружаются в GPU в этом вызове.
    pub fn sync_assets(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        world: &World,
        cpu_models: &mut HashMap<String, Arc<Model>>,
        loader: &mut ModelLoader,
        log: &mut ConsoleLog,
    ) -> Result<(), RenderError> {
        let mut active_environments = HashSet::new();
        self.accept_images(renderer, project, log)?;
        let finished = loader.finished();
        if !finished.is_empty() {
            // Экземпляры ссылаются на меши, которые сейчас могут быть заменены: освобождаем их заранее.
            self.release_skins(renderer)?;
        }
        for (path, loaded) in finished {
            // Успех и ошибка меняют состояние модели: мост аниматоров, который её использует,
            // должен это увидеть (загрузка или предупреждение). Остальные мосты не затрагиваются.
            self.model_generation += 1;
            self.model_versions
                .insert(path.clone(), self.model_generation);
            let previous = self.models.remove(&path).flatten();
            let had_previous = previous.is_some();
            match loaded {
                Ok(loaded) => {
                    if let Some(warning) = loaded.dependency_warning {
                        log.push(
                            crate::console::LogLevel::Warn,
                            format!("{path}: не удалось прочитать зависимости модели: {warning}"),
                        );
                    }
                    let model = Arc::new(loaded.model);
                    let gpu = match upload_model(renderer, &model) {
                        Ok(gpu) => gpu,
                        Err(error) if is_resource_error(&error) => {
                            self.models.insert(path.clone(), previous);
                            self.failed_reloads.insert(path.clone());
                            let message = if had_previous {
                                format!("{path}: {error}; сохранена прежняя версия модели")
                            } else {
                                format!("{path}: {error}")
                            };
                            log.error(message);
                            continue;
                        }
                        Err(error) => {
                            self.models.insert(path, previous);
                            return Err(error);
                        }
                    };
                    let (current, old, _) = cache_update(previous, Ok(gpu));
                    self.models.insert(path.clone(), current);
                    self.model_dependencies
                        .insert(path.clone(), loaded.dependencies);
                    self.failed_reloads.remove(&path);
                    cpu_models.insert(path, model);
                    if let Some(old) = old {
                        destroy_model(renderer, old)?;
                    }
                }
                Err(error) => {
                    let (current, _, error) = cache_update(previous, Err(error));
                    self.models.insert(path.clone(), current);
                    self.failed_reloads.insert(path.clone());
                    if let Some(error) = error {
                        if had_previous {
                            log.error(format!("{path}: {error}; сохранена прежняя версия модели"));
                        } else {
                            log.error(format!("{path}: {error}"));
                        }
                    }
                }
            }
        }
        for object in world.objects() {
            if let Some(path) = object
                .particle_emitter
                .as_ref()
                .and_then(|emitter| emitter.texture.as_ref())
            {
                self.sync_texture(
                    renderer,
                    project,
                    log,
                    path.as_str(),
                    TextureColorSpace::Srgb,
                )?;
            }
        }
        for object in world.objects() {
            if let Some(path) = object
                .environment
                .as_ref()
                .and_then(|environment| environment.hdr.as_ref())
            {
                let path = path.as_str();
                active_environments.insert(path.to_owned());
                if !self.environment_sources.contains_key(path)
                    && !self.images.request(ImageKind::Hdr, path)
                {
                    log.error(format!("{path}: фоновая загрузка недоступна"));
                    self.environment_sources.insert(path.to_owned(), None);
                }
            }
            let Some(mesh) = &object.mesh_renderer else {
                continue;
            };
            let levels = object
                .lod_group
                .as_ref()
                .map_or(1, |group| group.levels.len() + 1);
            for level in 0..levels {
                let Some(model_path) = lod_model_path(mesh, object.lod_group.as_ref(), level)
                else {
                    continue;
                };
                if !self.models.contains_key(model_path) {
                    match cpu_models.get(model_path).cloned() {
                        Some(model) => match upload_model(renderer, &model) {
                            Ok(gpu) => {
                                self.models.insert(model_path.to_owned(), Some(gpu));
                                self.failed_reloads.remove(model_path);
                            }
                            Err(error) if is_resource_error(&error) => {
                                self.models.insert(model_path.to_owned(), None);
                                self.failed_reloads.insert(model_path.to_owned());
                                log.error(format!("{model_path}: {error}"));
                            }
                            Err(error) => return Err(error),
                        },
                        None => {
                            if !loader.request(model_path) {
                                log.error(format!("{model_path}: фоновая загрузка недоступна"));
                                self.models.insert(model_path.to_owned(), None);
                            }
                        }
                    }
                }
            }
            for level in 0..levels {
                if let Some(path) = lod_texture_path(mesh, object.lod_group.as_ref(), level) {
                    self.sync_texture(renderer, project, log, path, TextureColorSpace::Srgb)?;
                }
            }
            if let Some(override_material) = &mesh.material {
                for (path, color_space) in [
                    override_material
                        .metallic_roughness_texture
                        .as_ref()
                        .map(|path| (path.as_str(), TextureColorSpace::Linear)),
                    override_material
                        .normal_texture
                        .as_ref()
                        .map(|path| (path.as_str(), TextureColorSpace::Linear)),
                    override_material
                        .emissive_texture
                        .as_ref()
                        .map(|path| (path.as_str(), TextureColorSpace::Srgb)),
                    override_material
                        .occlusion_texture
                        .as_ref()
                        .map(|path| (path.as_str(), TextureColorSpace::Linear)),
                ]
                .into_iter()
                .flatten()
                {
                    self.sync_texture(renderer, project, log, path, color_space)?;
                }
            }
        }
        self.environment_sources
            .retain(|path, _| active_environments.contains(path));
        Ok(())
    }

    /// Создаёт экранные шейдеры камеры, которых ещё нет в кэше. Ошибка записывается в `errors`
    /// и не мешает кадру: проход пропускается.
    pub fn sync_screen_shaders(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        screens: &[ScreenShaderRef],
        errors: &mut HashMap<String, String>,
    ) {
        for shader in screens {
            let path = shader.shader.as_str();
            if self.screen_shaders.contains_key(path) {
                continue;
            }
            let entry = match load_screen_shader(renderer, project, path) {
                Ok(id) => {
                    errors.remove(path);
                    EditorScreenShader { id: Some(id) }
                }
                Err(error) => {
                    errors.insert(path.to_owned(), error);
                    EditorScreenShader { id: None }
                }
            };
            self.screen_shaders.insert(path.to_owned(), entry);
        }
    }

    /// Создаёт LUT цветокоррекции постобработки, если её ещё нет в кэше. Ошибка пишется в консоль
    /// один раз и не мешает кадру: камера рисуется без LUT, пока файл не изменится.
    pub fn sync_color_lut(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        post: &PostProcessSettings,
        log: &mut ConsoleLog,
    ) {
        let Some(path) = post.color_grading.lut_path() else {
            return;
        };
        if self.color_luts.contains_key(path.as_str()) {
            return;
        }
        let lut = match load_color_lut(renderer, project, path.as_str()) {
            Ok(id) => Some(id),
            Err(error) => {
                log.error(format!("LUT цветокоррекции: {error}"));
                None
            }
        };
        self.color_luts.insert(path.as_str().to_owned(), lut);
    }

    fn color_lut(&self, post: &PostProcessSettings) -> Option<ColorLutId> {
        cached_color_lut(&self.color_luts, post)
    }

    /// Экранные шейдеры вида. Scene-вид их не применяет: искажения мешали бы редактированию.
    fn screen_shader_view(&self, screens: &[ScreenShaderRef], with_grid: bool) -> ScreenShaders {
        let mut view = ScreenShaders {
            time: self.screen_shader_started.elapsed().as_secs_f32(),
            ..ScreenShaders::default()
        };
        if with_grid {
            view.time = 0.0;
            return view;
        }
        let known = screens
            .iter()
            .filter(|shader| shader.enabled)
            .filter_map(|shader| {
                let id = self.screen_shaders.get(shader.shader.as_str())?.id?;
                Some((id, shader))
            });
        for (slot, (shader, source)) in known.take(view.passes.len()).enumerate() {
            let mut params = [0.0_f32; SCREEN_SHADER_PARAM_COUNT];
            for (value, parameter) in params.iter_mut().zip(&source.params) {
                *value = *parameter;
            }
            view.passes[slot] = Some(ScreenShaderPass {
                shader,
                stage: match source.stage {
                    ScreenShaderStageDoc::BeforePost => ScreenShaderStage::BeforePost,
                    ScreenShaderStageDoc::AfterPost => ScreenShaderStage::AfterPost,
                },
                params,
            });
        }
        view
    }

    pub fn ensure_material_preview_viewport(
        &mut self,
        renderer: &mut Renderer,
    ) -> Result<ViewportId, RenderError> {
        if let Some(viewport) = self.material_preview_viewport {
            return Ok(viewport);
        }
        let viewport = renderer.create_viewport(1, 1)?;
        self.material_preview_viewport = Some(viewport);
        Ok(viewport)
    }

    pub fn free_material_preview_viewport(
        &mut self,
        renderer: &mut Renderer,
    ) -> Result<(), RenderError> {
        if let Some(viewport) = self.material_preview_viewport.take() {
            renderer.free_viewport(viewport)?;
        }
        Ok(())
    }

    /// Перезагружает только ресурсы, совпавшие с изменёнными путями или зависимостями.
    pub fn reload_changed(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        changes: &AssetChanges,
        loader: &mut ModelLoader,
        log: &mut ConsoleLog,
        screen_errors: &mut HashMap<String, String>,
    ) -> Result<(), RenderError> {
        let texture_keys = self
            .textures
            .keys()
            .filter(|(path, _)| key_matches_changes(changes, path))
            .cloned()
            .collect::<Vec<_>>();
        for (path, color_space) in texture_keys {
            self.texture_wants
                .entry(path.clone())
                .or_default()
                .insert(color_space);
            self.images.reload(ImageKind::Texture, &path);
        }

        // Изменённый `.glsl` (при rescan — все кэшированные) перекомпилируется; ошибка оставляет
        // прежнюю версию и текст для инспектора.
        let changed_shaders = self
            .screen_shaders
            .keys()
            .filter(|path| key_matches_changes(changes, path))
            .cloned()
            .collect::<Vec<_>>();
        for path in changed_shaders {
            match load_screen_shader(renderer, project, &path) {
                Ok(id) => {
                    screen_errors.remove(&path);
                    let previous = self
                        .screen_shaders
                        .insert(path, EditorScreenShader { id: Some(id) });
                    if let Some(previous) = previous.and_then(|entry| entry.id) {
                        renderer.destroy_screen_shader(previous)?;
                    }
                }
                Err(error) => {
                    log.error(format!(
                        "экранный шейдер: {error}; сохранена прежняя версия"
                    ));
                    screen_errors.insert(path, error);
                }
            }
        }

        // Изменённая `.cube` пересоздаётся; ошибка оставляет прежнюю LUT.
        let changed_luts = self
            .color_luts
            .keys()
            .filter(|path| key_matches_changes(changes, path))
            .cloned()
            .collect::<Vec<_>>();
        for path in changed_luts {
            match load_color_lut(renderer, project, &path) {
                Ok(id) => {
                    if let Some(previous) = self.color_luts.insert(path, Some(id)).flatten() {
                        renderer.destroy_color_lut(previous)?;
                    }
                }
                Err(error) => {
                    log.error(format!(
                        "LUT цветокоррекции: {error}; сохранена прежняя версия"
                    ));
                }
            }
        }

        let environment_paths = self
            .environment_sources
            .keys()
            .filter(|path| {
                changes.rescan
                    || changes
                        .hdr_paths()
                        .any(|changed| same_virtual_path(changed.as_str(), path))
                    || changes.directory_paths.iter().any(|changed| {
                        VirtualPath::parse(path).is_ok_and(|key| path_is_parent(changed, &key))
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        for path in environment_paths {
            self.images.reload(ImageKind::Hdr, &path);
        }

        let pending = loader
            .pending_paths()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let plan = model_reload_plan(
            changes,
            &self.models,
            &self.model_dependencies,
            &self.failed_reloads,
            &pending,
        );
        let unknown_dependencies_changed = changes.rescan
            || changes.png_paths().next().is_some()
            || !changes.directory_paths.is_empty();
        if unknown_dependencies_changed {
            loader.stale_pending();
        } else {
            for path in plan.pending {
                loader.reload(&path);
            }
        }
        for path in plan.cached {
            loader.reload(&path);
        }
        Ok(())
    }

    /// Применяет декодированную PNG к кэшу текстуры в цветовом пространстве `color_space`.
    /// Первая загрузка и перезагрузка идут одним путём: ошибка сохраняет прежнюю текстуру.
    fn apply_texture(
        &mut self,
        renderer: &mut Renderer,
        log: &mut ConsoleLog,
        path: String,
        color_space: TextureColorSpace,
        result: Result<&engine_assets::TextureData, String>,
    ) -> Result<(), RenderError> {
        let key = (path.clone(), color_space);
        let previous = self.textures.get(&key).copied().flatten();
        let had_previous = previous.is_some();
        match result {
            Ok(data) => {
                let replacement = match renderer.create_texture_with_color_space(
                    data.width,
                    data.height,
                    &data.rgba,
                    color_space,
                ) {
                    Ok(replacement) => replacement,
                    Err(error) if is_resource_error(&error) => {
                        let (current, _, error) = cache_update(previous, Err(error.to_string()));
                        self.textures.insert(key, current);
                        if let Some(error) = error {
                            let message = if had_previous {
                                format!("{path}: {error}; сохранена прежняя версия текстуры")
                            } else {
                                format!("{path}: {error}")
                            };
                            log.error(message);
                        }
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                let (current, old, _) = cache_update(previous, Ok(replacement));
                self.textures.insert(key, current);
                self.bump_texture_revision((path.clone(), color_space));
                if let Some(old) = old {
                    renderer.destroy_texture(old)?;
                }
            }
            Err(error) => {
                let (current, _, error) = cache_update(previous, Err(error.to_string()));
                self.textures.insert(key, current);
                if let Some(error) = error {
                    let message = if had_previous {
                        format!("{path}: {error}; сохранена прежняя версия текстуры")
                    } else {
                        format!("{path}: {error}")
                    };
                    log.error(message);
                }
            }
        }
        Ok(())
    }

    /// Применяет декодированный HDR к источникам окружения; текущее GPU-окружение этого пути
    /// пересоздаётся. Ошибка сохраняет прежний источник.
    fn apply_hdr(
        &mut self,
        renderer: &mut Renderer,
        log: &mut ConsoleLog,
        path: String,
        result: Result<engine_assets::HdrImageData, String>,
    ) -> Result<(), RenderError> {
        let previous = self.environment_sources.get(&path).cloned().flatten();
        let had_previous = previous.is_some();
        match result {
            Ok(image) => {
                let replacement = Arc::new(EnvironmentSource::Equirect {
                    width: image.width,
                    height: image.height,
                    rgb: image.rgb,
                });
                let key = EditorEnvironmentKey::Hdr(path.clone());
                let current_matches = self
                    .current_environment
                    .as_ref()
                    .is_some_and(|(current, _)| current == &key);
                let gpu_replacement = if current_matches {
                    match renderer.create_environment(&replacement) {
                        Ok(environment) => Some(environment),
                        Err(error) if is_resource_error(&error) => {
                            let transition = environment_transition(
                                self.current_environment
                                    .as_ref()
                                    .map(|(current, _)| current),
                                &key,
                                EnvironmentCreation::ResourceError,
                            );
                            let (current, _, _) = cache_update(previous, Err(error.to_string()));
                            self.environment_sources.insert(path.clone(), current);
                            if transition == EnvironmentTransition::Keep {
                                log.error(format!("{path}: {error}; сохранено прежнее окружение"));
                            } else {
                                log.error(format!("{path}: {error}"));
                                if let Some((_, previous)) = self.current_environment.take() {
                                    renderer.destroy_environment(previous)?;
                                }
                            }
                            self.failed_environment = Some(key);
                            return Ok(());
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    None
                };
                self.environment_sources
                    .insert(path.clone(), Some(replacement));
                if let Some(environment) = gpu_replacement {
                    let previous = self.current_environment.replace((key, environment));
                    self.failed_environment = None;
                    if let Some((_, previous)) = previous {
                        renderer.destroy_environment(previous)?;
                    }
                } else if self.failed_environment.as_ref() == Some(&key) {
                    self.failed_environment = None;
                }
            }
            Err(error) => {
                let (current, _, error) = cache_update(previous, Err(error.to_string()));
                self.environment_sources.insert(path.clone(), current);
                if let Some(error) = error {
                    let message = if had_previous {
                        format!("{path}: {error}; сохранена прежняя версия HDR")
                    } else {
                        format!("{path}: {error}")
                    };
                    log.error(message);
                }
            }
        }
        Ok(())
    }

    /// Принимает изображения, декодированные в фоне, и создаёт для них GPU-ресурсы.
    fn accept_images(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        log: &mut ConsoleLog,
    ) -> Result<(), RenderError> {
        self.ensure_image_loader(project);
        for (kind, path, result) in self.images.finished() {
            match (kind, result) {
                (ImageKind::Texture, result) => {
                    let data = result.map(|image| match image {
                        DecodedImage::Texture(data) => data,
                        DecodedImage::Hdr(_) => unreachable!("запрос текстуры вернул HDR"),
                    });
                    let wants = self.texture_wants.remove(&path).unwrap_or_default();
                    for color_space in wants {
                        self.apply_texture(
                            renderer,
                            log,
                            path.clone(),
                            color_space,
                            data.as_ref().map_err(Clone::clone),
                        )?;
                    }
                }
                (ImageKind::Hdr, result) => {
                    let image = result.map(|image| match image {
                        DecodedImage::Hdr(image) => image,
                        DecodedImage::Texture(_) => unreachable!("запрос HDR вернул текстуру"),
                    });
                    self.apply_hdr(renderer, log, path, image)?;
                }
            }
        }
        Ok(())
    }

    /// Декодер изображений работает с ресурсами открытого проекта; смена проекта
    /// перезапускает его, и результаты прежнего проекта отбрасываются.
    fn ensure_image_loader(&mut self, project: &OpenProject) {
        if self.images_root.as_deref() != Some(project.root.as_path()) {
            self.images.restart(project.assets.clone());
            self.images_root = Some(project.root.clone());
            self.texture_wants.clear();
        }
    }

    /// Есть изображения, которые ещё декодируются: окно должно проснуться, чтобы показать их.
    pub fn images_pending(&self) -> bool {
        self.images.has_pending()
    }

    /// Запрашивает текстуру, если её ещё нет в кэше. PNG декодируется в фоне: до готовности
    /// материал рисуется без неё, а готовая текстура повышает ревизию и пересобирает материалы.
    fn sync_texture(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        log: &mut ConsoleLog,
        path: &str,
        color_space: TextureColorSpace,
    ) -> Result<(), RenderError> {
        let key = (path.to_owned(), color_space);
        if self.textures.contains_key(&key) {
            return Ok(());
        }
        self.accept_images(renderer, project, log)?;
        if self.textures.contains_key(&key)
            || self
                .texture_wants
                .get(path)
                .is_some_and(|wants| wants.contains(&color_space))
        {
            return Ok(());
        }
        if self.images.request(ImageKind::Texture, path) {
            self.texture_wants
                .entry(path.to_owned())
                .or_default()
                .insert(color_space);
        } else {
            log.error(format!("{path}: фоновая загрузка недоступна"));
            self.textures.insert(key, None);
        }
        Ok(())
    }

    fn bump_texture_revision(&mut self, key: (String, TextureColorSpace)) {
        self.next_texture_revision = self.next_texture_revision.wrapping_add(1).max(1);
        self.texture_revisions
            .insert(key, self.next_texture_revision);
    }

    fn ensure_error_material(
        &mut self,
        renderer: &mut Renderer,
    ) -> Result<MaterialId, RenderError> {
        if let Some(material) = self.error_material {
            return Ok(material);
        }
        let material = renderer.create_material(&Material {
            base_color: [1.0, 0.0, 1.0],
            emissive: [0.08, 0.0, 0.08],
            roughness: 1.0,
            ..Material::default()
        })?;
        self.error_material = Some(material);
        Ok(material)
    }

    fn ensure_surface_shader(
        &mut self,
        renderer: &mut Renderer,
        path: &str,
        library: &MaterialLibrary,
        log: &mut ConsoleLog,
    ) -> Result<Option<GpuSurfaceShader>, RenderError> {
        let Some(cached) = library.shader(path) else {
            self.report_material_error(log, format!("{path}: шейдер отсутствует в библиотеке"));
            return Ok(None);
        };
        if let Some(error) = &cached.error {
            self.report_material_error(log, error.clone());
        }
        let Some(asset) = cached.asset.as_ref() else {
            if let Some(error) = &cached.error {
                self.report_material_error(log, error.clone());
            }
            return Ok(None);
        };
        if let Some(shader) = self.surface_shaders.get(path)
            && shader.revision == cached.revision
        {
            return Ok(Some(shader.clone()));
        }
        if self
            .failed_surface_shaders
            .contains(&(path.to_owned(), cached.revision))
        {
            return Ok(self.surface_shaders.get(path).cloned());
        }
        let source = library
            .shader_source(path)
            .unwrap_or_else(|| crate::materials::surface_source(asset));
        match renderer.create_surface_shader(&source) {
            Ok(id) => {
                let replacement = GpuSurfaceShader {
                    revision: cached.revision,
                    asset: asset.clone(),
                    id,
                };
                if let Some(previous) = self
                    .surface_shaders
                    .insert(path.to_owned(), replacement.clone())
                {
                    self.retired_surface_shaders.push(previous.id);
                }
                Ok(Some(replacement))
            }
            Err(RenderError::InvalidShader(error)) => {
                self.failed_surface_shaders
                    .insert((path.to_owned(), cached.revision));
                let message = if self.surface_shaders.contains_key(path) {
                    format!("{path}: {error}; сохранена прежняя версия шейдера")
                } else {
                    format!("{path}: {error}")
                };
                self.report_material_error(log, message);
                Ok(self.surface_shaders.get(path).cloned())
            }
            Err(error) => Err(error),
        }
    }

    fn report_material_error(&mut self, log: &mut ConsoleLog, message: String) {
        if self.reported_material_errors.insert(message.clone()) {
            log.error(message);
        }
    }

    fn prepare_asset_materials(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        library: &MaterialLibrary,
        paths: &HashSet<String>,
        preview_shader_path: Option<&str>,
        log: &mut ConsoleLog,
    ) -> Result<HashMap<String, (Material, MaterialId)>, RenderError> {
        let error_id = self.ensure_error_material(renderer)?;
        self.reload_surface_shaders_transactionally(
            renderer,
            project,
            library,
            paths,
            preview_shader_path,
            log,
        )?;
        let mut prepared = HashMap::with_capacity(paths.len());
        for path in paths {
            let Some(cached) = library.material(path).cloned() else {
                self.report_material_error(
                    log,
                    format!("{path}: материал отсутствует в библиотеке"),
                );
                prepared.insert(path.clone(), (error_material(), error_id));
                continue;
            };
            if let Some(error) = cached.error.as_ref().filter(|_| cached.draft.is_none()) {
                self.report_material_error(log, error.clone());
                prepared.insert(path.clone(), (error_material(), error_id));
                continue;
            }
            let Some(asset) = cached.effective_asset().cloned() else {
                prepared.insert(path.clone(), (error_material(), error_id));
                continue;
            };
            let shader = if let Some(shader_path) = &asset.shader {
                self.ensure_surface_shader(renderer, shader_path.as_str(), library, log)?
            } else {
                None
            };
            if asset.shader.is_some() && shader.is_none() {
                prepared.insert(path.clone(), (error_material(), error_id));
                continue;
            }
            let (key, material) = self.resolve_asset_material(
                renderer,
                project,
                path,
                cached.revision,
                &asset,
                shader.as_ref(),
                log,
            )?;
            let current = self.asset_materials.get(path);
            let id = if current.is_some_and(|current| current.key == key) {
                current.unwrap().id
            } else if self
                .failed_asset_materials
                .contains(&(path.clone(), key.clone()))
            {
                if let Some(current) = current {
                    prepared.insert(path.clone(), (current.material, current.id));
                    continue;
                }
                prepared.insert(path.clone(), (error_material(), error_id));
                continue;
            } else {
                match renderer.create_material(&material) {
                    Ok(id) => {
                        self.failed_asset_materials
                            .retain(|(failed_path, _)| failed_path != path);
                        let previous = self
                            .asset_materials
                            .insert(path.clone(), GpuAssetMaterial { key, material, id });
                        if let Some(previous) = previous {
                            renderer.destroy_material(previous.id)?;
                        }
                        id
                    }
                    Err(error) if is_resource_error(&error) => {
                        self.failed_asset_materials
                            .insert((path.clone(), key.clone()));
                        let message = format!("{path}: {error}; используется материал ошибки");
                        self.report_material_error(log, message);
                        prepared.insert(path.clone(), (error_material(), error_id));
                        continue;
                    }
                    Err(error) => return Err(error),
                }
            };
            prepared.insert(path.clone(), (material, id));
        }
        Ok(prepared)
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_asset_material(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        path: &str,
        material_revision: u64,
        asset: &MaterialAsset,
        shader: Option<&GpuSurfaceShader>,
        log: &mut ConsoleLog,
    ) -> Result<(AssetMaterialKey, Material), RenderError> {
        let schema = shader.map_or_else(engine_scene::ShaderSchema::lit, |shader| {
            shader.asset.layout()
        });
        let resolved = asset.resolve(&schema);
        let warning_key = format!("{path}:{material_revision}");
        for warning in &resolved.warnings {
            let key = format!("{warning_key}:{warning}");
            if self.reported_material_warnings.insert(key) {
                log.push(crate::console::LogLevel::Warn, format!("{path}: {warning}"));
            }
        }
        let mut texture_revisions = Vec::new();
        let mut material = resolved_asset_material(&resolved, shader.map(|shader| shader.id));
        for (slot, texture) in resolved.textures.iter().enumerate() {
            let Some((texture_path, color_space)) = texture else {
                continue;
            };
            let render_space = match color_space {
                ShaderColorSpace::Srgb => TextureColorSpace::Srgb,
                ShaderColorSpace::Linear => TextureColorSpace::Linear,
            };
            self.sync_texture(renderer, project, log, texture_path.as_str(), render_space)?;
            let texture_id = self
                .textures
                .get(&(texture_path.as_str().to_owned(), render_space))
                .copied()
                .flatten();
            if schema.builtin_lit {
                if let Some(property) = schema.properties.iter().find(|property| {
                    property.property.kind == ShaderPropertyKind::Texture && property.slot == slot
                }) {
                    set_lit_texture(&mut material, &property.property.name, texture_id);
                }
            } else if let Some(target) = material.user_textures.get_mut(slot) {
                *target = texture_id;
            }
            let revision = self
                .texture_revisions
                .get(&(texture_path.as_str().to_owned(), render_space))
                .copied()
                .unwrap_or(0);
            texture_revisions.push((texture_path.as_str().to_owned(), render_space, revision));
        }
        Ok((
            AssetMaterialKey {
                material_revision,
                shader_generation: shader.map_or(0, |shader| shader.revision),
                texture_revisions,
            },
            material,
        ))
    }

    fn reload_surface_shaders_transactionally(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        library: &MaterialLibrary,
        active_materials: &HashSet<String>,
        preview_shader_path: Option<&str>,
        log: &mut ConsoleLog,
    ) -> Result<(), RenderError> {
        let mut shader_paths = active_materials
            .iter()
            .filter_map(|path| library.material(path)?.effective_asset()?.shader.as_ref())
            .map(|path| path.as_str().to_owned())
            .collect::<HashSet<_>>();
        if let Some(path) = preview_shader_path {
            shader_paths.insert(path.to_owned());
        }

        for path in shader_paths {
            let Some(cached) = library.shader(&path) else {
                continue;
            };
            let Some(asset) = cached.asset.as_ref() else {
                continue;
            };
            if !surface_shader_rebuild_needed(
                self.surface_shaders
                    .get(&path)
                    .map(|current| current.revision),
                &self.failed_surface_shaders,
                &path,
                cached.revision,
            ) {
                continue;
            }
            let source = library
                .shader_source(&path)
                .unwrap_or_else(|| crate::materials::surface_source(asset));
            let shader_id = match renderer.create_surface_shader(&source) {
                Ok(shader) => shader,
                Err(RenderError::InvalidShader(error)) => {
                    self.failed_surface_shaders
                        .insert((path.clone(), cached.revision));
                    self.report_material_error(
                        log,
                        if self.surface_shaders.contains_key(&path) {
                            format!("{path}: {error}; сохранена прежняя версия шейдера")
                        } else {
                            format!("{path}: {error}")
                        },
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
            let candidate = GpuSurfaceShader {
                revision: cached.revision,
                asset: asset.clone(),
                id: shader_id,
            };
            let dependent_paths = materials_using_shader(
                active_materials,
                |material_path| {
                    library
                        .material(material_path)
                        .and_then(|material| material.effective_asset())
                        .and_then(|material| material.shader.as_ref())
                        .map(|shader_path| shader_path.as_str().to_owned())
                },
                &path,
            );
            let mut staged = Vec::with_capacity(dependent_paths.len());
            let mut failure = None;
            for material_path in dependent_paths {
                let Some(material) = library.material(&material_path) else {
                    continue;
                };
                if material.error.is_some() && material.draft.is_none() {
                    continue;
                }
                let Some(asset) = material.effective_asset() else {
                    continue;
                };
                let (key, resolved) = match self.resolve_asset_material(
                    renderer,
                    project,
                    &material_path,
                    material.revision,
                    asset,
                    Some(&candidate),
                    log,
                ) {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        for (_, _, _, id) in staged.drain(..) {
                            renderer.destroy_material(id)?;
                        }
                        renderer.destroy_surface_shader(shader_id)?;
                        return Err(error);
                    }
                };
                match renderer.create_material(&resolved) {
                    Ok(id) => staged.push((material_path, key, resolved, id)),
                    Err(error) if is_resource_error(&error) => {
                        failure = Some((material_path, error));
                        break;
                    }
                    Err(error) => {
                        for (_, _, _, id) in staged.drain(..) {
                            renderer.destroy_material(id)?;
                        }
                        renderer.destroy_surface_shader(shader_id)?;
                        return Err(error);
                    }
                }
            }
            if let Some((failed_path, error)) = failure {
                for (_, _, _, id) in staged.drain(..) {
                    renderer.destroy_material(id)?;
                }
                renderer.destroy_surface_shader(shader_id)?;
                self.failed_surface_shaders
                    .insert((path.clone(), cached.revision));
                self.report_material_error(
                    log,
                    format!(
                        "{failed_path}: {error}; сохранена прежняя версия шейдера и материалов"
                    ),
                );
                continue;
            }

            if let Some(previous) = self.surface_shaders.insert(path.clone(), candidate) {
                self.retired_surface_shaders.push(previous.id);
            }
            self.failed_surface_shaders
                .retain(|(failed_path, _)| failed_path != &path);
            let mut retired_materials = Vec::new();
            for (material_path, key, material, id) in staged {
                if let Some(previous) = self.asset_materials.insert(
                    material_path.clone(),
                    GpuAssetMaterial { key, material, id },
                ) {
                    retired_materials.push(previous.id);
                }
                self.failed_asset_materials
                    .retain(|(failed_path, _)| failed_path != &material_path);
            }
            for material in retired_materials {
                renderer.destroy_material(material)?;
            }
        }
        Ok(())
    }

    fn retire_unused_surface_shaders(
        &mut self,
        renderer: &mut Renderer,
        preview_shader_path: Option<&str>,
    ) -> Result<(), RenderError> {
        let mut referenced = self
            .asset_materials
            .values()
            .filter_map(|material| material.material.shader)
            .collect::<HashSet<_>>();
        if let Some((_, _, material, _)) = &self.preview_shader_material
            && let Some(shader) = material.shader
        {
            referenced.insert(shader);
        }
        let unused = unused_surface_shaders(
            self.surface_shaders
                .iter()
                .map(|(path, shader)| (path.clone(), shader.id)),
            &referenced,
            preview_shader_path,
        );
        for (path, shader) in unused {
            renderer.destroy_surface_shader(shader)?;
            self.surface_shaders.remove(&path);
        }
        let mut keep = Vec::new();
        for shader in self.retired_surface_shaders.drain(..) {
            if referenced.contains(&shader) {
                keep.push(shader);
            } else {
                renderer.destroy_surface_shader(shader)?;
            }
        }
        self.retired_surface_shaders = keep;
        Ok(())
    }

    /// Возвращает offscreen-viewport захвата, создавая его при первом запросе.
    pub fn ensure_capture_viewport(
        &mut self,
        renderer: &mut Renderer,
    ) -> Result<ViewportId, RenderError> {
        if let Some(viewport) = self.capture_viewport {
            return Ok(viewport);
        }
        let viewport = renderer.create_viewport(1, 1)?;
        self.capture_viewport = Some(viewport);
        Ok(viewport)
    }

    /// Освобождает offscreen-viewport после завершения или отмены захвата.
    pub fn release_capture_viewport(&mut self, renderer: &mut Renderer) -> Result<(), RenderError> {
        self.capture_draws.clear();
        if let Some(viewport) = self.capture_viewport.take() {
            renderer.free_viewport(viewport)?;
        }
        Ok(())
    }

    /// Список захвата текущего кадра; пуст, если захвата нет.
    pub fn capture_draws(&self) -> &[DrawCommand] {
        &self.capture_draws
    }

    /// Строит отдельные draw lists для Scene и Game; сетка добавляется только в Scene.
    /// Список захвата строится только при `capture_camera`, без захвата работы нет.
    #[allow(clippy::too_many_arguments)]
    pub fn build_draws(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        materials: &MaterialLibrary,
        log: &mut ConsoleLog,
        preview_asset: Option<&str>,
        world: &World,
        frame: &SceneFrame,
        particle_commands: &[ParticleCommand],
        particle_dt: Option<f32>,
        scene_camera: Camera,
        game_camera: Camera,
        grid_camera_distance: Option<f32>,
        play_bridge: Option<&AnimationBridge>,
        capture_camera: Option<Camera>,
    ) -> Result<(), RenderError> {
        let active_asset_paths = active_material_paths(world, preview_asset);
        let preview_shader_path =
            preview_asset.filter(|path| path.to_ascii_lowercase().ends_with(".rveshader"));
        let asset_materials = self.prepare_asset_materials(
            renderer,
            project,
            materials,
            &active_asset_paths,
            preview_shader_path,
            log,
        )?;
        self.preview_asset_material = preview_asset
            .filter(|path| path.to_ascii_lowercase().ends_with(".rvemat"))
            .and_then(|path| {
                asset_materials
                    .get(path)
                    .map(|(_, material)| (path.to_owned(), *material))
            });
        self.scene_draws.clear();
        self.game_draws.clear();
        self.capture_draws.clear();
        self.sync_particles(renderer, world, frame, particle_commands, particle_dt)?;
        let bridge = play_bridge.or(self.animation.bridge());
        let palette_index = palette_indices(&self.skins, bridge);
        self.lod_palettes.clear();
        let used_object_materials = {
            let mut resources = DrawResources {
                models: &self.models,
                textures: &self.textures,
                asset_materials: &asset_materials,
                error_material: self.error_material,
                object_materials: &mut self.object_materials,
                used_object_materials: HashSet::new(),
                skins: &self.skins,
                palette_index: &palette_index,
                bridge,
                lod_palettes: &mut self.lod_palettes,
                lod_palette_index: HashMap::new(),
            };
            append_world_draws(
                renderer,
                world,
                frame,
                scene_camera,
                &mut self.scene_lod,
                &mut resources,
                &mut self.motion_history,
                &mut self.scene_draws,
            )?;
            append_world_draws(
                renderer,
                world,
                frame,
                game_camera,
                &mut self.game_lod,
                &mut resources,
                &mut self.motion_history,
                &mut self.game_draws,
            )?;
            if let Some(camera) = capture_camera {
                append_world_draws(
                    renderer,
                    world,
                    frame,
                    camera,
                    &mut self.capture_lod,
                    &mut resources,
                    &mut self.motion_history,
                    &mut self.capture_draws,
                )?;
            }
            resources.used_object_materials
        };
        let stale = self
            .object_materials
            .keys()
            .filter(|key| !used_object_materials.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in stale {
            if let Some((_, material)) = self.object_materials.remove(&key) {
                renderer.destroy_material(material)?;
            }
        }
        let stale_assets = self
            .asset_materials
            .keys()
            .filter(|path| !active_asset_paths.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in stale_assets {
            if let Some(material) = self.asset_materials.remove(&path) {
                renderer.destroy_material(material.id)?;
            }
        }
        self.retire_unused_surface_shaders(renderer, preview_shader_path)?;
        if let Some(distance) = grid_camera_distance {
            let grid = match self.grid {
                Some(grid) => grid,
                None => {
                    let (vertices, indices) = grid_mesh();
                    let grid = renderer.create_mesh(&vertices, &indices)?;
                    self.grid = Some(grid);
                    grid
                }
            };
            let grid_material = match self.grid_material {
                Some(material) => material,
                None => {
                    let shader = renderer.create_surface_shader(&grid_surface_source())?;
                    self.grid_shader = Some(shader);
                    let material = renderer.create_material(&Material {
                        shader: Some(shader),
                        double_sided: true,
                        ..Material::default()
                    })?;
                    self.grid_material = Some(material);
                    material
                }
            };
            self.scene_draws.push(DrawCommand {
                mesh: grid,
                material: Some(grid_material),
                model: Mat4::from_scale(Vec3::splat(grid_scale(distance))).to_cols_array_2d(),
                previous_model: None,
                lod_level: 0,
                casts_shadow: false,
                skin: None,
            });
        }
        Ok(())
    }

    /// Освобождает все экземпляры скиннинга; вызывается перед заменой, сбросом моделей и закрытием.
    pub fn release_skins(&mut self, renderer: &mut Renderer) -> Result<(), RenderError> {
        for (_, skin) in self.skins.drain() {
            release_skin(renderer, skin)?;
        }
        Ok(())
    }

    /// Продвигает мост превью редактора, применяет превью выбранного объекта и синхронизирует
    /// скиннинг. Во время Play время ведёт сессия, и этот метод не вызывается.
    pub fn sync_animation(
        &mut self,
        renderer: &mut Renderer,
        assets: &Assets,
        models: &HashMap<String, Arc<Model>>,
        world: &World,
        preview: Option<(ObjectHandle, &mut AnimationPreview)>,
        warnings: &mut Vec<String>,
    ) -> Result<(), RenderError> {
        let dt = self.animation.frame_seconds();
        let epoch = self.model_generation;
        let versions = &self.model_versions;
        self.animation.advance(
            world,
            epoch,
            |path| versions.get(path).copied().unwrap_or(0),
            dt,
            preview,
            warnings,
            |world, clips| {
                project_preview_bridge(
                    world,
                    models,
                    |path| is_failed_model(&self.models, &self.failed_reloads, path),
                    assets,
                    clips,
                )
            },
        );
        self.sync_skins(renderer, world)?;
        self.report_palette_mismatches(None, warnings);
        Ok(())
    }

    /// Предупреждает один раз о каждом объекте, чья палитра моста не совпала с числом костей
    /// меша: такой объект рисуется без скиннинга и не валит кадр. Проверяется `play` (мост Play),
    /// а без него — мост превью. Предупреждение снимается, когда несоответствие исчезает.
    pub fn report_palette_mismatches(
        &mut self,
        play: Option<&AnimationBridge>,
        warnings: &mut Vec<String>,
    ) {
        let mismatched = match play.or(self.animation.bridge()) {
            Some(bridge) => palette_mismatches(&self.skins, bridge),
            None => Vec::new(),
        };
        warnings.extend(mismatch_warnings(
            &mut self.skin_mismatch_reported,
            mismatched,
        ));
    }

    /// Сведения превью объекта для инспектора.
    pub fn preview_status(&self, object: ObjectHandle, motion: Option<&str>) -> PreviewStatus {
        self.animation.status(object, motion)
    }

    /// Приводит экземпляры скиннинга к аниматорам мира: объект с аниматором и скелетной моделью
    /// получает экземпляр на каждую часть меша. Удаление аниматора или объекта, смена модели и
    /// Undo/Redo проходят через это же планирование.
    pub fn sync_skins(
        &mut self,
        renderer: &mut Renderer,
        world: &World,
    ) -> Result<(), RenderError> {
        let mut wanted: HashMap<ObjectHandle, WantedSkin<MeshId>> = HashMap::new();
        let skinned_meshes = |path: &str| -> Option<(String, Vec<MeshId>)> {
            let model = self.models.get(path)?.as_ref()?;
            model.skinned.then(|| {
                let mesh_ids = model.meshes.iter().map(|(id, _, _)| *id).collect();
                (path.to_owned(), mesh_ids)
            })
        };
        for object in world.objects() {
            let (Some(_), Some(mesh)) = (&object.animator, &object.mesh_renderer) else {
                continue;
            };
            let Some((path, mesh_ids)) = skinned_meshes(mesh.model.as_str()) else {
                continue;
            };
            let lod_count = object
                .lod_group
                .as_ref()
                .map_or(0, |group| group.levels.len());
            let lods = (1..=lod_count)
                .map(|level| {
                    lod_model_path(mesh, object.lod_group.as_ref(), level).and_then(skinned_meshes)
                })
                .collect();
            wanted.insert(object.handle, (path, mesh_ids, lods));
        }
        let plan = plan_skin_sync(&self.skins, &wanted);
        for object in plan.release {
            if let Some(skin) = self.skins.remove(&object) {
                release_skin(renderer, skin)?;
            }
        }
        for object in plan.create {
            let Some((model, mesh_ids, lods)) = wanted.remove(&object) else {
                continue;
            };
            let base = self.models.get(&model).and_then(Option::as_ref);
            let joint_count = base.map_or(0, |gpu| gpu.joint_count);
            let lods = lods
                .into_iter()
                .map(|lod| {
                    lod.map(|(path, mesh_ids)| {
                        let remap = base
                            .and_then(|gpu| gpu.skeleton.as_ref())
                            .zip(
                                self.models
                                    .get(&path)
                                    .and_then(Option::as_ref)
                                    .and_then(|gpu| gpu.skeleton.as_ref()),
                            )
                            .and_then(|(base, lod)| PaletteRemap::new(base, lod))
                            .map(Arc::new);
                        (path, mesh_ids, remap)
                    })
                })
                .collect();
            let skin = create_skin(renderer, model, joint_count, mesh_ids, lods)?;
            self.skins.insert(object, skin);
        }
        Ok(())
    }

    /// Палитры кадра в порядке, который использует `SkinBinding::palette`. Мост — активный
    /// мост Play, иначе мост редактора.
    pub fn joint_palette_refs<'b>(
        &'b self,
        play_bridge: Option<&'b AnimationBridge>,
    ) -> Vec<&'b [[[f32; 4]; 4]]> {
        let Some(bridge) = play_bridge.or(self.animation.bridge()) else {
            return Vec::new();
        };
        palette_objects(&self.skins, bridge)
            .into_iter()
            .filter_map(|object| bridge.palette(object))
            .chain(self.lod_palettes.iter().map(Vec::as_slice))
            .collect()
    }

    pub fn begin_motion_frame(&mut self, game_camera: ObjectHandle, playing: bool) {
        self.playing = playing;
        self.motion_history.begin_frame(game_camera, playing);
    }

    pub fn commit_motion_history(&mut self) {
        self.motion_history.commit_frame();
    }

    /// Собирает `Frame` для заданной матрицы камеры; `with_grid` выбирает Scene draw list.
    #[allow(clippy::too_many_arguments)]
    pub fn frame<'a>(
        &'a self,
        camera: Camera,
        width: u32,
        height: u32,
        settings: CameraSettings,
        post: &PostProcessSettings,
        screens: &[ScreenShaderRef],
        light: &LightState,
        local_lights: &'a [LocalLight],
        local_light_shadows: &'a [LocalLightShadow],
        scene_has_light: bool,
        environment_state: Option<&EnvironmentState>,
        shadow_cascades: &'a [[[f32; 4]; 4]],
        joint_palettes: &'a [&'a [[[f32; 4]; 4]]],
        with_grid: bool,
        debug_view: DebugView,
    ) -> Frame<'a> {
        let light = frame_light(light, scene_has_light);
        let environment = self.frame_environment(environment_state);
        Frame {
            joint_palettes,
            clear_color: [
                settings.render.clear_color[0],
                settings.render.clear_color[1],
                settings.render.clear_color[2],
                1.0,
            ],
            camera: FrameCamera {
                view: camera.view().to_cols_array_2d(),
                projection: camera.projection(width, height).to_cols_array_2d(),
                position: camera.position.to_array(),
            },
            environment,
            shadow_cascades,
            light,
            lights: local_lights,
            local_light_shadows,
            draws: if with_grid {
                &self.scene_draws
            } else {
                &self.game_draws
            },
            overlay: &[],
            ui: &[],
            debug_view,
            reset_history: self.motion_history.reset_pending,
            // Симулирует вид, который показывает игру: Scene в редактировании, Game во время Play.
            particles: &self.particle_frames,
            particle_step: if with_grid != self.playing {
                self.particle_step
            } else {
                0.0
            },
            view: ViewSettings {
                render_scale: settings.render.render_scale,
                post: render_post_process(post, self.color_lut(post), with_grid),
                screen_shaders: self.screen_shader_view(screens, with_grid),
                anti_aliasing: match settings.render.anti_aliasing {
                    AntiAliasing::Fxaa => RenderAntiAliasing::Fxaa,
                    AntiAliasing::None => RenderAntiAliasing::None,
                    AntiAliasing::Taa => RenderAntiAliasing::Taa,
                    AntiAliasing::Fsr => RenderAntiAliasing::Fsr,
                    AntiAliasing::Dlss => RenderAntiAliasing::Dlss,
                },
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn material_preview_frame(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        library: &MaterialLibrary,
        asset_path: Option<&str>,
        primitive: MaterialPreviewPrimitive,
        yaw: f32,
        pitch: f32,
        zoom: f32,
        log: &mut ConsoleLog,
    ) -> Result<MaterialPreviewDraw, RenderError> {
        let mesh = if let Some(mesh) = self.preview_meshes.get(&primitive) {
            *mesh
        } else {
            let (vertices, indices) = crate::viewport::primitives::mesh(primitive);
            let mesh = renderer.create_mesh(&vertices, &indices)?;
            self.preview_meshes.insert(primitive, mesh);
            mesh
        };
        let error_id = self.ensure_error_material(renderer)?;
        let material = if let Some(path) = asset_path {
            if path.to_ascii_lowercase().ends_with(".rvemat") {
                self.preview_asset_material
                    .as_ref()
                    .filter(|(cached_path, _)| cached_path == path)
                    .map_or(error_id, |(_, material)| *material)
            } else if path.to_ascii_lowercase().ends_with(".rveshader") {
                self.preview_shader_material_id(renderer, project, library, path, log)?
                    .unwrap_or(error_id)
            } else {
                error_id
            }
        } else if let Some(material) = self.preview_default_material {
            material
        } else {
            let material = renderer.create_material(&Material::default())?;
            self.preview_default_material = Some(material);
            material
        };
        let draw = DrawCommand {
            mesh,
            material: Some(material),
            model: Mat4::IDENTITY.to_cols_array_2d(),
            previous_model: None,
            lod_level: 0,
            casts_shadow: false,
            skin: None,
        };
        let horizontal = pitch.cos() * zoom;
        let position = Vec3::new(
            yaw.sin() * horizontal,
            pitch.sin() * zoom,
            yaw.cos() * horizontal,
        );
        let camera = Camera {
            position,
            target: Vec3::ZERO,
            up: Vec3::Y,
            fov_narrow: std::f32::consts::FRAC_PI_4,
            near: 0.05,
            far: 100.0,
        };
        let environment = self.ensure_preview_environment(renderer)?;
        Ok(MaterialPreviewDraw {
            draw,
            camera,
            environment: FrameEnvironment {
                id: environment,
                intensity: 0.7,
                rotation: Mat3::IDENTITY.to_cols_array_2d(),
                skybox: false,
            },
        })
    }

    fn ensure_preview_environment(
        &mut self,
        renderer: &mut Renderer,
    ) -> Result<EnvironmentId, RenderError> {
        if let Some(environment) = self.preview_environment {
            return Ok(environment);
        }
        let environment =
            renderer.create_environment(&EnvironmentSource::Procedural(ProceduralSky {
                zenith_color: [0.25, 0.35, 0.55],
                horizon_color: [0.7, 0.72, 0.75],
                ground_color: [0.14, 0.13, 0.12],
                sun_size_deg: 2.0,
                sun_intensity: 1.0,
                sun_direction: [0.4, 0.8, -0.4],
            }))?;
        self.preview_environment = Some(environment);
        Ok(environment)
    }

    fn preview_shader_material_id(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        library: &MaterialLibrary,
        path: &str,
        log: &mut ConsoleLog,
    ) -> Result<Option<MaterialId>, RenderError> {
        let Some(shader) = self.ensure_surface_shader(renderer, path, library, log)? else {
            return Ok(None);
        };
        let schema = shader.asset.layout();
        let mut defaults = MaterialAsset::default();
        for layout in &schema.properties {
            if let Some(value) = crate::materials::material_to_toml_value(&layout.property) {
                defaults
                    .properties
                    .insert(layout.property.name.clone(), value);
            }
        }
        let resolved = defaults.resolve(&schema);
        let mut material = resolved_asset_material(&resolved, Some(shader.id));
        let mut texture_revisions = Vec::new();
        for (slot, texture) in resolved.textures.iter().enumerate() {
            let Some((texture_path, color_space)) = texture else {
                continue;
            };
            let render_space = match color_space {
                ShaderColorSpace::Srgb => TextureColorSpace::Srgb,
                ShaderColorSpace::Linear => TextureColorSpace::Linear,
            };
            self.sync_texture(renderer, project, log, texture_path.as_str(), render_space)?;
            let texture_id = self
                .textures
                .get(&(texture_path.as_str().to_owned(), render_space))
                .copied()
                .flatten();
            if let Some(target) = material.user_textures.get_mut(slot) {
                *target = texture_id;
            }
            let revision = self
                .texture_revisions
                .get(&(texture_path.as_str().to_owned(), render_space))
                .copied()
                .unwrap_or(0);
            texture_revisions.push((texture_path.as_str().to_owned(), render_space, revision));
        }
        let key = AssetMaterialKey {
            material_revision: 0,
            shader_generation: shader.revision,
            texture_revisions,
        };
        if let Some((current_path, current_key, _, id)) = &self.preview_shader_material
            && current_path == path
            && current_key == &key
        {
            return Ok(Some(*id));
        }
        match renderer.create_material(&material) {
            Ok(id) => {
                let previous =
                    self.preview_shader_material
                        .replace((path.to_owned(), key, material, id));
                if let Some((_, _, _, previous)) = previous {
                    renderer.destroy_material(previous)?;
                }
                Ok(Some(id))
            }
            Err(error) if is_resource_error(&error) => {
                self.report_material_error(
                    log,
                    format!("{path}: {error}; используется материал ошибки"),
                );
                Ok(self
                    .preview_shader_material
                    .as_ref()
                    .filter(|(current_path, _, _, _)| current_path == path)
                    .map(|(_, _, _, id)| *id))
            }
            Err(error) => Err(error),
        }
    }

    /// Создаёт, обновляет и удаляет GPU-эмиттеры по компонентам сцены.
    ///
    /// Шаг симуляции — wall-clock время между кадрами. Симулирует вид, который показывает
    /// игру: Scene в режиме редактирования и Game во время Play. Переключение Play перезапускает эмиттеры.
    fn sync_particles(
        &mut self,
        renderer: &mut Renderer,
        world: &World,
        frame: &SceneFrame,
        commands: &[ParticleCommand],
        particle_dt: Option<f32>,
    ) -> Result<(), RenderError> {
        // Старые ссылки на эмиттеры могут уйти на замену; при ошибке кадр не должен их использовать.
        self.particle_frames.clear();
        let now = Instant::now();
        self.particle_step = particle_dt.map_or_else(
            || {
                self.particle_clock
                    .map_or(0.0, |previous| (now - previous).as_secs_f32())
            },
            |dt| if dt.is_finite() { dt.max(0.0) } else { 0.0 },
        );
        self.particle_clock = Some(now);
        if self.particles_playing != Some(self.playing) {
            for entry in self.particle_emitters.values() {
                renderer.restart_particle_emitter(entry.id)?;
            }
            self.particles_playing = Some(self.playing);
        }
        let mut frames = Vec::with_capacity(frame.particle_emitters.len());
        let mut live = HashSet::new();
        for instance in &frame.particle_emitters {
            let Some(component) = world.particle_emitter(instance.object) else {
                continue;
            };
            live.insert(instance.object);
            let texture_path = component
                .texture
                .as_ref()
                .map(|path| path.as_str().to_owned());
            let texture = texture_path.as_ref().and_then(|path| {
                self.textures
                    .get(&(path.clone(), TextureColorSpace::Srgb))
                    .copied()
                    .flatten()
            });
            let stale = self
                .particle_emitters
                .get(&instance.object)
                .is_none_or(|entry| {
                    entry.component != *component
                        || entry.texture != texture_path
                        || entry.texture_id != texture
                });
            if stale {
                // Новый эмиттер создаётся до уничтожения старого: при ошибке старый остаётся рабочим.
                let previous = replace_entry(
                    &mut self.particle_emitters,
                    instance.object,
                    || -> Result<EditorParticleEmitter, RenderError> {
                        let id = renderer
                            .create_particle_emitter(editor_particle_desc(component, texture))?;
                        Ok(EditorParticleEmitter {
                            id,
                            component: component.clone(),
                            texture: texture_path,
                            texture_id: texture,
                        })
                    },
                )?;
                if let Some(old) = previous {
                    renderer.destroy_particle_emitter(old.id)?;
                }
            }
            if let Some(entry) = self.particle_emitters.get(&instance.object) {
                frames.push(ParticleEmitterFrame {
                    emitter: entry.id,
                    transform: instance.model.to_cols_array_2d(),
                    emitting: instance.emitting,
                });
            }
        }
        let removed: Vec<ObjectHandle> = self
            .particle_emitters
            .keys()
            .filter(|handle| !live.contains(handle))
            .copied()
            .collect();
        for handle in removed {
            if let Some(old) = self.particle_emitters.remove(&handle) {
                renderer.destroy_particle_emitter(old.id)?;
            }
        }
        for command in commands {
            let (object, burst) = match *command {
                ParticleCommand::Restart { object } => (object, None),
                ParticleCommand::Burst { object, count } => (object, Some(count)),
            };
            let Some(entry) = self.particle_emitters.get(&object) else {
                continue;
            };
            match burst {
                None => renderer.restart_particle_emitter(entry.id)?,
                Some(count) => renderer.emit_particle_burst(entry.id, count)?,
            }
        }
        self.particle_frames = frames;
        Ok(())
    }

    pub fn sync_environment(
        &mut self,
        renderer: &mut Renderer,
        state: Option<&EnvironmentState>,
        log: &mut ConsoleLog,
    ) -> Result<(), RenderError> {
        let Some(state) = state else {
            if let Some((_, environment)) = self.current_environment.take() {
                renderer.destroy_environment(environment)?;
            }
            self.failed_environment = None;
            return Ok(());
        };
        let key = editor_environment_key(state);
        if self
            .current_environment
            .as_ref()
            .is_some_and(|(current, _)| current == &key)
        {
            return Ok(());
        }
        if self.failed_environment.as_ref() == Some(&key) {
            return Ok(());
        }
        if let Some(path) = &state.component.hdr
            && !self.environment_sources.contains_key(path.as_str())
        {
            // HDR ещё декодируется в фоне: остаётся прежнее окружение или окружение по умолчанию.
            return Ok(());
        }
        let source = if let Some(path) = &state.component.hdr {
            self.environment_sources
                .get(path.as_str())
                .and_then(Option::as_ref)
                .map(|source| renderer.create_environment(source))
        } else {
            editor_procedural_sky(state)
                .map(|sky| renderer.create_environment(&EnvironmentSource::Procedural(sky)))
        };
        match source {
            Some(Ok(environment)) => {
                let transition = environment_transition(
                    self.current_environment
                        .as_ref()
                        .map(|(current, _)| current),
                    &key,
                    EnvironmentCreation::Success,
                );
                debug_assert_eq!(transition, EnvironmentTransition::Replace);
                let previous = self.current_environment.replace((key, environment));
                self.failed_environment = None;
                if let Some((_, previous)) = previous {
                    renderer.destroy_environment(previous)?;
                }
            }
            Some(Err(error)) if is_resource_error(&error) => {
                let transition = environment_transition(
                    self.current_environment
                        .as_ref()
                        .map(|(current, _)| current),
                    &key,
                    EnvironmentCreation::ResourceError,
                );
                let path = state
                    .component
                    .hdr
                    .as_ref()
                    .map_or("environment", VirtualPath::as_str);
                if transition == EnvironmentTransition::Keep {
                    log.error(format!("{path}: {error}; сохранено прежнее окружение"));
                } else {
                    log.error(format!("{path}: {error}"));
                    if let Some((_, previous)) = self.current_environment.take() {
                        renderer.destroy_environment(previous)?;
                    }
                }
                self.failed_environment = Some(key);
            }
            Some(Err(error)) => return Err(error),
            None => {
                let transition = environment_transition(
                    self.current_environment
                        .as_ref()
                        .map(|(current, _)| current),
                    &key,
                    EnvironmentCreation::MissingSource,
                );
                let path = state
                    .component
                    .hdr
                    .as_ref()
                    .map_or("environment", VirtualPath::as_str);
                if transition == EnvironmentTransition::Keep {
                    log.error(format!(
                        "{path}: источник окружения недоступен; сохранено прежнее окружение"
                    ));
                } else {
                    log.error(format!("{path}: источник окружения недоступен"));
                    if let Some((_, previous)) = self.current_environment.take() {
                        renderer.destroy_environment(previous)?;
                    }
                }
                self.failed_environment = Some(key);
            }
        }
        Ok(())
    }

    fn frame_environment(&self, state: Option<&EnvironmentState>) -> Option<FrameEnvironment> {
        let state = state?;
        let key = editor_environment_key(state);
        let (_, id) = self
            .current_environment
            .as_ref()
            .filter(|(current, _)| current == &key)?;
        Some(FrameEnvironment {
            id: *id,
            intensity: state.component.intensity,
            rotation: state.rotation,
            skybox: state.component.skybox,
        })
    }

    pub fn shadow_cascades(
        camera: Camera,
        width: u32,
        height: u32,
        light: &LightState,
        scene_has_light: bool,
    ) -> Vec<[[f32; 4]; 4]> {
        if !scene_has_light {
            return Vec::new();
        }
        let Some(settings) = light.shadow else {
            return Vec::new();
        };
        cascade_view_projections(
            camera,
            width.max(1) as f32 / height.max(1) as f32,
            Vec3::from_array(light.direction),
            settings,
            SHADOW_MAP_RESOLUTION,
        )
        .into_iter()
        .map(|matrix| matrix.to_cols_array_2d())
        .collect()
    }
}

fn lod_model_path<'a>(
    mesh: &'a MeshRenderer,
    group: Option<&'a LodGroup>,
    level: usize,
) -> Option<&'a str> {
    if level == 0 {
        return Some(mesh.model.as_str());
    }
    let model = group?.levels.get(level - 1)?.model.as_str();
    Some(model.strip_prefix('/').unwrap_or(model))
}

fn lod_texture_path<'a>(
    mesh: &'a MeshRenderer,
    group: Option<&'a LodGroup>,
    level: usize,
) -> Option<&'a str> {
    let texture = if level == 0 {
        mesh.texture.as_ref().map(|path| path.as_str())
    } else {
        group?
            .levels
            .get(level - 1)?
            .texture
            .as_deref()
            .or_else(|| mesh.texture.as_ref().map(|path| path.as_str()))
    }?;
    Some(texture.strip_prefix('/').unwrap_or(texture))
}

struct DrawResources<'a> {
    models: &'a HashMap<String, Option<GpuModel>>,
    textures: &'a HashMap<(String, TextureColorSpace), Option<TextureId>>,
    asset_materials: &'a HashMap<String, (Material, MaterialId)>,
    error_material: Option<MaterialId>,
    object_materials: &'a mut HashMap<(ObjectHandle, String, usize, usize), (Material, MaterialId)>,
    used_object_materials: HashSet<(ObjectHandle, String, usize, usize)>,
    /// Экземпляры скиннинга и индексы палитр кадра по объекту.
    skins: &'a HashMap<ObjectHandle, SkinnedObject>,
    palette_index: &'a HashMap<ObjectHandle, u32>,
    /// Мост кадра и палитры уровней LOD, переведённые из его палитр по запросу видов.
    bridge: Option<&'a AnimationBridge>,
    lod_palettes: &'a mut Vec<Vec<[[f32; 4]; 4]>>,
    lod_palette_index: HashMap<(ObjectHandle, usize), u32>,
}

impl DrawResources<'_> {
    /// Привязка скиннинга части `mesh_index` уровня `level`. Палитра уровня LOD строится один
    /// раз за кадр и идёт после палитр моста. Уровень без палитры моста той же длины, что скелет
    /// объекта, или без перевода костей рисуется без скиннинга.
    fn skin_binding(
        &mut self,
        object: ObjectHandle,
        level: usize,
        mesh_index: usize,
    ) -> Option<SkinBinding> {
        let skin = self.skins.get(&object)?;
        let base_index = *self.palette_index.get(&object)?;
        if level == 0 {
            return Some(SkinBinding {
                instance: *skin.instances.get(mesh_index)?,
                palette: base_index,
            });
        }
        let lod = skin.lods.get(level - 1)?.as_ref()?;
        let instance = *lod.instances.get(mesh_index)?;
        let palette = match self.lod_palette_index.get(&(object, level)) {
            Some(index) => *index,
            None => {
                let base = self.bridge?.palette(object)?;
                let mut out = Vec::new();
                if !lod.remap.as_ref()?.apply(base, &mut out) {
                    return None;
                }
                self.lod_palettes.push(out);
                let index = (self.palette_index.len() + self.lod_palettes.len() - 1) as u32;
                self.lod_palette_index.insert((object, level), index);
                index
            }
        };
        Some(SkinBinding { instance, palette })
    }
}

// Both viewports can reference the same model with different LOD textures in one frame.
fn object_material_key(
    object: ObjectHandle,
    model_path: &str,
    level: usize,
    mesh_index: usize,
) -> (ObjectHandle, String, usize, usize) {
    (object, model_path.to_owned(), level, mesh_index)
}

#[allow(clippy::too_many_arguments)]
fn append_world_draws(
    renderer: &mut Renderer,
    world: &World,
    frame: &SceneFrame,
    camera: Camera,
    lod: &mut LodSelector,
    resources: &mut DrawResources<'_>,
    motion_history: &mut MotionHistory,
    draws: &mut Vec<DrawCommand>,
) -> Result<(), RenderError> {
    for (instance, level) in visible_mesh_levels(world, frame, camera, lod) {
        let Some(object) = world.object(instance.object) else {
            continue;
        };
        let Some(mesh) = &object.mesh_renderer else {
            continue;
        };
        let Some(model_path) = lod_model_path(mesh, object.lod_group.as_ref(), level) else {
            continue;
        };
        let Some(Some(model)) = resources.models.get(model_path) else {
            continue;
        };
        let replacement =
            lod_texture_path(mesh, object.lod_group.as_ref(), level).and_then(|path| {
                resources
                    .textures
                    .get(&(path.to_owned(), TextureColorSpace::Srgb))
                    .copied()
                    .flatten()
            });
        for (mesh_index, (mesh_id, base_material, base_material_id)) in
            model.meshes.iter().enumerate()
        {
            // Путь из set_material заменяет слоты всех частей, как в runtime.
            let slot = world
                .mesh_material_path(instance.object)
                .or_else(|| mesh.materials.get(mesh_index).and_then(Option::as_ref));
            let assigned = slot.and_then(|path| resources.asset_materials.get(path.as_str()));
            let (base_material, base_material_id) = assigned
                .map_or((*base_material, *base_material_id), |(material, id)| {
                    (*material, *id)
                });
            let custom_shader = base_material.shader.is_some()
                || resources.error_material == Some(base_material_id);
            let override_material = mesh.material.as_ref();
            let mr_texture = override_material
                .and_then(|material| material.metallic_roughness_texture.as_ref())
                .and_then(|path| {
                    resources
                        .textures
                        .get(&(path.as_str().to_owned(), TextureColorSpace::Linear))
                        .copied()
                        .flatten()
                });
            let normal_texture = override_material
                .and_then(|material| material.normal_texture.as_ref())
                .and_then(|path| {
                    resources
                        .textures
                        .get(&(path.as_str().to_owned(), TextureColorSpace::Linear))
                        .copied()
                        .flatten()
                });
            let emissive_texture = override_material
                .and_then(|material| material.emissive_texture.as_ref())
                .and_then(|path| {
                    resources
                        .textures
                        .get(&(path.as_str().to_owned(), TextureColorSpace::Srgb))
                        .copied()
                        .flatten()
                });
            let occlusion_texture = override_material
                .and_then(|material| material.occlusion_texture.as_ref())
                .and_then(|path| {
                    resources
                        .textures
                        .get(&(path.as_str().to_owned(), TextureColorSpace::Linear))
                        .copied()
                        .flatten()
                });
            let resolved = if custom_shader {
                base_material
            } else {
                resolve_material(
                    base_material,
                    replacement,
                    override_material,
                    mr_texture,
                    normal_texture,
                    emissive_texture,
                    occlusion_texture,
                )
            };
            let material_id = if custom_shader || resolved == base_material {
                base_material_id
            } else {
                let key = object_material_key(instance.object, model_path, level, mesh_index);
                resources.used_object_materials.insert(key.clone());
                match resources.object_materials.get(&key) {
                    Some((cached, id)) if *cached == resolved => *id,
                    previous => {
                        let id = renderer.create_material(&resolved)?;
                        if let Some((_, old_id)) = previous {
                            renderer.destroy_material(*old_id)?;
                        }
                        resources.object_materials.insert(key, (resolved, id));
                        id
                    }
                }
            };
            let skin = resources.skin_binding(instance.object, level, mesh_index);
            draws.push(DrawCommand {
                mesh: *mesh_id,
                material: Some(material_id),
                model: instance.model.to_cols_array_2d(),
                previous_model: motion_history.previous_model(instance.object),
                lod_level: u8::try_from(level).unwrap_or(u8::MAX),
                casts_shadow: true,
                skin,
            });
            motion_history.record_current(instance.object, instance.model.to_cols_array_2d());
        }
    }
    lod.finish_frame();
    Ok(())
}

/// Мешевые экземпляры кадра с уровнем LOD для камеры одного вида. Объект, отсечённый
/// `cull_distance` этого вида, пропускается; список кадра при этом не фильтруется по камерам.
fn visible_mesh_levels<'a>(
    world: &'a World,
    frame: &'a SceneFrame,
    camera: Camera,
    lod: &'a mut LodSelector,
) -> impl Iterator<Item = (&'a MeshInstance, usize)> + 'a {
    frame.meshes.iter().filter_map(move |instance| {
        let level = lod.select(world, instance.object, camera.position, camera.fov_narrow)?;
        Some((instance, level))
    })
}

/// Объекты, у которых есть экземпляры скиннинга и палитра моста той же длины, что число
/// костей меша, по порядку handle. Объект с другой длиной палитры рисуется статично.
fn palette_objects<M, S>(
    skins: &HashMap<ObjectHandle, SkinnedObject<M, S>>,
    bridge: &AnimationBridge,
) -> Vec<ObjectHandle> {
    let mut objects: Vec<ObjectHandle> = skins
        .iter()
        .filter(|(object, skin)| {
            bridge
                .palette(**object)
                .is_some_and(|palette| palette.len() == skin.joint_count as usize)
        })
        .map(|(object, _)| *object)
        .collect();
    objects.sort_by_key(|object| object.index());
    objects
}

/// Объекты с палитрой моста, чья длина не совпадает с числом костей меша, и их модели.
fn palette_mismatches<M, S>(
    skins: &HashMap<ObjectHandle, SkinnedObject<M, S>>,
    bridge: &AnimationBridge,
) -> Vec<(ObjectHandle, String)> {
    let mut mismatched: Vec<(ObjectHandle, String)> = skins
        .iter()
        .filter(|(object, skin)| {
            bridge
                .palette(**object)
                .is_some_and(|palette| palette.len() != skin.joint_count as usize)
        })
        .map(|(object, skin)| (*object, skin.model.clone()))
        .collect();
    mismatched.sort_by_key(|(object, _)| object.index());
    mismatched
}

/// Предупреждения о несовпадении палитры, которые ещё не выводились. Объект, у которого
/// несовпадение исчезло, снимается из `reported`: повторное несовпадение даст предупреждение снова.
fn mismatch_warnings(
    reported: &mut HashSet<ObjectHandle>,
    mismatched: Vec<(ObjectHandle, String)>,
) -> Vec<String> {
    reported.retain(|object| mismatched.iter().any(|(mismatch, _)| mismatch == object));
    let mut warnings = Vec::new();
    for (object, model) in mismatched {
        if reported.insert(object) {
            warnings.push(format!(
                "{model}: число костей палитры не совпадает с мешем; объект рисуется без скиннинга"
            ));
        }
    }
    warnings
}

/// Индекс палитры каждого объекта: тот же порядок, что у `joint_palette_refs`.
fn palette_indices<M, S>(
    skins: &HashMap<ObjectHandle, SkinnedObject<M, S>>,
    bridge: Option<&AnimationBridge>,
) -> HashMap<ObjectHandle, u32> {
    bridge.map_or_else(HashMap::new, |bridge| {
        palette_objects(skins, bridge)
            .into_iter()
            .enumerate()
            .map(|(index, object)| (object, index as u32))
            .collect()
    })
}

/// Создаёт по экземпляру скиннинга на каждую часть модели и каждого уровня LOD с переводом
/// палитры; при ошибке уже созданные освобождаются.
fn create_skin(
    renderer: &mut Renderer,
    model: String,
    joint_count: u32,
    mesh_ids: Vec<MeshId>,
    lods: Vec<Option<NewLodSkin>>,
) -> Result<SkinnedObject, RenderError> {
    let mut skin = SkinnedObject {
        model,
        joint_count,
        mesh_ids,
        instances: Vec::new(),
        lods: Vec::with_capacity(lods.len()),
    };
    let created = (|| {
        for index in 0..skin.mesh_ids.len() {
            let instance = renderer.create_skin_instance(skin.mesh_ids[index])?;
            skin.instances.push(instance);
        }
        for lod in lods {
            let Some((model, mesh_ids, remap)) = lod else {
                skin.lods.push(None);
                continue;
            };
            let mut level = LodSkin {
                model,
                mesh_ids,
                instances: Vec::new(),
                remap,
            };
            if level.remap.is_some() {
                for index in 0..level.mesh_ids.len() {
                    match renderer.create_skin_instance(level.mesh_ids[index]) {
                        Ok(instance) => level.instances.push(instance),
                        Err(error) => {
                            skin.lods.push(Some(level));
                            return Err(error);
                        }
                    }
                }
            }
            skin.lods.push(Some(level));
        }
        Ok(())
    })();
    match created {
        Ok(()) => Ok(skin),
        Err(error) => {
            release_skin(renderer, skin)?;
            Err(error)
        }
    }
}

fn release_skin(renderer: &mut Renderer, skin: SkinnedObject) -> Result<(), RenderError> {
    let lod_instances = skin
        .lods
        .into_iter()
        .flatten()
        .flat_map(|level| level.instances);
    for instance in skin.instances.into_iter().chain(lod_instances) {
        renderer.destroy_skin_instance(instance)?;
    }
    Ok(())
}

/// Что сделать с экземплярами скиннинга, чтобы они соответствовали аниматорам мира.
#[derive(Debug, Default, PartialEq)]
struct SkinSyncPlan {
    /// Устаревшие экземпляры: аниматор или объект удалены, модель или набор мешей изменились.
    release: Vec<ObjectHandle>,
    /// Объекты, которым нужны новые экземпляры; сюда входят освобождённые.
    create: Vec<ObjectHandle>,
}

/// Сравнивает экземпляры с желаемым состоянием. `current` — модель и меши уже созданных
/// экземпляров, `wanted` — то же для объектов, которым нужен скиннинг.
fn plan_skin_sync<M: PartialEq, S>(
    current: &HashMap<ObjectHandle, SkinnedObject<M, S>>,
    wanted: &HashMap<ObjectHandle, WantedSkin<M>>,
) -> SkinSyncPlan {
    let mut release: Vec<ObjectHandle> = current
        .iter()
        .filter(|(object, skin)| {
            wanted.get(*object).is_none_or(|(model, mesh_ids, lods)| {
                *model != skin.model
                    || *mesh_ids != skin.mesh_ids
                    || lods.len() != skin.lods.len()
                    || lods.iter().zip(&skin.lods).any(|(wanted, current)| {
                        match (wanted, current) {
                            (None, None) => false,
                            (Some((model, mesh_ids)), Some(current)) => {
                                *model != current.model || *mesh_ids != current.mesh_ids
                            }
                            _ => true,
                        }
                    })
            })
        })
        .map(|(object, _)| *object)
        .collect();
    release.sort_by_key(|object| object.index());
    let mut create: Vec<ObjectHandle> = wanted
        .keys()
        .copied()
        .filter(|object| !current.contains_key(object) || release.contains(object))
        .collect();
    create.sort_by_key(|object| object.index());
    SkinSyncPlan { release, create }
}

/// Модель не загрузилась: в кэше есть пустая запись или загрузка зафиксирована как ошибочная.
fn is_failed_model(
    models: &HashMap<String, Option<GpuModel>>,
    failed_reloads: &HashSet<String>,
    path: &str,
) -> bool {
    models.get(path).is_some_and(Option::is_none) || failed_reloads.contains(path)
}

fn destroy_model(renderer: &mut Renderer, model: GpuModel) -> Result<(), RenderError> {
    for (mesh, _, material) in model.meshes {
        renderer.destroy_mesh(mesh)?;
        renderer.destroy_material(material)?;
    }
    for texture in model.textures {
        renderer.destroy_texture(texture)?;
    }
    Ok(())
}

fn cache_update<T>(
    previous: Option<T>,
    result: Result<T, String>,
) -> (Option<T>, Option<T>, Option<String>) {
    let had_previous = previous.is_some();
    match result {
        Ok(current) => (Some(current), previous, None),
        Err(error) if had_previous => (previous, None, Some(error)),
        Err(error) => (None, None, Some(error)),
    }
}

fn is_resource_error(error: &RenderError) -> bool {
    matches!(
        error,
        RenderError::InvalidTexture(_)
            | RenderError::InvalidMesh(_)
            | RenderError::InvalidMaterial(_)
            | RenderError::InvalidEnvironment(_)
    )
}

struct ModelReloadPlan {
    cached: HashSet<String>,
    pending: HashSet<String>,
}

fn model_reload_plan<T>(
    changes: &AssetChanges,
    models: &HashMap<String, Option<T>>,
    dependencies: &HashMap<String, Vec<VirtualPath>>,
    failed_reloads: &HashSet<String>,
    pending: &[String],
) -> ModelReloadPlan {
    let unknown_dependencies_changed = changes.rescan
        || changes.png_paths().next().is_some()
        || !changes.directory_paths.is_empty();
    let cached = models
        .iter()
        .filter_map(|(path, model)| {
            let dependency_changed = dependencies
                .iter()
                .find(|(candidate, _)| same_virtual_path(candidate, path))
                .is_some_and(|(_, paths)| {
                    paths
                        .iter()
                        .any(|path| key_matches_changes(changes, path.as_str()))
                });
            (changes.rescan
                || key_matches_model(changes, path)
                || dependency_changed
                || ((model.is_none() || failed_reloads.contains(path))
                    && unknown_dependencies_changed))
                .then(|| path.clone())
        })
        .collect();
    let pending = pending
        .iter()
        .filter(|path| unknown_dependencies_changed || key_matches_model(changes, path))
        .cloned()
        .collect();
    ModelReloadPlan { cached, pending }
}

/// Экранный шейдер камеры в кэше редактора.
#[derive(Clone, Copy, Debug)]
struct EditorScreenShader {
    id: Option<ScreenShaderId>,
}

/// LUT постобработки из кэша; ключи — канонические пути без ведущего `/`, поэтому поиск в кадре
/// не выделяет память. LUT, которая не загрузилась, даёт `None`.
fn cached_color_lut(
    luts: &HashMap<String, Option<ColorLutId>>,
    post: &PostProcessSettings,
) -> Option<ColorLutId> {
    let path = post.color_grading.lut.as_deref()?;
    luts.get(path.strip_prefix('/').unwrap_or(path))
        .copied()
        .flatten()
}

/// Читает и разбирает `.cube` из проекта и создаёт LUT renderer.
fn load_color_lut(
    renderer: &mut Renderer,
    project: &OpenProject,
    path: &str,
) -> Result<ColorLutId, String> {
    let file = project
        .assets_dir
        .resolve(path)
        .map_err(|error| format!("/{path}: {error}"))?;
    let source = std::fs::read_to_string(&file).map_err(|error| format!("/{path}: {error}"))?;
    let lut = CubeLut::parse(format!("/{path}"), &source).map_err(|error| error.to_string())?;
    renderer
        .create_color_lut(lut.size(), lut.texels())
        .map_err(|error| format!("/{path}: {error}"))
}

/// Читает `.glsl` из проекта и создаёт шейдер renderer; текст ошибки годится для инспектора.
fn load_screen_shader(
    renderer: &mut Renderer,
    project: &OpenProject,
    path: &str,
) -> Result<ScreenShaderId, String> {
    let file = project
        .assets_dir
        .resolve(path)
        .map_err(|error| format!("/{path}: {error}"))?;
    let source = std::fs::read_to_string(&file).map_err(|error| format!("/{path}: {error}"))?;
    renderer
        .create_screen_shader(&source)
        .map_err(|error| format!("/{path}: {error}"))
}

fn key_matches_changes(changes: &AssetChanges, key: &str) -> bool {
    if changes.rescan {
        return true;
    }
    let Ok(key) = VirtualPath::parse(key) else {
        return false;
    };
    changes
        .paths
        .iter()
        .any(|changed| path_is_parent(changed, &key))
}

fn key_matches_model(changes: &AssetChanges, key: &str) -> bool {
    if changes.rescan {
        return true;
    }
    changes
        .fbx_paths()
        .any(|changed| same_virtual_path(changed.as_str(), key))
        || changes
            .directory_paths
            .iter()
            .any(|changed| VirtualPath::parse(key).is_ok_and(|key| path_is_parent(changed, &key)))
}

fn path_is_parent(changed: &VirtualPath, key: &VirtualPath) -> bool {
    key.as_str() == changed.as_str()
        || key
            .as_str()
            .strip_prefix(changed.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn same_virtual_path(left: &str, right: &str) -> bool {
    matches!(
        (VirtualPath::parse(left), VirtualPath::parse(right)),
        (Ok(left), Ok(right)) if left == right
    )
}

fn upload_model(renderer: &mut Renderer, model: &Model) -> Result<GpuModel, RenderError> {
    let mut textures_by_role = HashMap::new();
    let mut textures = Vec::new();
    let mut meshes = Vec::with_capacity(model.meshes.len());
    let mut materials = Vec::with_capacity(model.meshes.len());
    let upload = (|| {
        for model_mesh in &model.meshes {
            let base = &model_mesh.material;
            let material = Material {
                base_color: base.base_color,
                metallic: base.metallic,
                roughness: base.roughness,
                emissive: base.emissive,
                opacity: base.opacity,
                alpha_mode: match base.alpha_mode {
                    ModelAlphaMode::Opaque => AlphaMode::Opaque,
                    ModelAlphaMode::Blend => AlphaMode::Blend,
                },
                base_color_texture: model_texture(
                    renderer,
                    model,
                    base.base_color_texture,
                    TextureColorSpace::Srgb,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                metallic_roughness_texture: model_texture(
                    renderer,
                    model,
                    base.metallic_roughness_texture,
                    TextureColorSpace::Linear,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                normal_texture: model_texture(
                    renderer,
                    model,
                    base.normal_texture,
                    TextureColorSpace::Linear,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                emissive_texture: model_texture(
                    renderer,
                    model,
                    base.emissive_texture,
                    TextureColorSpace::Srgb,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                occlusion_texture: model_texture(
                    renderer,
                    model,
                    base.occlusion_texture,
                    TextureColorSpace::Linear,
                    &mut textures_by_role,
                    &mut textures,
                )?,
                normal_scale: base.normal_scale,
                occlusion_strength: base.occlusion_strength,
                double_sided: base.double_sided,
                ..Material::default()
            };
            let material_id = renderer.create_material(&material)?;
            materials.push(material_id);
            let vertices = model_mesh
                .vertices
                .iter()
                .map(|vertex| Vertex {
                    position: vertex.position,
                    normal: vertex.normal,
                    uv: vertex.uv,
                    color: [1.0; 3],
                })
                .collect::<Vec<_>>();
            let mesh_id = match (&model_mesh.skin, model.skeleton.as_ref()) {
                (Some(skin), Some(skeleton)) => renderer.create_skinned_mesh(
                    &vertices,
                    &skin_vertices(skin),
                    &model_mesh.indices,
                    skeleton.joints.len() as u32,
                )?,
                _ => renderer.create_mesh(&vertices, &model_mesh.indices)?,
            };
            meshes.push((mesh_id, material, material_id));
        }
        Ok::<(), RenderError>(())
    })();
    if let Err(error) = upload {
        for (mesh, _, _) in meshes {
            let _ = renderer.destroy_mesh(mesh);
        }
        for material in materials {
            let _ = renderer.destroy_material(material);
        }
        for texture in textures {
            let _ = renderer.destroy_texture(texture);
        }
        return Err(error);
    }
    Ok(GpuModel {
        meshes,
        textures,
        skinned: model.skeleton.is_some(),
        joint_count: model
            .skeleton
            .as_ref()
            .map_or(0, |skeleton| skeleton.joints.len() as u32),
        skeleton: model.skeleton.clone(),
    })
}

fn skin_vertices(skin: &engine_assets::ModelSkin) -> Vec<SkinVertex> {
    skin.joints
        .iter()
        .zip(&skin.weights)
        .map(|(joints, weights)| SkinVertex {
            joints: *joints,
            weights: *weights,
        })
        .collect()
}

fn model_texture(
    renderer: &mut Renderer,
    model: &Model,
    index: Option<usize>,
    color_space: TextureColorSpace,
    cache: &mut HashMap<(usize, TextureColorSpace), TextureId>,
    owned: &mut Vec<TextureId>,
) -> Result<Option<TextureId>, RenderError> {
    let Some(index) = index else {
        return Ok(None);
    };
    if let Some(texture) = cache.get(&(index, color_space)) {
        return Ok(Some(*texture));
    }
    let data = model.textures.get(index).ok_or_else(|| {
        RenderError::InvalidMaterial(format!("Model texture index {index} is out of range"))
    })?;
    let texture = renderer.create_texture_with_color_space(
        data.width,
        data.height,
        &data.rgba,
        color_space,
    )?;
    cache.insert((index, color_space), texture);
    owned.push(texture);
    Ok(Some(texture))
}

fn resolve_material(
    mut base: Material,
    replacement_texture: Option<TextureId>,
    override_material: Option<&MaterialOverride>,
    metallic_roughness_texture: Option<TextureId>,
    normal_texture: Option<TextureId>,
    emissive_texture: Option<TextureId>,
    occlusion_texture: Option<TextureId>,
) -> Material {
    if let Some(texture) = replacement_texture {
        base.base_color_texture = Some(texture);
    }
    if let Some(override_material) = override_material {
        if let Some(color) = override_material.base_color {
            base.base_color = color;
        }
        if let Some(metallic) = override_material.metallic {
            base.metallic = metallic;
        }
        if let Some(roughness) = override_material.roughness {
            base.roughness = roughness;
        }
        if let Some(emissive) = override_material.emissive {
            base.emissive = emissive;
        }
        if let Some(opacity) = override_material.opacity {
            base.opacity = opacity;
        }
        if let Some(alpha_mode) = override_material.alpha_mode {
            base.alpha_mode = match alpha_mode {
                engine_scene::MaterialAlphaMode::Opaque => AlphaMode::Opaque,
                engine_scene::MaterialAlphaMode::Mask => AlphaMode::Mask {
                    cutoff: override_material.alpha_cutoff.unwrap_or(0.5),
                },
                engine_scene::MaterialAlphaMode::Blend => AlphaMode::Blend,
            };
        }
        if let Some(texture) = metallic_roughness_texture {
            base.metallic_roughness_texture = Some(texture);
        }
        if let Some(texture) = normal_texture {
            base.normal_texture = Some(texture);
        }
        if let Some(texture) = emissive_texture {
            base.emissive_texture = Some(texture);
        }
        if let Some(texture) = occlusion_texture {
            base.occlusion_texture = Some(texture);
        }
        if let Some(scale) = override_material.normal_scale {
            base.normal_scale = scale;
        }
        if let Some(strength) = override_material.occlusion_strength {
            base.occlusion_strength = strength;
        }
        if let Some(double_sided) = override_material.double_sided {
            base.double_sided = double_sided;
        }
    }
    base
}

fn error_material() -> Material {
    Material {
        base_color: [1.0, 0.0, 1.0],
        emissive: [0.08, 0.0, 0.08],
        roughness: 1.0,
        ..Material::default()
    }
}

fn resolved_asset_material(resolved: &ResolvedMaterialAsset, shader: Option<ShaderId>) -> Material {
    let mut material = Material {
        shader,
        user_values: resolved.uniforms,
        alpha_mode: match resolved.alpha.mode {
            MaterialAlphaMode::Opaque => AlphaMode::Opaque,
            MaterialAlphaMode::Mask => AlphaMode::Mask {
                cutoff: resolved.alpha.cutoff,
            },
            MaterialAlphaMode::Blend => AlphaMode::Blend,
        },
        double_sided: resolved.double_sided,
        ..Material::default()
    };
    if let Some(lit) = resolved.lit {
        material.base_color = lit.base_color;
        material.metallic = lit.metallic;
        material.roughness = lit.roughness;
        material.emissive = lit.emissive;
        material.opacity = lit.opacity;
        material.normal_scale = lit.normal_scale;
        material.occlusion_strength = lit.occlusion_strength;
    }
    material
}

fn set_lit_texture(material: &mut Material, name: &str, texture: Option<TextureId>) {
    let target = match name {
        "base_color_texture" => &mut material.base_color_texture,
        "metallic_roughness_texture" => &mut material.metallic_roughness_texture,
        "normal_texture" => &mut material.normal_texture,
        "emissive_texture" => &mut material.emissive_texture,
        "occlusion_texture" => &mut material.occlusion_texture,
        _ => return,
    };
    *target = texture;
}

/// Свет кадра. Сцена без directional light показывается светом renderer по умолчанию,
/// иначе объекты редактора выглядели бы плоско залитыми.
fn frame_light(light: &LightState, scene_has_light: bool) -> DirectionalLight {
    if !scene_has_light {
        return DirectionalLight::default();
    }
    DirectionalLight {
        direction: light.direction,
        color: light.color,
        ambient: light.ambient,
    }
}

pub(crate) fn render_local_lights(
    states: &[LocalLightState],
) -> (Vec<LocalLight>, Vec<LocalLightShadow>) {
    let mut lights = Vec::with_capacity(states.len());
    let mut shadows = Vec::new();
    for (light_index, state) in states.iter().enumerate() {
        match *state {
            LocalLightState::Point {
                position,
                color,
                intensity,
                range,
                shadows: casts_shadow,
            } => {
                lights.push(LocalLight::Point {
                    position,
                    color_intensity: color.map(|component| {
                        (f64::from(component) * f64::from(intensity)).min(f64::from(f32::MAX))
                            as f32
                    }),
                    range,
                });
                if casts_shadow
                    && let Some(matrices) =
                        point_shadow_view_projections(Vec3::from_array(position), range)
                {
                    shadows.push(LocalLightShadow {
                        light_index,
                        matrices: LocalLightShadowMatrices::Point {
                            matrices: matrices.map(|matrix| matrix.to_cols_array_2d()),
                        },
                    });
                }
            }
            LocalLightState::Spot {
                position,
                direction,
                color,
                intensity,
                range,
                inner_angle_deg,
                outer_angle_deg,
                shadows: casts_shadow,
            } => {
                lights.push(LocalLight::Spot {
                    position,
                    direction,
                    color_intensity: color.map(|component| {
                        (f64::from(component) * f64::from(intensity)).min(f64::from(f32::MAX))
                            as f32
                    }),
                    range,
                    cos_inner_angle: inner_angle_deg.to_radians().cos().max(0.0),
                    cos_outer_angle: outer_angle_deg.to_radians().cos().max(0.0),
                });
                if casts_shadow
                    && let Some(matrix) = spot_shadow_view_projection(
                        Vec3::from_array(position),
                        Vec3::from_array(direction),
                        inner_angle_deg,
                        outer_angle_deg,
                        range,
                    )
                {
                    shadows.push(LocalLightShadow {
                        light_index,
                        matrices: LocalLightShadowMatrices::Spot {
                            matrix: matrix.to_cols_array_2d(),
                        },
                    });
                }
            }
        }
    }
    (lights, shadows)
}

fn editor_environment_key(state: &EnvironmentState) -> EditorEnvironmentKey {
    if let Some(path) = &state.component.hdr {
        return EditorEnvironmentKey::Hdr(path.as_str().to_owned());
    }
    let sky = editor_procedural_sky(state).unwrap_or(ProceduralSky {
        zenith_color: [0.2, 0.35, 0.65],
        horizon_color: [0.65, 0.7, 0.75],
        ground_color: [0.12, 0.11, 0.1],
        sun_size_deg: 1.0,
        sun_intensity: 1.0,
        sun_direction: [0.0, 1.0, 0.0],
    });
    EditorEnvironmentKey::Procedural {
        colors_and_sun: [
            sky.zenith_color[0].to_bits(),
            sky.zenith_color[1].to_bits(),
            sky.zenith_color[2].to_bits(),
            sky.horizon_color[0].to_bits(),
            sky.horizon_color[1].to_bits(),
            sky.horizon_color[2].to_bits(),
            sky.ground_color[0].to_bits(),
            sky.ground_color[1].to_bits(),
            sky.ground_color[2].to_bits(),
            sky.sun_size_deg.to_bits(),
            sky.sun_intensity.to_bits(),
        ],
        sun_direction: sky
            .sun_direction
            .map(|value| (value * 4096.0).round() as i32),
    }
}

fn editor_procedural_sky(state: &EnvironmentState) -> Option<ProceduralSky> {
    let procedural = state.component.procedural?;
    let world_sun = state.sun_direction.unwrap_or([0.0, 1.0, 0.0]);
    let world_sun = Vec3::from_array(world_sun).normalize_or_zero();
    let rotation = Mat3::from_cols_array_2d(&state.rotation);
    let sun_direction = (rotation.transpose() * world_sun).normalize_or_zero();
    Some(ProceduralSky {
        zenith_color: procedural.zenith_color,
        horizon_color: procedural.horizon_color,
        ground_color: procedural.ground_color,
        sun_size_deg: procedural.sun_size_deg,
        sun_intensity: procedural.sun_intensity,
        sun_direction: sun_direction.to_array(),
    })
}

/// Шаг сетки — степень десяти по расстоянию камеры: линии сохраняют толщину на экране.
fn grid_scale(camera_distance: f32) -> f32 {
    10_f32.powf(camera_distance.max(1e-3).log10().floor())
}

/// Unlit-поверхность для сетки: без освещения сетка не получает тени от объектов и AO.
fn grid_surface_source() -> SurfaceShaderSource {
    SurfaceShaderSource {
        lighting: SurfaceLighting::Unlit,
        properties: Vec::new(),
        code: "void surface(inout Surface s) {}".to_owned(),
    }
}

/// Сетка пола из тонких квадратов: renderer рисует только треугольники.
fn grid_mesh() -> (Vec<Vertex>, Vec<u32>) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let extent = GRID_HALF_CELLS as f32;
    let half = GRID_LINE_WIDTH * 0.5;
    let mut quad = |corners: [[f32; 2]; 4], color: [f32; 3]| {
        let first = vertices.len() as u32;
        vertices.extend(corners.map(|[x, z]| Vertex {
            position: [x, 0.0, z],
            normal: [0.0, 1.0, 0.0],
            uv: [0.0, 0.0],
            color,
        }));
        indices.extend([first, first + 2, first + 1, first, first + 3, first + 2]);
    };
    for line in -GRID_HALF_CELLS..=GRID_HALF_CELLS {
        let offset = line as f32;
        let (x_color, z_color) = match line {
            0 => ([0.75, 0.2, 0.2], [0.2, 0.35, 0.8]),
            _ if line % 5 == 0 => ([0.32; 3], [0.32; 3]),
            _ => ([0.2; 3], [0.2; 3]),
        };
        // Линия вдоль X в точке Z = offset и линия вдоль Z в точке X = offset.
        quad(
            [
                [-extent, offset - half],
                [extent, offset - half],
                [extent, offset + half],
                [-extent, offset + half],
            ],
            x_color,
        );
        quad(
            [
                [offset - half, -extent],
                [offset + half, -extent],
                [offset + half, extent],
                [offset - half, extent],
            ],
            z_color,
        );
    }
    (vertices, indices)
}

/// Переводит постобработку камеры в настройки renderer. Scene-вид редактора не выполняет DoF и
/// motion blur: эти эффекты мешают редактированию, остальные эффекты совпадают с Game-видом.
fn render_post_process(
    post: &PostProcessSettings,
    lut: Option<ColorLutId>,
    scene_view: bool,
) -> PostProcess {
    let exposure = if post.exposure.enabled {
        Exposure {
            mode: match post.exposure.mode {
                engine_scene::ExposureMode::Manual => engine_render::ExposureMode::Manual,
                engine_scene::ExposureMode::Auto => engine_render::ExposureMode::Auto,
            },
            ev: post.exposure.ev,
            min_ev: post.exposure.min_ev,
            max_ev: post.exposure.max_ev,
            speed_up: post.exposure.speed_up,
            speed_down: post.exposure.speed_down,
            low_percent: post.exposure.low_percent,
            high_percent: post.exposure.high_percent,
        }
    } else {
        Exposure::default()
    };
    let tonemapper = match (post.tonemapping.enabled, post.tonemapping.tonemapper) {
        (false, _) | (true, Tonemapper::None) => RenderTonemapper::None,
        (true, Tonemapper::Aces) => RenderTonemapper::Aces,
        (true, Tonemapper::Neutral) => RenderTonemapper::Neutral,
    };
    let quality = match post.ambient_occlusion.quality {
        engine_scene::AmbientOcclusionQuality::Low => engine_render::AmbientOcclusionQuality::Low,
        engine_scene::AmbientOcclusionQuality::Medium => {
            engine_render::AmbientOcclusionQuality::Medium
        }
        engine_scene::AmbientOcclusionQuality::High => engine_render::AmbientOcclusionQuality::High,
    };
    let bloom = post.bloom;
    let ao = post.ambient_occlusion;
    let grading = &post.color_grading;
    let vignette = post.vignette;
    let aberration = post.chromatic_aberration;
    let grain = post.film_grain;
    let dof = post.depth_of_field;
    let motion = post.motion_blur;
    PostProcess {
        exposure,
        tonemapper,
        bloom: engine_render::Bloom {
            enabled: bloom.enabled,
            intensity: bloom.intensity,
            threshold: bloom.threshold,
            knee: bloom.knee,
            scatter: bloom.scatter,
            tint: bloom.tint,
        },
        ambient_occlusion: engine_render::AmbientOcclusion {
            enabled: ao.enabled,
            radius: ao.radius,
            intensity: ao.intensity,
            falloff: ao.falloff,
            quality,
        },
        color_grading: engine_render::ColorGrading {
            enabled: grading.enabled,
            temperature: grading.temperature,
            tint: grading.tint,
            contrast: grading.contrast,
            saturation: grading.saturation,
            lift: grading.lift,
            gamma: grading.gamma,
            gain: grading.gain,
            lut,
            lut_intensity: grading.lut_intensity,
        },
        vignette: engine_render::Vignette {
            enabled: vignette.enabled,
            intensity: vignette.intensity,
            smoothness: vignette.smoothness,
            roundness: vignette.roundness,
            color: vignette.color,
        },
        chromatic_aberration: engine_render::ChromaticAberration {
            enabled: aberration.enabled,
            intensity: aberration.intensity,
        },
        film_grain: engine_render::FilmGrain {
            enabled: grain.enabled,
            intensity: grain.intensity,
            response: grain.response,
        },
        depth_of_field: engine_render::DepthOfField {
            enabled: dof.enabled && !scene_view,
            focus_distance: dof.focus_distance,
            aperture: dof.aperture,
            focal_length_mm: dof.focal_length_mm,
            max_blur_px: dof.max_blur_px,
        },
        motion_blur: engine_render::MotionBlur {
            enabled: motion.enabled && !scene_view,
            shutter_angle_deg: motion.shutter_angle_deg,
            samples: motion.samples,
            max_blur_px: motion.max_blur_px,
        },
    }
}

/// Пересоздавать ли шейдер при hot-reload: нужна ревизия, которая ещё не загружена и не падала.
/// Провалившаяся ревизия не повторяется, пока файл не изменится; прежний шейдер остаётся активным.
fn surface_shader_rebuild_needed(
    loaded_revision: Option<u64>,
    failed: &HashSet<(String, u64)>,
    path: &str,
    revision: u64,
) -> bool {
    loaded_revision != Some(revision) && !failed.contains(&(path.to_owned(), revision))
}

/// Материалы из `active`, которые ссылаются на `shader_path`; при смене шейдера их надо пересоздать.
fn materials_using_shader(
    active: &HashSet<String>,
    shader_of: impl Fn(&str) -> Option<String>,
    shader_path: &str,
) -> Vec<String> {
    let mut paths = active
        .iter()
        .filter(|material| shader_of(material).as_deref() == Some(shader_path))
        .cloned()
        .collect::<Vec<_>>();
    paths.sort();
    paths
}

fn active_material_paths(world: &World, preview_asset: Option<&str>) -> HashSet<String> {
    let mut paths = world
        .objects()
        .iter()
        .filter_map(|object| object.mesh_renderer.as_ref())
        .flat_map(|mesh| mesh.materials.iter().flatten())
        .map(|path| path.as_str().to_owned())
        .collect::<HashSet<_>>();
    // Все объявленные `.rvemat` готовятся, а не только слоты: скрипт назначает их через set_material.
    paths.extend(
        world
            .asset_paths()
            .iter()
            .filter(|path| path.as_str().to_ascii_lowercase().ends_with(".rvemat"))
            .map(|path| path.as_str().to_owned()),
    );
    paths.extend(
        world
            .objects()
            .iter()
            .filter_map(|object| world.mesh_material_path(object.handle))
            .map(|path| path.as_str().to_owned()),
    );
    if let Some(path) = preview_asset
        && path.to_ascii_lowercase().ends_with(".rvemat")
    {
        paths.insert(path.to_owned());
    }
    paths
}

/// Шейдеры без ссылок из материалов. Шейдер превью исключается: его материал создаётся позже
/// в том же кадре, и уничтожение заставило бы компилировать шейдер заново.
fn unused_surface_shaders<T: Copy + Eq + Hash>(
    loaded: impl IntoIterator<Item = (String, T)>,
    referenced: &HashSet<T>,
    preview_shader_path: Option<&str>,
) -> Vec<(String, T)> {
    loaded
        .into_iter()
        .filter(|(path, id)| !referenced.contains(id) && Some(path.as_str()) != preview_shader_path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_files::VirtualPath;
    use engine_scene::{
        EnvironmentComponent, LodLevel, PrefabTemplate, ProceduralSkyComponent, SpawnOptions,
    };

    fn fake_lut(value: usize) -> ColorLutId {
        // SAFETY: ColorLutId — обёртка над одним usize; любое значение допустимо для теста.
        unsafe { std::mem::transmute(value) }
    }

    #[test]
    fn color_lut_is_found_by_canonical_path_and_mapped_in_both_views() {
        let luts = HashMap::from([
            ("luts/film.cube".to_owned(), Some(fake_lut(3))),
            ("luts/broken.cube".to_owned(), None),
        ]);
        let mut post = PostProcessSettings::default();
        assert_eq!(cached_color_lut(&luts, &post), None);
        post.color_grading.lut = Some("/luts/film.cube".to_owned());
        assert_eq!(cached_color_lut(&luts, &post), Some(fake_lut(3)));
        post.color_grading.lut = Some("luts/film.cube".to_owned());
        assert_eq!(cached_color_lut(&luts, &post), Some(fake_lut(3)));
        post.color_grading.lut = Some("/luts/broken.cube".to_owned());
        assert_eq!(cached_color_lut(&luts, &post), None, "LUT с ошибкой");

        post.color_grading.lut_intensity = 0.3;
        for scene_view in [false, true] {
            let mapped = render_post_process(&post, Some(fake_lut(3)), scene_view);
            assert_eq!(mapped.color_grading.lut, Some(fake_lut(3)));
            assert_eq!(mapped.color_grading.lut_intensity, 0.3);
        }
    }

    #[test]
    fn hot_reload_rebuilds_only_new_revisions_that_have_not_failed() {
        let path = "/s.rveshader";
        let mut failed = HashSet::new();
        let needed = |loaded, failed: &HashSet<(String, u64)>, path, revision| {
            surface_shader_rebuild_needed(loaded, failed, path, revision)
        };
        assert!(!needed(Some(4), &failed, path, 4));
        assert!(needed(Some(4), &failed, path, 5));
        assert!(needed(None, &failed, path, 1));
        failed.insert((path.to_owned(), 5));
        assert!(!needed(Some(4), &failed, path, 5));
        assert!(needed(Some(4), &failed, path, 6));
        assert!(needed(Some(4), &failed, "/other.rveshader", 5));
    }

    #[test]
    fn shader_change_selects_only_its_dependent_materials_in_order() {
        let active: HashSet<String> = ["/m/b.rvemat", "/m/a.rvemat", "/m/lit.rvemat"]
            .map(str::to_owned)
            .into_iter()
            .collect();
        let shader_of = |material: &str| match material {
            "/m/a.rvemat" | "/m/b.rvemat" => Some("/s/water.rveshader".to_owned()),
            _ => None,
        };
        assert_eq!(
            materials_using_shader(&active, shader_of, "/s/water.rveshader"),
            ["/m/a.rvemat", "/m/b.rvemat"]
        );
        assert!(materials_using_shader(&active, shader_of, "/s/other.rveshader").is_empty());
    }

    #[test]
    fn unreferenced_shader_is_retired_unless_it_backs_the_preview() {
        let loaded = vec![
            ("/s/unused.rveshader".to_owned(), 1_usize),
            ("/s/preview.rveshader".to_owned(), 2),
            ("/s/used.rveshader".to_owned(), 3),
        ];
        let referenced = HashSet::from([3_usize]);
        let retired = unused_surface_shaders(loaded, &referenced, Some("/s/preview.rveshader"));
        assert_eq!(retired, [("/s/unused.rveshader".to_owned(), 1)]);
    }

    #[test]
    fn spawned_prefab_material_is_active_for_play_rendering() {
        let document = engine_scene::SceneDocument::parse(
            "materials.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let prefab_path = VirtualPath::parse("prefabs/crate.rveprefab").unwrap();
        let prefab = PrefabTemplate::load(&prefab_path, &mut |_| {
            Ok("[[assets]]\npath = \"/models/crate.fbx\"\n[[assets]]\npath = \"/materials/gold.rvemat\"\n[[objects]]\nid = \"crate\"\n[objects.mesh_renderer]\nmodel = \"/models/crate.fbx\"\n".to_owned())
        })
        .unwrap();
        let handles = world
            .spawn(
                vec![world.reserve_object()],
                &prefab,
                SpawnOptions::default(),
            )
            .unwrap();
        let material = VirtualPath::parse("/materials/gold.rvemat").unwrap();
        world.set_mesh_material_path(handles[0], material).unwrap();

        assert!(active_material_paths(&world, None).contains("materials/gold.rvemat"));
    }

    fn camera() -> Camera {
        Camera {
            position: Vec3::new(0.0, 0.0, 5.0),
            target: Vec3::ZERO,
            up: Vec3::Y,
            fov_narrow: std::f32::consts::FRAC_PI_4,
            near: 0.1,
            far: 100.0,
        }
    }

    #[test]
    fn cache_update_preserves_old_values_on_error_and_returns_replaced_values() {
        let (current, old, error) = cache_update(Some(1), Err("broken".to_owned()));
        assert_eq!(current, Some(1));
        assert_eq!(old, None);
        assert_eq!(error.as_deref(), Some("broken"));

        let (current, old, error) = cache_update(None::<i32>, Err("missing".to_owned()));
        assert_eq!(current, None);
        assert_eq!(old, None);
        assert_eq!(error.as_deref(), Some("missing"));

        let (current, old, error) = cache_update(Some(1), Ok(2));
        assert_eq!(current, Some(2));
        assert_eq!(old, Some(1));
        assert_eq!(error, None);
    }

    #[test]
    fn resource_validation_errors_are_distinguished_from_fatal_errors() {
        assert!(is_resource_error(&RenderError::InvalidTexture(
            "bad".to_owned()
        )));
        assert!(is_resource_error(&RenderError::InvalidMesh("bad")));
        assert!(is_resource_error(&RenderError::InvalidMaterial(
            "bad".to_owned()
        )));
        assert!(is_resource_error(&RenderError::InvalidEnvironment(
            "bad".to_owned()
        )));
        assert!(!is_resource_error(&RenderError::InvalidConfig("bad")));
    }

    #[test]
    fn changed_png_selects_only_models_that_use_it_and_normalizes_slashes() {
        let models = HashMap::from([
            ("/models/a.fbx".to_owned(), Some(())),
            ("models/b.fbx".to_owned(), Some(())),
        ]);
        let dependencies = HashMap::from([
            (
                "models/a.fbx".to_owned(),
                vec![VirtualPath::parse("/textures/a.png").unwrap()],
            ),
            (
                "/models/b.fbx".to_owned(),
                vec![VirtualPath::parse("textures/b.png").unwrap()],
            ),
        ]);
        let changes = AssetChanges {
            paths: HashSet::from([VirtualPath::parse("textures/a.png").unwrap()]),
            ..AssetChanges::default()
        };
        let plan = model_reload_plan(&changes, &models, &dependencies, &HashSet::new(), &[]);
        assert_eq!(plan.cached, HashSet::from(["/models/a.fbx".to_owned()]));

        let fbx_change = AssetChanges {
            paths: HashSet::from([VirtualPath::parse("models/a.fbx").unwrap()]),
            ..AssetChanges::default()
        };
        let plan = model_reload_plan(&fbx_change, &models, &dependencies, &HashSet::new(), &[]);
        assert_eq!(plan.cached, HashSet::from(["/models/a.fbx".to_owned()]));
    }

    #[test]
    fn png_or_directory_change_retries_failed_models_and_stales_pending_models() {
        let models = HashMap::from([("models/missing.fbx".to_owned(), None::<()>)]);
        let dependencies = HashMap::new();
        let pending = vec!["models/pending.fbx".to_owned()];
        let png_change = AssetChanges {
            paths: HashSet::from([VirtualPath::parse("textures/new.png").unwrap()]),
            ..AssetChanges::default()
        };
        let plan = model_reload_plan(
            &png_change,
            &models,
            &dependencies,
            &HashSet::new(),
            &pending,
        );
        assert_eq!(
            plan.cached,
            HashSet::from(["models/missing.fbx".to_owned()])
        );
        assert_eq!(
            plan.pending,
            HashSet::from(["models/pending.fbx".to_owned()])
        );

        let folder_change = AssetChanges {
            paths: HashSet::from([VirtualPath::parse("textures").unwrap()]),
            directory_paths: HashSet::from([VirtualPath::parse("textures").unwrap()]),
            ..AssetChanges::default()
        };
        let plan = model_reload_plan(
            &folder_change,
            &models,
            &dependencies,
            &HashSet::new(),
            &pending,
        );
        assert_eq!(
            plan.cached,
            HashSet::from(["models/missing.fbx".to_owned()])
        );
        assert_eq!(
            plan.pending,
            HashSet::from(["models/pending.fbx".to_owned()])
        );
    }

    #[test]
    fn png_change_retries_a_model_with_a_failed_reload() {
        let models = HashMap::from([("models/ship.fbx".to_owned(), Some(()))]);
        let dependencies = HashMap::new();
        let changes = AssetChanges {
            paths: HashSet::from([VirtualPath::parse("textures/b.png").unwrap()]),
            ..AssetChanges::default()
        };
        let failed_reloads = HashSet::from(["models/ship.fbx".to_owned()]);

        let plan = model_reload_plan(&changes, &models, &dependencies, &failed_reloads, &[]);
        assert_eq!(plan.cached, HashSet::from(["models/ship.fbx".to_owned()]));

        let plan = model_reload_plan(&changes, &models, &dependencies, &HashSet::new(), &[]);
        assert!(plan.cached.is_empty());
    }

    #[test]
    fn environment_cache_key_ignores_frame_controls_and_quantizes_sun_direction() {
        let state = EnvironmentState {
            component: EnvironmentComponent {
                hdr: None,
                procedural: Some(ProceduralSkyComponent {
                    zenith_color: [0.2, 0.35, 0.65],
                    horizon_color: [0.65, 0.7, 0.75],
                    ground_color: [0.12, 0.11, 0.1],
                    sun_size_deg: 1.0,
                    sun_intensity: 1.0,
                }),
                intensity: 1.0,
                skybox: true,
            },
            rotation: Mat3::IDENTITY.to_cols_array_2d(),
            sun_direction: Some([0.0, 1.0, 0.0]),
        };
        let key = editor_environment_key(&state);
        let mut frame_controls = state.clone();
        frame_controls.component.intensity = 2.0;
        frame_controls.component.skybox = false;
        assert_eq!(key, editor_environment_key(&frame_controls));

        let mut small_sun_change = state.clone();
        small_sun_change.sun_direction = Some([0.0, 1.0, 0.00005]);
        assert_eq!(key, editor_environment_key(&small_sun_change));
        small_sun_change.sun_direction = Some([0.0, 0.99, 0.14]);
        assert_ne!(key, editor_environment_key(&small_sun_change));

        let mut hdr = state;
        hdr.component.hdr = Some(VirtualPath::parse("/environment/sky.hdr").unwrap());
        hdr.component.procedural = None;
        assert_ne!(key, editor_environment_key(&hdr));
    }

    #[test]
    fn failed_environment_creation_keeps_only_the_same_key() {
        let current = EditorEnvironmentKey::Hdr("environment/a.hdr".to_owned());
        let other = EditorEnvironmentKey::Hdr("environment/b.hdr".to_owned());

        assert_eq!(
            environment_transition(Some(&current), &current, EnvironmentCreation::ResourceError,),
            EnvironmentTransition::Keep
        );
        assert_eq!(
            environment_transition(Some(&current), &other, EnvironmentCreation::MissingSource,),
            EnvironmentTransition::DropToDefault
        );
        assert_eq!(
            environment_transition(Some(&current), &other, EnvironmentCreation::ResourceError,),
            EnvironmentTransition::DropToDefault
        );
    }

    #[test]
    fn successful_environment_creation_replaces_the_current_key() {
        let current = EditorEnvironmentKey::Hdr("environment/a.hdr".to_owned());
        let requested = EditorEnvironmentKey::Hdr("environment/b.hdr".to_owned());

        assert_eq!(
            environment_transition(Some(&current), &requested, EnvironmentCreation::Success,),
            EnvironmentTransition::Replace
        );
    }

    #[test]
    fn lod_materials_for_two_viewports_do_not_replace_each_other() {
        let document = engine_scene::SceneDocument::parse(
            "test",
            r#"
[[objects]]
id = "camera"
[objects.camera]
"#,
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let object = world.handle("camera").unwrap();
        let scene_key = object_material_key(object, "shared.fbx", 1, 0);
        let game_key = object_material_key(object, "shared.fbx", 2, 0);
        let mut cache = HashMap::new();
        cache.insert(scene_key.clone(), "scene texture material");
        cache.insert(game_key.clone(), "game texture material");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache[&scene_key], "scene texture material");
        assert_eq!(cache[&game_key], "game texture material");
    }

    #[test]
    fn capture_eye_keeps_an_object_culled_for_both_panels() {
        let document = engine_scene::SceneDocument::parse(
            "test",
            r#"
[[assets]]
path="/base.fbx"
[[assets]]
path="/lod.fbx"
[[objects]]
id="camera"
[objects.camera]
[[objects]]
id="prop"
[objects.mesh_renderer]
model="/base.fbx"
[objects.lod_group]
cull_distance=20.0
[[objects.lod_group.levels]]
model="/lod.fbx"
distance=10.0
"#,
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let (frame, _) = world.build_frame();
        let prop = world.handle("prop").unwrap();
        // Обе панельные камеры отсекают объект, а capture-камера рядом с ним видит его уровень 0.
        let camera = |position: Vec3| Camera {
            position,
            target: Vec3::ZERO,
            up: Vec3::Y,
            fov_narrow: engine_scene::LOD_REFERENCE_FOV,
            near: 0.1,
            far: 100.0,
        };
        for panel_eye in [Vec3::new(40.0, 0.0, 0.0), Vec3::new(-40.0, 0.0, 0.0)] {
            let mut lod = LodSelector::default();
            assert!(
                visible_mesh_levels(&world, &frame, camera(panel_eye), &mut lod)
                    .all(|(instance, _)| instance.object != prop)
            );
        }
        let mut lod = LodSelector::default();
        let captured =
            visible_mesh_levels(&world, &frame, camera(Vec3::new(5.0, 0.0, 0.0)), &mut lod)
                .find(|(instance, _)| instance.object == prop)
                .map(|(_, level)| level);
        assert_eq!(captured, Some(0));
    }

    #[test]
    fn grid_is_flat_indexed_quads_with_two_lines_per_row() {
        let (vertices, indices) = grid_mesh();
        let rows = (GRID_HALF_CELLS * 2 + 1) as usize;
        assert_eq!(vertices.len(), rows * 2 * 4);
        assert_eq!(indices.len(), rows * 2 * 6);
        assert!(vertices.iter().all(|vertex| vertex.position[1] == 0.0));
        assert!(
            indices
                .iter()
                .all(|&index| (index as usize) < vertices.len())
        );
        for triangle in indices.as_chunks::<3>().0 {
            let a = Vec3::from_array(vertices[triangle[0] as usize].position);
            let b = Vec3::from_array(vertices[triangle[1] as usize].position);
            let c = Vec3::from_array(vertices[triangle[2] as usize].position);
            let normal = Vec3::from_array(vertices[triangle[0] as usize].normal);
            assert!((b - a).cross(c - a).dot(normal) > 0.0);
        }
    }

    #[test]
    fn grid_surface_is_unlit_and_compiles() {
        let source = grid_surface_source();
        assert_eq!(source.lighting, SurfaceLighting::Unlit);
        assert!(source.properties.is_empty());
        let variants = engine_render::compile_surface_shader(&source)
            .expect("unlit grid surface must compile");
        assert!(!variants.opaque.is_empty());
    }

    #[test]
    fn grid_step_follows_the_camera_distance_in_powers_of_ten() {
        assert_eq!(grid_scale(8.0), 1.0);
        assert_eq!(grid_scale(25.0), 10.0);
        assert!((grid_scale(0.2) - 0.1).abs() < 1e-5);
        assert!(grid_scale(0.0) > 0.0);
    }

    #[test]
    fn lod_paths_keep_model_order_and_resolve_texture_inheritance() {
        let mesh = MeshRenderer {
            model: VirtualPath::parse("/models/base.fbx").unwrap(),
            texture: Some(VirtualPath::parse("/textures/base.png").unwrap()),
            material: None,
            materials: Vec::new(),
        };
        let group = LodGroup {
            levels: vec![
                LodLevel {
                    model: "/models/lod1.fbx".to_owned(),
                    texture: Some("/textures/lod.png".to_owned()),
                    distance: 10.0,
                },
                LodLevel {
                    model: "/models/lod2.fbx".to_owned(),
                    texture: None,
                    distance: 20.0,
                },
            ],
            cull_distance: None,
        };
        assert_eq!(
            lod_model_path(&mesh, Some(&group), 0),
            Some("models/base.fbx")
        );
        assert_eq!(
            lod_model_path(&mesh, Some(&group), 1),
            Some("models/lod1.fbx")
        );
        assert_eq!(
            lod_model_path(&mesh, Some(&group), 2),
            Some("models/lod2.fbx")
        );
        assert_eq!(
            lod_texture_path(&mesh, Some(&group), 0),
            Some("textures/base.png")
        );
        assert_eq!(
            lod_texture_path(&mesh, Some(&group), 1),
            Some("textures/lod.png")
        );
        assert_eq!(
            lod_texture_path(&mesh, Some(&group), 2),
            Some("textures/base.png")
        );
    }

    #[test]
    fn scene_without_a_light_uses_the_renderer_default_and_no_shadows() {
        let state = LightState::default();
        let light = frame_light(&state, false);
        assert_eq!(light.direction, DirectionalLight::default().direction);
        assert!(SceneRenderer::shadow_cascades(camera(), 1280, 720, &state, false).is_empty());
    }

    #[test]
    fn scene_light_and_shadow_are_passed_through() {
        let state = LightState {
            direction: [0.0, -1.0, 0.0],
            color: [0.5; 3],
            ambient: [0.1; 3],
            shadow: Some(engine_scene::ShadowSettings {
                distance: 40.0,
                cascades: 2,
            }),
        };
        let light = frame_light(&state, true);
        assert_eq!(light.color, [0.5; 3]);
        assert_eq!(
            SceneRenderer::shadow_cascades(camera(), 1280, 720, &state, true).len(),
            2
        );
        let without_shadow = LightState {
            shadow: None,
            ..state
        };
        assert!(
            SceneRenderer::shadow_cascades(camera(), 1280, 720, &without_shadow, true).is_empty()
        );
    }

    #[test]
    fn editor_maps_scene_local_lights_for_each_viewport_frame() {
        let states = [
            LocalLightState::Point {
                position: [1.0, 2.0, 3.0],
                color: [0.25, 0.5, 1.0],
                intensity: 4.0,
                range: 9.0,
                shadows: true,
            },
            LocalLightState::Spot {
                position: [0.0, 0.0, 1.0],
                direction: [0.0, 0.0, -1.0],
                color: [1.0, 0.5, 0.25],
                intensity: 2.0,
                range: 12.0,
                inner_angle_deg: 15.0,
                outer_angle_deg: 30.0,
                shadows: false,
            },
        ];
        let (lights, shadow_matrices) = render_local_lights(&states);
        assert_eq!(shadow_matrices.len(), 1);
        assert_eq!(shadow_matrices[0].light_index, 0);
        assert!(matches!(
            shadow_matrices[0].matrices,
            LocalLightShadowMatrices::Point { .. }
        ));
        assert_eq!(
            lights[0],
            LocalLight::Point {
                position: [1.0, 2.0, 3.0],
                color_intensity: [1.0, 2.0, 4.0],
                range: 9.0,
            }
        );
        assert!(matches!(
            lights[1],
            LocalLight::Spot {
                color_intensity: [2.0, 1.0, 0.5],
                cos_inner_angle,
                cos_outer_angle,
                ..
            } if (cos_inner_angle - 15.0_f32.to_radians().cos()).abs() < 1e-6
                && (cos_outer_angle - 30.0_f32.to_radians().cos()).abs() < 1e-6
        ));

        let ninety_degree = LocalLightState::Spot {
            position: [0.0; 3],
            direction: [0.0, 0.0, -1.0],
            color: [1.0; 3],
            intensity: 1.0,
            range: 5.0,
            inner_angle_deg: 90.0,
            outer_angle_deg: 90.0,
            shadows: true,
        };
        let (ninety_lights, ninety_shadows) = render_local_lights(&[ninety_degree]);
        assert!(matches!(
            ninety_lights[0],
            LocalLight::Spot {
                cos_inner_angle: 0.0,
                cos_outer_angle: 0.0,
                ..
            }
        ));
        assert!(matches!(
            ninety_shadows.as_slice(),
            [LocalLightShadow {
                light_index: 0,
                matrices: LocalLightShadowMatrices::Spot { .. },
            }]
        ));
    }

    #[test]
    fn scene_valid_local_light_factors_saturate_without_infinity() {
        let document = engine_scene::SceneDocument::parse(
            "lights.rvescene",
            r#"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "point"
[objects.point_light]
color = [2e38, 1, 0]
intensity = 2
range = 10
[[objects]]
id = "spot"
[objects.spot_light]
color = [2e38, 1, 0]
intensity = 2
range = 10
"#,
        )
        .unwrap();
        let mut world = engine_scene::World::from_document(&document).unwrap();
        let (lights, _) = render_local_lights(&world.build_frame().0.local_lights);
        assert_eq!(lights.len(), 2);
        for light in lights {
            let color_intensity = match light {
                LocalLight::Point {
                    color_intensity, ..
                }
                | LocalLight::Spot {
                    color_intensity, ..
                } => color_intensity,
            };
            assert_eq!(color_intensity, [f32::MAX, 2.0, 0.0]);
        }
    }
    #[test]
    fn scene_and_game_viewports_build_camera_relative_shadow_cascades() {
        let light = LightState {
            shadow: Some(engine_scene::ShadowSettings {
                distance: 60.0,
                cascades: 3,
            }),
            ..LightState::default()
        };
        let first = camera();
        let mut second = first;
        second.position += Vec3::X * 12.0;
        second.target += Vec3::X * 12.0;
        let first_cascades = SceneRenderer::shadow_cascades(first, 1280, 720, &light, true);
        let second_cascades = SceneRenderer::shadow_cascades(second, 1280, 720, &light, true);
        assert_eq!(first_cascades.len(), 3);
        assert_eq!(second_cascades.len(), 3);
        assert_ne!(first_cascades[0], second_cascades[0]);
    }

    #[test]
    fn scene_material_factors_override_fbx_values_and_unset_fields_are_kept() {
        let base = Material {
            base_color: [0.4, 0.5, 0.6],
            metallic: 0.2,
            roughness: 0.8,
            normal_scale: 0.7,
            ..Material::default()
        };
        let override_material = MaterialOverride {
            base_color: Some([0.8, 0.7, 0.6]),
            metallic: Some(1.0),
            roughness: None,
            emissive: Some([2.0, 1.0, 0.5]),
            opacity: Some(0.4),
            alpha_mode: Some(engine_scene::MaterialAlphaMode::Mask),
            alpha_cutoff: Some(0.3),
            metallic_roughness_texture: None,
            normal_texture: None,
            emissive_texture: None,
            occlusion_texture: None,
            normal_scale: Some(0.5),
            occlusion_strength: Some(0.25),
            double_sided: Some(true),
        };

        let resolved =
            resolve_material(base, None, Some(&override_material), None, None, None, None);

        assert_eq!(resolved.base_color, [0.8, 0.7, 0.6]);
        assert_eq!(resolved.metallic, 1.0);
        assert_eq!(resolved.roughness, 0.8);
        assert_eq!(resolved.emissive, [2.0, 1.0, 0.5]);
        assert_eq!(resolved.opacity, 0.4);
        assert_eq!(resolved.alpha_mode, AlphaMode::Mask { cutoff: 0.3 });
        assert_eq!(resolved.normal_scale, 0.5);
        assert_eq!(resolved.occlusion_strength, 0.25);
        assert_eq!(resolved.base_color_texture, None);
        assert_eq!(resolved.metallic_roughness_texture, None);
        assert_eq!(resolved.normal_texture, None);
    }

    #[test]
    fn editor_motion_history_is_shared_until_frame_commit_and_resets_on_camera_or_play_change() {
        let document = engine_scene::SceneDocument::parse(
            "motion.rvescene",
            "[[objects]]\nid = \"main\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"game\"\n[objects.camera]\n[[objects]]\nid = \"mesh\"\n[objects.transform]\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let main_camera = world.handle("main").unwrap();
        let game_camera = world.handle("game").unwrap();
        let mesh = world.handle("mesh").unwrap();
        let first_model = Mat4::from_translation(Vec3::X).to_cols_array_2d();
        let next_model = Mat4::from_translation(Vec3::Y).to_cols_array_2d();
        let mut history = MotionHistory::default();

        history.begin_frame(game_camera, false);
        history.record_current(mesh, first_model);
        assert_eq!(history.previous_model(mesh), None);
        history.record_current(mesh, first_model);
        assert_eq!(history.previous_model(mesh), None);
        history.commit_frame();

        history.begin_frame(game_camera, false);
        history.record_current(mesh, next_model);
        assert_eq!(history.previous_model(mesh), Some(first_model));
        assert_eq!(history.previous_model(mesh), Some(first_model));
        history.commit_frame();
        assert_eq!(history.previous_model(mesh), Some(next_model));

        history.begin_frame(main_camera, false);
        assert!(history.reset_pending);
        assert_eq!(history.previous_model(mesh), None);
        history.commit_frame();
        history.begin_frame(main_camera, true);
        assert!(history.reset_pending);
        assert_eq!(history.previous_model(mesh), None);
    }

    /// Два объекта с аниматором, использующие одну модель, и один без аниматора.
    fn skin_world() -> World {
        let text = "[[assets]]\npath = \"/models/hero.fbx\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"hero\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n[objects.animator]\n[[objects]]\nid = \"guard\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n[objects.animator]\n[[objects]]\nid = \"prop\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n";
        World::from_document(&engine_scene::SceneDocument::parse("scene.rvescene", text).unwrap())
            .unwrap()
    }

    fn skin(model: &str, meshes: &[u32]) -> SkinnedObject<u32, u32> {
        SkinnedObject {
            model: model.to_owned(),
            // Одна кость, как у модели `walk_model` в тестах моста.
            joint_count: 1,
            mesh_ids: meshes.to_vec(),
            instances: meshes.iter().map(|mesh| mesh + 100).collect(),
            lods: Vec::new(),
        }
    }

    fn wanted(entries: &[(ObjectHandle, &str, &[u32])]) -> HashMap<ObjectHandle, WantedSkin<u32>> {
        entries
            .iter()
            .map(|(object, model, meshes)| {
                (*object, ((*model).to_owned(), meshes.to_vec(), Vec::new()))
            })
            .collect()
    }

    #[test]
    fn skin_sync_keeps_matching_instances_and_creates_missing_ones() {
        let world = skin_world();
        let hero = world.handle("hero").unwrap();
        let current = HashMap::from([(hero, skin("models/hero.fbx", &[1, 2]))]);
        let same = wanted(&[(hero, "models/hero.fbx", &[1, 2])]);
        assert_eq!(plan_skin_sync(&current, &same), SkinSyncPlan::default());
        assert_eq!(
            plan_skin_sync(
                &HashMap::<ObjectHandle, SkinnedObject<u32, u32>>::new(),
                &same
            ),
            SkinSyncPlan {
                release: Vec::new(),
                create: vec![hero],
            },
            "новый аниматор получает экземпляры"
        );
    }

    #[test]
    fn skin_sync_recreates_instances_when_the_model_or_its_parts_change() {
        let world = skin_world();
        let hero = world.handle("hero").unwrap();
        let current = HashMap::from([(hero, skin("models/hero.fbx", &[1, 2]))]);
        let other_model = plan_skin_sync(&current, &wanted(&[(hero, "models/other.fbx", &[1, 2])]));
        assert_eq!(
            other_model,
            SkinSyncPlan {
                release: vec![hero],
                create: vec![hero],
            }
        );
        let other_parts = plan_skin_sync(&current, &wanted(&[(hero, "models/hero.fbx", &[1, 3])]));
        assert_eq!(other_parts, other_model);
    }

    #[test]
    fn skin_sync_recreates_instances_when_a_lod_level_model_changes() {
        let world = skin_world();
        let hero = world.handle("hero").unwrap();
        let mut current_skin = skin("models/hero.fbx", &[1]);
        current_skin.lods = vec![
            Some(LodSkin {
                model: "models/hero_lod.fbx".to_owned(),
                mesh_ids: vec![5],
                instances: vec![105],
                remap: None,
            }),
            None,
        ];
        let current = HashMap::from([(hero, current_skin)]);
        let wanted_lods = |lods: Vec<Option<(&str, Vec<u32>)>>| {
            HashMap::from([(
                hero,
                (
                    "models/hero.fbx".to_owned(),
                    vec![1],
                    lods.into_iter()
                        .map(|lod| lod.map(|(model, meshes)| (model.to_owned(), meshes)))
                        .collect::<Vec<_>>(),
                ),
            )])
        };
        let same = wanted_lods(vec![Some(("models/hero_lod.fbx", vec![5])), None]);
        assert_eq!(plan_skin_sync(&current, &same), SkinSyncPlan::default());
        let recreate = SkinSyncPlan {
            release: vec![hero],
            create: vec![hero],
        };
        for changed in [
            wanted_lods(vec![Some(("models/hero_lod.fbx", vec![6])), None]),
            wanted_lods(vec![
                Some(("models/hero_lod.fbx", vec![5])),
                Some(("models/far.fbx", vec![7])),
            ]),
            wanted_lods(vec![Some(("models/hero_lod.fbx", vec![5]))]),
        ] {
            assert_eq!(plan_skin_sync(&current, &changed), recreate);
        }
    }

    #[test]
    fn removed_animator_releases_only_its_instances_and_new_ones_follow_document_order() {
        let world = skin_world();
        let hero = world.handle("hero").unwrap();
        let guard = world.handle("guard").unwrap();
        let current = HashMap::from([
            (hero, skin("models/hero.fbx", &[1])),
            (guard, skin("models/hero.fbx", &[1])),
        ]);
        let only_guard = wanted(&[(guard, "models/hero.fbx", &[1])]);
        assert_eq!(
            plan_skin_sync(&current, &only_guard),
            SkinSyncPlan {
                release: vec![hero],
                create: Vec::new(),
            },
            "у guard экземпляры остаются"
        );
        let both = wanted(&[
            (guard, "models/hero.fbx", &[1]),
            (hero, "models/hero.fbx", &[1]),
        ]);
        assert_eq!(
            plan_skin_sync(
                &HashMap::<ObjectHandle, SkinnedObject<u32, u32>>::new(),
                &both
            )
            .create,
            vec![hero, guard],
            "порядок создания совпадает с порядком документа"
        );
    }

    #[test]
    fn palette_indices_follow_the_bridge_order_of_skinned_objects() {
        let world = skin_world();
        let hero = world.handle("hero").unwrap();
        let guard = world.handle("guard").unwrap();
        let bridge = AnimationBridge::build(
            &world,
            |_| {
                Ok(std::sync::Arc::new(
                    crate::viewport::animation::test_support::walk_model(),
                ))
            },
            |_| Ok(std::sync::Arc::new(Vec::new())),
        );
        let skins = HashMap::from([
            (guard, skin("models/hero.fbx", &[1])),
            (hero, skin("models/hero.fbx", &[1])),
        ]);
        assert_eq!(
            palette_indices(&skins, Some(&bridge)),
            HashMap::from([(hero, 0), (guard, 1)])
        );
        assert!(
            palette_indices(&skins, None).is_empty(),
            "без моста палитр нет"
        );
        let hero_only = HashMap::from([(hero, skin("models/hero.fbx", &[1]))]);
        assert_eq!(
            palette_indices(&hero_only, Some(&bridge)),
            HashMap::from([(hero, 0)])
        );
    }

    #[test]
    fn palette_of_another_joint_count_is_not_bound_and_is_reported() {
        // Регрессия: палитра моста старой модели (1 кость) не должна попадать в меш с 2 костями.
        let world = skin_world();
        let hero = world.handle("hero").unwrap();
        let bridge = AnimationBridge::build(
            &world,
            |_| {
                Ok(std::sync::Arc::new(
                    crate::viewport::animation::test_support::walk_model(),
                ))
            },
            |_| Ok(std::sync::Arc::new(Vec::new())),
        );
        let mut two_joints = skin("models/hero.fbx", &[1]);
        two_joints.joint_count = 2;
        let skins = HashMap::from([(hero, two_joints)]);
        assert!(palette_indices(&skins, Some(&bridge)).is_empty());
        assert_eq!(
            palette_mismatches(&skins, &bridge),
            vec![(hero, "models/hero.fbx".to_owned())]
        );
    }

    #[test]
    fn palette_mismatch_warns_once_and_again_after_the_palette_matched() {
        // Регрессия: после того как палитра снова совпала, новое несоответствие молчало.
        let world = skin_world();
        let hero = world.handle("hero").unwrap();
        let mismatch = vec![(hero, "models/hero.fbx".to_owned())];
        let mut reported = HashSet::new();
        assert_eq!(mismatch_warnings(&mut reported, mismatch.clone()).len(), 1);
        assert!(
            mismatch_warnings(&mut reported, mismatch.clone()).is_empty(),
            "пока несоответствие держится, предупреждение не повторяется"
        );
        assert!(
            mismatch_warnings(&mut reported, Vec::new()).is_empty(),
            "палитра совпала"
        );
        assert_eq!(
            mismatch_warnings(&mut reported, mismatch).len(),
            1,
            "новое несоответствие выводится снова"
        );
    }

    #[test]
    fn failed_particle_replacement_keeps_previous_entry() {
        let mut entries = HashMap::from([(1u32, "old")]);
        let result: Result<Option<&str>, &str> =
            replace_entry(&mut entries, 1, || Err("create failed"));
        assert_eq!(result, Err("create failed"));
        assert_eq!(entries.get(&1), Some(&"old"));
    }

    #[test]
    fn successful_particle_replacement_returns_previous_for_destruction() {
        let mut entries = HashMap::from([(1u32, "old")]);
        let previous = replace_entry::<_, _, ()>(&mut entries, 1, || Ok("new")).unwrap();
        assert_eq!(previous, Some("old"));
        assert_eq!(entries.get(&1), Some(&"new"));
    }

    #[test]
    fn first_particle_creation_has_no_previous_entry() {
        let mut entries = HashMap::new();
        let previous = replace_entry::<u32, &str, ()>(&mut entries, 7, || Ok("new")).unwrap();
        assert_eq!(previous, None);
        assert_eq!(entries.get(&7), Some(&"new"));
    }
}
