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
    GpuSceneTable, MotionHistory, PendingRelease, PhysicsWait, ReleaseTarget, RenderBackend,
    RenderControl, RenderThreadConfig, RuntimeStatus, SCRIPTS_THREAD, ScriptControl,
    ScriptStartupError, ScriptThreadConfig, apply_physics_step, drop_mismatched_skins,
    finish_releases, level_palette, log_script_events_with, make_snapshot,
    procedural_environment_key, receive_physics_interruptibly, render_local_lights, render_loop,
    render_snapshot, render_thread, selected_level, send_snapshot, sync_input_background,
    wait_background_tick,
};
use super::{FrameLatency, publish_script_cursor, set_initial_cursor};
use super::{MaterialSync, ScriptReply};
#[cfg(feature = "dev-console")]
use super::{PROFILE_COMMAND, PROFILE_COMMAND_DESCRIPTION, publish_command_catalog};
use super::{SpawnPhysics, apply_world_changes, drain_prefab_replies};
use super::{
    attach_prefab_levels, attach_prefab_material_sources, detach_object_render_data, detach_objects,
};
use crate::cursor::{CursorControl, RecordingCursorSink};
use crate::input::{InputAccumulator, lock_input};
use crate::module_host::ModuleHost;
use crate::pipeline::start_scripts;
use crate::prefabs::ParticleRequest;
use crate::prefabs::{
    LoadedPrefab, LoaderRequest, PrefabCpu, PrefabLoads, PrefabReady, PrefabReply, PrefabToken,
    RenderResourceRequest,
};
use crate::prefabs::{MaterialLevels, MaterialReply, MaterialRequest};
use crate::profiling::{PhaseWindow, SOURCE_RENDER, render_groups, render_phase};
use crate::scene::{ObjectModel, PreparedMaterialAssets, PreparedPrefabs};
use crate::scene_switch::{SceneLoader, SceneUploadReply};
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
use engine_render::{
    DrawCommand, FrameOutcome, FrameStatus, LatencyMarker, LocalLight, MaterialId, MeshId,
};
use engine_render::{EnvironmentSource, ProceduralSky};
use engine_scene::CameraSettings;
use engine_scene::camera::Camera;
use engine_scene::world::LightState;
use engine_scene::world::MeshMaterialParams;
use engine_scene::{LocalLightState, ObjectHandle, SceneDocument, SpawnOptions, World};
use engine_script::ScriptLoadOptions;
use engine_script::{
    Key, ScriptEvent, ScriptEventLevel, ScriptHost, ScriptInterruptHandle, ScriptTime,
    SpawnedObject,
};
use engine_script::{PrefabCatalog, PrefabKind, PrefabSpec};
use engine_window::CursorState;
use flume::Receiver;
use flume::Sender;
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "input.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
    let mut snapshot = motion_snapshot(camera_a, &[(first, first_model), (second, second_model)]);

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

    let select = |eye: Vec3| {
        let mut lod = engine_scene::LodSelector::default();
        selected_level(
            &mut lod,
            &world,
            object,
            eye,
            engine_scene::LOD_REFERENCE_FOV,
            &level_meshes,
        )
    };
    assert_eq!(select(Vec3::ZERO), Some((1, &level_meshes[1])));
    assert_eq!(
        select(Vec3::new(0.0, 0.0, -4.5)),
        Some((0, &level_meshes[0]))
    );
    assert_eq!(select(Vec3::new(0.0, 0.0, 3.0)), None);
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
    released_materials: Option<mpsc::Sender<Vec<MaterialId>>>,
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

    fn release_materials(&mut self, materials: &[MaterialId]) {
        if let Some(released) = &self.released_materials {
            let _ = released.send(materials.to_vec());
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
static QUIT_UPDATE_CALLS: AtomicUsize = AtomicUsize::new(0);
static QUIT_DESTROY_CALLS: AtomicUsize = AtomicUsize::new(0);

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

struct QuitProbe;

impl Script for QuitProbe {
    fn on_update(&mut self, ctx: &mut Context<'_>, _dt: f32) {
        assert!(ctx.quit());
        QUIT_UPDATE_CALLS.fetch_add(1, Ordering::SeqCst);
    }

    fn on_destroy(&mut self, _ctx: &mut Context<'_>) {
        QUIT_DESTROY_CALLS.fetch_add(1, Ordering::SeqCst);
    }
}

fn quit_probe_factory(_: &Params) -> Result<Box<dyn Script>, String> {
    Ok(Box::new(QuitProbe))
}

fn script_module() -> &'static rve_sdk::abi::RveModule {
    rve_sdk::script_module! {
        "DestroyProbe" => destroy_probe_factory,
        "TickProbe" => tick_probe_factory,
        "QuitProbe" => quit_probe_factory,
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
        color_lut: None,
        ui: crate::game_ui::empty_paint(),
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
        color_lut: None,
        ui: crate::game_ui::empty_paint(),
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
        &mut engine_scene::LodSelector::default(),
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
            &AnimationBridge::default(),
            &mut engine_scene::LodSelector::default(),
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
    let snapshot = make_snapshot(
        frame,
        &world,
        &scene,
        &AnimationBridge::default(),
        &mut engine_scene::LodSelector::default(),
    )
    .unwrap();
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
        &mut engine_scene::LodSelector::default(),
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "scene.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
    frame_tx
        .send(snapshot_with_particle_commands(Vec::new()))
        .unwrap();
    control_tx.send(RenderControl::Occluded(false)).unwrap();
    assert!(matches!(
        script_rx.recv().unwrap(),
        ScriptControl::Background(false)
    ));
    frame_tx
        .send(snapshot_with_particle_commands(Vec::new()))
        .unwrap();
    control_tx.send(RenderControl::Shutdown).unwrap();
    assert!(worker.join().unwrap().is_ok());
    assert_eq!(
        applied_rx.try_iter().collect::<Vec<_>>(),
        vec![vec![command]]
    );
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
        .send(RenderControl::DebugView(
            engine_render::DebugView::Wireframe,
        ))
        .unwrap();
    control_tx.send(RenderControl::Shutdown).unwrap();
    assert!(worker.join().unwrap().is_ok());
    assert!(
        applied_rx.try_iter().next().is_none(),
        "перерисовка не применяет команды повторно"
    );
}

const MATERIAL_SCENE: &str = "[[assets]]\npath = \"/models/crate.fbx\"\n[[assets]]\npath = \"/materials/gold.rvemat\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"crate\"\n[objects.mesh_renderer]\nmodel = \"/models/crate.fbx\"\n";

const MATERIAL_LOD_SCENE: &str = "[[assets]]\npath = \"/models/crate.fbx\"\n[[assets]]\npath = \"/models/crate_lod.fbx\"\n[[assets]]\npath = \"/materials/gold.rvemat\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"crate\"\n[objects.mesh_renderer]\nmodel = \"/models/crate.fbx\"\n[objects.lod_group]\n[[objects.lod_group.levels]]\nmodel = \"/models/crate_lod.fbx\"\ndistance = 10.0\n";

const MATERIAL_PREFAB: &str = "[[assets]]\npath = \"/models/crate.fbx\"\n[[assets]]\npath = \"/materials/gold.rvemat\"\n[[objects]]\nid = \"crate\"\n[objects.mesh_renderer]\nmodel = \"/models/crate.fbx\"\n";

fn material_test_world() -> (World, ObjectHandle) {
    let world =
        World::from_document(&SceneDocument::parse("materials.rvescene", MATERIAL_SCENE).unwrap())
            .unwrap();
    let crate_handle = world.handle("crate").unwrap();
    (world, crate_handle)
}

fn next_material_request(requests: &Receiver<RenderResourceRequest>) -> Box<MaterialRequest> {
    match requests.try_recv() {
        Ok(RenderResourceRequest::Material(request)) => request,
        _ => panic!("ожидалась заявка материала"),
    }
}

fn material_source() -> ObjectModel {
    ObjectModel {
        levels: vec![("models/crate.fbx".to_owned(), None)],
        material: None,
        material_slots: vec![None, None],
        animated: false,
    }
}

fn fake_mesh_id(value: usize) -> MeshId {
    // SAFETY: MeshId is a single usize wrapper; every usize is a valid test-only id.
    unsafe { std::mem::transmute(value) }
}

fn fake_material_id(value: usize) -> MaterialId {
    // SAFETY: MaterialId is a single usize wrapper; every usize is a valid test-only id.
    unsafe { std::mem::transmute(value) }
}

fn material_levels(seed: usize) -> Vec<Vec<(MeshId, MaterialId)>> {
    vec![vec![
        (fake_mesh_id(seed), fake_material_id(seed + 1)),
        (fake_mesh_id(seed + 2), fake_material_id(seed + 3)),
    ]]
}

fn snapshot_materials(
    world: &mut World,
    gpu_scene: &GpuSceneTable,
    object: ObjectHandle,
) -> Vec<MaterialId> {
    let (frame, warnings) = world.build_frame();
    assert!(warnings.is_empty(), "{warnings:?}");
    let snapshot = make_snapshot(
        frame,
        world,
        gpu_scene,
        &AnimationBridge::default(),
        &mut engine_scene::LodSelector::default(),
    )
    .unwrap();
    let range = snapshot
        .object_motion
        .get(&object)
        .expect("object has snapshot draws")
        .draw_range
        .clone();
    snapshot.draws[range]
        .iter()
        .map(|draw| draw.material.unwrap())
        .collect()
}

#[test]
fn material_changes_request_one_upload_per_object_and_a_reply_swaps_its_levels() {
    let (mut world, crate_handle) = material_test_world();
    let mut gpu_scene = GpuSceneTable::default();
    gpu_scene
        .by_object
        .insert(crate_handle, material_levels(10));
    gpu_scene
        .material_sources
        .insert(crate_handle, material_source());
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
    assert_eq!(first.levels, material_levels(10));
    assert_eq!(
        first.model.material.as_ref().and_then(|m| m.roughness),
        Some(0.5)
    );
    assert_eq!(first.model.material_slots, vec![None, None]);

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

    // Устаревший ответ не меняет уровни; его материалы освобождаются сразу, затем уходит
    // заявка с последней ревизией.
    sync.accept(MaterialReply {
        object: crate_handle,
        revision: first.revision,
        result: Ok(MaterialLevels {
            levels: material_levels(20),
            created: vec![fake_material_id(21), fake_material_id(23)],
        }),
    });
    sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 5);
    assert_eq!(
        snapshot_materials(&mut world, &gpu_scene, crate_handle),
        [fake_material_id(11), fake_material_id(13)],
        "stale reply must not change the next frame snapshot"
    );
    match resources_rx.try_recv().unwrap() {
        RenderResourceRequest::ReleaseMaterials {
            materials,
            last_seq,
        } => {
            assert_eq!(materials, [fake_material_id(21), fake_material_id(23)]);
            assert_eq!(last_seq, 0, "stale materials never entered a snapshot");
        }
        _ => panic!("stale reply materials must be released immediately"),
    }
    sync.request_changes(&mut world, &gpu_scene, &resources_tx);
    let second = next_material_request(&resources_rx);
    assert_eq!(second.revision, 2);
    assert_eq!(second.levels, material_levels(10));
    let material = second
        .model
        .material
        .as_ref()
        .expect("переопределения переданы");
    assert_eq!(material.roughness, Some(0.5), "прежние поля сохраняются");
    assert_eq!(material.base_color, Some([1.0, 0.0, 0.0]));

    sync.accept(MaterialReply {
        object: crate_handle,
        revision: second.revision,
        result: Ok(MaterialLevels {
            levels: material_levels(30),
            created: vec![fake_material_id(31), fake_material_id(33)],
        }),
    });
    sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 7);
    assert_eq!(
        snapshot_materials(&mut world, &gpu_scene, crate_handle),
        [fake_material_id(31), fake_material_id(33)]
    );
    assert!(
        resources_rx.try_recv().is_err(),
        "the accepted second reply has no previous owned materials"
    );
}

#[test]
fn set_material_path_reaches_the_request_and_a_refused_reply_keeps_the_levels() {
    let (mut world, crate_handle) = material_test_world();
    let mut gpu_scene = GpuSceneTable::default();
    gpu_scene
        .by_object
        .insert(crate_handle, material_levels(40));
    gpu_scene
        .material_sources
        .insert(crate_handle, material_source());
    let (resources_tx, resources_rx) = flume::unbounded();
    let mut sync = MaterialSync::default();
    let gold = engine_files::VirtualPath::parse("/materials/gold.rvemat").unwrap();
    world.set_mesh_material_path(crate_handle, gold).unwrap();

    sync.request_changes(&mut world, &gpu_scene, &resources_tx);
    let request = next_material_request(&resources_rx);
    assert_eq!(
        request.model.material_slots,
        vec![
            Some("materials/gold.rvemat".to_owned()),
            Some("materials/gold.rvemat".to_owned())
        ]
    );
    sync.accept(MaterialReply {
        object: crate_handle,
        revision: request.revision,
        result: Err("материал не загрузился".to_owned()),
    });
    sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 3);
    assert_eq!(
        snapshot_materials(&mut world, &gpu_scene, crate_handle),
        [fake_material_id(41), fake_material_id(43)],
        "отказ не меняет уровни"
    );
    sync.request_changes(&mut world, &gpu_scene, &resources_tx);
    assert!(
        resources_rx.try_recv().is_err(),
        "ревизия не менялась: повторной заявки нет"
    );
}

#[test]
fn set_material_path_covers_parts_from_every_lod_level() {
    let document = SceneDocument::parse("materials.rvescene", MATERIAL_LOD_SCENE).unwrap();
    let mut world = World::from_document(&document).unwrap();
    let crate_handle = world.handle("crate").unwrap();
    let first_level = material_levels(170).remove(0);
    let second_level = vec![
        (fake_mesh_id(180), fake_material_id(181)),
        (fake_mesh_id(182), fake_material_id(183)),
        (fake_mesh_id(184), fake_material_id(185)),
    ];
    let mut gpu_scene = GpuSceneTable::default();
    gpu_scene
        .by_object
        .insert(crate_handle, vec![first_level, second_level]);
    gpu_scene.material_sources.insert(
        crate_handle,
        ObjectModel {
            levels: vec![
                ("models/crate.fbx".to_owned(), None),
                ("models/crate_lod.fbx".to_owned(), None),
            ],
            material: None,
            material_slots: vec![None; 2],
            animated: false,
        },
    );
    world
        .set_mesh_material_path(
            crate_handle,
            engine_files::VirtualPath::parse("/materials/gold.rvemat").unwrap(),
        )
        .unwrap();
    let (resources_tx, resources_rx) = flume::unbounded();
    MaterialSync::default().request_changes(&mut world, &gpu_scene, &resources_tx);

    let request = next_material_request(&resources_rx);
    assert_eq!(
        request.model.material_slots,
        vec![Some("materials/gold.rvemat".to_owned()); 3]
    );
}

#[test]
fn spawned_fast_and_heavy_prefabs_send_material_sources_and_swap_snapshot_levels() {
    let world_document = SceneDocument::parse(
            "materials.rvescene",
            "[[assets]]\npath = \"/materials/scene.rvemat\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n",
        )
        .unwrap();
    let mut world = World::from_document(&world_document).unwrap();
    let fast_path = "prefabs/fast.rveprefab";
    let heavy_path = "prefabs/heavy.rveprefab";
    let fast_template = prefab_template(fast_path, MATERIAL_PREFAB);
    let heavy_template = prefab_template(heavy_path, MATERIAL_PREFAB);
    let fast_handles = world
        .spawn(
            vec![world.reserve_object()],
            &fast_template,
            SpawnOptions::default(),
        )
        .unwrap();
    let heavy_handles = world
        .spawn(
            vec![world.reserve_object()],
            &heavy_template,
            SpawnOptions::default(),
        )
        .unwrap();
    let fast = SpawnedObject {
        handle: fast_handles[0],
        objects: fast_handles,
        prefab: fast_path.to_owned(),
        template_index: 0,
        initial_velocity: None,
    };
    let heavy = SpawnedObject {
        handle: heavy_handles[0],
        objects: heavy_handles,
        prefab: heavy_path.to_owned(),
        template_index: 0,
        initial_velocity: None,
    };
    let token = PrefabToken {
        path: heavy_path.to_owned(),
        load_id: 4,
    };
    let mut gpu_scene = GpuSceneTable::default();
    gpu_scene
        .prefabs
        .insert(fast_path.to_owned(), vec![material_levels(50)]);
    gpu_scene
        .prefabs
        .insert(heavy_path.to_owned(), vec![material_levels(60)]);
    gpu_scene
        .material_templates
        .insert(fast_path.to_owned(), vec![Some(material_source())]);
    gpu_scene
        .material_templates
        .insert(heavy_path.to_owned(), vec![Some(material_source())]);
    gpu_scene
        .prefab_tokens
        .insert(heavy_path.to_owned(), token.clone());
    attach_prefab_levels(
        &mut gpu_scene.by_object,
        &gpu_scene.prefabs[fast_path],
        &fast,
    );
    attach_prefab_material_sources(&mut gpu_scene, &fast);
    attach_prefab_levels(
        &mut gpu_scene.by_object,
        &gpu_scene.prefabs[heavy_path],
        &heavy,
    );
    attach_prefab_material_sources(&mut gpu_scene, &heavy);

    let gold = engine_files::VirtualPath::parse("/materials/gold.rvemat").unwrap();
    world
        .set_mesh_material_path(fast.handle, gold.clone())
        .unwrap();
    let scene_material = engine_files::VirtualPath::parse("/materials/scene.rvemat").unwrap();
    world
        .set_mesh_material_path(heavy.handle, scene_material)
        .unwrap();
    let (resources_tx, resources_rx) = flume::unbounded();
    let mut sync = MaterialSync::default();
    sync.request_changes(&mut world, &gpu_scene, &resources_tx);
    let requests = [
        next_material_request(&resources_rx),
        next_material_request(&resources_rx),
    ];
    let fast_request = requests
        .iter()
        .find(|request| request.object == fast.handle)
        .unwrap();
    let heavy_request = requests
        .iter()
        .find(|request| request.object == heavy.handle)
        .unwrap();
    assert!(fast_request.prefab_token.is_none());
    assert_eq!(heavy_request.prefab_token.as_ref(), Some(&token));
    assert!(
        fast_request
            .model
            .material_slots
            .iter()
            .all(Option::is_some)
    );
    assert!(
        heavy_request
            .model
            .material_slots
            .iter()
            .all(Option::is_some)
    );
    assert_eq!(
        fast_request.model.material_slots[0].as_deref(),
        Some("materials/gold.rvemat")
    );
    assert_eq!(
        heavy_request.model.material_slots[0].as_deref(),
        Some("materials/scene.rvemat")
    );

    for (request, seed) in [(fast_request, 80), (heavy_request, 100)] {
        sync.accept(MaterialReply {
            object: request.object,
            revision: request.revision,
            result: Ok(MaterialLevels {
                levels: material_levels(seed),
                created: vec![fake_material_id(seed + 1), fake_material_id(seed + 3)],
            }),
        });
    }
    sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 3);
    assert_eq!(
        snapshot_materials(&mut world, &gpu_scene, fast.handle),
        [fake_material_id(81), fake_material_id(83)]
    );
    assert_eq!(
        snapshot_materials(&mut world, &gpu_scene, heavy.handle),
        [fake_material_id(101), fake_material_id(103)]
    );
}

#[test]
fn deleted_prefab_object_releases_an_outdated_material_reply_immediately() {
    let world_document = SceneDocument::parse(
        "materials.rvescene",
        "[[objects]]\nid = \"camera\"\n[objects.camera]\n",
    )
    .unwrap();
    let mut world = World::from_document(&world_document).unwrap();
    let prefab_path = "prefabs/heavy.rveprefab";
    let template = prefab_template(prefab_path, MATERIAL_PREFAB);
    let handles = world
        .spawn(
            vec![world.reserve_object()],
            &template,
            SpawnOptions::default(),
        )
        .unwrap();
    let spawned = SpawnedObject {
        handle: handles[0],
        objects: handles.clone(),
        prefab: prefab_path.to_owned(),
        template_index: 0,
        initial_velocity: None,
    };
    let token = PrefabToken {
        path: prefab_path.to_owned(),
        load_id: 8,
    };
    let mut gpu_scene = GpuSceneTable::default();
    gpu_scene
        .prefabs
        .insert(prefab_path.to_owned(), vec![material_levels(120)]);
    gpu_scene
        .material_templates
        .insert(prefab_path.to_owned(), vec![Some(material_source())]);
    gpu_scene
        .prefab_tokens
        .insert(prefab_path.to_owned(), token);
    attach_prefab_levels(
        &mut gpu_scene.by_object,
        &gpu_scene.prefabs[prefab_path],
        &spawned,
    );
    attach_prefab_material_sources(&mut gpu_scene, &spawned);
    world
        .set_mesh_material_params(
            spawned.handle,
            MeshMaterialParams {
                roughness: Some(0.25),
                ..Default::default()
            },
        )
        .unwrap();
    let (resources_tx, resources_rx) = flume::unbounded();
    let mut sync = MaterialSync::default();
    sync.request_changes(&mut world, &gpu_scene, &resources_tx);
    let request = next_material_request(&resources_rx);

    world.despawn_many(&[spawned.handle]).unwrap();
    detach_object_render_data(&mut gpu_scene, &handles);
    assert!(!gpu_scene.material_sources.contains_key(&spawned.handle));
    assert!(!gpu_scene.material_tokens.contains_key(&spawned.handle));
    sync.accept(MaterialReply {
        object: request.object,
        revision: request.revision,
        result: Ok(MaterialLevels {
            levels: material_levels(140),
            created: vec![fake_material_id(141), fake_material_id(143)],
        }),
    });
    sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 9);
    assert!(!gpu_scene.by_object.contains_key(&spawned.handle));
    match resources_rx.try_recv().unwrap() {
        RenderResourceRequest::ReleaseMaterials {
            materials,
            last_seq,
        } => {
            assert_eq!(materials, [fake_material_id(141), fake_material_id(143)]);
            assert_eq!(
                last_seq, 0,
                "the object never appeared in a submitted snapshot"
            );
        }
        _ => panic!("unused reply materials must be released immediately"),
    }
}

#[test]
fn despawn_releases_accepted_materials_after_the_last_snapshot() {
    let (mut world, crate_handle) = material_test_world();
    let mut gpu_scene = GpuSceneTable::default();
    gpu_scene
        .by_object
        .insert(crate_handle, material_levels(150));
    gpu_scene
        .material_sources
        .insert(crate_handle, material_source());
    let (resources_tx, resources_rx) = flume::unbounded();
    let mut sync = MaterialSync::default();
    world
        .set_mesh_material_params(
            crate_handle,
            MeshMaterialParams {
                roughness: Some(0.75),
                ..Default::default()
            },
        )
        .unwrap();
    sync.request_changes(&mut world, &gpu_scene, &resources_tx);
    let request = next_material_request(&resources_rx);
    sync.accept(MaterialReply {
        object: crate_handle,
        revision: request.revision,
        result: Ok(MaterialLevels {
            levels: material_levels(160),
            created: vec![fake_material_id(161), fake_material_id(163)],
        }),
    });
    sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 6);
    assert_eq!(
        snapshot_materials(&mut world, &gpu_scene, crate_handle),
        [fake_material_id(161), fake_material_id(163)]
    );

    world.despawn_many(&[crate_handle]).unwrap();
    detach_object_render_data(&mut gpu_scene, &[crate_handle]);
    sync.apply_replies(&world, &mut gpu_scene, &resources_tx, 12);
    match resources_rx.try_recv().unwrap() {
        RenderResourceRequest::ReleaseMaterials {
            materials,
            last_seq,
        } => {
            assert_eq!(materials, [fake_material_id(161), fake_material_id(163)]);
            assert_eq!(last_seq, 12);
        }
        _ => panic!("despawned object's materials must be retired after submitted snapshots"),
    }
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
    let world = World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
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
            ui: Default::default(),
            template: prefab_template(BALL_PATH, BALL_PREFAB),
            sources: BTreeMap::new(),
            geometry: Vec::new(),
            material_models: Vec::new(),
        },
        material_assets: Arc::new(PreparedMaterialAssets::default()),
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
    let scripts = start_test_scripts(latency_test_world(), &[], PreparedPrefabs::default(), None);
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
    let scripts = start_test_scripts(latency_test_world(), &[], PreparedPrefabs::default(), None);
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
        fixed_hz: 60,
        max_substeps: 8,
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "startup.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
fn physics_steps_use_the_scaled_clamped_runtime_clock_in_foreground_and_background() {
    let script = r#"
local Clock = {}
function Clock:on_update(dt)
  if rve.time.frame == 1 then
    assert(math.abs(dt - 0.25) < 0.00001)
    assert(math.abs(rve.time.elapsed - 0.25) < 0.00001)
    rve.time.scale = 0.5
  elseif rve.time.frame == 2 then
    assert(math.abs(dt - 0.125) < 0.00001)
    assert(math.abs(rve.time.elapsed - 0.375) < 0.00001)
  end
  self.transform.position = { x = rve.time.elapsed, y = 0, z = 0 }
end
return Clock
"#;
    let document = SceneDocument::parse(
            "startup.rvescene",
            "[[assets]]\npath = 'clock.lua'\n[[objects]]\nid = 'camera'\n[objects.camera]\n[[objects.lite_scripts]]\npath = 'clock.lua'\n",
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
        fixed_hz: 60,
        max_substeps: 8,
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "startup.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
            cursor: CursorControl::default(),
            sources: BTreeMap::from([("clock.lua".to_owned(), script.to_owned())]),
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
    assert_eq!(background_dt, 0.25);
    waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(frame_rx.try_recv().is_err());
    clock.advance(Duration::from_millis(250));
    script_tx.send(ScriptControl::Background(false)).unwrap();

    let foreground_dt = physics_dt_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(foreground_dt, 0.125);
    let snapshot = frame_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!((snapshot.camera.position.x - 0.375).abs() < 1.0e-6);
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "physics-start.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
    let initialization = PhysicsInitialization::from_prebuilt(&driver, bindings.clone()).unwrap();
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "physics-teleport.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
    let initialization = PhysicsInitialization::from_prebuilt(&driver, bindings.clone()).unwrap();
    let mut sync = PhysicsSceneSync::new(&world, bindings.clone()).unwrap();
    let ball = bindings[0].object;
    let mut transform = *world.local_transform(ball).unwrap();
    transform.scale = Vec3::ZERO;
    world.set_local_transform(ball, transform).unwrap();
    world.recalculate_world_matrices();
    let mut scripts = ScriptHost::load(&world, "physics-scale", &BTreeMap::new(), None).unwrap();

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
        fixed_hz: 60,
        max_substeps: 8,
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "physics-failure.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "startup.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "startup.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "startup.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "startup.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
fn script_quit_stops_the_headless_pipeline_gracefully_with_success_status() {
    QUIT_UPDATE_CALLS.store(0, Ordering::SeqCst);
    QUIT_DESTROY_CALLS.store(0, Ordering::SeqCst);
    let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\n\n[[objects.heavy_scripts]]\nname = \"QuitProbe\"\n",
        )
        .unwrap();
    let world = World::from_document(&document).unwrap();
    let mut modules = ModuleHost::new();
    let (script_tx, script_rx) = flume::unbounded();
    let (frame_tx, _frame_rx) = flume::unbounded();
    let (render_tx, render_rx) = flume::unbounded();
    let interrupt = ScriptInterruptHandle::new();
    let status = Arc::new(RuntimeStatus::default());
    let script_thread = start_scripts(
        &mut modules,
        world,
        ScriptThreadConfig {
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "startup.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
    script_thread
        .stopped
        .recv_timeout(Duration::from_secs(2))
        .expect("the headless scripts loop should stop on ctx.quit()");

    assert_eq!(QUIT_UPDATE_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(QUIT_DESTROY_CALLS.load(Ordering::SeqCst), 1);
    assert_eq!(status.exit_code(), 0);
    assert!(!interrupt.is_interrupted());
    assert_eq!(render_rx.recv().unwrap(), RenderControl::Shutdown);
    modules.join_named(SCRIPTS_THREAD).unwrap();
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
            scene_loader: None,
            platform: Default::default(),
            gpu_render: false,
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "startup.rvescene".to_owned(),
            enabled_scenes: Vec::new(),
            save_path: None,
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
    start_test_scripts_wired(
        world,
        sources,
        prefabs,
        physics,
        clock,
        frame_limit,
        background,
        SceneWiring::default(),
    )
}

/// Подключение замены сцены: загрузчик, канал render thread и включённые сцены проекта.
/// Без канала render thread замена применяется на scripts thread (headless).
#[derive(Default)]
struct SceneWiring {
    loader: Option<SceneLoader>,
    render_resources: Option<Sender<RenderResourceRequest>>,
    enabled_scenes: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn start_test_scripts_wired(
    world: World,
    sources: &[(&str, &str)],
    prefabs: PreparedPrefabs,
    physics: Option<(PhysicsClient, PhysicsInitialization)>,
    clock: Arc<FakeClock>,
    frame_limit: Option<u64>,
    background: bool,
    wiring: SceneWiring,
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
            gpu_render: wiring.render_resources.is_some(),
            scene_loader: wiring.loader,
            platform: Default::default(),
            ui: Default::default(),
            ui_text_focus: Default::default(),
            scene_name: "spawn.rvescene".to_owned(),
            enabled_scenes: wiring.enabled_scenes,
            save_path: None,
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
            render_resources: wiring
                .render_resources
                .unwrap_or_else(|| flume::unbounded().0),
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

/// Временная папка с файлами сцен для загрузчика; удаляется в `Drop`.
struct SceneDir(std::path::PathBuf);

impl SceneDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("rve-pipeline-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
}

impl Drop for SceneDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn headless_scene_switch_applies_physics_without_a_render_thread() {
    const SWITCHER: &str = r#"
local Switcher = {}
function Switcher:on_update(dt)
  self.frames = (self.frames or 0) + 1
  if self.frames == 2 then
    rve.scene.load("levels/b.rvescene")
  end
end
return Switcher
"#;
    const START: &str = "[[assets]]
path = \"switcher.lua\"
[[objects]]
id = \"camera\"
[objects.camera]
active = true
[[objects]]
id = \"player\"
keep_on_load = true
[[objects.lite_scripts]]
path = \"switcher.lua\"
[[objects]]
id = \"crate\"
[objects.collider]
shape = \"sphere\"
radius = 0.25
[objects.rigid_body]
kind = \"dynamic\"
";
    const NEXT: &str = "[[objects]]
id = \"camera_b\"
[objects.camera]
active = true
[[objects]]
id = \"boss\"
[objects.collider]
shape = \"sphere\"
radius = 0.5
[objects.rigid_body]
kind = \"dynamic\"
";

    let dir = SceneDir::new("headless-switch");
    dir.write("levels/b.rvescene", NEXT);
    let resources = engine_resources::ResourceSystem::start(engine_resources::ResourceConfig {
        root: dir.0.clone(),
        source_mode: engine_resources::SourceMode::Loose,
        worker_count: 1,
        ..engine_resources::ResourceConfig::default()
    })
    .unwrap();
    let loader = SceneLoader::new(
        resources.handle(),
        engine_project::PhysicsSettings::default(),
        false,
    );
    let world =
        World::from_document(&SceneDocument::parse("levels/a.rvescene", START).unwrap()).unwrap();
    let (client, initialization, steps, worker) = fake_physics(&world);
    let scripts = start_test_scripts_wired(
        world,
        &[("switcher.lua", SWITCHER)],
        PreparedPrefabs::default(),
        Some((client, initialization)),
        Arc::new(FakeClock::default()),
        None,
        false,
        SceneWiring {
            loader: Some(loader),
            render_resources: None,
            enabled_scenes: vec!["levels/b.rvescene".to_owned()],
        },
    );
    scripts.thread.ready.recv().unwrap().unwrap();
    scripts
        .control
        .send(ScriptControl::Go(Box::default()))
        .unwrap();

    let (mut inserted, mut removed) = (0, 0);
    for _ in 0..20 {
        let step = next_step(&scripts, &steps);
        for change in &step.changes {
            match change {
                BodyChange::Insert { .. } => inserted += 1,
                BodyChange::Remove { .. } => removed += 1,
            }
        }
        if inserted > 0 {
            break;
        }
    }
    assert_eq!(
        inserted, 1,
        "без render thread замена применяется на scripts thread"
    );
    assert_eq!(removed, 1, "тело старой сцены снято");
    stop_test_scripts(scripts, worker);
}

#[test]
fn scene_switch_uploads_through_render_thread_then_applies_physics_of_the_new_scene() {
    const SWITCHER: &str = r#"
local Switcher = {}
function Switcher:on_update(dt)
  self.frames = (self.frames or 0) + 1
  if self.frames == 2 then
    rve.scene.load("levels/b.rvescene")
  end
end
return Switcher
"#;
    const START: &str = "[[assets]]\npath = \"switcher.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"player\"\nkeep_on_load = true\n[[objects.lite_scripts]]\npath = \"switcher.lua\"\n[[objects]]\nid = \"crate\"\n[objects.collider]\nshape = \"sphere\"\nradius = 0.25\n[objects.rigid_body]\nkind = \"dynamic\"\n";
    const NEXT: &str = "[[objects]]\nid = \"camera_b\"\n[objects.camera]\nactive = true\n[[objects]]\nid = \"boss\"\n[objects.collider]\nshape = \"sphere\"\nradius = 0.5\n[objects.rigid_body]\nkind = \"dynamic\"\n";

    let dir = SceneDir::new("switch");
    dir.write("levels/b.rvescene", NEXT);
    let resources = engine_resources::ResourceSystem::start(engine_resources::ResourceConfig {
        root: dir.0.clone(),
        source_mode: engine_resources::SourceMode::Loose,
        worker_count: 1,
        ..engine_resources::ResourceConfig::default()
    })
    .unwrap();
    let loader = SceneLoader::new(
        resources.handle(),
        engine_project::PhysicsSettings::default(),
        false,
    );
    let (render_tx, render_rx) = flume::unbounded();
    let world =
        World::from_document(&SceneDocument::parse("levels/a.rvescene", START).unwrap()).unwrap();
    let (client, initialization, steps, worker) = fake_physics(&world);
    let scripts = start_test_scripts_wired(
        world,
        &[("switcher.lua", SWITCHER)],
        PreparedPrefabs::default(),
        Some((client, initialization)),
        Arc::new(FakeClock::default()),
        None,
        false,
        SceneWiring {
            loader: Some(loader),
            render_resources: Some(render_tx),
            enabled_scenes: vec!["levels/b.rvescene".to_owned()],
        },
    );
    scripts.thread.ready.recv().unwrap().unwrap();
    scripts
        .control
        .send(ScriptControl::Go(Box::default()))
        .unwrap();

    // Заявка приходит render thread, пока цикл крутит кадры; физические шаги и кадры забираем.
    let upload = (0..20)
        .find_map(|_| {
            let _ = next_step(&scripts, &steps);
            match render_rx.try_recv() {
                Ok(RenderResourceRequest::Scene(upload)) => Some(upload),
                Ok(_) => panic!("render thread получил не заявку сцены"),
                Err(_) => None,
            }
        })
        .expect("запрос rve.scene.load должен уйти на render thread");
    assert_eq!(upload.generation, 1);

    // До ответа render thread мир не меняется: физика не получает вставок новой сцены.
    for _ in 0..3 {
        let step = next_step(&scripts, &steps);
        assert!(
            !step
                .changes
                .iter()
                .any(|change| matches!(change, BodyChange::Insert { .. })),
            "до ответа render thread вставок нет: {:?}",
            step.changes
        );
    }

    scripts
        .control
        .send(ScriptControl::Scene(Box::new(SceneUploadReply {
            generation: upload.generation,
            result: Ok(Box::default()),
        })))
        .unwrap();

    // После ответа замена применяется: тело 'crate' снимается, тело 'boss' вставляется.
    let mut inserted = 0;
    let mut removed = 0;
    for _ in 0..20 {
        let step = next_step(&scripts, &steps);
        for change in &step.changes {
            match change {
                BodyChange::Insert { .. } => inserted += 1,
                BodyChange::Remove { .. } => removed += 1,
            }
        }
        if inserted > 0 {
            break;
        }
    }
    assert_eq!(inserted, 1, "вставлено тело новой сцены");
    assert_eq!(
        removed, 1,
        "снято тело старой сцены, персистентное `player` без тела"
    );
    // Удаление объекта очищает его эмиттеры (`ParticleRequest::Destroy`); заявок сцены нет.
    for request in render_rx.try_iter() {
        match request {
            RenderResourceRequest::Particle(ParticleRequest::Destroy(_)) => {}
            RenderResourceRequest::Scene(_) => panic!("вторая заявка сцены"),
            RenderResourceRequest::ReleaseScene { ids, .. } => {
                panic!("освобождение без живых ссылок: {} мешей", ids.meshes.len())
            }
            _ => panic!("неожиданная заявка render thread"),
        }
    }
    stop_test_scripts(scripts, worker);
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
    let world = World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
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
    let world = World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
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
    let world = World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
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
    let world = World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
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
    let world = World::from_document(&SceneDocument::parse("spawn.rvescene", SPAWN_SCENE).unwrap())
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
            ui: Default::default(),
            template: prefab_template(BALL_PATH, BALL_PREFAB),
            sources: BTreeMap::new(),
            geometry: Vec::new(),
            material_models: Vec::new(),
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
            &mut crate::game_ui::UiRegistry::default(),
            &mut crate::scene_switch::SceneSwitch::default(),
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

#[test]
fn material_release_waits_until_the_last_snapshot_is_dropped() {
    let (released_tx, released_rx) = mpsc::channel();
    let mut backend = FakeBackend {
        released_materials: Some(released_tx),
        ..FakeBackend::default()
    };
    let materials = vec![fake_material_id(77)];
    let mut snapshot = empty_snapshot();
    snapshot.sequence = 5;
    let mut latest = Some(Arc::new(snapshot));
    let mut releases = vec![PendingRelease {
        target: ReleaseTarget::Materials(materials.clone()),
        last_seq: 5,
    }];

    finish_releases(&mut backend, &mut releases, 4, &mut latest);
    assert!(released_rx.try_recv().is_err());
    assert_eq!(releases.len(), 1);
    assert!(latest.is_some());

    finish_releases(&mut backend, &mut releases, 5, &mut latest);
    assert_eq!(released_rx.try_recv().unwrap(), materials);
    assert!(releases.is_empty());
    assert!(latest.is_none());
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
            ui: Default::default(),
            template: prefab_template(BALL_PATH, BALL_PREFAB),
            sources: BTreeMap::new(),
            geometry: Vec::new(),
            material_models: Vec::new(),
        },
        material_assets: Arc::new(PreparedMaterialAssets::default()),
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
    let world =
        World::from_document(&engine_scene::SceneDocument::parse("scene.rvescene", text).unwrap())
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

#[test]
fn lod_level_palette_is_translated_to_the_level_skeleton() {
    use engine_assets::{ModelInheritMode, ModelJoint, ModelNode, ModelSkeleton, ModelTransform};
    use glam::Mat4;
    let node = |name: &str| ModelNode {
        name: name.to_owned(),
        parent: None,
        inherit_mode: ModelInheritMode::Normal,
        inherit_scale_node: None,
        rest: ModelTransform {
            translation: [0.0; 3],
            rotation: [0.0, 0.0, 0.0, 1.0],
            scale: [1.0; 3],
        },
    };
    let joint = |node: usize, inverse_bind: Mat4| ModelJoint {
        node,
        inverse_bind: inverse_bind.to_cols_array_2d(),
    };
    let base = ModelSkeleton {
        nodes: vec![node("hip"), node("head")],
        joints: vec![joint(0, Mat4::IDENTITY), joint(1, Mat4::IDENTITY)],
    };
    let lod = ModelSkeleton {
        nodes: vec![node("head")],
        joints: vec![joint(0, Mat4::from_scale(glam::Vec3::splat(2.0)))],
    };
    let remap = engine_animation::PaletteRemap::new(&base, &lod).unwrap();
    let head = Mat4::from_translation(glam::Vec3::new(0.0, 3.0, 0.0));
    let palette = [Mat4::IDENTITY.to_cols_array_2d(), head.to_cols_array_2d()];

    assert_eq!(level_palette(None, &palette), Some(palette.to_vec()));
    let level = level_palette(Some(&remap), &palette).unwrap();
    assert_eq!(level.len(), 1);
    assert!(
        Mat4::from_cols_array_2d(&level[0])
            .abs_diff_eq(head * Mat4::from_scale(glam::Vec3::splat(2.0)), 1e-5)
    );
    // Палитра другой длины не переводится: уровень рисуется без скиннинга.
    assert_eq!(level_palette(Some(&remap), &palette[..1]), None);
}
