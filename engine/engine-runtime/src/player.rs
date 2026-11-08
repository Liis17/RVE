use crate::activation::PendingActivation;
#[cfg(feature = "dev-console")]
use crate::console::{CommandCatalogSnapshot, ConsoleUiFrame, ConsoleUiSlot, snapshot_lines};
use crate::cursor::CursorControl;
#[cfg(feature = "dev-console")]
use crate::dev_overlays::DevOverlays;
use crate::input::{InputAccumulator, lock_input};
use crate::module_host::ModuleHost;
use crate::pipeline::{
    ExitRequester, FrameSnapshot, RENDER_THREAD, RenderControl, RenderThreadConfig, RuntimeStatus,
    SCRIPTS_THREAD, ScriptControl, VulkanBackend, start_render,
};
use crate::prefabs::{PREFAB_LOADER_THREAD, RenderResourceRequest};
use crate::profiling::StartupProfile;
#[cfg(feature = "dev-console")]
use crate::profiling::profile_command_lines;
use crate::scene::{PreparedRenderAssets, upload_scene};
#[cfg(feature = "dev-console")]
use egui::{Context, Rect, ViewportId};
#[cfg(feature = "dev-console")]
use egui_winit::State as EguiWinitState;
#[cfg(feature = "dev-console")]
use engine_console::{CommandLine, ConsoleBuffer, ConsoleLevel, ConsoleLine, parse_command_line};
use engine_project::BootstrapPresentMode;
use engine_render::{DebugView, PresentMode, Renderer, RendererConfig, UpscalerConfig};
use engine_resources::ResourceHandle;
use engine_script::ScriptInterruptHandle;
#[cfg(feature = "dev-console")]
use engine_ui::console::ConsoleOverlay;
use engine_window::{Key, MouseButton, Window, WindowConfig, WindowHandler, WindowProxy, run};
use flume::{Receiver, Sender};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
#[cfg(feature = "dev-console")]
use winit::event::{ElementState, WindowEvent};
#[cfg(feature = "dev-console")]
use winit::keyboard::{KeyCode, PhysicalKey};

const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
const PREFAB_LOADER_STOP_TIMEOUT: Duration = Duration::from_secs(2);

struct WindowExit(WindowProxy);

impl ExitRequester for WindowExit {
    fn request_exit(&self) {
        self.0.request_exit();
    }
}

pub(crate) struct PlayerConfig {
    pub window: WindowConfig,
    pub present_mode: Option<BootstrapPresentMode>,
    pub pipeline_cache_path: Option<PathBuf>,
    pub fsr_library: Option<PathBuf>,
    pub dlss_library: Option<PathBuf>,
    pub dlss_project_id: Option<String>,
    pub dlss_application_id: Option<u64>,
    pub dlss_engine_version: Option<String>,
    pub pipeline_cache: Vec<u8>,
    pub frame_limit: Option<u64>,
    pub debug_view: DebugView,
    pub render_assets: PreparedRenderAssets,
    pub activation: PendingActivation,
    pub cursor: CursorControl,
    pub script_tx: Sender<ScriptControl>,
    pub render_tx: Sender<RenderControl>,
    pub render_rx: Receiver<RenderControl>,
    pub frame_rx: Receiver<Arc<FrameSnapshot>>,
    pub resources: Receiver<RenderResourceRequest>,
    pub script_stopped_rx: std::sync::mpsc::Receiver<()>,
    pub prefab_loader_stopped_rx: std::sync::mpsc::Receiver<()>,
    pub interrupt: ScriptInterruptHandle,
    pub status: Arc<RuntimeStatus>,
    pub resource_stats: ResourceHandle,
    pub startup: StartupProfile,
    pub input: Arc<Mutex<InputAccumulator>>,
    #[cfg(feature = "dev-console")]
    pub console_buffer: Arc<Mutex<ConsoleBuffer>>,
    #[cfg(feature = "dev-console")]
    pub command_catalog: Arc<Mutex<CommandCatalogSnapshot>>,
    #[cfg(feature = "dev-console")]
    pub console_ui: Arc<ConsoleUiSlot>,
}

pub(crate) fn run_window(config: PlayerConfig, modules: &mut ModuleHost) -> u8 {
    let PlayerConfig {
        window,
        present_mode,
        pipeline_cache_path,
        fsr_library,
        dlss_library,
        dlss_project_id,
        dlss_application_id,
        dlss_engine_version,
        pipeline_cache,
        frame_limit,
        debug_view,
        render_assets,
        activation,
        cursor,
        script_tx,
        render_tx,
        render_rx,
        frame_rx,
        resources,
        script_stopped_rx,
        prefab_loader_stopped_rx,
        interrupt,
        status,
        resource_stats,
        startup,
        input,
        #[cfg(feature = "dev-console")]
        console_buffer,
        #[cfg(feature = "dev-console")]
        command_catalog,
        #[cfg(feature = "dev-console")]
        console_ui,
    } = config;
    let mut handler = PlayerWindow {
        frame_limit,
        debug_view,
        #[cfg(feature = "dev-console")]
        wireframe_supported: false,
        #[cfg(feature = "dev-console")]
        f8_down: false,
        present_mode,
        pipeline_cache_path,
        fsr_library,
        dlss_library,
        dlss_project_id,
        dlss_application_id,
        dlss_engine_version,
        pipeline_cache,
        render_assets,
        modules,
        activation,
        cursor,
        script_tx,
        render_tx,
        render_rx,
        frame_rx: Some(frame_rx),
        resource_rx: Some(resources),
        script_stopped_rx,
        prefab_loader_stopped_rx: Some(prefab_loader_stopped_rx),
        interrupt,
        status: Arc::clone(&status),
        resource_stats,
        startup,
        render_stopped_rx: None,
        close_deadline: None,
        window_exit: None,
        render_started: false,
        cleaned: false,
        input,
        #[cfg(feature = "dev-console")]
        console_buffer,
        #[cfg(feature = "dev-console")]
        command_catalog,
        #[cfg(feature = "dev-console")]
        console_ui,
        #[cfg(feature = "dev-console")]
        console_overlay: ConsoleOverlay::default(),
        #[cfg(feature = "dev-console")]
        overlays: DevOverlays::default(),
        #[cfg(feature = "dev-console")]
        egui_state: None,
        #[cfg(feature = "dev-console")]
        window_focused: false,
        #[cfg(feature = "dev-console")]
        window_occluded: false,
        #[cfg(feature = "dev-console")]
        console_needs_redraw: false,
        #[cfg(feature = "dev-console")]
        console_repaint_at: None,
        #[cfg(feature = "dev-console")]
        seen_console_generation: None,
        #[cfg(feature = "dev-console")]
        seen_catalog_generation: None,
    };
    let run_result = run(window, &mut handler);
    handler.cleanup();
    if let Err(error) = run_result {
        status.record_failure(format!("ошибка оконного цикла: {error}"));
    }
    if let Some(message) = status.failure_message() {
        log::error!("Ошибка runtime: {message}");
    }
    status.exit_code()
}

struct PlayerWindow<'a> {
    frame_limit: Option<u64>,
    present_mode: Option<BootstrapPresentMode>,
    pipeline_cache_path: Option<PathBuf>,
    fsr_library: Option<PathBuf>,
    dlss_library: Option<PathBuf>,
    dlss_project_id: Option<String>,
    dlss_application_id: Option<u64>,
    dlss_engine_version: Option<String>,
    pipeline_cache: Vec<u8>,
    render_assets: PreparedRenderAssets,
    modules: &'a mut ModuleHost,
    activation: PendingActivation,
    cursor: CursorControl,
    script_tx: Sender<ScriptControl>,
    render_tx: Sender<RenderControl>,
    render_rx: Receiver<RenderControl>,
    frame_rx: Option<Receiver<Arc<FrameSnapshot>>>,
    resource_rx: Option<Receiver<RenderResourceRequest>>,
    script_stopped_rx: std::sync::mpsc::Receiver<()>,
    prefab_loader_stopped_rx: Option<std::sync::mpsc::Receiver<()>>,
    interrupt: ScriptInterruptHandle,
    status: Arc<RuntimeStatus>,
    resource_stats: ResourceHandle,
    startup: StartupProfile,
    render_stopped_rx: Option<Receiver<()>>,
    close_deadline: Option<Instant>,
    window_exit: Option<Arc<dyn ExitRequester>>,
    render_started: bool,
    cleaned: bool,
    input: Arc<Mutex<InputAccumulator>>,
    debug_view: DebugView,
    #[cfg(feature = "dev-console")]
    wireframe_supported: bool,
    #[cfg(feature = "dev-console")]
    f8_down: bool,
    #[cfg(feature = "dev-console")]
    console_buffer: Arc<Mutex<ConsoleBuffer>>,
    #[cfg(feature = "dev-console")]
    command_catalog: Arc<Mutex<CommandCatalogSnapshot>>,
    #[cfg(feature = "dev-console")]
    console_ui: Arc<ConsoleUiSlot>,
    #[cfg(feature = "dev-console")]
    console_overlay: ConsoleOverlay,
    #[cfg(feature = "dev-console")]
    overlays: DevOverlays,
    #[cfg(feature = "dev-console")]
    egui_state: Option<EguiWinitState>,
    #[cfg(feature = "dev-console")]
    window_focused: bool,
    #[cfg(feature = "dev-console")]
    window_occluded: bool,
    #[cfg(feature = "dev-console")]
    console_needs_redraw: bool,
    #[cfg(feature = "dev-console")]
    console_repaint_at: Option<Instant>,
    #[cfg(feature = "dev-console")]
    seen_console_generation: Option<u64>,
    #[cfg(feature = "dev-console")]
    seen_catalog_generation: Option<u64>,
}

#[cfg(feature = "dev-console")]
fn new_egui_state(window: &Window) -> EguiWinitState {
    EguiWinitState::new(
        Context::default(),
        ViewportId::ROOT,
        window,
        Some(window.scale_factor() as f32),
        window.theme(),
        None,
    )
}

impl PlayerWindow<'_> {
    fn fail_init(&mut self, message: impl Into<String>) -> bool {
        self.status.record_failure(message);
        self.interrupt.interrupt();
        let _ = self.script_tx.send(ScriptControl::Shutdown);
        let _ = self.render_tx.send(RenderControl::Shutdown);
        false
    }

    /// Останавливает загрузчик префабов раньше render thread: его результаты уходят в render,
    /// поэтому сначала он завершается, а потом останавливается рендер.
    fn stop_prefab_loader(&mut self) {
        let Some(stopped) = self.prefab_loader_stopped_rx.take() else {
            return;
        };
        self.modules.stop_named(PREFAB_LOADER_THREAD);
        wait_for_prefab_loader_or_detach(
            self.modules,
            &stopped,
            &self.status,
            PREFAB_LOADER_STOP_TIMEOUT,
        );
    }

    fn cleanup(&mut self) {
        if self.cleaned {
            return;
        }
        self.cleaned = true;
        let _ = self.script_tx.send(ScriptControl::Shutdown);
        self.stop_prefab_loader();

        if self.render_started && !self.status.render_stopped() {
            let _ = self.render_tx.send(RenderControl::Shutdown);
            if let Some(receiver) = &self.render_stopped_rx {
                let timeout = self
                    .close_deadline
                    .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                    .unwrap_or(CLOSE_TIMEOUT);
                wait_for_render_or_detach(self.modules, &self.status, receiver, timeout);
            }
        }

        cleanup_threads(
            self.modules,
            self.status.render_stopped(),
            &self.script_stopped_rx,
            &self.interrupt,
            &self.status,
            Duration::from_secs(2),
        );
    }

    #[cfg(feature = "dev-console")]
    fn set_console_open(&mut self, open: bool, window: &Window) {
        if self.overlays.console_open() == open {
            return;
        }
        self.console_overlay.set_open(open);
        self.overlays = self.overlays.with_console(open);
        self.cursor.set_console_open(open);
        self.apply_overlays(window);
    }

    #[cfg(feature = "dev-console")]
    fn toggle_profile(&mut self, window: &Window) {
        self.overlays = self.overlays.toggled_profile();
        self.apply_overlays(window);
    }

    /// Приводит egui-состояние, ввод и последний кадр оверлеев к текущему [`DevOverlays`].
    #[cfg(feature = "dev-console")]
    fn apply_overlays(&mut self, window: &Window) {
        let plan = self.overlays.plan();
        self.console_repaint_at = None;
        lock_input(&self.input).console_open(plan.blocks_game_input, self.window_focused);
        if plan.egui_context {
            if self.egui_state.is_none() {
                self.egui_state = Some(new_egui_state(window));
            }
            self.console_needs_redraw = true;
        } else {
            self.console_needs_redraw = false;
            self.egui_state = None;
            self.publish_console_ui(ConsoleUiFrame::default());
        }
    }

    #[cfg(feature = "dev-console")]
    fn publish_console_ui(&self, frame: ConsoleUiFrame) {
        if self.console_ui.publish(frame) {
            let _ = self.render_tx.send(RenderControl::ConsoleUiChanged);
        }
    }

    #[cfg(feature = "dev-console")]
    fn update_console(&mut self, window: &Window) {
        if !self.overlays.plan().egui_context {
            return;
        }
        if self.window_occluded {
            self.console_ui.discard_geometry_preserve_texture_delta();
            return;
        }
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            self.console_ui.discard_geometry_preserve_texture_delta();
            return;
        }

        let console_generation = self
            .console_buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation();
        let catalog_generation = self
            .command_catalog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .generation;
        if self.seen_console_generation != Some(console_generation)
            || self.seen_catalog_generation != Some(catalog_generation)
        {
            self.console_needs_redraw = true;
        }
        if self
            .console_repaint_at
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.console_needs_redraw = true;
        }
        if !self.console_needs_redraw {
            return;
        }

        let Some(state) = self.egui_state.as_mut() else {
            return;
        };
        let context = state.egui_ctx().clone();
        let raw_input = state.take_egui_input(window);
        let screen_rect = raw_input.screen_rect.unwrap_or_else(|| {
            let scale = egui_winit::pixels_per_point(&context, window);
            Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(size.width as f32 / scale, size.height as f32 / scale),
            )
        });
        let lines = snapshot_lines(&self.console_buffer);
        let catalog = self
            .command_catalog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .catalog
            .clone();
        let mut console_output = Default::default();
        let profile_open = self.overlays.profile_open();
        let profile_reports: Vec<Arc<engine_core::profile::ProfileReport>> = if profile_open {
            self.status
                .profile
                .reports()
                .into_iter()
                .map(|(_, report)| report)
                .collect()
        } else {
            Vec::new()
        };
        let console_open = self.overlays.console_open();
        let output = context.run_ui(raw_input, |_ui| {
            if console_open {
                console_output = self
                    .console_overlay
                    .show(&context, screen_rect, &lines, &catalog);
            }
            if profile_open {
                engine_ui::profiler::overlay(&context, &profile_reports);
            }
        });
        if console_output.toggled {
            self.set_console_open(self.console_overlay.is_open(), window);
        }
        let egui::FullOutput {
            platform_output,
            textures_delta,
            shapes,
            pixels_per_point,
            viewport_output,
        } = output;
        let repaint_delay = viewport_output
            .get(&ViewportId::ROOT)
            .map(|viewport| viewport.repaint_delay);
        if let Some(delay) = repaint_delay {
            self.console_repaint_at = Instant::now().checked_add(delay);
        }
        if self.overlays.profile_open() {
            let refresh = Instant::now() + crate::profiling::PUBLISH_INTERVAL;
            self.console_repaint_at = Some(
                self.console_repaint_at
                    .map_or(refresh, |deadline| deadline.min(refresh)),
            );
        }
        if let Some(state) = self.egui_state.as_mut() {
            state.handle_platform_output(window, platform_output);
        }

        let visible = self.overlays.visible();
        let frame = ConsoleUiFrame {
            visible,
            primitives: if visible {
                context.tessellate(shapes, pixels_per_point)
            } else {
                Vec::new()
            },
            textures_delta,
            pixels_per_point,
        };
        self.console_needs_redraw = false;
        self.seen_console_generation = Some(console_generation);
        self.seen_catalog_generation = Some(catalog_generation);
        self.publish_console_ui(frame);

        if let Some(command) = console_output.submitted {
            self.submit_console_command(command);
            self.console_needs_redraw = true;
            self.console_repaint_at = Some(Instant::now());
        }
    }

    #[cfg(feature = "dev-console")]
    fn submit_console_command(&mut self, text: String) {
        let command = match parse_command_line(&text) {
            Ok(command) => command,
            Err(error) => {
                push_console_line(
                    &self.console_buffer,
                    ConsoleLevel::Info,
                    "console",
                    format!("> {text}"),
                );
                push_console_line(
                    &self.console_buffer,
                    ConsoleLevel::Error,
                    "console",
                    error.to_string(),
                );
                return;
            }
        };
        if command.name == "clear" {
            if let Err(error) = validate_builtin_args(&command, 0, "clear") {
                push_console_line(
                    &self.console_buffer,
                    ConsoleLevel::Info,
                    "console",
                    format!("> {text}"),
                );
                push_console_line(&self.console_buffer, ConsoleLevel::Error, "console", error);
                return;
            }
            self.console_buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clear();
            push_console_line(
                &self.console_buffer,
                ConsoleLevel::Info,
                "console",
                format!("> {text}"),
            );
            return;
        }
        push_console_line(
            &self.console_buffer,
            ConsoleLevel::Info,
            "console",
            format!("> {text}"),
        );
        match command.name.as_str() {
            "profile" => {
                if let Err(error) = validate_builtin_args(&command, 0, "profile") {
                    push_console_line(&self.console_buffer, ConsoleLevel::Error, "console", error);
                    return;
                }
                for line in profile_command_lines(&self.status.profile.reports()) {
                    push_console_line(&self.console_buffer, ConsoleLevel::Info, "profile", line);
                }
            }
            "help" => {
                if let Err(error) = validate_builtin_args(&command, 1, "help [prefix]") {
                    push_console_line(&self.console_buffer, ConsoleLevel::Error, "console", error);
                    return;
                }
                let catalog = self
                    .command_catalog
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .catalog
                    .clone();
                let matches = catalog.help(command.args.first().map_or("", String::as_str));
                if matches.is_empty() {
                    push_console_line(
                        &self.console_buffer,
                        ConsoleLevel::Info,
                        "console",
                        "no commands match the prefix",
                    );
                } else {
                    for line in matches {
                        push_console_line(
                            &self.console_buffer,
                            ConsoleLevel::Info,
                            "console",
                            line,
                        );
                    }
                }
            }
            _ => {
                if self
                    .script_tx
                    .send(ScriptControl::Console(command))
                    .is_err()
                {
                    push_console_line(
                        &self.console_buffer,
                        ConsoleLevel::Error,
                        "console",
                        "script thread is not available",
                    );
                }
            }
        }
    }
}

#[cfg(feature = "dev-console")]
fn validate_builtin_args(
    command: &CommandLine,
    max_args: usize,
    usage: &str,
) -> Result<(), String> {
    if command.args.len() <= max_args {
        Ok(())
    } else {
        Err(format!("usage: {usage}"))
    }
}

#[cfg(feature = "dev-console")]
fn push_console_line(
    buffer: &Arc<Mutex<ConsoleBuffer>>,
    level: ConsoleLevel,
    source: &str,
    text: impl Into<String>,
) {
    let line = ConsoleLine {
        level,
        source: source.to_owned(),
        text: text.into(),
    };
    buffer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(line);
}

trait RuntimeThreads {
    fn join_named(&mut self, name: &str) -> Result<(), String>;
    fn detach(&mut self, name: &str) -> bool;
}

impl RuntimeThreads for ModuleHost {
    fn join_named(&mut self, name: &str) -> Result<(), String> {
        ModuleHost::join_named(self, name)
    }

    fn detach(&mut self, name: &str) -> bool {
        ModuleHost::detach(self, name)
    }
}

fn cleanup_threads(
    modules: &mut impl RuntimeThreads,
    render_stopped: bool,
    stopped: &std::sync::mpsc::Receiver<()>,
    interrupt: &ScriptInterruptHandle,
    status: &RuntimeStatus,
    timeout: Duration,
) {
    if render_stopped && let Err(error) = modules.join_named(RENDER_THREAD) {
        status.record_failure(error);
    }
    if wait_for_scripts_or_detach(modules, stopped, interrupt, status, timeout)
        && let Err(error) = modules.join_named(SCRIPTS_THREAD)
    {
        status.record_failure(error);
    }
}

fn wait_for_scripts_or_detach(
    modules: &mut impl RuntimeThreads,
    stopped: &std::sync::mpsc::Receiver<()>,
    interrupt: &ScriptInterruptHandle,
    status: &RuntimeStatus,
    timeout: Duration,
) -> bool {
    match stopped.recv_timeout(timeout) {
        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            log::error!("поток скриптов не остановился за {timeout:?}; прерываю Lua");
            status.record_failure(format!("поток скриптов не остановился за {timeout:?}"));
            interrupt.interrupt();
            match stopped.recv_timeout(timeout) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    log::error!(
                        "поток скриптов не завершился после прерывания за {timeout:?}; отсоединяю его"
                    );
                    status.record_failure(format!(
                        "поток скриптов не завершился после прерывания за {timeout:?}"
                    ));
                    modules.detach(SCRIPTS_THREAD);
                    false
                }
            }
        }
    }
}

/// Ждёт завершения загрузчика не дольше `timeout`. Зависший при штатном закрытии загрузчик
/// отсоединяется с предупреждением: его результаты уже не нужны, а код выхода не меняется.
/// Код 1 дает только паника потока, которую показывает `join`.
fn wait_for_prefab_loader_or_detach(
    modules: &mut impl RuntimeThreads,
    stopped: &std::sync::mpsc::Receiver<()>,
    status: &RuntimeStatus,
    timeout: Duration,
) {
    match stopped.recv_timeout(timeout) {
        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            if let Err(error) = modules.join_named(PREFAB_LOADER_THREAD) {
                status.record_failure(error);
            }
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            log::warn!("поток загрузки префабов не остановился за {timeout:?}; отсоединяю его");
            modules.detach(PREFAB_LOADER_THREAD);
        }
    }
}

fn wait_for_render_or_detach(
    modules: &mut impl RuntimeThreads,
    status: &RuntimeStatus,
    stopped: &Receiver<()>,
    timeout: Duration,
) -> bool {
    if stopped.recv_timeout(timeout).is_ok() || status.render_stopped() {
        return true;
    }
    log::error!("поток рендера не остановился за 2 секунды; отсоединяю его");
    status.record_failure("поток рендера не остановился за 2 секунды".to_owned());
    modules.detach(RENDER_THREAD);
    false
}

impl WindowHandler for PlayerWindow<'_> {
    fn attach_proxy(&mut self, proxy: WindowProxy) {
        self.activation.attach(proxy.clone());
        self.cursor.attach(proxy.clone());
        self.window_exit = Some(Arc::new(WindowExit(proxy)));
    }

    #[cfg(feature = "dev-console")]
    fn window_event(&mut self, window: &Window, event: &WindowEvent) {
        #[cfg(feature = "dev-console")]
        if let WindowEvent::KeyboardInput { event, .. } = event
            && event.physical_key == PhysicalKey::Code(KeyCode::F3)
        {
            if event.state == ElementState::Pressed && !event.repeat {
                self.toggle_profile(window);
            }
            return;
        }
        if let WindowEvent::KeyboardInput { event, .. } = event
            && event.physical_key == PhysicalKey::Code(KeyCode::Backquote)
        {
            if event.state == ElementState::Pressed && !event.repeat {
                self.set_console_open(!self.overlays.console_open(), window);
            }
            return;
        }
        if self.overlays.console_open()
            && self
                .egui_state
                .as_mut()
                .is_some_and(|state| state.on_window_event(window, event).repaint)
        {
            self.console_needs_redraw = true;
        }
    }

    fn next_wakeup(&mut self) -> Option<Instant> {
        #[cfg(feature = "dev-console")]
        if self.overlays.visible()
            && let Some(console_wakeup) = self.console_repaint_at
        {
            return Some(
                self.close_deadline
                    .map_or(console_wakeup, |current| current.min(console_wakeup)),
            );
        }
        self.close_deadline
    }

    fn init(&mut self, window: Arc<Window>) -> bool {
        #[cfg(feature = "dev-console")]
        {
            self.egui_state = Some(new_egui_state(window.as_ref()));
            self.window_focused = window.has_focus();
            lock_input(&self.input).console_open(false, self.window_focused);
        }
        let size = window.inner_size();
        let phase_started = Instant::now();
        let mut renderer = match Renderer::new(
            Arc::clone(&window),
            size.width,
            size.height,
            RendererConfig {
                present_mode: self
                    .present_mode
                    .map(|mode| match mode {
                        BootstrapPresentMode::Fifo => PresentMode::Fifo,
                        BootstrapPresentMode::Mailbox => PresentMode::Mailbox,
                        BootstrapPresentMode::Immediate => PresentMode::Immediate,
                    })
                    .unwrap_or_default(),
                pipeline_cache: self.pipeline_cache.clone(),
                upscalers: UpscalerConfig {
                    fsr_library: self.fsr_library.clone(),
                    dlss_library: self.dlss_library.clone(),
                    dlss_project_id: self.dlss_project_id.clone(),
                    dlss_application_id: self.dlss_application_id,
                    dlss_engine_version: self.dlss_engine_version.clone(),
                },
                ..RendererConfig::default()
            },
        ) {
            Ok(renderer) => renderer,
            Err(error) => return self.fail_init(format!("ошибка renderer: {error}")),
        };
        self.startup.record("renderer", phase_started.elapsed());
        #[cfg(feature = "dev-console")]
        {
            self.wireframe_supported = renderer.capabilities().wireframe;
        }
        let phase_started = Instant::now();
        let gpu_scene = match upload_scene(&mut renderer, &self.render_assets) {
            Ok(scene) => scene,
            Err(error) => return self.fail_init(format!("ошибка загрузки GPU-ресурсов: {error}")),
        };
        self.startup.record("GPU upload", phase_started.elapsed());
        self.startup.publish(&self.status.profile);
        let Some(frame_rx) = self.frame_rx.take() else {
            return self.fail_init("канал кадров отсутствует при запуске рендера");
        };
        let Some(resource_rx) = self.resource_rx.take() else {
            return self.fail_init("канал ресурсов отсутствует при запуске рендера");
        };
        let Some(window_exit) = self.window_exit.clone() else {
            return self.fail_init("WindowProxy не прикреплён перед init");
        };
        let (render_stopped_tx, render_stopped_rx) = flume::bounded(1);
        self.render_stopped_rx = Some(render_stopped_rx);
        let backend =
            VulkanBackend::new(renderer, self.pipeline_cache_path.clone(), self.debug_view);
        #[cfg(feature = "dev-console")]
        let backend = backend.with_console(Arc::clone(&self.console_ui));
        if let Err(error) = start_render(
            self.modules,
            RenderThreadConfig {
                backend: Box::new(backend),
                width: size.width,
                height: size.height,
                frame_limit: self.frame_limit,
                frame_rx,
                resources: resource_rx,
                control_tx: self.render_tx.clone(),
                control_rx: self.render_rx.clone(),
                script_tx: self.script_tx.clone(),
                interrupt: self.interrupt.clone(),
                status: Arc::clone(&self.status),
                resource_stats: Some(self.resource_stats.clone()),
                stopped_tx: render_stopped_tx,
                exit: window_exit,
            },
        ) {
            return self.fail_init(format!("не удалось запустить поток рендера: {error}"));
        }
        self.render_started = true;
        if self.script_tx.send(ScriptControl::Go(Box::new(gpu_scene))).is_err() {
            self.status
                .record_failure("поток скриптов завершился до запуска сцены");
            let _ = self.render_tx.send(RenderControl::Shutdown);
            return false;
        }
        true
    }

    fn frame(&mut self, _window: &Window) -> bool {
        true
    }

    fn update(&mut self, _window: &Window) -> bool {
        if close_timed_out(
            Instant::now(),
            self.close_deadline,
            self.status.render_stopped(),
        ) {
            self.status
                .record_failure("поток рендера не завершился после закрытия окна");
            false
        } else {
            #[cfg(feature = "dev-console")]
            self.update_console(_window);
            true
        }
    }

    fn resized(&mut self, width: u32, height: u32) {
        let _ = self.render_tx.send(RenderControl::Resize(width, height));
        #[cfg(feature = "dev-console")]
        if width > 0 && height > 0 && self.overlays.visible() && !self.window_occluded {
            self.console_needs_redraw = true;
        }
    }

    fn close_requested(&mut self) -> bool {
        self.close_deadline
            .get_or_insert_with(|| Instant::now() + CLOSE_TIMEOUT);
        let _ = self.script_tx.send(ScriptControl::Shutdown);
        let _ = self.render_tx.send(RenderControl::Shutdown);
        false
    }

    fn occluded(&mut self, occluded: bool) {
        let _ = self.render_tx.send(RenderControl::Occluded(occluded));
        #[cfg(feature = "dev-console")]
        {
            self.window_occluded = occluded;
            if occluded {
                self.console_ui.discard_geometry_preserve_texture_delta();
            } else if self.overlays.visible() {
                self.console_needs_redraw = true;
            }
        }
    }

    fn key(&mut self, key: Key, pressed: bool) {
        #[cfg(feature = "dev-console")]
        if key == Key::F8 {
            if pressed && !self.f8_down {
                self.f8_down = true;
                if !self.overlays.console_open() && self.window_focused {
                    self.debug_view = next_debug_view(self.debug_view, self.wireframe_supported);
                    let _ = self
                        .render_tx
                        .send(RenderControl::DebugView(self.debug_view));
                }
            } else if !pressed {
                self.f8_down = false;
            }
            return;
        }
        #[cfg(feature = "dev-console")]
        if self.overlays.console_open() {
            return;
        }
        lock_input(&self.input).key(key, pressed);
    }

    fn mouse_button(&mut self, button: MouseButton, pressed: bool) {
        #[cfg(feature = "dev-console")]
        if self.overlays.console_open() {
            return;
        }
        lock_input(&self.input).mouse_button(button, pressed);
    }

    fn cursor_moved(&mut self, x: f64, y: f64) {
        #[cfg(feature = "dev-console")]
        if self.overlays.console_open() {
            return;
        }
        lock_input(&self.input).cursor_moved(x, y);
    }

    fn mouse_motion(&mut self, dx: f64, dy: f64) {
        #[cfg(feature = "dev-console")]
        if self.overlays.console_open() {
            return;
        }
        lock_input(&self.input).mouse_motion(dx, dy);
    }

    fn mouse_wheel(&mut self, dx: f32, dy: f32) {
        #[cfg(feature = "dev-console")]
        if self.overlays.console_open() {
            return;
        }
        lock_input(&self.input).mouse_wheel(dx, dy);
    }

    fn focused(&mut self, focused: bool) {
        #[cfg(feature = "dev-console")]
        {
            self.window_focused = focused;
            if !focused {
                self.f8_down = false;
            }
            lock_input(&self.input).console_open(self.overlays.console_open(), self.window_focused);
        }
        #[cfg(not(feature = "dev-console"))]
        lock_input(&self.input).focused(focused);
    }

    fn exiting(&mut self) {
        self.cleanup();
    }
}

fn close_timed_out(now: Instant, deadline: Option<Instant>, render_stopped: bool) -> bool {
    !render_stopped && deadline.is_some_and(|deadline| now >= deadline)
}

#[cfg(feature = "dev-console")]
fn next_debug_view(current: DebugView, wireframe_supported: bool) -> DebugView {
    let views = [
        DebugView::Lit,
        DebugView::Wireframe,
        DebugView::Normals,
        DebugView::Overdraw,
        DebugView::LodLevels,
        DebugView::ShadowCascades,
    ];
    let mut index = views.iter().position(|view| *view == current).unwrap_or(0);
    for _ in 0..views.len() {
        index = (index + 1) % views.len();
        let candidate = views[index];
        if candidate != DebugView::Wireframe || wireframe_supported {
            return candidate;
        }
    }
    DebugView::Lit
}

#[cfg(test)]
#[cfg(feature = "dev-console")]
mod debug_view_tests {
    use super::next_debug_view;
    use engine_render::DebugView;

    #[test]
    fn debug_view_cycle_skips_wireframe_when_the_device_lacks_support() {
        assert_eq!(next_debug_view(DebugView::Lit, false), DebugView::Normals);
        assert_eq!(
            next_debug_view(DebugView::ShadowCascades, false),
            DebugView::Lit
        );
    }

    #[test]
    fn debug_view_cycle_includes_wireframe_when_supported() {
        assert_eq!(next_debug_view(DebugView::Lit, true), DebugView::Wireframe);
        assert_eq!(
            next_debug_view(DebugView::Wireframe, true),
            DebugView::Normals
        );
        let cycle = [
            DebugView::Wireframe,
            DebugView::Normals,
            DebugView::Overdraw,
            DebugView::LodLevels,
            DebugView::ShadowCascades,
            DebugView::Lit,
        ];
        let mut current = DebugView::Lit;
        for expected in cycle {
            current = next_debug_view(current, true);
            assert_eq!(current, expected);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RuntimeThreads, cleanup_threads, close_timed_out, wait_for_prefab_loader_or_detach,
        wait_for_render_or_detach, wait_for_scripts_or_detach,
    };
    use crate::module_host::ModuleHost;
    use crate::pipeline::RuntimeStatus;
    use engine_script::ScriptInterruptHandle;
    use flume::bounded;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    #[test]
    fn close_timeout_only_expires_when_renderer_is_still_live() {
        let now = Instant::now();
        assert!(!close_timed_out(now, None, false));
        assert!(!close_timed_out(
            now,
            Some(now + Duration::from_millis(1)),
            false
        ));
        assert!(close_timed_out(now, Some(now), false));
        assert!(!close_timed_out(now, Some(now), true));
    }

    #[derive(Default)]
    struct FakeThreads(Vec<String>);

    impl RuntimeThreads for FakeThreads {
        fn join_named(&mut self, name: &str) -> Result<(), String> {
            self.0.push(format!("join:{name}"));
            Ok(())
        }

        fn detach(&mut self, name: &str) -> bool {
            self.0.push(format!("detach:{name}"));
            true
        }
    }

    #[test]
    fn unresponsive_prefab_loader_is_detached_without_failing_the_run() {
        let (_stopped_tx, stopped_rx) = mpsc::channel::<()>();
        let status = RuntimeStatus::default();
        let mut modules = FakeThreads(Vec::new());
        wait_for_prefab_loader_or_detach(&mut modules, &stopped_rx, &status, Duration::ZERO);
        assert_eq!(modules.0, ["detach:rve-prefabs"]);
        assert_eq!(status.exit_code(), 0);
        assert!(status.failure_message().is_none());
    }

    #[test]
    fn stopped_prefab_loader_is_joined_without_marking_the_run_failed() {
        let (stopped_tx, stopped_rx) = mpsc::channel::<()>();
        stopped_tx.send(()).unwrap();
        let status = RuntimeStatus::default();
        let mut modules = FakeThreads(Vec::new());
        wait_for_prefab_loader_or_detach(
            &mut modules,
            &stopped_rx,
            &status,
            Duration::from_secs(1),
        );
        assert_eq!(modules.0, ["join:rve-prefabs"]);
        assert_eq!(status.exit_code(), 0);
    }

    #[test]
    fn emergency_cleanup_detaches_a_hung_render_thread_without_joining_it() {
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let mut modules = ModuleHost::new();
        modules
            .spawn(
                "rve-render",
                || {},
                move || {
                    let _ = release_rx.recv();
                    let _ = finished_tx.send(());
                },
            )
            .unwrap();
        let (stopped_tx, stopped_rx) = bounded(1);
        let status = Arc::new(RuntimeStatus::default());

        assert!(!wait_for_render_or_detach(
            &mut modules,
            &status,
            &stopped_rx,
            Duration::ZERO,
        ));
        assert_eq!(status.exit_code(), 1);
        assert!(status.failure_message().unwrap().contains("не остановился"));
        assert!(modules.shutdown().is_empty());

        release_tx.send(()).unwrap();
        finished_rx.recv().unwrap();
        drop(stopped_tx);
    }

    #[test]
    fn hung_scripts_are_interrupted_then_detached_without_joining() {
        let (release_tx, release_rx) = mpsc::channel();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let mut modules = ModuleHost::new();
        modules
            .spawn(
                "rve-scripts",
                || {},
                move || {
                    let _ = started_tx.send(());
                    let _ = release_rx.recv();
                    let _ = finished_tx.send(());
                },
            )
            .unwrap();
        started_rx.recv().unwrap();
        let (stopped_tx, stopped_rx) = mpsc::channel();
        let interrupt = ScriptInterruptHandle::new();
        let status = RuntimeStatus::default();

        assert!(!wait_for_scripts_or_detach(
            &mut modules,
            &stopped_rx,
            &interrupt,
            &status,
            Duration::ZERO,
        ));
        assert!(interrupt.is_interrupted());
        assert_eq!(status.exit_code(), 1);
        assert!(status.failure_message().unwrap().contains("скриптов"));
        assert!(modules.shutdown().is_empty());
        drop(stopped_tx);
        release_tx.send(()).unwrap();
        finished_rx.recv().unwrap();
    }

    #[test]
    fn cleanup_joins_render_only_after_render_stopped_then_joins_scripts() {
        for (render_stopped, expected) in [
            (true, vec!["join:rve-render", "join:rve-scripts"]),
            (false, vec!["join:rve-scripts"]),
        ] {
            let mut modules = FakeThreads::default();
            let (stopped_tx, stopped_rx) = mpsc::channel();
            stopped_tx.send(()).unwrap();
            let status = RuntimeStatus::default();

            cleanup_threads(
                &mut modules,
                render_stopped,
                &stopped_rx,
                &ScriptInterruptHandle::new(),
                &status,
                Duration::ZERO,
            );

            assert_eq!(modules.0, expected);
            assert_eq!(status.exit_code(), 0);
        }
    }
}
