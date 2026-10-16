use crate::module_host::{ModuleHost, panic_message};
use engine_render::{
    DirectionalLight, DrawCommand, Frame, FrameStatus, MeshId, Renderer, TextureId,
};
use engine_scene::camera::Camera;
use engine_scene::light::shadow_view_projection;
use engine_scene::{LightState, ObjectHandle, World};
use engine_script::{
    HeavyRegistry, ScriptEvent, ScriptEventLevel, ScriptHost, ScriptInterruptHandle, ScriptTime,
};
use flume::{Receiver, RecvTimeoutError, Selector, Sender, TryRecvError};
use glam::Vec3;
use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

pub(crate) const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.12, 1.0];
pub(crate) const SCRIPTS_THREAD: &str = "rve-scripts";
pub(crate) const RENDER_THREAD: &str = "rve-render";

#[derive(Debug)]
pub(crate) struct FrameSnapshot {
    pub camera: Camera,
    pub light: LightState,
    pub draws: Vec<DrawCommand>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct GpuSceneTable {
    pub by_object: HashMap<ObjectHandle, Vec<(MeshId, Option<TextureId>)>>,
}

#[derive(Debug)]
pub(crate) enum ScriptControl {
    Go(GpuSceneTable),
    Background(bool),
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RenderControl {
    Resize(u32, u32),
    Occluded(bool),
    Shutdown,
}

#[derive(Default)]
pub(crate) struct RuntimeStatus {
    exit_code: AtomicU8,
    render_stopped: AtomicBool,
    error: Mutex<Option<String>>,
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
    fn render(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
    ) -> Result<FrameStatus, String>;
    fn wait_idle(&mut self) {}
}

pub(crate) trait ExitRequester: Send + Sync {
    fn request_exit(&self);
}

pub(crate) struct ScriptThreadConfig {
    pub scene_name: String,
    pub sources: std::collections::BTreeMap<String, String>,
    pub registry: HeavyRegistry,
    pub interrupt: ScriptInterruptHandle,
    pub clock: Arc<dyn Clock>,
    pub background_tick_hz: u32,
    pub control_tx: Sender<ScriptControl>,
    pub control_rx: Receiver<ScriptControl>,
    pub frame_tx: Sender<Arc<FrameSnapshot>>,
    pub render_tx: Sender<RenderControl>,
    pub status: Arc<RuntimeStatus>,
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
    pub stopped_tx: Sender<()>,
    pub exit: Arc<dyn ExitRequester>,
}

#[derive(Debug)]
pub(crate) enum ScriptStartupError {
    Load(String),
    Panic(String),
}

pub(crate) struct VulkanBackend {
    renderer: Renderer,
}

impl VulkanBackend {
    pub fn new(renderer: Renderer) -> Self {
        Self { renderer }
    }
}

impl RenderBackend for VulkanBackend {
    fn resize(&mut self, width: u32, height: u32) {
        self.renderer.resize(width, height);
    }

    fn render(
        &mut self,
        snapshot: &FrameSnapshot,
        width: u32,
        height: u32,
    ) -> Result<FrameStatus, String> {
        let light = DirectionalLight {
            direction: snapshot.light.direction,
            color: snapshot.light.color,
            ambient: snapshot.light.ambient,
        };
        let shadow = snapshot.light.shadow.map(|(center, radius)| {
            shadow_view_projection(Vec3::from_array(snapshot.light.direction), center, radius)
                .to_cols_array_2d()
        });
        let frame = Frame {
            clear_color: CLEAR_COLOR,
            view_projection: snapshot
                .camera
                .view_projection(width, height)
                .to_cols_array_2d(),
            shadow_view_projection: shadow,
            light,
            draws: &snapshot.draws,
            overlay: &[],
        };
        self.renderer
            .render_frame(&frame)
            .map_err(|error| error.to_string())
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
        registry,
        interrupt,
        clock,
        background_tick_hz,
        control_rx,
        frame_tx,
        render_tx,
        ..
    } = config;
    let mut scripts = match ScriptHost::load_with_interrupt(
        &world,
        &scene_name,
        &sources,
        &registry,
        interrupt.clone(),
    ) {
        Ok(scripts) => scripts,
        Err(error) => {
            if let Some(ready_tx) = ready_tx.take() {
                let _ = ready_tx.send(Err(ScriptStartupError::Load(error.to_string())));
            }
            return;
        }
    };
    if let Some(ready_tx) = ready_tx.take()
        && ready_tx.send(Ok(())).is_err()
    {
        return;
    }

    let mut background = false;
    let gpu_scene = loop {
        match control_rx.recv() {
            Ok(ScriptControl::Go(scene)) => break Some(scene),
            Ok(ScriptControl::Shutdown) | Err(_) => break None,
            Ok(ScriptControl::Background(value)) => background = value,
        }
    };
    let Some(gpu_scene) = gpu_scene else {
        scripts.destroy(&mut world, ScriptTime::default());
        print_script_events(scripts.drain_events());
        return;
    };

    let mut frame_number = 0_u64;
    let mut last_tick = clock.now();
    let start_time = ScriptTime {
        elapsed: last_tick.as_secs_f64(),
        frame: frame_number,
    };
    scripts.start(&mut world, start_time);
    print_script_events(scripts.drain_events());
    let interval = Duration::from_secs_f64(1.0 / background_tick_hz.max(1) as f64);
    let now = clock.now();
    let mut next_background_tick =
        now.saturating_add(if background { interval } else { Duration::ZERO });

    loop {
        if background
            && !wait_background_tick(
                &control_rx,
                clock.as_ref(),
                &mut background,
                next_background_tick,
            )
        {
            break;
        }
        if !background {
            match control_rx.try_recv() {
                Ok(ScriptControl::Shutdown) | Err(TryRecvError::Disconnected) => break,
                Ok(ScriptControl::Background(value)) => {
                    background = value;
                    if background {
                        next_background_tick = clock.now().saturating_add(interval);
                        continue;
                    }
                }
                Ok(ScriptControl::Go(_)) | Err(TryRecvError::Empty) => {}
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
        scripts.update(&mut world, dt, time);
        print_script_events(scripts.drain_events());
        if background {
            next_background_tick = now.saturating_add(interval);
            continue;
        }

        let (scene_frame, warnings) = world.build_frame();
        for warning in warnings {
            eprintln!(
                "[scene] {}: объект {}: {}",
                warning.source, warning.object_id, warning.message
            );
        }
        let snapshot = Arc::new(make_snapshot(scene_frame, &gpu_scene));
        match send_snapshot(&frame_tx, &control_rx, snapshot, &mut background) {
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
    let _ = render_tx.send(RenderControl::Shutdown);
}

fn make_snapshot(scene: engine_scene::SceneFrame, gpu_scene: &GpuSceneTable) -> FrameSnapshot {
    let mut draws = Vec::new();
    for instance in scene.meshes {
        if let Some(meshes) = gpu_scene.by_object.get(&instance.object) {
            draws.extend(meshes.iter().map(|(mesh, texture)| DrawCommand {
                mesh: *mesh,
                texture: *texture,
                model: instance.model.to_cols_array_2d(),
            }));
        }
    }
    FrameSnapshot {
        camera: scene.camera,
        light: scene.light,
        draws,
    }
}

fn print_script_events(events: Vec<ScriptEvent>) {
    for event in events {
        match event.level {
            ScriptEventLevel::Info => println!("[script] {event}"),
            ScriptEventLevel::Error => eprintln!("[script] {event}"),
        }
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
            ScriptSelection::Control(Ok(ScriptControl::Go(_))) => {}
        }
    }
}

fn wait_background_tick(
    control_rx: &Receiver<ScriptControl>,
    clock: &dyn Clock,
    background: &mut bool,
    deadline: Duration,
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
        stopped_tx,
        exit,
        ..
    } = config;
    let result = catch_unwind(AssertUnwindSafe(|| {
        render_loop(
            &mut *backend,
            width,
            height,
            frame_limit,
            &frame_rx,
            &control_rx,
            &script_tx,
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

fn render_loop(
    backend: &mut dyn RenderBackend,
    mut width: u32,
    mut height: u32,
    frame_limit: Option<u64>,
    frame_rx: &Receiver<Arc<FrameSnapshot>>,
    control_rx: &Receiver<RenderControl>,
    script_tx: &Sender<ScriptControl>,
) -> Result<(), String> {
    let mut occluded = false;
    let mut paused = false;
    let mut presented = 0_u64;
    loop {
        loop {
            match control_rx.try_recv() {
                Ok(RenderControl::Resize(new_width, new_height)) => {
                    width = new_width;
                    height = new_height;
                    backend.resize(width, height);
                }
                Ok(RenderControl::Occluded(value)) => occluded = value,
                Ok(RenderControl::Shutdown) | Err(TryRecvError::Disconnected) => return Ok(()),
                Err(TryRecvError::Empty) => break,
            }
        }
        let next_paused = occluded || width == 0 || height == 0;
        if next_paused != paused {
            paused = next_paused;
            let _ = script_tx.send(ScriptControl::Background(paused));
        }

        match Selector::new()
            .recv(frame_rx, RenderSelection::Frame)
            .recv(control_rx, RenderSelection::Control)
            .wait()
        {
            RenderSelection::Control(Ok(RenderControl::Resize(new_width, new_height))) => {
                width = new_width;
                height = new_height;
                backend.resize(width, height);
            }
            RenderSelection::Control(Ok(RenderControl::Occluded(value))) => occluded = value,
            RenderSelection::Control(Ok(RenderControl::Shutdown))
            | RenderSelection::Control(Err(_))
            | RenderSelection::Frame(Err(_)) => return Ok(()),
            RenderSelection::Frame(Ok(snapshot)) => {
                let Some(frame_status) = render_snapshot(backend, snapshot, paused, width, height)?
                else {
                    continue;
                };
                match frame_status {
                    FrameStatus::Presented => {
                        presented = presented.saturating_add(1);
                        if frame_limit.is_some_and(|limit| presented >= limit) {
                            return Ok(());
                        }
                    }
                    FrameStatus::Skipped => {}
                }
            }
        }
    }
}

fn render_snapshot(
    backend: &mut dyn RenderBackend,
    snapshot: Arc<FrameSnapshot>,
    paused: bool,
    width: u32,
    height: u32,
) -> Result<Option<FrameStatus>, String> {
    if paused {
        drop(snapshot);
        return Ok(None);
    }
    backend.render(&snapshot, width, height).map(Some)
}

#[cfg(test)]
mod tests {
    use super::{
        Clock, ExitRequester, FrameSnapshot, GpuSceneTable, RenderBackend, RenderControl,
        RenderThreadConfig, RuntimeStatus, SCRIPTS_THREAD, ScriptControl, ScriptStartupError,
        ScriptThreadConfig, render_loop, render_snapshot, render_thread, send_snapshot,
        wait_background_tick,
    };
    use crate::module_host::ModuleHost;
    use crate::pipeline::start_scripts;
    use engine_render::{DrawCommand, FrameStatus};
    use engine_scene::camera::Camera;
    use engine_scene::world::LightState;
    use engine_scene::{SceneDocument, World};
    use engine_script::{
        HeavyRegistry, HeavyScript, ScriptContext, ScriptInterruptHandle, ScriptParams,
    };
    use glam::Vec3;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    #[derive(Default)]
    struct FakeBackend {
        renders: usize,
        shared_renders: Option<Arc<AtomicUsize>>,
        fail: bool,
        panic: bool,
        panic_wait_idle: bool,
    }

    impl RenderBackend for FakeBackend {
        fn resize(&mut self, _width: u32, _height: u32) {}

        fn render(
            &mut self,
            _snapshot: &FrameSnapshot,
            _width: u32,
            _height: u32,
        ) -> Result<FrameStatus, String> {
            self.renders += 1;
            if let Some(renders) = &self.shared_renders {
                renders.fetch_add(1, Ordering::SeqCst);
            }
            assert!(!self.panic, "fake render panic");
            if self.fail {
                Err("fake render failure".to_owned())
            } else {
                Ok(FrameStatus::Presented)
            }
        }

        fn wait_idle(&mut self) {
            assert!(!self.panic_wait_idle, "fake wait_idle panic");
        }
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

    struct DestroyProbe;

    impl HeavyScript for DestroyProbe {
        fn on_destroy(&mut self, _ctx: &mut ScriptContext<'_>) {
            DESTROY_CALLS.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn destroy_probe_factory(_: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
        Ok(Box::new(DestroyProbe))
    }

    struct FakeExit(mpsc::Sender<()>);

    impl ExitRequester for FakeExit {
        fn request_exit(&self) {
            let _ = self.0.send(());
        }
    }

    fn empty_snapshot() -> FrameSnapshot {
        FrameSnapshot {
            camera: Camera {
                position: Vec3::new(0.0, 0.0, 4.0),
                target: Vec3::ZERO,
                up: Vec3::Y,
                fov_narrow: std::f32::consts::FRAC_PI_4,
                near: 0.1,
                far: 100.0,
            },
            light: LightState::default(),
            draws: Vec::<DrawCommand>::new(),
        }
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
        );
        assert!(matches!(result, super::SendOutcome::Shutdown));
        assert_eq!(frame_rx.len(), 1);
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
            )
        });
        control_tx.send(RenderControl::Shutdown).unwrap();
        assert!(worker.join().unwrap().is_ok());
        drop(frame_tx);
        drop(script_rx);
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
        assert!(
            render_snapshot(&mut backend, Arc::new(empty_snapshot()), true, 0, 0)
                .unwrap()
                .is_none()
        );
        assert_eq!(backend.renders, 0);
    }

    #[test]
    fn background_scheduler_uses_injected_clock_and_control_recv_timeout() {
        let clock = Arc::new(FakeClock::default());
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
                );
                if !ticked || !background {
                    break;
                }
                let now = clock_for_wait.now();
                deadline = now + Duration::from_millis(100);
                let _ = done_tx.send(now);
            }
        });
        clock.advance(Duration::from_millis(100));
        control_tx.send(ScriptControl::Background(true)).unwrap();
        assert_eq!(done_rx.recv().unwrap(), Duration::from_millis(100));
        assert!(done_rx.try_recv().is_err());
        clock.advance(Duration::from_millis(100));
        control_tx.send(ScriptControl::Background(true)).unwrap();
        assert_eq!(done_rx.recv().unwrap(), Duration::from_millis(200));
        worker.join().unwrap();
    }

    struct TickProbe;

    impl HeavyScript for TickProbe {
        fn on_update(&mut self, ctx: &mut ScriptContext<'_>, _dt: f32) {
            ctx.transform.position[0] += 1.0;
        }
    }

    fn tick_probe_factory(_: &ScriptParams) -> Result<Box<dyn HeavyScript>, String> {
        Ok(Box::new(TickProbe))
    }

    #[test]
    fn background_before_go_is_kept_and_ticks_without_sending_snapshots() {
        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\nfov_deg = 60.0\nnear = 0.1\nfar = 100.0\n[[objects.heavy_scripts]]\nname = \"TickProbe\"\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let mut registry = HeavyRegistry::new();
        registry.register("TickProbe", tick_probe_factory).unwrap();
        let mut modules = ModuleHost::new();
        let clock = Arc::new(FakeClock::default());
        let waiting_rx = clock.observe_waits();
        let (script_tx, script_rx) = flume::unbounded();
        let (frame_tx, frame_rx) = flume::bounded(1);
        let (render_tx, render_rx) = flume::unbounded();
        let script_thread = start_scripts(
            &mut modules,
            world,
            ScriptThreadConfig {
                scene_name: "startup.rvescene".to_owned(),
                sources: BTreeMap::new(),
                registry,
                interrupt: ScriptInterruptHandle::new(),
                clock: clock.clone(),
                background_tick_hz: 1,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
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
        waiting_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(frame_rx.try_recv().is_err());
        script_tx.send(ScriptControl::Background(false)).unwrap();

        let snapshot = frame_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(snapshot.camera.position.x, 2.0);
        script_tx.send(ScriptControl::Shutdown).unwrap();
        script_thread
            .stopped
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert_eq!(render_rx.recv().unwrap(), RenderControl::Shutdown);
        modules.join_named(SCRIPTS_THREAD).unwrap();
        assert!(modules.shutdown().is_empty());
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
                &mut false
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
            "[[assets]]\npath = \"scripts/bad.lua\"\n[[objects]]\nid = \"camera\"\n[objects.camera]\nfov_deg = 60.0\nnear = 0.1\nfar = 100.0\n[[objects]]\nid = \"scripted\"\n[[objects.lite_scripts]]\npath = \"scripts/bad.lua\"\n",
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
                registry: HeavyRegistry::new(),
                interrupt,
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                control_tx: script_tx,
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
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
    fn graceful_script_shutdown_runs_on_destroy_before_interrupt() {
        DESTROY_CALLS.store(0, Ordering::SeqCst);
        let document = SceneDocument::parse(
            "startup.rvescene",
            "[[objects]]\nid = \"camera\"\n[objects.camera]\nfov_deg = 60.0\nnear = 0.1\nfar = 100.0\n[[objects.heavy_scripts]]\nname = \"DestroyProbe\"\n",
        )
        .unwrap();
        let world = World::from_document(&document).unwrap();
        let mut registry = HeavyRegistry::new();
        registry
            .register("DestroyProbe", destroy_probe_factory)
            .unwrap();
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
                registry,
                interrupt: interrupt.clone(),
                clock: Arc::new(FakeClock::default()),
                background_tick_hz: 30,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::new(RuntimeStatus::default()),
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
            "[[objects]]\nid = \"camera\"\n[objects.camera]\nfov_deg = 60.0\nnear = 0.1\nfar = 100.0\n",
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
                registry: HeavyRegistry::new(),
                interrupt: ScriptInterruptHandle::new(),
                clock: Arc::new(PanicClock),
                background_tick_hz: 30,
                control_tx: script_tx.clone(),
                control_rx: script_rx,
                frame_tx,
                render_tx,
                status: Arc::clone(&status),
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
