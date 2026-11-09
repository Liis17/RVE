#[cfg(feature = "dev-console")]
use crate::console::ConsoleUiSlot;
#[cfg(feature = "dev-console")]
use crate::console::{CommandCatalogSnapshot, ConsoleUiFrame};
use crate::cursor::CursorControl;
use crate::input::{InputAccumulator, lock_input};
use crate::module_host::{ModuleHost, panic_message};
use crate::net::{self, NetworkSession, ScriptNetwork};
use crate::prefabs::{
    LoadedPrefab, LoaderRequest, PREFAB_UPLOAD_BUDGET, ParticleRequest, PrefabCpu, PrefabGpu,
    MaterialLevels, MaterialReply, MaterialRequest, PrefabLoads, PrefabReady, PrefabReply,
    PrefabToken, RenderResourceRequest,
};
use crate::profiling::{
    PhaseWindow, SOURCE_RENDER, SOURCE_RESOURCES, SOURCE_SCRIPTS, SOURCE_STARTUP,
    log_startup_profile, record_frame_profile, record_update_profile, render_groups, render_phase,
    script_phase, scripts_groups,
};
use crate::scene::{
    MaterialUploadCaches, ObjectModel, ParticleSource, PreparedPrefabs, PreparedRenderAssets,
    particle_emitter_desc, upload_object_levels,
};
use engine_animation::{AnimationBridge, AnimationEvent};
use engine_assets::{Model, ModelAnimation, TextureData};
#[cfg(feature = "dev-console")]
use engine_console::CommandLine;
use engine_core::profile::ProfileBoard;
use engine_network::SpawnedRoot;
use engine_physics::{
    BodyChange, BodyHandle, ModelGeometry, PhysicsClient, PhysicsCommand, PhysicsInitialization,
    PhysicsReceive, PhysicsSceneSync, PhysicsStepResult, ShapeCache,
};
use engine_render::{
    AntiAliasing as RenderAntiAliasing, DebugView, DirectionalLight, DrawCommand, EnvironmentId,
    EnvironmentSource, Exposure, Frame, FrameCamera, FrameEnvironment, FrameOutcome, FrameProfile,
    FrameStatus, LatencyHandle, LatencyMarker, LocalLight, LocalLightShadow,
    LocalLightShadowMatrices, MaterialId, MeshId, ParticleEmitterDesc, ParticleEmitterFrame,
    ParticleEmitterId, PostProcess, ProceduralSky, RenderError, Renderer, SHADOW_MAP_RESOLUTION,
    TextureColorSpace, TextureId, Tonemapper as RenderTonemapper, ViewSettings,
};
use engine_render::{SkinBinding, SkinId};
#[cfg(feature = "dev-console")]
use engine_render::{UiImage, UiPrimitive, UiVertex, ViewportId};
use engine_resources::ResourceHandle;
use engine_scene::camera::Camera;
use engine_scene::light::{
    cascade_view_projections, point_shadow_view_projections, spot_shadow_view_projection,
};
use engine_scene::world::ParticleCommand;
use engine_scene::{
    AntiAliasing, AudioRequest, CameraSettings, EnvironmentState, LightState, LocalLightState,
    ObjectHandle, PostProcessSettings, PrefabTemplate, Tonemapper, VoiceId, World,
    activity_audio_requests,
};
#[cfg(feature = "dev-console")]
use engine_script::CommandError;
use engine_script::{
    PrefabRequest, ScriptCursor, ScriptEvent, ScriptEventLevel, ScriptHost, ScriptInterruptHandle,
    ScriptLibrary, ScriptLoadOptions, ScriptTime, SpawnedObject, WorldChanges,
};
#[cfg(feature = "dev-console")]
use engine_ui::render::UiBridge;
use flume::{Receiver, RecvTimeoutError, Selector, Sender, TryRecvError};
use glam::{Mat3, Vec3};
use std::collections::VecDeque;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

pub(crate) const SCRIPTS_THREAD: &str = "rve-scripts";
pub(crate) const RENDER_THREAD: &str = "rve-render";
pub(crate) const SCRIPT_CALL_TIME_LIMIT: Duration = Duration::from_secs(5);
/// Встроенная команда профайлера: имеет приоритет над одноимённой командой скрипта.
#[cfg(feature = "dev-console")]
pub(crate) const PROFILE_COMMAND: &str = "profile";
#[cfg(feature = "dev-console")]
const PROFILE_COMMAND_DESCRIPTION: &str = "показать CPU-фазы, GPU-проходы и замеры старта";

#[derive(Clone, Debug)]
pub(crate) struct FrameSnapshot {
    /// Номер снимка в цикле скриптов; растёт, пока есть отправки. По нему render thread понимает,
    /// какие снимки ещё ссылаются на освобождаемый префаб.
    pub sequence: u64,
    pub camera: Camera,
    pub active_camera: ObjectHandle,
    pub view: CameraSettings,
    /// Постобработка активной камеры, уже разрешённая из `.postprocess` или `[color]`.
    pub post_process: PostProcessSettings,
    pub reset_history: bool,
    pub light: LightState,
    pub local_lights: Vec<LocalLight>,
    pub local_light_shadows: Vec<LocalLightShadow>,
    pub environment: Option<SceneEnvironmentSnapshot>,
    pub draws: Vec<DrawCommand>,
    /// Палитры skinning кадра; draw ссылается на них по индексу в `SkinBinding::palette`.
    pub joint_palettes: Vec<Vec<[[f32; 4]; 4]>>,
    pub particles: Vec<ParticleSnapshot>,
    /// Команды эмиттеров, поставленные скриптами за кадр: перезапуск и выброс.
    pub particle_commands: Vec<engine_scene::world::ParticleCommand>,
    /// Игровое время скриптового потока на момент снимка; основа шага симуляции частиц.
    pub sim_time: f64,
    /// Экранные шейдеры вида: включённые проходы камеры и время сцены.
    pub screen_shaders: engine_render::ScreenShaders,
    object_motion: HashMap<ObjectHandle, ObjectMotion>,
}

#[derive(Clone, Debug)]
struct ObjectMotion {
    model: [[f32; 4]; 4],
    draw_range: std::ops::Range<usize>,
}

#[derive(Clone, Debug)]
pub(crate) struct SceneEnvironmentSnapshot {
    pub key: EnvironmentCacheKey,
    pub source: EnvironmentSnapshotSource,
    pub intensity: f32,
    pub rotation: [[f32; 3]; 3],
    pub skybox: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum EnvironmentSnapshotSource {
    Hdr(Arc<EnvironmentSource>),
    Procedural(ProceduralSky),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EnvironmentCacheKey {
    Hdr(String),
    Procedural {
        colors_and_sun: [u32; 11],
        sun_direction: [i32; 3],
    },
}

/// Эмиттер частиц в снимке: render thread берёт id по хендлу объекта, поэтому хендл, а не id.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ParticleSnapshot {
    pub object: ObjectHandle,
    pub transform: [[f32; 4]; 4],
    /// Рождаются ли частицы в этом кадре (`particles:play`/`stop` из скриптов).
    pub emitting: bool,
}

/// Уровни LOD объекта: каждый уровень — список пар «меш и материал».
type GpuLevels = Vec<Vec<(MeshId, MaterialId)>>;

#[derive(Clone, Debug, Default)]
pub(crate) struct GpuSceneTable {
    pub by_object: HashMap<ObjectHandle, GpuLevels>,
    /// Уровни шаблонов быстрых префабов по пути. Элемент `i` относится к объекту шаблона `i`.
    pub prefabs: HashMap<String, Vec<GpuLevels>>,
    /// Эмиттеры частиц объектов сцены; создаются при загрузке и передаются render thread.
    pub particles: HashMap<ObjectHandle, ParticleEmitterId>,
    /// Описания эмиттеров быстрых префабов по пути. Элемент `i` относится к объекту шаблона `i`.
    pub particle_templates: HashMap<String, Vec<Option<ParticleEmitterDesc>>>,
    pub environments: HashMap<String, Arc<EnvironmentSource>>,
    /// Скелеты и анимации моделей для моста анимации; вершины сюда не входят.
    pub animation_models: HashMap<String, Arc<Model>>,
    /// Клипы `animator.clips` по виртуальному пути файла.
    pub animation_clips: HashMap<String, Arc<Vec<ModelAnimation>>>,
    /// Экземпляры skinning по объекту: по одному на часть меша уровня 0.
    pub skin_instances: HashMap<ObjectHandle, Vec<SkinId>>,
    /// Число костей модели объекта с экземплярами skinning; палитра моста должна иметь ту же длину.
    pub joint_counts: HashMap<ObjectHandle, u32>,
    /// Экранные шейдеры камер по пути `.glsl`; создаются при загрузке сцены и освобождаются с ней.
    pub screen_shaders: HashMap<String, engine_render::ScreenShaderId>,
}

impl GpuSceneTable {
    /// Все экземпляры skinning сцены; рендер освобождает их при завершении.
    pub(crate) fn skin_instance_ids(&self) -> Vec<SkinId> {
        self.skin_instances.values().flatten().copied().collect()
    }

    /// Экранные шейдеры сцены; рендер освобождает их при выгрузке.
    pub(crate) fn screen_shader_ids(&self) -> Vec<engine_render::ScreenShaderId> {
        self.screen_shaders.values().copied().collect()
    }
}

type FrameLighting = (
    DirectionalLight,
    Vec<[[f32; 4]; 4]>,
    Option<FrameEnvironment>,
);

/// Вызовы NVIDIA Reflex из скриптового потока; реализация — `engine_render::LatencyHandle`.
pub(crate) trait FrameLatency: Send + Sync {
    /// Ждёт разрешения Reflex на начало симуляции кадра.
    fn sleep(&self);
    fn marker(&self, frame_id: u64, marker: LatencyMarker);
}

impl FrameLatency for LatencyHandle {
    fn sleep(&self) {
        LatencyHandle::sleep(self);
    }

    fn marker(&self, frame_id: u64, marker: LatencyMarker) {
        LatencyHandle::marker(self, frame_id, marker);
    }
}

impl std::fmt::Debug for dyn FrameLatency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameLatency")
    }
}

/// Ответ render thread, который scripts thread принимает в очередь ответов до `update`.
pub(crate) enum ScriptReply {
    Prefab(PrefabReply),
    Material(Box<MaterialReply>),
}

#[derive(Debug)]
pub(crate) enum ScriptControl {
    /// Reflex-handle renderer; приходит один раз перед первым `Go`.
    Latency(Arc<dyn FrameLatency>),
    Go(Box<GpuSceneTable>),
    Background(bool),
    #[cfg(feature = "dev-console")]
    Console(CommandLine),
    /// Ответ загрузчика или render thread на загрузку тяжёлого префаба.
    Prefab(PrefabReply),
    /// Ответ render thread на замену материала объекта скриптом.
    Material(Box<MaterialReply>),
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RenderControl {
    Resize(u32, u32),
    Occluded(bool),
    #[cfg(feature = "dev-console")]
    DebugView(DebugView),
    #[cfg(feature = "dev-console")]
    ConsoleUiChanged,
    Shutdown,
}

pub(crate) struct RuntimeStatus {
    exit_code: AtomicU8,
    render_stopped: AtomicBool,
    error: Mutex<Option<String>>,
    /// Отчёты профайлера: потоки публикуют, оверлей F3 и команда `profile` читают.
    pub(crate) profile: Arc<ProfileBoard>,
}

impl Default for RuntimeStatus {
    fn default() -> Self {
        Self {
            exit_code: AtomicU8::new(0),
            render_stopped: AtomicBool::new(false),
            error: Mutex::new(None),
            profile: Arc::new(ProfileBoard::new(&[
                SOURCE_SCRIPTS,
                SOURCE_RENDER,
                SOURCE_RESOURCES,
                SOURCE_STARTUP,
            ])),
        }
    }
}

impl RuntimeStatus {
    pub fn record_failure(&self, message: impl Into<String>) {
        self.exit_code.store(1, Ordering::Release);
        let message = message.into();
        if let Ok(mut error) = self.error.lock()
            && error.is_none()
        {
            *error = Some(message.clone());
        }
    }

    pub fn mark_render_stopped(&self) {
        self.render_stopped.store(true, Ordering::Release);
    }

    pub fn render_stopped(&self) -> bool {
        self.render_stopped.load(Ordering::Acquire)
    }

    pub fn exit_code(&self) -> u8 {
        self.exit_code.load(Ordering::Acquire)
    }

    pub fn failure_message(&self) -> Option<String> {
        self.error.lock().ok().and_then(|error| error.clone())
    }
}

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Duration;

    fn wait_for_control(
        &self,
        control_rx: &Receiver<ScriptControl>,
        timeout: Duration,
    ) -> Result<ScriptControl, RecvTimeoutError> {
        control_rx.recv_timeout(timeout)
    }
}

pub(crate) struct MonotonicClock {
    started: Instant,
}

impl MonotonicClock {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Clock for MonotonicClock {
    fn now(&self) -> Duration {
        self.started.elapsed()
    }
}

pub(crate) trait RenderBackend: Send {
    fn resize(&mut self, width: u32, height: u32);
    #[cfg(feature = "dev-console")]
    fn set_debug_view(&mut self, _debug_view: DebugView) {}
    fn render(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
    ) -> Result<FrameOutcome, String>;
    /// Забирает профиль последнего кадра renderer; у тестовых backend профиля нет.
    fn take_frame_profile(&mut self) -> Option<FrameProfile> {
        None
    }
    #[cfg(feature = "dev-console")]
    fn take_console_ui(&mut self) -> Option<ConsoleUiFrame> {
        None
    }
    #[cfg(feature = "dev-console")]
    fn discard_console_geometry(&mut self) {}
    #[cfg(feature = "dev-console")]
    fn render_console(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
        _ui: &mut ConsoleUiFrame,
    ) -> Result<FrameOutcome, String> {
        self.render(snapshot, width, height)
    }
    #[cfg(feature = "dev-console")]
    fn close_console(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn wait_idle(&mut self) {}
    /// Освобождает GPU-ресурсы сцены до уничтожения backend.
    fn release_scene(&mut self) {}
    /// Ставит загруженные на CPU ресурсы тяжёлого префаба в очередь GPU-загрузки.
    fn submit_prefab(&mut self, prefab: LoadedPrefab) -> Option<PrefabReply>;
    /// Выполняет до `budget` шагов GPU-загрузки и возвращает завершённые префабы.
    fn advance_prefab_uploads(&mut self, budget: usize) -> Vec<PrefabReply>;
    fn has_pending_prefab_uploads(&self) -> bool;
    /// Немедленно уничтожает GPU-ресурсы токена; вызывается, когда снимки ими не пользуются.
    fn release_prefab(&mut self, token: &PrefabToken);
    /// Применяет заявку эмиттера частиц; у тестовых backend эмиттеров нет.
    fn apply_particle_request(&mut self, _request: ParticleRequest) {}
    /// Применяет команды частиц перед отрисовкой кадра. Вызывается render thread ровно один раз
    /// для каждой команды, даже если тот же снимок перерисовывается.
    fn apply_particle_commands(&mut self, _commands: &[engine_scene::world::ParticleCommand]) {}
    /// Создаёт GPU-материалы объекта по заявке скрипта. Без GPU-ресурсов сцены отвечает ошибкой.
    fn submit_material(&mut self, request: MaterialRequest) -> MaterialReply {
        MaterialReply {
            object: request.object,
            revision: request.revision,
            result: Err("замена материала не поддерживается этим backend".to_owned()),
        }
    }
    /// Уничтожает материалы, которыми уже не пользуются снимки; вызывается после их `last_seq`.
    fn release_materials(&mut self, _materials: &[MaterialId]) {}
}

pub(crate) trait ExitRequester: Send + Sync {
    fn request_exit(&self);
}

pub(crate) struct ScriptThreadConfig {
    pub scene_name: String,
    pub sources: std::collections::BTreeMap<String, String>,
    pub input_settings: engine_project::InputSettings,
    pub scripts_library: Option<PathBuf>,
    #[cfg(test)]
    pub script_module: Option<&'static rve_sdk::abi::RveModule>,
    pub interrupt: ScriptInterruptHandle,
    pub clock: Arc<dyn Clock>,
    pub background_tick_hz: u32,
    /// Открывать ли геймпады (`gilrs`). `false` для выделенного сервера без окна: он не читает устройства ввода.
    pub gamepad_devices: bool,
    pub control_tx: Sender<ScriptControl>,
    pub control_rx: Receiver<ScriptControl>,
    pub frame_tx: Sender<Arc<FrameSnapshot>>,
    pub render_tx: Sender<RenderControl>,
    pub status: Arc<RuntimeStatus>,
    pub input: Arc<Mutex<InputAccumulator>>,
    pub cursor: CursorControl,
    pub physics_client: Option<PhysicsClient>,
    pub physics_initialization: Option<PhysicsInitialization>,
    /// Sound subsystem handle; `None` disables audio for this scripts thread.
    pub audio: Option<engine_audio::AudioHandle>,
    /// Быстрые префабы сцены: шаблоны для регистрации в скриптах и лимиты создания.
    pub prefabs: PreparedPrefabs,
    /// Геометрия моделей для коллайдеров spawn-объектов.
    pub physics_geometry: Vec<ModelGeometry>,
    /// Заявки загрузчика тяжёлых префабов.
    pub prefab_loader: Sender<LoaderRequest>,
    /// Заявки render thread: выгрузка тяжёлых префабов.
    pub render_resources: Sender<RenderResourceRequest>,
    /// Сетевой поток и режим моста; `None` — offline, мост не подключается.
    pub network: Option<NetworkSession>,
    /// Число тиков, после которого цикл останавливается сам (`rve-server --frames`); `None` — без предела.
    pub frame_limit: Option<u64>,
    #[cfg(feature = "dev-console")]
    pub command_catalog: Arc<Mutex<CommandCatalogSnapshot>>,
}

pub(crate) struct ScriptThreadHandle {
    pub ready: mpsc::Receiver<Result<(), ScriptStartupError>>,
    pub stopped: mpsc::Receiver<()>,
}

pub(crate) struct RenderThreadConfig {
    pub backend: Box<dyn RenderBackend>,
    pub width: u32,
    pub height: u32,
    pub frame_limit: Option<u64>,
    pub frame_rx: Receiver<Arc<FrameSnapshot>>,
    /// Ресурсы тяжёлых префабов: загрузки от загрузчика и освобождения от скриптов.
    pub resources: Receiver<RenderResourceRequest>,
    pub control_tx: Sender<RenderControl>,
    pub control_rx: Receiver<RenderControl>,
    pub script_tx: Sender<ScriptControl>,
    pub interrupt: ScriptInterruptHandle,
    pub status: Arc<RuntimeStatus>,
    /// Источник статистики ресурсов для профайлера; `None` оставляет источник пустым.
    pub resource_stats: Option<ResourceHandle>,
    pub stopped_tx: Sender<()>,
    pub exit: Arc<dyn ExitRequester>,
}

#[derive(Debug)]
pub(crate) enum ScriptStartupError {
    Load(String),
    Physics(String),
    Network(String),
    Panic(String),
}

/// Ресурсы эмиттеров тяжёлого префаба на render thread.
#[derive(Default)]
struct PrefabParticles {
    textures: Vec<TextureId>,
    /// Отдаются с `PrefabReady` и затем обнуляются.
    descs: Option<Vec<Option<ParticleEmitterDesc>>>,
}

pub(crate) struct VulkanBackend {
    renderer: Renderer,
    /// GPU-ресурсы тяжёлых префабов; `Renderer` выполняет создание и освобождение.
    prefabs: PrefabGpu<Renderer>,
    debug_view: DebugView,
    current_environment: Option<(EnvironmentCacheKey, EnvironmentId)>,
    pipeline_cache_path: Option<std::path::PathBuf>,
    /// Игровое время последнего отправленного кадра: шаг частиц считается от него.
    last_submitted_sim_time: Option<f64>,
    /// Эмиттеры частиц по хендлам объектов: сцены при загрузке, spawn-объекты по заявкам.
    particle_emitters: HashMap<ObjectHandle, ParticleEmitterId>,
    /// Текстуры тяжёлых префабов: описания ждут готовности, текстуры живут до освобождения.
    prefab_particles: HashMap<PrefabToken, PrefabParticles>,
    #[cfg(feature = "dev-console")]
    console_ui: Option<Arc<ConsoleUiSlot>>,
    #[cfg(feature = "dev-console")]
    ui_bridge: UiBridge,
    #[cfg(feature = "dev-console")]
    console_viewport: Option<(ViewportId, u32, u32)>,
    /// Экземпляры skinning сцены, которые освобождаются в `release_scene`.
    scene_skin_instances: Vec<SkinId>,
    scene_screen_shaders: Vec<engine_render::ScreenShaderId>,
    /// Подготовленные ресурсы сцены для замены материалов объектов; нет у тестовых сцен без GPU.
    material_assets: Option<Arc<PreparedRenderAssets>>,
    /// Индекс объектов `material_assets` по хендлу.
    material_objects: HashMap<ObjectHandle, usize>,
    /// Текстуры, шейдеры и запасной материал сцены, которые переиспользуют замены материалов.
    material_caches: MaterialUploadCaches,
}

#[cfg(feature = "dev-console")]
type PendingConsoleCommands = VecDeque<CommandLine>;
#[cfg(not(feature = "dev-console"))]
type PendingConsoleCommands = ();

impl VulkanBackend {
    pub fn new(
        renderer: Renderer,
        pipeline_cache_path: Option<std::path::PathBuf>,
        debug_view: DebugView,
    ) -> Self {
        Self {
            renderer,
            prefabs: PrefabGpu::default(),
            debug_view,
            current_environment: None,
            pipeline_cache_path,
            last_submitted_sim_time: None,
            particle_emitters: HashMap::new(),
            prefab_particles: HashMap::new(),
            #[cfg(feature = "dev-console")]
            console_ui: None,
            #[cfg(feature = "dev-console")]
            ui_bridge: UiBridge::default(),
            #[cfg(feature = "dev-console")]
            console_viewport: None,
            scene_skin_instances: Vec::new(),
            scene_screen_shaders: Vec::new(),
            material_assets: None,
            material_objects: HashMap::new(),
            material_caches: MaterialUploadCaches::default(),
        }
    }

    /// Передаёт ресурсы сцены и GPU-кэши, которые остались после загрузки: по ним создаются
    /// материалы при замене скриптом.
    pub fn with_material_uploads(
        mut self,
        assets: Arc<PreparedRenderAssets>,
        caches: MaterialUploadCaches,
    ) -> Self {
        self.material_objects = assets
            .objects
            .iter()
            .enumerate()
            .map(|(index, (handle, _))| (*handle, index))
            .collect();
        self.material_assets = Some(assets);
        self.material_caches = caches;
        self
    }

    /// Создаёт материалы объекта по текущему состоянию мира. Меши и уровни LOD не меняются; новые
    /// материалы принадлежат только этому ответу, и при ошибке созданные уничтожаются.
    fn upload_object_materials(
        &mut self,
        object: ObjectHandle,
        levels: Vec<Vec<(MeshId, MaterialId)>>,
        material: Option<engine_scene::MaterialOverride>,
        path: Option<String>,
    ) -> Result<MaterialLevels, String> {
        let assets = self
            .material_assets
            .clone()
            .ok_or("ресурсы сцены не переданы render thread")?;
        let index = *self.material_objects.get(&object).ok_or(
            "объект без CPU-модели сцены (префаб или созданный скриптом): материал не заменён",
        )?;
        let source = &assets.objects[index].1;
        let part_count = levels.first().map_or(0, Vec::len);
        let model = ObjectModel {
            levels: source.levels.clone(),
            material,
            material_slots: match path {
                Some(path) => vec![Some(path); part_count],
                None => source.material_slots.clone(),
            },
            animated: source.animated,
        };
        let gpu_models: HashMap<String, Vec<(MeshId, MaterialId)>> = source
            .levels
            .iter()
            .zip(&levels)
            .map(|((model_path, _), meshes)| (model_path.clone(), meshes.clone()))
            .collect();
        let mut material_cache = HashMap::new();
        let uploaded = upload_object_levels(
            &mut self.renderer,
            &assets,
            &gpu_models,
            &mut self.material_caches.textures,
            &mut self.material_caches.shaders,
            &mut material_cache,
            &mut self.material_caches.error_material,
            &model,
        );
        let created: Vec<MaterialId> = material_cache.into_values().collect();
        match uploaded {
            Ok(levels) => Ok(MaterialLevels { levels, created }),
            Err(error) => {
                self.destroy_materials(&created);
                Err(error.to_string())
            }
        }
    }

    fn destroy_materials(&mut self, materials: &[MaterialId]) {
        for &material in materials {
            if let Err(error) = self.renderer.destroy_material(material) {
                log::error!("материал не уничтожен: {error}");
            }
        }
    }

    /// Принимает эмиттеры объектов сцены, созданные при загрузке.
    pub fn with_particle_emitters(
        mut self,
        emitters: HashMap<ObjectHandle, ParticleEmitterId>,
    ) -> Self {
        self.particle_emitters = emitters;
        self
    }

    #[cfg(feature = "dev-console")]
    pub fn with_console(mut self, console_ui: Arc<ConsoleUiSlot>) -> Self {
        self.console_ui = Some(console_ui);
        self
    }

    pub fn with_scene_screen_shaders(
        mut self,
        shaders: Vec<engine_render::ScreenShaderId>,
    ) -> Self {
        self.scene_screen_shaders = shaders;
        self
    }

    pub fn with_scene_skin_instances(mut self, instances: Vec<SkinId>) -> Self {
        self.scene_skin_instances = instances;
        self
    }

    fn frame_lighting(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
    ) -> Result<FrameLighting, String> {
        let light = DirectionalLight {
            direction: snapshot.light.direction,
            color: snapshot.light.color,
            ambient: snapshot.light.ambient,
        };
        let shadow_cascades = snapshot.light.shadow.map_or_else(Vec::new, |settings| {
            cascade_view_projections(
                snapshot.camera,
                width.max(1) as f32 / height.max(1) as f32,
                Vec3::from_array(snapshot.light.direction),
                settings,
                SHADOW_MAP_RESOLUTION,
            )
            .into_iter()
            .map(|matrix| matrix.to_cols_array_2d())
            .collect()
        });
        let environment = match &snapshot.environment {
            Some(environment) => {
                let id = match &self.current_environment {
                    Some((key, id)) if key == &environment.key => *id,
                    _ => {
                        let id = match &environment.source {
                            EnvironmentSnapshotSource::Hdr(source) => {
                                self.renderer.create_environment(source)
                            }
                            EnvironmentSnapshotSource::Procedural(sky) => self
                                .renderer
                                .create_environment(&EnvironmentSource::Procedural(*sky)),
                        }
                        .map_err(|error| error.to_string())?;
                        if let Some((_, previous)) = self.current_environment.take() {
                            self.renderer
                                .destroy_environment(previous)
                                .map_err(|error| error.to_string())?;
                        }
                        self.current_environment = Some((environment.key.clone(), id));
                        id
                    }
                };
                Some(FrameEnvironment {
                    id,
                    intensity: environment.intensity,
                    rotation: environment.rotation,
                    skybox: environment.skybox,
                })
            }
            None => {
                if let Some((_, id)) = self.current_environment.take() {
                    self.renderer
                        .destroy_environment(id)
                        .map_err(|error| error.to_string())?;
                }
                None
            }
        };
        Ok((light, shadow_cascades, environment))
    }
}

impl RenderBackend for VulkanBackend {
    fn take_frame_profile(&mut self) -> Option<FrameProfile> {
        self.renderer.take_frame_profile()
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.renderer.resize(width, height);
    }

    fn submit_prefab(&mut self, mut prefab: LoadedPrefab) -> Option<PrefabReply> {
        let token = prefab.token.clone();
        let particles = std::mem::take(&mut prefab.particles);
        let created =
            match create_prefab_particles(&mut self.renderer, &prefab.textures, &particles) {
                Ok(created) => created,
                Err(error) => {
                    return Some(PrefabReply::Failed {
                        token,
                        error: error.to_string(),
                    });
                }
            };
        let reply = self.prefabs.submit(prefab);
        if reply.is_some() {
            release_prefab_particles(&mut self.renderer, created.textures);
        } else {
            self.prefab_particles.insert(token, created);
        }
        reply
    }

    fn advance_prefab_uploads(&mut self, budget: usize) -> Vec<PrefabReply> {
        let mut replies = self.prefabs.advance(&mut self.renderer, budget);
        for reply in &mut replies {
            match reply {
                PrefabReply::Ready(ready) => {
                    if let Some(entry) = self.prefab_particles.get_mut(&ready.token) {
                        ready.particles = entry.descs.take().unwrap_or_default();
                    }
                }
                PrefabReply::Failed { token, .. } => {
                    if let Some(entry) = self.prefab_particles.remove(token) {
                        release_prefab_particles(&mut self.renderer, entry.textures);
                    }
                }
            }
        }
        replies
    }

    fn has_pending_prefab_uploads(&self) -> bool {
        self.prefabs.has_pending()
    }

    fn release_prefab(&mut self, token: &PrefabToken) {
        self.prefabs.release(&mut self.renderer, token);
        if let Some(entry) = self.prefab_particles.remove(token) {
            release_prefab_particles(&mut self.renderer, entry.textures);
        }
    }

    fn submit_material(&mut self, request: MaterialRequest) -> MaterialReply {
        let MaterialRequest {
            object,
            revision,
            levels,
            material,
            path,
        } = request;
        let result = self.upload_object_materials(object, levels, material, path);
        MaterialReply {
            object,
            revision,
            result,
        }
    }

    fn release_materials(&mut self, materials: &[MaterialId]) {
        self.destroy_materials(materials);
    }

    fn apply_particle_commands(&mut self, commands: &[engine_scene::world::ParticleCommand]) {
        for (emitter, burst) in resolve_particle_commands(&self.particle_emitters, commands) {
            let outcome = match burst {
                None => self.renderer.restart_particle_emitter(emitter),
                Some(count) => self.renderer.emit_particle_burst(emitter, count),
            };
            if let Err(error) = outcome {
                log::error!("команда эмиттера частиц не применена: {error}");
            }
        }
    }

    fn apply_particle_request(&mut self, request: ParticleRequest) {
        apply_emitter_request(
            &mut self.particle_emitters,
            &mut self.renderer,
            request,
            |renderer, desc| {
                renderer
                    .create_particle_emitter(desc)
                    .map_err(|error| error.to_string())
            },
            |renderer, id| {
                if let Err(error) = renderer.destroy_particle_emitter(id) {
                    log::error!("эмиттер частиц не уничтожен: {error}");
                }
            },
        );
    }

    #[cfg(feature = "dev-console")]
    fn set_debug_view(&mut self, debug_view: DebugView) {
        self.debug_view = debug_view;
    }

    fn render(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
    ) -> Result<FrameOutcome, String> {
        self.renderer.set_latency_frame(snapshot.sequence);
        let (light, shadow_cascades, environment) = self.frame_lighting(snapshot, width, height)?;
        let particle_step = particle_step(snapshot.sim_time, self.last_submitted_sim_time);
        let particles: Vec<ParticleEmitterFrame> = snapshot
            .particles
            .iter()
            .filter_map(|instance| {
                Some(ParticleEmitterFrame {
                    emitter: *self.particle_emitters.get(&instance.object)?,
                    transform: instance.transform,
                    emitting: instance.emitting,
                })
            })
            .collect();
        let joint_palettes = palette_refs(snapshot);
        let frame = Frame {
            joint_palettes: &joint_palettes,
            clear_color: [
                snapshot.view.render.clear_color[0],
                snapshot.view.render.clear_color[1],
                snapshot.view.render.clear_color[2],
                1.0,
            ],
            camera: FrameCamera {
                view: snapshot.camera.view().to_cols_array_2d(),
                projection: snapshot.camera.projection(width, height).to_cols_array_2d(),
                position: snapshot.camera.position.to_array(),
            },
            environment,
            lights: &snapshot.local_lights,
            local_light_shadows: &snapshot.local_light_shadows,
            shadow_cascades: &shadow_cascades,
            light,
            draws: &snapshot.draws,
            overlay: &[],
            debug_view: self.debug_view,
            view: ViewSettings {
                render_scale: snapshot.view.render.render_scale,
                post: render_post_process(&snapshot.post_process),
                screen_shaders: snapshot.screen_shaders,
                anti_aliasing: match snapshot.view.render.anti_aliasing {
                    AntiAliasing::Fxaa => RenderAntiAliasing::Fxaa,
                    AntiAliasing::None => RenderAntiAliasing::None,
                    AntiAliasing::Taa => RenderAntiAliasing::Taa,
                    AntiAliasing::Fsr => RenderAntiAliasing::Fsr,
                    AntiAliasing::Dlss => RenderAntiAliasing::Dlss,
                },
            },
            reset_history: snapshot.reset_history,
            particles: &particles,
            particle_step,
        };
        let outcome = self
            .renderer
            .render_frame_with_outcome(&frame)
            .map_err(|error| error.to_string())?;
        // Состояние частиц считается продвинутым только для отправленного кадра.
        if outcome.submitted {
            self.last_submitted_sim_time = Some(snapshot.sim_time);
        }
        Ok(outcome)
    }

    fn release_scene(&mut self) {
        for instance in self.scene_skin_instances.drain(..) {
            if let Err(error) = self.renderer.destroy_skin_instance(instance) {
                eprintln!("failed to release skin instance: {error}");
            }
        }
        for shader in self.scene_screen_shaders.drain(..) {
            if let Err(error) = self.renderer.destroy_screen_shader(shader) {
                eprintln!("failed to release screen shader: {error}");
            }
        }
    }

    fn wait_idle(&mut self) {
        match self.renderer.pipeline_cache_data() {
            Ok(data) => {
                engine_platform::save_pipeline_cache(self.pipeline_cache_path.as_deref(), &data)
            }
            Err(error) => eprintln!("failed to retrieve pipeline cache: {error}"),
        }
    }

    #[cfg(feature = "dev-console")]
    fn take_console_ui(&mut self) -> Option<ConsoleUiFrame> {
        self.console_ui.as_ref().and_then(|slot| slot.take())
    }

    #[cfg(feature = "dev-console")]
    fn discard_console_geometry(&mut self) {
        if let Some(slot) = &self.console_ui {
            slot.discard_geometry_preserve_texture_delta();
        }
    }

    #[cfg(feature = "dev-console")]
    fn render_console(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
        ui: &mut ConsoleUiFrame,
    ) -> Result<FrameOutcome, String> {
        if !ui.textures_delta.set.is_empty() {
            if let Err(error) = self
                .ui_bridge
                .managed
                .apply_updates(&ui.textures_delta, &mut self.renderer)
            {
                ui.textures_delta.clear();
                return Err(error.to_string());
            }
            ui.textures_delta.set.clear();
        }

        let viewport = match self.console_viewport {
            Some((viewport, old_width, old_height)) => {
                if old_width != width || old_height != height {
                    self.renderer
                        .resize_viewport(viewport, width, height)
                        .map_err(|error| error.to_string())?;
                    self.console_viewport = Some((viewport, width, height));
                }
                viewport
            }
            None => {
                let viewport = self
                    .renderer
                    .create_viewport(width, height)
                    .map_err(|error| error.to_string())?;
                self.console_viewport = Some((viewport, width, height));
                viewport
            }
        };

        let (light, shadow_cascades, environment) = self.frame_lighting(snapshot, width, height)?;
        let joint_palettes = palette_refs(snapshot);
        let scene = Frame {
            joint_palettes: &joint_palettes,
            clear_color: [
                snapshot.view.render.clear_color[0],
                snapshot.view.render.clear_color[1],
                snapshot.view.render.clear_color[2],
                1.0,
            ],
            camera: FrameCamera {
                view: snapshot.camera.view().to_cols_array_2d(),
                projection: snapshot.camera.projection(width, height).to_cols_array_2d(),
                position: snapshot.camera.position.to_array(),
            },
            environment,
            lights: &snapshot.local_lights,
            local_light_shadows: &snapshot.local_light_shadows,
            shadow_cascades: &shadow_cascades,
            light,
            draws: &snapshot.draws,
            overlay: &[],
            debug_view: self.debug_view,
            view: ViewSettings {
                render_scale: snapshot.view.render.render_scale,
                post: render_post_process(&snapshot.post_process),
                screen_shaders: snapshot.screen_shaders,
                anti_aliasing: match snapshot.view.render.anti_aliasing {
                    AntiAliasing::Fxaa => RenderAntiAliasing::Fxaa,
                    AntiAliasing::None => RenderAntiAliasing::None,
                    AntiAliasing::Taa => RenderAntiAliasing::Taa,
                    AntiAliasing::Fsr => RenderAntiAliasing::Fsr,
                    AntiAliasing::Dlss => RenderAntiAliasing::Dlss,
                },
            },
            reset_history: snapshot.reset_history,
            particles: &[],
            particle_step: 0.0,
        };
        let vertices = [
            UiVertex {
                position: [0.0, 0.0],
                uv: [0.0, 0.0],
                color: [255; 4],
            },
            UiVertex {
                position: [width as f32, 0.0],
                uv: [1.0, 0.0],
                color: [255; 4],
            },
            UiVertex {
                position: [width as f32, height as f32],
                uv: [1.0, 1.0],
                color: [255; 4],
            },
            UiVertex {
                position: [0.0, height as f32],
                uv: [0.0, 1.0],
                color: [255; 4],
            },
        ];
        let indices = [0, 1, 2, 0, 2, 3];
        let converted = self.ui_bridge.convert(&ui.primitives, ui.pixels_per_point);
        let mut primitives = Vec::with_capacity(converted.len() + 1);
        primitives.push(UiPrimitive {
            clip_rect: [0.0, 0.0, width as f32, height as f32],
            image: UiImage::Viewport(viewport),
            vertices: &vertices,
            indices: &indices,
        });
        primitives.extend(converted.iter().map(|mesh| mesh.primitive()));
        let outcome = self
            .renderer
            .render_frame_with_ui_outcome(&[(viewport, &scene)], &primitives, [0.0, 0.0, 0.0, 1.0])
            .map_err(|error| error.to_string())?;
        if outcome.status == FrameStatus::Presented && !ui.textures_delta.free.is_empty() {
            self.ui_bridge
                .managed
                .apply_frees(&ui.textures_delta, &mut self.renderer)
                .map_err(|error| error.to_string())?;
            ui.textures_delta.free.clear();
        }
        Ok(outcome)
    }

    #[cfg(feature = "dev-console")]
    fn close_console(&mut self) -> Result<(), String> {
        self.ui_bridge
            .managed
            .clear(&mut self.renderer)
            .map_err(|error| error.to_string())?;
        if let Some((viewport, _, _)) = self.console_viewport.take() {
            self.renderer
                .free_viewport(viewport)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

pub(crate) fn start_scripts(
    modules: &mut ModuleHost,
    world: World,
    config: ScriptThreadConfig,
) -> std::io::Result<ScriptThreadHandle> {
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);
    let stop_control = config.control_tx.clone();
    let stop_interrupt = config.interrupt.clone();
    let status = Arc::clone(&config.status);
    let panic_interrupt = config.interrupt.clone();
    let panic_render_tx = config.render_tx.clone();
    modules.spawn(
        SCRIPTS_THREAD,
        move || {
            stop_interrupt.interrupt();
            let _ = stop_control.send(ScriptControl::Shutdown);
        },
        move || {
            let mut ready_tx = Some(ready_tx);
            let result = catch_unwind(AssertUnwindSafe(|| {
                scripts_thread(world, config, &mut ready_tx);
            }));
            if let Err(payload) = result {
                let message = format!(
                    "поток скриптов завершился паникой: {}",
                    panic_message(payload)
                );
                if let Some(ready_tx) = ready_tx.take() {
                    let _ = ready_tx.send(Err(ScriptStartupError::Panic(message)));
                } else {
                    status.record_failure(message);
                    panic_interrupt.interrupt();
                    let _ = panic_render_tx.send(RenderControl::Shutdown);
                }
            }
            let _ = stopped_tx.send(());
        },
    )?;
    Ok(ScriptThreadHandle {
        ready: ready_rx,
        stopped: stopped_rx,
    })
}

fn scripts_thread(
    mut world: World,
    config: ScriptThreadConfig,
    ready_tx: &mut Option<mpsc::SyncSender<Result<(), ScriptStartupError>>>,
) {
    let ScriptThreadConfig {
        scene_name,
        sources,
        input_settings,
        scripts_library,
        #[cfg(test)]
        script_module,
        interrupt,
        clock,
        background_tick_hz,
        gamepad_devices,
        control_rx,
        frame_tx,
        render_tx,
        status,
        input,
        cursor,
        #[cfg(feature = "dev-console")]
        command_catalog,
        physics_client,
        physics_initialization,
        audio,
        prefabs,
        physics_geometry,
        prefab_loader,
        render_resources,
        network,
        frame_limit,
        ..
    } = config;
    let library = match load_script_library(
        scripts_library.as_deref(),
        #[cfg(test)]
        script_module,
    ) {
        Ok(library) => library,
        Err(error) => {
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(ScriptStartupError::Load(error)));
            }
            return;
        }
    };
    let pad_tuning = gamepad_tuning(&input_settings.gamepad);
    let mut scripts = match ScriptHost::load_with_options(
        &world,
        &scene_name,
        &sources,
        library.as_ref(),
        ScriptLoadOptions {
            interrupt: interrupt.clone(),
            call_time_limit: Some(SCRIPT_CALL_TIME_LIMIT),
            input: input_settings,
            prefabs: prefabs.catalog.clone(),
        },
    ) {
        Ok(scripts) => scripts,
        Err(error) => {
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(ScriptStartupError::Load(error.to_string())));
            }
            return;
        }
    };
    for (path, template) in &prefabs.templates {
        if let Err(error) = scripts.register_prefab(&world, path, Arc::clone(template), &sources) {
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(ScriptStartupError::Load(error.to_string())));
            }
            return;
        }
    }
    let mut spawn_physics = SpawnPhysics {
        geometry: physics_geometry,
        ..SpawnPhysics::default()
    };
    let mut physics_client = physics_client;
    let mut physics_sync = None;
    match (physics_client.as_mut(), physics_initialization) {
        (Some(_), Some(initialization)) => {
            let sync = match PhysicsSceneSync::new(&world, initialization.bindings) {
                Ok(sync) => sync,
                Err(error) => {
                    if let Some(ready_tx) = ready_tx.take() {
                        let _ = ready_tx.send(Err(ScriptStartupError::Physics(error.to_string())));
                    }
                    return;
                }
            };
            scripts.set_physics_snapshot(initialization.result.snapshot, sync.bindings());
            physics_sync = Some(sync);
        }
        (None, None) => {}
        _ => {
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(ScriptStartupError::Physics(
                    "physics client and initialization must be provided together".to_owned(),
                )));
            }
            return;
        }
    }
    // Мост подключается до `start`: `on_start` уже видит режим сети и сетевые id объектов сцены.
    let mut network: Option<ScriptNetwork> = match network {
        Some(session) => match net::attach(session, &mut world, &mut scripts) {
            Ok(bridge) => Some(bridge),
            Err(error) => {
                if let Some(ready_tx) = ready_tx.take() {
                    let _ = ready_tx.send(Err(ScriptStartupError::Network(error.to_string())));
                }
                return;
            }
        },
        None => None,
    };
    if let Some(ready_tx) = ready_tx.take()
        && ready_tx.send(Ok(())).is_err()
    {
        return;
    }

    let mut background = false;
    let mut applied_background = false;
    #[cfg(feature = "dev-console")]
    let mut pending_console_commands = PendingConsoleCommands::default();
    let material_resources = render_resources.clone();
    let mut prefab_loads = PrefabLoads::new(prefab_loader, render_resources);
    let mut pending_prefab = VecDeque::new();
    let mut material_sync = MaterialSync::default();
    let mut frame_latency: Option<Arc<dyn FrameLatency>> = None;
    let gpu_scene = loop {
        match control_rx.recv() {
            Ok(ScriptControl::Latency(latency)) => frame_latency = Some(latency),
            Ok(ScriptControl::Go(scene)) => break Some(*scene),
            Ok(ScriptControl::Shutdown) | Err(_) => break None,
            Ok(ScriptControl::Background(value)) => {
                background = value;
                sync_input_background(&input, background, &mut applied_background);
            }
            #[cfg(feature = "dev-console")]
            Ok(ScriptControl::Console(command)) => pending_console_commands.push_back(command),
            Ok(ScriptControl::Prefab(reply)) => pending_prefab.push_back(ScriptReply::Prefab(reply)),
            Ok(ScriptControl::Material(reply)) => pending_prefab.push_back(ScriptReply::Material(reply)),
        }
    };
    let Some(mut gpu_scene) = gpu_scene else {
        scripts.destroy(&mut world, ScriptTime::default());
        print_script_events(scripts.drain_events());
        return;
    };

    sync_input_background(&input, background, &mut applied_background);

    let mut timings = PhaseWindow::new(
        SOURCE_SCRIPTS,
        Arc::clone(&status.profile),
        scripts_groups(),
    );
    let mut frame_number = 0_u64;
    let mut last_tick = clock.now();
    let mut sim_time = 0.0_f64;
    let start_time = ScriptTime {
        elapsed: last_tick.as_secs_f64(),
        frame: frame_number,
    };
    if let Some(handle) = audio.as_ref() {
        preload_prefab_sounds(handle, &prefabs.templates);
    }
    let mut published_cursor = None;
    set_initial_cursor(&mut scripts, &world, &cursor, &mut published_cursor);
    scripts.start(&mut world, start_time);
    let mut particle_commands = scripts.drain_particle_commands();
    print_script_events(scripts.drain_events());
    publish_script_cursor(&scripts, &cursor, &mut published_cursor);
    let mut audio = audio.map(engine_audio::AudioBridge::new);
    if let Some(audio) = audio.as_mut() {
        // Матрицы до первого кадра не посчитаны; play_on_start берёт позиции из них.
        world.recalculate_world_matrices();
        audio.start(&world);
        audio.apply(scripts.drain_audio_requests(), &world);
    }
    let mut animation = build_animation(&world, &gpu_scene);
    drop_mismatched_skins(&world, &mut gpu_scene, &animation);
    animation.apply(scripts.drain_animation_requests());
    let mut spawned_roots = apply_world_changes(
        &mut world,
        &mut scripts,
        physics_sync.as_mut(),
        &mut spawn_physics,
        &mut gpu_scene,
        &mut prefab_loads,
        audio.as_mut(),
    );
    net::tick(&mut network, &mut world, &mut scripts, &mut spawned_roots);
    #[cfg(feature = "dev-console")]
    let mut published_catalog_generation = u64::MAX;
    #[cfg(feature = "dev-console")]
    publish_command_catalog(
        &scripts,
        &command_catalog,
        &mut published_catalog_generation,
    );
    let mut gamepads = if gamepad_devices {
        engine_input::GilrsBackend::new(pad_tuning)
    } else {
        engine_input::GilrsBackend::without_devices(pad_tuning)
    };
    let interval = Duration::from_secs_f64(1.0 / background_tick_hz.max(1) as f64);
    let now = clock.now();
    let mut next_background_tick =
        now.saturating_add(if background { interval } else { Duration::ZERO });
    loop {
        timings.publish_if_due(Instant::now());
        if frame_limit.is_some_and(|limit| frame_number >= limit) {
            break;
        }
        let mut stop_after_physics = false;
        if let (Some(client), Some(sync)) = (&mut physics_client, &mut physics_sync) {
            let physics_started = Instant::now();
            let received =
                receive_physics_interruptibly(client, &control_rx, |control| match control {
                    ScriptControl::Shutdown => true,
                    ScriptControl::Background(value) => {
                        background = value;
                        if value {
                            next_background_tick = clock.now().saturating_add(interval);
                        }
                        false
                    }
                    ScriptControl::Latency(latency) => {
                        frame_latency = Some(latency);
                        false
                    }
                    ScriptControl::Go(_) => false,
                    ScriptControl::Prefab(reply) => {
                        pending_prefab.push_back(ScriptReply::Prefab(reply));
                        false
                    }
                    ScriptControl::Material(reply) => {
                        pending_prefab.push_back(ScriptReply::Material(reply));
                        false
                    }
                    #[cfg(feature = "dev-console")]
                    ScriptControl::Console(command) => {
                        pending_console_commands.push_back(command);
                        false
                    }
                });
            timings.record(
                script_phase::LOOP,
                script_phase::PHYSICS_WAIT,
                physics_started.elapsed(),
            );
            match received {
                Ok(PhysicsWait::Idle) => {}
                Ok(PhysicsWait::Step(result)) => {
                    if let Err(error) = apply_physics_step(&mut scripts, &mut world, sync, result) {
                        status.record_failure(format!("ошибка синхронизации физики: {error}"));
                        let _ = render_tx.send(RenderControl::Shutdown);
                        stop_after_physics = true;
                    }
                }
                Ok(PhysicsWait::Shutdown) => {
                    stop_after_physics = true;
                }
                Err(error) => {
                    status.record_failure(format!("ошибка потока физики: {error}"));
                    let _ = render_tx.send(RenderControl::Shutdown);
                    stop_after_physics = true;
                }
            }
        }
        if stop_after_physics {
            break;
        }
        sync_input_background(&input, background, &mut applied_background);
        if background {
            if !wait_background_tick(
                &control_rx,
                clock.as_ref(),
                &mut background,
                next_background_tick,
                #[cfg(feature = "dev-console")]
                &mut pending_console_commands,
                #[cfg(not(feature = "dev-console"))]
                &mut (),
                &mut pending_prefab,
            ) {
                break;
            }
            if !background {
                continue;
            }
        }
        if !background {
            let mut should_stop = false;
            let mut entered_background = false;
            loop {
                match control_rx.try_recv() {
                    Ok(ScriptControl::Shutdown) | Err(TryRecvError::Disconnected) => {
                        should_stop = true;
                        break;
                    }
                    Ok(ScriptControl::Background(true)) => {
                        background = true;
                        next_background_tick = clock.now().saturating_add(interval);
                        entered_background = true;
                        break;
                    }
                    Ok(ScriptControl::Latency(latency)) => frame_latency = Some(latency),
                    Ok(ScriptControl::Background(false)) | Ok(ScriptControl::Go(_)) => {}
                    Ok(ScriptControl::Prefab(reply)) => pending_prefab.push_back(ScriptReply::Prefab(reply)),
                    Ok(ScriptControl::Material(reply)) => pending_prefab.push_back(ScriptReply::Material(reply)),
                    #[cfg(feature = "dev-console")]
                    Ok(ScriptControl::Console(command)) => {
                        pending_console_commands.push_back(command);
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            if should_stop {
                break;
            }
            if entered_background {
                continue;
            }
        }

        // Reflex: сон и маркеры только в интерактивном режиме; в background кадры без Reflex.
        let reflex = frame_latency.as_deref().filter(|_| !background);
        let next_frame = frame_number.saturating_add(1);
        if let Some(latency) = reflex {
            latency.sleep();
            latency.marker(next_frame, LatencyMarker::SimulationStart);
        }
        let now = clock.now();
        let dt = now.saturating_sub(last_tick).as_secs_f32();
        last_tick = now;
        sim_time += f64::from(dt);
        frame_number = frame_number.saturating_add(1);
        let time = ScriptTime {
            elapsed: now.as_secs_f64(),
            frame: frame_number,
        };
        let console_started = Instant::now();
        #[cfg(feature = "dev-console")]
        {
            for (name, error) in
                execute_console_commands(&mut scripts, &mut world, &mut pending_console_commands)
            {
                log::error!(target: "console", "command {name} failed: {error}");
            }
            spawned_roots.extend(apply_world_changes(
                &mut world,
                &mut scripts,
                physics_sync.as_mut(),
                &mut spawn_physics,
                &mut gpu_scene,
                &mut prefab_loads,
                audio.as_mut(),
            ));
            publish_script_cursor(&scripts, &cursor, &mut published_cursor);
            publish_command_catalog(
                &scripts,
                &command_catalog,
                &mut published_catalog_generation,
            );
        }
        timings.record(
            script_phase::LOOP,
            script_phase::CONSOLE_COMMANDS,
            console_started.elapsed(),
        );
        // Ответы префабов регистрируют шаблон и ставят отложенные spawn в очередь; их применяет
        // ближайший `update`, поэтому обработка стоит прямо перед ним.
        let prefab_started = Instant::now();
        drain_prefab_replies(
            &mut pending_prefab,
            &mut scripts,
            &mut world,
            &mut gpu_scene,
            &mut spawn_physics,
            &mut prefab_loads,
            &mut material_sync,
        );
        material_sync.apply_replies(&world, &mut gpu_scene, &material_resources, frame_number);
        timings.record(
            script_phase::LOOP,
            script_phase::PREFAB_REPLIES,
            prefab_started.elapsed(),
        );
        if let Some(latency) = reflex {
            latency.marker(frame_number, LatencyMarker::InputSample);
        }
        net::receive(&mut network, &mut world, &mut scripts);
        gamepads.poll();
        let (input_frame, accepting_input) = {
            let mut accumulator = lock_input(&input);
            let accepting = accumulator.set_pads(&gamepads.snapshot());
            (accumulator.take_frame(), accepting)
        };
        if !accepting_input {
            gamepads.stop_rumble();
        }
        scripts.set_input(input_frame);
        let update_started = Instant::now();
        scripts.update(&mut world, dt, time);
        let rumble = scripts.take_rumble_requests();
        if accepting_input {
            gamepads.rumble_frame(
                rumble
                    .iter()
                    .map(|request| (request.pad, request.strong, request.weak, request.seconds)),
            );
        }
        publish_script_cursor(&scripts, &cursor, &mut published_cursor);
        timings.record(
            script_phase::LOOP,
            script_phase::SCRIPTS_UPDATE,
            update_started.elapsed(),
        );
        record_update_profile(&mut timings, scripts.last_update_profile());
        print_script_events(scripts.drain_events());
        if let Some(audio) = audio.as_mut() {
            audio.apply(scripts.drain_audio_requests(), &world);
        }
        animation.apply(scripts.drain_animation_requests());
        particle_commands.extend(scripts.drain_particle_commands());
        let finished = animation.advance(dt);
        scripts.queue_animation_events(
            &world,
            finished.into_iter().map(|event| match event {
                AnimationEvent::Finished { object, motion } => (object, motion),
            }),
        );
        log_animation_warnings(&mut animation);
        let world_changes_started = Instant::now();
        net::apply(&mut network, &mut world, &mut scripts);
        spawned_roots.extend(apply_world_changes(
            &mut world,
            &mut scripts,
            physics_sync.as_mut(),
            &mut spawn_physics,
            &mut gpu_scene,
            &mut prefab_loads,
            audio.as_mut(),
        ));
        net::tick(&mut network, &mut world, &mut scripts, &mut spawned_roots);
        material_sync.request_changes(&mut world, &gpu_scene, &material_resources);
        timings.record(
            script_phase::LOOP,
            script_phase::WORLD_CHANGES,
            world_changes_started.elapsed(),
        );
        #[cfg(feature = "dev-console")]
        publish_command_catalog(
            &scripts,
            &command_catalog,
            &mut published_catalog_generation,
        );
        let sync_started = Instant::now();
        if let (Some(client), Some(sync)) = (&mut physics_client, &mut physics_sync) {
            let mut commands = scripts.drain_physics_commands();
            match sync.sync_world_commands(&mut world) {
                Ok(world_commands) => {
                    commands.extend(world_commands);
                    for warning in sync.take_warnings() {
                        log::warn!("[physics] привязка не обновлена: {warning}");
                    }
                }
                Err(error) => {
                    status.record_failure(format!("ошибка синхронизации сцены с физикой: {error}"));
                    let _ = render_tx.send(RenderControl::Shutdown);
                    break;
                }
            }
            commands.append(&mut spawn_physics.velocities);
            let changes = std::mem::take(&mut spawn_physics.changes);
            if let Err(error) = client.step_with_changes(dt, changes, commands) {
                // Остановка выставляет interrupt до Shutdown физики: шаг, отправленный уже
                // после выхода сервера, — ожидаемое завершение, а не сбой.
                if !interrupt.is_interrupted() {
                    status.record_failure(format!("ошибка шага физики: {error}"));
                }
                let _ = render_tx.send(RenderControl::Shutdown);
                break;
            }
        }
        timings.record(
            script_phase::LOOP,
            script_phase::PHYSICS_SYNC,
            sync_started.elapsed(),
        );
        if background {
            next_background_tick = now.saturating_add(interval);
            continue;
        }

        let build_started = Instant::now();
        let (scene_frame, warnings) = world.build_frame();
        timings.record(
            script_phase::LOOP,
            script_phase::BUILD_FRAME,
            build_started.elapsed(),
        );
        for warning in warnings {
            log::warn!(
                "[scene] {}: объект {}: {}",
                warning.source,
                warning.object_id,
                warning.message
            );
        }
        if let Some(audio) = audio.as_mut() {
            audio.sync_positions(&world);
        }
        let snapshot_started = Instant::now();
        let made =
            make_snapshot(scene_frame, &world, &gpu_scene, &animation).map(|mut snapshot| {
                snapshot.sim_time = sim_time;
                snapshot.particle_commands = std::mem::take(&mut particle_commands);
                snapshot.screen_shaders.time = screen_shader_time(sim_time);
                snapshot
            });
        timings.record(
            script_phase::LOOP,
            script_phase::MAKE_SNAPSHOT,
            snapshot_started.elapsed(),
        );
        let snapshot = match made {
            Ok(mut snapshot) => {
                snapshot.sequence = frame_number;
                if let Some(latency) = reflex {
                    latency.marker(frame_number, LatencyMarker::SimulationEnd);
                }
                Arc::new(snapshot)
            }
            Err(error) => {
                status.record_failure(error);
                break;
            }
        };
        let send_started = Instant::now();
        let sent = send_snapshot(
            &frame_tx,
            &control_rx,
            snapshot,
            &mut background,
            #[cfg(feature = "dev-console")]
            &mut pending_console_commands,
            #[cfg(not(feature = "dev-console"))]
            &mut (),
            &mut pending_prefab,
        );
        timings.record(
            script_phase::LOOP,
            script_phase::SNAPSHOT_SEND_WAIT,
            send_started.elapsed(),
        );
        match sent {
            SendOutcome::Sent => prefab_loads.set_last_sent_seq(frame_number),
            SendOutcome::Background => {
                next_background_tick = clock.now().saturating_add(interval);
                continue;
            }
            SendOutcome::Shutdown | SendOutcome::Disconnected => break,
        }
    }

    scripts.destroy(
        &mut world,
        ScriptTime {
            elapsed: clock.now().as_secs_f64(),
            frame: frame_number,
        },
    );
    print_script_events(scripts.drain_events());
    #[cfg(feature = "dev-console")]
    publish_command_catalog(
        &scripts,
        &command_catalog,
        &mut published_catalog_generation,
    );
    let _ = render_tx.send(RenderControl::Shutdown);
}

fn gamepad_tuning(settings: &engine_project::GamepadSettings) -> engine_input::GamepadTuning {
    engine_input::GamepadTuning {
        stick_deadzone: settings.stick_deadzone,
        trigger_threshold: settings.trigger_threshold,
    }
}

fn sync_input_background(
    input: &Mutex<InputAccumulator>,
    background: bool,
    applied_background: &mut bool,
) {
    if *applied_background != background {
        lock_input(input).set_background(background);
        *applied_background = background;
    }
}

fn apply_physics_step(
    scripts: &mut ScriptHost,
    world: &mut World,
    sync: &mut PhysicsSceneSync,
    result: PhysicsStepResult,
) -> Result<(), String> {
    if result.skipped_commands > 0 {
        log::warn!(
            "[physics] отклонено команд коллайдера или тела: {}",
            result.skipped_commands
        );
    }
    sync.apply_physics_poses(world, &result.poses)
        .map_err(|error| error.to_string())?;
    scripts.set_physics_snapshot(result.snapshot, sync.bindings());
    scripts.queue_physics_events(result.events);
    Ok(())
}

enum PhysicsWait {
    Idle,
    Step(PhysicsStepResult),
    Shutdown,
}

fn receive_physics_interruptibly(
    client: &mut PhysicsClient,
    control_rx: &Receiver<ScriptControl>,
    mut handle_control: impl FnMut(ScriptControl) -> bool,
) -> Result<PhysicsWait, String> {
    loop {
        match client
            .receive(control_rx)
            .map_err(|error| error.to_string())?
        {
            None => return Ok(PhysicsWait::Idle),
            Some(PhysicsReceive::Step(result)) => return Ok(PhysicsWait::Step(result)),
            Some(PhysicsReceive::Interrupted(control)) => {
                if handle_control(control) {
                    return Ok(PhysicsWait::Shutdown);
                }
            }
        }
    }
}

/// Состояние физики spawn-объектов, которое копится до ближайшего шага физики.
#[derive(Default)]
struct SpawnPhysics {
    /// Формы мешей, построенные для spawn-объектов; переиспользуются между spawn одной модели.
    shapes: ShapeCache,
    /// Геометрия моделей для коллайдеров мешей.
    geometry: Vec<ModelGeometry>,
    /// Модели, добавленные тяжёлыми префабами, и число загруженных префабов на каждую. Модели
    /// сцены сюда не входят и не удаляются.
    heavy_users: HashMap<String, usize>,
    /// Вставки и удаления тел; уходят в `step_with_changes` ближайшего шага.
    changes: Vec<BodyChange>,
    /// Скорости, заданные при spawn; применяются сразу после вставки тела в том же шаге.
    velocities: Vec<PhysicsCommand>,
}

impl SpawnPhysics {
    /// Добавляет геометрию тяжёлого префаба. Путь, уже занятый сценой или другим префабом,
    /// не заменяется: содержимое одной модели одинаково.
    fn add_heavy_geometry(&mut self, models: Vec<ModelGeometry>) {
        for model in models {
            if let Some(users) = self.heavy_users.get_mut(&model.path) {
                *users += 1;
                continue;
            }
            if self.geometry.iter().any(|known| known.path == model.path) {
                continue;
            }
            self.heavy_users.insert(model.path.clone(), 1);
            self.geometry.push(model);
        }
    }

    /// Снимает ссылки выгруженного префаба. Модель уходит вместе с формами кэша, когда её больше
    /// никто не использует.
    fn remove_heavy_geometry(&mut self, paths: &[String]) {
        for path in paths {
            let Some(users) = self.heavy_users.get_mut(path) else {
                continue;
            };
            *users -= 1;
            if *users == 0 {
                self.heavy_users.remove(path);
                self.geometry.retain(|model| &model.path != path);
                self.shapes.remove_model(path);
            }
        }
    }
}

/// Переключает тела объектов, у которых изменилась действующая активность: тела неактивных
/// объектов удаляются, тела активированных создаются заново. Возвращает заявки звука: источники
/// неактивных объектов останавливаются, у показанных запускаются `play_on_start`. Тела вставляются
/// и удаляются до `sync_world_commands`, как и spawn.
fn apply_activity_changes(
    world: &mut World,
    physics: Option<&mut PhysicsSceneSync>,
    spawn_physics: &mut SpawnPhysics,
) -> Vec<AudioRequest> {
    let changed = world.take_activity_changes();
    if changed.is_empty() {
        return Vec::new();
    }
    let (hidden, shown): (Vec<ObjectHandle>, Vec<ObjectHandle>) = changed
        .into_iter()
        .partition(|&object| world.active_in_hierarchy(object) != Some(true));
    if let Some(sync) = physics {
        spawn_physics.changes.extend(sync.remove_objects(&hidden));
        let bound: std::collections::HashSet<ObjectHandle> = sync
            .bindings()
            .iter()
            .map(|binding| binding.object)
            .collect();
        let unbound: Vec<ObjectHandle> = shown
            .iter()
            .copied()
            .filter(|object| world.is_alive(*object) && !bound.contains(object))
            .collect();
        if !unbound.is_empty() {
            match sync.insert_objects(
                world,
                &unbound,
                &spawn_physics.geometry,
                &mut spawn_physics.shapes,
            ) {
                Ok(inserted) => spawn_physics.changes.extend(inserted),
                Err(error) => {
                    log::warn!("физика не создала тела активированных объектов: {error}");
                }
            }
        }
    }
    activity_audio_requests(world, &hidden, &shown)
}

/// Применяет spawn и destroy, закоммиченные скриптами в фазе: обновляет GPU-таблицу, физику и
/// звук. Тела удаляются и вставляются до `sync_world_commands`, поэтому позы новых тел не дают
/// ложных телепортаций. Возвращает корни созданных объектов для сетевого моста.
fn apply_world_changes(
    world: &mut World,
    scripts: &mut ScriptHost,
    mut physics: Option<&mut PhysicsSceneSync>,
    spawn_physics: &mut SpawnPhysics,
    gpu_scene: &mut GpuSceneTable,
    prefabs: &mut PrefabLoads,
    mut audio: Option<&mut engine_audio::AudioBridge>,
) -> Vec<SpawnedRoot> {
    let activity_requests = apply_activity_changes(world, physics.as_deref_mut(), spawn_physics);
    if let Some(audio) = audio.as_deref_mut()
        && !activity_requests.is_empty()
    {
        audio.apply(activity_requests, world);
    }
    let mut roots = Vec::new();
    // Выгрузка может сразу запросить новую загрузку (отложенный spawn), поэтому заявки
    // вычитываются, пока очередь не опустеет.
    loop {
        let changes = scripts.drain_world_changes();
        if changes.spawned.is_empty()
            && changes.despawned.is_empty()
            && changes.prefab_requests.is_empty()
        {
            return roots;
        }
        roots.extend(changes.spawned.iter().map(|spawned| SpawnedRoot {
            handle: spawned.handle,
            prefab: spawned.prefab.clone(),
        }));
        apply_changes(
            world,
            scripts,
            physics.as_deref_mut(),
            spawn_physics,
            gpu_scene,
            prefabs,
            audio.as_deref_mut(),
            changes,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_changes(
    world: &mut World,
    scripts: &mut ScriptHost,
    mut physics: Option<&mut PhysicsSceneSync>,
    spawn_physics: &mut SpawnPhysics,
    gpu_scene: &mut GpuSceneTable,
    prefabs: &mut PrefabLoads,
    audio: Option<&mut engine_audio::AudioBridge>,
    changes: WorldChanges,
) {
    let mut requests = Vec::new();
    for spawned in &changes.spawned {
        if let Some(templates) = gpu_scene.prefabs.get(&spawned.prefab) {
            attach_prefab_levels(&mut gpu_scene.by_object, templates, spawned);
        }
    }
    detach_objects(&mut gpu_scene.by_object, &changes.despawned);
    if !changes.despawned.is_empty() {
        prefabs.send_particles(ParticleRequest::Destroy(changes.despawned.clone()));
    }
    requests.extend(
        changes
            .despawned
            .iter()
            .map(|&object| AudioRequest::StopSource { object }),
    );
    if let Some(sync) = physics.as_deref_mut() {
        spawn_physics
            .changes
            .extend(sync.remove_objects(&changes.despawned));
    }
    for spawned in &changes.spawned {
        // Объект, созданный и удалённый в одной фазе, уже не жив: тело и звук ему не нужны.
        if !world.is_alive(spawned.handle) {
            continue;
        }
        if let Some(sync) = physics.as_deref_mut() {
            // Объект, который активация этого кадра уже вставила, второе тело не получает.
            let fresh = sync.unbound_objects(&spawned.objects);
            let inserted = sync.insert_objects(
                world,
                &fresh,
                &spawn_physics.geometry,
                &mut spawn_physics.shapes,
            );
            match inserted {
                Ok(inserted) => {
                    spawn_physics.changes.extend(inserted);
                    if let Some(velocity) = spawned.initial_velocity
                        && let Some(body) = root_body(sync, spawned.handle)
                    {
                        spawn_physics
                            .velocities
                            .push(PhysicsCommand::SetVelocity { body, velocity });
                    }
                }
                Err(error) => {
                    // Откат снимает и экземпляры скриптов; Error из него печатается сразу.
                    scripts.revert_spawn(
                        world,
                        spawned.handle,
                        &format!("физика отклонила объект {:?}: {error}", spawned.prefab),
                    );
                    print_script_events(scripts.drain_events());
                    detach_objects(&mut gpu_scene.by_object, &spawned.objects);
                    continue;
                }
            }
        }
        if let Some(descs) = gpu_scene.particle_templates.get(&spawned.prefab) {
            for request in spawn_particle_requests(&spawned.objects, descs) {
                prefabs.send_particles(request);
            }
        }
        requests.extend(
            spawned
                .objects
                .iter()
                .filter(|&&object| {
                    world
                        .audio_source(object)
                        .is_some_and(|audio| audio.play_on_start)
                })
                .map(|&object| AudioRequest::PlaySource {
                    object,
                    voice: VoiceId::allocate(),
                }),
        );
    }
    apply_prefab_requests(
        world,
        scripts,
        spawn_physics,
        gpu_scene,
        prefabs,
        &changes.prefab_requests,
    );
    if let Some(audio) = audio {
        audio.apply(requests, world);
    }
}

/// Загрузка и выгрузка тяжёлых префабов. Выгрузка снимает GPU-уровни и геометрию, отправляет
/// освобождение и сообщает хосту, что ресурсы ушли.
fn apply_prefab_requests(
    world: &World,
    scripts: &mut ScriptHost,
    spawn_physics: &mut SpawnPhysics,
    gpu_scene: &mut GpuSceneTable,
    prefabs: &mut PrefabLoads,
    requests: &[PrefabRequest],
) {
    for request in requests {
        match request {
            PrefabRequest::Load(path) => {
                if let Err(error) = prefabs.start_load(path) {
                    scripts.prefab_failed(world, path, &error);
                }
            }
            PrefabRequest::Unload(path) => {
                gpu_scene.prefabs.remove(path);
                gpu_scene.particle_templates.remove(path);
                if let Some(slot) = prefabs.forget(path) {
                    if let Some(geometry) = slot.geometry {
                        spawn_physics.remove_heavy_geometry(&geometry);
                    }
                    prefabs.release(slot.token);
                }
                scripts.prefab_released(path);
            }
        }
    }
}

/// Замены материалов меш-рендерера скриптом на scripts thread: заявки render thread, владение
/// материалами, созданными по ним, и ответы, ещё не применённые к `by_object`.
#[derive(Default)]
struct MaterialSync {
    owned: MaterialOwnership<MaterialId>,
    /// Объекты, для которых заявка уже у render thread: новая заявка ждёт ответа.
    in_flight: HashSet<ObjectHandle>,
    /// Объекты, у которых материал надо создать заново.
    dirty: HashSet<ObjectHandle>,
    /// Принятые ответы render thread, которые ещё не применены.
    replies: VecDeque<MaterialReply>,
}

impl MaterialSync {
    fn accept(&mut self, reply: MaterialReply) {
        self.replies.push_back(reply);
    }

    /// Ставит заявки на объекты, у которых изменился материал. На объект не больше одной заявки в
    /// полёте: изменения во время заявки ждут ответа и уходят следующей заявкой.
    fn request_changes(
        &mut self,
        world: &mut World,
        gpu_scene: &GpuSceneTable,
        resources: &Sender<RenderResourceRequest>,
    ) {
        self.dirty.extend(world.take_mesh_material_changes());
        let ready: Vec<ObjectHandle> = self
            .dirty
            .iter()
            .copied()
            .filter(|handle| !self.in_flight.contains(handle))
            .collect();
        for handle in ready {
            self.dirty.remove(&handle);
            let Some(levels) = gpu_scene.by_object.get(&handle) else {
                continue;
            };
            let (Some(revision), Some(object)) =
                (world.mesh_material_revision(handle), world.object(handle))
            else {
                continue;
            };
            let Some(mesh) = object.mesh_renderer.as_ref() else {
                continue;
            };
            let request = MaterialRequest {
                object: handle,
                revision,
                levels: levels.clone(),
                material: mesh.material.clone(),
                path: world
                    .mesh_material_path(handle)
                    .map(|path| path.as_str().to_owned()),
            };
            if resources
                .send(RenderResourceRequest::Material(Box::new(request)))
                .is_ok()
            {
                self.in_flight.insert(handle);
            }
        }
    }

    /// Применяет ответы: новые уровни заменяют прежние, материалы прежних освобождаются после снимков
    /// `seq ≥ last_seq`. Ответ по удалённому объекту освобождает свои материалы сразу.
    fn apply_replies(
        &mut self,
        world: &World,
        gpu_scene: &mut GpuSceneTable,
        resources: &Sender<RenderResourceRequest>,
        last_seq: u64,
    ) {
        while let Some(reply) = self.replies.pop_front() {
            self.in_flight.remove(&reply.object);
            match reply.result {
                Ok(MaterialLevels { levels, created })
                    if world.is_alive(reply.object)
                        && gpu_scene.by_object.contains_key(&reply.object) =>
                {
                    gpu_scene.by_object.insert(reply.object, levels);
                    let previous = self.owned.replace(reply.object, created);
                    send_material_release(resources, previous, last_seq);
                }
                Ok(MaterialLevels { created, .. }) => {
                    send_material_release(resources, created, 0);
                }
                Err(message) => {
                    let id = world
                        .object(reply.object)
                        .map_or("?", |object| object.id.as_str());
                    log::warn!("[material] объект {id}: материал не заменён: {message}");
                }
            }
            // Изменения, пришедшие во время заявки, ставят объект в очередь ещё раз.
            if world.mesh_material_revision(reply.object) != Some(reply.revision) {
                self.dirty.insert(reply.object);
            }
        }
        // Материалы объектов, удалённых из таблицы GPU, освобождаются вместе с ними.
        let gone = self
            .owned
            .take_dead(|handle| gpu_scene.by_object.contains_key(&handle));
        send_material_release(resources, gone, last_seq);
    }
}

/// Материалы, созданные по заявкам замены, по объектам: при следующей замене или удалении объекта
/// прежние материалы уходят на освобождение. Материалы сцены сюда не входят: они могут быть общими.
struct MaterialOwnership<M> {
    owned: HashMap<ObjectHandle, Vec<M>>,
}

impl<M> Default for MaterialOwnership<M> {
    fn default() -> Self {
        Self {
            owned: HashMap::new(),
        }
    }
}

impl<M> MaterialOwnership<M> {
    /// Записывает материалы новой замены и возвращает материалы прежней.
    fn replace(&mut self, object: ObjectHandle, created: Vec<M>) -> Vec<M> {
        self.owned.insert(object, created).unwrap_or_default()
    }

    /// Забирает материалы объектов, для которых `is_live` ложно.
    fn take_dead(&mut self, is_live: impl Fn(ObjectHandle) -> bool) -> Vec<M> {
        let dead: Vec<ObjectHandle> = self
            .owned
            .keys()
            .copied()
            .filter(|&handle| !is_live(handle))
            .collect();
        dead.into_iter()
            .filter_map(|handle| self.owned.remove(&handle))
            .flatten()
            .collect()
    }
}

fn send_material_release(
    resources: &Sender<RenderResourceRequest>,
    materials: Vec<MaterialId>,
    last_seq: u64,
) {
    if !materials.is_empty() {
        let _ = resources.send(RenderResourceRequest::ReleaseMaterials {
            materials,
            last_seq,
        });
    }
}

/// Применяет ответы загрузки: регистрирует шаблон, добавляет его GPU-уровни и геометрию. Ответы
/// со старым токеном освобождаются сразу: их ресурсы никто не использует.
fn drain_prefab_replies(
    pending: &mut VecDeque<ScriptReply>,
    scripts: &mut ScriptHost,
    world: &mut World,
    gpu_scene: &mut GpuSceneTable,
    spawn_physics: &mut SpawnPhysics,
    prefabs: &mut PrefabLoads,
    materials: &mut MaterialSync,
) {
    while let Some(reply) = pending.pop_front() {
        let reply = match reply {
            ScriptReply::Material(reply) => {
                materials.accept(*reply);
                continue;
            }
            ScriptReply::Prefab(reply) => reply,
        };
        match reply {
            PrefabReply::Ready(ready) => {
                let PrefabReady {
                    token,
                    gpu,
                    particles,
                    cpu,
                } = *ready;
                if !prefabs.is_current(&token) {
                    prefabs.release(token);
                    continue;
                }
                let PrefabCpu {
                    template,
                    sources,
                    geometry,
                } = cpu;
                if let Err(error) = scripts.register_prefab(world, &token.path, template, &sources)
                {
                    prefabs.forget(&token.path);
                    scripts.prefab_failed(world, &token.path, &error.to_string());
                    prefabs.release(token);
                    continue;
                }
                let geometry_paths = geometry.iter().map(|model| model.path.clone()).collect();
                spawn_physics.add_heavy_geometry(geometry);
                gpu_scene.prefabs.insert(token.path.clone(), gpu);
                gpu_scene
                    .particle_templates
                    .insert(token.path.clone(), particles);
                prefabs.mark_ready(&token, geometry_paths);
            }
            PrefabReply::Failed { token, error } => {
                if prefabs.is_current(&token) {
                    prefabs.forget(&token.path);
                    scripts.prefab_failed(world, &token.path, &error);
                }
            }
        }
    }
}

/// Записывает уровни шаблона на объекты поддерева: объект `i` получает уровни шаблона `i`.
/// Объекты без меша записи не получают.
fn attach_prefab_levels<T: Clone>(
    by_object: &mut HashMap<ObjectHandle, Vec<Vec<T>>>,
    templates: &[Vec<Vec<T>>],
    spawned: &SpawnedObject,
) {
    for (object, levels) in spawned.objects.iter().zip(templates) {
        if !levels.is_empty() {
            by_object.insert(*object, levels.clone());
        }
    }
}

fn detach_objects<T>(by_object: &mut HashMap<ObjectHandle, Vec<Vec<T>>>, handles: &[ObjectHandle]) {
    for handle in handles {
        by_object.remove(handle);
    }
}

/// Тело корня spawn-объекта; `None`, если у корня нет коллайдера и тела не создано.
fn root_body(sync: &PhysicsSceneSync, root: ObjectHandle) -> Option<BodyHandle> {
    sync.bindings()
        .iter()
        .rev()
        .find(|binding| binding.object == root)
        .map(|binding| binding.body)
}

/// Клипы шаблонов быстрых префабов предзагружаются вместе со сценой: spawn-объект должен
/// проигрывать звук без ожидания загрузки.
fn preload_prefab_sounds(
    audio: &engine_audio::AudioHandle,
    templates: &BTreeMap<String, Arc<PrefabTemplate>>,
) {
    let mut clips = templates
        .values()
        .flat_map(|template| template.objects())
        .filter_map(|object| object.audio_source.as_ref())
        .map(|source| source.clip.as_str().to_owned())
        .collect::<Vec<_>>();
    clips.sort();
    clips.dedup();
    if clips.is_empty() {
        return;
    }
    if let Err(error) = audio.preload(clips) {
        log::warn!("предзагрузка звука префабов: {error}");
    }
}

fn load_script_library(
    path: Option<&Path>,
    #[cfg(test)] module: Option<&'static rve_sdk::abi::RveModule>,
) -> Result<Option<ScriptLibrary>, String> {
    #[cfg(test)]
    if let Some(module) = module {
        // SAFETY: test modules are generated by `rve_sdk::script_module!` and live for the process.
        return unsafe { ScriptLibrary::from_module(std::ptr::from_ref(module)) }
            .map(Some)
            .map_err(|error| error.to_string());
    }
    let Some(path) = path else {
        return Ok(None);
    };
    // SAFETY: script libraries are native code supplied by the project being run.
    unsafe { ScriptLibrary::load(path) }
        .map(Some)
        .map_err(|error| error.to_string())
}

#[derive(Default)]
struct MotionHistory {
    active_camera: Option<ObjectHandle>,
    previous_models: HashMap<ObjectHandle, [[f32; 4]; 4]>,
}

impl MotionHistory {
    fn previous_model(&self, object: ObjectHandle, camera_changed: bool) -> Option<[[f32; 4]; 4]> {
        if camera_changed {
            None
        } else {
            self.previous_models.get(&object).copied()
        }
    }

    fn prepare(&self, snapshot: &mut FrameSnapshot) {
        let camera_changed = self
            .active_camera
            .is_some_and(|previous| previous != snapshot.active_camera);
        snapshot.reset_history = camera_changed;
        for (object, motion) in &snapshot.object_motion {
            let previous_model = self.previous_model(*object, camera_changed);
            for draw in &mut snapshot.draws[motion.draw_range.clone()] {
                draw.previous_model = previous_model;
            }
        }
    }

    fn submitted(&mut self, snapshot: &FrameSnapshot) {
        self.previous_models.clear();
        for (object, motion) in &snapshot.object_motion {
            self.previous_models.insert(*object, motion.model);
        }
        self.active_camera = Some(snapshot.active_camera);
    }
}

/// Включённые проходы камеры, для которых создан GPU-шейдер, в порядке файла сцены. Шейдер без
/// созданного ресурса (ошибка загрузки) пропускается; остальные сохраняют порядок и стадию.
fn enabled_screen_passes<T: Copy>(
    shaders: &[engine_scene::ScreenShaderRef],
    ids: &HashMap<String, T>,
) -> [Option<(
    T,
    engine_render::ScreenShaderStage,
    [f32; engine_render::SCREEN_SHADER_PARAM_COUNT],
)>; engine_render::SCREEN_SHADER_MAX_PASSES] {
    let mut passes = [None; engine_render::SCREEN_SHADER_MAX_PASSES];
    let known = shaders
        .iter()
        .filter(|shader| shader.enabled)
        .filter_map(|shader| Some((ids.get(shader.shader.as_str())?, shader)));
    for (slot, (id, shader)) in known.take(passes.len()).enumerate() {
        let mut params = [0.0_f32; engine_render::SCREEN_SHADER_PARAM_COUNT];
        for (value, parameter) in params.iter_mut().zip(&shader.params) {
            *value = *parameter;
        }
        let stage = match shader.stage {
            engine_scene::ScreenShaderStage::BeforePost => {
                engine_render::ScreenShaderStage::BeforePost
            }
            engine_scene::ScreenShaderStage::AfterPost => {
                engine_render::ScreenShaderStage::AfterPost
            }
        };
        passes[slot] = Some((*id, stage, params));
    }
    passes
}

/// Экранные шейдеры вида для кадра; время задаёт вызывающий код.
pub(crate) fn screen_shader_view(
    shaders: &[engine_scene::ScreenShaderRef],
    ids: &HashMap<String, engine_render::ScreenShaderId>,
) -> engine_render::ScreenShaders {
    let mut view = engine_render::ScreenShaders::default();
    for (target, pass) in view
        .passes
        .iter_mut()
        .zip(enabled_screen_passes(shaders, ids))
    {
        *target = pass.map(|(shader, stage, params)| engine_render::ScreenShaderPass {
            shader,
            stage,
            params,
        });
    }
    view
}

fn make_snapshot(
    scene: engine_scene::SceneFrame,
    world: &World,
    gpu_scene: &GpuSceneTable,
    animation: &AnimationBridge,
) -> Result<FrameSnapshot, String> {
    let mut draws = Vec::new();
    let mut joint_palettes = Vec::new();
    let camera_position = scene.camera.position;
    let mut object_motion = HashMap::new();
    for instance in scene.meshes {
        let Some(levels) = gpu_scene.by_object.get(&instance.object) else {
            continue;
        };
        let Some((lod_level, meshes)) =
            selected_level(world, instance.object, camera_position, levels)
        else {
            continue;
        };
        let model = instance.model.to_cols_array_2d();
        let draw_start = draws.len();
        // Анимируемый объект рисуется экземплярами skinning, пока у моста есть палитра;
        // иначе — bind-pose мешем без скиннинга.
        let palette = gpu_scene
            .skin_instances
            .get(&instance.object)
            .zip(animation.palette(instance.object))
            .map(|(instances, palette)| {
                joint_palettes.push(palette.to_vec());
                (instances, (joint_palettes.len() - 1) as u32)
            });
        for (index, (mesh, material)) in meshes.iter().enumerate() {
            let skin = palette.and_then(|(instances, palette)| {
                instances
                    .get(index)
                    .map(|&instance| SkinBinding { instance, palette })
            });
            draws.push(DrawCommand {
                mesh: *mesh,
                material: Some(*material),
                model,
                previous_model: None,
                lod_level,
                casts_shadow: true,
                skin,
            });
        }
        if draw_start != draws.len() {
            object_motion.insert(
                instance.object,
                ObjectMotion {
                    model,
                    draw_range: draw_start..draws.len(),
                },
            );
        }
    }
    let environment = resolve_environment_snapshot(scene.environment.as_ref(), gpu_scene)?;
    let (local_lights, local_light_shadows) = render_local_lights(&scene.local_lights);
    let particles = scene
        .particle_emitters
        .iter()
        .map(|instance| ParticleSnapshot {
            object: instance.object,
            transform: instance.model.to_cols_array_2d(),
            emitting: instance.emitting,
        })
        .collect();
    Ok(FrameSnapshot {
        sequence: 0,
        camera: scene.camera,
        active_camera: world.active_camera(),
        view: scene.view,
        post_process: scene.post_process,
        screen_shaders: screen_shader_view(&scene.screen_shaders, &gpu_scene.screen_shaders),
        reset_history: false,
        light: scene.light,
        local_lights,
        local_light_shadows,
        environment,
        draws,
        particles,
        particle_commands: Vec::new(),
        sim_time: 0.0,
        joint_palettes,
        object_motion,
    })
}

/// Палитры кадра как срезы для `Frame::joint_palettes`.
fn palette_refs(snapshot: &FrameSnapshot) -> Vec<&[[[f32; 4]; 4]]> {
    snapshot.joint_palettes.iter().map(Vec::as_slice).collect()
}

/// Мост анимации для потока скриптов: модели и клипы берутся из GPU-таблицы.
fn build_animation(world: &World, gpu_scene: &GpuSceneTable) -> AnimationBridge {
    let mut animation = AnimationBridge::build(
        world,
        |path| {
            gpu_scene
                .animation_models
                .get(path.as_str())
                .cloned()
                .ok_or_else(|| format!("модель {} не подготовлена для анимации", path.as_str()))
        },
        |path| {
            gpu_scene
                .animation_clips
                .get(path.as_str())
                .cloned()
                .ok_or_else(|| format!("клип {} не загружен", path.as_str()))
        },
    );
    log_animation_warnings(&mut animation);
    animation
}

/// Убирает skinning у объектов, чья палитра моста не совпала с числом костей меша: такой объект
/// рисуется статично и не валит кадр. Экземпляры остаются у renderer до его завершения.
fn drop_mismatched_skins(
    world: &World,
    gpu_scene: &mut GpuSceneTable,
    animation: &AnimationBridge,
) -> Vec<ObjectHandle> {
    let mut mismatched: Vec<ObjectHandle> = gpu_scene
        .joint_counts
        .iter()
        .filter(|(object, joints)| {
            animation
                .palette(**object)
                .is_some_and(|palette| palette.len() != **joints as usize)
        })
        .map(|(object, _)| *object)
        .collect();
    mismatched.sort_by_key(|object| object.index());
    for object in &mismatched {
        gpu_scene.skin_instances.remove(object);
        let id = world.object(*object).map_or_else(
            || format!("объект #{}", object.index()),
            |item| item.id.clone(),
        );
        log::warn!(
            "[animation] {id}: число костей палитры не совпадает с мешем; объект рисуется без скиннинга"
        );
    }
    mismatched
}

fn log_animation_warnings(animation: &mut AnimationBridge) {
    for warning in animation.take_warnings() {
        log::warn!("[animation] {warning}");
    }
}

/// Создаёт текстуры эмиттеров тяжёлого префаба и их описания. Объекты с одним путём текстуры
/// делят её. При ошибке созданные текстуры уничтожаются.
fn create_prefab_particles(
    renderer: &mut Renderer,
    textures: &HashMap<String, TextureData>,
    particles: &[Option<ParticleSource>],
) -> Result<PrefabParticles, RenderError> {
    let mut out = PrefabParticles::default();
    let mut by_path = HashMap::<String, TextureId>::new();
    let mut descs = Vec::with_capacity(particles.len());
    let built = (|| -> Result<(), RenderError> {
        for source in particles {
            let Some(source) = source else {
                descs.push(None);
                continue;
            };
            let texture = match &source.texture {
                Some(path) => match by_path.get(path) {
                    Some(id) => Some(*id),
                    None => {
                        let data = textures.get(path).ok_or(RenderError::InvalidState(
                            "CPU-текстура частиц отсутствует в таблице подготовленных ресурсов",
                        ))?;
                        let id = renderer.create_texture_with_color_space(
                            data.width,
                            data.height,
                            &data.rgba,
                            TextureColorSpace::Srgb,
                        )?;
                        out.textures.push(id);
                        by_path.insert(path.clone(), id);
                        Some(id)
                    }
                },
                None => None,
            };
            descs.push(Some(particle_emitter_desc(&source.emitter, texture)));
        }
        Ok(())
    })();
    match built {
        Ok(()) => {
            out.descs = Some(descs);
            Ok(out)
        }
        Err(error) => {
            release_prefab_particles(renderer, out.textures);
            Err(error)
        }
    }
}

fn release_prefab_particles(renderer: &mut Renderer, textures: Vec<TextureId>) {
    for texture in textures {
        if let Err(error) = renderer.destroy_texture(texture) {
            log::error!("текстура эмиттера частиц не освобождена: {error}");
        }
    }
}

/// Применяет заявку эмиттеров к таблице по хендлам объектов. Create заменяет прежний эмиттер
/// объекта; ошибка создания оставляет объект без эмиттера. Destroy неизвестного хендла ничего
/// не делает, поэтому повтор и поздняя заявка безопасны.
fn apply_emitter_request<E: Copy, R>(
    table: &mut HashMap<ObjectHandle, E>,
    renderer: &mut R,
    request: ParticleRequest,
    mut create: impl FnMut(&mut R, ParticleEmitterDesc) -> Result<E, String>,
    mut destroy: impl FnMut(&mut R, E),
) {
    match request {
        ParticleRequest::Create { object, desc } => {
            if let Some(previous) = table.remove(&object) {
                destroy(renderer, previous);
            }
            match create(renderer, *desc) {
                Ok(id) => {
                    table.insert(object, id);
                }
                Err(error) => log::error!("эмиттер частиц объекта {object:?} не создан: {error}"),
            }
        }
        ParticleRequest::Destroy(objects) => {
            for object in objects {
                if let Some(id) = table.remove(&object) {
                    destroy(renderer, id);
                }
            }
        }
    }
}

/// Заявки создания для объектов поддерева: объект `i` получает описание шаблона `i`; объекты
/// без эмиттера пропускаются.
fn spawn_particle_requests(
    objects: &[ObjectHandle],
    descs: &[Option<ParticleEmitterDesc>],
) -> Vec<ParticleRequest> {
    objects
        .iter()
        .zip(descs)
        .filter_map(|(&object, desc)| {
            Some(ParticleRequest::Create {
                object,
                desc: Box::new(desc.clone()?),
            })
        })
        .collect()
}

/// Переводит команды скриптов в действия над эмиттерами: `None` — перезапуск, `Some(n)` — выброс.
/// Команда объекта без эмиттера пропускается, как и заявки, пришедшие после уничтожения объекта.
fn resolve_particle_commands<E: Copy>(
    emitters: &HashMap<ObjectHandle, E>,
    commands: &[ParticleCommand],
) -> Vec<(E, Option<u32>)> {
    commands
        .iter()
        .filter_map(|command| {
            let (object, burst) = match *command {
                ParticleCommand::Restart { object } => (object, None),
                ParticleCommand::Burst { object, count } => (object, Some(count)),
            };
            Some((*emitters.get(&object)?, burst))
        })
        .collect()
}

/// Шаг симуляции частиц: сколько игрового времени прошло с последнего отправленного кадра.
/// Первый кадр шага не получает, чтобы время загрузки не превратилось в скачок.
fn particle_step(sim_time: f64, last_submitted: Option<f64>) -> f32 {
    last_submitted.map_or(0.0, |last| (sim_time - last).max(0.0) as f32)
}

/// Время для `rve.time` экранных шейдеров: остаток от 3600 считается в f64, иначе после долгой
/// игры f32 округляет дробную часть секунд.
fn screen_shader_time(sim_time: f64) -> f32 {
    sim_time.rem_euclid(3600.0) as f32
}

fn render_local_lights(states: &[LocalLightState]) -> (Vec<LocalLight>, Vec<LocalLightShadow>) {
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

fn resolve_environment_snapshot(
    state: Option<&EnvironmentState>,
    gpu_scene: &GpuSceneTable,
) -> Result<Option<SceneEnvironmentSnapshot>, String> {
    let Some(state) = state else {
        return Ok(None);
    };
    let component = &state.component;
    let (key, source) = match (&component.hdr, component.procedural) {
        (Some(path), None) => {
            let path = path.as_str().to_owned();
            let source = gpu_scene
                .environments
                .get(&path)
                .cloned()
                .ok_or_else(|| format!("не загружены HDR данные окружения {path}"))?;
            (
                EnvironmentCacheKey::Hdr(path.clone()),
                EnvironmentSnapshotSource::Hdr(source),
            )
        }
        (None, Some(procedural)) => {
            let world_sun = state.sun_direction.unwrap_or([0.0, 1.0, 0.0]);
            let world_sun = Vec3::from_array(world_sun).normalize_or_zero();
            let rotation = Mat3::from_cols_array_2d(&state.rotation);
            let local_sun = (rotation.transpose() * world_sun).normalize_or_zero();
            let sky = ProceduralSky {
                zenith_color: procedural.zenith_color,
                horizon_color: procedural.horizon_color,
                ground_color: procedural.ground_color,
                sun_size_deg: procedural.sun_size_deg,
                sun_intensity: procedural.sun_intensity,
                sun_direction: local_sun.to_array(),
            };
            (
                procedural_environment_key(sky),
                EnvironmentSnapshotSource::Procedural(sky),
            )
        }
        _ => return Err("окружение должно иметь ровно один источник".to_owned()),
    };
    Ok(Some(SceneEnvironmentSnapshot {
        key,
        source,
        intensity: component.intensity,
        rotation: state.rotation,
        skybox: component.skybox,
    }))
}

fn procedural_environment_key(sky: ProceduralSky) -> EnvironmentCacheKey {
    let colors_and_sun = [
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
    ];
    let sun_direction = sky
        .sun_direction
        .map(|value| (value * 4096.0).round() as i32);
    EnvironmentCacheKey::Procedural {
        colors_and_sun,
        sun_direction,
    }
}

fn selected_level<'a, T>(
    world: &World,
    object: ObjectHandle,
    eye: Vec3,
    levels: &'a [T],
) -> Option<(u8, &'a T)> {
    let level = world.lod_level(object, eye)?;
    Some((level.try_into().ok()?, levels.get(level)?))
}

/// Задаёт курсор скриптов из флагов активной камеры и публикует его до запуска скриптов.
fn set_initial_cursor(
    scripts: &mut ScriptHost,
    world: &World,
    cursor: &CursorControl,
    published: &mut Option<ScriptCursor>,
) {
    scripts.set_cursor(ScriptCursor::from(world.active_camera_cursor()));
    publish_script_cursor(scripts, cursor, published);
}

/// Публикует курсор скриптов, если он отличается от последней опубликованного значения.
fn publish_script_cursor(
    scripts: &ScriptHost,
    cursor: &CursorControl,
    published: &mut Option<ScriptCursor>,
) {
    let current = scripts.cursor();
    if *published != Some(current) {
        *published = Some(current);
        cursor.set_script_cursor(current);
    }
}

fn print_script_events(events: Vec<ScriptEvent>) {
    log_script_events_with(&events, log::logger());
}

#[cfg(feature = "dev-console")]
fn publish_command_catalog(
    scripts: &ScriptHost,
    shared: &Mutex<CommandCatalogSnapshot>,
    published_generation: &mut u64,
) {
    let generation = scripts.commands_generation();
    if generation == *published_generation {
        return;
    }
    let mut snapshot = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    snapshot.generation = generation;
    snapshot.catalog = scripts.command_catalog();
    let shadowed = snapshot.catalog.get(PROFILE_COMMAND).is_some();
    if shadowed && !snapshot.profile_shadowed {
        log::warn!(
            target: "console",
            "команда profile из скрипта затенена встроенной командой профайлера"
        );
    }
    snapshot.profile_shadowed = shadowed;
    if !shadowed
        && let Ok(spec) =
            engine_console::CommandSpec::new(PROFILE_COMMAND, PROFILE_COMMAND_DESCRIPTION)
    {
        let _ = snapshot.catalog.register(spec);
    }
    *published_generation = generation;
}

#[cfg(feature = "dev-console")]
fn execute_console_commands(
    scripts: &mut ScriptHost,
    world: &mut World,
    pending: &mut PendingConsoleCommands,
) -> Vec<(String, CommandError)> {
    let mut errors = Vec::new();
    while let Some(command) = pending.pop_front() {
        if let Err(error) = scripts.execute_command(world, &command) {
            errors.push((command.name, error));
        }
        print_script_events(scripts.drain_events());
    }
    errors
}

fn log_script_events_with(events: &[ScriptEvent], logger: &dyn log::Log) {
    for event in events {
        let (level, text) = match event.level {
            ScriptEventLevel::Info => (log::Level::Info, format!("[script] {event}")),
            ScriptEventLevel::Warn => (log::Level::Warn, format!("[script warn] {event}")),
            ScriptEventLevel::Error => (log::Level::Error, format!("[script] {event}")),
        };
        let args = format_args!("{text}");
        let record = log::Record::builder()
            .args(args)
            .level(level)
            .target(&event.source)
            .build();
        logger.log(&record);
    }
}

enum SendOutcome {
    Sent,
    Background,
    Shutdown,
    Disconnected,
}

enum ScriptSelection {
    Sent(Result<(), flume::SendError<Arc<FrameSnapshot>>>),
    Control(Result<ScriptControl, flume::RecvError>),
}

fn send_snapshot(
    frame_tx: &Sender<Arc<FrameSnapshot>>,
    control_rx: &Receiver<ScriptControl>,
    snapshot: Arc<FrameSnapshot>,
    background: &mut bool,
    #[cfg(feature = "dev-console")] pending_console_commands: &mut PendingConsoleCommands,
    #[cfg(not(feature = "dev-console"))] _: &mut PendingConsoleCommands,
    pending_prefab: &mut VecDeque<ScriptReply>,
) -> SendOutcome {
    loop {
        let selection = Selector::new()
            .send(frame_tx, snapshot.clone(), ScriptSelection::Sent)
            .recv(control_rx, ScriptSelection::Control)
            .wait();
        match selection {
            ScriptSelection::Sent(Ok(())) => return SendOutcome::Sent,
            ScriptSelection::Sent(Err(_)) | ScriptSelection::Control(Err(_)) => {
                return SendOutcome::Disconnected;
            }
            ScriptSelection::Control(Ok(ScriptControl::Shutdown)) => return SendOutcome::Shutdown,
            ScriptSelection::Control(Ok(ScriptControl::Background(value))) => {
                *background = value;
                if value {
                    return SendOutcome::Background;
                }
            }
            #[cfg(feature = "dev-console")]
            ScriptSelection::Control(Ok(ScriptControl::Console(command))) => {
                pending_console_commands.push_back(command);
            }
            // Latency и Go приходят только до первого кадра, поэтому здесь они не нужны.
            ScriptSelection::Control(Ok(ScriptControl::Go(_) | ScriptControl::Latency(_))) => {}
            ScriptSelection::Control(Ok(ScriptControl::Prefab(reply))) => {
                pending_prefab.push_back(ScriptReply::Prefab(reply));
            }
            ScriptSelection::Control(Ok(ScriptControl::Material(reply))) => {
                pending_prefab.push_back(ScriptReply::Material(reply));
            }
        }
    }
}

fn wait_background_tick(
    control_rx: &Receiver<ScriptControl>,
    clock: &dyn Clock,
    background: &mut bool,
    deadline: Duration,
    #[cfg(feature = "dev-console")] pending_console_commands: &mut PendingConsoleCommands,
    #[cfg(not(feature = "dev-console"))] _: &mut PendingConsoleCommands,
    pending_prefab: &mut VecDeque<ScriptReply>,
) -> bool {
    while *background {
        let now = clock.now();
        if now >= deadline {
            return true;
        }
        match clock.wait_for_control(control_rx, deadline - now) {
            Ok(ScriptControl::Shutdown) | Err(RecvTimeoutError::Disconnected) => return false,
            Ok(ScriptControl::Background(value)) => {
                *background = value;
                if !*background {
                    return true;
                }
            }
            #[cfg(feature = "dev-console")]
            Ok(ScriptControl::Console(command)) => pending_console_commands.push_back(command),
            Ok(ScriptControl::Prefab(reply)) => pending_prefab.push_back(ScriptReply::Prefab(reply)),
            Ok(ScriptControl::Material(reply)) => pending_prefab.push_back(ScriptReply::Material(reply)),
            Ok(ScriptControl::Go(_) | ScriptControl::Latency(_))
            | Err(RecvTimeoutError::Timeout) => {}
        }
    }
    true
}

pub(crate) fn start_render(
    modules: &mut ModuleHost,
    config: RenderThreadConfig,
) -> std::io::Result<()> {
    let stop_control = config.control_tx.clone();
    let stop_interrupt = config.interrupt.clone();
    modules.spawn(
        RENDER_THREAD,
        move || {
            stop_interrupt.interrupt();
            let _ = stop_control.send(RenderControl::Shutdown);
        },
        move || render_thread(config),
    )
}

fn render_thread(config: RenderThreadConfig) {
    let RenderThreadConfig {
        mut backend,
        width,
        height,
        frame_limit,
        frame_rx,
        resources,
        control_rx,
        script_tx,
        interrupt,
        status,
        resource_stats,
        stopped_tx,
        exit,
        ..
    } = config;
    let mut profiler =
        PhaseWindow::new(SOURCE_RENDER, Arc::clone(&status.profile), render_groups())
            .with_resources(resource_stats);
    let result = catch_unwind(AssertUnwindSafe(|| {
        render_loop(
            &mut *backend,
            width,
            height,
            frame_limit,
            &frame_rx,
            &resources,
            &control_rx,
            &script_tx,
            &mut profiler,
        )
    }));
    let failed = match result {
        Ok(Ok(())) => false,
        Ok(Err(message)) => {
            status.record_failure(message);
            true
        }
        Err(payload) => {
            status.record_failure(format!(
                "поток рендера завершился паникой: {}",
                panic_message(payload)
            ));
            true
        }
    };
    if failed {
        interrupt.interrupt();
    }
    let _ = script_tx.send(ScriptControl::Shutdown);
    #[cfg(feature = "dev-console")]
    if let Err(error) = backend.close_console() {
        status.record_failure(format!("ошибка освобождения UI консоли: {error}"));
    }
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| backend.release_scene())) {
        status.record_failure(format!(
            "ошибка освобождения ресурсов сцены: {}",
            panic_message(payload)
        ));
        interrupt.interrupt();
        let _ = script_tx.send(ScriptControl::Shutdown);
    }
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| backend.wait_idle())) {
        status.record_failure(format!(
            "ошибка ожидания renderer при завершении: {}",
            panic_message(payload)
        ));
        interrupt.interrupt();
        let _ = script_tx.send(ScriptControl::Shutdown);
    }
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(backend))) {
        status.record_failure(format!(
            "паника при уничтожении renderer: {}",
            panic_message(payload)
        ));
        interrupt.interrupt();
        let _ = script_tx.send(ScriptControl::Shutdown);
    }
    status.mark_render_stopped();
    let _ = stopped_tx.try_send(());
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| exit.request_exit())) {
        status.record_failure(format!(
            "паника при запросе закрытия окна: {}",
            panic_message(payload)
        ));
    }
}

enum RenderSelection {
    Frame(Result<Arc<FrameSnapshot>, flume::RecvError>),
    Control(Result<RenderControl, flume::RecvError>),
    Resource(Result<RenderResourceRequest, flume::RecvError>),
}

/// Освобождение, которое ждёт, пока render thread обработает снимки с `seq ≥ last_seq`.
struct PendingRelease {
    target: ReleaseTarget,
    last_seq: u64,
}

enum ReleaseTarget {
    Prefab(PrefabToken),
    Materials(Vec<MaterialId>),
}

#[allow(clippy::too_many_arguments)]
fn render_loop(
    backend: &mut dyn RenderBackend,
    mut width: u32,
    mut height: u32,
    frame_limit: Option<u64>,
    frame_rx: &Receiver<Arc<FrameSnapshot>>,
    resources: &Receiver<RenderResourceRequest>,
    control_rx: &Receiver<RenderControl>,
    script_tx: &Sender<ScriptControl>,
    profiler: &mut PhaseWindow,
) -> Result<(), String> {
    let mut occluded = false;
    let mut paused = false;
    let mut presented = 0_u64;
    let mut motion_history = MotionHistory::default();
    #[cfg(feature = "dev-console")]
    let mut debug_view_dirty = false;
    let mut latest_snapshot: Option<Arc<FrameSnapshot>> = None;
    // Номер последнего снимка, принятого из канала кадров.
    let mut taken_seq = 0_u64;
    let mut releases: Vec<PendingRelease> = Vec::new();
    // Команды частиц принятых снимков, которые ещё не отрисованы. Снимок копирует их при приёме,
    // поэтому перерисовка того же снимка и пауза не применяют и не теряют их.
    let mut particle_queue: Vec<engine_scene::world::ParticleCommand> = Vec::new();
    let mut resources_open = true;
    #[cfg(feature = "dev-console")]
    let mut ui_frame = ConsoleUiFrame::default();
    #[cfg(feature = "dev-console")]
    let mut console_active = false;
    loop {
        profiler.publish_if_due(Instant::now());
        #[cfg(feature = "dev-console")]
        let mut should_render = std::mem::take(&mut debug_view_dirty);
        #[cfg(not(feature = "dev-console"))]
        let mut should_render = false;
        loop {
            match control_rx.try_recv() {
                Ok(RenderControl::Resize(new_width, new_height)) => {
                    width = new_width;
                    height = new_height;
                    backend.resize(width, height);
                    #[cfg(feature = "dev-console")]
                    {
                        should_render = true;
                    }
                }
                Ok(RenderControl::Occluded(value)) => {
                    occluded = value;
                    #[cfg(feature = "dev-console")]
                    {
                        should_render = true;
                    }
                }
                #[cfg(feature = "dev-console")]
                Ok(RenderControl::DebugView(value)) => {
                    backend.set_debug_view(value);
                    should_render = true;
                }
                #[cfg(feature = "dev-console")]
                Ok(RenderControl::ConsoleUiChanged) => should_render = true,
                Ok(RenderControl::Shutdown) | Err(TryRecvError::Disconnected) => return Ok(()),
                Err(TryRecvError::Empty) => break,
            }
        }

        // Ресурсы префабов принимаются и на паузе: загрузка и освобождение не ждут новых снимков.
        let resources_started = Instant::now();
        if resources_open {
            loop {
                match resources.try_recv() {
                    Ok(request) => {
                        handle_resource_request(backend, request, &mut releases, script_tx);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        resources_open = false;
                        break;
                    }
                }
            }
        }
        finish_releases(backend, &mut releases, taken_seq, &mut latest_snapshot);
        if backend.has_pending_prefab_uploads() {
            for reply in backend.advance_prefab_uploads(PREFAB_UPLOAD_BUDGET) {
                send_prefab_reply(script_tx, reply);
            }
        }
        profiler.record(
            render_phase::LOOP,
            render_phase::RESOURCE_REQUESTS,
            resources_started.elapsed(),
        );

        #[cfg(feature = "dev-console")]
        if let Some(frame) = backend.take_console_ui() {
            ui_frame.replace_with(frame);
            should_render = true;
            if ui_frame.visible {
                console_active = true;
            } else if console_active {
                ui_frame.textures_delta.clear();
                backend.close_console()?;
                console_active = false;
            }
        }

        let next_paused = occluded || width == 0 || height == 0;
        if next_paused != paused {
            paused = next_paused;
            let _ = script_tx.send(ScriptControl::Background(paused));
            #[cfg(feature = "dev-console")]
            {
                should_render = true;
            }
        }
        if paused {
            #[cfg(feature = "dev-console")]
            {
                backend.discard_console_geometry();
                ui_frame.primitives.clear();
            }
            should_render = false;
        }

        if should_render
            && let Some(snapshot) = latest_snapshot.as_mut()
            && let Some(outcome) = render_snapshot(
                backend,
                Arc::make_mut(snapshot),
                &mut motion_history,
                &mut particle_queue,
                paused,
                width,
                height,
                #[cfg(feature = "dev-console")]
                &mut ui_frame,
                profiler,
            )?
            && outcome.status == FrameStatus::Presented
        {
            presented = presented.saturating_add(1);
            if presented == 1 {
                log_startup_profile(profiler.board());
            }
            if frame_limit.is_some_and(|limit| presented >= limit) {
                return Ok(());
            }
        }

        let mut selector = Selector::new()
            .recv(frame_rx, RenderSelection::Frame)
            .recv(control_rx, RenderSelection::Control);
        if resources_open {
            selector = selector.recv(resources, RenderSelection::Resource);
        }
        // Пока загрузка не закончена, цикл не блокируется: следующая итерация продолжит её.
        let wait_started = Instant::now();
        let selection = if backend.has_pending_prefab_uploads() {
            selector.wait_timeout(Duration::ZERO).ok()
        } else {
            Some(selector.wait())
        };
        profiler.record(
            render_phase::LOOP,
            render_phase::FRAME_WAIT,
            wait_started.elapsed(),
        );
        let Some(selection) = selection else {
            continue;
        };
        match selection {
            RenderSelection::Resource(Ok(request)) => {
                let started = Instant::now();
                handle_resource_request(backend, request, &mut releases, script_tx);
                profiler.record(
                    render_phase::LOOP,
                    render_phase::RESOURCE_REQUESTS,
                    started.elapsed(),
                );
            }
            RenderSelection::Resource(Err(_)) => resources_open = false,
            RenderSelection::Control(Ok(RenderControl::Resize(new_width, new_height))) => {
                width = new_width;
                height = new_height;
                backend.resize(width, height);
            }
            RenderSelection::Control(Ok(RenderControl::Occluded(value))) => occluded = value,
            #[cfg(feature = "dev-console")]
            RenderSelection::Control(Ok(RenderControl::DebugView(value))) => {
                backend.set_debug_view(value);
                debug_view_dirty = true;
            }
            #[cfg(feature = "dev-console")]
            RenderSelection::Control(Ok(RenderControl::ConsoleUiChanged)) => {}
            RenderSelection::Control(Ok(RenderControl::Shutdown))
            | RenderSelection::Control(Err(_))
            | RenderSelection::Frame(Err(_)) => return Ok(()),
            RenderSelection::Frame(Ok(snapshot)) => {
                taken_seq = snapshot.sequence;
                particle_queue.extend_from_slice(&snapshot.particle_commands);
                latest_snapshot = Some(snapshot);
                let Some(snapshot) = latest_snapshot.as_mut() else {
                    continue;
                };
                let Some(outcome) = render_snapshot(
                    backend,
                    Arc::make_mut(snapshot),
                    &mut motion_history,
                    &mut particle_queue,
                    paused,
                    width,
                    height,
                    #[cfg(feature = "dev-console")]
                    &mut ui_frame,
                    profiler,
                )?
                else {
                    continue;
                };
                if outcome.status == FrameStatus::Presented {
                    presented = presented.saturating_add(1);
                    if presented == 1 {
                        log_startup_profile(profiler.board());
                    }
                    if frame_limit.is_some_and(|limit| presented >= limit) {
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn handle_resource_request(
    backend: &mut dyn RenderBackend,
    request: RenderResourceRequest,
    releases: &mut Vec<PendingRelease>,
    script_tx: &Sender<ScriptControl>,
) {
    match request {
        RenderResourceRequest::Upload(prefab) => {
            if let Some(reply) = backend.submit_prefab(*prefab) {
                send_prefab_reply(script_tx, reply);
            }
        }
        RenderResourceRequest::Particle(request) => backend.apply_particle_request(request),
        RenderResourceRequest::Release { token, last_seq } => {
            releases.push(PendingRelease {
                target: ReleaseTarget::Prefab(token),
                last_seq,
            });
        }
        RenderResourceRequest::Material(request) => {
            let reply = backend.submit_material(*request);
            let _ = script_tx.send(ScriptControl::Material(Box::new(reply)));
        }
        RenderResourceRequest::ReleaseMaterials {
            materials,
            last_seq,
        } => {
            releases.push(PendingRelease {
                target: ReleaseTarget::Materials(materials),
                last_seq,
            });
        }
    }
}

/// Уничтожает GPU-ресурсы освобождённых префабов, когда render thread принял снимок с
/// `seq ≥ last_seq`. Удерживаемый для перерисовки снимок с `seq ≤ last_seq` сбрасывается:
/// он может ссылаться на уничтожаемые ресурсы.
fn finish_releases(
    backend: &mut dyn RenderBackend,
    releases: &mut Vec<PendingRelease>,
    taken_seq: u64,
    latest: &mut Option<Arc<FrameSnapshot>>,
) {
    releases.retain(|release| {
        if taken_seq < release.last_seq {
            return true;
        }
        if latest
            .as_ref()
            .is_some_and(|snapshot| snapshot.sequence <= release.last_seq)
        {
            *latest = None;
        }
        match &release.target {
            ReleaseTarget::Prefab(token) => backend.release_prefab(token),
            ReleaseTarget::Materials(materials) => backend.release_materials(materials),
        }
        false
    });
}

fn send_prefab_reply(script_tx: &Sender<ScriptControl>, reply: PrefabReply) {
    let _ = script_tx.send(ScriptControl::Prefab(reply));
}

#[allow(clippy::too_many_arguments)]
fn render_snapshot(
    backend: &mut dyn RenderBackend,
    snapshot: &mut FrameSnapshot,
    motion_history: &mut MotionHistory,
    particle_queue: &mut Vec<engine_scene::world::ParticleCommand>,
    paused: bool,
    width: u32,
    height: u32,
    #[cfg(feature = "dev-console")] ui_frame: &mut ConsoleUiFrame,
    profiler: &mut PhaseWindow,
) -> Result<Option<FrameOutcome>, String> {
    if paused {
        return Ok(None);
    }
    if !particle_queue.is_empty() {
        let commands = std::mem::take(particle_queue);
        backend.apply_particle_commands(&commands);
    }
    motion_history.prepare(snapshot);
    let started = Instant::now();
    #[cfg(feature = "dev-console")]
    let outcome = if ui_frame.visible {
        backend.render_console(snapshot, width, height, ui_frame)?
    } else {
        backend.render(snapshot, width, height)?
    };
    #[cfg(not(feature = "dev-console"))]
    let outcome = backend.render(snapshot, width, height)?;
    profiler.record(
        render_phase::LOOP,
        render_phase::RENDER_BACKEND,
        started.elapsed(),
    );
    if let Some(frame) = backend.take_frame_profile() {
        record_frame_profile(profiler, &frame);
    }
    if outcome.submitted {
        motion_history.submitted(snapshot);
    }
    Ok(Some(outcome))
}

/// Переводит постобработку камеры в настройки renderer.
///
/// Выключенная экспозиция становится ручной с EV 0, выключенный tonemapping — `None`. Выключенные
/// эффекты сохраняют параметры, но renderer их не выполняет, поэтому `enabled = false` совпадает
/// с тождественным преобразованием.
pub fn render_post_process(post: &PostProcessSettings) -> PostProcess {
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
    let tonemapper = if post.tonemapping.enabled {
        match post.tonemapping.tonemapper {
            Tonemapper::Aces => RenderTonemapper::Aces,
            Tonemapper::Neutral => RenderTonemapper::Neutral,
            Tonemapper::None => RenderTonemapper::None,
        }
    } else {
        RenderTonemapper::None
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
            enabled: dof.enabled,
            focus_distance: dof.focus_distance,
            aperture: dof.aperture,
            focal_length_mm: dof.focal_length_mm,
            max_blur_px: dof.max_blur_px,
        },
        motion_blur: engine_render::MotionBlur {
            enabled: motion.enabled,
            shutter_angle_deg: motion.shutter_angle_deg,
            samples: motion.samples,
            max_blur_px: motion.max_blur_px,
        },
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "dev-console")]
    use super::CommandCatalogSnapshot;
    #[cfg(feature = "dev-console")]
    use super::ConsoleUiFrame;
    #[cfg(feature = "dev-console")]
    use super::ConsoleUiSlot;
    use super::{MaterialOwnership, MaterialSync, ScriptReply};
    use crate::prefabs::{MaterialLevels, MaterialReply, MaterialRequest};
    use engine_scene::world::MeshMaterialParams;
    use flume::Receiver;
    #[cfg(feature = "dev-console")]
    use super::execute_console_commands;
    use super::{
        Clock, EnvironmentCacheKey, EnvironmentSnapshotSource, ExitRequester, FrameSnapshot,
        GpuSceneTable, MotionHistory, PhysicsWait, RenderBackend, RenderControl,
        RenderThreadConfig, RuntimeStatus, SCRIPTS_THREAD, ScriptControl, ScriptStartupError,
        ScriptThreadConfig, apply_physics_step, drop_mismatched_skins, log_script_events_with,
        make_snapshot, procedural_environment_key, receive_physics_interruptibly,
        render_local_lights, render_loop, render_snapshot, render_thread, selected_level,
        send_snapshot, sync_input_background, wait_background_tick,
    };
    use super::{FrameLatency, publish_script_cursor, set_initial_cursor};
    #[cfg(feature = "dev-console")]
    use super::{PROFILE_COMMAND, PROFILE_COMMAND_DESCRIPTION, publish_command_catalog};
    use super::{SpawnPhysics, apply_world_changes, drain_prefab_replies};
    use super::{attach_prefab_levels, detach_objects};
    use crate::cursor::{CursorControl, RecordingCursorSink};
    use crate::input::{InputAccumulator, lock_input};
    use crate::module_host::ModuleHost;
    use crate::pipeline::start_scripts;
    use crate::prefabs::{
        LoadedPrefab, LoaderRequest, PrefabCpu, PrefabLoads, PrefabReady, PrefabReply, PrefabToken,
        RenderResourceRequest,
    };
    use crate::profiling::{PhaseWindow, SOURCE_RENDER, render_groups, render_phase};
    use crate::scene::PreparedPrefabs;
    use engine_animation::AnimationBridge;
    #[cfg(feature = "dev-console")]
    use engine_console::CommandLine;
    use engine_console::{ConsoleBuffer, ConsoleLevel, ConsoleLogger};
    use engine_core::profile::{ProfileBoard, ProfileValue};
    use engine_physics::{BodyChange, BodyKind};
    use engine_physics::{
        PhysicsClient, PhysicsCommand, PhysicsEvent, PhysicsInitialization, PhysicsReply,
        PhysicsRequest, PhysicsSceneDesc, PhysicsSceneSync, PhysicsServer, PhysicsStepResult,
        PhysicsWorld, Pose,
    };
    use engine_project::{InputAction, InputSettings};
    use engine_render::{DrawCommand, FrameOutcome, FrameStatus, LatencyMarker, LocalLight};
    use engine_render::{EnvironmentSource, ProceduralSky};
    use engine_scene::CameraSettings;
    use engine_scene::camera::Camera;
    use engine_scene::world::LightState;
    use engine_scene::{LocalLightState, ObjectHandle, SceneDocument, World};
    use engine_script::ScriptLoadOptions;
    use engine_script::{
        Key, ScriptEvent, ScriptEventLevel, ScriptHost, ScriptInterruptHandle, ScriptTime,
    };
    use engine_script::{PrefabCatalog, PrefabKind, PrefabSpec};
    use engine_window::CursorState;
    use glam::Vec3;
    use rve_sdk::{Context, Params, Script};
    use std::collections::BTreeMap;
    use std::collections::HashMap;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    fn test_input() -> Arc<Mutex<InputAccumulator>> {
        Arc::new(Mutex::new(InputAccumulator::default()))
    }

    #[test]
    fn scripts_receive_project_input_settings() {
        let document = SceneDocument::parse(
            "input.rvescene",
            r#"
[[assets]]
path = "/input.lua"
[[objects]]
id = "camera"
[objects.camera]
active = true
[[objects.lite_scripts]]
path = "/input.lua"
"#,
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let script = r#"
local Input = {}
function Input:on_update()
  if rve.input.action_pressed("Jump") then
    self.transform.position = { x = 9, y = 0, z = 0 }
  end
end
return Input
"#;
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let input = test_input();
        {
            let mut accumulator = lock_input(&input);
            accumulator.focused(true);
            accumulator.key(engine_window::Key::Space, true);
        }
        let mut modules = ModuleHost::new();
        let thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "input.rvescene".to_owned(),
                cursor: CursorControl::default(),
                sources: BTreeMap::from([("input.lua".to_owned(), script.to_owned())]),
                input_settings: InputSettings {
                    gamepad: Default::default(),
                    default_context: Some("game".to_owned()),
                    contexts: BTreeMap::from([(
                        "game".to_owned(),
                        BTreeMap::from([(
                            "Jump".to_owned(),
                            InputAction::Button {
                                inputs: vec!["Space".to_owned()],
                            },
                        )]),
                    )]),
                },
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                physics_client: None,
                physics_initialization: None,
                audio: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        thread.ready.recv().unwrap().unwrap();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();
        let snapshot = frame_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(snapshot.camera.position.x, 9.0);

        script_tx.send(ScriptControl::Shutdown).unwrap();
        thread.stopped.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(render_rx.recv().unwrap(), RenderControl::Shutdown);
        modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
    }

    #[cfg(feature = "dev-console")]
    fn test_command_catalog() -> Arc<Mutex<CommandCatalogSnapshot>> {
        Arc::new(Mutex::new(CommandCatalogSnapshot::default()))
    }

    #[test]
    fn script_events_keep_their_log_levels_and_prefixes() {
        let sink = Arc::new(Mutex::new(ConsoleBuffer::with_capacity(4)));
        let logger = ConsoleLogger::new(Some(sink.clone()));
        log_script_events_with(
            &[
                ScriptEvent {
                    level: ScriptEventLevel::Info,
                    source: "scene/script".into(),
                    message: "info".into(),
                },
                ScriptEvent {
                    level: ScriptEventLevel::Warn,
                    source: "scene/script".into(),
                    message: "warn".into(),
                },
                ScriptEvent {
                    level: ScriptEventLevel::Error,
                    source: "scene/script".into(),
                    message: "error".into(),
                },
            ],
            &logger,
        );
        let lines = sink.lock().unwrap().snapshot();
        assert_eq!(
            lines.iter().map(|line| line.level).collect::<Vec<_>>(),
            [ConsoleLevel::Info, ConsoleLevel::Warn, ConsoleLevel::Error]
        );
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            [
                "[script] scene/script: info",
                "[script warn] scene/script: warn",
                "[script] scene/script: error"
            ]
        );
        assert_eq!(lines[0].source, "scene/script");
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeMeshId(usize);

    #[test]
    fn motion_history_tracks_submitted_models_by_object_handle() {
        let (camera_a, _, first, second) = motion_test_handles();
        let first_model = [[1.0; 4]; 4];
        let second_model = [[2.0; 4]; 4];
        let next_model = [[3.0; 4]; 4];
        let mut history = MotionHistory::default();
        let mut backend = FakeBackend::default();
        let mut snapshot =
            motion_snapshot(camera_a, &[(first, first_model), (second, second_model)]);

        render_test_snapshot(&mut backend, &mut snapshot, &mut history, false).unwrap();
        assert_eq!(history.previous_models.get(&first), Some(&first_model));
        assert_eq!(history.previous_models.get(&second), Some(&second_model));
        assert_eq!(history.previous_model(first, false), Some(first_model));

        let mut snapshot = motion_snapshot(camera_a, &[(first, next_model)]);
        assert_eq!(history.previous_model(first, false), Some(first_model));
        render_test_snapshot(&mut backend, &mut snapshot, &mut history, false).unwrap();
        assert_eq!(history.previous_models.len(), 1);
        assert_eq!(history.previous_models.get(&first), Some(&next_model));
        assert!(!history.previous_models.contains_key(&second));

        // Re-rendering the same latest snapshot uses its already-submitted transform.
        assert_eq!(history.previous_model(first, false), Some(next_model));
        render_test_snapshot(&mut backend, &mut snapshot, &mut history, false).unwrap();
        assert_eq!(backend.renders, 3);
        assert!(!snapshot.reset_history);
    }

    #[test]
    fn motion_history_advances_only_after_a_successful_queue_submission() {
        let (camera_a, _, first, _) = motion_test_handles();
        let model = [[1.0; 4]; 4];
        let mut snapshot = motion_snapshot(camera_a, &[(first, model)]);
        let mut history = MotionHistory::default();
        let mut backend = FakeBackend {
            outcome: Some(FrameOutcome {
                status: FrameStatus::Skipped,
                submitted: false,
            }),
            ..Default::default()
        };

        assert!(
            render_test_snapshot(&mut backend, &mut snapshot, &mut history, true)
                .unwrap()
                .is_none()
        );
        assert!(history.previous_models.is_empty());

        backend.fail = true;
        assert!(render_test_snapshot(&mut backend, &mut snapshot, &mut history, false).is_err());
        assert!(history.previous_models.is_empty());

        backend.fail = false;
        render_test_snapshot(&mut backend, &mut snapshot, &mut history, false).unwrap();
        assert!(history.previous_models.is_empty());

        // A present can be skipped after submit (for example, OUT_OF_DATE); the submitted
        // scene frame still advances temporal model history.
        backend.outcome = Some(FrameOutcome {
            status: FrameStatus::Skipped,
            submitted: true,
        });
        render_test_snapshot(&mut backend, &mut snapshot, &mut history, false).unwrap();
        assert_eq!(history.previous_models.get(&first), Some(&model));
    }

    #[test]
    fn camera_reset_persists_until_submission_then_is_consumed_once() {
        let (camera_a, camera_b, first, _) = motion_test_handles();
        let first_model = [[1.0; 4]; 4];
        let second_model = [[2.0; 4]; 4];
        let mut history = MotionHistory::default();
        let mut backend = FakeBackend::default();
        let mut first_frame = motion_snapshot(camera_a, &[(first, first_model)]);
        render_test_snapshot(&mut backend, &mut first_frame, &mut history, false).unwrap();

        let mut switched_frame = motion_snapshot(camera_b, &[(first, second_model)]);
        backend.outcome = Some(FrameOutcome {
            status: FrameStatus::Skipped,
            submitted: false,
        });
        render_test_snapshot(&mut backend, &mut switched_frame, &mut history, false).unwrap();
        assert!(switched_frame.reset_history);
        assert_eq!(history.active_camera, Some(camera_a));
        assert_eq!(history.previous_models.get(&first), Some(&first_model));

        backend.outcome = Some(FrameOutcome {
            status: FrameStatus::Presented,
            submitted: true,
        });
        render_test_snapshot(&mut backend, &mut switched_frame, &mut history, false).unwrap();
        assert!(switched_frame.reset_history);
        assert_eq!(history.active_camera, Some(camera_b));
        assert_eq!(history.previous_models.get(&first), Some(&second_model));

        render_test_snapshot(&mut backend, &mut switched_frame, &mut history, false).unwrap();
        assert!(!switched_frame.reset_history);
    }

    #[test]
    fn snapshot_level_selection_uses_distance_and_skips_culled_objects() {
        let input = r#"
[[assets]]
path="/models/base.fbx"
[[assets]]
path="/models/lod.fbx"
[[objects]]
id="camera"
[objects.camera]
[[objects]]
id="mesh"
[objects.transform]
position=[0.0,0.0,-5.0]
[objects.mesh_renderer]
model="/models/base.fbx"
[objects.lod_group]
cull_distance=8.0
[[objects.lod_group.levels]]
model="/models/lod.fbx"
distance=2.0
"#;
        let document = SceneDocument::parse("snapshot.rvescene", input).unwrap();
        let world = World::from_document(&document).unwrap();
        let object = world.handle("mesh").unwrap();
        let level_meshes = [vec![FakeMeshId(1)], vec![FakeMeshId(2)]];

        assert_eq!(
            selected_level(&world, object, Vec3::ZERO, &level_meshes),
            Some((1, &level_meshes[1]))
        );
        assert_eq!(
            selected_level(&world, object, Vec3::new(0.0, 0.0, -4.5), &level_meshes),
            Some((0, &level_meshes[0]))
        );
        assert_eq!(
            selected_level(&world, object, Vec3::new(0.0, 0.0, 3.0), &level_meshes),
            None
        );
    }

    #[derive(Default)]
    struct FakeBackend {
        renders: usize,
        shared_renders: Option<Arc<AtomicUsize>>,
        outcome: Option<FrameOutcome>,
        fail: bool,
        panic: bool,
        panic_wait_idle: bool,
        #[cfg(feature = "dev-console")]
        console_slot: Option<Arc<ConsoleUiSlot>>,
        #[cfg(feature = "dev-console")]
        console_rendered: Option<mpsc::Sender<()>>,
        /// Загрузки, принятые без GPU: готовы на следующем `advance` с пустыми уровнями.
        prefab_uploads: VecDeque<LoadedPrefab>,
        /// Куда сообщать об освобождении токенов: тест ждёт событие, а не опрашивает состояние.
        released: Option<mpsc::Sender<PrefabToken>>,
        /// Куда сообщать о смене debug view: тест ждёт, что цикл уже обработал её.
        #[cfg(feature = "dev-console")]
        debug_views: Option<mpsc::Sender<engine_render::DebugView>>,
        /// Пачки команд частиц, принятые `apply_particle_commands`: тест считает применения.
        applied_particles: Option<mpsc::Sender<Vec<engine_scene::world::ParticleCommand>>>,
    }

    impl RenderBackend for FakeBackend {
        fn resize(&mut self, _width: u32, _height: u32) {}

        #[cfg(feature = "dev-console")]
        fn set_debug_view(&mut self, debug_view: engine_render::DebugView) {
            if let Some(debug_views) = &self.debug_views {
                let _ = debug_views.send(debug_view);
            }
        }

        fn submit_prefab(&mut self, prefab: LoadedPrefab) -> Option<PrefabReply> {
            self.prefab_uploads.push_back(prefab);
            None
        }

        fn advance_prefab_uploads(&mut self, _budget: usize) -> Vec<PrefabReply> {
            self.prefab_uploads
                .drain(..)
                .map(|prefab| {
                    PrefabReply::Ready(Box::new(PrefabReady {
                        token: prefab.token,
                        gpu: Vec::new(),
                        particles: Vec::new(),
                        cpu: prefab.cpu,
                    }))
                })
                .collect()
        }

        fn has_pending_prefab_uploads(&self) -> bool {
            !self.prefab_uploads.is_empty()
        }

        fn release_prefab(&mut self, token: &PrefabToken) {
            if let Some(released) = &self.released {
                let _ = released.send(token.clone());
            }
        }

        fn apply_particle_commands(&mut self, commands: &[engine_scene::world::ParticleCommand]) {
            if let Some(applied) = &self.applied_particles {
                let _ = applied.send(commands.to_vec());
            }
        }

        fn render(
            &mut self,
            _snapshot: &FrameSnapshot,
            _width: u32,
            _height: u32,
        ) -> Result<FrameOutcome, String> {
            self.renders += 1;
            if let Some(renders) = &self.shared_renders {
                renders.fetch_add(1, Ordering::SeqCst);
            }
            assert!(!self.panic, "fake render panic");
            if self.fail {
                Err("fake render failure".to_owned())
            } else {
                Ok(self.outcome.unwrap_or(FrameOutcome {
                    status: FrameStatus::Presented,
                    submitted: true,
                }))
            }
        }

        fn wait_idle(&mut self) {
            assert!(!self.panic_wait_idle, "fake wait_idle panic");
        }

        #[cfg(feature = "dev-console")]
        fn take_console_ui(&mut self) -> Option<ConsoleUiFrame> {
            self.console_slot.as_ref().and_then(|slot| slot.take())
        }

        #[cfg(feature = "dev-console")]
        fn render_console(
            &mut self,
            snapshot: &FrameSnapshot,
            width: u32,
            height: u32,
            ui: &mut ConsoleUiFrame,
        ) -> Result<FrameOutcome, String> {
            assert!(ui.visible);
            let status = self.render(snapshot, width, height)?;
            if let Some(rendered) = &self.console_rendered {
                let _ = rendered.send(());
            }
            Ok(status)
        }
    }

    fn render_test_snapshot(
        backend: &mut FakeBackend,
        snapshot: &mut FrameSnapshot,
        history: &mut MotionHistory,
        paused: bool,
    ) -> Result<Option<FrameOutcome>, String> {
        #[cfg(feature = "dev-console")]
        let mut ui_frame = ConsoleUiFrame::default();
        render_snapshot(
            backend,
            snapshot,
            history,
            &mut Vec::new(),
            paused,
            640,
            480,
            #[cfg(feature = "dev-console")]
            &mut ui_frame,
            &mut render_test_window(),
        )
    }

    #[derive(Default)]
    struct FakeClock {
        time: Mutex<Duration>,
        waiting: Mutex<Option<mpsc::Sender<()>>>,
    }

    impl FakeClock {
        fn advance(&self, elapsed: Duration) {
            *self.time.lock().unwrap() += elapsed;
        }

        fn observe_waits(&self) -> mpsc::Receiver<()> {
            let (waiting_tx, waiting_rx) = mpsc::channel();
            *self.waiting.lock().unwrap() = Some(waiting_tx);
            waiting_rx
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            *self.time.lock().unwrap()
        }

        fn wait_for_control(
            &self,
            control_rx: &flume::Receiver<ScriptControl>,
            _timeout: Duration,
        ) -> Result<ScriptControl, flume::RecvTimeoutError> {
            if let Some(waiting) = self.waiting.lock().unwrap().as_ref() {
                let _ = waiting.send(());
            }
            control_rx
                .recv()
                .map_err(|_| flume::RecvTimeoutError::Disconnected)
        }
    }

    static DESTROY_CALLS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct InputObservation {
        key_down: bool,
        key_pressed: bool,
        key_released: bool,
    }

    static INPUT_PROBE: Mutex<Option<(mpsc::Sender<InputObservation>, mpsc::Receiver<()>)>> =
        Mutex::new(None);

    struct InputProbe;

    impl Script for InputProbe {
        fn on_update(&mut self, ctx: &mut Context<'_>, _dt: f32) {
            let input = ctx.input();
            let observation = InputObservation {
                key_down: input.key_down(Key::A),
                key_pressed: input.key_pressed(Key::A),
                key_released: input.key_released(Key::A),
            };
            let probe = INPUT_PROBE.lock().unwrap();
            let (observations, continue_rx) = probe.as_ref().unwrap();
            observations.send(observation).unwrap();
            continue_rx.recv().unwrap();
        }
    }

    fn input_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(InputProbe))
    }

    struct DestroyProbe;

    impl Script for DestroyProbe {
        fn on_destroy(&mut self, _ctx: &mut Context<'_>) {
            DESTROY_CALLS.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn destroy_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(DestroyProbe))
    }

    struct TickProbe;

    impl Script for TickProbe {
        fn on_update(&mut self, ctx: &mut Context<'_>, dt: f32) {
            ctx.transform.position[0] += dt;
        }
    }

    fn tick_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
        Ok(Box::new(TickProbe))
    }

    fn script_module() -> &'static rve_sdk::abi::RveModule {
        rve_sdk::script_module! {
            "DestroyProbe" => destroy_probe_factory,
            "TickProbe" => tick_probe_factory,
        }
    }

    fn input_probe_module() -> &'static rve_sdk::abi::RveModule {
        rve_sdk::script_module! {
            "InputProbe" => input_probe_factory,
        }
    }

    struct FakeExit(mpsc::Sender<()>);

    impl ExitRequester for FakeExit {
        fn request_exit(&self) {
            let _ = self.0.send(());
        }
    }

    fn empty_snapshot() -> FrameSnapshot {
        let active_camera = motion_test_handles().0;
        FrameSnapshot {
            particles: Vec::new(),
            particle_commands: Vec::new(),
            sim_time: 0.0,
            sequence: 0,
            camera: Camera {
                position: Vec3::new(0.0, 0.0, 4.0),
                target: Vec3::ZERO,
                up: Vec3::Y,
                fov_narrow: std::f32::consts::FRAC_PI_4,
                near: 0.1,
                far: 100.0,
            },
            active_camera,
            view: CameraSettings::default(),
            post_process: engine_scene::PostProcessSettings::default(),
            screen_shaders: engine_render::ScreenShaders::default(),
            reset_history: false,
            light: LightState::default(),
            local_lights: Vec::new(),
            local_light_shadows: Vec::new(),
            environment: None,
            draws: Vec::<DrawCommand>::new(),
            joint_palettes: Vec::new(),
            object_motion: HashMap::new(),
        }
    }

    fn motion_test_handles() -> (ObjectHandle, ObjectHandle, ObjectHandle, ObjectHandle) {
        let document = SceneDocument::parse(
            "motion.rvescene",
            "[[objects]]\nid=\"camera_a\"\n[objects.camera]\nactive=true\n[[objects]]\nid=\"camera_b\"\n[objects.camera]\n[[objects]]\nid=\"first\"\n[[objects]]\nid=\"second\"\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        (
            world.handle("camera_a").unwrap(),
            world.handle("camera_b").unwrap(),
            world.handle("first").unwrap(),
            world.handle("second").unwrap(),
        )
    }

    fn motion_snapshot(
        active_camera: ObjectHandle,
        objects: &[(ObjectHandle, [[f32; 4]; 4])],
    ) -> FrameSnapshot {
        FrameSnapshot {
            particles: Vec::new(),
            particle_commands: Vec::new(),
            sim_time: 0.0,
            sequence: 0,
            camera: Camera {
                position: Vec3::new(0.0, 0.0, 4.0),
                target: Vec3::ZERO,
                up: Vec3::Y,
                fov_narrow: std::f32::consts::FRAC_PI_4,
                near: 0.1,
                far: 100.0,
            },
            active_camera,
            view: CameraSettings::default(),
            post_process: engine_scene::PostProcessSettings::default(),
            screen_shaders: engine_render::ScreenShaders::default(),
            reset_history: false,
            light: LightState::default(),
            local_lights: Vec::new(),
            local_light_shadows: Vec::new(),
            environment: None,
            joint_palettes: Vec::new(),
            draws: Vec::new(),
            object_motion: objects
                .iter()
                .map(|(object, model)| {
                    (
                        *object,
                        super::ObjectMotion {
                            model: *model,
                            draw_range: 0..0,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn runtime_maps_scene_local_lights_to_renderer_factors_and_cosines() {
        let scene = [
            LocalLightState::Point {
                position: [1.0, 2.0, 3.0],
                color: [0.5, 1.0, 2.0],
                intensity: 4.0,
                range: 8.0,
                shadows: true,
            },
            LocalLightState::Spot {
                position: [0.0, 1.0, 2.0],
                direction: [0.0, 0.0, -1.0],
                color: [1.0, 0.5, 0.25],
                intensity: 2.0,
                range: 12.0,
                inner_angle_deg: 10.0,
                outer_angle_deg: 25.0,
                shadows: false,
            },
        ];
        let (mapped, shadow_matrices) = render_local_lights(&scene);
        assert_eq!(shadow_matrices.len(), 1);
        assert_eq!(shadow_matrices[0].light_index, 0);
        assert!(matches!(
            shadow_matrices[0].matrices,
            engine_render::LocalLightShadowMatrices::Point { .. }
        ));
        assert_eq!(
            mapped[0],
            LocalLight::Point {
                position: [1.0, 2.0, 3.0],
                color_intensity: [2.0, 4.0, 8.0],
                range: 8.0,
            }
        );
        assert!(matches!(
            mapped[1],
            LocalLight::Spot {
                color_intensity: [2.0, 1.0, 0.5],
                range: 12.0,
                cos_inner_angle,
                cos_outer_angle,
                ..
            } if (cos_inner_angle - 10.0_f32.to_radians().cos()).abs() < 1e-6
                && (cos_outer_angle - 25.0_f32.to_radians().cos()).abs() < 1e-6
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
            [engine_render::LocalLightShadow {
                light_index: 0,
                matrices: engine_render::LocalLightShadowMatrices::Spot { .. },
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
    fn frame_snapshot_preserves_active_camera_view_settings() {
        let document = engine_scene::SceneDocument::parse(
            "scene.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
        let mut world = engine_scene::World::from_document(&document).unwrap();
        let mut settings = CameraSettings::default();
        settings.render.render_scale = 0.5;
        settings.render.clear_color = [0.2, 0.3, 0.4];
        settings.render.anti_aliasing = engine_scene::AntiAliasing::None;
        settings
            .color
            .get_or_insert_with(Default::default)
            .exposure_ev = 1.0;
        settings
            .color
            .get_or_insert_with(Default::default)
            .tonemapper = engine_scene::Tonemapper::Neutral;
        world
            .set_camera_settings(world.active_camera(), settings)
            .unwrap();
        let (frame, _) = world.build_frame();
        let snapshot = make_snapshot(
            frame,
            &world,
            &GpuSceneTable::default(),
            &AnimationBridge::default(),
        )
        .unwrap();
        assert_eq!(snapshot.view, settings);
    }

    #[test]
    fn hdr_environment_is_resolved_from_prepared_scene_assets() {
        let document = SceneDocument::parse(
            "scene.rvescene",
            "[[assets]]\npath=\"/environment/sky.hdr\"\n[[objects]]\nid=\"camera\"\n[objects.camera]\n[[objects]]\nid=\"sky\"\n[objects.environment]\nhdr=\"/environment/sky.hdr\"\n",
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let (frame, warnings) = world.build_frame();
        assert!(warnings.is_empty());
        assert!(
            make_snapshot(
                frame.clone(),
                &world,
                &GpuSceneTable::default(),
                &AnimationBridge::default()
            )
            .is_err()
        );

        let source = Arc::new(EnvironmentSource::Equirect {
            width: 2,
            height: 1,
            rgb: vec![0.1; 6],
        });
        let scene = GpuSceneTable {
            environments: HashMap::from([("environment/sky.hdr".to_owned(), source)]),
            ..GpuSceneTable::default()
        };
        let snapshot = make_snapshot(frame, &world, &scene, &AnimationBridge::default()).unwrap();
        let environment = snapshot.environment.unwrap();
        assert_eq!(
            environment.key,
            EnvironmentCacheKey::Hdr("environment/sky.hdr".to_owned())
        );
        assert!(matches!(
            environment.source,
            EnvironmentSnapshotSource::Hdr(source)
                if matches!(source.as_ref(), EnvironmentSource::Equirect { width: 2, height: 1, .. })
        ));
    }

    #[test]
    fn procedural_environment_cache_tracks_quantized_sun_direction() {
        let sky = ProceduralSky {
            zenith_color: [0.2, 0.35, 0.65],
            horizon_color: [0.65, 0.7, 0.75],
            ground_color: [0.12, 0.11, 0.1],
            sun_size_deg: 1.0,
            sun_intensity: 1.0,
            sun_direction: [0.0, 1.0, 0.0],
        };
        let key = procedural_environment_key(sky);
        assert_eq!(
            key,
            procedural_environment_key(ProceduralSky {
                sun_direction: [0.0, 1.0, 0.00005],
                ..sky
            })
        );
        assert_ne!(
            key,
            procedural_environment_key(ProceduralSky {
                sun_direction: [0.0, 1.0, 0.0003],
                ..sky
            })
        );
    }

    #[test]
    fn lua_camera_exposure_update_is_visible_in_the_next_frame_snapshot() {
        let document = SceneDocument::parse(
            "scene.rvescene",
            r#"
[[assets]]
path = "/exposure.lua"
[[objects]]
id = "camera"
[objects.camera]
active = true
[[objects.lite_scripts]]
path = "/exposure.lua"
[[objects]]
id = "second"
[objects.camera]
"#,
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let script = r#"
local Script = {}
function Script:on_update()
  rve.active_camera():set_camera_settings({ color = { exposure_ev = 2.5 } })
end
return Script
"#;
        let sources = BTreeMap::from([("exposure.lua".to_owned(), script.to_owned())]);
        let mut host = ScriptHost::load(&world, "scene", &sources, None).unwrap();
        host.update(&mut world, 0.1, ScriptTime::default());

        let (frame, warnings) = world.build_frame();
        assert!(warnings.is_empty());
        let snapshot = make_snapshot(
            frame,
            &world,
            &GpuSceneTable::default(),
            &AnimationBridge::default(),
        )
        .unwrap();
        assert_eq!(snapshot.view.color.unwrap_or_default().exposure_ev, 2.5);
        assert!(host.drain_events().is_empty());
    }

    #[cfg(feature = "dev-console")]
    fn started_host_with_script(source: &str) -> ScriptHost {
        let document = SceneDocument::parse(
            "scene.rvescene",
            r#"
[[assets]]
path = "/profile.lua"
[[objects]]
id = "camera"
[objects.camera]
active = true
[[objects.lite_scripts]]
path = "/profile.lua"
"#,
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let sources = BTreeMap::from([("profile.lua".to_owned(), source.to_owned())]);
        let mut host = ScriptHost::load(&world, "scene.rvescene", &sources, None).unwrap();
        host.start(&mut world, ScriptTime::default());
        host
    }

    #[cfg(feature = "dev-console")]
    #[test]
    fn script_command_named_profile_is_shadowed_by_the_builtin_profiler() {
        let host = started_host_with_script(
            r#"
local Script = {}
function Script:on_start()
  rve.console.register("profile", function() end, "script profile")
end
return Script
"#,
        );
        let catalog = test_command_catalog();
        let mut generation = u64::MAX;

        publish_command_catalog(&host, &catalog, &mut generation);

        let snapshot = catalog.lock().unwrap();
        assert!(snapshot.profile_shadowed);
        assert_eq!(
            snapshot
                .catalog
                .get(PROFILE_COMMAND)
                .map(|spec| spec.description.as_str()),
            Some("script profile"),
            "при затенении остаётся команда скрипта"
        );
    }

    #[cfg(feature = "dev-console")]
    #[test]
    fn builtin_profile_command_is_listed_when_no_script_registers_it() {
        let host = started_host_with_script("local Script = {}\nreturn Script\n");
        let catalog = test_command_catalog();
        let mut generation = u64::MAX;

        publish_command_catalog(&host, &catalog, &mut generation);

        let snapshot = catalog.lock().unwrap();
        assert!(!snapshot.profile_shadowed);
        assert_eq!(
            snapshot
                .catalog
                .get(PROFILE_COMMAND)
                .map(|spec| spec.description.as_str()),
            Some(PROFILE_COMMAND_DESCRIPTION)
        );
    }

    #[cfg(feature = "dev-console")]
    #[test]
    fn queued_console_command_runs_before_update_and_publishes_catalog() {
        let document = SceneDocument::parse(
            "scene.rvescene",
            r#"
[[assets]]
path = "/console.lua"
[[objects]]
id = "camera"
[objects.camera]
active = true
[[objects.lite_scripts]]
path = "/console.lua"
"#,
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let script = r#"
local Script = {}
function Script:on_start()
  rve.console.register("arm", function()
    Script.command_ran = true
  end, "mark command as run")
end
function Script:on_update()
  if not Script.checked then
    Script.checked = true
    if Script.command_ran then
      rve.console.register("after_command", function() end, "command ran first")
    else
      rve.console.register("before_command", function() end, "update ran first")
    end
  end
end
return Script
"#;
        let sources = BTreeMap::from([("console.lua".to_owned(), script.to_owned())]);
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (render_tx, _render_rx) = flume::unbounded();
        let catalog = test_command_catalog();
        let mut modules = ModuleHost::new();
        let thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "scene.rvescene".to_owned(),
                cursor: CursorControl::default(),
                sources,
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
                audio: None,
                command_catalog: Arc::clone(&catalog),
            },
        )
        .unwrap();
        thread.ready.recv().unwrap().unwrap();
        script_tx
            .send(ScriptControl::Console(CommandLine {
                name: "arm".to_owned(),
                args: Vec::new(),
            }))
            .unwrap();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();

        let _frame = frame_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let catalog = catalog.lock().unwrap();
        assert!(catalog.generation > 0);
        assert_eq!(
            catalog.catalog.get("after_command").unwrap().description,
            "command ran first"
        );
        assert!(catalog.catalog.get("before_command").is_none());
        drop(catalog);

        script_tx.send(ScriptControl::Shutdown).unwrap();
        thread.stopped.recv_timeout(Duration::from_secs(1)).unwrap();
        modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
    }

    #[cfg(feature = "dev-console")]
    #[test]
    fn unknown_console_command_is_reported_as_unknown() {
        let document = SceneDocument::parse(
            "scene.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = true\n",
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let mut scripts = ScriptHost::load(&world, "scene", &BTreeMap::new(), None).unwrap();
        let mut pending = VecDeque::from([CommandLine {
            name: "missing".to_owned(),
            args: Vec::new(),
        }]);
        let errors = execute_console_commands(&mut scripts, &mut world, &mut pending);
        assert!(
            matches!(errors.as_slice(), [(name, engine_script::CommandError::Unknown(error))] if name == "missing" && error == "missing")
        );
    }

    #[test]
    fn full_snapshot_queue_does_not_prevent_scripts_from_observing_shutdown() {
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (control_tx, control_rx) = flume::unbounded();
        frame_tx.send(Arc::new(empty_snapshot())).unwrap();
        control_tx.send(ScriptControl::Shutdown).unwrap();
        let result = send_snapshot(
            &frame_tx,
            &control_rx,
            Arc::new(empty_snapshot()),
            &mut false,
            &mut Default::default(),
            &mut Default::default(),
        );
        assert!(matches!(result, super::SendOutcome::Shutdown));
        assert_eq!(frame_rx.len(), 1);
    }

    fn render_test_window() -> PhaseWindow {
        PhaseWindow::new(
            SOURCE_RENDER,
            Arc::new(ProfileBoard::new(&[SOURCE_RENDER])),
            render_groups(),
        )
    }

    #[test]
    fn render_loop_records_frame_wait_when_its_frame_source_ends() {
        let (frame_tx, frame_rx) = flume::unbounded::<Arc<FrameSnapshot>>();
        let (_resource_tx, resource_rx) = flume::unbounded();
        let (_control_tx, control_rx) = flume::unbounded();
        let (script_tx, _script_rx) = flume::unbounded();
        drop(frame_tx);
        let mut backend = FakeBackend::default();
        let mut window = render_test_window();

        let result = render_loop(
            &mut backend,
            640,
            480,
            None,
            &frame_rx,
            &resource_rx,
            &control_rx,
            &script_tx,
            &mut window,
        );

        assert!(result.is_ok());
        let report = window.report();
        let frame_wait = report.groups[render_phase::LOOP].rows[render_phase::FRAME_WAIT].value;
        assert!(matches!(
            frame_wait,
            ProfileValue::Timing(summary) if summary.samples == 1
        ));
        let resource_requests =
            report.groups[render_phase::LOOP].rows[render_phase::RESOURCE_REQUESTS].value;
        assert!(matches!(
            resource_requests,
            ProfileValue::Timing(summary) if summary.samples == 1
        ));
    }

    #[test]
    fn render_waiting_on_an_empty_queue_stops_on_control_message() {
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (control_tx, control_rx) = flume::unbounded();
        let (script_tx, script_rx) = flume::unbounded();
        let mut backend = FakeBackend::default();
        let worker = std::thread::spawn(move || {
            render_loop(
                &mut backend,
                640,
                480,
                None,
                &frame_rx,
                &flume::unbounded::<RenderResourceRequest>().1,
                &control_rx,
                &script_tx,
                &mut render_test_window(),
            )
        });
        control_tx.send(RenderControl::Shutdown).unwrap();
        assert!(worker.join().unwrap().is_ok());
        drop(frame_tx);
        drop(script_rx);
    }

    #[cfg(feature = "dev-console")]
    #[test]
    fn latest_scene_is_redrawn_when_console_ui_changes() {
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (control_tx, control_rx) = flume::unbounded();
        let (script_tx, _script_rx) = flume::unbounded();
        let (rendered_tx, rendered_rx) = mpsc::channel();
        let slot = Arc::new(ConsoleUiSlot::default());
        let mut backend = FakeBackend {
            console_slot: Some(Arc::clone(&slot)),
            console_rendered: Some(rendered_tx),
            ..FakeBackend::default()
        };
        let worker = std::thread::spawn(move || {
            render_loop(
                &mut backend,
                640,
                480,
                None,
                &frame_rx,
                &flume::unbounded::<RenderResourceRequest>().1,
                &control_rx,
                &script_tx,
                &mut render_test_window(),
            )
        });

        frame_tx.send(Arc::new(empty_snapshot())).unwrap();
        let mut frame = ConsoleUiFrame::default();
        frame.visible = true;
        let notify = slot.publish(frame);
        assert!(notify);
        control_tx.send(RenderControl::ConsoleUiChanged).unwrap();
        rendered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        control_tx.send(RenderControl::Shutdown).unwrap();
        assert!(worker.join().unwrap().is_ok());
    }

    #[test]
    fn paused_render_notifies_scripts_to_tick_in_background() {
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (control_tx, control_rx) = flume::unbounded();
        let (script_tx, script_rx) = flume::unbounded();
        let renders = Arc::new(AtomicUsize::new(0));
        let mut backend = FakeBackend {
            shared_renders: Some(Arc::clone(&renders)),
            ..Default::default()
        };
        let worker = std::thread::spawn(move || {
            render_loop(
                &mut backend,
                640,
                480,
                None,
                &frame_rx,
                &flume::unbounded::<RenderResourceRequest>().1,
                &control_rx,
                &script_tx,
                &mut render_test_window(),
            )
        });
        control_tx.send(RenderControl::Occluded(true)).unwrap();
        assert!(matches!(
            script_rx.recv().unwrap(),
            ScriptControl::Background(true)
        ));
        control_tx.send(RenderControl::Shutdown).unwrap();
        assert!(worker.join().unwrap().is_ok());
        assert_eq!(renders.load(Ordering::SeqCst), 0);
        let _ = frame_tx;
    }

    fn snapshot_with_particle_commands(
        commands: Vec<engine_scene::world::ParticleCommand>,
    ) -> Arc<FrameSnapshot> {
        let mut snapshot = empty_snapshot();
        snapshot.particle_commands = commands;
        Arc::new(snapshot)
    }

    #[test]
    fn particle_commands_of_a_paused_frame_are_applied_once_when_the_next_frame_renders() {
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (control_tx, control_rx) = flume::unbounded();
        let (script_tx, script_rx) = flume::unbounded();
        let (applied_tx, applied_rx) = mpsc::channel();
        let mut backend = FakeBackend {
            applied_particles: Some(applied_tx),
            ..Default::default()
        };
        let command = engine_scene::world::ParticleCommand::Restart {
            object: motion_test_handles().1,
        };
        let worker = std::thread::spawn(move || {
            render_loop(
                &mut backend,
                640,
                480,
                None,
                &frame_rx,
                &flume::unbounded::<RenderResourceRequest>().1,
                &control_rx,
                &script_tx,
                &mut render_test_window(),
            )
        });
        control_tx.send(RenderControl::Occluded(true)).unwrap();
        assert!(matches!(
            script_rx.recv().unwrap(),
            ScriptControl::Background(true)
        ));
        // Снимок с командой принят на паузе и заменён следующим: команда не должна пропасть.
        frame_tx
            .send(snapshot_with_particle_commands(vec![command]))
            .unwrap();
        frame_tx.send(snapshot_with_particle_commands(Vec::new())).unwrap();
        control_tx.send(RenderControl::Occluded(false)).unwrap();
        assert!(matches!(
            script_rx.recv().unwrap(),
            ScriptControl::Background(false)
        ));
        frame_tx.send(snapshot_with_particle_commands(Vec::new())).unwrap();
        control_tx.send(RenderControl::Shutdown).unwrap();
        assert!(worker.join().unwrap().is_ok());
        assert_eq!(applied_rx.try_iter().collect::<Vec<_>>(), vec![vec![command]]);
    }

    #[cfg(feature = "dev-console")]
    #[test]
    fn particle_commands_are_not_reapplied_when_the_latest_frame_is_redrawn() {
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (control_tx, control_rx) = flume::unbounded();
        let (script_tx, _script_rx) = flume::unbounded();
        let (applied_tx, applied_rx) = mpsc::channel();
        let mut backend = FakeBackend {
            applied_particles: Some(applied_tx),
            ..Default::default()
        };
        let command = engine_scene::world::ParticleCommand::Restart {
            object: motion_test_handles().1,
        };
        let worker = std::thread::spawn(move || {
            render_loop(
                &mut backend,
                640,
                480,
                None,
                &frame_rx,
                &flume::unbounded::<RenderResourceRequest>().1,
                &control_rx,
                &script_tx,
                &mut render_test_window(),
            )
        });
        frame_tx
            .send(snapshot_with_particle_commands(vec![command]))
            .unwrap();
        assert_eq!(
            applied_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            vec![command]
        );
        // Смена debug view перерисовывает последний снимок тем же набором команд.
        control_tx
            .send(RenderControl::DebugView(engine_render::DebugView::Wireframe))
            .unwrap();
        control_tx.send(RenderControl::Shutdown).unwrap();
        assert!(worker.join().unwrap().is_ok());
        assert!(
            applied_rx.try_iter().next().is_none(),
            "перерисовка не применяет команды повторно"
        );
    }

    const MATERIAL_SCENE: &str = "[[assets]]\npath = \"/models/crate.fbx\"\n[[assets]]\npath = \"/materials/gold.rvemat\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"crate\"\n[objects.mesh_renderer]\nmodel = \"/models/crate.fbx\"\n";

    fn material_test_world() -> (World, ObjectHandle) {
        let world = World::from_document(
            &SceneDocument::parse("materials.rvescene", MATERIAL_SCENE).unwrap(),
        )
        .unwrap();
        let crate_handle = world.handle("crate").unwrap();
        (world, crate_handle)
    }

    fn next_material_request(
        requests: &Receiver<RenderResourceRequest>,
    ) -> Box<MaterialRequest> {
        match requests.try_recv() {
            Ok(RenderResourceRequest::Material(request)) => request,
            _ => panic!("ожидалась заявка материала"),
        }
    }

    #[test]
    fn material_changes_request_one_upload_per_object_and_a_reply_swaps_its_levels() {
        let (mut world, crate_handle) = material_test_world();
        let mut gpu_scene = GpuSceneTable::default();
        gpu_scene.by_object.insert(crate_handle, Vec::new());
        let (resources_tx, resources_rx) = flume::unbounded();
        let mut sync = MaterialSync::default();

        world
            .set_mesh_material_params(
                crate_handle,
                MeshMaterialParams {
                    roughness: Some(0.5),
                    ..Default::default()
                },
            )
            .unwrap();
        sync.request_changes(&mut world, &gpu_scene, &resources_tx);
        let first = next_material_request(&resources_rx);
        assert_eq!((first.object, first.revision), (crate_handle, 1));
        assert!(first.levels.is_empty());
        assert_eq!(first.material.as_ref().and_then(|m| m.roughness), Some(0.5));
        assert_eq!(first.path, None);

        // Вторая замена ждёт ответа на первую: заявка на тот же объект не уходит.
        world
            .set_mesh_material_params(
                crate_handle,
                MeshMaterialParams {
                    base_color: Some([1.0, 0.0, 0.0]),
                    ..Default::default()
                },
            )
            .unwrap();
        sync.request_changes(&mut world, &gpu_scene, &resources_tx);
        assert!(resources_rx.try_recv().is_err());

        // Ответ на первую ревизию меняет уровни; вторая ревизия уходит следующей заявкой.
        sync.accept(MaterialReply {
            object: crate_handle,
            revision: first.revision,
            result: Ok(MaterialLevels {
                levels: vec![Vec::new()],
                created: Vec::new(),
            }),
        });
        sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 5);
        assert_eq!(gpu_scene.by_object[&crate_handle].len(), 1);
        sync.request_changes(&mut world, &gpu_scene, &resources_tx);
        let second = next_material_request(&resources_rx);
        assert_eq!(second.revision, 2);
        let material = second.material.as_ref().expect("переопределения переданы");
        assert_eq!(material.roughness, Some(0.5), "прежние поля сохраняются");
        assert_eq!(material.base_color, Some([1.0, 0.0, 0.0]));
    }

    #[test]
    fn set_material_path_reaches_the_request_and_a_refused_reply_keeps_the_levels() {
        let (mut world, crate_handle) = material_test_world();
        let mut gpu_scene = GpuSceneTable::default();
        gpu_scene.by_object.insert(crate_handle, vec![Vec::new()]);
        let (resources_tx, resources_rx) = flume::unbounded();
        let mut sync = MaterialSync::default();
        let gold = engine_files::VirtualPath::parse("/materials/gold.rvemat").unwrap();
        world
            .set_mesh_material_path(crate_handle, gold)
            .unwrap();

        sync.request_changes(&mut world, &gpu_scene, &resources_tx);
        let request = next_material_request(&resources_rx);
        assert_eq!(request.path.as_deref(), Some("materials/gold.rvemat"));
        sync.accept(MaterialReply {
            object: crate_handle,
            revision: request.revision,
            result: Err("материал не загрузился".to_owned()),
        });
        sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 3);
        assert_eq!(gpu_scene.by_object[&crate_handle].len(), 1, "отказ не меняет уровни");
        sync.request_changes(&mut world, &gpu_scene, &resources_tx);
        assert!(
            resources_rx.try_recv().is_err(),
            "ревизия не менялась: повторной заявки нет"
        );
    }

    #[test]
    fn reply_for_an_object_that_left_the_gpu_table_does_not_bring_its_levels_back() {
        let (world, crate_handle) = material_test_world();
        let mut gpu_scene = GpuSceneTable::default();
        let (resources_tx, resources_rx) = flume::unbounded();
        let mut sync = MaterialSync::default();
        sync.accept(MaterialReply {
            object: crate_handle,
            revision: 0,
            result: Ok(MaterialLevels {
                levels: vec![Vec::new()],
                created: Vec::new(),
            }),
        });
        sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 1);
        assert!(!gpu_scene.by_object.contains_key(&crate_handle));
        assert!(resources_rx.try_recv().is_err());
    }

    #[test]
    fn replaced_materials_are_returned_and_dead_objects_give_up_all_of_theirs() {
        let (a, b, _, _) = motion_test_handles();
        let mut owned = MaterialOwnership::<u32>::default();
        assert!(owned.replace(a, vec![1, 2]).is_empty());
        assert_eq!(owned.replace(a, vec![3]), vec![1, 2]);
        assert!(owned.replace(b, vec![4]).is_empty());
        assert_eq!(owned.take_dead(|handle| handle == b), vec![3]);
        assert_eq!(owned.take_dead(|_| false), vec![4]);
    }

    #[test]
    fn paused_snapshot_is_dropped_without_calling_the_renderer() {
        let mut backend = FakeBackend::default();
        let mut snapshot = empty_snapshot();
        let mut history = MotionHistory::default();
        assert!(
            render_snapshot(
                &mut backend,
                &mut snapshot,
                &mut history,
                &mut Vec::new(),
                true,
                0,
                0,
                #[cfg(feature = "dev-console")]
                &mut ConsoleUiFrame::default(),
                &mut render_test_window(),
            )
            .unwrap()
            .is_none()
        );
        assert_eq!(backend.renders, 0);
    }

    #[test]
    fn background_scheduler_uses_injected_clock_and_control_recv_timeout() {
        let clock = Arc::new(FakeClock::default());
        let waiting_rx = clock.observe_waits();
        let (control_tx, control_rx) = flume::unbounded();
        let clock_for_wait = Arc::clone(&clock);
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut background = true;
            let mut deadline = Duration::from_millis(100);
            for _ in 0..2 {
                let ticked = wait_background_tick(
                    &control_rx,
                    clock_for_wait.as_ref(),
                    &mut background,
                    deadline,
                    &mut Default::default(),
                    &mut Default::default(),
                );
                if !ticked || !background {
                    break;
                }
                let now = clock_for_wait.now();
                deadline = now + Duration::from_millis(100);
                let _ = done_tx.send(now);
            }
        });
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        clock.advance(Duration::from_millis(100));
        control_tx.send(ScriptControl::Background(true)).unwrap();
        assert_eq!(done_rx.recv().unwrap(), Duration::from_millis(100));
        assert!(done_rx.try_recv().is_err());
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        clock.advance(Duration::from_millis(100));
        control_tx.send(ScriptControl::Background(true)).unwrap();
        assert_eq!(done_rx.recv().unwrap(), Duration::from_millis(200));
        worker.join().unwrap();
    }

    #[test]
    fn headless_loop_stops_by_itself_after_frame_limit_ticks() {
        let clock = Arc::new(FakeClock::default());
        let waits = clock.observe_waits();
        let world =
            World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
                .unwrap();
        let mut scripts = start_test_scripts_configured(
            world,
            &[("gun.lua", "local Gun = {}\nreturn Gun\n")],
            PreparedPrefabs::default(),
            None,
            Arc::clone(&clock),
            Some(2),
            true,
        );
        // Каждый тик ждёт сообщение, а часы должны пройти дедлайн: так тики детерминированы.
        waits
            .recv_timeout(Duration::from_secs(2))
            .expect("первое ожидание тика");
        clock.advance(Duration::from_millis(100));
        scripts
            .control
            .send(ScriptControl::Background(true))
            .unwrap();
        waits
            .recv_timeout(Duration::from_secs(2))
            .expect("ожидание второго тика");
        assert!(
            scripts.thread.stopped.try_recv().is_err(),
            "после одного тика цикл продолжается"
        );
        clock.advance(Duration::from_millis(100));
        scripts
            .control
            .send(ScriptControl::Background(true))
            .unwrap();
        scripts
            .thread
            .stopped
            .recv_timeout(Duration::from_secs(2))
            .expect("цикл остановился по --frames без Shutdown");
        assert!(scripts.modules.shutdown().is_empty());
    }

    #[test]
    fn headless_reply_to_a_heavy_upload_materializes_the_spawn() {
        let mut harness = HeavyHarness::new(SPAWN_ON_START);
        harness.start();
        let token = harness.next_load();
        let loaded = LoadedPrefab {
            token,
            cpu: PrefabCpu {
                template: prefab_template(BALL_PATH, BALL_PREFAB),
                sources: BTreeMap::new(),
                geometry: Vec::new(),
            },
            models: HashMap::new(),
            textures: HashMap::new(),
            material_assets: HashMap::new(),
            shader_assets: HashMap::new(),
            levels: Vec::new(),
            particles: Vec::new(),
        };
        harness.reply(crate::headless::resource_reply(loaded));
        harness.update();
        assert!(
            harness.ball_alive(),
            "ответ headless без GPU открывает отложенный spawn"
        );
    }

    #[test]
    fn background_sync_releases_held_input_once() {
        let input = test_input();
        {
            let mut accumulator = lock_input(&input);
            accumulator.focused(true);
            accumulator.key(engine_window::Key::A, true);
        }
        let mut applied_background = false;

        sync_input_background(&input, true, &mut applied_background);
        let frame = lock_input(&input).take_frame();
        assert!(!frame.key_down(Key::A));
        assert!(!frame.key_pressed(Key::A));
        assert!(frame.key_released(Key::A));

        sync_input_background(&input, true, &mut applied_background);
        let frame = lock_input(&input).take_frame();
        assert!(!frame.key_released(Key::A));

        sync_input_background(&input, false, &mut applied_background);
        lock_input(&input).key(engine_window::Key::A, true);
        let frame = lock_input(&input).take_frame();
        assert!(frame.key_down(Key::A));
        assert!(frame.key_pressed(Key::A));
    }

    #[derive(Debug, PartialEq, Eq)]
    enum LatencyEvent {
        Sleep,
        Marker(u64, LatencyMarker),
    }

    #[derive(Default)]
    struct RecordingLatency {
        events: Mutex<Vec<LatencyEvent>>,
    }

    impl FrameLatency for RecordingLatency {
        fn sleep(&self) {
            self.events.lock().unwrap().push(LatencyEvent::Sleep);
        }

        fn marker(&self, frame_id: u64, marker: LatencyMarker) {
            self.events
                .lock()
                .unwrap()
                .push(LatencyEvent::Marker(frame_id, marker));
        }
    }

    fn latency_test_world() -> World {
        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
        World::from_document(&document).unwrap()
    }

    fn shutdown_test_scripts(mut scripts: TestScripts) {
        scripts.control.send(ScriptControl::Shutdown).unwrap();
        scripts
            .thread
            .stopped
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        scripts.modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(scripts.modules.shutdown().is_empty());
    }

    #[test]
    fn reflex_sleep_and_markers_run_in_order_for_the_submitted_sequence() {
        let latency = Arc::new(RecordingLatency::default());
        let scripts =
            start_test_scripts(latency_test_world(), &[], PreparedPrefabs::default(), None);
        scripts
            .control
            .send(ScriptControl::Latency(latency.clone()))
            .unwrap();
        scripts
            .control
            .send(ScriptControl::Go(Box::default()))
            .unwrap();
        let frame = scripts
            .frames
            .recv_timeout(Duration::from_secs(2))
            .expect("кадр");
        assert_eq!(frame.sequence, 1);
        // Цикл может уже готовить следующий кадр, поэтому сравниваем только первые события.
        let events = latency.events.lock().unwrap();
        assert_eq!(
            events[..4],
            [
                LatencyEvent::Sleep,
                LatencyEvent::Marker(frame.sequence, LatencyMarker::SimulationStart),
                LatencyEvent::Marker(frame.sequence, LatencyMarker::InputSample),
                LatencyEvent::Marker(frame.sequence, LatencyMarker::SimulationEnd),
            ]
        );
        drop(events);
        shutdown_test_scripts(scripts);
    }

    #[test]
    fn reflex_is_not_called_while_scripts_run_in_background() {
        let clock = Arc::new(FakeClock::default());
        let waiting = clock.observe_waits();
        let latency = Arc::new(RecordingLatency::default());
        let scripts = start_test_scripts_with_clock(
            latency_test_world(),
            &[],
            PreparedPrefabs::default(),
            None,
            Arc::clone(&clock),
        );
        scripts
            .control
            .send(ScriptControl::Latency(latency.clone()))
            .unwrap();
        scripts
            .control
            .send(ScriptControl::Background(true))
            .unwrap();
        scripts
            .control
            .send(ScriptControl::Go(Box::default()))
            .unwrap();
        waiting
            .recv_timeout(Duration::from_secs(2))
            .expect("фоновое ожидание тика");
        // Время проходит до дедлайна; Go здесь игнорируется и лишь пробуждает ожидание.
        clock.advance(Duration::from_secs(1));
        scripts
            .control
            .send(ScriptControl::Go(Box::default()))
            .unwrap();
        waiting
            .recv_timeout(Duration::from_secs(2))
            .expect("ожидание после фонового шага симуляции");
        // Второе ожидание начинается уже после фонового шага: Reflex за него не вызывался.
        assert!(latency.events.lock().unwrap().is_empty());
        scripts
            .control
            .send(ScriptControl::Background(false))
            .unwrap();
        let frame = scripts
            .frames
            .recv_timeout(Duration::from_secs(2))
            .expect("кадр после выхода из фона");
        // Позитивный контроль: фоновый шаг увеличил номер кадра, а Reflex появился только для кадра.
        assert_eq!(frame.sequence, 2);
        let events = latency.events.lock().unwrap();
        assert_eq!(
            events[..4],
            [
                LatencyEvent::Sleep,
                LatencyEvent::Marker(frame.sequence, LatencyMarker::SimulationStart),
                LatencyEvent::Marker(frame.sequence, LatencyMarker::InputSample),
                LatencyEvent::Marker(frame.sequence, LatencyMarker::SimulationEnd),
            ]
        );
        drop(events);
        shutdown_test_scripts(scripts);
    }

    #[test]
    fn scripts_produce_frames_without_a_reflex_handle() {
        let scripts =
            start_test_scripts(latency_test_world(), &[], PreparedPrefabs::default(), None);
        scripts
            .control
            .send(ScriptControl::Go(Box::default()))
            .unwrap();
        let frame = scripts
            .frames
            .recv_timeout(Duration::from_secs(2))
            .expect("кадр без Reflex");
        assert_eq!(frame.sequence, 1);
        shutdown_test_scripts(scripts);
    }

    #[test]
    fn physics_step_sent_after_host_stopped_physics_is_not_a_failure() {
        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let snapshot = PhysicsWorld::new(Vec3::ZERO)
            .unwrap()
            .query_snapshot()
            .unwrap();
        let initialization = PhysicsInitialization {
            result: PhysicsStepResult {
                skipped_commands: 0,
                sequence: 0,
                substeps: 0,
                poses: Vec::new(),
                events: Vec::new(),
                snapshot,
            },
            bindings: Vec::new(),
        };
        let (physics_request_tx, physics_request_rx) = flume::unbounded();
        let (_physics_reply_tx, physics_reply_rx) = flume::unbounded::<PhysicsReply>();
        // Сервер физики уже вышел по Shutdown хоста, а scripts thread ещё делает шаг.
        drop(physics_request_rx);
        let mut modules = ModuleHost::new();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, _frame_rx) = flume::bounded(1);
        let (render_tx, _render_rx) = flume::unbounded();
        let interrupt = ScriptInterruptHandle::new();
        let status = Arc::new(RuntimeStatus::default());
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "startup.rvescene".to_owned(),
                cursor: CursorControl::default(),
                input_settings: engine_project::InputSettings::default(),
                sources: BTreeMap::new(),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: None,
                interrupt: interrupt.clone(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 1,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::clone(&status),
                input: test_input(),
                physics_client: Some(PhysicsClient::new_preinitialized(
                    physics_request_tx,
                    physics_reply_rx,
                )),
                physics_initialization: Some(initialization),
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        script_thread
            .ready
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        interrupt.interrupt();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();

        script_thread
            .stopped
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(status.failure_message(), None);
        modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
    }

    #[test]
    fn physics_steps_use_the_same_injected_delta_in_foreground_and_background() {
        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n\n[[objects.heavy_scripts]]\nname = \"TickProbe\"\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let mut modules = ModuleHost::new();
        let clock = Arc::new(FakeClock::default());
        let waiting_rx = clock.observe_waits();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let snapshot = PhysicsWorld::new(Vec3::ZERO)
            .unwrap()
            .query_snapshot()
            .unwrap();
        let initialization = PhysicsInitialization {
            result: PhysicsStepResult {
                skipped_commands: 0,
                sequence: 0,
                substeps: 0,
                poses: Vec::new(),
                events: Vec::new(),
                snapshot: Arc::clone(&snapshot),
            },
            bindings: Vec::new(),
        };
        let (physics_request_tx, physics_request_rx) = flume::unbounded();
        let (physics_reply_tx, physics_reply_rx) = flume::unbounded();
        let physics_stop_tx = physics_request_tx.clone();
        let (physics_dt_tx, physics_dt_rx) = mpsc::channel();
        let physics_worker = std::thread::spawn(move || {
            while let Ok(request) = physics_request_rx.recv() {
                match request {
                    PhysicsRequest::Step { sequence, dt, .. } => {
                        physics_dt_tx.send(dt).unwrap();
                        // Как настоящий сервер: скрипты могут завершиться, не дождавшись
                        // ответа на последний шаг, и закрыть канал ответов.
                        let reply = PhysicsReply::Step(PhysicsStepResult {
                            skipped_commands: 0,
                            sequence,
                            substeps: 0,
                            poses: Vec::new(),
                            events: Vec::<PhysicsEvent>::new(),
                            snapshot: Arc::clone(&snapshot),
                        });
                        if physics_reply_tx.send(reply).is_err() {
                            break;
                        }
                    }
                    PhysicsRequest::Shutdown => break,
                    PhysicsRequest::Init(_) => panic!("preinitialized server received Init"),
                }
            }
        });
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "startup.rvescene".to_owned(),
                cursor: CursorControl::default(),
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: Some(script_module()),
                interrupt: ScriptInterruptHandle::new(),
                clock: clock.clone(),
                background_tick_hz: 1,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: Some(PhysicsClient::new_preinitialized(
                    physics_request_tx,
                    physics_reply_rx,
                )),
                physics_initialization: Some(initialization),
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        script_thread
            .ready
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        script_tx.send(ScriptControl::Background(false)).unwrap();
        script_tx.send(ScriptControl::Background(true)).unwrap();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();

        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(frame_rx.try_recv().is_err());
        clock.advance(Duration::from_millis(500));
        script_tx.send(ScriptControl::Background(true)).unwrap();
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(frame_rx.try_recv().is_err());

        clock.advance(Duration::from_millis(500));
        script_tx.send(ScriptControl::Background(true)).unwrap();
        let background_dt = physics_dt_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(background_dt, 1.0);
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(frame_rx.try_recv().is_err());
        clock.advance(Duration::from_millis(250));
        script_tx.send(ScriptControl::Background(false)).unwrap();

        let foreground_dt = physics_dt_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(foreground_dt, 0.25);
        let snapshot = frame_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!((snapshot.camera.position.x - background_dt - foreground_dt).abs() < 1.0e-6);
        script_tx.send(ScriptControl::Shutdown).unwrap();
        script_thread
            .stopped
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(render_rx.recv().unwrap(), RenderControl::Shutdown);
        modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
        // Воркер мог уже выйти из-за закрытого канала ответов.
        let _ = physics_stop_tx.send(PhysicsRequest::Shutdown);
        physics_worker.join().unwrap();
    }

    #[test]
    fn initial_physics_snapshot_is_available_to_on_start_raycast() {
        let script = r#"
local Script = {}
function Script:on_start()
  local hit = rve.physics.raycast({x=0,y=0,z=-2}, {x=0,y=0,z=1}, 5)
  assert(hit and hit.object.id == "wall")
  self.transform.position = {x=3,y=0,z=-2}
end
return Script
"#;
        let document = SceneDocument::parse(
            "physics-start.rvescene",
            "[[assets]]\npath='probe.lua'\n[[objects]]\nid='camera'\n[objects.transform]\nposition=[0.0,0.0,-2.0]\n[objects.camera]\n[[objects.lite_scripts]]\npath='probe.lua'\n[[objects]]\nid='wall'\n[objects.collider]\nshape='sphere'\nradius=0.5\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let desc = PhysicsSceneDesc::from_world(&world, &[], Vec3::ZERO, 60, 8).unwrap();
        let (driver, bindings) = desc.build().unwrap();
        let initialization = PhysicsInitialization::from_prebuilt(&driver, bindings).unwrap();
        let (physics_request_tx, physics_request_rx) = flume::unbounded();
        let (physics_reply_tx, physics_reply_rx) = flume::unbounded();
        let physics_stop_tx = physics_request_tx.clone();
        let physics_worker = std::thread::spawn(move || {
            PhysicsServer::run_preinitialized(physics_request_rx, physics_reply_tx, driver)
        });
        let mut modules = ModuleHost::new();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "physics-start.rvescene".to_owned(),
                cursor: CursorControl::default(),
                input_settings: engine_project::InputSettings::default(),
                sources: BTreeMap::from([("probe.lua".to_owned(), script.to_owned())]),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: Some(PhysicsClient::new_preinitialized(
                    physics_request_tx,
                    physics_reply_rx,
                )),
                physics_initialization: Some(initialization),
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        script_thread.ready.recv().unwrap().unwrap();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();
        let frame = frame_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!((frame.camera.position.x - 3.0).abs() < 1.0e-5);

        script_tx.send(ScriptControl::Shutdown).unwrap();
        script_thread
            .stopped
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(render_rx.recv().unwrap(), RenderControl::Shutdown);
        modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
        physics_stop_tx.send(PhysicsRequest::Shutdown).unwrap();
        physics_worker.join().unwrap().unwrap();
    }

    #[test]
    fn script_transform_is_sent_as_zero_substep_physics_teleport() {
        let script = r#"
local Move = {}
function Move:on_start()
  self.transform.position = {x=4,y=2,z=0}
end
return Move
"#;
        let document = SceneDocument::parse(
            "physics-teleport.rvescene",
            "[[assets]]\npath='move.lua'\n[[objects]]\nid='camera'\n[objects.camera]\n[[objects]]\nid='ball'\n[objects.transform]\nposition=[0.0,2.0,0.0]\n[objects.collider]\nshape='sphere'\nradius=0.5\n[objects.rigid_body]\nkind='dynamic'\n[[objects.lite_scripts]]\npath='move.lua'\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let ball = world.handle("ball").unwrap();
        let desc = PhysicsSceneDesc::from_world(&world, &[], Vec3::ZERO, 60, 8).unwrap();
        let (driver, bindings) = desc.build().unwrap();
        let body = bindings
            .iter()
            .find(|binding| binding.object == ball)
            .unwrap()
            .body;
        let initialization =
            PhysicsInitialization::from_prebuilt(&driver, bindings.clone()).unwrap();
        let (physics_request_tx, physics_request_rx) = flume::unbounded();
        let (physics_reply_tx, physics_reply_rx) = flume::unbounded();
        let (step_tx, step_rx) = mpsc::channel();
        let physics_worker = std::thread::spawn(move || {
            let mut driver = driver;
            while let Ok(request) = physics_request_rx.recv() {
                match request {
                    PhysicsRequest::Step {
                        sequence,
                        dt,
                        commands,
                        ..
                    } => {
                        let has_teleport = commands.iter().any(|command| {
                            matches!(
                                command,
                                PhysicsCommand::Teleport { body: target, pose }
                                    if *target == body && pose.position.x == 4.0
                            )
                        });
                        step_tx.send((dt, has_teleport)).unwrap();
                        let step = driver.advance_step(dt, commands).unwrap();
                        let snapshot = driver.world().query_snapshot().unwrap();
                        let _ = physics_reply_tx.send(PhysicsReply::Step(PhysicsStepResult {
                            skipped_commands: 0,
                            sequence,
                            substeps: step.substeps,
                            poses: step.poses,
                            events: step.events,
                            snapshot,
                        }));
                    }
                    PhysicsRequest::Shutdown => break,
                    PhysicsRequest::Init(_) => panic!("preinitialized server received Init"),
                }
            }
        });
        let mut modules = ModuleHost::new();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "physics-teleport.rvescene".to_owned(),
                cursor: CursorControl::default(),
                input_settings: engine_project::InputSettings::default(),
                sources: BTreeMap::from([("move.lua".to_owned(), script.to_owned())]),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: Some(PhysicsClient::new_preinitialized(
                    physics_request_tx,
                    physics_reply_rx,
                )),
                physics_initialization: Some(initialization),
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        script_thread.ready.recv().unwrap().unwrap();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();

        let (dt, has_teleport) = step_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(dt, 0.0);
        assert!(has_teleport, "script transform was not sent as a Teleport");
        frame_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        script_tx.send(ScriptControl::Shutdown).unwrap();
        script_thread
            .stopped
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(render_rx.recv().unwrap(), RenderControl::Shutdown);
        modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
        physics_worker.join().unwrap();
    }

    #[test]
    fn physics_pose_sync_remains_finite_after_runtime_scale_becomes_zero() {
        let document = SceneDocument::parse(
            "physics-scale.rvescene",
            "[[objects]]\nid='camera'\n[objects.camera]\n[[objects]]\nid='ball'\n[objects.transform]\nposition=[0.0,2.0,0.0]\n[objects.collider]\nshape='sphere'\nradius=0.5\n[objects.rigid_body]\nkind='dynamic'\n",
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let desc = PhysicsSceneDesc::from_world(&world, &[], Vec3::ZERO, 60, 8).unwrap();
        let (driver, bindings) = desc.build().unwrap();
        let initialization =
            PhysicsInitialization::from_prebuilt(&driver, bindings.clone()).unwrap();
        let mut sync = PhysicsSceneSync::new(&world, bindings.clone()).unwrap();
        let ball = bindings[0].object;
        let mut transform = *world.local_transform(ball).unwrap();
        transform.scale = Vec3::ZERO;
        world.set_local_transform(ball, transform).unwrap();
        world.recalculate_world_matrices();
        let mut scripts =
            ScriptHost::load(&world, "physics-scale", &BTreeMap::new(), None).unwrap();

        apply_physics_step(
            &mut scripts,
            &mut world,
            &mut sync,
            PhysicsStepResult {
                skipped_commands: 0,
                sequence: 1,
                substeps: 0,
                poses: vec![(
                    bindings[0].body,
                    Pose::new(Vec3::new(0.0, 2.0, 0.0), glam::Quat::IDENTITY).unwrap(),
                )],
                events: Vec::new(),
                snapshot: initialization.result.snapshot,
            },
        )
        .unwrap();

        assert!(world.world_matrix(ball).unwrap().is_finite());
        assert!(world.world_rotation(ball).unwrap().is_finite());
    }

    #[test]
    fn shutdown_interrupt_returns_without_outstanding_physics_result() {
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        let mut client = PhysicsClient::new_preinitialized(request_tx, reply_rx);
        assert_eq!(client.step(0.1, Vec::new()).unwrap(), 1);
        assert!(matches!(
            request_rx.recv().unwrap(),
            PhysicsRequest::Step { sequence: 1, .. }
        ));

        let (control_tx, control_rx) = flume::unbounded();
        let (control_seen_tx, control_seen_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let waiting = std::thread::spawn(move || {
            let result = receive_physics_interruptibly(&mut client, &control_rx, |control| {
                if matches!(control, ScriptControl::Shutdown) {
                    control_seen_tx.send(()).unwrap();
                    true
                } else {
                    false
                }
            });
            result_tx.send(result).unwrap();
        });
        control_tx.send(ScriptControl::Shutdown).unwrap();
        control_seen_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        let result_before_reply = result_rx.recv_timeout(Duration::from_secs(1));
        drop(reply_tx);
        waiting.join().unwrap();
        assert!(
            matches!(result_before_reply, Ok(Ok(PhysicsWait::Shutdown))),
            "script wait should return on Shutdown without a StepResult"
        );
    }

    #[test]
    fn physics_reply_disconnect_sets_runtime_exit_code_one() {
        let world = World::from_document(
            &SceneDocument::parse(
                "physics-failure.rvescene",
                "[[objects]]\nid='camera'\n[objects.camera]\n",
            )
            .unwrap(),
        )
        .unwrap();
        let snapshot = PhysicsWorld::new(Vec3::ZERO)
            .unwrap()
            .query_snapshot()
            .unwrap();
        let initialization = PhysicsInitialization {
            result: PhysicsStepResult {
                skipped_commands: 0,
                sequence: 0,
                substeps: 0,
                poses: Vec::new(),
                events: Vec::new(),
                snapshot,
            },
            bindings: Vec::new(),
        };
        let (physics_request_tx, physics_request_rx) = flume::unbounded();
        let (physics_reply_tx, physics_reply_rx) = flume::unbounded();
        let physics_worker = std::thread::spawn(move || {
            assert!(matches!(
                physics_request_rx.recv().unwrap(),
                PhysicsRequest::Step { sequence: 1, .. }
            ));
            drop(physics_reply_tx);
        });
        let status = Arc::new(RuntimeStatus::default());
        let mut modules = ModuleHost::new();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "physics-failure.rvescene".to_owned(),
                cursor: CursorControl::default(),
                input_settings: engine_project::InputSettings::default(),
                sources: BTreeMap::new(),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::clone(&status),
                input: test_input(),
                physics_client: Some(PhysicsClient::new_preinitialized(
                    physics_request_tx,
                    physics_reply_rx,
                )),
                physics_initialization: Some(initialization),
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        script_thread.ready.recv().unwrap().unwrap();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();
        frame_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        script_thread
            .stopped
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        assert_eq!(status.exit_code(), 1);
        assert!(
            status
                .failure_message()
                .unwrap()
                .contains("ошибка потока физики")
        );
        assert_eq!(render_rx.recv().unwrap(), RenderControl::Shutdown);
        modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
        physics_worker.join().unwrap();
    }

    #[test]
    fn scripts_input_edges_follow_background_transitions() {
        let (observation_tx, observation_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        *INPUT_PROBE.lock().unwrap() = Some((observation_tx, continue_rx));

        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n\n[[objects.heavy_scripts]]\nname = \"InputProbe\"\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let mut modules = ModuleHost::new();
        let clock = Arc::new(FakeClock::default());
        let waiting_rx = clock.observe_waits();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, _frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let input = test_input();
        {
            let mut accumulator = lock_input(&input);
            accumulator.focused(true);
            accumulator.key(engine_window::Key::A, true);
        }
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "startup.rvescene".to_owned(),
                cursor: CursorControl::default(),
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: Some(input_probe_module()),
                interrupt: ScriptInterruptHandle::new(),
                clock: clock.clone(),
                background_tick_hz: 1,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: Arc::clone(&input),
                physics_client: None,
                physics_initialization: None,
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        script_thread
            .ready
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();

        script_tx.send(ScriptControl::Background(true)).unwrap();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        clock.advance(Duration::from_secs(1));
        script_tx.send(ScriptControl::Background(true)).unwrap();
        assert_eq!(
            observation_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            InputObservation {
                key_down: false,
                key_pressed: false,
                key_released: true,
            }
        );
        continue_tx.send(()).unwrap();

        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        script_tx.send(ScriptControl::Background(false)).unwrap();
        assert_eq!(
            observation_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            InputObservation {
                key_down: false,
                key_pressed: false,
                key_released: false,
            }
        );
        lock_input(&input).key(engine_window::Key::A, true);
        continue_tx.send(()).unwrap();
        assert_eq!(
            observation_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            InputObservation {
                key_down: true,
                key_pressed: true,
                key_released: false,
            }
        );

        script_tx.send(ScriptControl::Background(true)).unwrap();
        continue_tx.send(()).unwrap();
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        clock.advance(Duration::from_secs(1));
        script_tx.send(ScriptControl::Background(true)).unwrap();
        assert_eq!(
            observation_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            InputObservation {
                key_down: false,
                key_pressed: false,
                key_released: true,
            }
        );
        continue_tx.send(()).unwrap();

        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        script_tx.send(ScriptControl::Shutdown).unwrap();
        script_thread
            .stopped
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(render_rx.recv().unwrap(), RenderControl::Shutdown);
        modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
        *INPUT_PROBE.lock().unwrap() = None;
    }

    #[test]
    fn disconnected_frame_consumer_releases_snapshot_sender() {
        let (frame_tx, frame_rx) = flume::unbounded();
        let (control_tx, control_rx) = flume::unbounded();
        drop(frame_rx);
        assert!(matches!(
            send_snapshot(
                &frame_tx,
                &control_rx,
                Arc::new(empty_snapshot()),
                &mut false,
                &mut Default::default(),
                &mut Default::default()
            ),
            super::SendOutcome::Disconnected
        ));
        drop(control_tx);
    }

    #[test]
    fn frames_limit_ends_after_presented_frames_and_keeps_success_code() {
        let (frame_tx, frame_rx) = flume::unbounded();
        let (control_tx, control_rx) = flume::unbounded();
        let (script_tx, script_rx) = flume::unbounded();
        let (stopped_tx, stopped_rx) = flume::bounded(1);
        let (exit_tx, exit_rx) = mpsc::channel();
        let status = Arc::new(RuntimeStatus::default());
        let interrupt = ScriptInterruptHandle::new();
        frame_tx.send(Arc::new(empty_snapshot())).unwrap();
        frame_tx.send(Arc::new(empty_snapshot())).unwrap();
        render_thread(RenderThreadConfig {
            backend: Box::new(FakeBackend::default()),
            width: 640,
            height: 480,
            frame_limit: Some(2),
            frame_rx,
            resources: flume::unbounded().1,
            control_tx: control_tx.clone(),
            control_rx,
            script_tx,
            interrupt: interrupt.clone(),
            status: Arc::clone(&status),
            resource_stats: None,
            stopped_tx,
            exit: Arc::new(FakeExit(exit_tx)),
        });
        stopped_rx.recv().unwrap();
        exit_rx.recv().unwrap();
        assert_eq!(status.exit_code(), 0);
        assert!(status.render_stopped());
        assert!(!interrupt.is_interrupted());
        assert!(matches!(script_rx.recv().unwrap(), ScriptControl::Shutdown));
        drop((frame_tx, control_tx));
    }

    /// Backend для проверки порядка завершения: записывает `release_scene`, `wait_idle` и drop.
    struct TeardownBackend {
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl RenderBackend for TeardownBackend {
        fn resize(&mut self, _width: u32, _height: u32) {}

        fn submit_prefab(&mut self, _prefab: LoadedPrefab) -> Option<PrefabReply> {
            None
        }

        fn advance_prefab_uploads(&mut self, _budget: usize) -> Vec<PrefabReply> {
            Vec::new()
        }

        fn has_pending_prefab_uploads(&self) -> bool {
            false
        }

        fn release_prefab(&mut self, _token: &PrefabToken) {}

        fn render(
            &mut self,
            _snapshot: &FrameSnapshot,
            _width: u32,
            _height: u32,
        ) -> Result<FrameOutcome, String> {
            Ok(FrameOutcome {
                status: FrameStatus::Presented,
                submitted: true,
            })
        }

        fn release_scene(&mut self) {
            self.log.lock().unwrap().push("release_scene");
        }

        fn wait_idle(&mut self) {
            self.log.lock().unwrap().push("wait_idle");
        }
    }

    impl Drop for TeardownBackend {
        fn drop(&mut self) {
            self.log.lock().unwrap().push("drop");
        }
    }

    #[test]
    fn render_thread_releases_scene_once_before_waiting_and_dropping_backend() {
        // Регрессия: ресурсы сцены освобождаются один раз, до wait_idle и уничтожения backend.
        let teardown = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (frame_tx, frame_rx) = flume::unbounded();
        let (control_tx, control_rx) = flume::unbounded();
        let (script_tx, _script_rx) = flume::unbounded();
        let (stopped_tx, stopped_rx) = flume::bounded(1);
        let (exit_tx, exit_rx) = mpsc::channel();
        let status = Arc::new(RuntimeStatus::default());
        frame_tx.send(Arc::new(empty_snapshot())).unwrap();
        render_thread(RenderThreadConfig {
            backend: Box::new(TeardownBackend {
                log: Arc::clone(&teardown),
            }),
            width: 640,
            height: 480,
            frame_limit: Some(1),
            frame_rx,
            control_tx: control_tx.clone(),
            control_rx,
            script_tx,
            interrupt: ScriptInterruptHandle::new(),
            status: Arc::clone(&status),
            resources: flume::unbounded().1,
            resource_stats: None,
            stopped_tx,
            exit: Arc::new(FakeExit(exit_tx)),
        });
        stopped_rx.recv().unwrap();
        exit_rx.recv().unwrap();
        assert_eq!(status.exit_code(), 0);
        assert_eq!(
            *teardown.lock().unwrap(),
            ["release_scene", "wait_idle", "drop"]
        );
        drop((frame_tx, control_tx));
    }

    #[test]
    fn render_guard_catches_panics_and_errors_before_marking_stopped_and_exiting() {
        for backend in [
            FakeBackend {
                panic: true,
                ..Default::default()
            },
            FakeBackend {
                fail: true,
                ..Default::default()
            },
            FakeBackend {
                panic_wait_idle: true,
                ..Default::default()
            },
        ] {
            let (frame_tx, frame_rx) = flume::unbounded();
            let (control_tx, control_rx) = flume::unbounded();
            let (script_tx, script_rx) = flume::unbounded();
            let (stopped_tx, stopped_rx) = flume::bounded(1);
            let (exit_tx, exit_rx) = mpsc::channel();
            let status = Arc::new(RuntimeStatus::default());
            frame_tx.send(Arc::new(empty_snapshot())).unwrap();
            drop(frame_tx);
            render_thread(RenderThreadConfig {
                backend: Box::new(backend),
                width: 640,
                height: 480,
                frame_limit: None,
                frame_rx,
                resources: flume::unbounded().1,
                control_tx: control_tx.clone(),
                control_rx,
                script_tx,
                interrupt: ScriptInterruptHandle::new(),
                status: Arc::clone(&status),
                resource_stats: None,
                stopped_tx,
                exit: Arc::new(FakeExit(exit_tx)),
            });
            stopped_rx.recv().unwrap();
            exit_rx.recv().unwrap();
            assert!(status.render_stopped());
            assert_eq!(status.exit_code(), 1);
            assert!(matches!(script_rx.recv().unwrap(), ScriptControl::Shutdown));
            drop(control_tx);
        }
    }

    #[test]
    fn lua_load_error_is_reported_before_window_and_stops_modules() {
        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[assets]]\npath = \"scripts/bad.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n\n[[objects]]\nid = \"scripted\"\n[[objects.lite_scripts]]\npath = \"scripts/bad.lua\"\n",
        ).unwrap();
        let world = World::from_document(&document).unwrap();
        let mut modules = ModuleHost::new();
        let (module_stop_tx, module_stop_rx) = std::sync::mpsc::channel();
        modules
            .spawn(
                "rve-audio",
                move || {
                    let _ = module_stop_tx.send(());
                },
                || {},
            )
            .unwrap();
        let interrupt = ScriptInterruptHandle::new();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, _frame_rx) = flume::bounded(1);
        let (render_tx, _render_rx) = flume::unbounded();
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "startup.rvescene".to_owned(),
                cursor: CursorControl::default(),
                sources: BTreeMap::from([(
                    "scripts/bad.lua".to_owned(),
                    "this is not valid Lua !!!".to_owned(),
                )]),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: None,
                interrupt,
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx,
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        assert!(matches!(
            script_thread.ready.recv().unwrap(),
            Err(ScriptStartupError::Load(error)) if error.contains("bad.lua")
        ));
        assert!(modules.shutdown().is_empty());
        module_stop_rx.recv().unwrap();
        script_thread.stopped.try_recv().unwrap();
    }

    #[test]
    fn script_library_load_error_is_reported_before_ready() {
        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let mut modules = ModuleHost::new();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, _frame_rx) = flume::bounded(1);
        let (render_tx, _render_rx) = flume::unbounded();
        let library = std::env::temp_dir()
            .join(format!("rve-missing-script-library-{}", std::process::id()))
            .join("game_scripts.dll");
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "startup.rvescene".to_owned(),
                cursor: CursorControl::default(),
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: Some(library),
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx,
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();

        assert!(matches!(
            script_thread.ready.recv().unwrap(),
            Err(ScriptStartupError::Load(_))
        ));
        assert!(modules.shutdown().is_empty());
        script_thread.stopped.try_recv().unwrap();
    }

    #[test]
    fn graceful_script_shutdown_runs_on_destroy_before_interrupt() {
        DESTROY_CALLS.store(0, Ordering::SeqCst);
        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n\n[[objects.heavy_scripts]]\nname = \"DestroyProbe\"\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let mut modules = ModuleHost::new();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let interrupt = ScriptInterruptHandle::new();
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "startup.rvescene".to_owned(),
                cursor: CursorControl::default(),
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: Some(script_module()),
                interrupt: interrupt.clone(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();

        script_thread.ready.recv().unwrap().unwrap();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();
        frame_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        script_tx.send(ScriptControl::Shutdown).unwrap();
        script_thread
            .stopped
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        assert_eq!(DESTROY_CALLS.load(Ordering::SeqCst), 1);
        assert!(!interrupt.is_interrupted());
        assert_eq!(render_rx.recv().unwrap(), RenderControl::Shutdown);
        modules.join_named(super::SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
    }

    #[test]
    fn script_thread_panic_after_ready_sets_failure_and_stops_render() {
        struct PanicClock;

        impl Clock for PanicClock {
            fn now(&self) -> Duration {
                panic!("clock panic")
            }
        }

        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let mut modules = ModuleHost::new();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, _frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let status = Arc::new(RuntimeStatus::default());
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "startup.rvescene".to_owned(),
                cursor: CursorControl::default(),
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                prefabs: PreparedPrefabs::default(),
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(PanicClock),
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::clone(&status),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
                audio: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();

        script_thread.ready.recv().unwrap().unwrap();
        script_tx.send(ScriptControl::Go(Box::default())).unwrap();
        assert_eq!(
            render_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            RenderControl::Shutdown
        );
        modules.join_named(super::SCRIPTS_THREAD).unwrap();
        script_thread.stopped.try_recv().unwrap();
        assert_eq!(status.exit_code(), 1);
        assert!(status.failure_message().unwrap().contains("clock panic"));
        assert!(modules.shutdown().is_empty());
    }

    const SPAWN_SCENE: &str = "[[assets]]\npath = \"gun.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"gun\"\n[[objects.lite_scripts]]\npath = \"gun.lua\"\n";
    const BALL_PREFAB: &str = "[[objects]]\nid = \"root\"\n[objects.collider]\nshape = \"sphere\"\nradius = 0.25\n[objects.rigid_body]\nkind = \"dynamic\"\n";

    fn prefab_template(path: &str, text: &str) -> Arc<engine_scene::PrefabTemplate> {
        let path = engine_files::VirtualPath::parse(path).unwrap();
        Arc::new(engine_scene::PrefabTemplate::load(&path, &mut |_| Ok(text.to_owned())).unwrap())
    }

    /// Быстрый префаб «ball» с телом-сферой и без лимита экземпляров.
    fn ball_prefabs(text: &str) -> PreparedPrefabs {
        let path = "prefabs/ball.rveprefab";
        let mut catalog = PrefabCatalog::default();
        catalog.insert(
            path,
            PrefabSpec {
                kind: PrefabKind::Fast,
                max_instances: None,
            },
        );
        PreparedPrefabs {
            templates: BTreeMap::from([(path.to_owned(), prefab_template(path, text))]),
            catalog,
        }
    }

    /// Один шаг физики глазами теста: что пришло в `step_with_changes`.
    #[derive(Clone, Debug)]
    struct StepRecord {
        changes: Vec<BodyChange>,
        commands: Vec<PhysicsCommand>,
    }

    /// Физика без GPU: настоящий `StepDriver` применяет вставки и удаления, тест получает записи.
    fn fake_physics(
        world: &World,
    ) -> (
        PhysicsClient,
        PhysicsInitialization,
        mpsc::Receiver<StepRecord>,
        std::thread::JoinHandle<()>,
    ) {
        let desc = PhysicsSceneDesc::from_world(world, &[], Vec3::ZERO, 60, 8).unwrap();
        let (driver, bindings) = desc.build().unwrap();
        let initialization = PhysicsInitialization::from_prebuilt(&driver, bindings).unwrap();
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        let (record_tx, record_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut driver = driver;
            while let Ok(request) = request_rx.recv() {
                match request {
                    PhysicsRequest::Step {
                        sequence,
                        dt,
                        changes,
                        commands,
                    } => {
                        let _ = record_tx.send(StepRecord {
                            changes: changes.clone(),
                            commands: commands.clone(),
                        });
                        let step = driver
                            .advance_step_with_changes(dt, changes, commands)
                            .expect("fake physics step");
                        let snapshot = driver.world().query_snapshot().unwrap();
                        let _ = reply_tx.send(PhysicsReply::Step(PhysicsStepResult {
                            skipped_commands: 0,
                            sequence,
                            substeps: step.substeps,
                            poses: step.poses,
                            events: step.events,
                            snapshot,
                        }));
                    }
                    PhysicsRequest::Shutdown => break,
                    PhysicsRequest::Init(_) => panic!("preinitialized server received Init"),
                }
            }
        });
        (
            PhysicsClient::new_preinitialized(request_tx, reply_rx),
            initialization,
            record_rx,
            worker,
        )
    }

    struct TestScripts {
        modules: ModuleHost,
        control: flume::Sender<ScriptControl>,
        frames: flume::Receiver<Arc<FrameSnapshot>>,
        thread: super::ScriptThreadHandle,
        _renders: flume::Receiver<RenderControl>,
    }

    fn start_test_scripts(
        world: World,
        sources: &[(&str, &str)],
        prefabs: PreparedPrefabs,
        physics: Option<(PhysicsClient, PhysicsInitialization)>,
    ) -> TestScripts {
        start_test_scripts_with_clock(
            world,
            sources,
            prefabs,
            physics,
            Arc::new(FakeClock::default()),
        )
    }

    fn start_test_scripts_with_clock(
        world: World,
        sources: &[(&str, &str)],
        prefabs: PreparedPrefabs,
        physics: Option<(PhysicsClient, PhysicsInitialization)>,
        clock: Arc<FakeClock>,
    ) -> TestScripts {
        start_test_scripts_configured(world, sources, prefabs, physics, clock, None, false)
    }

    /// `background` и `frame_limit` — headless-режим: фон и предел тиков, как у `rve-server`.
    fn start_test_scripts_configured(
        world: World,
        sources: &[(&str, &str)],
        prefabs: PreparedPrefabs,
        physics: Option<(PhysicsClient, PhysicsInitialization)>,
        clock: Arc<FakeClock>,
        frame_limit: Option<u64>,
        background: bool,
    ) -> TestScripts {
        let mut modules = ModuleHost::new();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let (physics_client, physics_initialization) = match physics {
            Some((client, initialization)) => (Some(client), Some(initialization)),
            None => (None, None),
        };
        if background {
            let _ = script_tx.send(ScriptControl::Background(true));
            let _ = script_tx.send(ScriptControl::Go(Box::default()));
        }
        let thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "spawn.rvescene".to_owned(),
                cursor: CursorControl::default(),
                sources: sources
                    .iter()
                    .map(|(path, source)| ((*path).to_owned(), (*source).to_owned()))
                    .collect(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock,
                background_tick_hz: 30,
                gamepad_devices: false,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client,
                physics_initialization,
                audio: None,
                prefabs,
                physics_geometry: Vec::new(),
                prefab_loader: flume::unbounded().0,
                render_resources: flume::unbounded().0,
                network: None,
                frame_limit,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        TestScripts {
            modules,
            control: script_tx,
            frames: frame_rx,
            thread,
            _renders: render_rx,
        }
    }

    /// Ждёт очередной шаг физики и кадр: цикл скриптов отправляет их парой, а кадр нужно
    /// забрать, иначе цикл встаёт на отправке снимка.
    fn next_step(scripts: &TestScripts, steps: &mpsc::Receiver<StepRecord>) -> StepRecord {
        let record = steps
            .recv_timeout(Duration::from_secs(2))
            .expect("шаг физики");
        scripts
            .frames
            .recv_timeout(Duration::from_secs(2))
            .expect("кадр");
        record
    }

    fn stop_test_scripts(mut scripts: TestScripts, worker: std::thread::JoinHandle<()>) {
        scripts.control.send(ScriptControl::Shutdown).unwrap();
        scripts
            .thread
            .stopped
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        scripts.modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(scripts.modules.shutdown().is_empty());
        worker.join().unwrap();
    }

    #[test]
    fn spawned_prefab_enters_physics_with_its_velocity_and_leaves_on_destroy() {
        let script = r#"
local Gun = {}
function Gun:on_update(dt)
  self.frames = (self.frames or 0) + 1
  if self.frames == 1 then
    rve.spawn("prefabs/ball.rveprefab", { id = "ball", velocity = { x = 0, y = 0, z = 3 } })
  elseif self.frames == 3 then
    rve.destroy("ball")
  end
end
return Gun
"#;
        let world =
            World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
                .unwrap();
        let (client, initialization, steps, worker) = fake_physics(&world);
        let scripts = start_test_scripts(
            world,
            &[("gun.lua", script)],
            ball_prefabs(BALL_PREFAB),
            Some((client, initialization)),
        );
        scripts.thread.ready.recv().unwrap().unwrap();
        scripts
            .control
            .send(ScriptControl::Go(Box::default()))
            .unwrap();

        let first = next_step(&scripts, &steps);
        let [BodyChange::Insert { handle, desc }] = first.changes.as_slice() else {
            panic!("первый шаг должен вставить одно тело: {:?}", first.changes);
        };
        assert_eq!(desc.kind, BodyKind::Dynamic);
        assert!(
            first.commands.contains(&PhysicsCommand::SetVelocity {
                body: *handle,
                velocity: Vec3::new(0.0, 0.0, 3.0),
            }),
            "скорость spawn должна идти в том же шаге после вставки: {:?}",
            first.commands
        );

        let mut removed = false;
        for _ in 0..2 {
            let record = next_step(&scripts, &steps);
            removed |= record
                .changes
                .contains(&BodyChange::Remove { handle: *handle });
        }
        assert!(removed, "destroy должен удалить тело в физике");
        // После удаления шаги продолжаются без ошибок.
        assert!(next_step(&scripts, &steps).changes.is_empty());
        stop_test_scripts(scripts, worker);
    }

    #[test]
    fn spawn_rejected_by_physics_is_rolled_back_and_frees_its_id() {
        let script = r#"
local Gun = {}
function Gun:on_update(dt)
  self.frames = (self.frames or 0) + 1
  if self.frames == 1 then
    rve.spawn("prefabs/ball.rveprefab", { id = "ball", scale = { x = 0, y = 0, z = 0 } })
  elseif self.frames == 2 then
    rve.spawn("prefabs/ball.rveprefab", { id = "ball" })
  end
end
return Gun
"#;
        let world =
            World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
                .unwrap();
        let (client, initialization, steps, worker) = fake_physics(&world);
        let scripts = start_test_scripts(
            world,
            &[("gun.lua", script)],
            ball_prefabs(BALL_PREFAB),
            Some((client, initialization)),
        );
        scripts.thread.ready.recv().unwrap().unwrap();
        scripts
            .control
            .send(ScriptControl::Go(Box::default()))
            .unwrap();

        // Нулевой масштаб физика отвергает: тело не вставляется, runtime продолжает шаги.
        let rejected = next_step(&scripts, &steps);
        assert!(
            rejected
                .changes
                .iter()
                .all(|change| !matches!(change, BodyChange::Insert { .. })),
            "отклонённый spawn не должен давать вставку: {:?}",
            rejected.changes
        );
        // Откат освободил id: второй spawn с тем же id получает тело.
        let retried = next_step(&scripts, &steps);
        assert!(
            retried
                .changes
                .iter()
                .any(|change| matches!(change, BodyChange::Insert { .. })),
            "id отклонённого spawn должен быть свободен: {:?}",
            retried.changes
        );
        stop_test_scripts(scripts, worker);
    }

    /// Probe создаёт тело `dot` один раз на экземпляр. Чтение `self.object` у мёртвого объекта
    /// даёт nil, поэтому условие по id здесь не годится: вызов самого update должен быть виден.
    const PROBE_LUA: &str = r#"
local Probe = {}
function Probe:on_update(dt)
  if not self.spawned then
    self.spawned = true
    rve.spawn("prefabs/dot.rveprefab")
  end
end
return Probe
"#;
    const PROBE_BALL: &str = "[[assets]]\npath = \"probe.lua\"\n[[objects]]\nid = \"root\"\n[objects.collider]\nshape = \"sphere\"\nradius = 0.25\n[objects.rigid_body]\nkind = \"dynamic\"\n[[objects.lite_scripts]]\npath = \"probe.lua\"\n";

    #[test]
    fn spawn_rejected_by_physics_leaves_no_script_instances_behind() {
        let script = r#"
local Gun = {}
function Gun:on_update(dt)
  self.frames = (self.frames or 0) + 1
  if self.frames == 1 then
    rve.spawn("prefabs/ball.rveprefab", { id = "ball", scale = { x = 0, y = 0, z = 0 } })
  elseif self.frames == 2 then
    rve.spawn("prefabs/ball.rveprefab", { id = "ball" })
  end
end
return Gun
"#;
        let world =
            World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
                .unwrap();
        let mut prefabs = ball_prefabs(PROBE_BALL);
        let dot = "prefabs/dot.rveprefab";
        prefabs.catalog.insert(
            dot,
            PrefabSpec {
                kind: PrefabKind::Fast,
                max_instances: None,
            },
        );
        prefabs
            .templates
            .insert(dot.to_owned(), prefab_template(dot, BALL_PREFAB));
        let (client, initialization, steps, worker) = fake_physics(&world);
        let scripts = start_test_scripts(
            world,
            &[("gun.lua", script), ("probe.lua", PROBE_LUA)],
            prefabs,
            Some((client, initialization)),
        );
        scripts.thread.ready.recv().unwrap().unwrap();
        scripts
            .control
            .send(ScriptControl::Go(Box::default()))
            .unwrap();

        // Повторный `ball` даёт тело, и его probe даёт тело `dot`. Экземпляр отвергнутого spawn
        // не должен дать третье тело: иначе его probe вызывался бы у мёртвого объекта.
        let inserts = (0..5)
            .flat_map(|_| next_step(&scripts, &steps).changes)
            .filter(|change| matches!(change, BodyChange::Insert { .. }))
            .count();
        assert_eq!(inserts, 2, "отвергнутый spawn оставил экземпляр скрипта");
        stop_test_scripts(scripts, worker);
    }

    #[test]
    fn fast_prefab_with_a_missing_script_source_fails_startup_with_a_load_error() {
        let prefab = "[[assets]]\npath = \"bullet.lua\"\n[[objects]]\nid = \"root\"\n[[objects.lite_scripts]]\npath = \"bullet.lua\"\n";
        let world =
            World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
                .unwrap();
        let mut scripts = start_test_scripts(
            world,
            &[("gun.lua", "return {}")],
            ball_prefabs(prefab),
            None,
        );
        match scripts.thread.ready.recv().unwrap() {
            Err(ScriptStartupError::Load(message)) => {
                assert!(message.contains("bullet.lua"), "{message}")
            }
            other => panic!("ожидалась ошибка загрузки скриптов, получено {other:?}"),
        }
        scripts.modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(scripts.modules.shutdown().is_empty());
    }

    #[cfg(feature = "dev-console")]
    #[test]
    fn console_destroy_of_a_live_body_does_not_break_the_next_physics_step() {
        let script = r#"
local Gun = {}
function Gun:on_start()
  rve.spawn("prefabs/ball.rveprefab", { id = "ball" })
  rve.console.register("kill", function() rve.destroy("ball") end, "")
end
return Gun
"#;
        let world =
            World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
                .unwrap();
        let (client, initialization, steps, worker) = fake_physics(&world);
        let scripts = start_test_scripts(
            world,
            &[("gun.lua", script)],
            ball_prefabs(BALL_PREFAB),
            Some((client, initialization)),
        );
        scripts.thread.ready.recv().unwrap().unwrap();
        scripts
            .control
            .send(ScriptControl::Go(Box::default()))
            .unwrap();

        let first = next_step(&scripts, &steps);
        let [BodyChange::Insert { handle, .. }] = first.changes.as_slice() else {
            panic!("первый шаг должен вставить тело: {:?}", first.changes);
        };
        let handle = *handle;
        scripts
            .control
            .send(ScriptControl::Console(CommandLine {
                name: "kill".to_owned(),
                args: Vec::new(),
            }))
            .unwrap();
        // Команда может попасть в любой из ближайших шагов, но удаление должно прийти в одном из них.
        let mut removed = false;
        for _ in 0..4 {
            let record = next_step(&scripts, &steps);
            removed |= record.changes.contains(&BodyChange::Remove { handle });
        }
        assert!(removed, "консольный destroy должен удалить тело");
        stop_test_scripts(scripts, worker);
    }

    #[test]
    fn motion_history_does_not_carry_over_to_a_recycled_handle() {
        let old = ObjectHandle::from_abi(1).unwrap();
        let recycled = ObjectHandle::from_abi((1_u64 << 32) | 1).unwrap();
        let model = [[1.0; 4]; 4];
        let camera = motion_test_handles().0;
        let mut history = MotionHistory::default();
        history.submitted(&motion_snapshot(camera, &[(old, model)]));
        assert_eq!(history.previous_model(old, false), Some(model));
        assert_eq!(history.previous_model(recycled, false), None);

        // Удалённый объект выпадает из истории при следующей отправке кадра.
        history.submitted(&motion_snapshot(camera, &[]));
        assert_eq!(history.previous_model(old, false), None);
    }

    #[test]
    fn prefab_levels_are_copied_per_spawned_object_and_dropped_with_it() {
        let root = ObjectHandle::from_abi(1).unwrap();
        let child = ObjectHandle::from_abi(2).unwrap();
        let spawned = engine_script::SpawnedObject {
            handle: root,
            objects: vec![root, child],
            prefab: "prefabs/ball.rveprefab".to_owned(),
            template_index: 0,
            initial_velocity: None,
        };
        // Корень с мешом, ребёнок без меша: у ребёнка записи нет.
        let templates = vec![vec![vec![1_u32, 2]], Vec::new()];
        let mut by_object = HashMap::new();
        attach_prefab_levels(&mut by_object, &templates, &spawned);
        assert_eq!(by_object.get(&root), Some(&vec![vec![1, 2]]));
        assert!(!by_object.contains_key(&child));

        detach_objects(&mut by_object, &[root]);
        assert!(by_object.is_empty());
    }

    const BALL_PATH: &str = "prefabs/ball.rveprefab";

    /// Скрипт создаёт тяжёлый шар при старте.
    const SPAWN_ON_START: &str = "local Gun = {}\nfunction Gun:on_start()\n  rve.spawn(\"prefabs/ball.rveprefab\", { id = \"ball\" })\nend\nreturn Gun\n";
    /// Скрипт удаляет шар на втором кадре и создаёт его заново на третьем.
    const SPAWN_AND_RESPAWN: &str = "local Gun = {}\nfunction Gun:on_start()\n  rve.spawn(\"prefabs/ball.rveprefab\", { id = \"ball\" })\nend\nfunction Gun:on_update(dt)\n  self.frames = (self.frames or 0) + 1\n  if self.frames == 2 then\n    rve.destroy(\"ball\")\n  elseif self.frames == 3 then\n    rve.prefabs.unload(\"prefabs/ball.rveprefab\")\n  elseif self.frames == 4 then\n    rve.spawn(\"prefabs/ball.rveprefab\", { id = \"ball\" })\n  end\nend\nreturn Gun\n";

    fn heavy_ball_catalog() -> PrefabCatalog {
        let mut catalog = PrefabCatalog::default();
        catalog.insert(
            BALL_PATH,
            PrefabSpec {
                kind: PrefabKind::Heavy,
                max_instances: None,
            },
        );
        catalog
    }

    fn heavy_ready(token: PrefabToken) -> PrefabReply {
        PrefabReply::Ready(Box::new(PrefabReady {
            token,
            gpu: Vec::new(),
            particles: Vec::new(),
            cpu: PrefabCpu {
                template: prefab_template(BALL_PATH, BALL_PREFAB),
                sources: BTreeMap::new(),
                geometry: Vec::new(),
            },
        }))
    }

    /// Скрипты, PrefabLoads и мир без потоков: тест сам передаёт ответы и применяет изменения.
    struct HeavyHarness {
        world: World,
        scripts: ScriptHost,
        loads: PrefabLoads,
        gpu: GpuSceneTable,
        spawn_physics: SpawnPhysics,
        pending: VecDeque<ScriptReply>,
        material: MaterialSync,
        loader_rx: flume::Receiver<LoaderRequest>,
        render_rx: flume::Receiver<RenderResourceRequest>,
        elapsed: f64,
        frame: u64,
    }

    impl HeavyHarness {
        fn new(script: &str) -> Self {
            let world =
                World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
                    .unwrap();
            let sources = BTreeMap::from([("gun.lua".to_owned(), script.to_owned())]);
            let scripts = ScriptHost::load_with_options(
                &world,
                "spawn.rvescene",
                &sources,
                None,
                ScriptLoadOptions {
                    prefabs: heavy_ball_catalog(),
                    ..ScriptLoadOptions::default()
                },
            )
            .unwrap();
            let (loader_tx, loader_rx) = flume::unbounded();
            let (render_tx, render_rx) = flume::unbounded();
            Self {
                world,
                scripts,
                loads: PrefabLoads::new(loader_tx, render_tx),
                gpu: GpuSceneTable::default(),
                spawn_physics: SpawnPhysics::default(),
                pending: VecDeque::new(),
                material: MaterialSync::default(),
                loader_rx,
                render_rx,
                elapsed: 0.0,
                frame: 0,
            }
        }

        fn time(&self) -> engine_script::ScriptTime {
            engine_script::ScriptTime {
                elapsed: self.elapsed,
                frame: self.frame,
            }
        }

        fn apply(&mut self) {
            apply_world_changes(
                &mut self.world,
                &mut self.scripts,
                None,
                &mut self.spawn_physics,
                &mut self.gpu,
                &mut self.loads,
                None,
            );
        }

        fn start(&mut self) {
            let time = self.time();
            self.scripts.start(&mut self.world, time);
            self.apply();
        }

        fn reply(&mut self, reply: PrefabReply) {
            self.pending.push_back(ScriptReply::Prefab(reply));
            drain_prefab_replies(
                &mut self.pending,
                &mut self.scripts,
                &mut self.world,
                &mut self.gpu,
                &mut self.spawn_physics,
                &mut self.loads,
                &mut self.material,
            );
        }

        fn update(&mut self) {
            self.frame += 1;
            self.elapsed += 0.016;
            let time = self.time();
            self.scripts.update(&mut self.world, 0.016, time);
            self.apply();
        }

        fn next_load(&self) -> PrefabToken {
            match self.loader_rx.try_recv() {
                Ok(LoaderRequest::Load(token)) => token,
                _ => panic!("ожидалась заявка загрузки префаба"),
            }
        }

        fn ball_alive(&self) -> bool {
            self.world.object_by_id("ball").is_some()
        }
    }

    #[test]
    fn heavy_spawn_waits_for_its_load_and_materializes_once_ready() {
        let mut harness = HeavyHarness::new(SPAWN_ON_START);
        harness.start();
        let token = harness.next_load();
        assert_eq!(token.path, BALL_PATH);
        assert!(!harness.ball_alive(), "spawn ждёт готовности префаба");

        harness.reply(heavy_ready(token));
        harness.update();
        assert!(
            harness.ball_alive(),
            "после готовности отложенный spawn применён"
        );
    }

    #[test]
    fn failed_heavy_load_kills_its_pending_spawn_with_an_error() {
        let mut harness = HeavyHarness::new(SPAWN_ON_START);
        harness.start();
        let token = harness.next_load();
        harness.reply(PrefabReply::Failed {
            token,
            error: "файл не найден".to_owned(),
        });
        harness.update();
        assert!(!harness.ball_alive());
        let events = harness.scripts.drain_events();
        assert!(
            events.iter().any(|event| {
                matches!(event.level, engine_script::ScriptEventLevel::Error)
                    && event.message.contains("failed to load")
            }),
            "ожидалась ошибка загрузки: {events:?}"
        );
    }

    #[test]
    fn unload_then_respawn_uses_a_new_load_and_releases_only_the_old_token() {
        let mut harness = HeavyHarness::new(SPAWN_AND_RESPAWN);
        harness.loads.set_last_sent_seq(7);
        harness.start();
        let first = harness.next_load();
        harness.reply(heavy_ready(first.clone()));
        harness.update();
        assert!(harness.ball_alive());

        // Второй кадр удаляет последний экземпляр, третий явно выгружает префаб.
        harness.update();
        assert!(!harness.ball_alive());
        harness.update();
        // Заявки эмиттеров удалённых объектов идут в том же канале раньше освобождения.
        let released = std::iter::from_fn(|| harness.render_rx.try_recv().ok())
            .find(|request| matches!(request, RenderResourceRequest::Release { .. }));
        match released {
            Some(RenderResourceRequest::Release { token, last_seq }) => {
                assert_eq!(token, first);
                assert_eq!(last_seq, 7);
            }
            _ => panic!("выгрузка должна освободить старый токен"),
        }

        // Четвёртый кадр создаёт шар заново: новая загрузка получает новый load_id.
        harness.update();
        let second = harness.next_load();
        assert!(second.load_id > first.load_id);
        assert!(
            harness.render_rx.try_recv().is_err(),
            "новая загрузка не должна освобождать ресурсы"
        );
        harness.reply(heavy_ready(second));
        harness.update();
        assert!(harness.ball_alive());
    }

    #[test]
    fn ready_with_a_stale_load_id_is_released_immediately() {
        let mut harness = HeavyHarness::new(SPAWN_ON_START);
        harness.start();
        let current = harness.next_load();
        let stale = PrefabToken {
            path: BALL_PATH.to_owned(),
            load_id: current.load_id + 100,
        };
        harness.reply(heavy_ready(stale.clone()));
        match harness.render_rx.try_recv() {
            Ok(RenderResourceRequest::Release { token, .. }) => assert_eq!(token, stale),
            _ => panic!("устаревшая загрузка должна освобождаться сразу"),
        }
        assert!(!harness.ball_alive());
        harness.reply(heavy_ready(current));
        harness.update();
        assert!(harness.ball_alive());
    }

    /// Рендер с пустым FakeBackend: снимки, паузу и ресурсы можно отправлять напрямую.
    struct RenderHarness {
        frames: flume::Sender<Arc<FrameSnapshot>>,
        controls: flume::Sender<RenderControl>,
        resources: flume::Sender<RenderResourceRequest>,
        scripts: flume::Receiver<ScriptControl>,
        released: mpsc::Receiver<PrefabToken>,
        #[cfg(feature = "dev-console")]
        debug_views: mpsc::Receiver<engine_render::DebugView>,
        renders: Arc<AtomicUsize>,
        worker: std::thread::JoinHandle<Result<(), String>>,
    }

    fn start_render_harness() -> RenderHarness {
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (control_tx, control_rx) = flume::unbounded();
        let (script_tx, script_rx) = flume::unbounded();
        let (resource_tx, resource_rx) = flume::unbounded();
        let (released_tx, released_rx) = mpsc::channel();
        #[cfg(feature = "dev-console")]
        let (debug_view_tx, debug_view_rx) = mpsc::channel();
        let renders = Arc::new(AtomicUsize::new(0));
        let mut backend = FakeBackend {
            shared_renders: Some(Arc::clone(&renders)),
            released: Some(released_tx),
            #[cfg(feature = "dev-console")]
            debug_views: Some(debug_view_tx),
            ..FakeBackend::default()
        };
        let worker = std::thread::spawn(move || {
            render_loop(
                &mut backend,
                640,
                480,
                None,
                &frame_rx,
                &resource_rx,
                &control_rx,
                &script_tx,
                &mut render_test_window(),
            )
        });
        RenderHarness {
            frames: frame_tx,
            controls: control_tx,
            resources: resource_tx,
            scripts: script_rx,
            released: released_rx,
            #[cfg(feature = "dev-console")]
            debug_views: debug_view_rx,
            renders,
            worker,
        }
    }

    fn render_token(load_id: u64) -> PrefabToken {
        PrefabToken {
            path: BALL_PATH.to_owned(),
            load_id,
        }
    }

    #[test]
    fn paused_render_releases_a_prefab_without_new_snapshots() {
        let render = start_render_harness();
        let mut snapshot = empty_snapshot();
        snapshot.sequence = 3;
        render.frames.send(Arc::new(snapshot)).unwrap();
        render.controls.send(RenderControl::Occluded(true)).unwrap();
        assert!(matches!(
            render.scripts.recv_timeout(Duration::from_secs(1)).unwrap(),
            ScriptControl::Background(true)
        ));

        render
            .resources
            .send(RenderResourceRequest::Release {
                token: render_token(1),
                last_seq: 3,
            })
            .unwrap();
        assert_eq!(
            render
                .released
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            render_token(1)
        );
        render.controls.send(RenderControl::Shutdown).unwrap();
        assert!(render.worker.join().unwrap().is_ok());
    }

    #[test]
    fn release_waits_until_the_snapshot_that_may_reference_it_is_handled() {
        let render = start_render_harness();
        render
            .resources
            .send(RenderResourceRequest::Release {
                token: render_token(1),
                last_seq: 5,
            })
            .unwrap();
        for sequence in [4, 5] {
            let mut snapshot = empty_snapshot();
            snapshot.sequence = sequence;
            render.frames.send(Arc::new(snapshot)).unwrap();
        }
        assert_eq!(
            render
                .released
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            render_token(1)
        );
        // Освобождение наступает только после того, как снимок 5 отрисован.
        assert_eq!(render.renders.load(Ordering::SeqCst), 2);
        render.controls.send(RenderControl::Shutdown).unwrap();
        assert!(render.worker.join().unwrap().is_ok());
    }

    /// Удержанный снимок с `seq ≤ last_seq` сбрасывается вместе с освобождением, поэтому
    /// перерисовка по смене debug view его не рисует. Снимок 4 отправляется только после того,
    /// как цикл принял смену view: перерисовка успевает отработать до приёма следующего снимка.
    #[cfg(feature = "dev-console")]
    #[test]
    fn redraw_does_not_draw_a_snapshot_dropped_by_release() {
        let render = start_render_harness();
        let mut snapshot = empty_snapshot();
        snapshot.sequence = 3;
        render.frames.send(Arc::new(snapshot)).unwrap();
        render
            .resources
            .send(RenderResourceRequest::Release {
                token: render_token(1),
                last_seq: 3,
            })
            .unwrap();
        assert_eq!(
            render
                .released
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            render_token(1)
        );
        assert_eq!(render.renders.load(Ordering::SeqCst), 1);

        render
            .controls
            .send(RenderControl::DebugView(
                engine_render::DebugView::Wireframe,
            ))
            .unwrap();
        assert_eq!(
            render
                .debug_views
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            engine_render::DebugView::Wireframe
        );
        let mut next = empty_snapshot();
        next.sequence = 4;
        render.frames.send(Arc::new(next)).unwrap();
        render
            .resources
            .send(RenderResourceRequest::Release {
                token: render_token(2),
                last_seq: 4,
            })
            .unwrap();
        assert_eq!(
            render
                .released
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            render_token(2)
        );
        assert_eq!(
            render.renders.load(Ordering::SeqCst),
            2,
            "перерисовка не должна рисовать сброшенный снимок 3"
        );
        render.controls.send(RenderControl::Shutdown).unwrap();
        assert!(render.worker.join().unwrap().is_ok());
    }

    #[test]
    fn prefab_upload_completes_while_the_render_is_paused() {
        let render = start_render_harness();
        render.controls.send(RenderControl::Occluded(true)).unwrap();
        assert!(matches!(
            render.scripts.recv_timeout(Duration::from_secs(1)).unwrap(),
            ScriptControl::Background(true)
        ));
        let prefab = LoadedPrefab {
            token: render_token(1),
            cpu: PrefabCpu {
                template: prefab_template(BALL_PATH, BALL_PREFAB),
                sources: BTreeMap::new(),
                geometry: Vec::new(),
            },
            models: HashMap::new(),
            textures: HashMap::new(),
            material_assets: HashMap::new(),
            shader_assets: HashMap::new(),
            levels: Vec::new(),
            particles: Vec::new(),
        };
        render
            .resources
            .send(RenderResourceRequest::Upload(Box::new(prefab)))
            .unwrap();
        match render.scripts.recv_timeout(Duration::from_secs(1)).unwrap() {
            ScriptControl::Prefab(PrefabReply::Ready(ready)) => {
                assert_eq!(ready.token, render_token(1));
            }
            other => panic!("ожидался готовый префаб на паузе: {other:?}"),
        }
        render.controls.send(RenderControl::Shutdown).unwrap();
        assert!(render.worker.join().unwrap().is_ok());
    }

    #[test]
    fn active_camera_cursor_flags_are_published_before_scripts_start() {
        let document = SceneDocument::parse(
            "cursor.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = true\n[objects.camera.cursor]\nhide_on_start = true\nlock_on_start = true\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let mut scripts = ScriptHost::load(&world, "scene", &BTreeMap::new(), None).unwrap();
        let control = CursorControl::default();
        let sink = std::sync::Arc::new(RecordingCursorSink::default());
        control.attach_sink(sink.clone());
        let mut published = None;

        set_initial_cursor(&mut scripts, &world, &control, &mut published);

        assert_eq!(
            sink.0.lock().unwrap().last(),
            Some(&CursorState {
                visible: false,
                locked: true,
                lock_position: None,
            })
        );
    }

    #[test]
    fn script_cursor_change_is_published_after_update_once() {
        let document = SceneDocument::parse(
            "cursor.rvescene",
            "[[assets]]\npath = \"/cursor.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = true\n[[objects.lite_scripts]]\npath = \"/cursor.lua\"\n",
        )
        .unwrap();
        let mut world = World::from_document(&document).unwrap();
        let source = "local Cursor = {}\nfunction Cursor:on_update()\n  rve.input.set_cursor_locked(true)\nend\nreturn Cursor\n";
        let sources = BTreeMap::from([("cursor.lua".to_owned(), source.to_owned())]);
        let mut scripts = ScriptHost::load(&world, "scene", &sources, None).unwrap();
        let control = CursorControl::default();
        let sink = std::sync::Arc::new(RecordingCursorSink::default());
        control.attach_sink(sink.clone());
        let mut published = None;
        set_initial_cursor(&mut scripts, &world, &control, &mut published);

        scripts.update(&mut world, 0.1, ScriptTime::default());
        publish_script_cursor(&scripts, &control, &mut published);

        let states = sink.0.lock().unwrap().clone();
        assert_eq!(
            states.last(),
            Some(&CursorState {
                visible: true,
                locked: true,
                lock_position: None,
            })
        );
        publish_script_cursor(&scripts, &control, &mut published);
        assert_eq!(sink.0.lock().unwrap().len(), states.len());
    }

    #[test]
    fn skinned_object_whose_palette_has_another_joint_count_is_dropped_from_skinning() {
        // Регрессия: палитра другой длины не должна доходить до renderer и валить кадр.
        let text = "[[assets]]\npath = \"/models/hero.fbx\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"hero\"\n[objects.mesh_renderer]\nmodel = \"/models/hero.fbx\"\n[objects.animator]\n";
        let world = World::from_document(
            &engine_scene::SceneDocument::parse("scene.rvescene", text).unwrap(),
        )
        .unwrap();
        let hero = world.handle("hero").unwrap();
        let identity = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
            [0.0, 0.0, 0.0, 1.0],
        ];
        let model = std::sync::Arc::new(engine_assets::Model {
            meshes: Vec::new(),
            textures: Vec::new(),
            skeleton: Some(engine_assets::ModelSkeleton {
                nodes: vec![engine_assets::ModelNode {
                    name: "bone".to_owned(),
                    parent: None,
                    inherit_mode: engine_assets::ModelInheritMode::Normal,
                    inherit_scale_node: None,
                    rest: engine_assets::ModelTransform {
                        translation: [0.0; 3],
                        rotation: [0.0, 0.0, 0.0, 1.0],
                        scale: [1.0; 3],
                    },
                }],
                joints: vec![engine_assets::ModelJoint {
                    node: 0,
                    inverse_bind: identity,
                }],
            }),
            animations: Vec::new(),
        });
        // Палитра моста одна (одна кость), а меш объявлен с двумя костями.
        let animation = AnimationBridge::build(
            &world,
            |_| Ok(model.clone()),
            |_| Ok(std::sync::Arc::new(Vec::new())),
        );
        let mut scene = GpuSceneTable::default();
        scene.skin_instances.insert(hero, Vec::new());
        scene.joint_counts.insert(hero, 2);
        assert_eq!(
            drop_mismatched_skins(&world, &mut scene, &animation),
            vec![hero]
        );
        assert!(!scene.skin_instances.contains_key(&hero));

        scene.skin_instances.insert(hero, Vec::new());
        scene.joint_counts.insert(hero, 1);
        assert!(drop_mismatched_skins(&world, &mut scene, &animation).is_empty());
        assert!(scene.skin_instances.contains_key(&hero));
    }
}

#[cfg(test)]
mod particle_step_tests {
    use super::particle_step;

    #[test]
    fn first_submitted_frame_does_not_step_particles() {
        assert_eq!(particle_step(12.0, None), 0.0);
    }

    #[test]
    fn step_is_simulation_time_since_last_submitted_frame() {
        assert!((particle_step(1.5, Some(1.25)) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn non_advancing_or_backwards_time_gives_no_step() {
        assert_eq!(particle_step(2.0, Some(2.0)), 0.0);
        assert_eq!(particle_step(1.0, Some(2.0)), 0.0);
    }
}

#[cfg(test)]
mod particle_emitter_tests {
    use super::{ParticleRequest, apply_emitter_request, spawn_particle_requests};
    use crate::scene::particle_emitter_desc;
    use engine_render::ParticleEmitterDesc;
    use engine_scene::{ObjectHandle, ParticleEmitterComponent, SceneDocument, World};
    use std::collections::HashMap;

    fn handles() -> [ObjectHandle; 3] {
        let document = SceneDocument::parse(
            "particles.rvescene",
            "[[objects]]\nid=\"camera\"\n[objects.camera]\n[[objects]]\nid=\"a\"\n[[objects]]\nid=\"b\"\n[[objects]]\nid=\"c\"\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        ["a", "b", "c"].map(|id| world.handle(id).unwrap())
    }

    fn desc(capacity: u32) -> ParticleEmitterDesc {
        particle_emitter_desc(
            &ParticleEmitterComponent {
                capacity,
                ..ParticleEmitterComponent::default()
            },
            None,
        )
    }

    /// Эмиттер в тесте — его ёмкость; уничтожение пишет id в журнал.
    fn create(_: &mut Vec<u32>, desc: ParticleEmitterDesc) -> Result<u32, String> {
        if desc.capacity == 0 {
            return Err("ёмкость 0".to_owned());
        }
        Ok(desc.capacity)
    }

    fn destroy(log: &mut Vec<u32>, id: u32) {
        log.push(id);
    }

    fn apply(table: &mut HashMap<ObjectHandle, u32>, log: &mut Vec<u32>, request: ParticleRequest) {
        apply_emitter_request(table, log, request, create, destroy);
    }

    fn create_request(object: ObjectHandle, capacity: u32) -> ParticleRequest {
        ParticleRequest::Create {
            object,
            desc: Box::new(desc(capacity)),
        }
    }

    #[test]
    fn spawn_requests_follow_template_order_and_skip_objects_without_emitter() {
        let [a, b, c] = handles();
        let requests = spawn_particle_requests(&[a, b, c], &[Some(desc(8)), None, Some(desc(16))]);
        let created: Vec<(ObjectHandle, u32)> = requests
            .into_iter()
            .map(|request| match request {
                ParticleRequest::Create { object, desc } => (object, desc.capacity),
                ParticleRequest::Destroy(_) => panic!("spawn creates only"),
            })
            .collect();
        assert_eq!(created, vec![(a, 8), (c, 16)]);
    }

    #[test]
    fn create_inserts_the_emitter_of_the_object() {
        let [a, ..] = handles();
        let mut table = HashMap::new();
        let mut log = Vec::new();
        apply(&mut table, &mut log, create_request(a, 8));
        assert_eq!(table.get(&a), Some(&8));
        assert!(log.is_empty());
    }

    #[test]
    fn create_for_an_object_with_an_emitter_destroys_the_previous_one() {
        let [a, ..] = handles();
        let mut table = HashMap::new();
        let mut log = Vec::new();
        apply(&mut table, &mut log, create_request(a, 8));
        apply(&mut table, &mut log, create_request(a, 16));
        assert_eq!(table.get(&a), Some(&16));
        assert_eq!(log, vec![8]);
    }

    #[test]
    fn failed_create_leaves_the_object_without_an_emitter() {
        let [a, ..] = handles();
        let mut table = HashMap::new();
        let mut log = Vec::new();
        apply(&mut table, &mut log, create_request(a, 0));
        assert!(!table.contains_key(&a));
        assert!(log.is_empty());
    }

    #[test]
    fn destroy_removes_known_objects_and_ignores_unknown_ones() {
        let [a, b, c] = handles();
        let mut table = HashMap::new();
        let mut log = Vec::new();
        apply(&mut table, &mut log, create_request(a, 8));
        apply(&mut table, &mut log, create_request(b, 16));
        apply(&mut table, &mut log, ParticleRequest::Destroy(vec![a, c]));
        assert!(!table.contains_key(&a));
        assert_eq!(table.get(&b), Some(&16));
        assert_eq!(log, vec![8]);
        apply(&mut table, &mut log, ParticleRequest::Destroy(vec![a]));
        assert_eq!(log, vec![8], "повторное уничтожение не трогает эмиттер");
    }
}

#[cfg(test)]
mod particle_command_tests {
    use super::resolve_particle_commands;
    use engine_scene::world::ParticleCommand;
    use engine_scene::{ObjectHandle, SceneDocument, World};
    use std::collections::HashMap;

    const SCENE: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n\
[[objects]]\nid = \"fire\"\n[objects.particle_emitter]\n\
[[objects]]\nid = \"crate\"\n";

    #[test]
    fn commands_resolve_to_their_emitter_and_skip_objects_without_one() {
        let world =
            World::from_document(&SceneDocument::parse("test.rvescene", SCENE).unwrap()).unwrap();
        let fire: ObjectHandle = world.handle("fire").unwrap();
        let crate_handle: ObjectHandle = world.handle("crate").unwrap();
        let emitters = HashMap::from([(fire, 7_u32)]);
        let resolved = resolve_particle_commands(
            &emitters,
            &[
                ParticleCommand::Burst {
                    object: fire,
                    count: 4,
                },
                ParticleCommand::Restart { object: fire },
                ParticleCommand::Burst {
                    object: crate_handle,
                    count: 9,
                },
            ],
        );
        assert_eq!(resolved, vec![(7, Some(4)), (7, None)]);
    }
}

#[cfg(test)]
mod screen_shader_selection_tests {
    use super::*;
    use engine_files::VirtualPath;
    use engine_scene::ScreenShaderStage as SceneStage;

    fn shader(
        path: &str,
        stage: SceneStage,
        enabled: bool,
        params: &[f32],
    ) -> engine_scene::ScreenShaderRef {
        engine_scene::ScreenShaderRef {
            shader: VirtualPath::parse(path).unwrap(),
            stage,
            enabled,
            params: params.to_vec(),
        }
    }

    #[test]
    fn enabled_passes_keep_file_order_stage_and_padded_parameters() {
        let shaders = [
            shader("fx/vhs.glsl", SceneStage::AfterPost, true, &[0.7, 0.3]),
            shader("fx/off.glsl", SceneStage::BeforePost, false, &[1.0]),
            shader("fx/failed.glsl", SceneStage::BeforePost, true, &[]),
            shader("fx/color.glsl", SceneStage::BeforePost, true, &[]),
        ];
        let ids = HashMap::from([
            ("fx/vhs.glsl".to_owned(), 1_u32),
            ("fx/off.glsl".to_owned(), 3),
            ("fx/color.glsl".to_owned(), 2),
        ]);
        let [first, second, third, fourth] = enabled_screen_passes(&shaders, &ids);
        let (id, stage, params) = first.expect("первый включённый проход");
        assert_eq!(
            (id, stage),
            (1, engine_render::ScreenShaderStage::AfterPost)
        );
        assert_eq!(params[..2], [0.7, 0.3]);
        assert!(params[2..].iter().all(|value| *value == 0.0));
        assert_eq!(
            second.map(|(id, stage, _)| (id, stage)),
            Some((2, engine_render::ScreenShaderStage::BeforePost)),
            "выключенный и не созданный шейдеры пропущены"
        );
        assert!(third.is_none() && fourth.is_none());
    }

    #[test]
    fn selection_keeps_at_most_four_passes() {
        let shaders: Vec<_> = (0..5)
            .map(|index| {
                shader(
                    &format!("fx/{index}.glsl"),
                    SceneStage::AfterPost,
                    true,
                    &[],
                )
            })
            .collect();
        let ids: HashMap<String, u32> = (0..5)
            .map(|index| (format!("fx/{index}.glsl"), index))
            .collect();
        let passes = enabled_screen_passes(&shaders, &ids);
        assert_eq!(
            passes.iter().flatten().count(),
            engine_render::SCREEN_SHADER_MAX_PASSES
        );
    }
}

#[cfg(test)]
mod post_process_mapping_tests {
    use super::*;

    #[test]
    fn scene_defaults_map_to_renderer_defaults() {
        assert_eq!(
            render_post_process(&PostProcessSettings::default()),
            PostProcess::default()
        );
    }

    #[test]
    fn disabled_exposure_and_tonemapping_become_identity_values() {
        let mut post = PostProcessSettings::default();
        post.exposure.enabled = false;
        post.exposure.ev = 2.0;
        post.tonemapping.enabled = false;
        let mapped = render_post_process(&post);
        assert_eq!(mapped.exposure, Exposure::default());
        assert_eq!(mapped.tonemapper, RenderTonemapper::None);
    }

    #[test]
    fn enabled_effects_keep_their_parameters_and_quality() {
        let mut post = PostProcessSettings::default();
        post.bloom.enabled = true;
        post.bloom.intensity = 1.5;
        post.ambient_occlusion.enabled = true;
        post.ambient_occlusion.quality = engine_scene::AmbientOcclusionQuality::High;
        post.exposure.mode = engine_scene::ExposureMode::Auto;
        post.exposure.ev = -0.5;
        let mapped = render_post_process(&post);
        assert!(mapped.bloom.enabled);
        assert_eq!(mapped.bloom.intensity, 1.5);
        assert!(mapped.ambient_occlusion.enabled);
        assert_eq!(
            mapped.ambient_occlusion.quality,
            engine_render::AmbientOcclusionQuality::High
        );
        assert_eq!(mapped.exposure.mode, engine_render::ExposureMode::Auto);
        assert_eq!(mapped.exposure.ev, -0.5);
    }

    #[test]
    fn screen_shader_time_keeps_fraction_of_long_sessions() {
        // 3_600_000 имеет ulp 0.25 в f32: сначала переведённое в f32 время потеряло бы 0.1 с.
        let time = screen_shader_time(3_600_000.1);
        assert!((time - 0.1).abs() < 1e-4, "получено {time}");
        assert_eq!(screen_shader_time(3_600.0 + 2.5), 2.5);
    }
}

#[cfg(test)]
mod activity_tests {
    use super::*;
    use engine_scene::SceneDocument;

    const SPEAKER_SCENE: &str = "[[assets]]\npath = \"/audio/hum.wav\"\n\
        [[objects]]\nid = \"camera\"\n[objects.camera]\n\
        [[objects]]\nid = \"ball\"\n[objects.collider]\n\
        [objects.audio_source]\nclip = \"/audio/hum.wav\"\nplay_on_start = true\n";

    #[test]
    fn hiding_removes_body_and_sound_and_showing_recreates_body_and_restarts_sound() {
        let document = SceneDocument::parse("activity.rvescene", SPEAKER_SCENE).unwrap();
        let mut world = World::from_document(&document).unwrap();
        let ball = world.handle("ball").unwrap();
        let mut sync = PhysicsSceneSync::new(&world, Vec::new()).unwrap();
        let mut spawn = SpawnPhysics::default();
        let inserted = sync
            .insert_objects(&world, &[ball], &spawn.geometry, &mut spawn.shapes)
            .unwrap();
        assert_eq!(inserted.len(), 1);
        let body = sync.bindings()[0].body;

        world.set_active(ball, false).unwrap();
        let hidden = apply_activity_changes(&mut world, Some(&mut sync), &mut spawn);
        assert!(
            matches!(hidden.as_slice(), [AudioRequest::StopSource { object }] if *object == ball),
            "{hidden:?}"
        );
        assert_eq!(spawn.changes, [BodyChange::Remove { handle: body }]);
        assert!(sync.bindings().is_empty());

        spawn.changes.clear();
        world.set_active(ball, true).unwrap();
        let shown = apply_activity_changes(&mut world, Some(&mut sync), &mut spawn);
        assert!(
            matches!(shown.as_slice(), [AudioRequest::PlaySource { object, .. }] if *object == ball),
            "{shown:?}"
        );
        assert!(
            matches!(spawn.changes.as_slice(), [BodyChange::Insert { .. }]),
            "{:?}",
            spawn.changes
        );
        assert_eq!(sync.bindings().len(), 1);
    }

    #[test]
    fn destroying_an_inactive_object_removes_no_body_and_is_not_double_removed() {
        let document = SceneDocument::parse("activity.rvescene", SPEAKER_SCENE).unwrap();
        let mut world = World::from_document(&document).unwrap();
        let ball = world.handle("ball").unwrap();
        let mut sync = PhysicsSceneSync::new(&world, Vec::new()).unwrap();
        let mut spawn = SpawnPhysics::default();
        sync.insert_objects(&world, &[ball], &spawn.geometry, &mut spawn.shapes)
            .unwrap();
        let body = sync.bindings()[0].body;

        world.set_active(ball, false).unwrap();
        apply_activity_changes(&mut world, Some(&mut sync), &mut spawn);
        spawn.changes.clear();
        world.despawn_many(&[ball]).unwrap();
        let removed = sync.remove_objects(&[ball]);
        assert!(removed.is_empty(), "{removed:?}");
        assert!(spawn.changes.is_empty());
        assert_eq!(sync.object_for_body(body), Some(ball));
    }
}
