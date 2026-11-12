//! Панель Project: файловый менеджер каталога `Assets/`.

use crate::asset_refs::{self, PathRemap, Reference, ReferenceSource};
use crate::assets_fs::{AssetEntry, AssetKind};
use crate::icons::{file_icon, folder_icon};
use crate::state::{Action, AssetDrag, EditorState};
use engine_files::VirtualPath;
use engine_scene::{CombineMode, PhysicsMaterialSettings};

const TILE_SIZE: f32 = 88.0;

/// Открытый диалог файловой операции.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProjectDialog {
    NewFolder {
        name: String,
    },
    NewUi {
        name: String,
    },
    Rename {
        path: String,
        name: String,
        references: AssetReferences,
        update_references: bool,
    },
    Delete {
        path: String,
        is_dir: bool,
        references: AssetReferences,
    },
    NewPhysicsMaterial {
        name: String,
    },
    NewMaterial {
        folder: String,
        name: String,
    },
    NewShader {
        folder: String,
        name: String,
    },
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        if ui.button("New UI Canvas").clicked() {
            state.project_dialog = Some(ProjectDialog::NewUi {
                name: "Canvas".into(),
            });
        }
        if ui.button("New Folder").clicked() {
            state.project_dialog = Some(ProjectDialog::NewFolder {
                name: String::new(),
            });
        }
        if ui.button("New Physics Material").clicked() {
            state.project_dialog = Some(ProjectDialog::NewPhysicsMaterial {
                name: "New Physics Material".to_owned(),
            });
        }
        create_menu(ui, state, state.current_folder.clone());
        if ui.button("Import…").clicked() {
            state.action(Action::ImportDialog);
        }
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut state.project_filter)
                .hint_text("Search")
                .desired_width(160.0),
        );
        ui.separator();
        let folder = if state.current_folder.is_empty() {
            "Assets".to_owned()
        } else {
            format!("Assets/{}", state.current_folder)
        };
        ui.label(folder);
    });
    ui.separator();
    egui::Panel::left("project_tree")
        .default_size(180.0)
        .show(ui, |ui| {
            egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                let root_selected = state.current_folder.is_empty();
                let root_response = ui
                    .horizontal(|ui| {
                        folder_icon_image(ui, state, "Assets", !state.asset_tree.is_empty());
                        if ui.selectable_label(root_selected, "Assets").clicked() {
                            state.current_folder.clear();
                        }
                    })
                    .response;
                root_response.context_menu(|ui| create_menu(ui, state, "".to_owned()));
                let tree = std::mem::take(&mut state.asset_tree);
                folder_tree(ui, state, &tree);
                state.asset_tree = tree;
            });
        });
    if state.physics_material_edit.is_some() {
        egui::Panel::bottom("physics_material").show(ui, |ui| {
            show_physics_material_fields(ui, state);
        });
    }
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                let tree = std::mem::take(&mut state.asset_tree);
                let query = state.project_filter.trim().to_owned();
                // Поиск идёт по всему Assets, а не только по открытой папке.
                let entries = if query.is_empty() {
                    find_folder(&tree, &state.current_folder)
                        .unwrap_or(&tree)
                        .iter()
                        .collect()
                } else {
                    search_entries(&tree, &query)
                };
                ui.horizontal_wrapped(|ui| {
                    for entry in entries {
                        tile(ui, state, entry);
                    }
                });
                let available = egui::Rect::from_min_size(ui.cursor().min, ui.available_size());
                let empty_response = ui.interact(
                    available,
                    ui.make_persistent_id("project_empty_context"),
                    egui::Sense::click(),
                );
                empty_response.context_menu(|ui| {
                    create_menu(ui, state, state.current_folder.clone());
                });
                state.asset_tree = tree;
            });
    });
}

/// Иконка папки перед подписью в дереве; без загруженной текстуры подпись выводится одна.
fn folder_icon_image(ui: &mut egui::Ui, state: &EditorState, name: &str, has_children: bool) {
    let Some(texture) = state.icons.get(folder_icon(name, has_children)) else {
        return;
    };
    let size = ui.spacing().interact_size.y;
    ui.add(egui::Image::new(egui::load::SizedTexture::new(
        texture,
        egui::vec2(size, size),
    )));
}

fn folder_tree(ui: &mut egui::Ui, state: &mut EditorState, entries: &[AssetEntry]) {
    for entry in entries.iter().filter(|entry| entry.is_dir) {
        let selected = state.current_folder == entry.path;
        let has_folders = entry.children.iter().any(|child| child.is_dir);
        let has_children = !entry.children.is_empty();
        if has_folders {
            let id = ui.make_persistent_id(("project_folder", &entry.path));
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
                .show_header(ui, |ui| {
                    folder_icon_image(ui, state, &entry.name, has_children);
                    let response = ui.selectable_label(selected, &entry.name);
                    if response.clicked() {
                        state.current_folder = entry.path.clone();
                    }
                    response.context_menu(|ui| {
                        create_menu(ui, state, create_target(Some(entry), ""));
                    });
                })
                .body(|ui| folder_tree(ui, state, &entry.children));
        } else {
            let response = ui
                .horizontal(|ui| {
                    ui.add_space(ui.spacing().indent);
                    folder_icon_image(ui, state, &entry.name, has_children);
                    let response = ui.selectable_label(selected, &entry.name);
                    if response.clicked() {
                        state.current_folder = entry.path.clone();
                    }
                    response
                })
                .inner;
            response.context_menu(|ui| {
                create_menu(ui, state, create_target(Some(entry), ""));
            });
        }
    }
}

/// Содержимое папки по пути; пустой путь — корень.
pub fn find_folder<'a>(tree: &'a [AssetEntry], path: &str) -> Option<&'a [AssetEntry]> {
    if path.is_empty() {
        return Some(tree);
    }
    let mut entries = tree;
    for component in path.split('/') {
        entries = &entries
            .iter()
            .find(|entry| entry.is_dir && entry.name == component)?
            .children;
    }
    Some(entries)
}

/// Файлы и папки всего дерева, имя которых содержит `query` без учёта регистра.
fn search_entries<'a>(tree: &'a [AssetEntry], query: &str) -> Vec<&'a AssetEntry> {
    fn collect<'a>(entries: &'a [AssetEntry], query: &str, found: &mut Vec<&'a AssetEntry>) {
        for entry in entries {
            if entry.name.to_lowercase().contains(query) {
                found.push(entry);
            }
            collect(&entry.children, query, found);
        }
    }
    let mut found = Vec::new();
    collect(tree, &query.to_lowercase(), &mut found);
    found
}

fn kind_label(entry: &AssetEntry) -> &'static str {
    if entry.is_dir {
        return "DIR";
    }
    match AssetKind::of(&entry.path) {
        AssetKind::Scene => "SCENE",
        AssetKind::Ui => "UI",
        AssetKind::Prefab => "PREFAB",
        AssetKind::CameraSettings => "CAMERA",
        AssetKind::PostProcess => "POST",
        AssetKind::ScreenShader => "GLSL",
        AssetKind::PhysicsMaterial => "PHYSMAT",
        AssetKind::Material => "MATERIAL",
        AssetKind::Shader => "SHADER",
        AssetKind::Model => "FBX",
        AssetKind::Texture => "PNG",
        AssetKind::Hdr => "HDR",
        AssetKind::Audio => "AUDIO",
        AssetKind::Script => "LUA",
        AssetKind::Other => "FILE",
    }
}

fn tile(ui: &mut egui::Ui, state: &mut EditorState, entry: &AssetEntry) {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(TILE_SIZE, TILE_SIZE),
        egui::Sense::click_and_drag(),
    );
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact(&response);
        let painter = ui.painter();
        if response.hovered() {
            painter.rect_filled(rect, 4.0, visuals.bg_fill);
        }
        if state.project_selection.as_deref() == Some(entry.path.as_str()) {
            painter.rect_stroke(
                rect,
                4.0,
                ui.visuals().selection.stroke,
                egui::StrokeKind::Inside,
            );
        }
        let image_rect = egui::Rect::from_min_size(
            rect.min + egui::vec2(16.0, 4.0),
            egui::vec2(TILE_SIZE - 32.0, TILE_SIZE - 32.0),
        );
        let preview = (!entry.is_dir
            && matches!(
                AssetKind::of(&entry.path),
                AssetKind::Texture | AssetKind::Hdr
            ))
        .then(|| {
            let project = state.project.as_ref();
            state.previews.get(&entry.path, || {
                project.and_then(|project| project.assets_dir.resolve(&entry.path).ok())
            })
        })
        .flatten();
        let icon = if entry.is_dir {
            Some(folder_icon(&entry.name, !entry.children.is_empty()))
        } else {
            file_icon(AssetKind::of(&entry.path))
        }
        .and_then(|name| state.icons.get(name));
        match preview.or(icon) {
            Some(texture) => {
                painter.image(
                    texture,
                    image_rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                painter.rect_filled(image_rect, 3.0, ui.visuals().extreme_bg_color);
                painter.text(
                    image_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    kind_label(entry),
                    egui::FontId::proportional(12.0),
                    visuals.text_color(),
                );
            }
        }
        let name_rect = egui::Rect::from_min_max(
            egui::pos2(rect.min.x + 2.0, image_rect.max.y + 2.0),
            rect.max - egui::vec2(2.0, 0.0),
        );
        let mut job = egui::text::LayoutJob::simple(
            entry.name.clone(),
            egui::FontId::proportional(11.0),
            visuals.text_color(),
            name_rect.width(),
        );
        job.halign = egui::Align::Center;
        let galley = painter.layout_job(job);
        painter.with_clip_rect(name_rect).galley(
            name_rect.center_top(),
            galley,
            visuals.text_color(),
        );
    }
    let response = response.on_hover_text(&entry.path);
    if response.clicked()
        && !entry.is_dir
        && AssetKind::of(&entry.path) == AssetKind::PhysicsMaterial
    {
        open_physics_material(state, &entry.path);
    }
    if response.clicked() && !entry.is_dir {
        state.project_selection = Some(entry.path.clone());
        state.inspected_asset = matches!(
            AssetKind::of(&entry.path),
            AssetKind::Material | AssetKind::Shader
        )
        .then(|| entry.path.clone());
    }
    if response.double_clicked() {
        if AssetKind::of(&entry.path) == AssetKind::Ui && !entry.is_dir {
            crate::ui_editor::open(state, &entry.path);
        } else if entry.is_dir {
            state.current_folder = entry.path.clone();
        } else if matches!(
            AssetKind::of(&entry.path),
            AssetKind::Scene | AssetKind::Prefab
        ) {
            state.action(Action::OpenScene(Some(entry.path.clone())));
        } else if matches!(
            AssetKind::of(&entry.path),
            AssetKind::Material | AssetKind::Shader
        ) && let Some(project) = state.project.as_ref()
            && let Err(error) = state.external_editor.open(project, &entry.path)
        {
            state.log.error(error);
        }
    }
    if !entry.is_dir {
        response.dnd_set_drag_payload(AssetDrag(entry.path.clone()));
    }
    response.context_menu(|ui| {
        create_menu(ui, state, create_target(Some(entry), &state.current_folder));
        ui.separator();
        if ui.button("Rename").clicked() {
            let references = find_references(state, &entry.path);
            state.project_dialog = Some(ProjectDialog::Rename {
                path: entry.path.clone(),
                name: entry.name.clone(),
                update_references: !references.is_empty(),
                references,
            });
        }
        if ui.button("Delete").clicked() {
            let references = find_references(state, &entry.path);
            state.project_dialog = Some(ProjectDialog::Delete {
                path: entry.path.clone(),
                is_dir: entry.is_dir,
                references,
            });
        }
    });
}

/// Resolves where a Create action from Project should write its asset.
pub fn create_target(entry: Option<&AssetEntry>, current_folder: &str) -> String {
    match entry {
        Some(entry) if entry.is_dir => entry.path.clone(),
        Some(entry) => entry
            .path
            .rsplit_once('/')
            .map_or("", |(parent, _)| parent)
            .to_owned(),
        None => current_folder.to_owned(),
    }
}

/// Подменю «Create» с пунктами создания ресурсов в `folder`; используется и в шапке, и в контекстных меню.
pub(super) fn create_menu(ui: &mut egui::Ui, state: &mut EditorState, folder: String) {
    ui.menu_button("Create", |ui| {
        if ui.button("Material").clicked() {
            state.project_dialog = Some(ProjectDialog::NewMaterial {
                folder: folder.clone(),
                name: "New Material".to_owned(),
            });
            ui.close();
        }
        if ui.button("Shader").clicked() {
            state.project_dialog = Some(ProjectDialog::NewShader {
                folder,
                name: "New Shader".to_owned(),
            });
            ui.close();
        }
    });
}

/// Показывает модальный диалог файловой операции и выполняет её при подтверждении.
pub fn show_dialog(ctx: &egui::Context, state: &mut EditorState) {
    let Some(mut dialog) = state.project_dialog.clone() else {
        return;
    };
    let mut confirmed = false;
    let mut close = false;
    egui::Modal::new(egui::Id::new("project_dialog")).show(ctx, |ui| {
        ui.set_max_width(380.0);
        match &mut dialog {
            ProjectDialog::NewUi { name } => {
                ui.heading("New UI Canvas");
                ui.text_edit_singleline(name);
            }
            ProjectDialog::NewFolder { name } => {
                ui.heading("New Folder");
                ui.text_edit_singleline(name).request_focus();
            }
            ProjectDialog::NewPhysicsMaterial { name } => {
                ui.heading("New Physics Material");
                ui.text_edit_singleline(name).request_focus();
            }
            ProjectDialog::NewMaterial { name, .. } => {
                ui.heading("New Material");
                ui.text_edit_singleline(name).request_focus();
            }
            ProjectDialog::NewShader { name, .. } => {
                ui.heading("New Shader");
                ui.text_edit_singleline(name).request_focus();
            }
            ProjectDialog::Rename {
                name,
                references,
                update_references,
                ..
            } => {
                ui.heading("Rename");
                ui.text_edit_singleline(name).request_focus();
                if !references.is_empty() {
                    ui.add_space(4.0);
                    ui.checkbox(update_references, "Обновить ссылки в файлах:");
                    show_references(ui, references);
                    if !*update_references {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "Ссылки останутся на старый путь и перестанут загружаться.",
                        );
                    }
                }
            }
            ProjectDialog::Delete {
                path,
                is_dir,
                references,
            } => {
                ui.heading("Delete");
                let what = if *is_dir {
                    "папку с содержимым"
                } else {
                    "файл"
                };
                ui.label(format!("Удалить {what} {path}? Это действие необратимо."));
                if !references.is_empty() {
                    ui.add_space(4.0);
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "На него ссылаются файлы; после удаления ссылки останутся битыми:",
                    );
                    show_references(ui, references);
                }
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let label = match dialog {
                ProjectDialog::Delete { .. } => "Delete",
                _ => "OK",
            };
            confirmed =
                ui.button(label).clicked() || ui.input(|input| input.key_pressed(egui::Key::Enter));
            close = ui.button("Cancel").clicked()
                || ui.input(|input| input.key_pressed(egui::Key::Escape));
        });
    });
    if close {
        state.project_dialog = None;
    } else if confirmed {
        state.project_dialog = None;
        run_dialog(state, &dialog);
    } else {
        state.project_dialog = Some(dialog);
    }
}

fn run_dialog(state: &mut EditorState, dialog: &ProjectDialog) {
    let Some(project) = state.project.as_ref() else {
        return;
    };
    if let ProjectDialog::NewUi { name } = dialog {
        let name = if name.ends_with(".rveui") {
            name.clone()
        } else {
            format!("{name}.rveui")
        };
        let text = "[canvas]\nlayer = 0\n";
        match project
            .assets_dir
            .create_file(&state.current_folder, &name, text)
        {
            Ok(path) => {
                state.action(Action::RefreshTree);
                crate::ui_editor::open(state, &path);
            }
            Err(error) => state.log.error(error.to_string()),
        }
        return;
    }
    if let ProjectDialog::NewPhysicsMaterial { name } = dialog {
        match project.create_physics_material(&state.current_folder, name) {
            Ok(path) => {
                state.log.info(format!("Создан физический материал {path}"));
                state.action(Action::RefreshTree);
            }
            Err(error) => state.log.error(error),
        }
        return;
    }
    if let ProjectDialog::NewMaterial { folder, name } = dialog {
        match project.create_material(folder, name) {
            Ok(path) => {
                state.log.info(format!("Создан материал {path}"));
                state.action(Action::RefreshTree);
            }
            Err(error) => state.log.error(error),
        }
        return;
    }
    if let ProjectDialog::NewShader { folder, name } = dialog {
        match project.create_shader(folder, name) {
            Ok(path) => {
                state.log.info(format!("Создан шейдер {path}"));
                state.action(Action::RefreshTree);
            }
            Err(error) => state.log.error(error),
        }
        return;
    }
    let assets = project.assets_dir.clone();
    let result = match dialog {
        ProjectDialog::NewFolder { name } => assets
            .create_folder(&state.current_folder, name)
            .map(|path| format!("Создана папка {path}")),
        ProjectDialog::Rename {
            path,
            name,
            references,
            update_references,
        } => match assets.rename(path, name) {
            Ok(target) => {
                let message = format!("{path} переименован в {target}");
                let remap = PathRemap::new(path, &target);
                follow_rename(state, &remap, references, *update_references);
                Ok(message)
            }
            Err(error) => Err(error),
        },
        ProjectDialog::NewPhysicsMaterial { .. } | ProjectDialog::NewUi { .. } => return,
        ProjectDialog::NewMaterial { .. } | ProjectDialog::NewShader { .. } => return,
        ProjectDialog::Delete { path, .. } => {
            assets.remove(path).map(|()| format!("Удалён {path}"))
        }
    };
    match result {
        Ok(message) => {
            state.log.info(message);
            state.action(Action::RefreshTree);
        }
        Err(error) => state.log.error(error.to_string()),
    }
}

/// Файлы, ссылающиеся на ресурс, который переименовывают или удаляют.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AssetReferences {
    /// Сцены, префабы и `prefabs.rveprefabs` на диске.
    pub files: Vec<Reference>,
    /// Открытые документы, у которых ссылки есть только в несохранённых правках.
    pub unsaved: Vec<String>,
}

impl AssetReferences {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.unsaved.is_empty()
    }
}

/// Ищет ссылки на `path` в файлах проекта и в несохранённых открытых документах.
pub fn find_references(state: &mut EditorState, path: &str) -> AssetReferences {
    let Some(project) = state.project.as_ref() else {
        return AssetReferences::default();
    };
    let remap = PathRemap::new(path, path);
    let scan = asset_refs::scan(&project.root, &project.assets_dir, &remap);
    for error in scan.errors {
        state
            .log
            .warn(format!("Поиск ссылок на {path}: не прочитан {error}"));
    }
    let on_disk = |source: &ReferenceSource| scan.references.iter().any(|r| &r.source == source);
    let mut unsaved = Vec::new();
    if let Some(scene) = state.scene.as_ref().filter(|scene| scene.is_dirty())
        && scene.count_asset_references(&remap) > 0
    {
        let source = scene
            .path()
            .map(|path| ReferenceSource::Asset(path.as_str().to_owned()));
        if !source.as_ref().is_some_and(on_disk) {
            unsaved.push(source.map_or("Untitled".to_owned(), |source| source.to_string()));
        }
    }
    if let Some(prefabs) = state
        .project_prefabs
        .as_ref()
        .filter(|list| list.is_dirty())
        && prefabs.count_references(&remap) > 0
        && !on_disk(&ReferenceSource::ProjectPrefabs)
    {
        unsaved.push(ReferenceSource::ProjectPrefabs.to_string());
    }
    AssetReferences {
        files: scan.references,
        unsaved,
    }
}

fn show_references(ui: &mut egui::Ui, references: &AssetReferences) {
    egui::ScrollArea::vertical()
        .max_height(160.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for reference in &references.files {
                ui.label(format!("{} — {}", reference.source, reference.count));
            }
            for label in &references.unsaved {
                ui.label(format!("{label} — несохранённые правки"));
            }
        });
}

/// После переименования переписывает ссылки в файлах и открытых документах и переносит путь
/// открытой сцены, если переименован её файл или папка.
fn follow_rename(
    state: &mut EditorState,
    remap: &PathRemap,
    references: &AssetReferences,
    update_references: bool,
) {
    let mut written = Vec::new();
    if update_references && let Some(project) = state.project.as_ref() {
        let (files, errors) =
            asset_refs::rewrite_files(&project.root, &project.assets_dir, &references.files, remap);
        for error in errors {
            state.log.error(format!("Ссылки не обновлены: {error}"));
        }
        if !files.is_empty() {
            let list = files
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            state.log.info(format!("Обновлены ссылки: {list}"));
        }
        written = files;
    }
    if let Some(scene) = state.scene.as_mut() {
        scene.follow_rename(remap);
        if update_references {
            let on_disk = scene.path().is_some_and(|path| {
                written.contains(&ReferenceSource::Asset(path.as_str().to_owned()))
            });
            if let Err(error) = scene.remap_asset_paths(remap, on_disk) {
                state
                    .log
                    .error(format!("Ссылки открытой сцены не обновлены: {error}"));
            }
        }
    }
    if update_references && let Some(prefabs) = state.project_prefabs.as_mut() {
        prefabs.remap_paths(remap, written.contains(&ReferenceSource::ProjectPrefabs));
    }
}

/// `.rvephysmat`, открытый панелью полей: значения записываются в файл при каждом изменении.
#[derive(Clone, Debug, PartialEq)]
pub struct PhysicsMaterialEdit {
    pub path: String,
    pub settings: PhysicsMaterialSettings,
}

fn open_physics_material(state: &mut EditorState, path: &str) {
    let Some(project) = state.project.as_ref() else {
        return;
    };
    let settings = project.read_text(path).and_then(|source| {
        PhysicsMaterialSettings::parse(path, &source).map_err(|error| error.to_string())
    });
    match settings {
        Ok(settings) => {
            state.physics_material_edit = Some(PhysicsMaterialEdit {
                path: path.to_owned(),
                settings,
            });
        }
        Err(error) => state.log.error(error),
    }
}

/// Панель полей выбранного `.rvephysmat`; изменение сразу сохраняется в файл.
fn show_physics_material_fields(ui: &mut egui::Ui, state: &mut EditorState) {
    let Some(mut edit) = state.physics_material_edit.take() else {
        return;
    };
    let mut changed = false;
    let mut close = false;
    ui.horizontal(|ui| {
        ui.strong(format!("Physics Material {}", edit.path));
        close = ui.button("Close").clicked();
    });
    egui::Grid::new("physics_material_fields")
        .num_columns(2)
        .show(ui, |ui| {
            changed |= field_drag(
                ui,
                "Static Friction",
                &mut edit.settings.static_friction,
                0.0..=f32::MAX,
            );
            changed |= field_drag(
                ui,
                "Dynamic Friction",
                &mut edit.settings.dynamic_friction,
                0.0..=f32::MAX,
            );
            changed |= field_drag(ui, "Bounciness", &mut edit.settings.bounciness, 0.0..=1.0);
            changed |= combine_row(ui, "Friction Combine", &mut edit.settings.friction_combine);
            changed |= combine_row(ui, "Bounce Combine", &mut edit.settings.bounce_combine);
        });
    if changed
        && let Some(project) = state.project.as_ref()
        && let Ok(path) = VirtualPath::parse(&edit.path)
        && let Err(error) = project.save_physics_material(&path, &edit.settings)
    {
        state.log.error(error);
    }
    if !close {
        state.physics_material_edit = Some(edit);
    }
}

fn field_drag(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
) -> bool {
    ui.label(label);
    let changed = ui
        .add(egui::DragValue::new(value).speed(0.01).range(range))
        .changed();
    ui.end_row();
    changed
}

fn combine_row(ui: &mut egui::Ui, label: &str, mode: &mut CombineMode) -> bool {
    ui.label(label);
    let before = *mode;
    egui::ComboBox::from_id_salt(label)
        .selected_text(combine_label(*mode))
        .show_ui(ui, |ui| {
            for option in [
                CombineMode::Average,
                CombineMode::Minimum,
                CombineMode::Multiply,
                CombineMode::Maximum,
            ] {
                ui.selectable_value(mode, option, combine_label(option));
            }
        });
    ui.end_row();
    *mode != before
}

fn combine_label(mode: CombineMode) -> &'static str {
    match mode {
        CombineMode::Average => "Average",
        CombineMode::Minimum => "Minimum",
        CombineMode::Multiply => "Multiply",
        CombineMode::Maximum => "Maximum",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str, path: &str, children: Vec<AssetEntry>) -> AssetEntry {
        AssetEntry {
            name: name.to_owned(),
            path: path.to_owned(),
            is_dir: true,
            children,
        }
    }

    #[test]
    fn nested_folders_are_found_by_path() {
        let tree = vec![folder(
            "a",
            "a",
            vec![folder("b", "a/b", vec![folder("c", "a/b/c", Vec::new())])],
        )];
        assert_eq!(find_folder(&tree, "").unwrap().len(), 1);
        assert_eq!(find_folder(&tree, "a/b").unwrap()[0].name, "c");
        assert!(find_folder(&tree, "a/x").is_none());
    }

    #[test]
    fn search_finds_nested_files_and_folders_ignoring_case() {
        let file = |name: &str, path: &str| AssetEntry {
            name: name.to_owned(),
            path: path.to_owned(),
            is_dir: false,
            children: Vec::new(),
        };
        let tree = vec![
            folder(
                "Models",
                "Models",
                vec![folder(
                    "Cars",
                    "Models/Cars",
                    vec![file("Car.fbx", "Models/Cars/Car.fbx")],
                )],
            ),
            file("car.png", "car.png"),
            file("tree.fbx", "tree.fbx"),
        ];
        let paths = search_entries(&tree, "CAR")
            .into_iter()
            .map(|entry| entry.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, ["Models/Cars", "Models/Cars/Car.fbx", "car.png"]);
        assert!(search_entries(&tree, "missing").is_empty());
    }

    #[test]
    fn create_target_uses_folder_file_parent_or_current_folder() {
        let folder = folder("Models", "Models", Vec::new());
        let file = AssetEntry {
            name: "ball.rvemat".to_owned(),
            path: "Materials/ball.rvemat".to_owned(),
            is_dir: false,
            children: Vec::new(),
        };
        assert_eq!(create_target(Some(&folder), "Current"), "Models");
        assert_eq!(create_target(Some(&file), "Current"), "Materials");
        assert_eq!(create_target(None, "Current"), "Current");
        assert_eq!(create_target(None, ""), "");
    }
}
