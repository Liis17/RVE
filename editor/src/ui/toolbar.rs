//! Верхнее меню, строка инструментов и горячие клавиши.

use crate::layout::{Tab, default_layout, is_open};
use crate::scene_edit::{NewObject, objects_from_clipboard, objects_to_clipboard};
use crate::state::{Action, EditorState};
use crate::ui::scene_view;
use crate::viewport::gizmo::{GizmoSpace, GizmoTool};
use egui::{Key, KeyboardShortcut, Modifiers};
use egui_dock::DockState;

const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const UNDO: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Z);
const REDO: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Y);
/// Привычный для macOS redo; проверяется раньше `UNDO`, который игнорирует лишний Shift.
const REDO_SHIFT: KeyboardShortcut =
    KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::Z);
const DUPLICATE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::D);

pub fn handle_shortcuts(ctx: &egui::Context, state: &mut EditorState) {
    // Модальное окно перекрывает указатель, но не клавиатуру: правки за ним запрещены.
    if state.unsaved_prompt.is_some()
        || state.project_dialog.is_some()
        || state.recovery_prompt.is_some()
        || state.apply_prompt.is_some()
        || state.console_open
    {
        return;
    }
    if ctx.input_mut(|input| input.consume_shortcut(&SAVE)) {
        state.action(Action::Save);
    }
    if ctx.input_mut(|input| input.consume_shortcut(&REDO_SHIFT)) {
        redo(state);
    }
    if ctx.input_mut(|input| input.consume_shortcut(&UNDO)) {
        undo(state);
    }
    if ctx.input_mut(|input| input.consume_shortcut(&REDO)) {
        redo(state);
    }
    // Одиночные клавиши не должны срабатывать при вводе текста и полёте камеры (ПКМ+WASD).
    // Esc снимает фокус поля ещё до этого кадра, поэтому учитывается и фокус прошлого кадра.
    if ctx.egui_wants_keyboard_input()
        || state.text_focused_last_frame
        || ctx.input(|input| input.pointer.secondary_down())
    {
        return;
    }
    if ctx.input_mut(|input| input.consume_shortcut(&DUPLICATE)) {
        duplicate_selection(state);
    }
    // egui-winit превращает Ctrl/Cmd+C и +V в события Copy и Paste без события клавиши.
    let (copy, paste) = ctx.input(|input| {
        let copy = input.events.contains(&egui::Event::Copy);
        let paste = input.events.iter().find_map(|event| match event {
            egui::Event::Paste(text) => Some(text.clone()),
            _ => None,
        });
        (copy, paste)
    });
    if copy && let Some(text) = copy_selection(state) {
        ctx.copy_text(text);
    }
    if let Some(text) = paste {
        paste_objects(state, &text);
    }
    let pressed = |key| ctx.input(|input| input.key_pressed(key) && input.modifiers.is_none());
    if pressed(Key::W) {
        state.tool = GizmoTool::Move;
    }
    if pressed(Key::E) {
        state.tool = GizmoTool::Rotate;
    }
    if pressed(Key::R) {
        state.tool = GizmoTool::Scale;
    }
    if pressed(Key::F) {
        scene_view::focus_selection(state);
    }
    if pressed(Key::Delete) {
        delete_selection(state);
    }
    if pressed(Key::F2) {
        rename_selection(state);
    }
    if pressed(Key::Escape)
        && !state.is_playing()
        && let Some(scene) = &mut state.scene
    {
        scene.select(None);
    }
}

/// Начинает переименование выделенного объекта в Hierarchy.
pub fn rename_selection(state: &mut EditorState) {
    if state.is_playing() {
        return;
    }
    let selected = state
        .scene
        .as_ref()
        .and_then(|scene| scene.selected_object())
        .map(|object| (object.id.clone(), object.display_name().to_owned()));
    if let Some(rename) = selected {
        state.hierarchy_rename = Some(rename);
    }
}

pub fn undo(state: &mut EditorState) {
    state.edit_scene(|scene| Ok(scene.undo()));
    state.gizmo_drag = None;
}

pub fn redo(state: &mut EditorState) {
    state.edit_scene(|scene| Ok(scene.redo()));
    state.gizmo_drag = None;
}

/// Текст для буфера обмена ОС с выделенными поддеревьями; `None` — выделения нет.
pub fn copy_selection(state: &mut EditorState) -> Option<String> {
    let scene = state.scene.as_ref()?;
    let roots = scene.selection_roots();
    if roots.is_empty() {
        return None;
    }
    match scene
        .subtrees(&roots)
        .and_then(|objects| objects_to_clipboard(&objects))
    {
        Ok(text) => Some(text),
        Err(error) => {
            state.log.error(error);
            None
        }
    }
}

/// Вставляет объекты из буфера обмена; чужой текст игнорируется.
pub fn paste_objects(state: &mut EditorState, text: &str) {
    match objects_from_clipboard(text) {
        None => {}
        Some(Ok(objects)) => {
            state.edit_scene(|scene| scene.insert_copies(objects));
        }
        Some(Err(error)) => state.log.error(error),
    }
}

pub fn delete_selection(state: &mut EditorState) {
    state.edit_scene(|scene| scene.delete_selection());
}

pub fn duplicate_selection(state: &mut EditorState) {
    state.edit_scene(|scene| scene.duplicate_selection());
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState, dock: &mut DockState<Tab>) {
    egui::Panel::top("menu_bar").show(ui, |ui| {
        egui::MenuBar::new().ui(ui, |ui| show_menus(ui, state, dock));
    });
    if state.project.is_some() {
        egui::Panel::top("tool_bar").show(ui, |ui| {
            ui.horizontal(|ui| show_tools(ui, state));
        });
    }
}

fn show_menus(ui: &mut egui::Ui, state: &mut EditorState, dock: &mut DockState<Tab>) {
    let has_project = state.project.is_some();
    let editing = has_project && !state.is_playing();
    let has_selection = state
        .scene
        .as_ref()
        .is_some_and(|scene| scene.selection().is_some());
    ui.menu_button("File", |ui| {
        if ui
            .add_enabled(editing, egui::Button::new("New Scene"))
            .clicked()
        {
            state.action(Action::NewScene);
        }
        if ui
            .add_enabled(!state.is_playing(), egui::Button::new("Open Project…"))
            .clicked()
        {
            state.action(Action::OpenProject);
        }
        ui.add_enabled_ui(
            !state.is_playing() && !state.recent_projects.is_empty(),
            |ui| ui.menu_button("Open Recent", |ui| super::recent_projects(ui, state)),
        );
        if ui
            .add_enabled(editing, egui::Button::new("Open Scene…"))
            .clicked()
        {
            state.action(Action::OpenScene(None));
        }
        ui.separator();
        let save = egui::Button::new("Save").shortcut_text(ui.ctx().format_shortcut(&SAVE));
        if ui.add_enabled(editing, save).clicked() {
            state.action(Action::Save);
        }
        if ui
            .add_enabled(editing, egui::Button::new("Save As…"))
            .clicked()
        {
            state.action(Action::SaveAs);
        }
        ui.separator();
        if ui
            .add_enabled(
                editing && state.build.is_none(),
                egui::Button::new("Build Player…"),
            )
            .clicked()
        {
            state.action(Action::BuildPlayer);
        }
        ui.separator();
        if ui.button("Exit").clicked() {
            state.action(Action::Exit);
        }
    });
    ui.menu_button("Edit", |ui| {
        let scene = state.scene.as_ref();
        let can_undo = editing && scene.is_some_and(|scene| scene.can_undo());
        let can_redo = editing && scene.is_some_and(|scene| scene.can_redo());
        let undo_button = egui::Button::new("Undo").shortcut_text(ui.ctx().format_shortcut(&UNDO));
        if ui.add_enabled(can_undo, undo_button).clicked() {
            undo(state);
        }
        let redo_button = egui::Button::new("Redo").shortcut_text(ui.ctx().format_shortcut(&REDO));
        if ui.add_enabled(can_redo, redo_button).clicked() {
            redo(state);
        }
        ui.separator();
        let can_change = editing && has_selection;
        if ui
            .add_enabled(can_change, egui::Button::new("Delete").shortcut_text("Del"))
            .clicked()
        {
            delete_selection(state);
        }
        let duplicate =
            egui::Button::new("Duplicate").shortcut_text(ui.ctx().format_shortcut(&DUPLICATE));
        if ui.add_enabled(can_change, duplicate).clicked() {
            duplicate_selection(state);
        }
        if ui
            .add_enabled(can_change, egui::Button::new("Rename").shortcut_text("F2"))
            .clicked()
        {
            rename_selection(state);
        }
    });
    ui.menu_button("GameObject", |ui| {
        ui.add_enabled_ui(editing, |ui| {
            let items = [
                ("Create Empty", NewObject::Empty),
                ("Camera", NewObject::Camera),
                ("Directional Light", NewObject::DirectionalLight),
                ("Point Light", NewObject::PointLight),
                ("Spot Light", NewObject::SpotLight),
            ];
            for (label, kind) in items {
                if ui.button(label).clicked() {
                    state.create_object(&kind, None);
                }
            }
            let already_has_environment = state.scene.as_ref().is_some_and(|scene| {
                scene
                    .view()
                    .objects
                    .iter()
                    .any(|object| object.environment.is_some())
            });
            if ui
                .add_enabled(!already_has_environment, egui::Button::new("Environment"))
                .on_disabled_hover_text("В сцене уже есть Environment")
                .clicked()
            {
                state.create_object(&NewObject::Environment, None);
            }
            ui.menu_button("Mesh", |ui| {
                let models = state.assets_of_kind(crate::assets_fs::AssetKind::Model);
                if models.is_empty() {
                    ui.label("В Assets нет .fbx");
                }
                for model in models {
                    if ui.button(&model).clicked() {
                        let position = state.camera.target.to_array();
                        state.create_from_asset(&model, None, position);
                    }
                }
            });
        });
    });
    ui.menu_button("Window", |ui| {
        for tab in Tab::ALL {
            let open = is_open(dock, tab);
            if ui
                .add_enabled(!open, egui::Button::selectable(open, tab.title()))
                .clicked()
            {
                dock.push_to_first_leaf(tab);
            }
        }
        ui.separator();
        if ui.button("Reset Layout").clicked() {
            *dock = default_layout();
        }
    });
}

/// Поле шага привязки; шаг всегда положительный.
fn snap_step<'a>(value: &'a mut f32, prefix: &str, suffix: &str) -> egui::DragValue<'a> {
    egui::DragValue::new(value)
        .speed(0.01)
        .range(0.001..=f32::MAX)
        .max_decimals(3)
        .prefix(prefix)
        .suffix(suffix)
}

fn show_tools(ui: &mut egui::Ui, state: &mut EditorState) {
    for (tool, label, tip) in [
        (GizmoTool::Move, "Move", "Перемещение (W)"),
        (GizmoTool::Rotate, "Rotate", "Вращение (E)"),
        (GizmoTool::Scale, "Scale", "Масштаб (R)"),
    ] {
        ui.selectable_value(&mut state.tool, tool, label)
            .on_hover_text(tip);
    }
    ui.separator();
    ui.selectable_value(&mut state.space, GizmoSpace::Global, "Global");
    ui.selectable_value(&mut state.space, GizmoSpace::Local, "Local");
    ui.separator();
    let snap = &mut state.snap;
    ui.toggle_value(&mut snap.enabled, "Snap")
        .on_hover_text("Привязка к шагу; без неё действует при удержании Ctrl");
    ui.add(snap_step(&mut snap.move_step, "Move ", ""));
    ui.add(snap_step(&mut snap.rotate_step_deg, "Rotate ", "°"));
    ui.add(snap_step(&mut snap.scale_step, "Scale ", ""));
    ui.separator();
    let playing = state.is_playing();
    if ui.selectable_label(playing, "Play").clicked() {
        state.action(if playing { Action::Stop } else { Action::Play });
    }
    let paused = state.play.as_ref().is_some_and(|play| play.is_paused());
    if ui
        .add_enabled(playing, egui::Button::selectable(paused, "Pause"))
        .clicked()
        && let Some(play) = &mut state.play
    {
        play.set_paused(!paused);
    }
    if ui.add_enabled(playing, egui::Button::new("Stop")).clicked() {
        state.action(Action::Stop);
    }
}
