#[cfg(feature = "dev-console")]
use crate::console::ConsoleUiSlot;
#[cfg(feature = "dev-console")]
use crate::console::{CommandCatalogSnapshot, ConsoleUiFrame};
use crate::input::{InputAccumulator, lock_input};
use crate::module_host::{ModuleHost, panic_message};
use crate::profiling::{
    PhaseWindow, SOURCE_RENDER, SOURCE_RESOURCES, SOURCE_SCRIPTS, SOURCE_STARTUP,
    log_startup_profile, record_frame_profile, record_update_profile, render_groups, render_phase,
    script_phase, scripts_groups,
};
#[cfg(feature = "dev-console")]
use engine_console::CommandLine;
use engine_core::profile::ProfileBoard;
use engine_physics::{
    PhysicsClient, PhysicsInitialization, PhysicsReceive, PhysicsSceneSync, PhysicsStepResult,
};
use engine_render::{
    AntiAliasing as RenderAntiAliasing, DebugView, DirectionalLight, DrawCommand, EnvironmentId,
    EnvironmentSource, Frame, FrameCamera, FrameEnvironment, FrameOutcome, FrameProfile,
    FrameStatus, LocalLight, LocalLightShadow, LocalLightShadowMatrices, MaterialId, MeshId,
    ProceduralSky, Renderer, SHADOW_MAP_RESOLUTION, Tonemapper as RenderTonemapper, ViewSettings,
};
#[cfg(feature = "dev-console")]
use engine_render::{UiImage, UiPrimitive, UiVertex, ViewportId};
use engine_resources::ResourceHandle;
use engine_scene::camera::Camera;
use engine_scene::light::{
    cascade_view_projections, point_shadow_view_projections, spot_shadow_view_projection,
};
use engine_scene::{
    AntiAliasing, CameraSettings, EnvironmentState, LightState, LocalLightState, ObjectHandle,
    Tonemapper, World,
};
#[cfg(feature = "dev-console")]
use engine_script::CommandError;
use engine_script::{
    ScriptEvent, ScriptEventLevel, ScriptHost, ScriptInterruptHandle, ScriptLibrary,
    ScriptLoadOptions, ScriptTime,
};
#[cfg(feature = "dev-console")]
use engine_ui::render::UiBridge;
use flume::{Receiver, RecvTimeoutError, Selector, Sender, TryRecvError};
use glam::{Mat3, Vec3};
use std::collections::HashMap;
#[cfg(feature = "dev-console")]
use std::collections::VecDeque;
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
    pub camera: Camera,
    pub active_camera: ObjectHandle,
    pub view: CameraSettings,
    pub reset_history: bool,
    pub light: LightState,
    pub local_lights: Vec<LocalLight>,
    pub local_light_shadows: Vec<LocalLightShadow>,
    pub environment: Option<SceneEnvironmentSnapshot>,
    pub draws: Vec<DrawCommand>,
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

#[derive(Clone, Debug, Default)]
pub(crate) struct GpuSceneTable {
    pub by_object: HashMap<ObjectHandle, Vec<Vec<(MeshId, MaterialId)>>>,
    pub environments: HashMap<String, Arc<EnvironmentSource>>,
}

type FrameLighting = (
    DirectionalLight,
    Vec<[[f32; 4]; 4]>,
    Option<FrameEnvironment>,
);

#[derive(Debug)]
pub(crate) enum ScriptControl {
    Go(GpuSceneTable),
    Background(bool),
    #[cfg(feature = "dev-console")]
    Console(CommandLine),
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
    pub control_tx: Sender<ScriptControl>,
    pub control_rx: Receiver<ScriptControl>,
    pub frame_tx: Sender<Arc<FrameSnapshot>>,
    pub render_tx: Sender<RenderControl>,
    pub status: Arc<RuntimeStatus>,
    pub input: Arc<Mutex<InputAccumulator>>,
    pub physics_client: Option<PhysicsClient>,
    pub physics_initialization: Option<PhysicsInitialization>,
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
    pub control_tx: Sender<RenderControl>,
    pub control_rx: Receiver<RenderControl>,
    pub script_tx: Sender<ScriptControl>,
    pub interrupt: ScriptInterruptHandle,
    pub status: Arc<RuntimeStatus>,
    /// Источник статистики ресурсов для профайлера; `None` оставляет источник пустым.
    pub resources: Option<ResourceHandle>,
    pub stopped_tx: Sender<()>,
    pub exit: Arc<dyn ExitRequester>,
}

#[derive(Debug)]
pub(crate) enum ScriptStartupError {
    Load(String),
    Physics(String),
    Panic(String),
}

pub(crate) struct VulkanBackend {
    renderer: Renderer,
    debug_view: DebugView,
    current_environment: Option<(EnvironmentCacheKey, EnvironmentId)>,
    pipeline_cache_path: Option<std::path::PathBuf>,
    #[cfg(feature = "dev-console")]
    console_ui: Option<Arc<ConsoleUiSlot>>,
    #[cfg(feature = "dev-console")]
    ui_bridge: UiBridge,
    #[cfg(feature = "dev-console")]
    console_viewport: Option<(ViewportId, u32, u32)>,
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
            debug_view,
            current_environment: None,
            pipeline_cache_path,
            #[cfg(feature = "dev-console")]
            console_ui: None,
            #[cfg(feature = "dev-console")]
            ui_bridge: UiBridge::default(),
            #[cfg(feature = "dev-console")]
            console_viewport: None,
        }
    }

    #[cfg(feature = "dev-console")]
    pub fn with_console(mut self, console_ui: Arc<ConsoleUiSlot>) -> Self {
        self.console_ui = Some(console_ui);
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
        let (light, shadow_cascades, environment) = self.frame_lighting(snapshot, width, height)?;
        let frame = Frame {
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
                exposure_ev: snapshot.view.color.exposure_ev,
                tonemapper: match snapshot.view.color.tonemapper {
                    Tonemapper::Aces => RenderTonemapper::Aces,
                    Tonemapper::Neutral => RenderTonemapper::Neutral,
                    Tonemapper::None => RenderTonemapper::None,
                },
                anti_aliasing: match snapshot.view.render.anti_aliasing {
                    AntiAliasing::Fxaa => RenderAntiAliasing::Fxaa,
                    AntiAliasing::None => RenderAntiAliasing::None,
                    AntiAliasing::Taa => RenderAntiAliasing::Taa,
                    AntiAliasing::Fsr => RenderAntiAliasing::Fsr,
                    AntiAliasing::Dlss => RenderAntiAliasing::Dlss,
                },
            },
            reset_history: snapshot.reset_history,
        };
        self.renderer
            .render_frame_with_outcome(&frame)
            .map_err(|error| error.to_string())
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
        let scene = Frame {
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
                exposure_ev: snapshot.view.color.exposure_ev,
                tonemapper: match snapshot.view.color.tonemapper {
                    Tonemapper::Aces => RenderTonemapper::Aces,
                    Tonemapper::Neutral => RenderTonemapper::Neutral,
                    Tonemapper::None => RenderTonemapper::None,
                },
                anti_aliasing: match snapshot.view.render.anti_aliasing {
                    AntiAliasing::Fxaa => RenderAntiAliasing::Fxaa,
                    AntiAliasing::None => RenderAntiAliasing::None,
                    AntiAliasing::Taa => RenderAntiAliasing::Taa,
                    AntiAliasing::Fsr => RenderAntiAliasing::Fsr,
                    AntiAliasing::Dlss => RenderAntiAliasing::Dlss,
                },
            },
            reset_history: snapshot.reset_history,
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
        control_rx,
        frame_tx,
        render_tx,
        status,
        input,
        #[cfg(feature = "dev-console")]
        command_catalog,
        physics_client,
        physics_initialization,
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
    let mut scripts = match ScriptHost::load_with_options(
        &world,
        &scene_name,
        &sources,
        library.as_ref(),
        ScriptLoadOptions {
            interrupt: interrupt.clone(),
            call_time_limit: Some(SCRIPT_CALL_TIME_LIMIT),
            input: input_settings,
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
    if let Some(ready_tx) = ready_tx.take()
        && ready_tx.send(Ok(())).is_err()
    {
        return;
    }

    let mut background = false;
    let mut applied_background = false;
    #[cfg(feature = "dev-console")]
    let mut pending_console_commands = PendingConsoleCommands::default();
    let gpu_scene = loop {
        match control_rx.recv() {
            Ok(ScriptControl::Go(scene)) => break Some(scene),
            Ok(ScriptControl::Shutdown) | Err(_) => break None,
            Ok(ScriptControl::Background(value)) => {
                background = value;
                sync_input_background(&input, background, &mut applied_background);
            }
            #[cfg(feature = "dev-console")]
            Ok(ScriptControl::Console(command)) => pending_console_commands.push_back(command),
        }
    };
    let Some(gpu_scene) = gpu_scene else {
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
    let start_time = ScriptTime {
        elapsed: last_tick.as_secs_f64(),
        frame: frame_number,
    };
    scripts.start(&mut world, start_time);
    print_script_events(scripts.drain_events());
    #[cfg(feature = "dev-console")]
    let mut published_catalog_generation = u64::MAX;
    #[cfg(feature = "dev-console")]
    publish_command_catalog(
        &scripts,
        &command_catalog,
        &mut published_catalog_generation,
    );
    let interval = Duration::from_secs_f64(1.0 / background_tick_hz.max(1) as f64);
    let now = clock.now();
    let mut next_background_tick =
        now.saturating_add(if background { interval } else { Duration::ZERO });
    loop {
        timings.publish_if_due(Instant::now());
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
                    ScriptControl::Go(_) => false,
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
                    Ok(ScriptControl::Background(false)) | Ok(ScriptControl::Go(_)) => {}
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

        let now = clock.now();
        let dt = now.saturating_sub(last_tick).as_secs_f32();
        last_tick = now;
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
        let input_frame = lock_input(&input).take_frame();
        scripts.set_input(input_frame);
        let update_started = Instant::now();
        scripts.update(&mut world, dt, time);
        timings.record(
            script_phase::LOOP,
            script_phase::SCRIPTS_UPDATE,
            update_started.elapsed(),
        );
        record_update_profile(&mut timings, scripts.last_update_profile());
        print_script_events(scripts.drain_events());
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
                Ok(world_commands) => commands.extend(world_commands),
                Err(error) => {
                    status.record_failure(format!("ошибка синхронизации сцены с физикой: {error}"));
                    let _ = render_tx.send(RenderControl::Shutdown);
                    break;
                }
            }
            if let Err(error) = client.step(dt, commands) {
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
        let snapshot_started = Instant::now();
        let made = make_snapshot(scene_frame, &world, &gpu_scene);
        timings.record(
            script_phase::LOOP,
            script_phase::MAKE_SNAPSHOT,
            snapshot_started.elapsed(),
        );
        let snapshot = match made {
            Ok(snapshot) => Arc::new(snapshot),
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
        );
        timings.record(
            script_phase::LOOP,
            script_phase::SNAPSHOT_SEND_WAIT,
            send_started.elapsed(),
        );
        match sent {
            SendOutcome::Sent => {}
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

fn make_snapshot(
    scene: engine_scene::SceneFrame,
    world: &World,
    gpu_scene: &GpuSceneTable,
) -> Result<FrameSnapshot, String> {
    let mut draws = Vec::new();
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
        for (mesh, material) in meshes {
            draws.push(DrawCommand {
                mesh: *mesh,
                material: Some(*material),
                model,
                previous_model: None,
                lod_level,
                casts_shadow: true,
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
    Ok(FrameSnapshot {
        camera: scene.camera,
        active_camera: world.active_camera(),
        view: scene.view,
        reset_history: false,
        light: scene.light,
        local_lights,
        local_light_shadows,
        environment,
        draws,
        object_motion,
    })
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
            ScriptSelection::Control(Ok(ScriptControl::Go(_))) => {}
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
            Ok(ScriptControl::Go(_)) | Err(RecvTimeoutError::Timeout) => {}
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
        control_rx,
        script_tx,
        interrupt,
        status,
        resources,
        stopped_tx,
        exit,
        ..
    } = config;
    let mut profiler =
        PhaseWindow::new(SOURCE_RENDER, Arc::clone(&status.profile), render_groups())
            .with_resources(resources);
    let result = catch_unwind(AssertUnwindSafe(|| {
        render_loop(
            &mut *backend,
            width,
            height,
            frame_limit,
            &frame_rx,
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
}

#[allow(clippy::too_many_arguments)]
fn render_loop(
    backend: &mut dyn RenderBackend,
    mut width: u32,
    mut height: u32,
    frame_limit: Option<u64>,
    frame_rx: &Receiver<Arc<FrameSnapshot>>,
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

        let wait_started = Instant::now();
        let selection = Selector::new()
            .recv(frame_rx, RenderSelection::Frame)
            .recv(control_rx, RenderSelection::Control)
            .wait();
        profiler.record(
            render_phase::LOOP,
            render_phase::FRAME_WAIT,
            wait_started.elapsed(),
        );
        match selection {
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
                latest_snapshot = Some(snapshot);
                let Some(snapshot) = latest_snapshot.as_mut() else {
                    continue;
                };
                let Some(outcome) = render_snapshot(
                    backend,
                    Arc::make_mut(snapshot),
                    &mut motion_history,
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

#[allow(clippy::too_many_arguments)]
fn render_snapshot(
    backend: &mut dyn RenderBackend,
    snapshot: &mut FrameSnapshot,
    motion_history: &mut MotionHistory,
    paused: bool,
    width: u32,
    height: u32,
    #[cfg(feature = "dev-console")] ui_frame: &mut ConsoleUiFrame,
    profiler: &mut PhaseWindow,
) -> Result<Option<FrameOutcome>, String> {
    if paused {
        return Ok(None);
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

#[cfg(test)]
mod tests {
    #[cfg(feature = "dev-console")]
    use super::CommandCatalogSnapshot;
    #[cfg(feature = "dev-console")]
    use super::ConsoleUiFrame;
    #[cfg(feature = "dev-console")]
    use super::ConsoleUiSlot;
    #[cfg(feature = "dev-console")]
    use super::execute_console_commands;
    use super::{
        Clock, EnvironmentCacheKey, EnvironmentSnapshotSource, ExitRequester, FrameSnapshot,
        GpuSceneTable, MotionHistory, PhysicsWait, RenderBackend, RenderControl,
        RenderThreadConfig, RuntimeStatus, SCRIPTS_THREAD, ScriptControl, ScriptStartupError,
        ScriptThreadConfig, apply_physics_step, log_script_events_with, make_snapshot,
        procedural_environment_key, receive_physics_interruptibly, render_local_lights,
        render_loop, render_snapshot, render_thread, selected_level, send_snapshot,
        sync_input_background, wait_background_tick,
    };
    #[cfg(feature = "dev-console")]
    use super::{PROFILE_COMMAND, PROFILE_COMMAND_DESCRIPTION, publish_command_catalog};
    use crate::input::{InputAccumulator, lock_input};
    use crate::module_host::ModuleHost;
    use crate::pipeline::start_scripts;
    use crate::profiling::{PhaseWindow, SOURCE_RENDER, render_groups, render_phase};
    #[cfg(feature = "dev-console")]
    use engine_console::CommandLine;
    use engine_console::{ConsoleBuffer, ConsoleLevel, ConsoleLogger};
    use engine_core::profile::ProfileBoard;
    use engine_core::profile::ProfileValue;
    use engine_physics::{
        PhysicsClient, PhysicsCommand, PhysicsEvent, PhysicsInitialization, PhysicsReply,
        PhysicsRequest, PhysicsSceneDesc, PhysicsSceneSync, PhysicsServer, PhysicsStepResult,
        PhysicsWorld, Pose,
    };
    use engine_project::{InputAction, InputSettings};
    use engine_render::{DrawCommand, FrameOutcome, FrameStatus, LocalLight};
    use engine_render::{EnvironmentSource, ProceduralSky};
    use engine_scene::CameraSettings;
    use engine_scene::camera::Camera;
    use engine_scene::world::LightState;
    use engine_scene::{LocalLightState, ObjectHandle, SceneDocument, World};
    use engine_script::{
        Key, ScriptEvent, ScriptEventLevel, ScriptHost, ScriptInterruptHandle, ScriptTime,
    };
    use glam::Vec3;
    use rve_sdk::{Context, Params, Script};
    use std::collections::BTreeMap;
    use std::collections::HashMap;
    #[cfg(feature = "dev-console")]
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
                sources: BTreeMap::from([("input.lua".to_owned(), script.to_owned())]),
                input_settings: InputSettings {
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
                physics_client: None,
                physics_initialization: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
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
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();
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
    }

    impl RenderBackend for FakeBackend {
        fn resize(&mut self, _width: u32, _height: u32) {}

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
            reset_history: false,
            light: LightState::default(),
            local_lights: Vec::new(),
            local_light_shadows: Vec::new(),
            environment: None,
            draws: Vec::<DrawCommand>::new(),
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
            reset_history: false,
            light: LightState::default(),
            local_lights: Vec::new(),
            local_light_shadows: Vec::new(),
            environment: None,
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
        settings.color.exposure_ev = 1.0;
        settings.color.tonemapper = engine_scene::Tonemapper::Neutral;
        world
            .set_camera_settings(world.active_camera(), settings)
            .unwrap();
        let (frame, _) = world.build_frame();
        let snapshot = make_snapshot(frame, &world, &GpuSceneTable::default()).unwrap();
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
        assert!(make_snapshot(frame.clone(), &world, &GpuSceneTable::default(),).is_err());

        let source = Arc::new(EnvironmentSource::Equirect {
            width: 2,
            height: 1,
            rgb: vec![0.1; 6],
        });
        let scene = GpuSceneTable {
            environments: HashMap::from([("environment/sky.hdr".to_owned(), source)]),
            ..GpuSceneTable::default()
        };
        let snapshot = make_snapshot(frame, &world, &scene).unwrap();
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
        let snapshot = make_snapshot(frame, &world, &GpuSceneTable::default()).unwrap();
        assert_eq!(snapshot.view.color.exposure_ev, 2.5);
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
                sources,
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
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
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();

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
                input_settings: engine_project::InputSettings::default(),
                sources: BTreeMap::new(),
                scripts_library: None,
                script_module: None,
                interrupt: interrupt.clone(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 1,
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
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();

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
                        // Клиент может уже завершиться и уронить приёмник ответа: это не ошибка worker.
                        if physics_reply_tx
                            .send(PhysicsReply::Step(PhysicsStepResult {
                                sequence,
                                substeps: 0,
                                poses: Vec::new(),
                                events: Vec::<PhysicsEvent>::new(),
                                snapshot: Arc::clone(&snapshot),
                            }))
                            .is_err()
                        {
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
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                script_module: Some(script_module()),
                interrupt: ScriptInterruptHandle::new(),
                clock: clock.clone(),
                background_tick_hz: 1,
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
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();

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
        // Worker мог выйти раньше из-за закрытого приёмника ответа; тогда останавливать уже нечего.
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
                input_settings: engine_project::InputSettings::default(),
                sources: BTreeMap::from([("probe.lua".to_owned(), script.to_owned())]),
                scripts_library: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
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
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        script_thread.ready.recv().unwrap().unwrap();
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();
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
                input_settings: engine_project::InputSettings::default(),
                sources: BTreeMap::from([("move.lua".to_owned(), script.to_owned())]),
                scripts_library: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
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
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        script_thread.ready.recv().unwrap().unwrap();
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();

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
                input_settings: engine_project::InputSettings::default(),
                sources: BTreeMap::new(),
                scripts_library: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
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
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();
        script_thread.ready.recv().unwrap().unwrap();
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();
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
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                script_module: Some(input_probe_module()),
                interrupt: ScriptInterruptHandle::new(),
                clock: clock.clone(),
                background_tick_hz: 1,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: Arc::clone(&input),
                physics_client: None,
                physics_initialization: None,
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
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();
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
            control_tx: control_tx.clone(),
            control_rx,
            script_tx,
            interrupt: interrupt.clone(),
            status: Arc::clone(&status),
            resources: None,
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
                control_tx: control_tx.clone(),
                control_rx,
                script_tx,
                interrupt: ScriptInterruptHandle::new(),
                status: Arc::clone(&status),
                resources: None,
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
                sources: BTreeMap::from([(
                    "scripts/bad.lua".to_owned(),
                    "this is not valid Lua !!!".to_owned(),
                )]),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                script_module: None,
                interrupt,
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                control_tx: script_tx,
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
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
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: Some(library),
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                control_tx: script_tx,
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
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
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                script_module: Some(script_module()),
                interrupt: interrupt.clone(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();

        script_thread.ready.recv().unwrap().unwrap();
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();
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
                sources: BTreeMap::new(),
                input_settings: engine_project::InputSettings::default(),
                scripts_library: None,
                script_module: None,
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(PanicClock),
                background_tick_hz: 30,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::clone(&status),
                input: test_input(),
                physics_client: None,
                physics_initialization: None,
                #[cfg(feature = "dev-console")]
                command_catalog: test_command_catalog(),
            },
        )
        .unwrap();

        script_thread.ready.recv().unwrap().unwrap();
        script_tx
            .send(ScriptControl::Go(GpuSceneTable::default()))
            .unwrap();
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
}
