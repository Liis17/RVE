//! Play-режим: копия мира сцены и скрипты на главном потоке редактора.

use crate::profile::PlayStep;
use engine_animation::{AnimationBridge, AnimationEvent};
use engine_assets::Assets;
use engine_audio::{AudioBridge, AudioConfig, AudioEngine, AudioHandle};
use engine_console::{CommandCatalog, CommandLine};
use engine_physics::{
    ModelGeometry, ModelMesh as PhysicsModelMesh, PhysicsClient, PhysicsInitialization,
    PhysicsReceive, PhysicsSceneDesc, PhysicsSceneSync, PhysicsServer, PhysicsServerError,
};
use engine_project::{InputSettings, PhysicsSettings};
use engine_scene::{ColliderShape, SceneDocument, World};
use engine_script::{
    CommandError, ScriptCursor, ScriptEvent, ScriptHost, ScriptInput, ScriptInterruptHandle,
    ScriptLibrary, ScriptLoadOptions, ScriptTime,
};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Дольше этого Lua-вызов считается зависшим и прерывается, чтобы UI ожил.
const SCRIPT_TIME_LIMIT: Duration = Duration::from_secs(5);
/// Шаг времени не растёт при паузах окна и отладчике.
const MAX_DELTA_SECONDS: f32 = 0.25;

/// Прерывает Lua, если отмеченный участок выполняется дольше предела.
///
/// ABI-скрипты прервать нельзя: зависший Rust-скрипт останавливает весь редактор.
struct Watchdog {
    shared: Arc<WatchdogShared>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct WatchdogState {
    deadline: Option<Instant>,
    stop: bool,
}

#[derive(Default)]
struct WatchdogShared {
    state: Mutex<WatchdogState>,
    changed: Condvar,
}

impl Watchdog {
    fn start(interrupt: ScriptInterruptHandle) -> std::io::Result<Self> {
        let shared = Arc::new(WatchdogShared::default());
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("rve-editor-watchdog".to_owned())
            .spawn(move || watch(&thread_shared, &interrupt))?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    fn set_deadline(&self, deadline: Option<Instant>) {
        lock(&self.shared.state).deadline = deadline;
        self.shared.changed.notify_all();
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        lock(&self.shared.state).stop = true;
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

fn watch(shared: &WatchdogShared, interrupt: &ScriptInterruptHandle) {
    let mut state = lock(&shared.state);
    while !state.stop {
        state = match state.deadline {
            None => shared
                .changed
                .wait(state)
                .unwrap_or_else(|error| error.into_inner()),
            Some(deadline) => {
                let now = Instant::now();
                if now >= deadline {
                    interrupt.interrupt();
                    state.deadline = None;
                    continue;
                }
                shared
                    .changed
                    .wait_timeout(state, deadline - now)
                    .unwrap_or_else(|error| error.into_inner())
                    .0
            }
        };
    }
}

/// Запущенная сцена. Документ не меняется: правки скриптов живут только в этом мире,
/// поэтому Stop возвращает редактор к исходному документу простым удалением сессии.
pub struct PlaySession {
    world: World,
    host: ScriptHost,
    physics: Option<PlayPhysics>,
    audio: Option<PlayAudio>,
    /// Аниматоры мира Play: запросы скриптов, время и события `animation_finished`.
    animation: AnimationBridge,
    watchdog: Watchdog,
    time_limit: Duration,
    paused: bool,
    elapsed: f64,
    frame: u64,
    last_tick: Instant,
    input: ScriptInput,
    last_step: PlayStep,
}

impl PlaySession {
    /// Запускает сцену. Исходники Lua читаются через `read_source`; скрипты получают `on_start`
    /// при первом обновлении.
    #[cfg(test)]
    pub fn start(
        document: &SceneDocument,
        library: Option<&ScriptLibrary>,
        read_source: impl FnMut(&str) -> Result<String, String>,
    ) -> Result<Self, String> {
        let world = World::from_document(document).map_err(|error| error.to_string())?;
        Self::start_with_world(
            document,
            world,
            library,
            &InputSettings::default(),
            read_source,
        )
    }

    /// Starts from a world whose referenced camera settings are already loaded.
    #[cfg(test)]
    pub fn start_with_world(
        document: &SceneDocument,
        world: World,
        library: Option<&ScriptLibrary>,
        input_settings: &InputSettings,
        read_source: impl FnMut(&str) -> Result<String, String>,
    ) -> Result<Self, String> {
        Self::start_with_world_limit(
            document,
            world,
            library,
            input_settings,
            read_source,
            &[],
            &PhysicsSettings::default(),
            SCRIPT_TIME_LIMIT,
            AudioConfig::default(),
            AnimationBridge::default(),
        )
    }

    /// Starts Play with project input and physics settings, CPU geometry for mesh colliders,
    /// the audio configuration (ресурсы проекта и устройство вывода) and the animation bridge
    /// built from the same world.
    #[allow(clippy::too_many_arguments)]
    pub fn start_with_world_and_physics(
        document: &SceneDocument,
        world: World,
        library: Option<&ScriptLibrary>,
        input_settings: &InputSettings,
        read_source: impl FnMut(&str) -> Result<String, String>,
        model_geometry: &[ModelGeometry],
        physics_settings: &PhysicsSettings,
        audio: AudioConfig,
        animation: AnimationBridge,
    ) -> Result<Self, String> {
        Self::start_with_world_limit(
            document,
            world,
            library,
            input_settings,
            read_source,
            model_geometry,
            physics_settings,
            SCRIPT_TIME_LIMIT,
            audio,
            animation,
        )
    }

    /// Loads the CPU model geometry required by convex and triangle-mesh colliders.
    pub fn load_model_geometry(
        world: &World,
        assets: &Assets,
    ) -> Result<Vec<ModelGeometry>, String> {
        let paths = world
            .objects()
            .iter()
            .filter_map(|object| {
                let collider = object.collider?;
                if matches!(
                    collider.shape,
                    ColliderShape::ConvexMesh | ColliderShape::TriangleMesh
                ) {
                    Some(object.mesh_renderer.as_ref()?.model.as_str().to_owned())
                } else {
                    None
                }
            })
            .collect::<BTreeSet<_>>();
        paths
            .into_iter()
            .map(|path| {
                let model = assets
                    .load_model(&path)
                    .map_err(|error| format!("{path}: {error}"))?;
                Ok(ModelGeometry {
                    path,
                    meshes: model
                        .meshes
                        .iter()
                        .map(|mesh| PhysicsModelMesh {
                            positions: mesh
                                .vertices
                                .iter()
                                .map(|vertex| glam::Vec3::from_array(vertex.position))
                                .collect(),
                            indices: mesh.indices.clone(),
                        })
                        .collect(),
                })
            })
            .collect()
    }

    #[cfg(test)]
    fn start_with_limit(
        document: &SceneDocument,
        library: Option<&ScriptLibrary>,
        read_source: impl FnMut(&str) -> Result<String, String>,
        time_limit: Duration,
    ) -> Result<Self, String> {
        let world = World::from_document(document).map_err(|error| error.to_string())?;
        Self::start_with_world_limit(
            document,
            world,
            library,
            &InputSettings::default(),
            read_source,
            &[],
            &PhysicsSettings::default(),
            time_limit,
            AudioConfig::default(),
            AnimationBridge::default(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn start_with_world_limit(
        document: &SceneDocument,
        mut world: World,
        library: Option<&ScriptLibrary>,
        input_settings: &InputSettings,
        mut read_source: impl FnMut(&str) -> Result<String, String>,
        model_geometry: &[ModelGeometry],
        physics_settings: &PhysicsSettings,
        time_limit: Duration,
        audio: AudioConfig,
        animation: AnimationBridge,
    ) -> Result<Self, String> {
        let mut sources = BTreeMap::new();
        for object in world.objects() {
            for script in &object.lite_scripts {
                let path = script.path.as_str();
                if !sources.contains_key(path) {
                    sources.insert(path.to_owned(), read_source(path)?);
                }
            }
        }
        let interrupt = ScriptInterruptHandle::new();
        let watchdog = Watchdog::start(interrupt.clone())
            .map_err(|error| format!("не удалось запустить watchdog скриптов: {error}"))?;
        watchdog.set_deadline(Some(Instant::now() + time_limit));
        let mut host = ScriptHost::load_with_options(
            &world,
            document.source_label(),
            &sources,
            library,
            ScriptLoadOptions {
                interrupt,
                call_time_limit: None,
                input: input_settings.clone(),
            },
        )
        .map_err(|error| error.to_string())?;
        watchdog.set_deadline(None);
        host.set_cursor(ScriptCursor::from(world.active_camera_cursor()));
        let desc = PhysicsSceneDesc::from_world(
            &world,
            model_geometry,
            glam::Vec3::from_array(physics_settings.gravity),
            physics_settings.fixed_hz,
            physics_settings.max_substeps as usize,
        )
        .map_err(|error| error.to_string())?;
        let (driver, bindings) = desc.build().map_err(|error| error.to_string())?;
        let (physics, initial_snapshot) = PlayPhysics::start(&world, driver, bindings)?;
        host.set_physics_snapshot(initial_snapshot, physics.sync.bindings());
        let mut audio = PlayAudio::start(audio)?;
        // Матрицы до первого кадра не посчитаны; play_on_start берёт позиции из них.
        world.recalculate_world_matrices();
        audio.bridge.start(&world);
        Ok(Self {
            world,
            host,
            physics: Some(physics),
            audio: Some(audio),
            animation,
            watchdog,
            time_limit,
            paused: false,
            elapsed: 0.0,
            frame: 0,
            last_tick: Instant::now(),
            last_step: PlayStep::default(),
            input: ScriptInput::default(),
        })
    }

    pub fn world_mut(&mut self) -> &mut World {
        &mut self.world
    }

    /// Мир и мост анимации для кадра: палитры берутся из моста, а скиннинг — из мира.
    pub fn world_and_animation(&mut self) -> (&mut World, &AnimationBridge) {
        (&mut self.world, &self.animation)
    }

    /// Замеры последнего выполненного шага Play.
    pub fn last_step(&self) -> PlayStep {
        self.last_step
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Курсор, установленный скриптами последним успешным вызовом или флагами камеры при старте.
    pub fn cursor(&self) -> ScriptCursor {
        self.host.cursor()
    }

    /// Пауза не накапливает время: первый кадр после неё получает обычный шаг.
    /// Звук на паузе замолкает; позиции и запуски в это время не обрабатываются.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.last_tick = Instant::now();
        if let Some(audio) = &self.audio {
            // Поток звука уже остановлен, если команда не принята; тишина тогда не нужна.
            let _ = audio.handle.set_paused(paused);
        }
    }

    /// Вводит состояние кнопок и клавиш для следующего обновления.
    pub fn input_mut(&mut self) -> &mut ScriptInput {
        &mut self.input
    }

    pub(crate) fn clear_input(&mut self) {
        self.input.release_all();
        self.input.clear_pointer();
    }

    pub fn command_catalog(&self) -> CommandCatalog {
        self.host.command_catalog()
    }

    /// Executes a console callback under the same watchdog used for Play updates.
    pub fn execute_command(&mut self, command: &CommandLine) -> Result<(), CommandError> {
        self.watchdog
            .set_deadline(Some(Instant::now() + self.time_limit));
        let result = self.host.execute_command(&mut self.world, command);
        self.watchdog.set_deadline(None);
        result
    }

    /// Выполняет один кадр скриптов, если сцена не на паузе.
    pub fn tick(&mut self) -> Result<(), String> {
        let now = Instant::now();
        let dt = now
            .duration_since(self.last_tick)
            .as_secs_f32()
            .min(MAX_DELTA_SECONDS);
        self.last_tick = now;
        self.step(dt)
    }

    fn step(&mut self, dt: f32) -> Result<(), String> {
        if self.paused {
            return Ok(());
        }
        let mut physics_wait = Duration::ZERO;
        if let Some(physics) = &mut self.physics {
            let started = Instant::now();
            physics.receive(&mut self.world, &mut self.host)?;
            physics_wait = started.elapsed();
        }
        self.elapsed += f64::from(dt);
        self.frame += 1;
        self.host.set_input(self.input.take_frame());
        let time = self.script_time();
        self.watchdog
            .set_deadline(Some(Instant::now() + self.time_limit));
        self.host.update(&mut self.world, dt, time);
        self.watchdog.set_deadline(None);
        // Порядок как в runtime: запросы скриптов, затем время; события уйдут в следующий update.
        self.animation.apply(self.host.drain_animation_requests());
        let finished = self.animation.advance(dt);
        self.host.queue_animation_events(
            &self.world,
            finished.into_iter().map(|event| match event {
                AnimationEvent::Finished { object, motion } => (object, motion),
            }),
        );
        let physics_started = Instant::now();
        let mut physics_step = Duration::ZERO;
        if let Some(physics) = &mut self.physics {
            let mut commands = self.host.drain_physics_commands();
            commands.extend(
                physics
                    .sync
                    .sync_world_commands(&mut self.world)
                    .map_err(|error| error.to_string())?,
            );
            physics
                .client
                .step(dt, commands)
                .map_err(|error| error.to_string())?;
            physics_step = physics_started.elapsed();
        }
        self.last_step = PlayStep {
            script: self.host.last_update_profile(),
            physics_wait,
            physics_step,
        };
        if let Some(audio) = &mut self.audio {
            audio
                .bridge
                .apply(self.host.drain_audio_requests(), &self.world);
            // Редактор не вызывает build_frame, поэтому матрицы пересчитываются здесь.
            self.world.recalculate_world_matrices();
            audio.bridge.sync_positions(&self.world);
        }
        Ok(())
    }

    fn script_time(&self) -> ScriptTime {
        ScriptTime {
            elapsed: self.elapsed,
            frame: self.frame,
        }
    }

    pub fn drain_events(&mut self) -> Vec<ScriptEvent> {
        self.host.drain_events()
    }

    /// Предупреждения звука, которые ещё не показаны: клип не загрузился, устройство недоступно.
    pub fn drain_audio_warnings(&mut self) -> Vec<String> {
        self.audio
            .as_mut()
            .map_or_else(Vec::new, |audio| audio.bridge.take_warnings())
    }

    /// Предупреждения анимации: клип или модель не загрузились, движение не найдено.
    pub fn drain_animation_warnings(&mut self) -> Vec<String> {
        self.animation.take_warnings()
    }

    /// Завершает скрипты вызовом `on_destroy` и возвращает последние события.
    pub fn stop(mut self) -> Vec<ScriptEvent> {
        let time = self.script_time();
        self.watchdog
            .set_deadline(Some(Instant::now() + self.time_limit));
        self.host.destroy(&mut self.world, time);
        self.watchdog.set_deadline(None);
        let mut events = self.host.drain_events();
        if let Some(mut physics) = self.physics.take()
            && let Err(error) = physics.shutdown()
        {
            events.push(physics_error_event(error));
        }
        if let Some(mut audio) = self.audio.take() {
            audio.shutdown();
        }
        events
    }
}

impl Drop for PlaySession {
    fn drop(&mut self) {
        if let Some(mut physics) = self.physics.take() {
            let _ = physics.shutdown();
        }
        if let Some(mut audio) = self.audio.take() {
            audio.shutdown();
        }
    }
}

/// Звук Play: поток `rve-audio` и мост, который отдаёт ему запросы скриптов и позиции.
struct PlayAudio {
    bridge: AudioBridge,
    handle: AudioHandle,
    thread: Option<JoinHandle<()>>,
}

impl PlayAudio {
    fn start(config: AudioConfig) -> Result<Self, String> {
        let (engine, handle) = AudioEngine::new(config);
        let thread = std::thread::Builder::new()
            .name("rve-audio".to_owned())
            .spawn(move || engine.run())
            .map_err(|error| format!("не удалось запустить поток звука: {error}"))?;
        Ok(Self {
            bridge: AudioBridge::new(handle.clone()),
            handle,
            thread: Some(thread),
        })
    }

    /// Останавливает поток (он закрывает устройство вывода) и ждёт его завершения.
    /// Повторный вызов безвреден.
    fn shutdown(&mut self) {
        self.handle.shutdown();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct PlayPhysics {
    client: PhysicsClient,
    sync: PhysicsSceneSync,
    _control_tx: flume::Sender<()>,
    control_rx: flume::Receiver<()>,
    worker: Option<JoinHandle<Result<(), PhysicsServerError>>>,
}

impl PlayPhysics {
    fn start(
        world: &World,
        driver: engine_physics::StepDriver,
        bindings: Vec<engine_physics::PhysicsSceneBinding>,
    ) -> Result<(Self, Arc<engine_physics::QuerySnapshot>), String> {
        let initialization =
            PhysicsInitialization::from_prebuilt(&driver, bindings).map_err(|error| {
                format!("не удалось создать начальный snapshot физики Play: {error}")
            })?;
        let sync = PhysicsSceneSync::new(world, initialization.bindings.clone())
            .map_err(|error| format!("не удалось синхронизировать физику Play: {error}"))?;
        let snapshot = Arc::clone(&initialization.result.snapshot);
        let (request_tx, request_rx) = flume::unbounded();
        let (reply_tx, reply_rx) = flume::unbounded();
        let worker = std::thread::Builder::new()
            .name("rve-editor-physics".to_owned())
            .spawn(move || PhysicsServer::run_preinitialized(request_rx, reply_tx, driver))
            .map_err(|error| format!("не удалось запустить физику Play: {error}"))?;
        let (control_tx, control_rx) = flume::unbounded();
        Ok((
            Self {
                client: PhysicsClient::new_preinitialized(request_tx, reply_rx),
                sync,
                _control_tx: control_tx,
                control_rx,
                worker: Some(worker),
            },
            snapshot,
        ))
    }

    fn receive(&mut self, world: &mut World, host: &mut ScriptHost) -> Result<(), String> {
        match self
            .client
            .receive(&self.control_rx)
            .map_err(|error| format!("ошибка шага физики Play: {error}"))?
        {
            None => Ok(()),
            Some(PhysicsReceive::Step(result)) => {
                self.sync
                    .apply_physics_poses(world, &result.poses)
                    .map_err(|error| error.to_string())?;
                host.set_physics_snapshot(result.snapshot, self.sync.bindings());
                host.queue_physics_events(result.events);
                Ok(())
            }
            Some(PhysicsReceive::Interrupted(())) => {
                Err("физика Play получила неожиданное сообщение управления".to_owned())
            }
        }
    }

    fn shutdown(&mut self) -> Result<(), String> {
        let mut error = self
            .client
            .shutdown()
            .err()
            .map(|error| format!("не удалось остановить физику Play: {error}"));
        if let Some(worker) = self.worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(server_error)) => {
                    error.get_or_insert_with(|| {
                        format!("поток физики Play завершился: {server_error}")
                    });
                }
                Err(_) => {
                    error.get_or_insert_with(|| "поток физики Play завершился паникой".to_owned());
                }
            }
        }
        error.map_or(Ok(()), Err)
    }
}

impl Drop for PlayPhysics {
    fn drop(&mut self) {
        if self.worker.is_some() {
            let _ = self.shutdown();
        }
    }
}

fn physics_error_event(message: String) -> ScriptEvent {
    ScriptEvent {
        level: engine_script::ScriptEventLevel::Error,
        source: "physics".to_owned(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_project::InputAction;

    const SCENE: &str = r#"
[[assets]]
path = "/rotate.lua"

[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "spinner"
[[objects.lite_scripts]]
path = "/rotate.lua"
params = { speed = 90.0 }
"#;

    const ROTATE: &str = r#"
local Rotate = {}
function Rotate:on_start()
  rve.log_info("started")
end
function Rotate:on_update(dt)
  local rotation = self.transform.rotation
  self.transform.rotation = { x = rotation.x, y = rotation.y + self.params.speed * dt, z = rotation.z }
end
function Rotate:on_destroy()
  rve.log_info("destroyed")
end
return Rotate
"#;

    fn document(source: &str) -> SceneDocument {
        SceneDocument::parse("scene", source).unwrap()
    }

    fn start(script: &str) -> Result<PlaySession, String> {
        PlaySession::start(&document(SCENE), None, |_| Ok(script.to_owned()))
    }

    fn start_with_input_settings(
        script: &str,
        input_settings: InputSettings,
    ) -> Result<PlaySession, String> {
        let document = document(SCENE);
        let world = World::from_document(&document).map_err(|error| error.to_string())?;
        PlaySession::start_with_world(&document, world, None, &input_settings, |_| {
            Ok(script.to_owned())
        })
    }

    fn rotation_y(session: &mut PlaySession) -> f32 {
        let world = session.world_mut();
        let handle = world.handle("spinner").unwrap();
        world.local_transform(handle).unwrap().rotation_deg.y
    }

    #[test]
    fn lua_script_changes_the_play_world_but_not_the_document() {
        let document = document(SCENE);
        let mut session = PlaySession::start(&document, None, |_| Ok(ROTATE.to_owned())).unwrap();
        session.step(0.5).unwrap();
        assert!((rotation_y(&mut session) - 45.0).abs() < 1e-3);
        let original = document.objects.iter().find(|o| o.id == "spinner").unwrap();
        assert_eq!(original.transform.rotation, [0.0; 3]);
    }

    #[test]
    fn play_starts_with_the_active_camera_cursor_flags() {
        let scene = SCENE.replace(
            "[objects.camera]\n",
            "[objects.camera]\n[objects.camera.cursor]\nhide_on_start = true\nlock_on_start = true\n",
        );
        let session =
            PlaySession::start(&document(&scene), None, |_| Ok(ROTATE.to_owned())).unwrap();
        assert_eq!(
            session.cursor(),
            ScriptCursor {
                visible: false,
                locked: true,
            }
        );
    }

    #[test]
    fn pause_stops_updates_and_resume_continues() {
        let mut session = start(ROTATE).unwrap();
        session.step(0.5).unwrap();
        session.set_paused(true);
        assert!(session.is_paused());
        session.step(0.5).unwrap();
        session.tick().unwrap();
        assert!((rotation_y(&mut session) - 45.0).abs() < 1e-3);
        session.set_paused(false);
        session.step(0.5).unwrap();
        assert!((rotation_y(&mut session) - 90.0).abs() < 1e-3);
    }

    #[test]
    fn play_receives_initial_physics_snapshot_before_on_start() {
        let source = r#"
[[assets]]
path = "probe.lua"
[[objects]]
id = "camera"
[objects.transform]
position = [0.0, 0.0, -2.0]
[objects.camera]
[[objects.lite_scripts]]
path = "probe.lua"
[[objects]]
id = "wall"
[objects.collider]
shape = "sphere"
radius = 0.5
"#;
        let script = r#"
local Script = {}
function Script:on_start()
  local hit = rve.physics.raycast({x=0,y=0,z=-2}, {x=0,y=0,z=1}, 5)
  assert(hit and hit.object.id == "wall")
  self.transform.position = {x=3,y=0,z=-2}
end
return Script
"#;
        let mut session =
            PlaySession::start(&document(source), None, |_| Ok(script.to_owned())).unwrap();
        session.step(0.0).unwrap();
        let camera = session.world.handle("camera").unwrap();
        assert_eq!(
            session.world.local_transform(camera).unwrap().position.x,
            3.0
        );
        session.stop();
    }

    #[test]
    fn pause_does_not_submit_or_wait_for_an_extra_physics_step() {
        let source = r#"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "ball"
[objects.transform]
position = [0.0, 2.0, 0.0]
[objects.collider]
shape = "sphere"
radius = 0.5
[objects.rigid_body]
kind = "dynamic"
mass = 1.0
"#;
        let mut session =
            PlaySession::start(&document(source), None, |_| Ok(String::new())).unwrap();
        session.step(1.0 / 60.0).unwrap();
        session.set_paused(true);
        session.step(0.5).unwrap();
        session.tick().unwrap();
        session.set_paused(false);
        session.step(1.0 / 60.0).unwrap();

        let ball = session.world.handle("ball").unwrap();
        assert!(session.world.local_transform(ball).unwrap().position.y < 2.0);
        session.stop();
    }

    #[test]
    fn dropping_play_shuts_down_physics_with_an_outstanding_step() {
        let source = r#"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "ball"
[objects.transform]
position = [0.0, 2.0, 0.0]
[objects.collider]
shape = "sphere"
radius = 0.5
[objects.rigid_body]
kind = "dynamic"
mass = 1.0
"#;
        let mut session =
            PlaySession::start(&document(source), None, |_| Ok(String::new())).unwrap();
        session.step(1.0 / 60.0).unwrap();

        drop(session);
    }

    const ANIMATED_SCENE: &str = r#"
[[assets]]
path = "/models/hero.fbx"
[[assets]]
path = "/hero.lua"

[[objects]]
id = "camera"
[objects.camera]

[[objects]]
id = "hero"
[objects.mesh_renderer]
model = "/models/hero.fbx"
[objects.animator]
looping = false
[[objects.lite_scripts]]
path = "/hero.lua"
"#;

    const ANIMATED_HERO: &str = r#"
local Hero = {}
function Hero:on_start()
  rve.find("hero"):play_animation("Walk", { loop = false })
end
function Hero:on_message(sender, name, value)
  rve.log_info(name .. ":" .. value)
end
return Hero
"#;

    #[test]
    fn one_shot_animation_finish_reaches_the_script_as_a_message() {
        let document = document(ANIMATED_SCENE);
        let world = World::from_document(&document).unwrap();
        let animation = AnimationBridge::build(
            &world,
            |_| {
                Ok(std::sync::Arc::new(
                    crate::viewport::animation::test_support::walk_model(),
                ))
            },
            |_| Ok(std::sync::Arc::new(Vec::new())),
        );
        let mut session = PlaySession::start_with_world_limit(
            &document,
            world,
            None,
            &InputSettings::default(),
            |_| Ok(ANIMATED_HERO.to_owned()),
            &[],
            &PhysicsSettings::default(),
            SCRIPT_TIME_LIMIT,
            AudioConfig::default(),
            animation,
        )
        .unwrap();

        // Первый шаг запускает on_start и клип; конец клипа приходит в шаге, где время прошло.
        session.step(0.1).unwrap();
        let hero = session.world.handle("hero").unwrap();
        assert!(session.animation.has_animator(hero));
        session.step(1.5).unwrap();
        session.step(0.0).unwrap();
        let messages: Vec<String> = session
            .drain_events()
            .into_iter()
            .map(|event| event.message)
            .collect();
        assert!(
            messages
                .iter()
                .any(|message| message == "animation_finished:Walk"),
            "{messages:?}"
        );
        assert!(session.drain_animation_warnings().is_empty());
    }

    #[test]
    fn demo_worm_curl_finishes_into_wiggle_and_its_script_hears_it() {
        // Ассеты и скрипт берутся из examples/demo-game: тест следит за работоспособностью демо.
        let root = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../examples/demo-game"
        ));
        let project = crate::project::OpenProject::open(root).unwrap();
        const SCENE: &str = "[[assets]]\npath = \"/models/worm.fbx\"\n[[assets]]\npath = \"/anims/worm_curl.fbx\"\n[[assets]]\npath = \"/scripts/worm.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\n[[objects]]\nid = \"worm\"\n[objects.mesh_renderer]\nmodel = \"/models/worm.fbx\"\n[objects.animator]\nclips = [\"/anims/worm_curl.fbx\"]\nplay_on_start = false\n[[objects.lite_scripts]]\npath = \"/scripts/worm.lua\"\n";
        let document = SceneDocument::parse("worm.rvescene", SCENE).unwrap();
        let world = World::from_document(&document).unwrap();
        let animation = crate::viewport::animation::project_bridge(&world, &project.assets);
        let mut session = PlaySession::start_with_world_and_physics(
            &document,
            world,
            None,
            &project.settings.input,
            |path| project.read_text(path),
            &[],
            &project.settings.physics,
            AudioConfig::default(),
            animation,
        )
        .unwrap();

        // Разовый клип длится 1 с; за 1.5 с он заканчивается и переходит в цикл Wiggle.
        for _ in 0..30 {
            session.step(0.05).unwrap();
        }
        let messages: Vec<String> = session
            .drain_events()
            .into_iter()
            .map(|event| event.message)
            .collect();
        assert!(
            messages.contains(&"Worm: curl, then Wiggle".to_owned()),
            "{messages:?}"
        );
        assert!(
            messages.contains(&"Worm: worm_curl finished".to_owned()),
            "{messages:?}"
        );
        assert!(session.drain_animation_warnings().is_empty());
    }

    #[test]
    fn stop_runs_on_destroy_and_reports_events() {
        let mut session = start(ROTATE).unwrap();
        session.step(0.1).unwrap();
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "started")
        );
        let events = session.stop();
        assert!(events.iter().any(|event| event.message == "destroyed"));
    }

    #[test]
    fn heavy_script_without_a_library_refuses_to_start() {
        let source = format!("{SCENE}\n[[objects.heavy_scripts]]\nname = \"Bobber\"\n");
        let error = PlaySession::start(&document(&source), None, |_| Ok(ROTATE.to_owned()))
            .err()
            .unwrap();
        assert!(error.contains("Bobber"), "{error}");
    }

    #[test]
    fn unreadable_or_broken_lua_refuses_to_start() {
        let error = PlaySession::start(&document(SCENE), None, |_| Err("нет файла".to_owned()))
            .err()
            .unwrap();
        assert!(error.contains("нет файла"));
        assert!(start("this is not lua").is_err());
    }

    #[test]
    fn watchdog_interrupts_a_hanging_lua_update_and_disables_lua() {
        let hanging =
            "local H = {}\nfunction H:on_update(dt)\n  while true do end\nend\nreturn H\n";
        let mut session = PlaySession::start_with_limit(
            &document(SCENE),
            None,
            |_| Ok(hanging.to_owned()),
            Duration::from_millis(50),
        )
        .unwrap();
        session.step(0.1).unwrap();
        assert!(!session.drain_events().is_empty());
        // Прерывание постоянно: следующий кадр не должен снова зависнуть.
        session.step(0.1).unwrap();
    }

    #[test]
    fn watchdog_interrupts_a_hanging_top_level_chunk() {
        let result = PlaySession::start_with_limit(
            &document(SCENE),
            None,
            |_| Ok("while true do end".to_owned()),
            Duration::from_millis(50),
        );
        assert!(result.is_err());
    }

    #[test]
    fn input_snapshot_reaches_the_script() {
        let script = r#"
local S = {}
function S:on_update(dt)
  if rve.input.key_pressed("Space") then rve.log_info("space") end
end
return S
"#;
        let mut session = start(script).unwrap();
        session.input_mut().set_key(engine_script::Key::Space, true);
        session.step(0.1).unwrap();
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "space")
        );
    }

    #[test]
    fn project_input_actions_reach_editor_play() {
        let script = r#"
local S = {}
function S:on_update()
  if rve.input.action_pressed("Jump") then rve.log_info("jump action") end
end
return S
"#;
        let input_settings = InputSettings {
            default_context: Some("game".to_owned()),
            contexts: std::collections::BTreeMap::from([(
                "game".to_owned(),
                std::collections::BTreeMap::from([(
                    "Jump".to_owned(),
                    InputAction::Button {
                        inputs: vec!["Space".to_owned()],
                    },
                )]),
            )]),
        };
        let mut session = start_with_input_settings(script, input_settings).unwrap();
        session.input_mut().set_key(engine_script::Key::Space, true);
        session.step(0.1).unwrap();
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "jump action")
        );
    }

    #[test]
    fn clear_input_releases_buttons_and_discards_pointer_deltas() {
        let script = r#"
local S = {}
function S:on_update()
  local dx, dy = rve.input.mouse_delta()
  local wx, wy = rve.input.wheel()
  if rve.input.key_released("Space") and not rve.input.key_down("Space")
      and dx == 0 and dy == 0 and wx == 0 and wy == 0 then
    rve.log_info("play input cleared")
  end
end
return S
"#;
        let mut session = start(Box::leak(script.to_owned().into_boxed_str())).unwrap();
        let input = session.input_mut();
        input.set_key(engine_script::Key::Space, true);
        input.add_mouse_delta(10.0, -5.0);
        input.add_wheel(0.0, 2.0);
        session.clear_input();
        session.step(0.1).unwrap();
        assert!(
            session
                .drain_events()
                .iter()
                .any(|event| event.message == "play input cleared")
        );
    }

    #[test]
    fn console_commands_register_and_execute_in_play() {
        let script = r#"
local S = {}
function S:on_start()
  rve.console.register("echo", function(args)
    rve.console.write(args[1], "warning")
  end, "write one argument")
end
return S
"#;
        let mut session = start(script).unwrap();
        session.tick().unwrap();
        assert_eq!(
            session.command_catalog().get("echo").unwrap().description,
            "write one argument"
        );
        session
            .execute_command(&CommandLine {
                name: "echo".to_owned(),
                args: vec!["hello".to_owned()],
            })
            .unwrap();
        let event = session.drain_events().pop().unwrap();
        assert_eq!(event.level, engine_script::ScriptEventLevel::Warn);
        assert_eq!(event.message, "hello");
    }

    const AUDIO_SCENE: &str = r#"
[[assets]]
path = "/audio/hum.wav"
[[objects]]
id = "camera"
[objects.camera]
[[objects]]
id = "speaker"
[objects.transform]
position = [3.0, 0.0, 0.0]
[objects.audio_source]
clip = "/audio/hum.wav"
looping = true
play_on_start = true
spatial = true
"#;

    #[test]
    fn a_clip_that_cannot_load_is_reported_for_the_editor_log() {
        let mut session =
            PlaySession::start(&document(AUDIO_SCENE), None, |_| Ok(String::new())).unwrap();
        // Без ресурсов проекта play_on_start-клип не читается; барьер ждёт, пока поток это запишет.
        session.audio.as_ref().unwrap().handle.barrier().unwrap();
        let warnings = session.drain_audio_warnings();
        assert!(
            warnings
                .iter()
                .any(|message| message.contains("audio/hum.wav")),
            "{warnings:?}"
        );
        assert!(
            session.drain_audio_warnings().is_empty(),
            "предупреждение показывается один раз"
        );
    }

    #[test]
    fn play_audio_follows_a_moved_source_without_build_frame() {
        // Позиция отслеживается только у живого голоса: без читаемого клипа он завершается сразу.
        // Поэтому клип зациклен и читается из временного каталога ресурсов.
        let parent = std::env::temp_dir().join(format!("rve-play-audio-{}", std::process::id()));
        let assets = parent.join("Assets");
        std::fs::create_dir_all(assets.join("audio")).unwrap();
        std::fs::write(
            assets.join("audio").join("tone.ogg"),
            include_bytes!("../../engine/engine-audio/tests/fixtures/tone_mono_22050.ogg"),
        )
        .unwrap();
        let mut resources =
            engine_resources::ResourceSystem::start(engine_resources::ResourceConfig {
                root: assets,
                source_mode: engine_resources::SourceMode::Loose,
                ..engine_resources::ResourceConfig::default()
            })
            .unwrap();
        let scene = document(&AUDIO_SCENE.replace("hum.wav", "tone.ogg"));
        let world = World::from_document(&scene).unwrap();
        let audio = AudioConfig {
            resources: Some(resources.handle()),
            output: false,
        };
        let mut session = PlaySession::start_with_world_limit(
            &scene,
            world,
            None,
            &InputSettings::default(),
            |_| Ok(String::new()),
            &[],
            &PhysicsSettings::default(),
            SCRIPT_TIME_LIMIT,
            audio,
            AnimationBridge::default(),
        )
        .unwrap();
        let speaker = session.world_mut().handle("speaker").unwrap();
        session.tick().unwrap();
        let bridge = &session.audio.as_ref().unwrap().bridge;
        assert_eq!(bridge.emitter_of(speaker), Some([3.0, 0.0, 0.0]));

        let world = session.world_mut();
        let mut transform = *world.local_transform(speaker).unwrap();
        transform.position.x = 5.0;
        world.set_local_transform(speaker, transform).unwrap();
        session.tick().unwrap();
        let bridge = &session.audio.as_ref().unwrap().bridge;
        assert_eq!(bridge.emitter_of(speaker), Some([5.0, 0.0, 0.0]));

        drop(session);
        resources.shutdown().unwrap();
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn play_pause_and_stop_drive_the_audio_thread_without_hanging() {
        let mut session =
            PlaySession::start(&document(AUDIO_SCENE), None, |_| Ok(String::new())).unwrap();
        session.set_paused(true);
        session.set_paused(false);
        // Stop ждёт завершения потока звука; если он не остановится, тест зависнет.
        assert!(session.stop().is_empty());
    }
}
