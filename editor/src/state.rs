//! Состояние редактора, общее для панелей и обработчиков действий; не зависит от GPU.

use crate::asset_watch::AssetChanges;
use crate::assets_fs::{AssetEntry, AssetKind};
use crate::build::{BuildJob, ScriptGeneration, ScriptsJob};
use crate::console::ConsoleLog;
use crate::external_editor::ExternalEditor;
use crate::icons::IconCache;
use crate::materials::MaterialLibrary;
use crate::play::PlaySession;
use crate::play_cursor::PlayCursor;
use crate::play_network::PlayNetworkForm;
use crate::previews::PreviewCache;
use crate::profile::EditorProfile;
use crate::project::OpenProject;
use crate::project_prefabs::ProjectPrefabs;
use crate::scene_edit::{Component, EditorScene, NewObject};
use crate::viewport::animation::{AnimationPreview, PreviewStatus};
use crate::viewport::camera::{EditorCamera, View};
use crate::viewport::colliders::ColliderCache;
use crate::viewport::gizmo::{GizmoDrag, GizmoSpace, GizmoTool, SnapSettings};
use crate::viewport::model_loader::ModelLoader;
use engine_animation::AnimationBridge;
use engine_assets::Model;
use engine_files::VirtualPath;
use engine_input::GilrsBackend;
use engine_render::DebugView;
use engine_scene::{AntiAliasing, CameraSettings, World};
use engine_script::{ParamSchema, ScriptInterruptHandle, ScriptLibrary};
use std::collections::{HashMap, HashSet};
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
    NewProject,
    CreateProject {
        name: String,
        parent: PathBuf,
    },
    Save,
    SaveAs,
    /// Сохранить в явно заданный путь без системного диалога.
    SaveAsAt(String),
    Exit,
    Play,
    Stop,
    ImportDialog,
    ImportFiles(Vec<PathBuf>),
    /// Отразить собственную файловую операцию в дереве Project.
    RefreshTree,
    /// Выбрать каталог и собрать релиз проекта через `rve-build`; сцена сохраняется заранее.
    BuildPlayer,
    /// Собрать релиз в явно заданный каталог без системного диалога.
    BuildPlayerAt(PathBuf),
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

/// Перетаскиваемый объект из Hierarchy.
#[derive(Clone, Debug)]
pub struct ObjectDrag(pub String);

/// Команда numpad для камеры Scene. Копится между кадрами и применяется в начале кадра.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ViewCommand {
    /// Навести камеру на выделенный объект.
    Frame,
    Look(View),
    /// Шаг вращения глаза вокруг цели, в радианах.
    Rotate {
        yaw: f32,
        pitch: f32,
    },
    Opposite,
    /// Камера редактора принимает положение и направление камеры вкладки Game.
    AlignToGame,
}

impl ViewCommand {
    /// Повтор клавиши (удержание) допустим только для шагов вращения.
    pub fn repeats(self) -> bool {
        matches!(self, Self::Rotate { .. })
    }
}

/// Сведения о панели Scene за текущий кадр.
#[derive(Clone, Copy, Debug, Default)]
pub struct SceneViewInfo {
    /// Панель показана и её изображение нужно нарисовать в этом кадре.
    pub visible: bool,
    /// Курсор над панелью: ввод Play-режима относится к сцене.
    pub hovered: bool,
    /// Размер изображения в физических пикселях.
    pub size: [u32; 2],
    /// Прямоугольник изображения Game в координатах egui за текущий кадр.
    pub rect: Option<egui::Rect>,
    /// Render diagnostic selected for this panel.
    pub debug_view: DebugView,
    /// Рисовать рёбра всех коллайдеров; выделенный объект рисуется всегда.
    pub show_colliders: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum MaterialPreviewPrimitive {
    Plane,
    #[default]
    Cube,
    Sphere,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MaterialPreviewViewInfo {
    pub visible: bool,
    pub size: [u32; 2],
}

/// Раздельные заимствования состояния для подготовки кадра сцены.
pub struct FrameParts<'a> {
    pub world: &'a mut World,
    pub project: &'a OpenProject,
    pub materials: &'a MaterialLibrary,
    pub models: &'a mut HashMap<String, Arc<Model>>,
    pub loader: &'a mut ModelLoader,
    pub log: &'a mut ConsoleLog,
    /// Мост аниматоров Play; `None` в режиме редактирования.
    pub animation: Option<&'a AnimationBridge>,
    pub animation_preview: &'a mut AnimationPreview,
    /// Сведения превью для инспектора, которые пересчитывает кадр.
    pub animation_status: &'a mut PreviewStatus,
    /// Id выбранного объекта: превью работает только с ним.
    pub selected: Option<&'a str>,
}

pub struct EditorState {
    pub ui_document: Option<crate::ui_editor::CanvasEditor>,
    pub ui_pending_path: Option<String>,
    pub ui_editor_requested: bool,
    pub ui_editor_active: bool,
    pub ui_authoring: bool,
    pub ui_editor_rect: Option<egui::Rect>,
    /// Invalidates queued MCP calls whenever a project or scene is replaced.
    pub mcp_generation: u64,
    /// Незавершённый `capture_view`: ответ отправляется, когда кадр отдал PNG или задача снята.
    pub(crate) mcp_capture: Option<crate::mcp::capture::CaptureJob>,
    pub project: Option<OpenProject>,
    /// Открытый в подокне «Project Prefabs» список тяжёлых префабов; хранится с несохранёнными правками.
    pub project_prefabs: Option<ProjectPrefabs>,
    /// Несохранённые теги и слои проекта (вкладка Project Settings).
    pub tags_layers: Option<crate::project_tags::TagsLayersDraft>,
    pub scene: Option<EditorScene>,
    pub log: ConsoleLog,
    pub profile: EditorProfile,
    pub tool: GizmoTool,
    pub space: GizmoSpace,
    pub snap: SnapSettings,
    pub camera: EditorCamera,
    /// Команды numpad для камеры Scene, ожидающие применения в начале кадра.
    pub view_commands: Vec<ViewCommand>,
    pub scroll_navigation: crate::gestures::ScrollNavigation,
    pub last_wheel_trackpad: bool,
    pub smart_zoom: bool,
    pub gizmo_drag: Option<GizmoDrag>,
    pub play: Option<PlaySession>,
    /// The editor's gamepad backend, created by the first Play and kept across Play sessions.
    pub gamepads: Option<GilrsBackend>,
    pub scripts_path: Option<PathBuf>,
    /// Недавние проекты из настроек пользователя, последний открытый — первым.
    pub recent_projects: Vec<PathBuf>,
    pub library: Option<ScriptLibrary>,
    /// Библиотека, созданная автоматической сборкой; её копия загружается отдельно от Cargo target.
    pub script_generation: Option<ScriptGeneration>,
    /// Фоновая сборка heavy_scripts при открытии проекта или перед Play.
    pub scripts_job: Option<ScriptsJob>,
    /// Play, ожидающий успешного завершения автоматической сборки скриптов.
    pub pending_play: bool,
    /// Сетевой режим следующего Play и его параметры; пароль живёт только здесь, в памяти.
    pub play_network: PlayNetworkForm,
    pub actions: Vec<Action>,
    pub unsaved_prompt: Option<UnsavedPrompt>,
    pub recovery_prompt: Option<RecoveryPrompt>,
    pub asset_tree: Vec<AssetEntry>,
    pub current_folder: String,
    /// Ресурс, на который перешёл Inspector.
    pub project_selection: Option<String>,
    pub project_dialog: Option<crate::ui::project_panel::ProjectDialog>,
    /// `.rvephysmat`, открытый панелью полей Project.
    pub physics_material_edit: Option<crate::ui::project_panel::PhysicsMaterialEdit>,
    /// `.rvemat`/`.rveshader` selected in Project for the asset inspector.
    pub inspected_asset: Option<String>,
    pub materials: MaterialLibrary,
    pub external_editor: ExternalEditor,
    pub material_preview: MaterialPreviewViewInfo,
    pub material_preview_primitive: MaterialPreviewPrimitive,
    pub material_preview_yaw: f32,
    pub material_preview_pitch: f32,
    pub material_preview_zoom: f32,
    pub material_preview_asset: Option<String>,
    pub preview_requested: bool,
    pub material_preview_texture: Option<egui::TextureId>,
    pub new_project_dialog: Option<crate::new_project::NewProjectDialog>,
    pub hierarchy_rename: Option<(String, String)>,
    /// Текстовому полю переименования ещё нужно передать фокус; снимается после первого запроса.
    pub hierarchy_rename_focus: bool,
    /// В прошлом кадре фокус был в текстовом поле: Esc этого кадра закрывает поле,
    /// а не снимает выделение.
    pub text_focused_last_frame: bool,
    /// Developer console overlays Game and captures editor shortcuts while open.
    pub console_open: bool,
    /// Строки поиска панелей Hierarchy и Project.
    pub hierarchy_filter: String,
    pub project_filter: String,
    /// Компонент, скопированный в Inspector для вставки в объекты.
    pub component_clipboard: Option<Component>,
    pub previews: PreviewCache,
    /// Иконки папок для плиток панели Project.
    pub icons: IconCache,
    pub viewport_texture: Option<egui::TextureId>,
    pub scene_view: SceneViewInfo,
    pub game_viewport_texture: Option<egui::TextureId>,
    pub game_view: SceneViewInfo,
    /// Захват курсора панелью Game во время Play.
    pub play_cursor: PlayCursor,
    /// Optional renderer features used to filter the debug-view controls.
    pub wireframe_supported: bool,
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
    /// Хвост вывода последней/текущей сборки для MCP build_status.
    pub build_output_tail: Vec<String>,
    /// Итог последней сборки: None до завершения, Some(true) при успехе.
    pub build_succeeded: Option<bool>,
    /// Схемы `properties` lite-скриптов по пути; ошибка пишется в Console один раз.
    /// Кэшированные схемы читаются повторно после изменения исходного файла.
    pub lite_schemas: HashMap<String, Result<Option<ParamSchema>, String>>,
    /// Фоновый разбор FBX для `models`.
    pub model_loader: ModelLoader,
    /// Последняя ошибка компиляции экранного шейдера по пути `.glsl`; показывается в инспекторе.
    pub screen_shader_errors: HashMap<String, String>,
    /// Выпуклые оболочки и trimesh для рёбер коллайдеров и ошибок инспектора.
    pub collider_cache: ColliderCache,
    /// Настройки превью аниматора выбранного объекта; меняет инспектор.
    pub animation_preview: AnimationPreview,
    /// Сведения превью, которые кадр передаёт инспектору: движения и время.
    pub animation_status: PreviewStatus,
    edit_world: Option<(u64, World)>,
    dirty_lite_schemas: HashSet<String>,
    camera_settings_error_revision: Option<u64>,
}

impl EditorState {
    pub fn new(scripts_path: Option<PathBuf>) -> Self {
        Self {
            ui_document: None,
            ui_pending_path: None,
            ui_editor_requested: false,
            ui_editor_active: false,
            ui_authoring: false,
            ui_editor_rect: None,
            mcp_generation: 0,
            mcp_capture: None,
            project: None,
            project_prefabs: None,
            tags_layers: None,
            scene: None,
            log: ConsoleLog::default(),
            profile: EditorProfile::default(),
            tool: GizmoTool::Move,
            space: GizmoSpace::Global,
            snap: SnapSettings::default(),
            camera: EditorCamera::default(),
            view_commands: Vec::new(),
            scroll_navigation: Default::default(),
            last_wheel_trackpad: false,
            smart_zoom: false,
            gizmo_drag: None,
            play: None,
            gamepads: None,
            scripts_path,
            recent_projects: Vec::new(),
            library: None,
            script_generation: None,
            scripts_job: None,
            pending_play: false,
            play_network: PlayNetworkForm::default(),
            actions: Vec::new(),
            unsaved_prompt: None,
            recovery_prompt: None,
            asset_tree: Vec::new(),
            current_folder: String::new(),
            project_selection: None,
            project_dialog: None,
            physics_material_edit: None,
            inspected_asset: None,
            materials: MaterialLibrary::default(),
            external_editor: ExternalEditor::default(),
            material_preview: MaterialPreviewViewInfo::default(),
            material_preview_primitive: MaterialPreviewPrimitive::default(),
            material_preview_yaw: 0.5,
            material_preview_pitch: 0.25,
            material_preview_zoom: 4.0,
            material_preview_asset: None,
            preview_requested: false,
            material_preview_texture: None,
            new_project_dialog: None,
            hierarchy_rename: None,
            hierarchy_rename_focus: false,
            text_focused_last_frame: false,
            console_open: false,
            hierarchy_filter: String::new(),
            project_filter: String::new(),
            component_clipboard: None,
            previews: PreviewCache::default(),
            icons: IconCache::default(),
            viewport_texture: None,
            scene_view: SceneViewInfo::default(),
            game_viewport_texture: None,
            game_view: SceneViewInfo::default(),
            play_cursor: PlayCursor::default(),
            wireframe_supported: false,
            game_camera: None,
            pending_frame: None,
            models: HashMap::new(),
            build: None,
            build_output_tail: Vec::new(),
            build_succeeded: None,
            apply_prompt: None,
            lite_schemas: HashMap::new(),
            model_loader: ModelLoader::default(),
            screen_shader_errors: HashMap::new(),
            collider_cache: ColliderCache::default(),
            animation_preview: AnimationPreview::default(),
            animation_status: PreviewStatus::default(),
            edit_world: None,
            dirty_lite_schemas: HashSet::new(),
            camera_settings_error_revision: None,
        }
    }

    pub fn is_playing(&self) -> bool {
        self.play.is_some()
    }

    pub fn advance_mcp_generation(&mut self) {
        self.mcp_generation = self.mcp_generation.wrapping_add(1);
    }

    /// Начинает переименование в Hierarchy; поле получит фокус один раз при отрисовке.
    pub fn begin_hierarchy_rename(&mut self, id: String, text: String) {
        self.hierarchy_rename = Some((id, text));
        self.hierarchy_rename_focus = true;
    }

    /// Переход в Play-режим: активное переименование в Hierarchy отменяется.
    pub fn enter_play(&mut self, session: PlaySession) {
        self.play = Some(session);
        self.gizmo_drag = None;
        self.hierarchy_rename = None;
        self.hierarchy_rename_focus = false;
    }

    /// Конец Play-сессии для геймпадов: вибрация останавливается, пады читаются как отпущенные.
    /// Сам backend остаётся открытым для следующей сессии.
    pub fn end_play_gamepads(&mut self) {
        if let Some(gamepads) = self.gamepads.as_mut() {
            gamepads.end_session();
        }
    }

    /// Курсор над изображением сцены: ввод Play-режима относится к игре.
    pub fn pointer_over_view(&self) -> bool {
        self.scene_view.hovered || self.game_view.hovered
    }

    pub fn action(&mut self, action: Action) {
        self.actions.push(action);
    }

    /// Returns false and queues Play while project scripts must be compiled first.
    pub fn request_play(&mut self) -> bool {
        if self.scripts_path.is_none()
            && self
                .project
                .as_ref()
                .is_some_and(|project| project.settings.heavy_scripts.is_some())
        {
            self.pending_play = true;
            false
        } else {
            true
        }
    }

    pub fn finish_pending_play(&mut self, build_succeeded: bool) -> bool {
        std::mem::take(&mut self.pending_play) && build_succeeded
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
                        && let Some(error) = project
                            .apply_camera_settings_preview(&mut world)
                            .into_iter()
                            .next()
                        && self.camera_settings_error_revision != Some(revision)
                    {
                        self.log.error(error);
                        self.camera_settings_error_revision = Some(revision);
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
        let (world, animation) = match &mut self.play {
            Some(play) => {
                let (world, animation) = play.world_and_animation();
                (world, Some(animation))
            }
            None => (&mut self.edit_world.as_mut()?.1, None),
        };
        let selected = self
            .scene
            .as_ref()
            .and_then(|scene| scene.selected_object())
            .map(|object| object.id.as_str());
        Some(FrameParts {
            world,
            project: self.project.as_ref()?,
            materials: &self.materials,
            models: &mut self.models,
            loader: &mut self.model_loader,
            log: &mut self.log,
            animation,
            animation_preview: &mut self.animation_preview,
            animation_status: &mut self.animation_status,
            selected,
        })
    }

    /// Сохраняет правку `.postprocess` и обновляет все камеры, которые используют этот файл.
    pub fn save_post_process(
        &mut self,
        path: &engine_files::VirtualPath,
        settings: engine_scene::PostProcessSettings,
    ) -> bool {
        if self.is_playing() {
            return false;
        }
        let Some(project) = self.project.as_ref() else {
            self.log.error("проект не открыт");
            return false;
        };
        if let Err(error) = project.save_post_process(path, &settings) {
            self.log.error(error);
            return false;
        }
        let handles = self
            .world_mut()
            .map(|world| {
                world
                    .objects()
                    .iter()
                    .filter(|object| {
                        object
                            .camera
                            .as_ref()
                            .and_then(|camera| camera.post_process.as_ref())
                            == Some(path)
                    })
                    .map(|object| object.handle)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(world) = self.world_mut() {
            for handle in handles {
                let _ = world.set_post_process(handle, settings);
            }
        }
        self.log.info(format!("Сохранена постобработка {path}"));
        true
    }

    pub fn camera_settings_for(&mut self, id: &str) -> Option<CameraSettings> {
        let world = self.world_mut()?;
        let handle = world.handle(id)?;
        world.camera_settings(handle).copied()
    }

    /// Persists an AA change to the camera's assigned settings asset and refreshes all
    /// cameras that share that asset in the current editor world.
    pub fn set_camera_anti_aliasing(&mut self, id: &str, anti_aliasing: AntiAliasing) {
        if self.is_playing() {
            return;
        }
        let path = self
            .scene
            .as_ref()
            .and_then(|scene| scene.object(id))
            .and_then(|object| object.camera.as_ref())
            .and_then(|camera| camera.settings.clone());
        let Some(path) = path else {
            self.log.error("Назначьте камере ресурс .camerasettings");
            return;
        };
        let Some(mut settings) = self.camera_settings_for(id) else {
            self.log.error(format!("камера {id:?} не найдена в мире"));
            return;
        };
        settings.render.anti_aliasing = anti_aliasing;
        let Some(project) = self.project.as_ref() else {
            self.log.error("проект не открыт");
            return;
        };
        if let Err(error) = project.save_camera_settings(&path, &settings) {
            self.log.error(error);
            return;
        }
        let handles = self
            .world_mut()
            .map(|world| {
                world
                    .objects()
                    .iter()
                    .filter(|object| {
                        object
                            .camera
                            .as_ref()
                            .and_then(|camera| camera.settings.as_ref())
                            == Some(&path)
                    })
                    .map(|object| object.handle)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if let Some(world) = self.world_mut() {
            for handle in handles {
                let _ = world.set_camera_settings(handle, settings);
            }
        }
        self.log.info(format!("Сохранены настройки камеры {path}"));
    }

    /// Сбрасывает данные, связанные с прежней сценой.
    pub fn scene_replaced(&mut self) {
        self.edit_world = None;
        self.camera_settings_error_revision = None;
        self.gizmo_drag = None;
        self.hierarchy_rename = None;
        self.game_camera = None;
        self.pending_frame = None;
        self.inspected_asset = None;
    }

    /// Список тяжёлых префабов относится к проекту, который открыт сейчас: при открытии другого
    /// проекта несохранённые правки прошлого списка не переносятся.
    pub fn project_replaced(&mut self) {
        self.project_prefabs = None;
        self.tags_layers = None;
        self.ui_document = None;
        self.ui_pending_path = None;
        self.materials = MaterialLibrary::default();
        self.inspected_asset = None;
        self.material_preview_asset = None;
        self.preview_requested = false;
    }

    /// Есть ли несохранённые правки сцены или списка тяжёлых префабов.
    pub fn has_unsaved_changes(&self) -> bool {
        self.ui_document
            .as_ref()
            .is_some_and(crate::ui_editor::CanvasEditor::is_dirty)
            || self.scene.as_ref().is_some_and(EditorScene::is_dirty)
            || self
                .project_prefabs
                .as_ref()
                .is_some_and(ProjectPrefabs::is_dirty)
    }

    /// Сохраняет список тяжёлых префабов, если в нём есть правки; без правок файл не трогается.
    pub fn save_project_prefabs(&mut self) -> Result<(), String> {
        if let (Some(editor), Some(project)) = (&mut self.ui_document, &self.project)
            && editor.is_dirty()
        {
            editor.save(project)?;
        }
        match self.project_prefabs.as_mut() {
            Some(prefabs) if prefabs.is_dirty() => prefabs.save(),
            _ => Ok(()),
        }
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

    /// Схема `properties` lite-скрипта; изменённый исходник читается при следующем обращении.
    pub fn lite_schema(&mut self, path: &str) -> Result<Option<ParamSchema>, String> {
        let dirty = self.dirty_lite_schemas.remove(path);
        if !dirty && let Some(schema) = self.lite_schemas.get(path) {
            return schema.clone();
        }
        let Some(project) = &self.project else {
            return Ok(None);
        };
        let previous = self
            .lite_schemas
            .get(path)
            .and_then(|schema| schema.as_ref().ok())
            .cloned();
        let result = project
            .read_text(path)
            .and_then(|source| read_lite_schema(path, &source, SCHEMA_TIMEOUT));
        if let Err(error) = &result {
            self.log.error(error.clone());
            if let Some(previous) = previous {
                return Ok(previous);
            }
        }
        self.lite_schemas.insert(path.to_owned(), result.clone());
        result
    }

    /// Помечает закэшированные Lua-схемы, затронутые изменёнными файлами.
    pub fn refresh_lite_schemas(&mut self, changes: &AssetChanges) {
        if changes.rescan {
            self.dirty_lite_schemas
                .extend(self.lite_schemas.keys().cloned());
            return;
        }
        let changed = changes
            .lua_paths()
            .chain(changes.directory_paths.iter())
            .cloned()
            .collect::<Vec<_>>();
        self.dirty_lite_schemas.extend(
            self.lite_schemas
                .keys()
                .filter(|key| {
                    let Ok(key) = VirtualPath::parse(key) else {
                        return false;
                    };
                    changed.iter().any(|path| {
                        key == *path
                            || key
                                .as_str()
                                .strip_prefix(path.as_str())
                                .is_some_and(|suffix| suffix.starts_with('/'))
                    })
                })
                .cloned(),
        );
    }

    /// Сбрасывает кэш Lua-схем при смене проекта.
    pub fn clear_lite_schemas(&mut self) {
        self.lite_schemas.clear();
        self.dirty_lite_schemas.clear();
    }

    /// Сбрасывает edit-world после изменения `.camerasettings`, вне Play.
    pub fn camera_settings_changed(&mut self) {
        if !self.is_playing() {
            self.edit_world = None;
        }
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
        ..engine_scene::SceneObject::default()
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

    fn camera_fov(state: &mut EditorState) -> f32 {
        let world = state.world_mut().unwrap();
        let handle = world.handle("camera-a").unwrap();
        world.camera_settings(handle).unwrap().projection.fov_deg
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
                    if object.id == "camera-a" {
                        assert_eq!(
                            world
                                .camera_settings(object.handle)
                                .unwrap()
                                .projection
                                .fov_deg,
                            42.0
                        );
                    } else {
                        assert_eq!(
                            world.camera_settings(object.handle),
                            Some(&CameraSettings::default())
                        );
                    }
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
    fn camera_anti_aliasing_edits_update_the_asset_and_editor_world() {
        let temp = TempProject::new(None);
        let mut state = state_with_camera_assets(&temp.0);
        state.edit_scene(|scene| {
            scene.set_camera_settings(
                "camera-b",
                Some(VirtualPath::parse("/cameras/valid.camerasettings").unwrap()),
            )
        });
        state.set_camera_anti_aliasing("camera-a", AntiAliasing::Taa);

        let settings = CameraSettings::parse(
            "saved camera settings",
            &fs::read_to_string(temp.0.join("Assets/cameras/valid.camerasettings")).unwrap(),
        )
        .unwrap();
        assert_eq!(settings.render.anti_aliasing, AntiAliasing::Taa);
        assert_eq!(
            state
                .camera_settings_for("camera-a")
                .unwrap()
                .render
                .anti_aliasing,
            AntiAliasing::Taa
        );
        assert_eq!(
            state
                .camera_settings_for("camera-b")
                .unwrap()
                .render
                .anti_aliasing,
            AntiAliasing::Taa
        );
    }

    #[test]
    fn camera_preview_keeps_last_valid_settings_but_play_stays_strict() {
        let temp = TempProject::new(None);
        let mut state = EditorState::new(None);
        state.project = Some(OpenProject::open(&temp.0).unwrap());
        state.scene = Some(
            EditorScene::parse(
                VirtualPath::parse("main.rvescene").unwrap(),
                "[[assets]]\npath = \"/cameras/valid.camerasettings\"\n[[objects]]\nid = \"camera-a\"\n[objects.camera]\nsettings = \"/cameras/valid.camerasettings\"\n",
                no_prefabs(),
            )
            .unwrap(),
        );
        let valid_path = temp.0.join("Assets/cameras/valid.camerasettings");
        assert_eq!(camera_fov(&mut state), 42.0);

        fs::write(
            &valid_path,
            "[projection]\nfov_deg = 60.0\nnear = 0.3\nfar = 300.0\n",
        )
        .unwrap();
        state.camera_settings_changed();
        assert_eq!(camera_fov(&mut state), 60.0);

        fs::write(&valid_path, "[projection]\nfov_deg = 190.0\n").unwrap();
        state.camera_settings_changed();
        assert_eq!(camera_fov(&mut state), 60.0);
        state.create_object(&NewObject::Empty, None);
        assert_eq!(camera_fov(&mut state), 60.0);
        let revision = state.scene.as_ref().unwrap().revision();
        assert_eq!(state.camera_settings_error_revision, Some(revision));
        assert_eq!(
            state
                .log
                .entries()
                .filter(|entry| entry.level == crate::console::LogLevel::Error)
                .count(),
            1
        );

        let project = state.project.as_ref().unwrap();
        let mut play_world = World::from_document(state.scene.as_ref().unwrap().view()).unwrap();
        assert!(project.apply_camera_settings(&mut play_world).is_err());
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
    fn changed_lua_schema_keeps_last_valid_on_error_and_reloads_after_fix() {
        let temp = TempProject::new(None);
        fs::create_dir_all(temp.0.join("Assets/scripts")).unwrap();
        let first = temp.0.join("Assets/scripts/first.lua");
        let second = temp.0.join("Assets/scripts/second.lua");
        let source = |name: &str| {
            format!(
                "local Script = {{}}\nScript.properties = {{ {name} = {{ type = 'float', default = 1 }} }}\nreturn Script\n"
            )
        };
        fs::write(&first, source("speed")).unwrap();
        fs::write(&second, source("height")).unwrap();
        let mut state = EditorState::new(None);
        state.project = Some(OpenProject::open(&temp.0).unwrap());
        let first_schema = state.lite_schema("scripts/first.lua").unwrap().unwrap();
        assert!(first_schema.get("speed").is_some());
        assert!(
            state
                .lite_schema("scripts/second.lua")
                .unwrap()
                .unwrap()
                .get("height")
                .is_some()
        );

        fs::write(&first, "this is not valid Lua").unwrap();
        let changes = AssetChanges {
            paths: HashSet::from([VirtualPath::parse("scripts/first.lua").unwrap()]),
            ..AssetChanges::default()
        };
        state.refresh_lite_schemas(&changes);
        assert!(state.dirty_lite_schemas.contains("scripts/first.lua"));
        assert!(!state.dirty_lite_schemas.contains("scripts/second.lua"));
        assert!(
            state
                .lite_schema("scripts/first.lua")
                .unwrap()
                .unwrap()
                .get("speed")
                .is_some()
        );
        assert_eq!(state.log.entries().count(), 1);

        fs::write(&first, source("height")).unwrap();
        state.refresh_lite_schemas(&changes);
        let fixed = state.lite_schema("scripts/first.lua").unwrap().unwrap();
        assert!(fixed.get("height").is_some());
        assert!(fixed.get("speed").is_none());
        assert!(
            state
                .lite_schema("scripts/second.lua")
                .unwrap()
                .unwrap()
                .get("height")
                .is_some()
        );
    }

    #[test]
    fn hanging_schema_chunk_is_interrupted() {
        let timeout = Duration::from_millis(20);
        assert!(read_lite_schema("loop.lua", "while true do end", timeout).is_err());
    }

    #[test]
    fn play_waits_for_auto_scripts_build_and_is_cancelled_on_failure() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
        let mut state = EditorState::new(None);
        state.project = Some(OpenProject::open(std::path::Path::new(dir)).unwrap());

        assert!(!state.request_play());
        assert!(state.pending_play);
        assert!(!state.finish_pending_play(false));
        assert!(!state.pending_play);

        assert!(!state.request_play());
        assert!(state.finish_pending_play(true));

        state.scripts_path = Some(PathBuf::from("manual.dll"));
        assert!(state.request_play());
    }

    #[test]
    fn unsaved_project_prefab_list_counts_until_it_is_saved() {
        let temp = TempProject::new(None);
        let mut state = EditorState::new(None);
        state.project = Some(OpenProject::open(&temp.0).unwrap());
        state.project_prefabs = Some(ProjectPrefabs::open(&temp.0));
        assert!(!state.has_unsaved_changes());

        state
            .project_prefabs
            .as_mut()
            .unwrap()
            .add("prefabs/crate.rveprefab")
            .unwrap();
        assert!(state.has_unsaved_changes());
        state.save_project_prefabs().unwrap();
        assert!(!state.has_unsaved_changes());
        assert_eq!(ProjectPrefabs::open(&temp.0).entries().len(), 1);
    }

    #[test]
    fn opening_another_project_drops_unsaved_prefab_list_edits() {
        let temp = TempProject::new(None);
        let mut state = EditorState::new(None);
        state.project_prefabs = Some(ProjectPrefabs::open(&temp.0));
        state
            .project_prefabs
            .as_mut()
            .unwrap()
            .add("prefabs/crate.rveprefab")
            .unwrap();
        assert!(state.has_unsaved_changes());

        state.project_replaced();
        assert!(state.project_prefabs.is_none());
        assert!(!state.has_unsaved_changes());
        assert!(ProjectPrefabs::open(&temp.0).entries().is_empty());
    }
}
