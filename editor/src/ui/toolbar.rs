//! Верхнее меню, строка инструментов и горячие клавиши.

use crate::layout::{Tab, is_open};
use crate::scene_edit::NewObject;
use crate::state::{Action, EditorState};
use crate::ui::scene_view;
use crate::viewport::gizmo::{GizmoSpace, GizmoTool};
use egui::{Key, KeyboardShortcut, Modifiers};
use egui_dock::DockState;

const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const UNDO: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Z);
const REDO: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Y);

pub fn handle_shortcuts(ctx: &egui::Context, state: &mut EditorState) {
    if ctx.input_mut(|input| input.consume_shortcut(&SAVE)) {
        state.action(Action::Save);
    }
    if ctx.input_mut(|input| input.consume_shortcut(&UNDO)) {
        undo(state);
    }
    if ctx.input_mut(|input| input.consume_shortcut(&REDO)) {
        redo(state);
    }
    // Одиночные клавиши не должны срабатывать при вводе текста и полёте камеры (ПКМ+WASD).
    if ctx.egui_wants_keyboard_input() || ctx.input(|input| input.pointer.secondary_down()) {
        return;
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
}

pub fn undo(state: &mut EditorState) {
    state.edit_scene(|scene| Ok(scene.undo()));
    state.gizmo_drag = None;
}

pub fn redo(state: &mut EditorState) {
    state.edit_scene(|scene| Ok(scene.redo()));
    state.gizmo_drag = None;
}

pub fn delete_selection(state: &mut EditorState) {
    let selection = state
        .scene
        .as_ref()
        .and_then(|scene| scene.selection())
        .map(str::to_owned);
    if let Some(id) = selection {
        state.edit_scene(|scene| scene.delete_object(&id));
    }
}

pub fn duplicate_selection(state: &mut EditorState) {
    let selection = state
        .scene
        .as_ref()
        .and_then(|scene| scene.selection())
        .map(str::to_owned);
    if let Some(id) = selection {
        state.edit_scene(|scene| scene.duplicate_object(&id));
    }
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
        if ui
            .add_enabled(can_change, egui::Button::new("Duplicate"))
            .clicked()
        {
            duplicate_selection(state);
        }
    });
    ui.menu_button("GameObject", |ui| {
        ui.add_enabled_ui(editing, |ui| {
            let items = [
                ("Create Empty", NewObject::Empty),
                ("Camera", NewObject::Camera),
                ("Directional Light", NewObject::DirectionalLight),
            ];
            for (label, kind) in items {
                if ui.button(label).clicked() {
                    state.create_object(&kind, None);
                }
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
    });
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
