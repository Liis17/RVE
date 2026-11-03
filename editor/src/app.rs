//! Приложение редактора: окно, цикл кадра egui + renderer и выполнение действий.

use crate::console::LogLevel;
use crate::layout::{Tab, default_layout};
use crate::play::PlaySession;
use crate::project::OpenProject;
use crate::scene_edit::EditorScene;
use crate::state::{Action, EditorState, UnsavedPrompt};
use crate::ui;
use crate::ui::scene_view::bounding_sphere;
use crate::ui_render::{ConvertedMesh, UiBridge};
use crate::viewport::render::SceneRenderer;
use egui_dock::DockState;
use engine_files::VirtualPath;
use engine_render::{
    FrameStatus, RenderError, Renderer, RendererConfig, UiImage, UiPrimitive, ViewportId,
};
use engine_script::ScriptLibrary;
use engine_window::{Key, MouseButton, Window, WindowEvent, WindowHandler};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Цвет окна за панелями; всё окно закрыто UI, поэтому заметен только при первом кадре.
const WINDOW_CLEAR: [f32; 4] = [0.02, 0.02, 0.025, 1.0];
/// Как часто проверять готовность фоновых превью.
const PREVIEW_POLL: Duration = Duration::from_millis(30);
/// Как часто забирать вывод идущей сборки.
const BUILD_POLL: Duration = Duration::from_millis(50);

pub struct EditorApp {
    state: EditorState,
    dock: DockState<Tab>,
    initial_project: Option<PathBuf>,
    gpu: Option<GpuState>,
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
    bridge: UiBridge,
    repaint_at: Option<Instant>,
    exit: bool,
    pub exit_code: u8,
    window_title: String,
    window: Option<Arc<Window>>,
}

struct GpuState {
    renderer: Renderer,
    scene: SceneRenderer,
}

impl EditorApp {
    pub fn new(project: Option<PathBuf>, scripts: Option<PathBuf>) -> Self {
        Self {
            state: EditorState::new(scripts),
            dock: default_layout(),
            initial_project: project,
            gpu: None,
            egui_ctx: egui::Context::default(),
            egui_state: None,
            bridge: UiBridge::default(),
            repaint_at: None,
            exit: false,
            exit_code: 0,
            window_title: String::new(),
            window: None,
        }
    }

    fn fail(&mut self, error: &RenderError) {
        eprintln!("ошибка renderer: {error}");
        self.exit_code = 1;
        self.exit = true;
    }

    fn open_project(&mut self, dir: &Path) {
        match OpenProject::open(dir) {
            Ok(project) => {
                self.state
                    .log
                    .info(format!("Открыт проект {}", project.settings.name));
                let start = project.settings.starting_scene().map(str::to_owned);
                self.state.project = Some(project);
                self.state.current_folder.clear();
                self.load_library();
                self.refresh_assets();
                match start {
                    Some(path) => self.open_scene(&path),
                    None => self.new_scene(),
                }
            }
            Err(error) => self.state.log.error(error),
        }
    }

    fn refresh_assets(&mut self) {
        let Some(project) = &self.state.project else {
            return;
        };
        match project.assets_dir.list() {
            Ok(tree) => self.state.asset_tree = tree,
            Err(error) => self.state.log.error(error.to_string()),
        }
        self.state.previews.invalidate();
        self.state.lite_schemas.clear();
        if let Some(gpu) = &mut self.gpu
            && let Err(error) = gpu
                .scene
                .clear_cache(&mut gpu.renderer, &mut self.state.models)
        {
            self.fail(&error);
        }
    }

    fn new_scene(&mut self) {
        match EditorScene::new_untitled() {
            Ok(scene) => self.set_scene(scene),
            Err(error) => self.state.log.error(error),
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
                EditorScene::parse(path, &contents)
            });
        match result {
            Ok(scene) => self.set_scene(scene),
            Err(error) => self.state.log.error(error),
        }
    }

    fn set_scene(&mut self, scene: EditorScene) {
        self.state.scene = Some(scene);
        self.state.scene_replaced();
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
            Action::Exit => self.exit = true,
            Action::BuildPlayer => self.start_build(),
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
        let (project_dir, id) = (project.dir.clone(), project.settings.id.clone());
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

    fn assets_root(&self) -> Option<PathBuf> {
        Some(self.state.project.as_ref()?.assets_dir.root().to_owned())
    }

    fn pick_scene_file(&self) -> Option<PathBuf> {
        rfd::FileDialog::new()
            .set_directory(self.assets_root()?)
            .add_filter("RVE Scene", &["rvescene"])
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
        let target = match existing {
            Some(path) if !choose_path => path,
            _ => {
                let Some(root) = self.assets_root() else {
                    return false;
                };
                let Some(file) = rfd::FileDialog::new()
                    .set_directory(root)
                    .add_filter("RVE Scene", &["rvescene"])
                    .set_file_name("scene.rvescene")
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
                self.refresh_tree_only();
                true
            }
            Err(error) => {
                self.state.log.error(error);
                false
            }
        }
    }

    fn refresh_tree_only(&mut self) {
        if let Some(project) = &self.state.project
            && let Ok(tree) = project.assets_dir.list()
        {
            self.state.asset_tree = tree;
        }
    }

    fn start_play(&mut self) {
        let (Some(_), Some(scene)) = (&self.state.project, &self.state.scene) else {
            return;
        };
        let needs_library = scene
            .document()
            .objects
            .iter()
            .any(|object| !object.heavy_scripts.is_empty());
        if needs_library && self.state.library.is_none() {
            if self.state.scripts_path.is_none() {
                self.state
                    .log
                    .error("В сцене есть Rust-скрипты: запустите редактор с --scripts <library>");
                return;
            }
            if !self.load_library() {
                return;
            }
        }
        let (Some(project), Some(scene)) = (&self.state.project, &self.state.scene) else {
            return;
        };
        let result = PlaySession::start(scene.document(), self.state.library.as_ref(), |path| {
            project.read_text(path)
        });
        match result {
            Ok(session) => {
                self.state.play = Some(session);
                self.state.gizmo_drag = None;
                self.state.log.info("Play");
            }
            Err(error) => self.state.log.error(error),
        }
    }

    /// Загружает `--scripts` один раз: схемы нужны Inspector, сама библиотека — Play.
    /// Выгрузки нет, поэтому после пересборки cdylib редактор нужно перезапустить.
    fn load_library(&mut self) -> bool {
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
        self.refresh_assets();
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
                Action::ImportDialog => {
                    if let Some(files) = rfd::FileDialog::new().pick_files() {
                        self.import(&files);
                    }
                }
                Action::ImportFiles(files) => self.import(&files),
                Action::RefreshAssets => self.refresh_assets(),
                Action::BuildPlayer => {
                    if self.state.is_playing() {
                        self.state.log.error("Сначала остановите Play-режим");
                    } else {
                        self.run_guarded(Action::BuildPlayer);
                    }
                }
                Action::Discard(then) => self.run_confirmed(*then),
                Action::SaveThen(then) => {
                    if self.save(false) {
                        self.run_confirmed(*then);
                    }
                }
                action @ (Action::NewScene
                | Action::OpenScene(_)
                | Action::OpenProject
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
        let (Some(gpu), Some(egui_state)) = (&mut self.gpu, &mut self.egui_state) else {
            return Ok(());
        };
        self.state
            .previews
            .upload(&mut gpu.renderer, &mut self.bridge)?;
        if self.state.viewport_texture.is_none() {
            self.state.viewport_texture = Some(self.bridge.register_viewport(gpu.scene.viewport));
        }
        if self.state.game_viewport_texture.is_none() {
            self.state.game_viewport_texture =
                Some(self.bridge.register_viewport(gpu.scene.game_viewport));
        }

        let raw_input = egui_state.take_egui_input(window);
        let state = &mut self.state;
        let dock = &mut self.dock;
        let mut output = self.egui_ctx.run_ui(raw_input, |ui| {
            ui::show(ui, state, dock);
        });
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

        if let Some(play) = &mut self.state.play {
            play.tick();
            let events = play.drain_events();
            self.state.log.push_script_events(events);
        }

        // Камера могла сместиться к новой модели: гизмо и маркеры нужно перестроить.
        let framing = self.state.pending_frame.is_some();
        let status = render_scene_frame(gpu, &mut self.state, &ui_primitives)?;

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
    if scene_size.is_none() && game_size.is_none() {
        return render_ui_only(&mut gpu.renderer, ui, &[]);
    }
    let playing = state.is_playing();
    let editor_camera = state.camera.camera();
    let game_camera_id = state.game_camera.clone();
    // Сетка нужна только панели Scene вне Play-режима.
    let grid_camera_distance = scene_size
        .filter(|_| !playing)
        .map(|_| state.camera.distance);
    let pending_frame = state.pending_frame.take();
    let Some(parts) = state.frame_parts() else {
        return render_ui_only(&mut gpu.renderer, ui, &[]);
    };
    gpu.scene.sync_assets(
        &mut gpu.renderer,
        parts.project,
        parts.world,
        parts.models,
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
    let game_camera = game_camera_id
        .and_then(|id| parts.world.handle(&id))
        .and_then(|handle| parts.world.camera_of(handle))
        .unwrap_or(scene_frame.camera);
    let has_light = parts
        .world
        .objects()
        .iter()
        .any(|object| object.directional_light.is_some());
    let scene_camera = if playing {
        scene_frame.camera
    } else {
        editor_camera
    };
    gpu.scene.build_draws(
        &mut gpu.renderer,
        parts.world,
        &scene_frame,
        scene_camera.position,
        game_camera.position,
        grid_camera_distance,
    )?;
    let frame_target = pending_frame.and_then(|id| {
        let object = parts.world.object_by_id(&id)?;
        let model = parts
            .models
            .get(object.mesh_renderer.as_ref()?.model.as_str())?;
        Some(bounding_sphere(
            parts.world.world_matrix(object.handle)?,
            model.bounds()?,
        ))
    });
    if let Some((center, radius)) = frame_target
        && state.camera.needs_framing(center, radius)
    {
        state.camera.focus(center, radius);
    }
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
            scene_camera.view_projection(width, height),
            scene_camera.position,
            &scene_frame.light,
            has_light,
            true,
        )
    });
    let game_view = game_size.map(|[width, height]| {
        gpu.scene.frame(
            game_camera.view_projection(width, height),
            game_camera.position,
            &scene_frame.light,
            has_light,
            false,
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
    gpu.renderer
        .render_frame_with_ui(&scenes, &primitives, WINDOW_CLEAR)
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
    fn init(&mut self, window: Arc<Window>) -> bool {
        let size = window.inner_size();
        let renderer = Renderer::new(
            Arc::clone(&window),
            size.width,
            size.height,
            RendererConfig::default(),
        );
        let mut renderer = match renderer {
            Ok(renderer) => renderer,
            Err(error) => {
                self.fail(&error);
                return false;
            }
        };
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
        let previews_pending = self.state.previews.has_pending();
        let build_output = self.poll_build();
        if let Some(deadline) = self.repaint_at
            && Instant::now() >= deadline
        {
            self.repaint_at = None;
            window.request_redraw();
        } else if previews_pending || build_output {
            window.request_redraw();
        }
        !self.exit
    }

    fn next_wakeup(&mut self) -> Option<Instant> {
        let preview = self
            .state
            .previews
            .has_pending()
            .then(|| Instant::now() + PREVIEW_POLL);
        let build = self
            .state
            .build
            .is_some()
            .then(|| Instant::now() + BUILD_POLL);
        [self.repaint_at, preview, build]
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
        if let Some(job) = self.state.build.take() {
            job.kill();
        }
        // Renderer удерживает окно и освобождается до него.
        self.gpu = None;
        self.egui_state = None;
        self.window = None;
    }

    fn key(&mut self, key: Key, pressed: bool) {
        if (!pressed || self.state.pointer_over_view())
            && let Some(play) = &mut self.state.play
        {
            play.input_mut()
                .set_key(crate::input::map_key(key), pressed);
        }
    }

    fn mouse_button(&mut self, button: MouseButton, pressed: bool) {
        if (!pressed || self.state.pointer_over_view())
            && let Some(play) = &mut self.state.play
        {
            play.input_mut()
                .set_mouse_button(crate::input::map_mouse_button(button), pressed);
        }
    }

    fn focused(&mut self, focused: bool) {
        if !focused && let Some(play) = &mut self.state.play {
            play.input_mut().release_all();
        }
    }
}
