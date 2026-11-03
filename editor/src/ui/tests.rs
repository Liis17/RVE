//! Интеграционные тесты интерфейса без окна и GPU: ввод подаётся событиями, как от egui-winit.

use super::*;
use crate::layout::default_layout;
use crate::project::OpenProject;
use crate::scene_edit::{EditorScene, NewObject};
use crate::state::AssetDrag;
use crate::viewport::camera::project_point;
use crate::viewport::gizmo::GizmoTool;
use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, pos2, vec2};
use engine_assets::{Model, ModelMesh, ModelVertex};
use engine_files::VirtualPath;
use glam::Vec3;
use std::path::Path;
use std::sync::Arc;

const SCREEN: Rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1280.0, 720.0));

type Panel = fn(&mut egui::Ui, &mut EditorState);

struct Harness {
    ctx: egui::Context,
    state: EditorState,
    time: f64,
}

impl Harness {
    fn new() -> Self {
        let mut state = EditorState::new(None);
        state.scene = Some(EditorScene::new_untitled().unwrap());
        Self {
            ctx: egui::Context::default(),
            state,
            time: 0.0,
        }
    }

    fn scene(&self) -> &EditorScene {
        self.state.scene.as_ref().unwrap()
    }

    fn scene_mut(&mut self) -> &mut EditorScene {
        self.state.scene.as_mut().unwrap()
    }

    fn input(&mut self, events: Vec<Event>) -> RawInput {
        self.time += 0.1;
        RawInput {
            screen_rect: Some(SCREEN),
            time: Some(self.time),
            events,
            focused: true,
            ..RawInput::default()
        }
    }

    /// Один кадр только с панелью `panel` на весь экран.
    fn panel(&mut self, events: Vec<Event>, panel: Panel) {
        let input = self.input(events);
        let state = &mut self.state;
        let mut output = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| panel(ui, state));
        });
        output.textures_delta.clear();
    }

    fn click(&mut self, at: Pos2, panel: Panel) {
        self.panel(vec![Event::PointerMoved(at)], panel);
        self.panel(vec![button(at, PointerButton::Primary, true)], panel);
        self.panel(vec![button(at, PointerButton::Primary, false)], panel);
        self.panel(Vec::new(), panel);
    }

    fn drag(&mut self, from: Pos2, to: Pos2, pointer: PointerButton, panel: Panel) {
        self.panel(vec![Event::PointerMoved(from)], panel);
        self.panel(vec![button(from, pointer, true)], panel);
        for step in 1..=4 {
            let at = from + (to - from) * (step as f32 / 4.0);
            self.panel(vec![Event::PointerMoved(at)], panel);
        }
        self.panel(vec![button(to, pointer, false)], panel);
        self.panel(Vec::new(), panel);
    }

    /// Положение точки мира на экране, когда панель Scene занимает весь экран.
    fn project(&self, point: glam::Vec3) -> Pos2 {
        let [width, height] = self.state.scene_view.size;
        let view_projection = self.state.camera.camera().view_projection(width, height);
        let [u, v] = project_point(view_projection, point).unwrap();
        pos2(u * SCREEN.width(), v * SCREEN.height())
    }
}

fn button(pos: Pos2, button: PointerButton, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: Modifiers::NONE,
    }
}

fn key(key: egui::Key) -> Event {
    key_with(key, Modifiers::NONE)
}

fn key_with(key: egui::Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

impl Harness {
    /// Кадр только с обработкой горячих клавиш; модификаторы держатся весь кадр.
    fn shortcuts(&mut self, mut events: Vec<Event>, modifiers: Modifiers) {
        events.insert(0, Event::ModifiersChanged(modifiers));
        let input = self.input(events);
        let state = &mut self.state;
        let mut output = self.ctx.run_ui(input, |ui| {
            toolbar::handle_shortcuts(ui.ctx(), state);
        });
        output.textures_delta.clear();
    }

    fn press(&mut self, key: egui::Key, modifiers: Modifiers) {
        self.shortcuts(vec![key_with(key, modifiers)], modifiers);
    }
}

/// Куб с ребром 2 с центром в начале координат модели.
fn cube() -> Arc<Model> {
    let positions = [
        [-1.0, -1.0, -1.0],
        [1.0, -1.0, -1.0],
        [1.0, 1.0, -1.0],
        [-1.0, 1.0, -1.0],
        [-1.0, -1.0, 1.0],
        [1.0, -1.0, 1.0],
        [1.0, 1.0, 1.0],
        [-1.0, 1.0, 1.0],
    ];
    Arc::new(Model {
        meshes: vec![ModelMesh {
            vertices: positions
                .map(|position| ModelVertex {
                    position,
                    normal: [0.0, 1.0, 0.0],
                    uv: [0.0, 0.0],
                })
                .to_vec(),
            indices: vec![
                0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7, 0, 1, 5, 0, 5, 4, 2, 3, 7, 2, 7, 6, 0, 3, 7, 0,
                7, 4, 1, 2, 6, 1, 6, 5,
            ],
            material: Default::default(),
        }],
        textures: Vec::new(),
    })
}

const CUBE_POSITION: glam::Vec3 = glam::Vec3::new(3.0, 0.0, 0.0);

/// Сцена с кубом вдали от маркеров камеры и света.
fn harness_with_cube() -> (Harness, String) {
    let mut harness = Harness::new();
    let scene = harness.scene_mut();
    let model = VirtualPath::parse("/cube.fbx").unwrap();
    let id = scene
        .create_object(&NewObject::Mesh(model, [0.0; 3]), None)
        .unwrap();
    let mut transform = scene.object(&id).unwrap().transform;
    transform.position = CUBE_POSITION.to_array();
    scene.set_transform(&id, transform, None).unwrap();
    scene.select(None);
    harness.state.models.insert("cube.fbx".to_owned(), cube());
    // Первый кадр определяет размер панели.
    harness.panel(Vec::new(), scene_view::show);
    (harness, id)
}

/// Точка на оси Y гизмо куба, за которую можно взяться.
fn y_handle_grab(harness: &Harness) -> Pos2 {
    let handle = CUBE_POSITION.distance(harness.state.camera.eye()) * 0.18;
    harness.project(CUBE_POSITION + glam::Vec3::Y * handle * 0.8)
}

#[test]
fn whole_interface_builds_with_a_real_project_and_tessellates() {
    let mut harness = Harness::new();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
    let project = OpenProject::open(Path::new(dir)).unwrap();
    harness.state.asset_tree = project.assets_dir.list().unwrap();
    harness.state.project = Some(project);
    let mut dock = default_layout();
    for _ in 0..3 {
        let input = harness.input(Vec::new());
        let state = &mut harness.state;
        let mut output = harness.ctx.run_ui(input, |ui| show(ui, state, &mut dock));
        output.textures_delta.clear();
        let primitives = harness
            .ctx
            .tessellate(std::mem::take(&mut output.shapes), output.pixels_per_point);
        assert!(!primitives.is_empty());
    }
    assert!(harness.state.scene_view.visible);
    assert!(harness.state.scene_view.size[0] > 0);
}

#[test]
fn without_a_project_only_the_start_screen_is_available() {
    let mut harness = Harness::new();
    harness.state.scene = None;
    let mut dock = default_layout();
    let input = harness.input(Vec::new());
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| show(ui, state, &mut dock));
    output.textures_delta.clear();
    assert!(!harness.state.scene_view.visible);
}

#[test]
fn clicking_a_hierarchy_row_selects_the_object() {
    let mut harness = Harness::new();
    for _ in 0..2 {
        harness.panel(Vec::new(), hierarchy::show);
    }
    // Первая строка находится просмотром колонки сверху вниз.
    let mut selected = None;
    for y in (0..80).step_by(4) {
        harness.click(pos2(40.0, y as f32), hierarchy::show);
        selected = harness.scene().selection().map(str::to_owned);
        if selected.is_some() {
            break;
        }
    }
    assert_eq!(selected.as_deref(), Some("camera"));
}

#[test]
fn clicking_a_mesh_in_the_viewport_selects_it_and_empty_space_clears_selection() {
    let (mut harness, id) = harness_with_cube();
    let center = harness.project(CUBE_POSITION);
    harness.click(center, scene_view::show);
    assert_eq!(harness.scene().selection(), Some(id.as_str()));
    harness.click(pos2(30.0, 30.0), scene_view::show);
    assert_eq!(harness.scene().selection(), None);
}

#[test]
fn dragging_the_move_handle_moves_along_the_axis_as_one_undo_step() {
    let (mut harness, id) = harness_with_cube();
    harness.scene_mut().select(Some(id.as_str()));
    harness.panel(Vec::new(), scene_view::show);
    let grab = y_handle_grab(&harness);
    harness.drag(
        grab,
        grab - vec2(0.0, 60.0),
        PointerButton::Primary,
        scene_view::show,
    );
    let position = harness.scene().object(&id).unwrap().transform.position;
    assert!(position[1] > 0.1, "{position:?}");
    assert_eq!(position[0], CUBE_POSITION.x);
    assert_eq!(position[2], CUBE_POSITION.z);
    assert!(harness.state.gizmo_drag.is_none());
    let scene = harness.scene_mut();
    assert!(scene.undo());
    assert_eq!(
        scene.object(&id).unwrap().transform.position,
        CUBE_POSITION.to_array()
    );
}

#[test]
fn dragging_the_scale_handle_changes_only_that_axis() {
    let (mut harness, id) = harness_with_cube();
    harness.state.tool = GizmoTool::Scale;
    harness.scene_mut().select(Some(id.as_str()));
    harness.panel(Vec::new(), scene_view::show);
    let grab = y_handle_grab(&harness);
    harness.drag(
        grab,
        grab - vec2(0.0, 40.0),
        PointerButton::Primary,
        scene_view::show,
    );
    let scale = harness.scene().object(&id).unwrap().transform.scale;
    assert!(scale[1] > 1.0, "{scale:?}");
    assert_eq!((scale[0], scale[2]), (1.0, 1.0));
}

#[test]
fn secondary_drag_orbits_and_middle_drag_pans_without_touching_the_document() {
    let (mut harness, _) = harness_with_cube();
    let revision = harness.scene().revision();
    let start = harness.state.camera;
    let (from, to) = (pos2(200.0, 200.0), pos2(300.0, 240.0));
    harness.drag(from, to, PointerButton::Secondary, scene_view::show);
    assert_ne!(harness.state.camera.yaw, start.yaw);
    assert_eq!(harness.state.camera.target, start.target);
    harness.drag(from, to, PointerButton::Middle, scene_view::show);
    assert_ne!(harness.state.camera.target, start.target);
    assert_eq!(harness.scene().revision(), revision);
}

#[test]
fn play_mode_blocks_selection_gizmo_and_document_edits() {
    let (mut harness, id) = harness_with_cube();
    harness.scene_mut().select(Some(&id));
    let document = harness.scene().document().clone();
    harness.state.play =
        Some(crate::play::PlaySession::start(&document, None, |_| Ok(String::new())).unwrap());
    let revision = harness.scene().revision();
    harness.state.create_object(&NewObject::Empty, None);
    harness.panel(Vec::new(), scene_view::show);
    let center = harness.project(CUBE_POSITION);
    harness.click(center, scene_view::show);
    assert_eq!(harness.scene().revision(), revision);
    assert_eq!(harness.scene().selection(), Some(id.as_str()));
    assert!(harness.state.gizmo_drag.is_none());
}

#[test]
fn tool_shortcuts_apply_unless_a_text_field_has_focus() {
    let mut harness = Harness::new();
    let input = harness.input(vec![key(egui::Key::E)]);
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        toolbar::handle_shortcuts(ui.ctx(), state);
    });
    output.textures_delta.clear();
    assert_eq!(harness.state.tool, GizmoTool::Rotate);

    let text_id = egui::Id::new("focused_text");
    let mut text = String::new();
    let input = harness.input(Vec::new());
    let mut output = harness.ctx.run_ui(input, |ui| {
        ui.add(egui::TextEdit::singleline(&mut text).id(text_id));
        ui.ctx().memory_mut(|memory| memory.request_focus(text_id));
    });
    output.textures_delta.clear();
    let input = harness.input(vec![key(egui::Key::R)]);
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        ui.add(egui::TextEdit::singleline(&mut text).id(text_id));
        toolbar::handle_shortcuts(ui.ctx(), state);
    });
    output.textures_delta.clear();
    assert_eq!(harness.state.tool, GizmoTool::Rotate);
}

#[test]
fn ctrl_shift_z_redoes_instead_of_undoing() {
    let mut harness = Harness::new();
    let id = harness
        .scene_mut()
        .create_object(&NewObject::Empty, None)
        .unwrap();
    harness.press(egui::Key::Z, Modifiers::COMMAND);
    assert!(harness.scene().object(&id).is_none());
    harness.press(egui::Key::Z, Modifiers::COMMAND | Modifiers::SHIFT);
    assert!(harness.scene().object(&id).is_some());
    assert!(!harness.scene().can_redo());
}

#[test]
fn ctrl_d_duplicates_the_selection() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    harness.press(egui::Key::D, Modifiers::COMMAND);
    let scene = harness.scene();
    assert_eq!(scene.document().objects.len(), 3);
    let copy = scene.selection().unwrap();
    assert_ne!(copy, "camera");
    assert!(scene.object(copy).unwrap().camera.is_some());
}

#[test]
fn f2_starts_renaming_and_escape_clears_the_selection() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    harness.press(egui::Key::F2, Modifiers::NONE);
    assert_eq!(
        harness.state.hierarchy_rename,
        Some(("camera".to_owned(), "Camera".to_owned()))
    );
    harness.state.hierarchy_rename = None;
    harness.press(egui::Key::Escape, Modifiers::NONE);
    assert_eq!(harness.scene().selection(), None);
}

#[test]
fn escape_keeps_the_selection_while_a_dialog_is_open_or_playing() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    harness.state.project_dialog = Some(project_panel::ProjectDialog::NewFolder {
        name: String::new(),
    });
    harness.press(egui::Key::Escape, Modifiers::NONE);
    assert_eq!(harness.scene().selection(), Some("camera"));

    harness.state.project_dialog = None;
    let document = harness.scene().document().clone();
    harness.state.play =
        Some(crate::play::PlaySession::start(&document, None, |_| Ok(String::new())).unwrap());
    harness.press(egui::Key::Escape, Modifiers::NONE);
    harness.press(egui::Key::F2, Modifiers::NONE);
    assert_eq!(harness.scene().selection(), Some("camera"));
    assert_eq!(harness.state.hierarchy_rename, None);
}

#[test]
fn dropping_a_model_on_the_scene_places_it_on_the_floor_under_the_cursor() {
    let mut harness = Harness::new();
    harness.panel(Vec::new(), scene_view::show);
    let at = pos2(640.0, 500.0);
    harness.panel(vec![Event::PointerMoved(at)], scene_view::show);
    egui::DragAndDrop::set_payload(&harness.ctx, AssetDrag("models/a.fbx".to_owned()));
    harness.panel(
        vec![button(at, PointerButton::Primary, false)],
        scene_view::show,
    );

    let id = harness.state.pending_frame.clone().unwrap();
    let position = harness.scene().object(&id).unwrap().transform.position;
    assert_eq!(position[1], 0.0);
    let [width, height] = harness.state.scene_view.size;
    let view_projection = harness.state.camera.camera().view_projection(width, height);
    let [u, v] = project_point(view_projection, Vec3::from_array(position)).unwrap();
    assert!((u - 0.5).abs() < 0.02, "{u}");
    assert!(v > 0.6, "{v}");
}

#[test]
fn game_view_shows_the_image_and_takes_a_camera_from_the_scene() {
    let mut harness = Harness::new();
    harness.panel(Vec::new(), game_view::show);
    assert!(harness.state.game_view.visible);
    assert!(harness.state.game_view.size[0] > 0);
    assert_eq!(harness.state.game_camera.as_deref(), Some("camera"));

    // Камера исчезла из сцены: вкладка возвращается к первой.
    harness.state.game_camera = Some("deleted".to_owned());
    harness.panel(Vec::new(), game_view::show);
    assert_eq!(harness.state.game_camera.as_deref(), Some("camera"));

    harness.state.scene = None;
    harness.state.game_view.visible = false;
    harness.panel(Vec::new(), game_view::show);
    assert!(!harness.state.game_view.visible);
}
