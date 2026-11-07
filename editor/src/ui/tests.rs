//! Интеграционные тесты интерфейса без окна и GPU: ввод подаётся событиями, как от egui-winit.

use super::*;
use crate::layout::default_layout;
use crate::project::OpenProject;
use crate::scene_edit::{EditorScene, NewObject};
use crate::state::{AssetDrag, ViewCommand};
use crate::viewport::camera::{EditorCamera, View, project_point};
use crate::viewport::gizmo::GizmoTool;
use egui::{Event, Modifiers, PointerButton, Pos2, RawInput, Rect, pos2, vec2};
use engine_assets::{Model, ModelMesh, ModelVertex};
use engine_files::VirtualPath;
use glam::Vec3;
use std::collections::BTreeMap;
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
    scene
        .set_transforms(&[(id.clone(), transform)], None)
        .unwrap();
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

impl Harness {
    fn click_with(&mut self, at: Pos2, modifiers: Modifiers, panel: Panel) {
        let changed = Event::ModifiersChanged(modifiers);
        let press = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers,
        };
        self.panel(vec![changed.clone(), Event::PointerMoved(at)], panel);
        self.panel(vec![changed.clone(), press(true)], panel);
        self.panel(vec![changed.clone(), press(false)], panel);
        self.panel(vec![Event::ModifiersChanged(Modifiers::NONE)], panel);
    }
}

#[test]
fn ctrl_click_builds_a_group_that_the_move_handle_drags_as_one_undo_step() {
    let (mut harness, first) = harness_with_cube();
    let second_position = Vec3::new(0.0, 0.0, 3.0);
    let scene = harness.scene_mut();
    let model = VirtualPath::parse("/cube.fbx").unwrap();
    let second = scene
        .create_object(&NewObject::Mesh(model, second_position.to_array()), None)
        .unwrap();
    scene.select(None);
    harness.panel(Vec::new(), scene_view::show);

    let first_center = harness.project(CUBE_POSITION);
    let second_center = harness.project(second_position);
    harness.click(first_center, scene_view::show);
    harness.click_with(second_center, Modifiers::COMMAND, scene_view::show);
    assert_eq!(
        harness.scene().selected_ids(),
        [first.clone(), second.clone()]
    );
    // Ctrl-щелчок по выделенному снимает только его.
    harness.click_with(second_center, Modifiers::COMMAND, scene_view::show);
    assert_eq!(harness.scene().selected_ids(), std::slice::from_ref(&first));
    harness.click_with(second_center, Modifiers::SHIFT, scene_view::show);
    assert_eq!(
        harness.scene().selected_ids(),
        [first.clone(), second.clone()]
    );

    // Гизмо стоит в среднем положении группы.
    let center = (CUBE_POSITION + second_position) * 0.5;
    let handle = center.distance(harness.state.camera.eye()) * 0.18;
    let grab = harness.project(center + Vec3::Y * handle * 0.8);
    harness.drag(
        grab,
        grab - vec2(0.0, 60.0),
        PointerButton::Primary,
        scene_view::show,
    );
    let y = |harness: &Harness, id: &str| harness.scene().object(id).unwrap().transform.position;
    let (a, b) = (y(&harness, &first), y(&harness, &second));
    assert!(a[1] > 0.1, "{a:?}");
    assert!((a[1] - b[1]).abs() < 1e-5, "{a:?} {b:?}");
    assert_eq!((a[0], b[2]), (CUBE_POSITION.x, second_position.z));
    assert!(harness.scene_mut().undo());
    assert_eq!(y(&harness, &first), CUBE_POSITION.to_array());
    assert_eq!(y(&harness, &second), second_position.to_array());
}

#[test]
fn enabled_snapping_moves_the_object_in_whole_steps() {
    let (mut harness, id) = harness_with_cube();
    harness.state.snap.enabled = true;
    harness.state.snap.move_step = 0.5;
    harness.scene_mut().select(Some(id.as_str()));
    harness.panel(Vec::new(), scene_view::show);
    let grab = y_handle_grab(&harness);
    harness.drag(
        grab,
        grab - vec2(0.0, 90.0),
        PointerButton::Primary,
        scene_view::show,
    );
    let y = harness.scene().object(&id).unwrap().transform.position[1];
    assert!(y >= 0.5, "{y}");
    assert!((y / 0.5 - (y / 0.5).round()).abs() < 1e-4, "{y}");
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
fn tool_shortcuts_are_blocked_while_the_console_is_open() {
    let mut harness = Harness::new();
    harness.state.console_open = true;
    for key in [egui::Key::W, egui::Key::E, egui::Key::R] {
        harness.press(key, Modifiers::NONE);
        assert_eq!(harness.state.tool, GizmoTool::Move);
    }
}

#[test]
fn queued_numpad_frame_and_standard_view_apply_once_and_keep_the_target() {
    let (mut harness, id) = harness_with_cube();
    harness.scene_mut().select(Some(id.as_str()));
    harness.state.view_commands.push(ViewCommand::Frame);
    harness.shortcuts(Vec::new(), Modifiers::NONE);
    assert!((harness.state.camera.target - CUBE_POSITION).length() < 1e-4);
    assert!(harness.state.view_commands.is_empty());

    let distance = harness.state.camera.distance;
    harness
        .state
        .view_commands
        .push(ViewCommand::Look(View::Right));
    harness.shortcuts(Vec::new(), Modifiers::NONE);
    let camera = harness.state.camera;
    assert!((camera.target - CUBE_POSITION).length() < 1e-4);
    assert_eq!(camera.distance, distance);
    assert!((camera.eye() - camera.target).normalize().x > 0.999);
}

#[test]
fn queued_numpad_commands_are_dropped_while_typing_or_a_dialog_is_open() {
    let mut harness = Harness::new();
    let before = harness.state.camera;
    let text_id = egui::Id::new("numpad_text");
    let mut text = String::new();
    harness
        .state
        .view_commands
        .push(ViewCommand::Look(View::Top));
    let input = harness.input(Vec::new());
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        ui.add(egui::TextEdit::singleline(&mut text).id(text_id));
        ui.ctx().memory_mut(|memory| memory.request_focus(text_id));
        toolbar::handle_shortcuts(ui.ctx(), state);
    });
    output.textures_delta.clear();
    assert_eq!(harness.state.camera, before);
    assert!(harness.state.view_commands.is_empty());

    harness.state.console_open = true;
    harness
        .state
        .view_commands
        .push(ViewCommand::Look(View::Top));
    harness.shortcuts(Vec::new(), Modifiers::NONE);
    assert_eq!(harness.state.camera, before);
}

#[test]
fn numpad_zero_aligns_the_scene_camera_to_the_game_camera_or_does_nothing_without_one() {
    let mut harness = Harness::new();
    let game = {
        let world = harness.state.world_mut().unwrap();
        world.camera_of(world.active_camera()).unwrap()
    };
    harness.state.view_commands.push(ViewCommand::AlignToGame);
    harness.shortcuts(Vec::new(), Modifiers::NONE);
    let camera = harness.state.camera;
    assert!((camera.eye() - game.position).length() < 1e-3);
    let forward = (game.target - game.position).normalize();
    assert!((camera.target - camera.eye()).normalize().dot(forward) > 0.999);

    let before = EditorCamera { yaw: 1.0, ..camera };
    harness.state.camera = before;
    harness.state.scene = None;
    harness.state.view_commands.push(ViewCommand::AlignToGame);
    harness.shortcuts(Vec::new(), Modifiers::NONE);
    assert_eq!(harness.state.camera, before);
}

/// Полный кадр интерфейса, как в приложении: `ui::show` сам сбрасывает видимость панелей.
fn full_frame(harness: &mut Harness, dock: &mut DockState<Tab>) {
    let input = harness.input(Vec::new());
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| show(ui, state, dock));
    output.textures_delta.clear();
}

#[test]
fn numpad_view_in_play_needs_the_scene_panel_shown_on_the_previous_frame() {
    let (mut harness, _id) = harness_with_cube();
    // Без проекта `ui::show` показывает только стартовый экран, а dock с панелями не рисуется.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
    harness.state.project = Some(OpenProject::open(Path::new(dir)).unwrap());
    let mut dock = default_layout();
    let document = harness.scene().document().clone();
    harness.state.play =
        Some(crate::play::PlaySession::start(&document, None, |_| Ok(String::new())).unwrap());
    // Первые кадры раскладывают dock; панель Scene появляется в нём не сразу.
    for _ in 0..3 {
        full_frame(&mut harness, &mut dock);
    }
    assert!(harness.state.scene_view.visible);

    harness.state.scene_view.visible = false;
    let before = harness.state.camera;
    harness
        .state
        .view_commands
        .push(ViewCommand::Look(View::Top));
    full_frame(&mut harness, &mut dock);
    assert_eq!(harness.state.camera, before);
    assert!(harness.state.view_commands.is_empty());

    harness
        .state
        .view_commands
        .push(ViewCommand::Look(View::Front));
    full_frame(&mut harness, &mut dock);
    assert_eq!(harness.state.camera.yaw, 0.0);
    assert_eq!(harness.state.camera.pitch, 0.0);
}

#[test]
fn showing_the_inspector_keeps_hdr_light_colors_and_material_overrides_unchanged() {
    let mut harness = Harness::new();
    let scene = harness.scene_mut();
    scene
        .edit_object("sun", None, |object| {
            object.directional_light.as_mut().unwrap().color = [2.5, 1.5, 0.5];
            Ok(())
        })
        .unwrap();
    let model = VirtualPath::parse("/cube.fbx").unwrap();
    let mesh = scene
        .create_object(&NewObject::Mesh(model, [0.0; 3]), None)
        .unwrap();
    scene
        .set_material(
            &mesh,
            Some(engine_scene::document::MaterialOverride {
                base_color: Some([0.2, 0.4, 0.6]),
                metallic: Some(0.5),
                roughness: None,
                metallic_roughness_texture: None,
                normal_texture: None,
                normal_scale: Some(2.0),
                double_sided: None,
                ..engine_scene::document::MaterialOverride::default()
            }),
            None,
        )
        .unwrap();
    let before = harness.scene().document().to_toml().unwrap();
    let revision = harness.scene().revision();
    for selected in ["sun", mesh.as_str()] {
        harness.scene_mut().select(Some(selected));
        for _ in 0..2 {
            harness.panel(Vec::new(), inspector::show);
        }
    }
    assert_eq!(harness.scene().revision(), revision);
    assert_eq!(harness.scene().document().to_toml().unwrap(), before);
}

#[test]
fn start_screen_opens_an_existing_recent_project_and_skips_missing_ones() {
    let mut harness = Harness::new();
    harness.state.scene = None;
    let demo = std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../examples/demo-game"
    ));
    let missing = std::path::PathBuf::from("this/project/does/not/exist");
    harness.state.recent_projects = vec![missing, demo.clone()];
    harness.panel(Vec::new(), show_welcome);
    // Строки списка находятся просмотром центра экрана сверху вниз.
    for y in (240..720).step_by(4) {
        harness.click(pos2(640.0, y as f32), show_welcome);
        // Выше списка находится кнопка Open Project.
        harness
            .state
            .actions
            .retain(|action| *action != Action::OpenProject);
        if !harness.state.actions.is_empty() {
            break;
        }
    }
    assert_eq!(harness.state.actions, [Action::OpenProjectAt(demo.clone())]);

    // Кнопка Remove Missing — последняя строка списка.
    harness.state.actions.clear();
    for y in (240..720).step_by(4).rev() {
        harness.click(pos2(640.0, y as f32), show_welcome);
        if harness.state.recent_projects.len() == 1 {
            break;
        }
    }
    assert_eq!(harness.state.recent_projects, [demo]);
    assert!(harness.state.actions.is_empty());
}

#[test]
fn recovery_prompt_offers_restore_and_blocks_escape() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    harness.state.recovery_prompt = Some(crate::state::RecoveryPrompt {
        scene: None,
        contents: String::new(),
    });
    harness.press(egui::Key::Escape, Modifiers::NONE);
    assert_eq!(harness.scene().selection(), Some("camera"));

    let mut dock = default_layout();
    let mut frame = |harness: &mut Harness, events: Vec<Event>| {
        let input = harness.input(events);
        let state = &mut harness.state;
        let mut output = harness.ctx.run_ui(input, |ui| show(ui, state, &mut dock));
        output.textures_delta.clear();
    };
    frame(&mut harness, Vec::new());
    // Кнопки окна находятся просмотром середины экрана; Restore стоит левее Discard.
    'search: for y in (330..440).step_by(6) {
        for x in (480..800).step_by(16) {
            let at = pos2(x as f32, y as f32);
            frame(&mut harness, vec![Event::PointerMoved(at)]);
            frame(&mut harness, vec![button(at, PointerButton::Primary, true)]);
            frame(
                &mut harness,
                vec![button(at, PointerButton::Primary, false)],
            );
            if !harness.state.actions.is_empty() {
                break 'search;
            }
        }
    }
    assert_eq!(harness.state.actions, [Action::RestoreAutosave]);
}

#[test]
fn status_bar_summarises_the_scene_and_play_frame_rate() {
    let mut harness = Harness::new();
    assert_eq!(scene_status(&harness.state, 0.02), "untitled  |  2 objects");
    harness
        .scene_mut()
        .create_object(&NewObject::Empty, None)
        .unwrap();
    let document = harness.scene().document().clone();
    harness.state.play =
        Some(crate::play::PlaySession::start(&document, None, |_| Ok(String::new())).unwrap());
    assert_eq!(
        scene_status(&harness.state, 0.02),
        "untitled  |  * unsaved  |  3 objects  |  50 FPS"
    );
    harness.state.play.as_mut().unwrap().set_paused(true);
    assert!(scene_status(&harness.state, 0.02).ends_with("Paused"));
    harness.state.scene = None;
    assert_eq!(scene_status(&harness.state, 0.02), "");
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
fn copy_puts_objects_on_the_os_clipboard_and_paste_inserts_them() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    let input = harness.input(vec![Event::Copy]);
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        toolbar::handle_shortcuts(ui.ctx(), state);
    });
    output.textures_delta.clear();
    let copied = output
        .platform_output
        .commands
        .iter()
        .find_map(|command| match command {
            egui::OutputCommand::CopyText(text) => Some(text.clone()),
            _ => None,
        })
        .expect("Copy кладёт текст в буфер ОС");

    harness.shortcuts(vec![Event::Paste(copied.clone())], Modifiers::NONE);
    let scene = harness.scene();
    assert_eq!(scene.document().objects.len(), 3);
    let pasted = scene.selection().unwrap();
    assert_ne!(pasted, "camera");
    assert!(scene.object(pasted).unwrap().camera.is_some());

    // Чужой текст и вставка в текстовое поле объектов не создают.
    harness.shortcuts(vec![Event::Paste("hello".to_owned())], Modifiers::NONE);
    assert_eq!(harness.scene().document().objects.len(), 3);
    assert_eq!(harness.state.log.entries().count(), 0);
    let text_id = egui::Id::new("focused_text");
    let mut text = String::new();
    let input = harness.input(Vec::new());
    let mut output = harness.ctx.run_ui(input, |ui| {
        ui.add(egui::TextEdit::singleline(&mut text).id(text_id));
        ui.ctx().memory_mut(|memory| memory.request_focus(text_id));
    });
    output.textures_delta.clear();
    let input = harness.input(vec![Event::Paste(copied)]);
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        ui.add(egui::TextEdit::singleline(&mut text).id(text_id));
        toolbar::handle_shortcuts(ui.ctx(), state);
    });
    output.textures_delta.clear();
    assert_eq!(harness.scene().document().objects.len(), 3);
}

#[test]
fn escape_that_closes_a_text_field_keeps_the_selection() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    let text_id = egui::Id::new("search");
    let mut text = String::new();
    let mut frame = |harness: &mut Harness, events: Vec<Event>, focus: bool| {
        let input = harness.input(events);
        let state = &mut harness.state;
        let mut output = harness.ctx.run_ui(input, |ui| {
            toolbar::handle_shortcuts(ui.ctx(), state);
            ui.add(egui::TextEdit::singleline(&mut text).id(text_id));
            if focus {
                ui.ctx().memory_mut(|memory| memory.request_focus(text_id));
            }
            // Как в конце `ui::show`.
            state.text_focused_last_frame = ui.ctx().egui_wants_keyboard_input();
        });
        output.textures_delta.clear();
    };
    frame(&mut harness, Vec::new(), true);
    frame(&mut harness, Vec::new(), false);
    // egui снимает фокус по Esc до построения UI этого кадра.
    frame(&mut harness, vec![key(egui::Key::Escape)], false);
    assert_eq!(harness.scene().selection(), Some("camera"));
    // Следующий Esc вне поля снимает выделение.
    frame(&mut harness, vec![key(egui::Key::Escape)], false);
    assert_eq!(harness.scene().selection(), None);
}

#[test]
fn shortcuts_do_not_edit_the_scene_behind_a_modal() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    harness.state.recovery_prompt = Some(crate::state::RecoveryPrompt {
        scene: None,
        contents: String::new(),
    });
    harness.press(egui::Key::D, Modifiers::COMMAND);
    harness.press(egui::Key::Delete, Modifiers::NONE);
    assert_eq!(harness.scene().document().objects.len(), 2);
    assert!(!harness.scene().is_dirty());
}

#[test]
fn losing_the_selection_mid_drag_starts_a_new_history_entry() {
    let (mut harness, id) = harness_with_cube();
    harness.scene_mut().select(Some(id.as_str()));
    harness.panel(Vec::new(), scene_view::show);
    let grab = y_handle_grab(&harness);
    harness.panel(vec![Event::PointerMoved(grab)], scene_view::show);
    harness.panel(
        vec![button(grab, PointerButton::Primary, true)],
        scene_view::show,
    );
    for step in 1..=3 {
        let at = grab - vec2(0.0, 20.0 * step as f32);
        harness.panel(vec![Event::PointerMoved(at)], scene_view::show);
    }
    assert!(harness.state.gizmo_drag.is_some());
    let dragged = harness.scene().object(&id).unwrap().transform;
    harness.scene_mut().select(None);
    harness.panel(Vec::new(), scene_view::show);
    assert!(harness.state.gizmo_drag.is_none());

    let key = format!("{id}/gizmo");
    let moved = engine_scene::SceneTransform {
        position: [9.0, 9.0, 9.0],
        ..dragged
    };
    harness
        .scene_mut()
        .set_transforms(&[(id.clone(), moved)], Some(&key))
        .unwrap();
    assert!(harness.scene_mut().undo());
    assert_eq!(
        harness.scene().object(&id).unwrap().transform.position,
        dragged.position
    );
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

#[test]
fn scene_and_game_views_keep_independent_debug_modes() {
    let mut harness = Harness::new();
    harness.state.scene_view.debug_view = engine_render::DebugView::Normals;
    harness.state.game_view.debug_view = engine_render::DebugView::ShadowCascades;

    harness.panel(Vec::new(), scene_view::show);
    harness.panel(Vec::new(), game_view::show);

    assert_eq!(
        harness.state.scene_view.debug_view,
        engine_render::DebugView::Normals
    );
    assert_eq!(
        harness.state.game_view.debug_view,
        engine_render::DebugView::ShadowCascades
    );
}

/// Центр первой надписи `text` среди фигур кадра.
fn find_text(shapes: &[egui::epaint::ClippedShape], text: &str) -> Option<Pos2> {
    fn visit(shape: &egui::Shape, text: &str) -> Option<Pos2> {
        match shape {
            egui::Shape::Text(shape) if shape.galley.text() == text => {
                Some(shape.pos + shape.galley.rect.center().to_vec2())
            }
            egui::Shape::Vec(shapes) => shapes.iter().find_map(|shape| visit(shape, text)),
            _ => None,
        }
    }
    shapes
        .iter()
        .find_map(|clipped| visit(&clipped.shape, text))
}

#[test]
fn build_player_menu_item_queues_the_action() {
    let mut harness = Harness::new();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
    harness.state.project = Some(OpenProject::open(Path::new(dir)).unwrap());
    let mut dock = default_layout();
    let mut frame = |harness: &mut Harness, events: Vec<Event>| {
        let input = harness.input(events);
        let state = &mut harness.state;
        let mut output = harness
            .ctx
            .run_ui(input, |ui| toolbar::show(ui, state, &mut dock));
        output.textures_delta.clear();
        output.shapes
    };
    let mut click = |harness: &mut Harness, text: &str| {
        let shapes = frame(harness, Vec::new());
        let at = find_text(&shapes, text).unwrap_or_else(|| panic!("{text} not shown"));
        frame(harness, vec![Event::PointerMoved(at)]);
        frame(harness, vec![button(at, PointerButton::Primary, true)]);
        frame(harness, vec![button(at, PointerButton::Primary, false)]);
    };
    click(&mut harness, "File");
    click(&mut harness, "Build Player…");
    assert_eq!(harness.state.actions, [crate::state::Action::BuildPlayer]);
}

#[test]
fn new_project_menu_item_queues_the_action() {
    let mut harness = Harness::new();
    let mut dock = default_layout();
    let mut frame = |harness: &mut Harness, events: Vec<Event>| {
        let input = harness.input(events);
        let state = &mut harness.state;
        let mut output = harness
            .ctx
            .run_ui(input, |ui| toolbar::show(ui, state, &mut dock));
        output.textures_delta.clear();
        output.shapes
    };
    let mut click = |harness: &mut Harness, text: &str| {
        let shapes = frame(harness, Vec::new());
        let at = find_text(&shapes, text).unwrap_or_else(|| panic!("{text} not shown"));
        frame(harness, vec![Event::PointerMoved(at)]);
        frame(harness, vec![button(at, PointerButton::Primary, true)]);
        frame(harness, vec![button(at, PointerButton::Primary, false)]);
    };
    click(&mut harness, "File");
    click(&mut harness, "New Project…");
    assert_eq!(harness.state.actions, [crate::state::Action::NewProject]);
}

#[test]
fn stop_button_cancels_a_play_request_waiting_for_script_build() {
    let mut harness = Harness::new();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
    harness.state.project = Some(OpenProject::open(Path::new(dir)).unwrap());
    harness.state.pending_play = true;
    let mut dock = default_layout();
    let mut frame = |harness: &mut Harness, events: Vec<Event>| {
        let input = harness.input(events);
        let state = &mut harness.state;
        let mut output = harness
            .ctx
            .run_ui(input, |ui| toolbar::show(ui, state, &mut dock));
        output.textures_delta.clear();
        output.shapes
    };
    let shapes = frame(&mut harness, Vec::new());
    let at = find_text(&shapes, "Stop").unwrap_or_else(|| panic!("Stop not shown"));
    frame(&mut harness, vec![Event::PointerMoved(at)]);
    frame(&mut harness, vec![button(at, PointerButton::Primary, true)]);
    frame(
        &mut harness,
        vec![button(at, PointerButton::Primary, false)],
    );
    assert_eq!(harness.state.actions, [crate::state::Action::Stop]);
}

/// Все надписи кадра, для поиска виджетов в тестах.
fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, Pos2)> {
    fn visit(shape: &egui::Shape, out: &mut Vec<(String, Pos2)>) {
        match shape {
            egui::Shape::Text(shape) => out.push((
                shape.galley.text().to_owned(),
                shape.pos + shape.galley.rect.center().to_vec2(),
            )),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|shape| visit(shape, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    for clipped in shapes {
        visit(&clipped.shape, &mut out);
    }
    out
}

const SPEED_SCRIPT: &str = "scripts/rotate.lua";

fn speed_schema() -> engine_scene::ParamSchema {
    engine_scene::ParamSchema::new(vec![engine_scene::ParamSpec {
        name: "speed".to_owned(),
        kind: engine_scene::ParamKind::Float,
        default: engine_scene::ParamValue::Float(30.0),
        min: Some(0.0),
        max: Some(360.0),
        label: Some("Speed".to_owned()),
    }])
    .unwrap()
}

/// Камера с lite-скриптом выбрана; схема скрипта уже в кэше.
fn harness_with_script(schema: Option<engine_scene::ParamSchema>) -> Harness {
    let mut harness = Harness::new();
    let path = VirtualPath::parse(SPEED_SCRIPT).unwrap();
    let scene = harness.scene_mut();
    scene.add_lite_script("camera", path).unwrap();
    scene.select(Some("camera"));
    harness
        .state
        .lite_schemas
        .insert(SPEED_SCRIPT.to_owned(), Ok(schema));
    harness
}

fn inspector_frame(harness: &mut Harness, events: Vec<Event>) -> Vec<(String, Pos2)> {
    let input = harness.input(events);
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        egui::CentralPanel::default().show(ui, |ui| inspector::show(ui, state));
    });
    output.textures_delta.clear();
    texts(&output.shapes)
}

fn script_params(harness: &Harness) -> BTreeMap<String, engine_scene::ParamValue> {
    harness.scene().object("camera").unwrap().lite_scripts[0]
        .params
        .clone()
}

fn position_of(texts: &[(String, Pos2)], text: &str) -> Pos2 {
    texts
        .iter()
        .find(|(found, _)| found == text)
        .map(|(_, at)| *at)
        .unwrap_or_else(|| {
            panic!(
                "{text:?} not in {:?}",
                texts.iter().map(|t| &t.0).collect::<Vec<_>>()
            )
        })
}

fn inspector_click(harness: &mut Harness, at: Pos2) {
    inspector_frame(harness, vec![Event::PointerMoved(at)]);
    inspector_frame(harness, vec![button(at, PointerButton::Primary, true)]);
    inspector_frame(harness, vec![button(at, PointerButton::Primary, false)]);
    inspector_frame(harness, Vec::new());
}

#[test]
fn schema_field_shows_the_default_and_writes_the_key_only_after_an_edit() {
    let mut harness = harness_with_script(Some(speed_schema()));
    inspector_frame(&mut harness, Vec::new());
    let texts = inspector_frame(&mut harness, Vec::new());
    position_of(&texts, "Speed");
    let value = position_of(&texts, "30.00");
    assert!(script_params(&harness).is_empty());
    assert!(texts.iter().all(|(text, _)| text != "Add"));

    inspector_frame(&mut harness, vec![Event::PointerMoved(value)]);
    inspector_frame(
        &mut harness,
        vec![button(value, PointerButton::Primary, true)],
    );
    for step in 1..=4 {
        inspector_frame(
            &mut harness,
            vec![Event::PointerMoved(value + vec2(10.0 * step as f32, 0.0))],
        );
    }
    inspector_frame(
        &mut harness,
        vec![button(
            value + vec2(40.0, 0.0),
            PointerButton::Primary,
            false,
        )],
    );
    let params = script_params(&harness);
    let Some(engine_scene::ParamValue::Float(speed)) = params.get("speed") else {
        panic!("{params:?}");
    };
    assert!(*speed > 30.0 && *speed <= 360.0, "{speed}");
}

#[test]
fn reset_removes_the_key_and_returns_the_default() {
    let mut harness = harness_with_script(Some(speed_schema()));
    harness
        .scene_mut()
        .edit_object("camera", None, |object| {
            object.lite_scripts[0]
                .params
                .insert("speed".into(), engine_scene::ParamValue::Float(90.0));
            Ok(())
        })
        .unwrap();
    inspector_frame(&mut harness, Vec::new());
    let texts = inspector_frame(&mut harness, Vec::new());
    position_of(&texts, "90.00");
    inspector_click(&mut harness, position_of(&texts, "↺"));
    assert!(script_params(&harness).is_empty());
    assert!(harness.scene_mut().undo());
    assert_eq!(script_params(&harness).len(), 1);
}

#[test]
fn reset_uses_schema_default_when_prefab_base_has_the_parameter() {
    let mut harness = Harness::new();
    let prefab_path = VirtualPath::parse("/prefabs/params.rveprefab").unwrap();
    let prefab = "[[assets]]\npath = \"/scripts/rotate.lua\"\n[[objects]]\nid = \"root\"\n[[objects.lite_scripts]]\npath = \"/scripts/rotate.lua\"\nparams = { speed = 90.0 }\n".to_owned();
    harness
        .scene_mut()
        .set_prefab_loader(std::rc::Rc::new(move |path| {
            engine_scene::SceneDocument::parse_prefab(format!("/{}", path.as_str()), &prefab)
        }))
        .unwrap();
    harness
        .scene_mut()
        .create_object(&NewObject::Prefab(prefab_path, [0.0; 3]), None)
        .unwrap();
    harness
        .state
        .lite_schemas
        .insert(SPEED_SCRIPT.to_owned(), Ok(Some(speed_schema())));

    inspector_frame(&mut harness, Vec::new());
    let texts = inspector_frame(&mut harness, Vec::new());
    position_of(&texts, "90.00");
    inspector_click(&mut harness, position_of(&texts, "↺"));

    let params = &harness.scene().object("params").unwrap().lite_scripts[0].params;
    assert_eq!(params["speed"], engine_scene::ParamValue::Float(30.0));
    let change = harness.scene().object_override("params").unwrap();
    assert_eq!(
        change.lite_params[0].params["speed"],
        engine_scene::ParamValue::Float(30.0)
    );
}

#[test]
fn undeclared_keys_are_marked_and_can_be_removed() {
    let mut harness = harness_with_script(Some(speed_schema()));
    harness
        .scene_mut()
        .edit_object("camera", None, |object| {
            object.lite_scripts[0]
                .params
                .insert("spin".into(), engine_scene::ParamValue::Int(1));
            Ok(())
        })
        .unwrap();
    inspector_frame(&mut harness, Vec::new());
    let texts = inspector_frame(&mut harness, Vec::new());
    let marker = position_of(&texts, "⚠ spin");
    let remove = texts
        .iter()
        .filter(|(text, at)| text == "x" && (at.y - marker.y).abs() < 4.0)
        .map(|(_, at)| *at)
        .next()
        .expect("remove button on the marked row");
    inspector_click(&mut harness, remove);
    assert!(script_params(&harness).is_empty());
}

#[test]
fn without_a_schema_the_free_form_editor_stays() {
    let mut harness = harness_with_script(None);
    inspector_frame(&mut harness, Vec::new());
    let texts = inspector_frame(&mut harness, Vec::new());
    position_of(&texts, "Add");
    assert!(texts.iter().all(|(text, _)| text != "Speed"));
}

fn typed_library() -> engine_script::ScriptLibrary {
    struct Empty;
    impl rve_sdk::Script for Empty {}
    fn factory(_: &rve_sdk::Params) -> Result<Box<dyn rve_sdk::Script>, String> {
        Ok(Box::new(Empty))
    }
    let module = rve_sdk::script_module! {
        "Bobber" => factory { speed: f32 = 1.4 (0.0..=10.0) "Bob speed" },
    };
    // SAFETY: the SDK macro returns an immutable process-lifetime table.
    unsafe { engine_script::ScriptLibrary::from_module(module) }.unwrap()
}

#[test]
fn rust_script_parameters_use_the_library_schema() {
    let mut harness = Harness::new();
    harness.state.library = Some(typed_library());
    let scene = harness.scene_mut();
    scene
        .edit_object("camera", None, |object| {
            object.heavy_scripts.push(engine_scene::HeavyScript {
                name: "Bobber".to_owned(),
                params: BTreeMap::new(),
            });
            Ok(())
        })
        .unwrap();
    scene.select(Some("camera"));
    inspector_frame(&mut harness, Vec::new());
    let texts = inspector_frame(&mut harness, Vec::new());
    position_of(&texts, "Bob speed");
    position_of(&texts, "1.40");
}

fn hierarchy_frame(harness: &mut Harness, events: Vec<Event>) -> Vec<(String, Pos2)> {
    let input = harness.input(events);
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        egui::CentralPanel::default().show(ui, |ui| hierarchy::show(ui, state));
    });
    output.textures_delta.clear();
    texts(&output.shapes)
}

#[test]
fn hierarchy_marks_prefab_instances_and_does_not_drag_their_objects() {
    let mut harness = Harness::new();
    let prefab = "[[objects]]\nid = \"frame\"\n[[objects]]\nid = \"handle\"\nparent = \"frame\"\n";
    let loader: crate::scene_edit::PrefabLoader = std::rc::Rc::new(move |path| {
        engine_scene::SceneDocument::parse_prefab(format!("/{}", path.as_str()), prefab)
    });
    let scene = harness.scene_mut();
    scene.set_prefab_loader(loader).unwrap();
    scene
        .create_object(
            &NewObject::Prefab(
                VirtualPath::parse("/prefabs/door.rveprefab").unwrap(),
                [0.0; 3],
            ),
            None,
        )
        .unwrap();
    hierarchy_frame(&mut harness, Vec::new());
    let texts = hierarchy_frame(&mut harness, Vec::new());
    position_of(&texts, "📦 door");
    let handle = position_of(&texts, "handle");
    let camera = position_of(&texts, "Camera");
    let revision = harness.scene().revision();

    hierarchy_frame(&mut harness, vec![Event::PointerMoved(handle)]);
    hierarchy_frame(
        &mut harness,
        vec![button(handle, PointerButton::Primary, true)],
    );
    for step in 1..=4 {
        let at = handle + (camera - handle) * (step as f32 / 4.0);
        hierarchy_frame(&mut harness, vec![Event::PointerMoved(at)]);
    }
    hierarchy_frame(
        &mut harness,
        vec![button(camera, PointerButton::Primary, false)],
    );
    hierarchy_frame(&mut harness, Vec::new());
    assert_eq!(harness.scene().revision(), revision);
    assert_eq!(
        harness
            .scene()
            .object("door.handle")
            .unwrap()
            .parent
            .as_deref(),
        Some("door")
    );
}

#[test]
fn scene_scroll_modes_and_native_gestures() {
    use crate::gestures::ScrollNavigation as Mode;
    use egui::MouseWheelUnit as Unit;
    for (mode, unit, orbit) in [
        (Mode::Auto, Unit::Point, true),
        (Mode::Auto, Unit::Line, false),
        (Mode::Mouse, Unit::Point, false),
        (Mode::Trackpad, Unit::Line, true),
    ] {
        let mut h = Harness::new();
        h.state.scroll_navigation = mode;
        h.panel(
            vec![Event::PointerMoved(pos2(640.0, 360.0))],
            scene_view::show,
        );
        let before = h.state.camera;
        h.panel(
            vec![Event::MouseWheel {
                phase: egui::TouchPhase::Move,
                unit,
                delta: vec2(20.0, 30.0),
                modifiers: Modifiers::NONE,
            }],
            scene_view::show,
        );
        if orbit {
            assert_ne!(h.state.camera.yaw, before.yaw);
            assert_eq!(h.state.camera.distance, before.distance);
        } else {
            assert_ne!(h.state.camera.distance, before.distance);
            assert_eq!(h.state.camera.yaw, before.yaw);
        }
        assert!(!h.scene().is_dirty());
    }
    let mut h = Harness::new();
    h.panel(
        vec![Event::PointerMoved(pos2(640.0, 360.0))],
        scene_view::show,
    );
    let before = h.state.camera;
    h.panel(
        vec![Event::MouseWheel {
            phase: egui::TouchPhase::Move,
            unit: Unit::Point,
            delta: vec2(20.0, 30.0),
            modifiers: Modifiers::SHIFT,
        }],
        scene_view::show,
    );
    assert_ne!(h.state.camera.target, before.target);
    assert_eq!(h.state.camera.yaw, before.yaw);
    h.panel(vec![Event::Zoom(2.0), Event::Rotate(0.3)], scene_view::show);
    assert_eq!(h.state.camera.distance, before.distance / 2.0);
    assert!((h.state.camera.yaw - (before.yaw - 0.3)).abs() < 1e-5);
}

#[test]
fn scene_navigation_menu_changes_mode() {
    let mut harness = Harness::new();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
    harness.state.project = Some(OpenProject::open(Path::new(dir)).unwrap());
    let mut dock = default_layout();
    let mut frame = |harness: &mut Harness, events: Vec<Event>| {
        let input = harness.input(events);
        let state = &mut harness.state;
        let mut output = harness
            .ctx
            .run_ui(input, |ui| toolbar::show(ui, state, &mut dock));
        output.textures_delta.clear();
        output.shapes
    };
    let mut click = |harness: &mut Harness, text: &str| {
        let shapes = frame(harness, Vec::new());
        let at = find_text(&shapes, text).unwrap_or_else(|| panic!("{text} not shown"));
        frame(harness, vec![Event::PointerMoved(at)]);
        frame(harness, vec![button(at, PointerButton::Primary, true)]);
        frame(harness, vec![button(at, PointerButton::Primary, false)]);
    };
    click(&mut harness, "Edit");
    click(&mut harness, "Scene Navigation");
    click(&mut harness, "Trackpad (orbit/pan)");
    assert_eq!(
        harness.state.scroll_navigation,
        crate::gestures::ScrollNavigation::Trackpad
    );
}

#[test]
fn alt_command_drag_pans_without_orbit_or_document_edits() {
    let mut h = Harness::new();
    let start = pos2(200.0, 200.0);
    let end = start + vec2(80.0, 35.0);
    let mods = Modifiers {
        alt: true,
        command: true,
        ..Modifiers::NONE
    };
    let before = h.state.camera;
    h.panel(vec![Event::PointerMoved(start)], scene_view::show);
    let mut press = button(start, PointerButton::Primary, true);
    if let Event::PointerButton { modifiers, .. } = &mut press {
        *modifiers = mods;
    }
    h.panel(vec![Event::ModifiersChanged(mods), press], scene_view::show);
    h.panel(vec![Event::PointerMoved(end)], scene_view::show);
    h.panel(
        vec![button(end, PointerButton::Primary, false)],
        scene_view::show,
    );
    assert_eq!(h.state.camera.yaw, before.yaw);
    assert_ne!(h.state.camera.target, before.target);
    assert!(!h.scene().is_dirty());
}

#[test]
fn smart_zoom_frames_only_edit_scene_under_pointer_and_resets_when_hidden() {
    let (mut h, id) = harness_with_cube();
    h.scene_mut().select(Some(&id));
    h.state.smart_zoom = true;
    h.panel(
        vec![Event::PointerMoved(pos2(640.0, 360.0))],
        scene_view::show,
    );
    assert!((h.state.camera.target - CUBE_POSITION).length() < 1e-5);
    let document = h.scene().document().clone();
    h.state.play =
        Some(crate::play::PlaySession::start(&document, None, |_| Ok(String::new())).unwrap());
    h.state.camera.target = Vec3::splat(99.0);
    let before = h.state.camera;
    h.panel(Vec::new(), scene_view::show);
    assert_eq!(h.state.camera, before);
    h.state.play = None;
    // Full UI without a project has no Scene panel, but still consumes the flag.
    let mut dock = default_layout();
    let input = h.input(Vec::new());
    let mut output = h.ctx.run_ui(input, |ui| show(ui, &mut h.state, &mut dock));
    output.textures_delta.clear();
    assert!(!h.state.smart_zoom);
}

#[test]
fn project_tile_label_is_centered_under_its_tile() {
    let mut harness = Harness::new();
    harness.state.asset_tree = vec![crate::assets_fs::AssetEntry {
        name: "notes.txt".to_owned(),
        path: "notes.txt".to_owned(),
        is_dir: false,
        children: Vec::new(),
    }];
    let input = harness.input(Vec::new());
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        egui::CentralPanel::default().show(ui, |ui| project_panel::show(ui, state));
    });
    output.textures_delta.clear();
    // Заглушка типа рисуется по центру плитки, как и подпись под ней.
    let texts = texts(&output.shapes);
    let placeholder = position_of(&texts, "FILE");
    let label = position_of(&texts, "notes.txt");
    assert!(
        (placeholder.x - label.x).abs() < 0.5,
        "{label:?} не по центру плитки {placeholder:?}"
    );
}

#[test]
fn clicking_elsewhere_commits_the_hierarchy_rename_once() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    harness.press(egui::Key::F2, Modifiers::NONE);
    harness.panel(Vec::new(), hierarchy::show);
    harness.panel(vec![Event::Text("Z".to_owned())], hierarchy::show);
    assert_eq!(
        harness.state.hierarchy_rename,
        Some(("camera".to_owned(), "CameraZ".to_owned()))
    );
    // Щелчок мимо строк уводит фокус из поля: переименование завершается.
    harness.click(pos2(600.0, 600.0), hierarchy::show);
    assert_eq!(harness.state.hierarchy_rename, None);
    assert_eq!(
        harness.scene().object("camera").unwrap().name.as_deref(),
        Some("CameraZ")
    );
}

#[test]
fn escape_cancels_the_hierarchy_rename() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    let name = harness.scene().object("camera").unwrap().name.clone();
    harness.press(egui::Key::F2, Modifiers::NONE);
    harness.panel(Vec::new(), hierarchy::show);
    harness.panel(vec![Event::Text("Z".to_owned())], hierarchy::show);
    harness.panel(vec![key(egui::Key::Escape)], hierarchy::show);
    assert_eq!(harness.state.hierarchy_rename, None);
    assert_eq!(harness.scene().object("camera").unwrap().name, name);
}

#[test]
fn starting_play_cancels_the_hierarchy_rename_and_blocks_new_ones() {
    let mut harness = Harness::new();
    harness.scene_mut().select(Some("camera"));
    harness.press(egui::Key::F2, Modifiers::NONE);
    harness.panel(Vec::new(), hierarchy::show);
    let document = harness.scene().document().clone();
    let session = crate::play::PlaySession::start(&document, None, |_| Ok(String::new())).unwrap();
    harness.state.enter_play(session);
    assert_eq!(harness.state.hierarchy_rename, None);
    harness.press(egui::Key::F2, Modifiers::NONE);
    assert_eq!(harness.state.hierarchy_rename, None);
}

#[test]
fn double_clicking_a_hierarchy_row_selects_and_frames_it_without_renaming() {
    let (mut harness, id) = harness_with_cube();
    let label = harness
        .scene()
        .object(&id)
        .unwrap()
        .display_name()
        .to_owned();
    let texts = hierarchy_frame(&mut harness, Vec::new());
    let at = position_of(&texts, &label);
    hierarchy_frame(&mut harness, vec![Event::PointerMoved(at)]);
    // Два щелчка подряд в пределах двойного клика egui.
    for pressed in [true, false, true, false] {
        hierarchy_frame(
            &mut harness,
            vec![button(at, PointerButton::Primary, pressed)],
        );
    }
    hierarchy_frame(&mut harness, Vec::new());
    assert_eq!(harness.state.hierarchy_rename, None);
    assert_eq!(harness.scene().selection(), Some(id.as_str()));
    assert!((harness.state.camera.target - CUBE_POSITION).length() < 1e-4);
}

#[test]
fn context_menu_rename_starts_renaming_the_row() {
    let mut harness = Harness::new();
    hierarchy_frame(&mut harness, Vec::new());
    let texts = hierarchy_frame(&mut harness, Vec::new());
    let row = position_of(&texts, "Camera");
    hierarchy_frame(&mut harness, vec![Event::PointerMoved(row)]);
    hierarchy_frame(
        &mut harness,
        vec![button(row, PointerButton::Secondary, true)],
    );
    hierarchy_frame(
        &mut harness,
        vec![button(row, PointerButton::Secondary, false)],
    );
    let texts = hierarchy_frame(&mut harness, Vec::new());
    let rename = position_of(&texts, "Rename");
    hierarchy_frame(&mut harness, vec![Event::PointerMoved(rename)]);
    hierarchy_frame(
        &mut harness,
        vec![button(rename, PointerButton::Primary, true)],
    );
    hierarchy_frame(
        &mut harness,
        vec![button(rename, PointerButton::Primary, false)],
    );
    hierarchy_frame(&mut harness, Vec::new());
    assert_eq!(
        harness.state.hierarchy_rename,
        Some(("camera".to_owned(), "Camera".to_owned()))
    );
}

#[test]
fn folder_tree_labels_render_without_an_icon_cache() {
    let mut harness = Harness::new();
    let folder =
        |name: &str, children: Vec<crate::assets_fs::AssetEntry>| crate::assets_fs::AssetEntry {
            name: name.to_owned(),
            path: name.to_owned(),
            is_dir: true,
            children,
        };
    harness.state.asset_tree = vec![
        folder("Models", vec![folder("Textures", Vec::new())]),
        folder("Empty", Vec::new()),
    ];
    let input = harness.input(Vec::new());
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        egui::CentralPanel::default().show(ui, |ui| project_panel::show(ui, state));
    });
    output.textures_delta.clear();
    let texts = texts(&output.shapes);
    for label in ["Assets", "Models", "Textures", "Empty"] {
        position_of(&texts, label);
    }
}

/// Кадр панели Scene Assets. Без рамки, как `Harness::panel`: иначе координаты надписей не
/// совпадут с местом, куда `Harness::click` доставляет щелчок.
fn scene_assets_frame(harness: &mut Harness, events: Vec<Event>) -> Vec<(String, Pos2)> {
    let input = harness.input(events);
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE)
            .show(ui, |ui| scene_assets::show(ui, state));
    });
    output.textures_delta.clear();
    texts(&output.shapes)
}

fn scene_prefab_paths(harness: &Harness) -> Vec<String> {
    harness
        .scene()
        .document()
        .assets
        .iter()
        .map(|asset| asset.path.as_str().to_owned())
        .collect()
}

#[test]
fn scene_assets_remove_only_prefabs_without_an_instance() {
    let mut harness = Harness::new();
    let prefab = "[[objects]]\nid = \"frame\"\n";
    let loader: crate::scene_edit::PrefabLoader = std::rc::Rc::new(move |path| {
        engine_scene::SceneDocument::parse_prefab(format!("/{}", path.as_str()), prefab)
    });
    harness.scene_mut().set_prefab_loader(loader).unwrap();
    harness
        .scene_mut()
        .create_object(
            &NewObject::Prefab(
                VirtualPath::parse("/prefabs/door.rveprefab").unwrap(),
                [0.0; 3],
            ),
            None,
        )
        .unwrap();
    harness
        .scene_mut()
        .add_scene_prefab(VirtualPath::parse("/prefabs/bullet.rveprefab").unwrap())
        .unwrap();

    let texts = scene_assets_frame(&mut harness, Vec::new());
    position_of(&texts, "/prefabs/door.rveprefab");
    position_of(&texts, "используется экземпляром");
    position_of(&texts, "не используется");
    // Строки идут в порядке ресурсов сцены: сначала дверь (есть экземпляр), затем пуля.
    let removes: Vec<Pos2> = texts
        .iter()
        .filter(|(text, _)| text == "Remove")
        .map(|(_, at)| *at)
        .collect();
    assert_eq!(removes.len(), 2);

    harness.click(removes[0], scene_assets::show);
    assert_eq!(
        scene_prefab_paths(&harness),
        ["prefabs/door.rveprefab", "prefabs/bullet.rveprefab"]
    );
    harness.click(removes[1], scene_assets::show);
    assert_eq!(scene_prefab_paths(&harness), ["prefabs/door.rveprefab"]);
    assert!(harness.scene().can_undo());
}

#[test]
fn dropping_assets_on_scene_assets_adds_only_prefabs() {
    let mut harness = Harness::new();
    let texts = scene_assets_frame(&mut harness, Vec::new());
    let zone = position_of(&texts, "Перетащите сюда .rveprefab из Project");
    scene_assets_frame(&mut harness, vec![Event::PointerMoved(zone)]);

    egui::DragAndDrop::set_payload(&harness.ctx, AssetDrag("models/a.fbx".to_owned()));
    scene_assets_frame(
        &mut harness,
        vec![button(zone, PointerButton::Primary, false)],
    );
    assert!(scene_prefab_paths(&harness).is_empty());
    assert_eq!(harness.state.log.entries().count(), 1);

    egui::DragAndDrop::set_payload(
        &harness.ctx,
        AssetDrag("prefabs/bullet.rveprefab".to_owned()),
    );
    scene_assets_frame(
        &mut harness,
        vec![button(zone, PointerButton::Primary, false)],
    );
    assert_eq!(scene_prefab_paths(&harness), ["prefabs/bullet.rveprefab"]);
}

#[test]
fn project_prefabs_panel_loads_the_project_list_on_first_show() {
    let mut harness = Harness::new();
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../examples/demo-game");
    harness.state.project = Some(OpenProject::open(Path::new(dir)).unwrap());
    let input = harness.input(Vec::new());
    let state = &mut harness.state;
    let mut output = harness.ctx.run_ui(input, |ui| {
        egui::CentralPanel::default().show(ui, |ui| project_prefabs::show(ui, state));
    });
    output.textures_delta.clear();
    let texts = texts(&output.shapes);
    position_of(&texts, "Add Prefab");
    position_of(&texts, "Save");
    position_of(&texts, "Reload");
    let project_root = harness.state.project.as_ref().unwrap().root.clone();
    assert_eq!(
        harness.state.project_prefabs.as_ref().unwrap().root(),
        project_root
    );
    // Пример объявляет один тяжёлый префаб в prefabs.rveprefabs; панель должна показать его.
    let entries = harness.state.project_prefabs.as_ref().unwrap().entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "/prefabs/tower.rveprefab");
}
