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
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: Modifiers::NONE,
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
