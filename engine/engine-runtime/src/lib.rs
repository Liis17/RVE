//! Startup, module ownership and runtime thread coordination for the RVE player.

mod activation;
mod cli;
#[cfg(feature = "dev-console")]
mod console;
mod cursor;
mod hardware;
mod input;
mod layout;
mod module_host;
mod pipeline;
mod player;
mod scene;

use activation::{ActivationFailure, InstanceOutcome, PendingActivation, acquire_or_activate};
use cli::{PlayerArgs, USAGE};
#[cfg(feature = "dev-console")]
use console::{CommandCatalogSnapshot, ConsoleUiSlot};
use cursor::CursorControl;
use engine_assets::{Assets, TextureData};
#[cfg(feature = "dev-console")]
use engine_console::ConsoleBuffer;
use engine_console::install_logger;
use engine_physics::{
    PhysicsClient, PhysicsInitialization, PhysicsRequest, PhysicsServer, StepDriver,
};
use engine_platform::{Acquire, ActivationError, InstanceLock, activate_running_instance};
use engine_project::{BootstrapConfig, ProjectSettings};
use engine_render::{DebugView, probe_devices};
use engine_resources::{ResourceConfig, ResourceHandle, ResourceSystem};
use engine_script::ScriptInterruptHandle;
use engine_window::{RedrawMode, WindowConfig, WindowIcon};
use hardware::hardware_issues;
use layout::{
    Layout, ProjectSettingsLocation, normalize_layout_path, normalize_scripts_path, release_root,
};
use module_host::ModuleHost;
use pipeline::{
    Clock, MonotonicClock, RuntimeStatus, SCRIPTS_THREAD, ScriptStartupError, ScriptThreadConfig,
    start_scripts,
};
use scene::{PreparedScene, load_scene};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sysinfo::{CpuRefreshKind, System};

/// Версия крейта из `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const SCRIPTS_READY_TIMEOUT: Duration = Duration::from_secs(30);
const SCRIPTS_READY_GRACE: Duration = Duration::from_secs(2);

/// Runs an RVE game using the release layout or a loose-file development project.
pub fn run_player() -> ExitCode {
    let args = match PlayerArgs::parse_for_build(
        std::env::args_os().skip(1),
        cfg!(feature = "dev-console"),
    ) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("Ошибка аргументов: {error}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    #[cfg(feature = "dev-console")]
    let console_buffer = Arc::new(Mutex::new(ConsoleBuffer::default()));
    #[cfg(feature = "dev-console")]
    let _ = install_logger(Some(Arc::clone(&console_buffer)));
    #[cfg(not(feature = "dev-console"))]
    let _ = install_logger(None);
    let release_root = match executable_dir() {
        Ok(exe_dir) => release_root(&exe_dir),
        Err(error) => {
            return fail(
                2,
                format!("не удалось определить каталог исполняемого файла: {error}"),
            );
        }
    };
    // SAFETY: this is the first runtime operation that can affect process-wide
    // Vulkan configuration, and no runtime, resource, or activation thread exists yet.
    unsafe { engine_platform::prepare_vulkan_environment(&release_root) };
    let activation = PendingActivation::default();
    #[cfg(feature = "dev-console")]
    return run_from_args(release_root, args, activation, console_buffer);
    #[cfg(not(feature = "dev-console"))]
    run_from_args(release_root, args, activation)
}

fn executable_dir() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let executable = normalize_layout_path(&executable).map_err(|error| error.to_string())?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "путь исполняемого файла не содержит родительский каталог".to_owned())
}

fn run_from_args(
    release_root: PathBuf,
    args: PlayerArgs,
    activation: PendingActivation,
    #[cfg(feature = "dev-console")] console_buffer: Arc<Mutex<ConsoleBuffer>>,
) -> ExitCode {
    let debug_view = args.debug_view.unwrap_or(DebugView::Lit);
    #[cfg(feature = "dev-console")]
    let command_catalog = Arc::new(Mutex::new(CommandCatalogSnapshot::default()));
    #[cfg(feature = "dev-console")]
    let console_ui = Arc::new(ConsoleUiSlot::default());
    let layout = match args.project {
        Some(project) => {
            let project = match normalize_layout_path(&project) {
                Ok(path) if path.is_dir() => path,
                Ok(path) => {
                    return fail(
                        2,
                        format!("каталог проекта не является каталогом: {}", path.display()),
                    );
                }
                Err(error) => return fail(2, format!("проект {}: {error}", project.display())),
            };
            Layout::dev(&project)
        }
        None => Layout::release(&release_root),
    };
    let scripts_override = match args.scripts.as_deref() {
        Some(path) => match normalize_scripts_path(path) {
            Ok(path) => Some(path),
            Err(error) => {
                return fail(
                    2,
                    format!(
                        "не удалось разрешить путь --scripts {}: {error}",
                        path.display()
                    ),
                );
            }
        },
        None => None,
    };
    let scripts_library = layout.scripts_library_path(scripts_override);
    let bootstrap = match BootstrapConfig::load(&layout.bootstrap) {
        Ok(config) => config,
        Err(error) => return fail(2, format!("ошибка bootstrap: {error}")),
    };

    // Declared before ResourceSystem so normal Rust drop order stops resources first,
    // then releases the process lock on every early-return path.
    let mut instance_lock = None;
    let mut resources = match ResourceSystem::start(ResourceConfig {
        root: layout.assets.clone(),
        source_mode: layout.source_mode.clone(),
        ..ResourceConfig::default()
    }) {
        Ok(resources) => resources,
        Err(error) => return fail(2, format!("ошибка ресурсов: {error}")),
    };
    let resource_handle = resources.handle();
    let project = match load_project_settings(&layout.project_settings, &resource_handle) {
        Ok(project) => project,
        Err(error) => return fail(2, format!("ошибка проекта: {error}")),
    };
    let fsr_library = layout.fsr_library_path(project.runtime.fsr_library.as_deref());
    let dlss_library = layout.dlss_library_path(project.runtime.dlss_library.as_deref());

    if !bootstrap.allow_multiple_instances {
        let outcome = acquire_or_activate(
            || {
                let activation = activation.clone();
                match InstanceLock::acquire(&project.id, Box::new(move || activation.request())) {
                    Ok(Acquire::Acquired(lock)) => Ok(Some(lock)),
                    Ok(Acquire::AlreadyRunning) => Ok(None),
                    Err(error) => Err(error.to_string()),
                }
            },
            |deadline| match activate_running_instance(&project.id, deadline) {
                Ok(()) => Ok(()),
                Err(ActivationError::Timeout) => Err(ActivationFailure::Timeout),
                Err(error) => Err(ActivationFailure::Other(error.to_string())),
            },
            Duration::from_secs(5),
        );
        match outcome {
            InstanceOutcome::Continue(lock) => instance_lock = Some(lock),
            InstanceOutcome::Activated => {
                if let Err(error) = resources.shutdown() {
                    log::error!("Ошибка завершения ресурсного сервиса: {error}");
                }
                drop(instance_lock.take());
                return ExitCode::SUCCESS;
            }
            InstanceOutcome::Failure { code, message } => {
                if let Err(error) = resources.shutdown() {
                    log::error!("Ошибка завершения ресурсного сервиса: {error}");
                }
                drop(instance_lock.take());
                return fail(code, message);
            }
        }
    }

    let mut system = System::new();
    system.refresh_memory();
    system.refresh_cpu_list(CpuRefreshKind::nothing());
    let total_ram_mb = system.total_memory() / (1024 * 1024);
    let logical_cpus = system.cpus().len();
    let reports = probe_devices().map_err(|error| error.to_string());
    let issues = hardware_issues(
        reports.as_deref().map_err(String::as_str),
        total_ram_mb,
        logical_cpus,
        &project.requirements,
    );
    if !issues.is_empty() {
        let message = issues.join("\n");
        log::error!("{message}");
        engine_platform::show_error_dialog("RVE: системные требования", &message);
        if let Err(error) = resources.shutdown() {
            log::error!("Ошибка завершения ресурсного сервиса: {error}");
        }
        drop(instance_lock.take());
        return ExitCode::from(3);
    }

    let assets = Assets::with_resources(resource_handle.clone());
    let icon = match load_window_icon(&bootstrap, &assets) {
        Ok(icon) => icon,
        Err(error) => return fail(2, format!("ошибка иконки приложения: {error}")),
    };
    let scene_path = match project.starting_scene() {
        Some(path) => path.to_owned(),
        None => return fail(2, "в проекте не указана стартовая сцена"),
    };
    let prepared = match load_scene(&resource_handle, &assets, &scene_path) {
        Ok(prepared) => prepared,
        Err(error) => return fail(2, format!("ошибка загрузки сцены: {error}")),
    };
    let (physics_driver, physics_bindings) = match prepared.build_physics(&project.physics) {
        Ok(physics) => physics,
        Err(error) => return fail(2, format!("ошибка построения физической сцены: {error}")),
    };
    let physics_initialization =
        match PhysicsInitialization::from_prebuilt(&physics_driver, physics_bindings) {
            Ok(initialization) => initialization,
            Err(error) => {
                return fail(2, format!("ошибка инициализации физической сцены: {error}"));
            }
        };

    let mut modules = ModuleHost::new();
    let status = Arc::new(RuntimeStatus::default());
    let (physics_client, audio_handle) = match start_modules(
        &mut modules,
        physics_driver,
        Arc::clone(&status),
        resource_handle.clone(),
    ) {
        Ok(started) => started,
        Err(error) => {
            for error in modules.shutdown() {
                log::error!("Ошибка остановки модуля: {error}");
            }
            return fail(1, format!("не удалось запустить модуль: {error}"));
        }
    };

    let PreparedScene {
        world,
        name,
        sources,
        render_assets,
    } = prepared;
    let (script_tx, script_rx) = flume::unbounded();
    let (frame_tx, frame_rx) = flume::bounded(1);
    let (render_tx, render_rx) = flume::unbounded();
    let interrupt = ScriptInterruptHandle::new();
    let input = Arc::new(Mutex::new(input::InputAccumulator::default()));
    let cursor = CursorControl::default();
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    let script_thread = match start_scripts(
        &mut modules,
        world,
        ScriptThreadConfig {
            scene_name: name,
            sources,
            input_settings: project.input.clone(),
            scripts_library,
            #[cfg(test)]
            script_module: None,
            interrupt: interrupt.clone(),
            clock,
            background_tick_hz: project.runtime.background_tick_hz,
            control_tx: script_tx.clone(),
            control_rx: script_rx,
            frame_tx,
            render_tx: render_tx.clone(),
            status: Arc::clone(&status),
            input: Arc::clone(&input),
            cursor: cursor.clone(),
            physics_client: Some(physics_client),
            physics_initialization: Some(physics_initialization),
            audio: Some(audio_handle),
            #[cfg(feature = "dev-console")]
            command_catalog: Arc::clone(&command_catalog),
        },
    ) {
        Ok(ready) => ready,
        Err(error) => {
            let shutdown_errors = modules.shutdown();
            for error in shutdown_errors {
                log::error!("Ошибка остановки модуля: {error}");
            }
            return fail(1, format!("не удалось запустить поток скриптов: {error}"));
        }
    };
    let mut scripts_ready_timed_out = false;
    if let Err((code, message)) = wait_scripts_ready(
        &script_thread.ready,
        &interrupt,
        SCRIPTS_READY_TIMEOUT,
        SCRIPTS_READY_GRACE,
        &mut scripts_ready_timed_out,
    ) {
        if scripts_ready_timed_out {
            modules.detach(SCRIPTS_THREAD);
        }
        for error in modules.shutdown() {
            log::error!("Ошибка остановки модуля: {error}");
        }
        if let Err(error) = resources.shutdown() {
            log::error!("Ошибка завершения ресурсного сервиса: {error}");
        }
        drop(instance_lock.take());
        return fail(code, message);
    }

    let pipeline_cache_path = engine_platform::pipeline_cache_path(&project.name);
    let pipeline_cache = engine_platform::load_pipeline_cache(pipeline_cache_path.as_deref());
    let title = bootstrap.window_title.unwrap_or(project.name);
    let default_window = WindowConfig::default();
    let config = WindowConfig {
        title,
        width: bootstrap.window_size_x.unwrap_or(default_window.width),
        height: bootstrap.window_size_y.unwrap_or(default_window.height),
        icon,
        redraw: RedrawMode::OnDemand,
    };
    let code = player::run_window(
        player::PlayerConfig {
            window: config,
            present_mode: bootstrap.present_mode,
            pipeline_cache_path,
            fsr_library,
            dlss_library,
            dlss_project_id: project.runtime.dlss_project_id,
            dlss_application_id: project.runtime.dlss_application_id,
            dlss_engine_version: project.runtime.dlss_engine_version,
            pipeline_cache,
            frame_limit: args.frames,
            debug_view,
            render_assets,
            activation,
            cursor,
            script_tx,
            render_tx,
            render_rx,
            frame_rx,
            script_stopped_rx: script_thread.stopped,
            interrupt,
            status: Arc::clone(&status),
            input,
            #[cfg(feature = "dev-console")]
            console_buffer,
            #[cfg(feature = "dev-console")]
            command_catalog,
            #[cfg(feature = "dev-console")]
            console_ui,
        },
        &mut modules,
    );
    let mut code = code;
    for error in modules.shutdown() {
        log::error!("Ошибка завершения модуля: {error}");
        code = 1;
    }
    if let Err(error) = resources.shutdown() {
        log::error!("Ошибка завершения ресурсного сервиса: {error}");
        if code == 0 {
            code = 1;
        }
    }
    drop(instance_lock.take());
    ExitCode::from(code)
}

fn load_project_settings(
    location: &ProjectSettingsLocation,
    resources: &ResourceHandle,
) -> Result<ProjectSettings, String> {
    match location {
        ProjectSettingsLocation::File(path) => ProjectSettings::load(path),
        ProjectSettingsLocation::Resource(path) => {
            let bytes = resources
                .read_all(path)
                .map_err(|error| format!("{path}: {error}"))?
                .wait()
                .map_err(|error| format!("{path}: {error}"))?;
            let contents = String::from_utf8(bytes)
                .map_err(|error| format!("{path}: настройки проекта должны быть UTF-8: {error}"))?;
            ProjectSettings::parse(&contents).map_err(|error| format!("{path}: {error}"))
        }
    }
}

fn scripts_ready_result(
    result: Result<Result<(), ScriptStartupError>, std::sync::mpsc::RecvError>,
) -> Result<(), (u8, String)> {
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(ScriptStartupError::Load(error))) => {
            Err((2, format!("ошибка загрузки скриптов: {error}")))
        }
        Ok(Err(ScriptStartupError::Physics(error))) => {
            Err((1, format!("ошибка запуска физики: {error}")))
        }
        Ok(Err(ScriptStartupError::Panic(error))) => Err((1, error)),
        Err(error) => Err((1, format!("поток скриптов не подтвердил запуск: {error}"))),
    }
}

fn wait_scripts_ready(
    receiver: &std::sync::mpsc::Receiver<Result<(), ScriptStartupError>>,
    interrupt: &ScriptInterruptHandle,
    timeout: Duration,
    grace: Duration,
    timed_out: &mut bool,
) -> Result<(), (u8, String)> {
    wait_scripts_ready_after_timeout(
        receiver,
        timeout,
        grace,
        || interrupt.interrupt(),
        timed_out,
    )
}

fn wait_scripts_ready_after_timeout(
    receiver: &std::sync::mpsc::Receiver<Result<(), ScriptStartupError>>,
    timeout: Duration,
    grace: Duration,
    on_timeout: impl FnOnce(),
    timed_out: &mut bool,
) -> Result<(), (u8, String)> {
    match receiver.recv_timeout(timeout) {
        Ok(result) => scripts_ready_result(Ok(result)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            scripts_ready_result(Err(std::sync::mpsc::RecvError))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            on_timeout();
            match receiver.recv_timeout(grace) {
                Ok(result) => scripts_ready_result(Ok(result)),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    scripts_ready_result(Err(std::sync::mpsc::RecvError))
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    *timed_out = true;
                    Err((
                        1,
                        format!("поток скриптов не подтвердил запуск за {timeout:?}"),
                    ))
                }
            }
        }
    }
}

fn load_window_icon(
    bootstrap: &BootstrapConfig,
    assets: &Assets,
) -> Result<Option<WindowIcon>, String> {
    let Some(path) = bootstrap.app_icon.as_deref() else {
        return Ok(None);
    };
    let TextureData {
        width,
        height,
        rgba,
    } = assets
        .load_texture(path)
        .map_err(|error| error.to_string())?;
    Ok(Some(WindowIcon {
        rgba,
        width,
        height,
    }))
}

fn start_modules(
    modules: &mut ModuleHost,
    physics_driver: StepDriver,
    status: Arc<RuntimeStatus>,
    resources: ResourceHandle,
) -> std::io::Result<(PhysicsClient, engine_audio::AudioHandle)> {
    let (audio, audio_handle) = engine_audio::AudioEngine::new(engine_audio::AudioConfig {
        resources: Some(resources),
        output: true,
    });
    let audio_stop = audio_handle.clone();
    modules.spawn(
        "rve-audio",
        move || audio_stop.shutdown(),
        move || audio.run(),
    )?;

    let (physics_request_tx, physics_request_rx) = flume::unbounded();
    let (physics_reply_tx, physics_reply_rx) = flume::unbounded();
    let physics_stop_tx = physics_request_tx.clone();
    let physics_status = Arc::clone(&status);
    modules.spawn(
        "rve-physics",
        move || {
            let _ = physics_stop_tx.send(PhysicsRequest::Shutdown);
        },
        move || {
            if let Err(error) = PhysicsServer::run_preinitialized(
                physics_request_rx,
                physics_reply_tx,
                physics_driver,
            ) {
                physics_status
                    .record_failure(format!("поток физики завершился с ошибкой: {error}"));
            }
        },
    )?;

    let (network_tx, network_rx) = std::sync::mpsc::channel();
    modules.spawn(
        "rve-network",
        move || {
            let _ = network_tx.send(());
        },
        move || {
            engine_network::NetworkEngine::new().run(network_rx);
        },
    )?;

    let (neural_tx, neural_rx) = std::sync::mpsc::channel();
    modules.spawn(
        "rve-neural",
        move || {
            let _ = neural_tx.send(());
        },
        move || {
            engine_neural::NeuralEngine::new().run(neural_rx);
        },
    )?;
    Ok((
        PhysicsClient::new_preinitialized(physics_request_tx, physics_reply_rx),
        audio_handle,
    ))
}

fn fail(code: u8, message: impl AsRef<str>) -> ExitCode {
    log::error!("{}", message.as_ref());
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::{
        load_project_settings, scripts_ready_result, wait_scripts_ready,
        wait_scripts_ready_after_timeout,
    };
    use crate::layout::ProjectSettingsLocation;
    use crate::pipeline::ScriptStartupError;
    use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn reads_release_project_settings_through_resource_handle() {
        let root = std::env::temp_dir().join(format!(
            "rve-runtime-project-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("rve")).unwrap();
        fs::write(
            root.join("rve/project.toml"),
            "id = \"runtime-test\"\nname = \"Runtime Test\"\nversion = \"1\"\n[[scenes]]\npath = \"start.rvescene\"\n",
        )
        .unwrap();
        let mut resources = ResourceSystem::start(ResourceConfig {
            root: root.clone(),
            source_mode: SourceMode::Loose,
            ..ResourceConfig::default()
        })
        .unwrap();
        let project = load_project_settings(
            &ProjectSettingsLocation::Resource("/rve/project.toml"),
            &resources.handle(),
        )
        .unwrap();
        assert_eq!(project.id, "runtime-test");
        resources.shutdown().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn script_load_handshake_errors_map_to_exit_code_two() {
        let (code, message) = scripts_ready_result(Ok(Err(ScriptStartupError::Load(
            "bad.lua: syntax error".to_owned(),
        ))))
        .unwrap_err();
        assert_eq!(code, 2);
        assert!(message.contains("bad.lua"));
    }

    #[test]
    fn physics_startup_failure_maps_to_runtime_exit_code_one() {
        let (code, message) = scripts_ready_result(Ok(Err(ScriptStartupError::Physics(
            "server disconnected".to_owned(),
        ))))
        .unwrap_err();
        assert_eq!(code, 1);
        assert!(message.contains("server disconnected"));
    }

    #[test]
    fn script_ready_handshake_accepts_ready_and_preserves_disconnect() {
        let (ready_tx, ready_rx) = mpsc::channel();
        ready_tx.send(Ok(())).unwrap();
        let mut timed_out = false;
        assert!(
            wait_scripts_ready(
                &ready_rx,
                &engine_script::ScriptInterruptHandle::new(),
                Duration::ZERO,
                Duration::ZERO,
                &mut timed_out,
            )
            .is_ok()
        );
        assert!(!timed_out);

        let (ready_tx, ready_rx) = mpsc::channel();
        drop(ready_tx);
        let mut timed_out = false;
        let (code, message) = wait_scripts_ready(
            &ready_rx,
            &engine_script::ScriptInterruptHandle::new(),
            Duration::ZERO,
            Duration::ZERO,
            &mut timed_out,
        )
        .unwrap_err();
        assert_eq!(code, 1);
        assert!(message.contains("не подтвердил запуск"));
        assert!(!timed_out);
    }

    #[test]
    fn script_ready_timeout_interrupts_then_accepts_grace_response() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let interrupt = engine_script::ScriptInterruptHandle::new();
        let timeout_interrupt = interrupt.clone();
        let mut timed_out = false;
        assert!(
            wait_scripts_ready_after_timeout(
                &ready_rx,
                Duration::ZERO,
                Duration::ZERO,
                move || {
                    timeout_interrupt.interrupt();
                    ready_tx.send(Ok(())).unwrap();
                },
                &mut timed_out,
            )
            .is_ok()
        );
        assert!(interrupt.is_interrupted());
        assert!(!timed_out);
    }

    #[test]
    fn late_script_load_error_during_grace_keeps_exit_code_two() {
        let (ready_tx, ready_rx) = mpsc::channel();
        let mut timed_out = false;
        let (code, message) = wait_scripts_ready_after_timeout(
            &ready_rx,
            Duration::ZERO,
            Duration::ZERO,
            move || {
                ready_tx
                    .send(Err(ScriptStartupError::Load("late load error".to_owned())))
                    .unwrap();
            },
            &mut timed_out,
        )
        .unwrap_err();
        assert_eq!(code, 2);
        assert!(message.contains("late load error"));
        assert!(!timed_out);
    }

    #[test]
    fn script_ready_timeout_interrupts_and_reports_failure_after_grace() {
        let (_ready_tx, ready_rx) = mpsc::channel();
        let interrupt = engine_script::ScriptInterruptHandle::new();
        let mut timed_out = false;
        let (code, message) = wait_scripts_ready(
            &ready_rx,
            &interrupt,
            Duration::ZERO,
            Duration::ZERO,
            &mut timed_out,
        )
        .unwrap_err();
        assert_eq!(code, 1);
        assert!(message.contains("не подтвердил запуск за"));
        assert!(interrupt.is_interrupted());
        assert!(timed_out);
    }

    #[test]
    fn script_thread_panics_before_ready_map_to_exit_code_one() {
        let (code, message) = scripts_ready_result(Ok(Err(ScriptStartupError::Panic(
            "script thread panic".to_owned(),
        ))))
        .unwrap_err();
        assert_eq!(code, 1);
        assert!(message.contains("panic"));
    }
}
