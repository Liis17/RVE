//! Подготовка кадра сцены: GPU-кэш ресурсов и команды отрисовки.

use crate::asset_watch::AssetChanges;
use crate::console::ConsoleLog;
use crate::project::OpenProject;
use crate::viewport::animation::{
    AnimationPreview, EditorAnimation, PreviewStatus, project_preview_bridge,
};
use crate::viewport::model_loader::ModelLoader;
use engine_animation::AnimationBridge;
use engine_assets::{Assets, Model, ModelAlphaMode};
use engine_files::VirtualPath;
use engine_render::AlphaMode;
use engine_render::{
    AntiAliasing as RenderAntiAliasing, DebugView, DirectionalLight, DrawCommand, EnvironmentId,
    EnvironmentSource, Exposure, Frame, FrameCamera, FrameEnvironment, LocalLight,
    LocalLightShadow, LocalLightShadowMatrices, Material, MaterialId, MeshId, ParticleBlend,
    ParticleBurst, ParticleCollision, ParticleColorKey, ParticleEmitterDesc, ParticleEmitterFrame,
    ParticleEmitterId, ParticleFlipbook, ParticleShape, ParticleSizeKey, PostProcess,
    ProceduralSky, RenderError, Renderer, SHADOW_MAP_RESOLUTION, SkinBinding, SkinId, SkinVertex,
    TextureColorSpace, TextureId, Tonemapper as RenderTonemapper, Vertex, ViewSettings, ViewportId,
};
use engine_scene::PostProcessSettings;
use engine_scene::camera::Camera;
use engine_scene::document::{LodGroup, MaterialOverride, MeshRenderer};
use engine_scene::light::{
    cascade_view_projections, point_shadow_view_projections, spot_shadow_view_projection,
};
use engine_scene::world::ObjectHandle;
use engine_scene::{
    AntiAliasing, CameraSettings, EnvironmentState, LightState, LocalLightState, ParticleBlendMode,
    ParticleEmitterComponent, ParticleShapeComponent, SceneFrame, Tonemapper, World,
};
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
}

/// Экземпляры скиннинга объекта с `animator`: путь модели, её меши и по экземпляру на меш.
/// Типы параметризованы, чтобы планирование синхронизации тестировалось без GPU.
struct SkinnedObject<M = MeshId, S = SkinId> {
    model: String,
    joint_count: u32,
    mesh_ids: Vec<M>,
    instances: Vec<S>,
}

/// GPU-ресурсы вьюпорта. Модели и текстуры кэшируются по virtual path; ошибка загрузки
/// запоминается и пишется в консоль один раз, пока кэш не будет сброшен.
pub struct SceneRenderer {
    pub viewport: ViewportId,
    pub game_viewport: ViewportId,
    models: HashMap<String, Option<GpuModel>>,
    model_dependencies: HashMap<String, Vec<VirtualPath>>,
    failed_reloads: HashSet<String>,
    /// Поколение каждой модели на момент последней загрузки или ошибки; мост превью
    /// пересобирается только при смене поколения модели аниматора.
    model_versions: HashMap<String, u64>,
    textures: HashMap<(String, TextureColorSpace), Option<TextureId>>,
    environment_sources: HashMap<String, Option<Arc<EnvironmentSource>>>,
    current_environment: Option<(EditorEnvironmentKey, EnvironmentId)>,
    failed_environment: Option<EditorEnvironmentKey>,
    object_materials: HashMap<(ObjectHandle, String, usize, usize), (Material, MaterialId)>,
    grid: Option<MeshId>,
    grid_material: Option<MaterialId>,
    scene_draws: Vec<DrawCommand>,
    game_draws: Vec<DrawCommand>,
    motion_history: MotionHistory,
    /// Скиннинг аниматоров сцены по объектам.
    skins: HashMap<ObjectHandle, SkinnedObject>,
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
            models: HashMap::new(),
            model_dependencies: HashMap::new(),
            failed_reloads: HashSet::new(),
            model_versions: HashMap::new(),
            textures: HashMap::new(),
            particle_emitters: HashMap::new(),
            particle_frames: Vec::new(),
            particle_step: 0.0,
            particle_clock: None,
            particles_playing: None,
            playing: false,
            environment_sources: HashMap::new(),
            current_environment: None,
            failed_environment: None,
            object_materials: HashMap::new(),
            grid: None,
            grid_material: None,
            scene_draws: Vec::new(),
            game_draws: Vec::new(),
            motion_history: MotionHistory::default(),
            skins: HashMap::new(),
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
        self.environment_sources.clear();
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
        for texture in self.textures.drain().filter_map(|(_, texture)| texture) {
            renderer.destroy_texture(texture)?;
        }
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
                if !self.environment_sources.contains_key(path) {
                    let source = match project.assets.load_hdr(path) {
                        Ok(image) => Some(Arc::new(EnvironmentSource::Equirect {
                            width: image.width,
                            height: image.height,
                            rgb: image.rgb,
                        })),
                        Err(error) => {
                            log.error(format!("{path}: {error}"));
                            None
                        }
                    };
                    self.environment_sources.insert(path.to_owned(), source);
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

    /// Перезагружает только ресурсы, совпавшие с изменёнными путями или зависимостями.
    pub fn reload_changed(
        &mut self,
        renderer: &mut Renderer,
        project: &OpenProject,
        changes: &AssetChanges,
        loader: &mut ModelLoader,
        log: &mut ConsoleLog,
    ) -> Result<(), RenderError> {
        let texture_keys = self
            .textures
            .keys()
            .filter(|(path, _)| key_matches_changes(changes, path))
            .cloned()
            .collect::<Vec<_>>();
        for (path, color_space) in texture_keys {
            let key = (path.clone(), color_space);
            let previous = self.textures.get(&key).copied().flatten();
            let had_previous = previous.is_some();
            match project.assets.load_texture(&path) {
                Ok(data) => {
                    let replacement = match renderer.create_texture_with_color_space(
                        data.width,
                        data.height,
                        &data.rgba,
                        color_space,
                    ) {
                        Ok(replacement) => replacement,
                        Err(error) if is_resource_error(&error) => {
                            let (current, _, error) =
                                cache_update(previous, Err(error.to_string()));
                            self.textures.insert(key, current);
                            if let Some(error) = error {
                                let message = if had_previous {
                                    format!("{path}: {error}; сохранена прежняя версия текстуры")
                                } else {
                                    format!("{path}: {error}")
                                };
                                log.error(message);
                            }
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let (current, old, _) = cache_update(previous, Ok(replacement));
                    self.textures.insert(key, current);
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
            let previous = self.environment_sources.get(&path).cloned().flatten();
            let had_previous = previous.is_some();
            match project.assets.load_hdr(&path) {
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
                                let (current, _, _) =
                                    cache_update(previous, Err(error.to_string()));
                                self.environment_sources.insert(path.clone(), current);
                                if transition == EnvironmentTransition::Keep {
                                    log.error(format!(
                                        "{path}: {error}; сохранено прежнее окружение"
                                    ));
                                } else {
                                    log.error(format!("{path}: {error}"));
                                    if let Some((_, previous)) = self.current_environment.take() {
                                        renderer.destroy_environment(previous)?;
                                    }
                                }
                                self.failed_environment = Some(key);
                                continue;
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
        let gpu = match project.assets.load_texture(path) {
            Ok(data) => match renderer.create_texture_with_color_space(
                data.width,
                data.height,
                &data.rgba,
                color_space,
            ) {
                Ok(texture) => Some(texture),
                Err(error) if is_resource_error(&error) => {
                    log.error(format!("{path}: {error}"));
                    None
                }
                Err(error) => return Err(error),
            },
            Err(error) => {
                log.error(format!("{path}: {error}"));
                None
            }
        };
        self.textures.insert(key, gpu);
        Ok(())
    }

    /// Строит отдельные draw lists для Scene и Game; сетка добавляется только в Scene.
    #[allow(clippy::too_many_arguments)]
    pub fn build_draws(
        &mut self,
        renderer: &mut Renderer,
        world: &World,
        frame: &SceneFrame,
        scene_eye: Vec3,
        game_eye: Vec3,
        grid_camera_distance: Option<f32>,
        play_bridge: Option<&AnimationBridge>,
    ) -> Result<(), RenderError> {
        self.scene_draws.clear();
        self.game_draws.clear();
        self.sync_particles(renderer, world, frame)?;
        let palette_index = palette_indices(&self.skins, play_bridge.or(self.animation.bridge()));
        let used_object_materials = {
            let mut resources = DrawResources {
                models: &self.models,
                textures: &self.textures,
                object_materials: &mut self.object_materials,
                used_object_materials: HashSet::new(),
                skins: &self.skins,
                palette_index: &palette_index,
            };
            append_world_draws(
                renderer,
                world,
                frame,
                scene_eye,
                &mut resources,
                &mut self.motion_history,
                &mut self.scene_draws,
            )?;
            append_world_draws(
                renderer,
                world,
                frame,
                game_eye,
                &mut resources,
                &mut self.motion_history,
                &mut self.game_draws,
            )?;
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
                    let material = renderer.create_material(&Material {
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
        let mut wanted: HashMap<ObjectHandle, (String, Vec<MeshId>)> = HashMap::new();
        for object in world.objects() {
            let (Some(_), Some(mesh)) = (&object.animator, &object.mesh_renderer) else {
                continue;
            };
            let path = mesh.model.as_str();
            let Some(Some(model)) = self.models.get(path) else {
                continue;
            };
            if !model.skinned {
                continue;
            }
            let mesh_ids = model.meshes.iter().map(|(id, _, _)| *id).collect();
            wanted.insert(object.handle, (path.to_owned(), mesh_ids));
        }
        let plan = plan_skin_sync(&self.skins, &wanted);
        for object in plan.release {
            if let Some(skin) = self.skins.remove(&object) {
                release_skin(renderer, skin)?;
            }
        }
        for object in plan.create {
            let Some((model, mesh_ids)) = wanted.remove(&object) else {
                continue;
            };
            let joint_count = self
                .models
                .get(&model)
                .and_then(Option::as_ref)
                .map_or(0, |gpu| gpu.joint_count);
            let skin = create_skin(renderer, model, joint_count, mesh_ids)?;
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
        post: PostProcessSettings,
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
                post: render_post_process(&post, with_grid),
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

    /// Создаёт, обновляет и удаляет GPU-эмиттеры по компонентам сцены.
    ///
    /// Шаг симуляции — wall-clock время между кадрами. Симулирует вид, который показывает
    /// игру: Scene в режиме редактирования и Game во время Play. Переключение Play перезапускает эмиттеры.
    fn sync_particles(
        &mut self,
        renderer: &mut Renderer,
        world: &World,
        frame: &SceneFrame,
    ) -> Result<(), RenderError> {
        // Старые ссылки на эмиттеры могут уйти на замену; при ошибке кадр не должен их использовать.
        self.particle_frames.clear();
        let now = Instant::now();
        self.particle_step = self
            .particle_clock
            .map_or(0.0, |previous| (now - previous).as_secs_f32());
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
                    emitting: true,
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
    object_materials: &'a mut HashMap<(ObjectHandle, String, usize, usize), (Material, MaterialId)>,
    used_object_materials: HashSet<(ObjectHandle, String, usize, usize)>,
    /// Экземпляры скиннинга и индексы палитр кадра по объекту.
    skins: &'a HashMap<ObjectHandle, SkinnedObject>,
    palette_index: &'a HashMap<ObjectHandle, u32>,
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

fn append_world_draws(
    renderer: &mut Renderer,
    world: &World,
    frame: &SceneFrame,
    eye: Vec3,
    resources: &mut DrawResources<'_>,
    motion_history: &mut MotionHistory,
    draws: &mut Vec<DrawCommand>,
) -> Result<(), RenderError> {
    for instance in &frame.meshes {
        let Some(object) = world.object(instance.object) else {
            continue;
        };
        let Some(mesh) = &object.mesh_renderer else {
            continue;
        };
        let Some(level) = world.lod_level(instance.object, eye) else {
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
            let resolved = resolve_material(
                *base_material,
                replacement,
                override_material,
                mr_texture,
                normal_texture,
                emissive_texture,
                occlusion_texture,
            );
            let material_id = if resolved == *base_material {
                *base_material_id
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
            // Скиннинг применяется только к уровню 0: у аниматора LOD-группы нет.
            let skin = (level == 0)
                .then(|| {
                    let object = resources.skins.get(&instance.object)?;
                    let instance_id = *object.instances.get(mesh_index)?;
                    let palette = *resources.palette_index.get(&instance.object)?;
                    Some(SkinBinding {
                        instance: instance_id,
                        palette,
                    })
                })
                .flatten();
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
    Ok(())
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

/// Создаёт по экземпляру скиннинга на каждую часть модели; при ошибке уже созданные освобождаются.
fn create_skin(
    renderer: &mut Renderer,
    model: String,
    joint_count: u32,
    mesh_ids: Vec<MeshId>,
) -> Result<SkinnedObject, RenderError> {
    let mut skin = SkinnedObject {
        model,
        joint_count,
        mesh_ids,
        instances: Vec::new(),
    };
    for index in 0..skin.mesh_ids.len() {
        match renderer.create_skin_instance(skin.mesh_ids[index]) {
            Ok(instance) => skin.instances.push(instance),
            Err(error) => {
                release_skin(renderer, skin)?;
                return Err(error);
            }
        }
    }
    Ok(skin)
}

fn release_skin(renderer: &mut Renderer, skin: SkinnedObject) -> Result<(), RenderError> {
    for instance in skin.instances {
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
    wanted: &HashMap<ObjectHandle, (String, Vec<M>)>,
) -> SkinSyncPlan {
    let mut release: Vec<ObjectHandle> = current
        .iter()
        .filter(|(object, skin)| {
            wanted
                .get(*object)
                .is_none_or(|(model, mesh_ids)| *model != skin.model || *mesh_ids != skin.mesh_ids)
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
fn render_post_process(post: &PostProcessSettings, scene_view: bool) -> PostProcess {
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
    let grading = post.color_grading;
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

#[cfg(test)]
mod tests {
    use super::*;
    use engine_files::VirtualPath;
    use engine_scene::{EnvironmentComponent, LodLevel, ProceduralSkyComponent};

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
        }
    }

    fn wanted(
        entries: &[(ObjectHandle, &str, &[u32])],
    ) -> HashMap<ObjectHandle, (String, Vec<u32>)> {
        entries
            .iter()
            .map(|(object, model, meshes)| (*object, ((*model).to_owned(), meshes.to_vec())))
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
