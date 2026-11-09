//! Startup, module ownership and runtime thread coordination for the RVE player.

/// Версия крейта, заданная в `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

mod activation;
mod cli;
#[cfg(feature = "dev-console")]
mod console;
mod cursor;
#[cfg(any(feature = "dev-console", test))]
mod dev_overlays;
mod game_ui;
mod hardware;
mod headless;
mod input;
mod layout;
mod module_host;
mod net;
mod pipeline;
mod player;
mod prefabs;
mod profiling;
mod scene;

use activation::{ActivationFailure, InstanceOutcome, PendingActivation, acquire_or_activate};
use cli::{PlayerArgs, SERVER_USAGE, USAGE};
#[cfg(feature = "dev-console")]
use console::{CommandCatalogSnapshot, ConsoleUiSlot};
use cursor::CursorControl;
use engine_assets::{Assets, TextureData};
#[cfg(feature = "dev-console")]
use engine_console::ConsoleBuffer;
use engine_console::install_logger;
use engine_files::VirtualPath;
use engine_network::{NetworkEngine, NetworkHandle};
use engine_physics::{
    PhysicsClient, PhysicsInitialization, PhysicsRequest, PhysicsServer, StepDriver,
};
use engine_platform::{Acquire, ActivationError, InstanceLock, activate_running_instance};
use engine_project::{BootstrapConfig, PrefabCatalogFile, ProjectSettings};
use engine_render::{DebugView, probe_devices};
use engine_resources::{ResourceConfig, ResourceHandle, ResourceSystem};
use engine_script::{PrefabCatalog, PrefabKind, PrefabSpec, ScriptInterruptHandle};
use engine_window::{RedrawMode, WindowConfig, WindowIcon};
use hardware::{hardware_issues, memory_and_cpu_issues};
use headless::{run_resource_responder, spawn_quit_listener};
use layout::{
    Layout, PrefabCatalogLocation, ProjectSettingsLocation, normalize_layout_path,
    normalize_scripts_path, release_root, server_release_root,
};
use module_host::ModuleHost;
use net::{NetworkSession, prepare};
use pipeline::{
    Clock, MonotonicClock, RuntimeStatus, SCRIPTS_THREAD, ScriptControl, ScriptStartupError,
    ScriptThreadConfig, start_scripts,
};
use prefabs::{RenderResourceRequest, start_prefab_loader};
use profiling::StartupProfile;
use scene::{PreparedScene, load_scene};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sysinfo::{CpuRefreshKind, System};

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
    return run_from_args(release_root, args, false, activation, console_buffer);
    #[cfg(not(feature = "dev-console"))]
    run_from_args(release_root, args, false, activation)
}

/// Runs a dedicated headless server (`rve-server`): no window, renderer, GPU probe or audio output.
/// Scripts run in background ticks at `[network].tick_hz`. Stops on `--frames`, `quit` on stdin,
/// or a fatal error.
pub fn run_server() -> ExitCode {
    let args = match PlayerArgs::parse_server(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("Ошибка аргументов: {error}");
            eprintln!("{SERVER_USAGE}");
            return ExitCode::from(2);
        }
    };
    let _ = install_logger(None);
    let release_root = match executable_dir() {
        Ok(exe_dir) => server_release_root(&exe_dir),
        Err(error) => {
            return fail(
                2,
                format!("не удалось определить каталог исполняемого файла: {error}"),
            );
        }
    };
    #[cfg(feature = "dev-console")]
    let console_buffer = Arc::new(Mutex::new(ConsoleBuffer::default()));
    #[cfg(feature = "dev-console")]
    return run_from_args(
        release_root,
        args,
        true,
        PendingActivation::default(),
        console_buffer,
    );
    #[cfg(not(feature = "dev-console"))]
    run_from_args(release_root, args, true, PendingActivation::default())
}

fn executable_dir() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let executable = normalize_layout_path(&executable).map_err(|error| error.to_string())?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "путь исполняемого файла не содержит родительский каталог".to_owned())
}

/// Общий путь запуска игры (`rve-player`) и выделенного сервера (`headless`). Окно, renderer, проверка
/// Vulkan, иконка, экземпляр и звуковой вывод есть только у игры.
fn run_from_args(
    release_root: PathBuf,
    args: PlayerArgs,
    headless: bool,
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
    let prefab_list = match load_prefab_catalog(&layout.prefab_catalog, &resource_handle) {
        Ok(list) => list,
        Err(error) => return fail(2, format!("ошибка списка префабов: {error}")),
    };
    let fsr_library = layout.fsr_library_path(project.runtime.fsr_library.as_deref());
    let dlss_library = layout.dlss_library_path(project.runtime.dlss_library.as_deref());

    if !headless && !bootstrap.allow_multiple_instances {
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

    let mut startup = StartupProfile::default();
    let mut phase_started = Instant::now();
    let mut system = System::new();
    system.refresh_memory();
    system.refresh_cpu_list(CpuRefreshKind::nothing());
    let total_ram_mb = system.total_memory() / (1024 * 1024);
    let logical_cpus = system.cpus().len();
    let issues = if headless {
        // Серверу нужны RAM и CPU; Vulkan-устройство для него не требуется.
        memory_and_cpu_issues(total_ram_mb, logical_cpus, &project.requirements)
    } else {
        let reports = probe_devices().map_err(|error| error.to_string());
        hardware_issues(
            reports.as_deref().map_err(String::as_str),
            total_ram_mb,
            logical_cpus,
            &project.requirements,
        )
    };
    if !issues.is_empty() {
        let message = issues.join("\n");
        log::error!("{message}");
        if !headless {
            engine_platform::show_error_dialog("RVE: системные требования", &message);
        }
        if let Err(error) = resources.shutdown() {
            log::error!("Ошибка завершения ресурсного сервиса: {error}");
        }
        drop(instance_lock.take());
        return ExitCode::from(3);
    }

    startup.record("hardware probe", phase_started.elapsed());
    phase_started = Instant::now();
    let assets = Assets::with_resources(resource_handle.clone());
    let icon = if headless {
        None
    } else {
        match load_window_icon(&bootstrap, &assets) {
            Ok(icon) => icon,
            Err(error) => return fail(2, format!("ошибка иконки приложения: {error}")),
        }
    };
    startup.record("window icon", phase_started.elapsed());
    phase_started = Instant::now();
    let scene_path = match project.starting_scene() {
        Some(path) => path.to_owned(),
        None => return fail(2, "в проекте не указана стартовая сцена"),
    };
    let mut prepared = match load_scene(&resource_handle, &assets, &scene_path) {
        Ok(prepared) => prepared,
        Err(error) => return fail(2, format!("ошибка загрузки сцены: {error}")),
    };
    startup.record("scene load", phase_started.elapsed());
    phase_started = Instant::now();
    if let Err(error) = add_heavy_prefabs(&mut prepared.prefabs.catalog, &prefab_list) {
        return fail(2, format!("ошибка списка префабов: {error}"));
    }
    let physics_geometry = prepared.physics_geometry();
    let (physics_driver, physics_bindings) =
        match prepared.build_physics(&physics_geometry, &project.physics) {
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

    startup.record("physics build", phase_started.elapsed());
    phase_started = Instant::now();
    let network = if args.network.is_requested() {
        let plan = match prepare(&args.network, &project, prepared.scene_hash, headless) {
            Ok(plan) => plan,
            Err(error) => {
                if let Err(error) = resources.shutdown() {
                    log::error!("Ошибка завершения ресурсного сервиса: {error}");
                }
                drop(instance_lock.take());
                return fail(2, format!("ошибка сетевых настроек: {error}"));
            }
        };
        match NetworkEngine::new(plan.config) {
            Ok((engine, handle)) => Some((
                engine,
                NetworkSession {
                    handle,
                    bridge: plan.bridge,
                },
            )),
            Err(error) => {
                if let Err(error) = resources.shutdown() {
                    log::error!("Ошибка завершения ресурсного сервиса: {error}");
                }
                drop(instance_lock.take());
                return fail(1, format!("не удалось запустить сеть: {error}"));
            }
        }
    } else {
        None
    };
    let (network_engine, network_session) = match network {
        Some((engine, session)) => (
            Some((engine, NetworkHandle::clone(&session.handle))),
            Some(session),
        ),
        None => (None, None),
    };
    let mut modules = ModuleHost::new();
    let status = Arc::new(RuntimeStatus::default());
    let (physics_client, audio_handle) = match start_modules(
        &mut modules,
        physics_driver,
        Arc::clone(&status),
        resource_handle.clone(),
        !headless,
        network_engine,
    ) {
        Ok(started) => started,
        Err(error) => {
            for error in modules.shutdown() {
                log::error!("Ошибка остановки модуля: {error}");
            }
            return fail(1, format!("не удалось запустить модуль: {error}"));
        }
    };

    startup.record("physics module", phase_started.elapsed());
    phase_started = Instant::now();
    let PreparedScene {
        world,
        name,
        sources,
        render_assets,
        prefabs,
        ui,
        ..
    } = prepared;
    let mut render_assets = render_assets;
    if !headless {
        // Исходники экранных шейдеров читает только игровой клиент; headless их не открывает.
        render_assets.screen_shaders = scene::load_screen_shader_sources(&resource_handle, &world);
    }
    let ui_text_focus = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (script_tx, script_rx) = flume::unbounded();
    let (frame_tx, frame_rx) = flume::bounded(1);
    let (render_tx, render_rx) = flume::unbounded();
    let (resource_tx, resource_rx) = flume::unbounded::<RenderResourceRequest>();
    let (prefab_loader_tx, prefab_loader_stopped_rx) = match start_prefab_loader(
        &mut modules,
        resource_handle.clone(),
        script_tx.clone(),
        resource_tx.clone(),
    ) {
        Ok(started) => started,
        Err(error) => {
            for error in modules.shutdown() {
                log::error!("Ошибка остановки модуля: {error}");
            }
            return fail(
                1,
                format!("не удалось запустить загрузчик префабов: {error}"),
            );
        }
    };
    let interrupt = ScriptInterruptHandle::new();
    let input = Arc::new(Mutex::new(input::InputAccumulator::default()));
    let cursor = CursorControl::default();
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    if headless {
        // Сервер без render thread работает фоновыми тиками с пустой GPU-таблицей.
        let _ = script_tx.send(ScriptControl::Background(true));
        let _ = script_tx.send(ScriptControl::Go(Box::default()));
    }
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
            background_tick_hz: if headless {
                project.network.tick_hz
            } else {
                project.runtime.background_tick_hz
            },
            gamepad_devices: !headless,
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
            prefabs,
            physics_geometry,
            ui,
            ui_text_focus: Arc::clone(&ui_text_focus),
            prefab_loader: prefab_loader_tx,
            render_resources: resource_tx.clone(),
            network: network_session,
            frame_limit: if headless { args.frames } else { None },
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
    startup.record("scripts start", phase_started.elapsed());
    phase_started = Instant::now();
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

    startup.record("scripts ready", phase_started.elapsed());
    let code = if headless {
        run_headless(
            &mut modules,
            &status,
            resource_rx,
            script_tx,
            script_thread.stopped,
        )
    } else {
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
        player::run_window(
            player::PlayerConfig {
                window: config,
                present_mode: bootstrap.present_mode,
                reflex: bootstrap.reflex,
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
                resources: resource_rx,
                prefab_loader_stopped_rx,
                script_stopped_rx: script_thread.stopped,
                interrupt,
                status: Arc::clone(&status),
                resource_stats: resource_handle.clone(),
                startup,
                input,
                text_focus: ui_text_focus,
                #[cfg(feature = "dev-console")]
                console_buffer,
                #[cfg(feature = "dev-console")]
                command_catalog,
                #[cfg(feature = "dev-console")]
                console_ui,
            },
            &mut modules,
        )
    };
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

/// Читает список тяжёлых префабов. Отсутствующий файл dev-проекта — пустой список; в release
/// ресурс всегда есть, поэтому его отсутствие тоже ошибка.
fn load_prefab_catalog(
    location: &PrefabCatalogLocation,
    resources: &ResourceHandle,
) -> Result<PrefabCatalogFile, String> {
    match location {
        PrefabCatalogLocation::File(path) => PrefabCatalogFile::load(path),
        PrefabCatalogLocation::Resource(path) => {
            let bytes = resources
                .read_all(path)
                .map_err(|error| format!("{path}: {error}"))?
                .wait()
                .map_err(|error| format!("{path}: {error}"))?;
            let contents = String::from_utf8(bytes)
                .map_err(|error| format!("{path}: список префабов должен быть UTF-8: {error}"))?;
            PrefabCatalogFile::parse(&contents).map_err(|error| format!("{path}: {error}"))
        }
    }
}

/// Добавляет тяжёлые префабы списка проекта в каталог сцены. Путь, который сцена уже объявила
/// быстрым, остаётся быстрым: его запись сцены приоритетнее.
fn add_heavy_prefabs(catalog: &mut PrefabCatalog, list: &PrefabCatalogFile) -> Result<(), String> {
    for entry in &list.prefabs {
        let path = VirtualPath::parse(&entry.path)
            .map_err(|_| format!("некорректный путь префаба {:?}", entry.path))?;
        if catalog.get(path.as_str()).is_some() {
            continue;
        }
        catalog.insert(
            path.as_str(),
            PrefabSpec {
                kind: PrefabKind::Heavy,
                max_instances: entry.max_instances,
            },
        );
    }
    Ok(())
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
        Ok(Err(ScriptStartupError::Network(error))) => {
            Err((1, format!("ошибка запуска сетевого моста: {error}")))
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
    audio_output: bool,
    network: Option<(NetworkEngine, NetworkHandle)>,
) -> std::io::Result<(PhysicsClient, engine_audio::AudioHandle)> {
    let (audio, audio_handle) = engine_audio::AudioEngine::new(engine_audio::AudioConfig {
        resources: Some(resources),
        output: audio_output,
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
    if let Some((engine, handle)) = network {
        modules.spawn(
            "rve-network",
            move || handle.shutdown(),
            move || engine.run(),
        )?;
    }
    Ok((
        PhysicsClient::new_preinitialized(physics_request_tx, physics_reply_rx),
        audio_handle,
    ))
}

/// Ждёт завершения scripts thread. Оно наступает по `--frames`, строке `quit`, фатальной ошибке
/// любого потока или Ctrl+C (процесс завершается без `on_destroy`). Код выхода берётся из
/// `RuntimeStatus`; ответчик ресурсов и чтение stdin работают, пока не остановит `ModuleHost`.
fn run_headless(
    modules: &mut ModuleHost,
    status: &RuntimeStatus,
    requests: flume::Receiver<RenderResourceRequest>,
    script_tx: flume::Sender<ScriptControl>,
    stopped: std::sync::mpsc::Receiver<()>,
) -> u8 {
    let responder_stop = Arc::new(AtomicBool::new(false));
    let responder_flag = Arc::clone(&responder_stop);
    let responder_tx = script_tx.clone();
    if let Err(error) = modules.spawn(
        "rve-headless-resources",
        move || responder_flag.store(true, Ordering::Release),
        move || run_resource_responder(requests, responder_tx, responder_stop),
    ) {
        log::error!("не удалось запустить ответчик ресурсов: {error}");
        return 1;
    }
    if let Err(error) = spawn_quit_listener(script_tx) {
        log::error!("не удалось читать команды из stdin: {error}");
        return 1;
    }
    let _ = stopped.recv();
    if let Some(message) = status.failure_message() {
        log::error!("{message}");
    }
    status.exit_code()
}

fn fail(code: u8, message: impl AsRef<str>) -> ExitCode {
    log::error!("{}", message.as_ref());
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::{
        add_heavy_prefabs, load_prefab_catalog, load_project_settings, scripts_ready_result,
        wait_scripts_ready, wait_scripts_ready_after_timeout,
    };
    use crate::layout::{PrefabCatalogLocation, ProjectSettingsLocation};
    use crate::pipeline::ScriptStartupError;
    use engine_project::PrefabCatalogFile;
    use engine_resources::{ResourceConfig, ResourceSystem, SourceMode};
    use engine_script::{PrefabCatalog, PrefabKind, PrefabSpec};
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
    fn fast_prefab_of_the_scene_keeps_its_spec_when_the_project_list_repeats_it() {
        let mut catalog = PrefabCatalog::default();
        catalog.insert(
            "prefabs/fast.rveprefab",
            PrefabSpec {
                kind: PrefabKind::Fast,
                max_instances: Some(4),
            },
        );
        let list = PrefabCatalogFile::parse(
            "[[prefabs]]\npath = \"/prefabs/fast.rveprefab\"\nmax_instances = 9\n\
             [[prefabs]]\npath = \"/prefabs/house.rveprefab\"\nmax_instances = 2\n",
        )
        .unwrap();
        add_heavy_prefabs(&mut catalog, &list).unwrap();
        assert_eq!(
            catalog.get("prefabs/fast.rveprefab"),
            Some(PrefabSpec {
                kind: PrefabKind::Fast,
                max_instances: Some(4),
            })
        );
        assert_eq!(
            catalog.get("prefabs/house.rveprefab"),
            Some(PrefabSpec {
                kind: PrefabKind::Heavy,
                max_instances: Some(2),
            })
        );
    }

    #[test]
    fn release_prefab_list_is_read_through_resources_and_bad_format_names_the_file() {
        let root =
            std::env::temp_dir().join(format!("rve-runtime-prefab-list-{}", std::process::id()));
        fs::create_dir_all(root.join("rve")).unwrap();
        let mut resources = ResourceSystem::start(ResourceConfig {
            root: root.clone(),
            source_mode: SourceMode::Loose,
            ..ResourceConfig::default()
        })
        .unwrap();
        let location = PrefabCatalogLocation::Resource("/rve/prefabs.toml");

        fs::write(
            root.join("rve/prefabs.toml"),
            "[[prefabs]]\npath = \"/prefabs/a.txt\"\n",
        )
        .unwrap();
        let error = load_prefab_catalog(&location, &resources.handle()).unwrap_err();
        assert!(error.contains("/rve/prefabs.toml"), "{error}");

        fs::write(
            root.join("rve/prefabs.toml"),
            "[[prefabs]]\npath = \"/prefabs/a.rveprefab\"\nmax_instances = 3\n",
        )
        .unwrap();
        let list = load_prefab_catalog(&location, &resources.handle()).unwrap();
        assert_eq!(list.prefabs.len(), 1);
        assert_eq!(list.prefabs[0].max_instances, Some(3));

        resources.shutdown().unwrap();
        fs::remove_dir_all(root).unwrap();
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
