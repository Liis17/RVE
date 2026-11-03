//! Состояние редактора, общее для панелей и обработчиков действий; не зависит от GPU.

use crate::assets_fs::{AssetEntry, AssetKind};
use crate::build::BuildJob;
use crate::console::ConsoleLog;
use crate::play::PlaySession;
use crate::previews::PreviewCache;
use crate::project::OpenProject;
use crate::scene_edit::{Component, EditorScene, NewObject};
use crate::viewport::camera::EditorCamera;
use crate::viewport::gizmo::{GizmoDrag, GizmoSpace, GizmoTool, SnapSettings};
use crate::viewport::model_loader::ModelLoader;
use engine_assets::Model;
use engine_files::VirtualPath;
use engine_scene::World;
use engine_script::{ParamSchema, ScriptInterruptHandle, ScriptLibrary};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

/// Сколько может выполняться chunk Lua при чтении схемы, прежде чем его прервут.
const SCHEMA_TIMEOUT: Duration = Duration::from_secs(5);

/// Действие, которое выполняется после построения UI: диалоги ОС блокируют кадр,
/// а смена проекта или сцены затрагивает данные, заимствованные панелями.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    NewScene,
    /// `None` — выбрать файл в диалоге.
    OpenScene(Option<String>),
    OpenProject,
    /// Открыть проект из списка недавних.
    OpenProjectAt(PathBuf),
    Save,
    SaveAs,
    Exit,
    Play,
    Stop,
    ImportDialog,
    ImportFiles(Vec<PathBuf>),
    /// Файловая структура изменилась: сбросить кэши ресурсов и прочитать дерево заново.
    RefreshAssets,
    /// Выбрать каталог и собрать релиз проекта через `rve-build`; сцена сохраняется заранее.
    BuildPlayer,
    /// Сохранить поддерево объекта в новый префаб и заменить его экземпляром.
    CreatePrefab(String),
    /// Перенести overrides объекта экземпляра в файл префаба.
    ApplyToPrefab(String),
    /// Сохранить сцену и при успехе выполнить действие.
    SaveThen(Box<Action>),
    /// Выполнить действие, отбросив несохранённые изменения.
    Discard(Box<Action>),
    /// Открыть сцену из найденного автосохранения.
    RestoreAutosave,
    /// Удалить найденное автосохранение.
    DiscardAutosave,
}

/// Найденное при открытии сцены автосохранение.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryPrompt {
    /// Сцена копии; `None` — несохранённая новая сцена.
    pub scene: Option<VirtualPath>,
    pub contents: String,
}

/// Подтверждение действия при несохранённых изменениях сцены.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsavedPrompt {
    pub then: Action,
}

/// Перетаскиваемый ресурс из панели Project.
#[derive(Clone, Debug)]
pub struct AssetDrag(pub String);

/// Сведения о панели Scene за текущий кадр.
#[derive(Clone, Copy, Debug, Default)]
pub struct SceneViewInfo {
    /// Панель показана и её изображение нужно нарисовать в этом кадре.
    pub visible: bool,
    /// Курсор над панелью: ввод Play-режима относится к сцене.
    pub hovered: bool,
    /// Размер изображения в физических пикселях.
    pub size: [u32; 2],
}

/// Раздельные заимствования состояния для подготовки кадра сцены.
pub struct FrameParts<'a> {
    pub world: &'a mut World,
    pub project: &'a OpenProject,
    pub models: &'a mut HashMap<String, Arc<Model>>,
    pub loader: &'a mut ModelLoader,
    pub log: &'a mut ConsoleLog,
}

pub struct EditorState {
    pub project: Option<OpenProject>,
    pub scene: Option<EditorScene>,
    pub log: ConsoleLog,
    pub tool: GizmoTool,
    pub space: GizmoSpace,
    pub snap: SnapSettings,
    pub camera: EditorCamera,
    pub gizmo_drag: Option<GizmoDrag>,
    pub play: Option<PlaySession>,
    pub scripts_path: Option<PathBuf>,
    /// Недавние проекты из настроек пользователя, последний открытый — первым.
    pub recent_projects: Vec<PathBuf>,
    pub library: Option<ScriptLibrary>,
    pub actions: Vec<Action>,
    pub unsaved_prompt: Option<UnsavedPrompt>,
    pub recovery_prompt: Option<RecoveryPrompt>,
    pub asset_tree: Vec<AssetEntry>,
    pub current_folder: String,
    pub project_dialog: Option<crate::ui::project_panel::ProjectDialog>,
    pub hierarchy_rename: Option<(String, String)>,
    /// В прошлом кадре фокус был в текстовом поле: Esc этого кадра закрывает поле,
    /// а не снимает выделение.
    pub text_focused_last_frame: bool,
    /// Строки поиска панелей Hierarchy и Project.
    pub hierarchy_filter: String,
    pub project_filter: String,
    /// Компонент, скопированный в Inspector для вставки в объекты.
    pub component_clipboard: Option<Component>,
    pub previews: PreviewCache,
    pub viewport_texture: Option<egui::TextureId>,
    pub scene_view: SceneViewInfo,
    pub game_viewport_texture: Option<egui::TextureId>,
    pub game_view: SceneViewInfo,
    /// Id камеры вкладки Game; панель заменяет отсутствующую камеру первой в документе.
    pub game_camera: Option<String>,
    /// Объект с новой моделью: после её загрузки камера покажет его, если он почти не виден.
    pub pending_frame: Option<String>,
    /// CPU-модели, загруженные для отрисовки: по ним работают picking и фокус камеры.
    pub models: HashMap<String, Arc<Model>>,
    /// Объект, чьи overrides ждут подтверждения переноса в префаб.
    pub apply_prompt: Option<String>,
    /// Идущая сборка релиза; одновременно — не больше одной.
    pub build: Option<BuildJob>,
    /// Схемы `properties` lite-скриптов по пути; ошибка пишется в Console один раз.
    /// Сбрасывается вместе с кэшами ресурсов.
    pub lite_schemas: HashMap<String, Result<Option<ParamSchema>, String>>,
    /// Фоновый разбор FBX для `models`.
    pub model_loader: ModelLoader,
    edit_world: Option<(u64, World)>,
}

impl EditorState {
    pub fn new(scripts_path: Option<PathBuf>) -> Self {
        Self {
            project: None,
            scene: None,
            log: ConsoleLog::default(),
            tool: GizmoTool::Move,
            space: GizmoSpace::Global,
            snap: SnapSettings::default(),
            camera: EditorCamera::default(),
            gizmo_drag: None,
            play: None,
            scripts_path,
            recent_projects: Vec::new(),
            library: None,
            actions: Vec::new(),
            unsaved_prompt: None,
            recovery_prompt: None,
            asset_tree: Vec::new(),
            current_folder: String::new(),
            project_dialog: None,
            hierarchy_rename: None,
            text_focused_last_frame: false,
            hierarchy_filter: String::new(),
            project_filter: String::new(),
            component_clipboard: None,
            previews: PreviewCache::default(),
            viewport_texture: None,
            scene_view: SceneViewInfo::default(),
            game_viewport_texture: None,
            game_view: SceneViewInfo::default(),
            game_camera: None,
            pending_frame: None,
            models: HashMap::new(),
            build: None,
            apply_prompt: None,
            lite_schemas: HashMap::new(),
            model_loader: ModelLoader::default(),
            edit_world: None,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.play.is_some()
    }

    /// Курсор над изображением сцены: ввод Play-режима относится к игре.
    pub fn pointer_over_view(&self) -> bool {
        self.scene_view.hovered || self.game_view.hovered
    }

    pub fn action(&mut self, action: Action) {
        self.actions.push(action);
    }

    /// Мир текущего кадра: копия Play-режима либо мир, собранный из документа.
    pub fn world_mut(&mut self) -> Option<&mut World> {
        if let Some(play) = &mut self.play {
            return Some(play.world_mut());
        }
        let scene = self.scene.as_ref()?;
        let revision = scene.revision();
        let stale = self
            .edit_world
            .as_ref()
            .is_none_or(|(built, _)| *built != revision);
        if stale {
            match world_for_view(scene.view()) {
                Ok(mut world) => {
                    if let Some(project) = &self.project
                        && let Err(error) = project.apply_camera_settings(&mut world)
                    {
                        self.log.error(error);
                    }
                    self.edit_world = Some((revision, world));
                }
                Err(error) => {
                    self.log.error(error.to_string());
                    return None;
                }
            }
        }
        self.edit_world.as_mut().map(|(_, world)| world)
    }

    /// Заимствует всё, что нужно для подготовки кадра сцены.
    pub fn frame_parts(&mut self) -> Option<FrameParts<'_>> {
        self.world_mut()?;
        let world = match &mut self.play {
            Some(play) => play.world_mut(),
            None => &mut self.edit_world.as_mut()?.1,
        };
        Some(FrameParts {
            world,
            project: self.project.as_ref()?,
            models: &mut self.models,
            loader: &mut self.model_loader,
            log: &mut self.log,
        })
    }

    /// Сбрасывает данные, связанные с прежней сценой.
    pub fn scene_replaced(&mut self) {
        self.edit_world = None;
        self.gizmo_drag = None;
        self.hierarchy_rename = None;
        self.game_camera = None;
        self.pending_frame = None;
    }

    /// Выполняет правку сцены и пишет ошибку в консоль; в Play-режиме документ не меняется.
    pub fn edit_scene<T>(
        &mut self,
        change: impl FnOnce(&mut EditorScene) -> Result<T, String>,
    ) -> Option<T> {
        if self.is_playing() {
            return None;
        }
        let scene = self.scene.as_mut()?;
        match change(scene) {
            Ok(value) => Some(value),
            Err(error) => {
                self.log.error(error);
                None
            }
        }
    }

    pub fn create_object(&mut self, kind: &NewObject, parent: Option<&str>) {
        self.edit_scene(|scene| scene.create_object(kind, parent));
    }

    /// Создаёт объект из перетащенного ресурса: модель или экземпляр префаба;
    /// `position` — локальная позиция.
    pub fn create_from_asset(&mut self, path: &str, parent: Option<&str>, position: [f32; 3]) {
        let asset_kind = AssetKind::of(path);
        if !matches!(asset_kind, AssetKind::Model | AssetKind::Prefab) {
            self.log.error(format!(
                "{path}: на сцену можно перетащить только .fbx или .rveprefab"
            ));
            return;
        }
        match VirtualPath::parse(path) {
            Ok(path) => {
                let kind = if asset_kind == AssetKind::Prefab {
                    NewObject::Prefab(path, position)
                } else {
                    NewObject::Mesh(path, position)
                };
                self.pending_frame = self.edit_scene(|scene| scene.create_object(&kind, parent));
            }
            Err(_) => self.log.error(format!("{path}: некорректный путь ресурса")),
        }
    }

    /// Схема `properties` lite-скрипта из кэша; при первом обращении файл читается из проекта.
    pub fn lite_schema(&mut self, path: &str) -> Result<Option<ParamSchema>, String> {
        if let Some(schema) = self.lite_schemas.get(path) {
            return schema.clone();
        }
        let Some(project) = &self.project else {
            return Ok(None);
        };
        let result = project
            .read_text(path)
            .and_then(|source| read_lite_schema(path, &source, SCHEMA_TIMEOUT));
        if let Err(error) = &result {
            self.log.error(error.clone());
        }
        self.lite_schemas.insert(path.to_owned(), result.clone());
        result
    }

    /// Схема Rust-скрипта из загруженной библиотеки.
    pub fn heavy_schema(&self, name: &str) -> Option<ParamSchema> {
        self.library.as_ref()?.schema(name).cloned()
    }

    /// Пути ресурсов заданного вида для выпадающих списков Inspector.
    pub fn assets_of_kind(&self, kind: AssetKind) -> Vec<String> {
        fn collect(entries: &[AssetEntry], kind: AssetKind, paths: &mut Vec<String>) {
            for entry in entries {
                if entry.is_dir {
                    collect(&entry.children, kind, paths);
                } else if AssetKind::of(&entry.path) == kind {
                    paths.push(entry.path.clone());
                }
            }
        }
        let mut paths = Vec::new();
        collect(&self.asset_tree, kind, &mut paths);
        paths
    }
}

/// Мир для показа раскрытого документа. Префабу без камеры добавляется скрытая камера
/// под корнем: `World` требует камеру, а вид Scene всё равно использует камеру редактора.
fn world_for_view(view: &engine_scene::SceneDocument) -> Result<World, engine_scene::SceneError> {
    if !view.is_prefab() || view.objects.iter().any(|object| object.camera.is_some()) {
        return World::from_document(view);
    }
    let mut view = view.clone();
    let root = view
        .objects
        .iter()
        .find(|object| object.parent.is_none())
        .map(|object| object.id.clone());
    view.objects.push(engine_scene::SceneObject {
        id: EDITOR_CAMERA_ID.to_owned(),
        name: None,
        parent: root,
        transform: engine_scene::SceneTransform::default(),
        camera: Some(engine_scene::CameraComponent::default()),
        mesh_renderer: None,
        lod_group: None,
        directional_light: None,
        lite_scripts: Vec::new(),
        heavy_scripts: Vec::new(),
    });
    World::from_document(&view)
}

/// Id скрытой камеры префаба в мире вида; документ его не содержит.
const EDITOR_CAMERA_ID: &str = "__editor_camera";

/// Читает схему в отдельном Lua; chunk дольше `timeout` прерывается.
fn read_lite_schema(
    path: &str,
    source: &str,
    timeout: Duration,
) -> Result<Option<ParamSchema>, String> {
    let interrupt = ScriptInterruptHandle::new();
    let (done, finished) = mpsc::channel::<()>();
    let watchdog = {
        let interrupt = interrupt.clone();
        std::thread::spawn(move || {
            if finished.recv_timeout(timeout) == Err(RecvTimeoutError::Timeout) {
                interrupt.interrupt();
            }
        })
    };
    let result =
        engine_script::lite_properties(path, source, &interrupt).map_err(|error| error.to_string());
    drop(done);
    let _ = watchdog.join();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::OpenProject;
    use crate::scene_edit::{EditorScene, no_prefabs};
    use engine_scene::CameraSettings;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TempProject(PathBuf);

    impl TempProject {
        fn new(problem_settings: Option<&str>) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "rve-editor-state-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(path.join("Assets/cameras")).unwrap();
            fs::write(
                path.join("project.rveproject"),
                "id = \"game\"\nname = \"Game\"\nversion = \"1\"\n\n[[scenes]]\npath = \"/main.rvescene\"\n",
            )
            .unwrap();
            fs::write(
                path.join("Assets/cameras/valid.camerasettings"),
                "[projection]\nfov_deg = 42.0\nnear = 0.25\nfar = 250.0\n",
            )
            .unwrap();
            if let Some(settings) = problem_settings {
                fs::write(path.join("Assets/cameras/problem.camerasettings"), settings).unwrap();
            }
            Self(path)
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn state_with_camera_assets(project_dir: &std::path::Path) -> EditorState {
        const SCENE: &str = r#"
[[assets]]
path = "/cameras/valid.camerasettings"
[[assets]]
path = "/cameras/problem.camerasettings"

[[objects]]
id = "camera-a"
[objects.camera]
settings = "/cameras/valid.camerasettings"
active = true

[[objects]]
id = "camera-b"
[objects.camera]
settings = "/cameras/problem.camerasettings"
"#;
        let mut state = EditorState::new(None);
        state.project = Some(OpenProject::open(project_dir).unwrap());
        state.scene = Some(
            EditorScene::parse(
                VirtualPath::parse("main.rvescene").unwrap(),
                SCENE,
                no_prefabs(),
            )
            .unwrap(),
        );
        state
    }

    fn state_with_scene() -> EditorState {
        let mut state = EditorState::new(None);
        state.scene = Some(EditorScene::new_untitled().unwrap());
        state
    }

    #[test]
    fn world_is_rebuilt_only_after_the_document_changes() {
        let mut state = state_with_scene();
        assert_eq!(state.world_mut().unwrap().objects().len(), 2);
        state.create_object(&NewObject::Empty, None);
        assert_eq!(state.world_mut().unwrap().objects().len(), 3);
    }

    #[test]
    fn invalid_camera_asset_keeps_default_edit_world_and_logs_once_per_revision() {
        for problem_settings in [None, Some("[projection]\nfov_deg = 190.0\n")] {
            let temp = TempProject::new(problem_settings);
            let mut state = state_with_camera_assets(&temp.0);

            for frame in 0..4 {
                let world = state
                    .world_mut()
                    .expect("camera asset errors keep the edit world");
                assert_eq!(world.objects().len(), 2);
                for object in world
                    .objects()
                    .iter()
                    .filter(|object| object.camera.is_some())
                {
                    assert_eq!(
                        world.camera_settings(object.handle),
                        Some(&CameraSettings::default())
                    );
                }
                assert!(world.camera_of(world.active_camera()).is_some());
                if frame < 3 {
                    state.log.info(format!("frame {frame}"));
                }
            }

            let errors = state
                .log
                .entries()
                .filter(|entry| entry.level == crate::console::LogLevel::Error)
                .count();
            assert_eq!(errors, 1);

            state.create_object(&NewObject::Empty, None);
            assert!(state.world_mut().is_some());
            state.log.info("rebuilt scene");
            assert!(state.world_mut().is_some());
            let errors = state
                .log
                .entries()
                .filter(|entry| entry.level == crate::console::LogLevel::Error)
                .count();
            assert_eq!(errors, 2);
        }
    }

    #[test]
    fn failed_edit_is_logged_and_leaves_the_scene_unchanged() {
        let mut state = state_with_scene();
        state.create_object(&NewObject::DirectionalLight, None);
        assert_eq!(state.log.entries().count(), 1);
        assert_eq!(state.scene.as_ref().unwrap().document().objects.len(), 2);
    }

    #[test]
    fn dropping_a_non_model_asset_is_rejected() {
        let mut state = state_with_scene();
        state.create_from_asset("textures/a.png", None, [0.0; 3]);
        assert_eq!(state.log.entries().count(), 1);
        assert_eq!(state.pending_frame, None);
        state.create_from_asset("models/a.fbx", None, [1.0, 0.0, 3.0]);
        let scene = state.scene.as_ref().unwrap();
        assert_eq!(scene.document().objects.len(), 3);
        let id = state.pending_frame.as_deref().unwrap();
        assert_eq!(
            scene.object(id).unwrap().transform.position,
            [1.0, 0.0, 3.0]
        );
    }

    #[test]
    fn assets_are_filtered_by_kind_through_nested_folders() {
        let mut state = EditorState::new(None);
        let file = |path: &str| AssetEntry {
            name: path.rsplit('/').next().unwrap().to_owned(),
            path: path.to_owned(),
            is_dir: false,
            children: Vec::new(),
        };
        state.asset_tree = vec![
            AssetEntry {
                name: "m".to_owned(),
                path: "m".to_owned(),
                is_dir: true,
                children: vec![file("m/a.fbx"), file("m/a.png")],
            },
            file("b.fbx"),
        ];
        assert_eq!(state.assets_of_kind(AssetKind::Model), ["m/a.fbx", "b.fbx"]);
        assert_eq!(state.assets_of_kind(AssetKind::Script), [] as [&str; 0]);
    }

    #[test]
    fn lite_schemas_are_read_from_the_project_once_and_errors_logged_once() {
        let mut state = EditorState::new(None);
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        state.project = Some(OpenProject::open(std::path::Path::new(dir)).unwrap());
        let schema = state.lite_schema("scripts/rotate.lua").unwrap().unwrap();
        assert!(schema.get("speed").is_some());
        assert!(state.lite_schemas.contains_key("scripts/rotate.lua"));

        assert!(state.lite_schema("scripts/missing.lua").is_err());
        assert!(state.lite_schema("scripts/missing.lua").is_err());
        assert_eq!(state.log.entries().count(), 1);
    }

    #[test]
    fn hanging_schema_chunk_is_interrupted() {
        let timeout = Duration::from_millis(20);
        assert!(read_lite_schema("loop.lua", "while true do end", timeout).is_err());
    }
}
