//! Приложение редактора: окно, цикл кадра egui + renderer и выполнение действий.

use crate::asset_watch::{AssetChanges, AssetWatcher, ChangeBatch};
use crate::autosave::{AUTOSAVE_INTERVAL, AutosaveStore, autosave_due};
use crate::console::LogLevel;
use crate::layout::Tab;
use crate::play::PlaySession;
use crate::project::OpenProject;
use crate::scene_edit::EditorScene;
use crate::settings::{SettingsStore, UserSettings};
use crate::state::{Action, EditorState, RecoveryPrompt, UnsavedPrompt};
use crate::ui;
use crate::ui::scene_view::bounding_sphere;
use crate::viewport::render::{SceneRenderer, render_local_lights};
use egui_dock::DockState;
use engine_console::{
    CommandCatalog, ConsoleBuffer, ConsoleLevel, ConsoleLine, install_logger, parse_command_line,
};
use engine_files::VirtualPath;
use engine_render::{
    FrameStatus, RenderError, Renderer, RendererConfig, UiImage, UiPrimitive, ViewportId,
};
use engine_scene::{CameraSettings, World};
use engine_script::ScriptLibrary;
use engine_ui::console::ConsoleOverlay;
use engine_ui::render::{ConvertedMesh, UiBridge};
use engine_window::{Key, MouseButton, Window, WindowEvent, WindowHandler, WindowProxy};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use winit::event::ElementState;
use winit::keyboard::{KeyCode, PhysicalKey};

/// Цвет окна за панелями; всё окно закрыто UI, поэтому заметен только при первом кадре.
const WINDOW_CLEAR: [f32; 4] = [0.02, 0.02, 0.025, 1.0];
/// Как часто проверять готовность фоновых превью и моделей.
const PREVIEW_POLL: Duration = Duration::from_millis(30);
/// Как часто забирать вывод идущей сборки.
const BUILD_POLL: Duration = Duration::from_millis(50);

pub struct EditorApp {
    state: EditorState,
    control_click: bool,
    dock: DockState<Tab>,
    /// Файл настроек пользователя; `None`, если каталог настроек ОС неизвестен.
    settings: Option<SettingsStore>,
    initial_project: Option<PathBuf>,
    gpu: Option<GpuState>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    console_overlay: ConsoleOverlay,
    bridge: UiBridge,
    engine_log: Arc<Mutex<ConsoleBuffer>>,
    repaint_at: Option<Instant>,
    /// Ревизия сцены в последней копии автосохранения и время записи.
    autosaved_revision: Option<u64>,
    last_autosave: Instant,
    exit: bool,
    pub exit_code: u8,
    window_title: String,
    window: Option<Arc<Window>>,
    proxy: Option<WindowProxy>,
    asset_watcher: Option<AssetWatcher>,
    asset_changes: ChangeBatch,
    pipeline_cache_path: Option<PathBuf>,
    pipeline_cache: Vec<u8>,
}

struct GpuState {
    renderer: Renderer,
    scene: SceneRenderer,
}

impl EditorApp {
    pub fn new(project: Option<PathBuf>, scripts: Option<PathBuf>) -> Self {
        let engine_log = editor_log_buffer();
        let mut state = EditorState::new(scripts);
        let pipeline_cache_path = engine_platform::pipeline_cache_path("rve-editor");
        let pipeline_cache = engine_platform::load_pipeline_cache(pipeline_cache_path.as_deref());
        let settings = SettingsStore::default_location();
        let (user, problem) = settings
            .as_ref()
            .map(SettingsStore::load)
            .unwrap_or_default();
        if let Some(problem) = problem {
            state.log.push(LogLevel::Warn, problem);
        }
        let dock = user.layout_or_default();
        state.scroll_navigation = user.scroll_navigation;
        state.recent_projects = user.recent_projects;
        Self {
            state,
            control_click: false,
            dock,
            settings,
            initial_project: project,
            gpu: None,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            console_overlay: ConsoleOverlay::default(),
            bridge: UiBridge::default(),
            engine_log,
            repaint_at: None,
            autosaved_revision: None,
            last_autosave: Instant::now(),
            exit: false,
            exit_code: 0,
            window_title: String::new(),
            window: None,
            proxy: None,
            asset_watcher: None,
            asset_changes: ChangeBatch::default(),
            pipeline_cache_path,
            pipeline_cache,
        }
    }

    pub fn save_pipeline_cache(&self) {
        let Some(renderer) = self.gpu.as_ref().map(|gpu| &gpu.renderer) else {
            return;
        };
        match renderer.pipeline_cache_data() {
            Ok(data) => {
                engine_platform::save_pipeline_cache(self.pipeline_cache_path.as_deref(), &data)
            }
            Err(error) => eprintln!("failed to retrieve pipeline cache: {error}"),
        }
    }

    fn fail(&mut self, error: &RenderError) {
        eprintln!("ошибка renderer: {error}");
        self.exit_code = 1;
        self.exit = true;
    }

    fn toggle_console(&mut self) -> bool {
        if !toggle_console(&mut self.state, &mut self.console_overlay) {
            return false;
        }
        if self.state.console_open
            && let Some(play) = &mut self.state.play
        {
            play.clear_input();
        }
        true
    }

    fn open_project(&mut self, dir: &Path) {
        match OpenProject::open(dir) {
            Ok(project) => {
                self.cancel_project_jobs();
                self.stop_play();
                let previous_generation = self.state.script_generation.take();
                self.state.library = None;
                if let Some(generation) = previous_generation
                    && let Err(error) = std::fs::remove_file(&generation.path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    self.state.log.push(
                        LogLevel::Warn,
                        format!("{}: {error}", generation.path.display()),
                    );
                }
                if self.state.scripts_path.is_none()
                    && let Err(error) = crate::build::cleanup_script_generations(&project.root)
                {
                    self.state.log.push(
                        LogLevel::Warn,
                        format!(
                            "{}: {error}",
                            project.root.join(".rve/editor/scripts").display()
                        ),
                    );
                }
                self.state
                    .log
                    .info(format!("Открыт проект {}", project.settings.name));
                let start = project.settings.starting_scene().map(str::to_owned);
                UserSettings::remember_project(
                    &mut self.state.recent_projects,
                    project.root.clone(),
                );
                self.state.project = Some(project);
                self.save_settings();
                self.state.current_folder.clear();
                self.reset_project_assets();
                self.watch_project_assets();
                self.refresh_tree_only();
                if self.state.scripts_path.is_some() {
                    self.load_manual_library();
                } else if self
                    .state
                    .project
                    .as_ref()
                    .is_some_and(|project| project.settings.heavy_scripts.is_some())
                {
                    self.start_scripts_build();
                }
                match start {
                    Some(path) => self.open_scene(&path),
                    None => self.new_scene(),
                }
            }
            Err(error) => self.state.log.error(error),
        }
    }

    fn cancel_project_jobs(&mut self) {
        if let Some(job) = self.state.build.take() {
            job.kill();
        }
        if let Some(job) = self.state.scripts_job.take() {
            job.kill();
        }
        self.state.pending_play = false;
    }

    fn reset_project_assets(&mut self) {
        self.asset_changes = ChangeBatch::default();
        self.state.previews.clear();
        self.state.clear_lite_schemas();
        self.state.models.clear();
        if let Some(assets) = self
            .state
            .project
            .as_ref()
            .map(|project| project.assets.clone())
        {
            self.state.model_loader.restart(assets);
        }
        if let Some(gpu) = &mut self.gpu
            && let Err(error) = gpu
                .scene
                .clear_cache(&mut gpu.renderer, &mut self.state.models)
        {
            self.fail(&error);
        }
    }

    fn watch_project_assets(&mut self) {
        self.asset_watcher = None;
        let (Some(proxy), Some(project)) = (&self.proxy, &self.state.project) else {
            return;
        };
        match AssetWatcher::new(project.assets_dir.root(), proxy.clone()) {
            Ok(watcher) => self.asset_watcher = Some(watcher),
            Err(error) => self.state.log.push(
                LogLevel::Warn,
                format!("Автообновление ресурсов недоступно: {error}"),
            ),
        }
    }

    fn start_scripts_build(&mut self) -> bool {
        if self.state.scripts_job.is_some() {
            return true;
        }
        let Some(project) = &self.state.project else {
            return false;
        };
        if project.settings.heavy_scripts.is_none() || self.state.scripts_path.is_some() {
            return false;
        }
        let project_dir = project.root.clone();
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_owned));
        let tool = match crate::build::locate_build_tool(
            exe_dir.as_deref(),
            &crate::build::checkout_workspace(),
            std::env::var_os("CARGO"),
        ) {
            Ok(tool) => tool,
            Err(error) => {
                self.state.log.error(error);
                return false;
            }
        };
        let command = tool.scripts_command(&project_dir);
        match crate::build::ScriptsJob::spawn(command, project_dir.clone()) {
            Ok(job) => {
                self.state
                    .log
                    .info(format!("Build scripts: {}", project_dir.display()));
                self.state.scripts_job = Some(job);
                true
            }
            Err(error) => {
                self.state.log.error(format!("rve-build scripts: {error}"));
                false
            }
        }
    }

    /// Превью или модели ещё готовятся в фоне: окно должно проснуться, чтобы показать их.
    fn background_pending(&self) -> bool {
        self.state.previews.has_pending() || self.state.model_loader.has_pending()
    }

    /// Записывает недавние проекты и раскладку панелей; ошибка только пишется в Console.
    fn save_settings(&mut self) {
        let Some(store) = &self.settings else {
            return;
        };
        let settings = UserSettings {
            recent_projects: self.state.recent_projects.clone(),
            layout: Some(self.dock.clone()),
            scroll_navigation: self.state.scroll_navigation,
        };
        if let Err(error) = store.save(&settings) {
            self.state.log.push(LogLevel::Warn, error);
        }
    }

    fn apply_asset_changes(&mut self, changes: AssetChanges) {
        if changes.tree_changed || changes.rescan {
            self.refresh_tree_only();
        }
        if changes.rescan {
            self.state.previews.refresh_all();
        } else {
            self.state.previews.refresh_paths(&changes.paths);
        }
        self.state.refresh_lite_schemas(&changes);

        let prefab_reload = self.state.project.as_ref().map(|project| {
            let cached = project.cached_prefab_paths();
            let mut paths = changes
                .prefab_paths()
                .cloned()
                .collect::<std::collections::HashSet<_>>();
            paths.extend(
                cached
                    .iter()
                    .filter(|path| changes.rescan || changes.contains_path_or_parent(path))
                    .cloned(),
            );
            let reload_scene = !paths.is_empty();
            let (previous, errors) = project.reload_prefabs(&paths);
            (previous, reload_scene, errors)
        });
        let (prefab_previous, reload_scene, prefab_errors) = prefab_reload.unwrap_or_default();
        for error in prefab_errors {
            self.state.log.error(error);
        }
        if reload_scene
            && let Some(scene) = &mut self.state.scene
            && let Err(error) = scene.reload_prefabs()
        {
            self.state.log.error(error);
            if let Some(project) = &self.state.project {
                project.restore_prefabs(prefab_previous);
            }
        }

        let camera_settings_changed = changes.rescan
            || changes.camera_settings_paths().next().is_some()
            || self.state.scene.as_ref().is_some_and(|scene| {
                scene.view().assets.iter().any(|asset| {
                    asset
                        .path
                        .as_str()
                        .to_ascii_lowercase()
                        .ends_with(".camerasettings")
                        && changes.contains_path_or_parent(&asset.path)
                })
            });
        if camera_settings_changed {
            self.state.camera_settings_changed();
        }

        if let Some(project) = &self.state.project
            && let Some(gpu) = &mut self.gpu
            && let Err(error) = gpu.scene.reload_changed(
                &mut gpu.renderer,
                project,
                &changes,
                &mut self.state.model_loader,
                &mut self.state.log,
            )
        {
            self.fail(&error);
        }
    }

    fn new_scene(&mut self) {
        let loader = self.state.project.as_ref().map(OpenProject::prefab_loader);
        let scene = EditorScene::new_untitled().and_then(|mut scene| {
            if let Some(loader) = loader {
                scene.set_prefab_loader(loader)?;
            }
            Ok(scene)
        });
        match scene {
            Ok(scene) => {
                self.set_scene(scene);
                self.offer_recovery();
            }
            Err(error) => self.state.log.error(error),
        }
    }

    fn autosave_store(&self) -> Option<AutosaveStore> {
        Some(AutosaveStore::for_project(
            &self.state.project.as_ref()?.root,
        ))
    }

    /// Предлагает восстановить автосохранение открытой сцены, если оно осталось.
    fn offer_recovery(&mut self) {
        let scene = self
            .state
            .scene
            .as_ref()
            .and_then(|scene| scene.path().cloned());
        if let Some(contents) = self
            .autosave_store()
            .and_then(|store| store.read(scene.as_ref()))
        {
            self.state.recovery_prompt = Some(RecoveryPrompt { scene, contents });
        }
    }

    fn remove_autosave(&mut self, scene: Option<&VirtualPath>) {
        if let Some(store) = self.autosave_store()
            && let Err(error) = store.remove(scene)
        {
            self.state.log.push(LogLevel::Warn, error);
        }
    }

    /// Записывает копию изменённой сцены не чаще `AUTOSAVE_INTERVAL`.
    fn autosave_tick(&mut self) {
        let Some(scene) = &self.state.scene else {
            return;
        };
        let revision = scene.revision();
        // Пока решение о копии прошлого сеанса не принято, её нельзя перезаписать.
        if self.state.is_playing()
            || self.state.recovery_prompt.is_some()
            || !autosave_due(
                scene.is_dirty(),
                revision,
                self.autosaved_revision,
                self.last_autosave.elapsed(),
            )
        {
            return;
        }
        let Some(store) = self.autosave_store() else {
            return;
        };
        // Неудачная запись повторится через интервал, а не только после новой правки.
        self.last_autosave = Instant::now();
        let result = scene
            .document()
            .to_toml()
            .map_err(|error| error.to_string())
            .and_then(|text| store.write(scene.path(), &text));
        match result {
            Ok(()) => self.autosaved_revision = Some(revision),
            Err(error) => self
                .state
                .log
                .push(LogLevel::Warn, format!("автосохранение: {error}")),
        }
    }

    /// Срок следующего автосохранения, если в сцене есть незаписанные изменения.
    fn autosave_deadline(&self) -> Option<Instant> {
        let scene = self.state.scene.as_ref()?;
        let waiting = scene.is_dirty()
            && !self.state.is_playing()
            && self.state.recovery_prompt.is_none()
            && self.autosaved_revision != Some(scene.revision());
        waiting.then(|| self.last_autosave + AUTOSAVE_INTERVAL)
    }

    fn restore_autosave(&mut self) {
        let Some(prompt) = self.state.recovery_prompt.take() else {
            return;
        };
        let loader = self.state.project.as_ref().map(OpenProject::prefab_loader);
        let recovered = match loader {
            Some(loader) => {
                EditorScene::recover_with_loader(prompt.scene, &prompt.contents, loader)
            }
            None => EditorScene::recover(prompt.scene, &prompt.contents),
        };
        match recovered {
            Ok(scene) => {
                self.set_scene(scene);
                self.state.log.info("Восстановлено автосохранение");
            }
            Err(error) => self
                .state
                .log
                .error(format!("автосохранение не восстановлено: {error}")),
        }
    }

    fn open_scene(&mut self, path: &str) {
        let Some(project) = &self.state.project else {
            return;
        };
        let result = VirtualPath::parse(path)
            .map_err(|_| format!("{path}: некорректный путь сцены"))
            .and_then(|path| {
                let contents = project.read_text(path.as_str())?;
                EditorScene::parse(path, &contents, project.prefab_loader())
            });
        match result {
            Ok(scene) => {
                self.set_scene(scene);
                self.offer_recovery();
            }
            Err(error) => self.state.log.error(error),
        }
    }

    fn set_scene(&mut self, scene: EditorScene) {
        self.state.scene = Some(scene);
        self.state.scene_replaced();
        self.state.recovery_prompt = None;
        self.autosaved_revision = None;
        self.last_autosave = Instant::now();
    }

    /// Спрашивает о несохранённых изменениях перед действием, отбрасывающим сцену.
    fn run_guarded(&mut self, action: Action) {
        let dirty = self.state.scene.as_ref().is_some_and(EditorScene::is_dirty);
        if dirty {
            self.state.unsaved_prompt = Some(UnsavedPrompt { then: action });
        } else {
            self.run_confirmed(action);
        }
    }

    fn run_confirmed(&mut self, action: Action) {
        match action {
            Action::NewScene => self.new_scene(),
            Action::OpenScene(Some(path)) => self.open_scene(&path),
            Action::OpenScene(None) => {
                if let Some(path) = self.pick_scene_file()
                    && let Some(path) = self.virtual_path_of(&path)
                {
                    self.open_scene(path.as_str());
                }
            }
            Action::OpenProject => {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.open_project(&dir);
                }
            }
            Action::OpenProjectAt(dir) => self.open_project(&dir),
            Action::NewProject | Action::CreateProject { .. } => {}
            Action::Exit => self.exit = true,
            Action::BuildPlayer => self.start_build(),
            Action::CreatePrefab(id) => self.create_prefab(&id),
            _ => {}
        }
    }

    /// Запускает `rve-build` в выбранный каталог `<папка>/<id проекта>`; сцена уже
    /// сохранена, потому что сборщик читает её с диска.
    fn start_build(&mut self) {
        if self.state.build.is_some() {
            self.state.log.error("Сборка уже идёт");
            return;
        }
        let Some(project) = &self.state.project else {
            return;
        };
        let (project_dir, id) = (project.root.clone(), project.settings.id.clone());
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_owned));
        let tool = match crate::build::locate_build_tool(
            exe_dir.as_deref(),
            &crate::build::checkout_workspace(),
            std::env::var_os("CARGO"),
        ) {
            Ok(tool) => tool,
            Err(error) => {
                self.state.log.error(error);
                return;
            }
        };
        let Some(folder) = rfd::FileDialog::new().pick_folder() else {
            return;
        };
        let output = match std::path::absolute(folder.join(id)) {
            Ok(output) => output,
            Err(error) => {
                self.state
                    .log
                    .error(format!("{}: {error}", folder.display()));
                return;
            }
        };
        if output.exists() {
            self.state.log.error(format!(
                "{}: каталог уже существует, выберите другую папку",
                output.display()
            ));
            return;
        }
        let command = tool.command(&project_dir, &output);
        match crate::build::BuildJob::spawn(command, output.clone()) {
            Ok(job) => {
                self.state
                    .log
                    .info(format!("Build Player: {}", output.display()));
                self.state.build = Some(job);
            }
            Err(error) => self.state.log.error(format!("rve-build: {error}")),
        }
    }

    /// Сохраняет поддерево объекта в `Assets/prefabs/<id>.rveprefab` и заменяет его
    /// экземпляром. Запись файла не входит в undo: undo вернёт поддерево, файл останется.
    fn create_prefab(&mut self, id: &str) {
        let (Some(project), Some(scene)) = (&self.state.project, &mut self.state.scene) else {
            return;
        };
        let result = VirtualPath::parse(&format!("prefabs/{id}.rveprefab"))
            .map_err(|_| format!("{id}: некорректное имя префаба"))
            .and_then(|path| {
                let file = project
                    .assets_dir
                    .resolve(path.as_str())
                    .map_err(|error| error.to_string())?;
                if file.exists() {
                    return Err(format!("/{}: файл уже существует", path.as_str()));
                }
                let prefab = scene.prefab_from_subtree(id, &format!("/{}", path.as_str()))?;
                let text = prefab.to_toml().map_err(|error| error.to_string())?;
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| format!("{}: {error}", parent.display()))?;
                }
                crate::scene_edit::write_atomic(&file, text.as_bytes())?;
                project.clear_prefabs();
                scene.replace_with_instance(id, &path)?;
                Ok(path)
            });
        match result {
            Ok(path) => {
                self.state
                    .log
                    .info(format!("Создан префаб /{}", path.as_str()));
                self.refresh_tree_only();
            }
            Err(error) => self.state.log.error(error),
        }
    }

    /// Переносит overrides объекта экземпляра в файл префаба и убирает их из сцены.
    /// Запись файла атомарна и не входит в undo сцены.
    fn apply_to_prefab(&mut self, id: &str) {
        let (Some(project), Some(scene)) = (&self.state.project, &mut self.state.scene) else {
            return;
        };
        let result = scene.prefab_with_override(id).and_then(|(path, prefab)| {
            let file = project
                .assets_dir
                .resolve(path.as_str())
                .map_err(|error| error.to_string())?;
            let text = prefab.to_toml().map_err(|error| error.to_string())?;
            crate::scene_edit::write_atomic(&file, text.as_bytes())?;
            project.clear_prefabs();
            scene.drop_override(id)?;
            Ok(path)
        });
        match result {
            Ok(path) => self
                .state
                .log
                .info(format!("Overrides перенесены в /{}", path.as_str())),
            Err(error) => self.state.log.error(error),
        }
    }

    /// Переносит вывод сборки в Console; возвращает, появились ли строки.
    fn poll_build(&mut self) -> bool {
        let Some(job) = &mut self.state.build else {
            return false;
        };
        let lines = job.poll();
        if job.is_finished() {
            self.state.build = None;
        }
        let any = !lines.is_empty();
        for (level, line) in lines {
            self.state.log.push(level, line);
        }
        any
    }

    fn poll_scripts_build(&mut self) -> bool {
        let Some(job) = &mut self.state.scripts_job else {
            return false;
        };
        let lines = job.poll();
        let finished = job.is_finished();
        let completion = job.completion().cloned();
        let any = !lines.is_empty();
        for (level, line) in lines {
            self.state.log.push(level, line);
        }
        if !finished {
            return any;
        }
        self.state.scripts_job = None;
        let succeeded = match completion {
            Some(completion) => match crate::build::scripts_library_from_completion(&completion) {
                Ok(Some(path)) => match self.reload_generated_library(&path) {
                    Ok(()) => true,
                    Err(error) => {
                        self.state.log.error(error);
                        false
                    }
                },
                Ok(None) => {
                    self.clear_generated_library();
                    true
                }
                Err(error) => {
                    self.state.log.error(error);
                    false
                }
            },
            None => {
                self.state
                    .log
                    .error("scripts build finished without an exit result");
                false
            }
        };
        if self.state.finish_pending_play(succeeded) {
            self.start_play_now();
        }
        true
    }

    fn reload_generated_library(&mut self, source: &Path) -> Result<(), String> {
        if self.state.play.is_some() {
            return Err("scripts cannot be reloaded during Play".to_owned());
        }
        let project = self
            .state
            .project
            .as_ref()
            .ok_or_else(|| "no project is open".to_owned())?;
        let generation = crate::build::copy_script_generation(
            &project.root,
            source,
            self.state.script_generation.as_ref(),
        )
        .map_err(|error| format!("{}: {error}", source.display()))?;
        let Some(generation) = generation else {
            return Ok(());
        };

        let old_generation = self.state.script_generation.take();
        drop(self.state.library.take());
        if let Some(old) = old_generation
            && let Err(error) = std::fs::remove_file(&old.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            let _ = std::fs::remove_file(&generation.path);
            return Err(format!("{}: {error}", old.path.display()));
        }
        // SAFETY: generated project code is trusted for Play; the generation copy remains present.
        match unsafe { ScriptLibrary::load(&generation.path) } {
            Ok(library) => {
                self.state.library = Some(library);
                self.state.script_generation = Some(generation);
                Ok(())
            }
            Err(error) => {
                let _ = std::fs::remove_file(&generation.path);
                Err(format!("{}: {error}", generation.path.display()))
            }
        }
    }

    fn clear_generated_library(&mut self) {
        let generation = self.state.script_generation.take();
        drop(self.state.library.take());
        if let Some(generation) = generation
            && let Err(error) = std::fs::remove_file(&generation.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            self.state.log.push(
                LogLevel::Warn,
                format!("{}: {error}", generation.path.display()),
            );
        }
    }

    fn assets_root(&self) -> Option<PathBuf> {
        Some(self.state.project.as_ref()?.assets_dir.root().to_owned())
    }

    fn pick_scene_file(&self) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_directory(self.assets_root()?)
            .add_filter("RVE Scene", &["rvescene"])
            .add_filter("RVE Prefab", &["rveprefab"])
            .pick_file()
    }

    fn virtual_path_of(&mut self, file: &Path) -> Option<VirtualPath> {
        let project = self.state.project.as_ref()?;
        match project.assets_dir.virtual_path_of(file) {
            Ok(path) => Some(path),
            Err(error) => {
                self.state.log.error(error.to_string());
                None
            }
        }
    }

    fn save(&mut self, choose_path: bool) -> bool {
        let existing = self
            .state
            .scene
            .as_ref()
            .and_then(|scene| scene.path().cloned());
        let target = match existing.clone() {
            Some(path) if !choose_path => path,
            _ => {
                let Some(root) = self.assets_root() else {
                    return false;
                };
                let prefab = self
                    .state
                    .scene
                    .as_ref()
                    .is_some_and(|scene| scene.document().is_prefab());
                let kind = if prefab {
                    ("RVE Prefab", "rveprefab", "prefab")
                } else {
                    ("RVE Scene", "rvescene", "scene")
                };
                let Some(file) = rfd::FileDialog::new()
                    .set_directory(root)
                    .add_filter(kind.0, &[kind.1])
                    .set_file_name(format!("{}.{}", kind.2, kind.1))
                    .save_file()
                else {
                    return false;
                };
                let Some(path) = self.virtual_path_of(&file) else {
                    return false;
                };
                path
            }
        };
        let (Some(project), Some(scene)) = (&self.state.project, &mut self.state.scene) else {
            return false;
        };
        let result = project
            .assets_dir
            .resolve(target.as_str())
            .map_err(|error| error.to_string())
            .and_then(|file| scene.save(&file, target.clone()));
        match result {
            Ok(()) => {
                self.state
                    .log
                    .info(format!("Сохранено {}", target.as_str()));
                // Сохранённый префаб видят следующие раскрытия.
                if let Some(project) = &self.state.project {
                    project.clear_prefabs();
                }
                self.refresh_tree_only();
                // Копии прежнего и нового пути устарели: на диске теперь актуальная сцена.
                self.remove_autosave(existing.as_ref());
                self.remove_autosave(Some(&target));
                true
            }
            Err(error) => {
                self.state.log.error(error);
                false
            }
        }
    }

    fn refresh_tree_only(&mut self) {
        if let Some(project) = &self.state.project {
            match project.assets_dir.list() {
                Ok(tree) => self.state.asset_tree = tree,
                Err(error) => self.state.log.error(error.to_string()),
            }
        }
    }

    fn start_play(&mut self) {
        let Some(scene) = &self.state.scene else {
            return;
        };
        if scene.document().is_prefab() {
            self.state
                .log
                .error("Play недоступен для префаба: откройте сцену");
            return;
        }
        if !self.state.request_play() {
            if self.state.scripts_job.is_none() && !self.start_scripts_build() {
                self.state.pending_play = false;
            }
            return;
        }
        self.start_play_now();
    }

    fn start_play_now(&mut self) {
        let Some(scene) = &self.state.scene else {
            return;
        };
        if scene.document().is_prefab() {
            self.state
                .log
                .error("Play недоступен для префаба: откройте сцену");
            return;
        }
        let needs_library = scene
            .view()
            .objects
            .iter()
            .any(|object| !object.heavy_scripts.is_empty());
        if needs_library && self.state.library.is_none() {
            if self.state.scripts_path.is_none() {
                self.state
                    .log
                    .error("В сцене есть Rust-скрипты, но библиотека не загружена");
                return;
            }
            if !self.load_manual_library() {
                return;
            }
        }
        let (Some(project), Some(scene)) = (&self.state.project, &self.state.scene) else {
            return;
        };
        let result = (|| {
            let mut world =
                World::from_document(scene.view()).map_err(|error| error.to_string())?;
            project.apply_camera_settings(&mut world)?;
            let model_geometry = PlaySession::load_model_geometry(&world, &project.assets)?;
            PlaySession::start_with_world_and_physics(
                scene.view(),
                world,
                self.state.library.as_ref(),
                &project.settings.input,
                |path| project.read_text(path),
                &model_geometry,
                &project.settings.physics,
            )
        })();
        match result {
            Ok(session) => {
                self.state.play = Some(session);
                self.state.gizmo_drag = None;
                self.state.log.info("Play");
            }
            Err(error) => self.state.log.error(error),
        }
    }

    /// Loads the explicit `--scripts` library used for Inspector schemas and Play.
    fn load_manual_library(&mut self) -> bool {
        if self.state.library.is_some() {
            return true;
        }
        let Some(path) = self.state.scripts_path.clone() else {
            return false;
        };
        // SAFETY: библиотека скриптов — нативный код проекта, явно указанный пользователем.
        match unsafe { ScriptLibrary::load(&path) } {
            Ok(library) => {
                self.state.library = Some(library);
                true
            }
            Err(error) => {
                self.state.log.error(format!("{}: {error}", path.display()));
                false
            }
        }
    }

    fn stop_play(&mut self) {
        self.state.pending_play = false;
        if let Some(session) = self.state.play.take() {
            let events = session.stop();
            self.state.log.push_script_events(events);
            self.state.log.info("Stop");
        }
    }

    fn import(&mut self, files: &[PathBuf]) {
        let Some(project) = &self.state.project else {
            return;
        };
        for file in files {
            match project.assets_dir.import(file, &self.state.current_folder) {
                Ok(path) => self.state.log.info(format!("Импортирован {path}")),
                Err(error) => self.state.log.error(format!("{}: {error}", file.display())),
            }
        }
        self.refresh_tree_only();
    }

    fn run_actions(&mut self) {
        for action in std::mem::take(&mut self.state.actions) {
            match action {
                Action::Save => {
                    if !self.state.is_playing() {
                        self.save(false);
                    }
                }
                Action::SaveAs => {
                    if !self.state.is_playing() {
                        self.save(true);
                    }
                }
                Action::Play => self.start_play(),
                Action::Stop => self.stop_play(),
                Action::NewProject => {
                    self.state.new_project_dialog =
                        Some(crate::new_project::NewProjectDialog::default());
                }
                Action::CreateProject { name, parent } => {
                    match crate::new_project::create_project(&name, &parent) {
                        Ok(dir) => {
                            self.state.new_project_dialog = None;
                            self.run_guarded(Action::OpenProjectAt(dir));
                        }
                        Err(error) => {
                            if let Some(dialog) = &mut self.state.new_project_dialog {
                                dialog.error = Some(error);
                            }
                        }
                    }
                }
                Action::ImportDialog => {
                    if let Some(files) = rfd::FileDialog::new().pick_files() {
                        self.import(&files);
                    }
                }
                Action::ImportFiles(files) => self.import(&files),
                Action::RefreshTree => self.refresh_tree_only(),
                Action::BuildPlayer => {
                    if self.state.is_playing() {
                        self.state.log.error("Сначала остановите Play-режим");
                    } else {
                        self.run_guarded(Action::BuildPlayer);
                    }
                }
                Action::Discard(then) => {
                    // Отказ от изменений делает копию ненужной.
                    let scene = self
                        .state
                        .scene
                        .as_ref()
                        .and_then(|scene| scene.path().cloned());
                    self.remove_autosave(scene.as_ref());
                    self.run_confirmed(*then);
                }
                Action::RestoreAutosave => self.restore_autosave(),
                Action::DiscardAutosave => {
                    if let Some(prompt) = self.state.recovery_prompt.take() {
                        self.remove_autosave(prompt.scene.as_ref());
                    }
                }
                Action::SaveThen(then) => {
                    if self.save(false) {
                        self.run_confirmed(*then);
                    }
                }
                Action::ApplyToPrefab(id) => {
                    if self.state.is_playing() {
                        self.state.log.error("Сначала остановите Play-режим");
                    } else {
                        self.apply_to_prefab(&id);
                    }
                }
                Action::CreatePrefab(id) => {
                    if self.state.is_playing() {
                        self.state.log.error("Сначала остановите Play-режим");
                    } else {
                        self.run_confirmed(Action::CreatePrefab(id));
                    }
                }
                action @ (Action::NewScene
                | Action::OpenScene(_)
                | Action::OpenProject
                | Action::OpenProjectAt(_)
                | Action::Exit) => {
                    if action == Action::Exit {
                        self.stop_play();
                    }
                    if self.state.is_playing() {
                        self.state.log.error("Сначала остановите Play-режим");
                    } else {
                        self.run_guarded(action);
                    }
                }
            }
        }
    }

    fn update_title(&mut self, window: &Window) {
        let mut title = "RVE Editor".to_owned();
        if let Some(project) = &self.state.project {
            title.push_str(" — ");
            title.push_str(&project.settings.name);
        }
        if let Some(scene) = &self.state.scene {
            title.push_str(" — ");
            title.push_str(scene.path().map_or("untitled", |path| path.as_str()));
            if scene.is_dirty() {
                title.push('*');
            }
        }
        if title != self.window_title {
            window.set_title(&title);
            self.window_title = title;
        }
    }

    /// Рисует кадр; ошибка renderer завершает приложение.
    fn draw_frame(&mut self, window: &Window) -> Result<(), RenderError> {
        self.drain_engine_log();
        let (Some(gpu), Some(egui_state)) = (&mut self.gpu, &mut self.egui_state) else {
            return Ok(());
        };
        self.state
            .previews
            .upload(&mut gpu.renderer, &mut self.bridge, &mut self.state.log)?;
        if self.state.viewport_texture.is_none() {
            self.state.viewport_texture = Some(self.bridge.register_viewport(gpu.scene.viewport));
        }
        if self.state.game_viewport_texture.is_none() {
            self.state.game_viewport_texture =
                Some(self.bridge.register_viewport(gpu.scene.game_viewport));
        }

        let mut raw_input = egui_state.take_egui_input(window);
        if cfg!(target_os = "macos") {
            crate::gestures::remap_control_click(&mut raw_input.events, &mut self.control_click);
        }
        let console_lines = if self.state.console_open {
            snapshot_console_lines(&self.state)
        } else {
            Vec::new()
        };
        let command_catalog = self
            .state
            .play
            .as_ref()
            .map_or_else(CommandCatalog::default, |play| play.command_catalog());
        let state = &mut self.state;
        let dock = &mut self.dock;
        let console_overlay = &mut self.console_overlay;
        let console_open = state.console_open;
        let mut console_output = Default::default();
        let mut output = self.egui_ctx.run_ui(raw_input, |ui| {
            ui::show(ui, state, dock);
            if console_open && let Some(rect) = state.game_view.rect {
                console_output =
                    console_overlay.show(ui.ctx(), rect, &console_lines, &command_catalog);
            }
            if console_output.toggled {
                state.console_open = console_overlay.is_open();
            }
            state.text_focused_last_frame = ui.ctx().egui_wants_keyboard_input();
        });
        if let Some(command) = console_output.submitted {
            submit_console_command(&mut self.state, &command);
            window.request_redraw();
        }
        egui_state.handle_platform_output(window, std::mem::take(&mut output.platform_output));
        let repaint_delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(Duration::MAX, |viewport| viewport.repaint_delay);

        self.bridge
            .managed
            .apply_updates(&output.textures_delta, &mut gpu.renderer)?;
        let pixels_per_point = output.pixels_per_point;
        let shapes = std::mem::take(&mut output.shapes);
        let primitives = self.egui_ctx.tessellate(shapes, pixels_per_point);
        let meshes: Vec<ConvertedMesh<'_>> = self.bridge.convert(&primitives, pixels_per_point);
        let ui_primitives: Vec<UiPrimitive<'_>> =
            meshes.iter().map(ConvertedMesh::primitive).collect();

        let play_error = self.state.play.as_mut().and_then(|play| play.tick().err());
        if let Some(error) = play_error {
            self.state.log.error(error);
            if let Some(session) = self.state.play.take() {
                self.state.log.push_script_events(session.stop());
                self.state.log.info("Stop");
            }
        } else if let Some(play) = &mut self.state.play {
            let events = play.drain_events();
            self.state.log.push_script_events(events);
        }

        // Камера могла сместиться к новой модели: гизмо и маркеры нужно перестроить.
        // Пока модель загружается, кадр запросит готовность загрузки, а не этот флаг.
        let framing = self.state.pending_frame.is_some();
        let status = render_scene_frame(gpu, &mut self.state, &ui_primitives)?;
        let framing = framing && self.state.pending_frame.is_none();

        self.bridge
            .managed
            .apply_frees(&output.textures_delta, &mut gpu.renderer)?;
        output.textures_delta.clear();

        // Пропущенный кадр не показал обновление, поэтому он повторяется.
        let playing_now = self
            .state
            .play
            .as_ref()
            .is_some_and(|play| !play.is_paused());
        let size = window.inner_size();
        let skipped = status == FrameStatus::Skipped && size.width != 0 && size.height != 0;
        if playing_now || skipped || framing || repaint_delay.is_zero() {
            window.request_redraw();
            self.repaint_at = None;
        } else {
            self.repaint_at = Instant::now().checked_add(repaint_delay);
        }
        Ok(())
    }

    fn drain_engine_log(&mut self) {
        let lines = self
            .engine_log
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take_lines();
        for line in lines {
            let level = match line.level {
                ConsoleLevel::Info => LogLevel::Info,
                ConsoleLevel::Warn => LogLevel::Warn,
                ConsoleLevel::Error => LogLevel::Error,
            };
            self.state.log.push(level, line.text);
        }
    }
}

fn console_modal_open(state: &EditorState) -> bool {
    state.unsaved_prompt.is_some()
        || state.project_dialog.is_some()
        || state.new_project_dialog.is_some()
        || state.recovery_prompt.is_some()
        || state.apply_prompt.is_some()
}

fn toggle_console(state: &mut EditorState, overlay: &mut ConsoleOverlay) -> bool {
    if console_modal_open(state) {
        return false;
    }
    state.console_open = !state.console_open;
    overlay.set_open(state.console_open);
    true
}

fn snapshot_console_lines(state: &EditorState) -> Vec<ConsoleLine> {
    state
        .log
        .entries()
        .map(|entry| ConsoleLine {
            level: match entry.level {
                LogLevel::Info => ConsoleLevel::Info,
                LogLevel::Warn => ConsoleLevel::Warn,
                LogLevel::Error => ConsoleLevel::Error,
            },
            source: "editor".to_owned(),
            text: entry.message.clone(),
        })
        .collect()
}

fn submit_console_command(state: &mut EditorState, text: &str) {
    let command = match parse_command_line(text) {
        Ok(command) => command,
        Err(error) => {
            state.log.info(format!("> {text}"));
            state.log.error(error.to_string());
            return;
        }
    };
    if command.name == "clear" {
        if !command.args.is_empty() {
            state.log.info(format!("> {text}"));
            state.log.error("usage: clear");
            return;
        }
        state.log.clear();
        state.log.info(format!("> {text}"));
        return;
    }

    state.log.info(format!("> {text}"));
    if command.name == "help" {
        if command.args.len() > 1 {
            state.log.error("usage: help [prefix]");
            return;
        }
        let catalog = state
            .play
            .as_ref()
            .map_or_else(CommandCatalog::default, |play| play.command_catalog());
        let entries = catalog.help(command.args.first().map_or("", String::as_str));
        if entries.is_empty() {
            state.log.info("no commands match the prefix");
        } else {
            for entry in entries {
                state.log.info(entry);
            }
        }
        return;
    }

    let Some(play) = &mut state.play else {
        state.log.error("доступно только в Play");
        return;
    };
    let result = play.execute_command(&command);
    state.log.push_script_events(play.drain_events());
    if let Err(error) = result {
        state.log.error(error.to_string());
    }
}

fn editor_log_buffer() -> Arc<Mutex<ConsoleBuffer>> {
    static BUFFER: OnceLock<Arc<Mutex<ConsoleBuffer>>> = OnceLock::new();
    let buffer = Arc::clone(BUFFER.get_or_init(|| Arc::new(Mutex::new(ConsoleBuffer::default()))));
    let _ = install_logger(Some(buffer.clone()));
    buffer
}

/// Кадр с видимыми панелями Scene и Game. Без мира сцены рисуется только интерфейс.
fn render_scene_frame(
    gpu: &mut GpuState,
    state: &mut EditorState,
    ui: &[UiPrimitive<'_>],
) -> Result<FrameStatus, RenderError> {
    let drawable = |view: crate::state::SceneViewInfo| {
        (view.visible && view.size[0] > 0 && view.size[1] > 0).then_some(view.size)
    };
    let scene_size = drawable(state.scene_view);
    let game_size = drawable(state.game_view);
    let scene_debug_view = state.scene_view.debug_view;
    let game_debug_view = state.game_view.debug_view;
    if scene_size.is_none() && game_size.is_none() {
        return render_ui_only(&mut gpu.renderer, ui, &[]);
    }
    let playing = state.is_playing();
    let game_camera_id = state.game_camera.clone();
    // Сетка нужна только панели Scene вне Play-режима.
    let grid_camera_distance = scene_size
        .filter(|_| !playing)
        .map(|_| state.camera.distance);
    let pending_frame = state.pending_frame.take();
    // Модель ещё разбирается в фоне: наведение камеры ждёт её готовности.
    let mut still_loading = None;
    let (scene_frame, game_camera, game_settings, game_camera_handle, has_light, frame_target) = {
        let Some(parts) = state.frame_parts() else {
            return render_ui_only(&mut gpu.renderer, ui, &[]);
        };
        gpu.scene.sync_assets(
            &mut gpu.renderer,
            parts.project,
            parts.world,
            parts.models,
            parts.loader,
            parts.log,
        )?;
        let (scene_frame, warnings) = parts.world.build_frame();
        for warning in warnings {
            parts.log.push(
                LogLevel::Warn,
                format!(
                    "{}: {}: {}",
                    warning.source, warning.object_id, warning.message
                ),
            );
        }
        let game_camera_handle = game_camera_id
            .and_then(|id| parts.world.handle(&id))
            .filter(|handle| parts.world.camera_of(*handle).is_some())
            .unwrap_or_else(|| parts.world.active_camera());
        let game_camera = parts
            .world
            .camera_of(game_camera_handle)
            .unwrap_or(scene_frame.camera);
        let game_settings = parts
            .world
            .camera_settings(game_camera_handle)
            .copied()
            .unwrap_or_default();
        let has_light = parts
            .world
            .objects()
            .iter()
            .any(|object| object.directional_light.is_some());
        let frame_target = pending_frame.and_then(|id| {
            let object = parts.world.object_by_id(&id)?;
            let path = object.mesh_renderer.as_ref()?.model.as_str();
            let Some(model) = parts.models.get(path) else {
                if parts.loader.is_pending(path) {
                    still_loading = Some(id.clone());
                }
                return None;
            };
            Some(bounding_sphere(
                parts.world.world_matrix(object.handle)?,
                model.bounds()?,
            ))
        });
        (
            scene_frame,
            game_camera,
            game_settings,
            game_camera_handle,
            has_light,
            frame_target,
        )
    };
    state.pending_frame = still_loading;
    if let Some((center, radius)) = frame_target
        && state.camera.needs_framing(center, radius)
    {
        state.camera.focus(center, radius);
    }
    let scene_camera = if playing {
        scene_frame.camera
    } else {
        state.camera.camera()
    };
    let scene_settings = if playing {
        scene_frame.view
    } else {
        CameraSettings::default()
    };
    let Some(parts) = state.frame_parts() else {
        return render_ui_only(&mut gpu.renderer, ui, &[]);
    };
    let (local_lights, local_light_shadows) = render_local_lights(&scene_frame.local_lights);
    gpu.scene.begin_motion_frame(game_camera_handle, playing);
    gpu.scene.build_draws(
        &mut gpu.renderer,
        parts.world,
        &scene_frame,
        scene_camera.position,
        game_camera.position,
        grid_camera_distance,
    )?;
    gpu.scene.sync_environment(
        &mut gpu.renderer,
        scene_frame.environment.as_ref(),
        parts.log,
    )?;
    let scene_shadow_cascades = scene_size.map_or_else(Vec::new, |[width, height]| {
        SceneRenderer::shadow_cascades(scene_camera, width, height, &scene_frame.light, has_light)
    });
    let game_shadow_cascades = game_size.map_or_else(Vec::new, |[width, height]| {
        SceneRenderer::shadow_cascades(game_camera, width, height, &scene_frame.light, has_light)
    });
    if let Some([width, height]) = scene_size {
        gpu.renderer
            .resize_viewport(gpu.scene.viewport, width, height)?;
    }
    if let Some([width, height]) = game_size {
        gpu.renderer
            .resize_viewport(gpu.scene.game_viewport, width, height)?;
    }
    let scene_view = scene_size.map(|[width, height]| {
        gpu.scene.frame(
            scene_camera,
            width,
            height,
            scene_settings,
            &scene_frame.light,
            &local_lights,
            &local_light_shadows,
            has_light,
            scene_frame.environment.as_ref(),
            &scene_shadow_cascades,
            true,
            scene_debug_view,
        )
    });
    let game_view = game_size.map(|[width, height]| {
        gpu.scene.frame(
            game_camera,
            width,
            height,
            game_settings,
            &scene_frame.light,
            &local_lights,
            &local_light_shadows,
            has_light,
            scene_frame.environment.as_ref(),
            &game_shadow_cascades,
            false,
            game_debug_view,
        )
    });
    let mut scenes = Vec::with_capacity(2);
    if let Some(frame) = &scene_view {
        scenes.push((gpu.scene.viewport, frame));
    }
    if let Some(frame) = &game_view {
        scenes.push((gpu.scene.game_viewport, frame));
    }
    let drawn = scenes
        .iter()
        .map(|(viewport, _)| *viewport)
        .collect::<Vec<_>>();
    let primitives = visible_primitives(ui, &drawn);
    let result = gpu
        .renderer
        .render_frame_with_ui(&scenes, &primitives, WINDOW_CLEAR);
    if result.is_ok() {
        gpu.scene.commit_motion_history();
    }
    result
}

/// Примитивы без изображений viewport, которые не рисуются в этом кадре.
fn visible_primitives<'a>(
    primitives: &[UiPrimitive<'a>],
    drawn: &[ViewportId],
) -> Vec<UiPrimitive<'a>> {
    primitives
        .iter()
        .filter(|primitive| match primitive.image {
            UiImage::Texture(_) => true,
            UiImage::Viewport(viewport) => drawn.contains(&viewport),
        })
        .copied()
        .collect()
}

/// Кадр без сцены.
fn render_ui_only(
    renderer: &mut Renderer,
    primitives: &[UiPrimitive<'_>],
    drawn: &[ViewportId],
) -> Result<FrameStatus, RenderError> {
    let filtered = visible_primitives(primitives, drawn);
    renderer.render_frame_with_ui(&[], &filtered, WINDOW_CLEAR)
}

impl WindowHandler for EditorApp {
    fn attach_proxy(&mut self, proxy: WindowProxy) {
        self.proxy = Some(proxy);
    }

    fn init(&mut self, window: Arc<Window>) -> bool {
        let size = window.inner_size();
        let renderer = Renderer::new(
            Arc::clone(&window),
            size.width,
            size.height,
            RendererConfig {
                present_mode: engine_render::PresentMode::Fifo,
                pipeline_cache: self.pipeline_cache.clone(),
                ..RendererConfig::default()
            },
        );
        let mut renderer = match renderer {
            Ok(renderer) => renderer,
            Err(error) => {
                self.fail(&error);
                return false;
            }
        };
        self.state.wireframe_supported = renderer.capabilities().wireframe;
        let scene = match SceneRenderer::new(&mut renderer) {
            Ok(scene) => scene,
            Err(error) => {
                self.fail(&error);
                return false;
            }
        };
        self.egui_state = Some(egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            Some(renderer.max_texture_size() as usize),
        ));
        self.gpu = Some(GpuState { renderer, scene });
        self.window = Some(Arc::clone(&window));
        if let Some(project) = self.initial_project.take() {
            self.open_project(&project);
        }
        window.request_redraw();
        true
    }

    fn window_event(&mut self, window: &Window, event: &WindowEvent) {
        if matches!(event, WindowEvent::DoubleTapGesture { .. }) {
            self.state.smart_zoom = true;
            window.request_redraw();
        }
        if let WindowEvent::KeyboardInput {
            event,
            is_synthetic,
            ..
        } = event
            && event.physical_key == PhysicalKey::Code(KeyCode::Backquote)
        {
            if event.state == ElementState::Pressed
                && !event.repeat
                && !*is_synthetic
                && self.toggle_console()
            {
                window.request_redraw();
            }
            return;
        }
        if let WindowEvent::DroppedFile(path) = event
            && self.state.project.is_some()
        {
            self.state.action(Action::ImportFiles(vec![path.clone()]));
        }
        if let Some(egui_state) = &mut self.egui_state {
            let response = egui_state.on_window_event(window, event);
            if response.repaint {
                window.request_redraw();
            }
        }
    }

    fn frame(&mut self, window: &Window) -> bool {
        self.run_actions();
        self.update_title(window);
        if self.exit {
            return false;
        }
        if let Err(error) = self.draw_frame(window) {
            self.fail(&error);
            return false;
        }
        if !self.state.actions.is_empty() {
            window.request_redraw();
        }
        !self.exit
    }

    fn resized(&mut self, width: u32, height: u32) {
        if let Some(gpu) = &mut self.gpu {
            gpu.renderer.resize(width, height);
        }
    }

    fn update(&mut self, window: &Window) -> bool {
        // Здесь, а не в кадре: при простое окно просыпается к сроку без перерисовки.
        let now = Instant::now();
        let watcher_errors = self
            .asset_watcher
            .as_mut()
            .map(|watcher| watcher.drain(&mut self.asset_changes, now))
            .unwrap_or_default();
        for error in watcher_errors {
            self.state.log.push(LogLevel::Warn, error);
        }
        if let Some(changes) = self.asset_changes.take_ready(now) {
            self.apply_asset_changes(changes);
            window.request_redraw();
        }
        if self.exit {
            return false;
        }
        self.autosave_tick();
        let previews_pending = self.state.previews.has_pending();
        let build_output = self.poll_build();
        let scripts_output = self.poll_scripts_build();
        let background_pending = self.background_pending();
        if let Some(deadline) = self.repaint_at
            && Instant::now() >= deadline
        {
            self.repaint_at = None;
            window.request_redraw();
        } else if previews_pending || build_output || scripts_output || background_pending {
            window.request_redraw();
        }
        !self.exit
    }

    fn next_wakeup(&mut self) -> Option<Instant> {
        let preview = self
            .background_pending()
            .then(|| Instant::now() + PREVIEW_POLL);
        let build = self
            .state
            .build
            .is_some()
            .then(|| Instant::now() + BUILD_POLL);
        let scripts = self
            .state
            .scripts_job
            .is_some()
            .then(|| Instant::now() + BUILD_POLL);
        [
            self.repaint_at,
            preview,
            build,
            scripts,
            self.autosave_deadline(),
            self.asset_changes.deadline(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn close_requested(&mut self) -> bool {
        self.state.action(Action::Exit);
        self.run_actions();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
        self.exit
    }

    fn exiting(&mut self) {
        self.stop_play();
        self.asset_watcher = None;
        self.proxy = None;
        if let Some(job) = self.state.build.take() {
            job.kill();
        }
        if let Some(job) = self.state.scripts_job.take() {
            job.kill();
        }
        self.save_settings();
        // Renderer удерживает окно и освобождается до него.
        self.gpu = None;
        self.egui_state = None;
        self.window = None;
    }

    fn key(&mut self, key: Key, pressed: bool) {
        if self.state.console_open {
            return;
        }
        if (!pressed || self.state.pointer_over_view())
            && let Some(play) = &mut self.state.play
        {
            play.input_mut()
                .set_key(crate::input::map_key(key), pressed);
        }
    }

    fn mouse_button(&mut self, button: MouseButton, pressed: bool) {
        if self.state.console_open {
            return;
        }
        if (!pressed || self.state.pointer_over_view())
            && let Some(play) = &mut self.state.play
        {
            play.input_mut()
                .set_mouse_button(crate::input::map_mouse_button(button), pressed);
        }
    }

    fn focused(&mut self, focused: bool) {
        if !focused && let Some(play) = &mut self.state.play {
            play.clear_input();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Event, Key, Modifiers, RawInput, Rect, pos2, vec2};
    use engine_scene::SceneDocument;

    fn console_frame(
        ctx: &egui::Context,
        overlay: &mut ConsoleOverlay,
        events: Vec<Event>,
    ) -> engine_ui::console::ConsoleOutput {
        let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        let input = RawInput {
            screen_rect: Some(rect),
            events,
            ..RawInput::default()
        };
        let mut console_output = Default::default();
        let mut full_output = ctx.run_ui(input, |_ui| {
            console_output = overlay.show(ctx, rect, &[], &CommandCatalog::default());
        });
        full_output.textures_delta.clear();
        console_output
    }

    #[test]
    fn pending_play_rechecks_the_current_document_after_build() {
        let mut app = EditorApp::new(None, None);
        app.state.scene = Some(
            EditorScene::parse(
                VirtualPath::parse("prefabs/test.rveprefab").unwrap(),
                "[[objects]]\nid = \"root\"\n",
                crate::scene_edit::no_prefabs(),
            )
            .unwrap(),
        );
        app.start_play_now();
        assert!(!app.state.is_playing());
        assert!(
            app.state
                .log
                .entries()
                .any(|entry| entry.message.contains("префаба"))
        );
    }

    #[test]
    fn backquote_toggle_respects_editor_modals() {
        let mut state = EditorState::new(None);
        let mut overlay = ConsoleOverlay::default();
        assert!(toggle_console(&mut state, &mut overlay));
        assert!(state.console_open);
        assert!(overlay.is_open());
        assert!(toggle_console(&mut state, &mut overlay));
        assert!(!state.console_open);

        state.apply_prompt = Some("camera".to_owned());
        assert!(!toggle_console(&mut state, &mut overlay));
        assert!(!state.console_open);
        assert!(!overlay.is_open());
    }

    #[test]
    fn enter_submits_help_to_the_editor_console_log() {
        let mut state = EditorState::new(None);
        let ctx = egui::Context::default();
        let mut overlay = ConsoleOverlay::default();
        state.console_open = true;
        overlay.set_open(true);
        console_frame(&ctx, &mut overlay, Vec::new());
        console_frame(&ctx, &mut overlay, vec![Event::Text("help".to_owned())]);
        let output = console_frame(
            &ctx,
            &mut overlay,
            vec![Event::Key {
                key: Key::Enter,
                physical_key: Some(Key::Enter),
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }],
        );
        assert_eq!(output.submitted.as_deref(), Some("help"));
        submit_console_command(&mut state, output.submitted.as_deref().unwrap());
        assert!(
            state
                .log
                .entries()
                .any(|entry| entry.message.starts_with("help —"))
        );
    }

    #[test]
    fn clear_builtin_keeps_only_its_command_echo() {
        let mut state = EditorState::new(None);
        state.log.push(LogLevel::Warn, "old output");
        submit_console_command(&mut state, "clear");
        assert_eq!(state.log.entries().count(), 1);
        assert_eq!(state.log.entries().next().unwrap().message, "> clear");
    }

    #[test]
    fn script_commands_work_in_play_and_other_commands_are_rejected_outside_play() {
        let mut state = EditorState::new(None);
        submit_console_command(&mut state, "scene.reload");
        assert!(state.log.entries().any(|entry| {
            entry.level == LogLevel::Error && entry.message == "доступно только в Play"
        }));

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
        let script = r#"
local S = {}
function S:on_start()
  rve.console.register("editor.echo", function(args)
    rve.console.write(args[1], "warning")
  end, "write an editor message")
end
return S
"#;
        state.play = Some(PlaySession::start(&document, None, |_| Ok(script.to_owned())).unwrap());
        state.play.as_mut().unwrap().tick().unwrap();
        submit_console_command(&mut state, "editor.echo hello");
        assert!(
            state
                .log
                .entries()
                .any(|entry| { entry.level == LogLevel::Warn && entry.message.ends_with("hello") })
        );
    }
}
