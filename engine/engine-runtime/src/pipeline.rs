#[cfg(feature = "dev-console")]
use crate::console::ConsoleUiSlot;
#[cfg(feature = "dev-console")]
use crate::console::{CommandCatalogSnapshot, ConsoleUiFrame};
use crate::cursor::CursorControl;
use crate::game_ui::{GameUi, PreparedUi, UiInputBridge, UiRegistry, UiTick, empty_paint};
use crate::input::{InputAccumulator, lock_input};
use crate::module_host::{ModuleHost, panic_message};
use crate::net::{self, NetworkSession, ScriptNetwork};
use crate::prefabs::{
    LoadedPrefab, LoaderRequest, MaterialLevels, MaterialReply, MaterialRequest,
    PREFAB_UPLOAD_BUDGET, ParticleRequest, PrefabCpu, PrefabGpu, PrefabLoads, PrefabReady,
    PrefabReply, PrefabToken, RenderResourceRequest,
};
use crate::profiling::{
    PhaseWindow, SOURCE_RENDER, SOURCE_RESOURCES, SOURCE_SCRIPTS, SOURCE_STARTUP,
    log_startup_profile, record_frame_profile, record_update_profile, render_groups, render_phase,
    script_phase, scripts_groups,
};
use crate::scene::{
    MaterialAssetView, MaterialUploadCaches, ObjectModel, ParticleSource, PreparedMaterialAssets,
    PreparedPrefabs, PreparedRenderAssets, SceneParticles, particle_emitter_desc,
    upload_object_levels, upload_scene,
};
use crate::scene_switch::{
    SceneCommit, SceneGpuIds, SceneLoader, SceneSwitch, SceneUpload, SceneUploadReply,
};
use engine_animation::{AnimationBridge, AnimationEvent, PaletteRemap};
use engine_assets::{Model, ModelAnimation, TextureData};
#[cfg(feature = "dev-console")]
use engine_console::CommandLine;
use engine_core::profile::ProfileBoard;
use engine_gui::paint::UiPaint;
use engine_network::SpawnedRoot;
use engine_physics::{
    BodyChange, BodyHandle, ModelGeometry, PhysicsClient, PhysicsCommand, PhysicsInitialization,
    PhysicsReceive, PhysicsSceneSync, PhysicsSceneSyncError, PhysicsStepResult, ShapeCache,
};
use engine_render::UiTextureId;
use engine_render::{
    AntiAliasing as RenderAntiAliasing, DebugView, DirectionalLight, DrawCommand, EnvironmentId,
    EnvironmentSource, Exposure, Frame, FrameCamera, FrameEnvironment, FrameOutcome, FrameProfile,
    FrameStatus, LatencyHandle, LatencyMarker, LocalLight, LocalLightShadow,
    LocalLightShadowMatrices, MaterialId, MeshId, ParticleEmitterDesc, ParticleEmitterFrame,
    ParticleEmitterId, PostProcess, ProceduralSky, RenderError, Renderer, SHADOW_MAP_RESOLUTION,
    TextureColorSpace, TextureId, Tonemapper as RenderTonemapper, ViewSettings,
};
use engine_render::{ColorLutId, SkinBinding, SkinId};
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
    LodSelector, ObjectHandle, PostProcessSettings, PrefabTemplate, Tonemapper, VoiceId, World,
    activity_audio_requests,
};
#[cfg(feature = "dev-console")]
use engine_script::CommandError;
use engine_script::{
    PrefabRequest, ScriptCursor, ScriptEvent, ScriptEventLevel, ScriptFrameTime, ScriptHost,
    ScriptInterruptHandle, ScriptLibrary, ScriptLoadOptions, ScriptTime, SpawnedObject,
    WorldChanges,
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

mod backend;
mod changes;
mod materials;
mod particles;
mod physics;
mod prefab_sync;
mod render_thread;
mod script_events;
mod scripts;
mod snapshot;

pub(crate) use backend::*;
pub(crate) use changes::*;
pub(crate) use materials::*;
pub(crate) use particles::*;
pub(crate) use physics::*;
pub(crate) use prefab_sync::*;
pub(crate) use render_thread::*;
pub(crate) use script_events::*;
pub(crate) use scripts::*;
pub(crate) use snapshot::*;

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
    /// LUT цветокоррекции активной камеры; `None` без LUT или если LUT не загрузилась.
    pub color_lut: Option<ColorLutId>,
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
    /// Игровой UI кадра: draw list и атлас, как их выдал `engine-gui` на этом тике.
    pub ui: UiPaint,
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

/// Skinning одного уровня LOD: экземпляр на каждую часть меша и перевод палитры уровня 0 на
/// скелет уровня. Пустой список — уровень рисуется без скиннинга.
#[derive(Clone, Debug, Default)]
pub(crate) struct SkinLevel {
    pub instances: Vec<SkinId>,
    /// `None` у уровня 0: его палитра — палитра моста.
    pub remap: Option<Arc<PaletteRemap>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GpuSceneTable {
    pub by_object: HashMap<ObjectHandle, GpuLevels>,
    /// CPU material sources for scene objects and objects spawned from prefab templates.
    pub material_sources: HashMap<ObjectHandle, ObjectModel>,
    /// CPU material sources by object index for fast and loaded heavy prefabs.
    pub material_templates: HashMap<String, Vec<Option<ObjectModel>>>,
    /// Current heavy load generation by path and by spawned object.
    pub prefab_tokens: HashMap<String, PrefabToken>,
    pub material_tokens: HashMap<ObjectHandle, PrefabToken>,
    /// Уровни шаблонов быстрых префабов по пути. Элемент `i` относится к объекту шаблона `i`.
    pub prefabs: HashMap<String, Vec<GpuLevels>>,
    /// Эмиттеры частиц объектов сцены; создаются при загрузке и передаются render thread.
    pub particles: HashMap<ObjectHandle, ParticleEmitterId>,
    /// Описания эмиттеров быстрых префабов по пути. Элемент `i` относится к объекту шаблона `i`.
    pub particle_templates: HashMap<String, Vec<Option<ParticleEmitterDesc>>>,
    /// Scene cache owners for particle textures retained by active scene objects.
    pub particle_texture_owners: HashMap<ObjectHandle, u64>,
    /// Scene cache owners for particle textures retained by resident fast-prefab templates.
    pub particle_template_owners: HashMap<String, u64>,
    /// Описания эмиттеров объектов загруженной при замене сцены: эмиттер создаёт render thread
    /// по заявке `ParticleRequest::Create` с финальным хендлом объекта.
    pub particle_descs: HashMap<ObjectHandle, ParticleEmitterDesc>,
    pub environments: HashMap<String, Arc<EnvironmentSource>>,
    /// Скелеты и анимации моделей для моста анимации; вершины сюда не входят.
    pub animation_models: HashMap<String, Arc<Model>>,
    /// Клипы `animator.clips` по виртуальному пути файла.
    pub animation_clips: HashMap<String, Arc<Vec<ModelAnimation>>>,
    /// Экземпляры skinning по объекту: элемент `i` относится к уровню LOD `i`.
    pub skin_instances: HashMap<ObjectHandle, Vec<SkinLevel>>,
    /// Число костей модели объекта с экземплярами skinning; палитра моста должна иметь ту же длину.
    pub joint_counts: HashMap<ObjectHandle, u32>,
    /// UI-текстуры картинок сцены по пути; создаются при загрузке GPU-ресурсов.
    pub ui_images: HashMap<String, UiTextureId>,
    /// Поколения сцен, владеющие UI-картинками в `VulkanBackend::game_ui`.
    pub ui_scene_owners: HashSet<u64>,
    /// Экранные шейдеры камер по пути `.glsl`; создаются при загрузке сцены и освобождаются с ней.
    pub screen_shaders: HashMap<String, engine_render::ScreenShaderId>,
    /// LUT цветокоррекции камер по каноническому пути `.cube`; живут как экранные шейдеры.
    pub color_luts: HashMap<String, ColorLutId>,
}

impl GpuSceneTable {
    /// Все экземпляры skinning сцены; рендер освобождает их при завершении.
    pub(crate) fn skin_instance_ids(&self) -> Vec<SkinId> {
        self.skin_instances
            .values()
            .flatten()
            .flat_map(|level| level.instances.iter().copied())
            .collect()
    }

    /// Экранные шейдеры сцены; рендер освобождает их при выгрузке.
    pub(crate) fn screen_shader_ids(&self) -> Vec<engine_render::ScreenShaderId> {
        self.screen_shaders.values().copied().collect()
    }

    /// LUT цветокоррекции сцены; рендер освобождает их при выгрузке.
    pub(crate) fn color_lut_ids(&self) -> Vec<ColorLutId> {
        self.color_luts.values().copied().collect()
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
    /// Ответ render thread на загрузку GPU-ресурсов новой сцены.
    Scene(Box<SceneUploadReply>),
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
    /// Ответ render thread на загрузку GPU-ресурсов новой сцены.
    Scene(Box<SceneUploadReply>),
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

    /// Загружает GPU-ресурсы новой сцены. Без GPU ответ пустой.
    fn submit_scene(&mut self, upload: SceneUpload) -> SceneUploadReply {
        SceneUploadReply {
            generation: upload.generation,
            result: Ok(Box::default()),
        }
    }

    /// Уничтожает GPU-ресурсы сцены, которые уже не используются снимками.
    fn release_scene_resources(&mut self, _ids: &SceneGpuIds) {}
}

pub(crate) trait ExitRequester: Send + Sync {
    fn request_exit(&self);
}

pub(crate) struct ScriptThreadConfig {
    pub scene_name: String,
    pub save_path: Option<PathBuf>,
    /// Включённые сцены проекта: только они принимаются `rve.scene.load`.
    pub enabled_scenes: Vec<String>,
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
    /// `.rveui` сцены и быстрых префабов с ассетами: регистрируются до `start`.
    pub ui: PreparedUi,
    /// Фокус InputField: scripts thread пишет, главный поток включает по нему IME.
    pub ui_text_focus: Arc<AtomicBool>,
    /// Заявки загрузчика тяжёлых префабов.
    pub prefab_loader: Sender<LoaderRequest>,
    /// Заявки render thread: выгрузка тяжёлых префабов.
    pub render_resources: Sender<RenderResourceRequest>,
    /// Загрузчик сцен для `rve.scene.load`; `None` — замена сцены отключена.
    pub scene_loader: Option<SceneLoader>,
    /// Есть ли render thread с GPU: загрузку сцены тогда выполняет он, иначе scripts thread.
    pub gpu_render: bool,
    /// Сетевой поток и режим моста; `None` — offline, мост не подключается.
    pub network: Option<NetworkSession>,
    /// Число тиков, после которого цикл останавливается сам (`rve-server --frames`); `None` — без предела.
    pub frame_limit: Option<u64>,
    #[cfg(feature = "dev-console")]
    pub command_catalog: Arc<Mutex<CommandCatalogSnapshot>>,
    /// Окно, скриншоты и системные диалоги для скриптов.
    pub platform: crate::platform::ScriptPlatform,
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

#[cfg(test)]
mod activity_tests;
#[cfg(test)]
mod particle_command_tests;
#[cfg(test)]
mod particle_emitter_tests;
#[cfg(test)]
mod particle_step_tests;
#[cfg(test)]
mod post_process_mapping_tests;
#[cfg(test)]
mod screen_shader_selection_tests;
#[cfg(test)]
mod tests;
