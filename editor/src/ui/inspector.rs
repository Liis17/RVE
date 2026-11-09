//! Панель Inspector: поля и компоненты выбранного объекта.

use crate::assets_fs::AssetKind;
use crate::icons;
use crate::scene_edit::{
    Component, ComponentKind, OverrideField, default_environment, default_light,
};
use crate::state::{Action, AssetDrag, EditorState, ObjectDrag};
use engine_files::VirtualPath;
use engine_scene::ComponentKind as ScriptComponentKind;
use engine_scene::document::MaterialOverride;
use engine_scene::{
    AntiAliasing, AudioGroup, AudioSourceComponent, CameraComponent, CharacterControllerComponent,
    ColliderComponent, ColliderShape, EnvironmentComponent, HeavyScript, LAYER_COUNT, LodGroup,
    LodLevel, MAX_LOD_LEVELS, MaterialAlphaMode, NetAuthority, NetworkIdentityComponent,
    NetworkTransformComponent, ParamKind, ParamSchema, ParamSpec, ParamValue, PointLightComponent,
    ProceduralSkyComponent, RigidBodyComponent, RigidBodyKind, SceneObject, SceneTransform,
    SpotLightComponent,
};
use std::collections::BTreeMap;

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    if let Some(path) = state.inspected_asset.clone() {
        super::asset_inspector::show(ui, state, &path);
        return;
    }
    let Some(object) = state
        .scene
        .as_ref()
        .and_then(|scene| scene.selected_object())
        .cloned()
    else {
        ui.label("Объект не выбран");
        return;
    };
    let enabled = !state.is_playing();
    let selected = state
        .scene
        .as_ref()
        .map_or(0, |scene| scene.selected_ids().len());
    if selected > 1 {
        ui.label(format!(
            "Выделено объектов: {selected}; показан активный {}",
            object.display_name()
        ));
        ui.separator();
    }
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.add_enabled_ui(enabled, |ui| show_object(ui, state, &object));
        });
}

/// Завершает непрерывную правку, когда пользователь отпустил значение.
fn finish_edit(state: &mut EditorState, response: &egui::Response) {
    if (response.drag_stopped() || response.lost_focus())
        && let Some(scene) = &mut state.scene
    {
        scene.end_continuous_edit();
    }
}

/// Применяет изменение копии компонента, если значение поменялось.
fn commit(
    state: &mut EditorState,
    id: &str,
    field: &str,
    response: &egui::Response,
    change: impl FnOnce(&mut SceneObject),
) {
    if response.changed() {
        let key = format!("{id}/{field}");
        state.edit_scene(|scene| {
            scene.edit_object(id, Some(&key), |object| {
                change(object);
                Ok(())
            })
        });
    }
    finish_edit(state, response);
}

fn commit_script_params(
    state: &mut EditorState,
    id: &str,
    field: &str,
    response: &egui::Response,
    index: usize,
    params: BTreeMap<String, ParamValue>,
    lite: bool,
) {
    if response.changed() {
        let key = format!("{id}/{field}");
        state.edit_scene(|scene| {
            if lite {
                scene.set_lite_script_params(id, index, params, Some(&key))
            } else {
                scene.set_heavy_script_params(id, index, params, Some(&key))
            }
        });
    }
    finish_edit(state, response);
}

fn show_object(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let id = object.id.as_str();
    show_prefab_bar(ui, state, id);
    let name_mark = mark(state, id, OverrideField::Name);
    let part = state
        .scene
        .as_ref()
        .is_some_and(|scene| scene.is_prefab_part(id));
    let mut name_reverted = false;
    egui::Grid::new("object_fields")
        .num_columns(2)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(marked_text("Name", name_mark.overridden));
                if name_mark.overridden {
                    name_reverted = ui.small_button("↺").on_hover_text("Revert").clicked();
                }
            });
            let mut name = object.name.clone().unwrap_or_default();
            let response = ui.text_edit_singleline(&mut name);
            commit(state, id, "name", &response, |object| {
                object.name = (!name.is_empty()).then_some(name);
            });
            ui.end_row();

            ui.label("Id");
            let editor_id = ui.make_persistent_id(("object_id_edit", id));
            let mut text = ui
                .ctx()
                .data(|data| data.get_temp::<String>(editor_id))
                .unwrap_or_else(|| id.to_owned());
            let response = ui.add_enabled(!part, egui::TextEdit::singleline(&mut text));
            if response.lost_focus() {
                ui.ctx().data_mut(|data| data.remove::<String>(editor_id));
                state.edit_scene(|scene| scene.change_id(id, &text));
            } else if response.has_focus() {
                ui.ctx().data_mut(|data| data.insert_temp(editor_id, text));
            }
            ui.end_row();

            ui.label("Active");
            ui.add_enabled_ui(!part, |ui| {
                let mut active = object.active;
                let response = ui.checkbox(&mut active, "");
                commit(state, id, "active", &response, |object| {
                    object.active = active;
                });
            });
            ui.end_row();

            ui.label("Tag");
            ui.add_enabled_ui(!part, |ui| tag_combo(ui, state, object));
            ui.end_row();

            ui.label("Layer");
            ui.add_enabled_ui(!part, |ui| layer_combo(ui, state, object));
            ui.end_row();
        });
    if name_reverted {
        revert(state, id, OverrideField::Name);
        return;
    }

    ui.separator();
    show_transform(ui, state, object);
    if let Some(camera) = &object.camera {
        show_camera(ui, state, id, camera);
    }
    if object.mesh_renderer.is_some() {
        show_mesh(ui, state, object);
    }
    if object.lod_group.is_some() {
        show_lod_group(ui, state, object);
    }
    if object.directional_light.is_some() {
        show_light(ui, state, object);
    }
    if let Some(light) = object.point_light {
        show_point_light(ui, state, id, light);
    }
    if let Some(light) = object.spot_light {
        show_spot_light(ui, state, id, light);
    }
    if let Some(audio) = &object.audio_source {
        show_audio_source(ui, state, id, audio);
    }
    if let Some(animator) = &object.animator {
        show_animator(ui, state, id, animator);
    }
    if let Some(emitter) = &object.particle_emitter {
        show_particle_emitter(ui, state, id, emitter);
    }
    if let Some(environment) = &object.environment {
        show_environment(ui, state, id, environment);
    }
    if let Some(collider) = object.collider.clone() {
        show_collider(ui, state, object, collider);
    }
    if let Some(body) = object.rigid_body {
        show_rigid_body(ui, state, id, body);
    }
    if let Some(controller) = object.character_controller {
        show_character_controller(ui, state, id, controller);
    }
    if let Some(identity) = object.network_identity {
        show_network_identity(ui, state, id, identity);
    }
    if let Some(transform) = object.network_transform {
        show_network_transform(ui, state, id, transform);
    }
    if let Some(canvas) = &object.ui_canvas {
        show_ui_canvas(ui, state, id, canvas);
    }
    for index in 0..object.lite_scripts.len() {
        show_lite_script(ui, state, object, index);
    }
    for index in 0..object.heavy_scripts.len() {
        show_heavy_script(ui, state, object, index);
    }
    ui.separator();
    if !part {
        show_add_component(ui, state, object);
    }
}

#[derive(Clone, Copy, Default)]
struct Mark {
    locked: bool,
    overridden: bool,
}

const PREFAB_LOCKED: &str =
    "Компоненты объекта префаба меняются в самом префабе: Open Prefab или Unpack";
const OVERRIDE_COLOR: egui::Color32 = egui::Color32::from_rgb(110, 170, 255);

fn mark(state: &EditorState, id: &str, field: OverrideField) -> Mark {
    let Some(scene) = &state.scene else {
        return Mark::default();
    };
    Mark {
        locked: scene.origin(id).is_some(),
        overridden: scene.is_overridden(id, &field),
    }
}

fn revert(state: &mut EditorState, id: &str, field: OverrideField) {
    state.edit_scene(|scene| scene.revert_field(id, &field));
}

fn marked_text(text: &str, overridden: bool) -> egui::RichText {
    if overridden {
        egui::RichText::new(text).strong().color(OVERRIDE_COLOR)
    } else {
        egui::RichText::new(text)
    }
}

/// Тег объекта из `[tags]` проекта; пустой тег означает «Untagged».
fn tag_combo(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let names = state
        .project
        .as_ref()
        .map(|project| project.settings.tags.names.clone())
        .unwrap_or_default();
    let id = object.id.as_str();
    let before = object.tag.clone();
    let mut tag = before.clone();
    let selected = if tag.is_empty() {
        "Untagged".to_owned()
    } else {
        tag.clone()
    };
    egui::ComboBox::from_id_salt(("object_tag", id))
        .selected_text(selected)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut tag, String::new(), "Untagged");
            for name in &names {
                ui.selectable_value(&mut tag, name.clone(), name.as_str());
            }
        });
    if tag != before {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.tag = tag;
                Ok(())
            })
        });
    }
}

/// Слой объекта `0..32`; имя берётся из `[layers]` проекта по индексу.
fn layer_combo(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let names = state
        .project
        .as_ref()
        .map(|project| project.settings.layers.names.clone())
        .unwrap_or_default();
    let id = object.id.as_str();
    let before = object.layer;
    let mut layer = before;
    egui::ComboBox::from_id_salt(("object_layer", id))
        .selected_text(layer_label(before, &names))
        .show_ui(ui, |ui| {
            for index in 0..LAYER_COUNT {
                ui.selectable_value(&mut layer, index, layer_label(index, &names));
            }
        });
    if layer != before {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.layer = layer;
                Ok(())
            })
        });
    }
}

fn layer_label(layer: u8, names: &[String]) -> String {
    match names.get(usize::from(layer)) {
        Some(name) if !name.is_empty() => format!("{layer}: {name}"),
        _ => layer.to_string(),
    }
}

fn show_prefab_bar(ui: &mut egui::Ui, state: &mut EditorState, id: &str) {
    let Some(scene) = &state.scene else {
        return;
    };
    let Some(origin) = scene.origin(id) else {
        return;
    };
    let Some(prefab) = scene
        .instance(&origin.instance)
        .map(|instance| instance.prefab.as_str().to_owned())
    else {
        return;
    };
    let has_override = scene.object_override(id).is_some();
    let root_fields = scene
        .instance(id)
        .is_some_and(|instance| instance.name.is_some());
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new(format!("Prefab /{prefab}")).color(OVERRIDE_COLOR));
        if ui
            .add_enabled(has_override, egui::Button::new("Apply to Prefab"))
            .on_hover_text("Перенести overrides объекта в файл префаба")
            .clicked()
        {
            state.apply_prompt = Some(id.to_owned());
        }
        if ui
            .add_enabled(has_override || root_fields, egui::Button::new("Revert All"))
            .clicked()
        {
            state.edit_scene(|scene| scene.revert_override(id));
        }
        if ui.button("Open Prefab").clicked() {
            state.action(Action::OpenScene(Some(prefab.clone())));
        }
    });
    ui.separator();
}

fn vec3_row(ui: &mut egui::Ui, label: &str, value: &mut [f32; 3], speed: f64) -> egui::Response {
    ui.label(label);
    let response = ui
        .horizontal(|ui| {
            let mut combined: Option<egui::Response> = None;
            for component in value.iter_mut() {
                let response = ui.add(egui::DragValue::new(component).speed(speed));
                combined = Some(match combined {
                    Some(previous) => previous | response,
                    None => response,
                });
            }
            combined.expect("вектор содержит три компонента")
        })
        .inner;
    ui.end_row();
    response
}

/// Строка цвета: палитра для выбора на глаз и поля для точных значений.
/// Поля не ограничены сверху: свет допускает HDR-значения, которые палитра не показывает.
/// Возвращает признак выбора в палитре и ответ полей.
fn color_row(ui: &mut egui::Ui, label: &str, value: &mut [f32; 3]) -> (bool, egui::Response) {
    ui.label(label);
    let (picked, response) = ui
        .horizontal(|ui| {
            // Палитра получает копию: её преобразование цвета не должно трогать HDR-значения.
            let mut shown = *value;
            let picked = ui.color_edit_button_rgb(&mut shown).changed();
            if picked {
                *value = shown.map(|channel| channel.max(0.0));
            }
            let mut combined = None;
            for channel in value.iter_mut() {
                let field = egui::DragValue::new(channel)
                    .speed(0.01)
                    .range(0.0..=f32::MAX);
                merge_response(&mut combined, ui.add(field));
            }
            (picked, combined.expect("цвет содержит три компонента"))
        })
        .inner;
    ui.end_row();
    (picked, response)
}

/// Применяет выбор в палитре. Палитра не сообщает об окончании перетаскивания, поэтому
/// её правки объединяются в одну запись истории, пока нажата кнопка мыши.
fn commit_picker(
    state: &mut EditorState,
    ui: &egui::Ui,
    id: &str,
    field: &str,
    picked: bool,
    change: impl FnOnce(&mut SceneObject),
) {
    let key = format!("{id}/{field}-picker");
    if picked {
        state.edit_scene(|scene| {
            scene.edit_object(id, Some(&key), |object| {
                change(object);
                Ok(())
            })
        });
    }
    if !ui.input(|input| input.pointer.any_down())
        && let Some(scene) = &mut state.scene
    {
        scene.end_continuous_edit_for(&key);
    }
}

fn show_transform(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let id = object.id.as_str();
    let field = OverrideField::Transform;
    let menu = component_menu(state, id, ComponentKind::Transform);
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Transform",
            icon: component_icon(state, ComponentKind::Transform),
        },
        id,
        mark(state, id, field.clone()),
        Some(menu),
        false,
        |ui| {
            egui::Grid::new("transform").num_columns(2).show(ui, |ui| {
                let mut transform: SceneTransform = object.transform;
                let position = vec3_row(ui, "Position", &mut transform.position, 0.05);
                let rotation = vec3_row(ui, "Rotation", &mut transform.rotation, 0.5);
                let scale = vec3_row(ui, "Scale", &mut transform.scale, 0.02);
                let response = position | rotation | scale;
                if response.changed() {
                    let key = format!("{id}/transform");
                    state.edit_scene(|scene| scene.set_transform(id, transform, Some(&key)));
                }
                finish_edit(state, &response);
            });
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    run_menu_action(state, id, ComponentKind::Transform, action);
}

/// Действие из заголовка компонента.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderAction {
    None,
    Remove,
    Revert,
    Reset,
    Copy,
    Paste,
}

/// Доступные пункты контекстного меню заголовка; Copy доступен всегда.
#[derive(Clone, Copy, Debug)]
struct ComponentMenu {
    reset: bool,
    paste: bool,
}

fn component_menu(state: &EditorState, id: &str, kind: ComponentKind) -> ComponentMenu {
    let object = state.scene.as_ref().and_then(|scene| scene.object(id));
    let locked_lod = kind == ComponentKind::LodGroup
        && state
            .scene
            .as_ref()
            .is_some_and(|scene| scene.is_prefab_part(id));
    ComponentMenu {
        reset: object.is_some_and(|object| Component::reset(object, kind).is_some()),
        paste: state
            .component_clipboard
            .as_ref()
            .is_some_and(|copied| copied.kind() == kind)
            && !locked_lod,
    }
}

/// Выполняет Reset/Copy/Paste; Remove обрабатывает сам компонент.
fn run_menu_action(state: &mut EditorState, id: &str, kind: ComponentKind, action: HeaderAction) {
    let Some(object) = state.scene.as_ref().and_then(|scene| scene.object(id)) else {
        return;
    };
    let component = match action {
        HeaderAction::Copy => {
            state.component_clipboard = Component::read(object, kind);
            return;
        }
        HeaderAction::Reset => Component::reset(object, kind),
        HeaderAction::Paste => state
            .component_clipboard
            .clone()
            .filter(|copied| copied.kind() == kind),
        HeaderAction::None | HeaderAction::Remove | HeaderAction::Revert => None,
    };
    if let Some(component) = component {
        state.edit_scene(|scene| scene.set_component(id, component));
    }
}

/// Подпись заголовка компонента и иконка перед ней.
#[derive(Clone, Copy)]
struct HeaderTitle<'a> {
    text: &'a str,
    icon: Option<egui::TextureId>,
}

/// Загруженная иконка компонента; без рисунка или до загрузки — `None`.
fn component_icon(state: &EditorState, kind: ComponentKind) -> Option<egui::TextureId> {
    icons::component_icon(kind).and_then(|name| state.icons.get(name))
}

/// Заголовок компонента: иконка (если есть), кнопка удаления и контекстное меню Reset/Copy/Paste по щелчку ПКМ.
fn component_header(
    ui: &mut egui::Ui,
    title: HeaderTitle<'_>,
    salt: impl std::hash::Hash + std::fmt::Debug,
    mark: Mark,
    menu: Option<ComponentMenu>,
    removable: bool,
    body: impl FnOnce(&mut egui::Ui),
) -> HeaderAction {
    let mut action = HeaderAction::None;
    let id = ui.make_persistent_id(("component", title.text, salt));
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
        .show_header(ui, |ui| {
            if let Some(texture) = title.icon {
                let size = ui.spacing().interact_size.y;
                ui.add(egui::Image::new(egui::load::SizedTexture::new(
                    texture,
                    egui::vec2(size, size),
                )));
            }
            let label = ui.add(
                egui::Label::new(marked_text(title.text, mark.overridden))
                    .sense(egui::Sense::click()),
            );
            if let Some(menu) = menu {
                label.context_menu(|ui| {
                    if ui
                        .add_enabled(menu.reset, egui::Button::new("Reset"))
                        .clicked()
                    {
                        action = HeaderAction::Reset;
                    }
                    if ui.button("Copy").clicked() {
                        action = HeaderAction::Copy;
                    }
                    if ui
                        .add_enabled(menu.paste, egui::Button::new("Paste"))
                        .clicked()
                    {
                        action = HeaderAction::Paste;
                    }
                });
            }
            if removable {
                let remove = ui
                    .add_enabled(!mark.locked, egui::Button::new("Remove").small())
                    .on_disabled_hover_text(PREFAB_LOCKED)
                    .clicked();
                if remove {
                    action = HeaderAction::Remove;
                }
            }
            if mark.overridden && ui.small_button("Revert").clicked() {
                action = HeaderAction::Revert;
            }
        })
        .body(body);
    action
}

fn show_camera(ui: &mut egui::Ui, state: &mut EditorState, id: &str, camera: &CameraComponent) {
    let field = OverrideField::Camera;
    let menu = component_menu(state, id, ComponentKind::Camera);
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Camera",
            icon: component_icon(state, ComponentKind::Camera),
        },
        id,
        mark(state, id, field.clone()),
        Some(menu),
        true,
        |ui| {
            egui::Grid::new("camera").num_columns(2).show(ui, |ui| {
                ui.label("Settings");
                let mut options = vec!["Default".to_owned()];
                options.extend(state.assets_of_kind(AssetKind::CameraSettings));
                let current = camera
                    .settings
                    .as_ref()
                    .map(|path| path.as_str())
                    .unwrap_or("Default");
                if let Some(chosen) = asset_picker(ui, "camera_settings", current, &options) {
                    let settings = if chosen == "Default" {
                        Some(None)
                    } else {
                        parse_path(state, &chosen).map(Some)
                    };
                    if let Some(settings) = settings {
                        state.edit_scene(|scene| scene.set_camera_settings(id, settings));
                    }
                }
                ui.end_row();
                ui.label("Post Process");
                let mut post_options = vec!["Default".to_owned()];
                post_options.extend(state.assets_of_kind(AssetKind::PostProcess));
                let current_post = camera
                    .post_process
                    .as_ref()
                    .map(|path| path.as_str())
                    .unwrap_or("Default");
                if let Some(chosen) =
                    asset_picker(ui, "camera_post_process", current_post, &post_options)
                {
                    let post = if chosen == "Default" {
                        Some(None)
                    } else {
                        parse_path(state, &chosen).map(Some)
                    };
                    if let Some(post) = post {
                        state.edit_scene(|scene| scene.set_camera_post_process(id, post));
                    }
                }
                ui.end_row();
                ui.label("Anti-Aliasing").on_hover_text(
                    "Immediately saves to the assigned .camerasettings asset; TOML comments and formatting are not preserved.",
                );
                if camera.settings.is_some() {
                    let current = state
                        .camera_settings_for(id)
                        .unwrap_or_default()
                        .render
                        .anti_aliasing;
                    let mut anti_aliasing = current;
                    egui::ComboBox::from_id_salt("camera_anti_aliasing")
                        .selected_text(anti_aliasing_label(anti_aliasing))
                        .show_ui(ui, |ui| {
                            for (value, label) in ANTI_ALIASING_OPTIONS {
                                ui.selectable_value(&mut anti_aliasing, value, label);
                            }
                        });
                    if anti_aliasing != current {
                        state.set_camera_anti_aliasing(id, anti_aliasing);
                    }
                } else {
                    ui.label("Assign a .camerasettings asset");
                }
                ui.end_row();
                ui.label("Active");
                let mut active = camera.active;
                let response = ui.checkbox(&mut active, "");
                if response.changed() {
                    state.edit_scene(|scene| scene.set_camera_active(id, active));
                }
                finish_edit(state, &response);
            });
            if let Some(path) = camera.post_process.clone() {
                show_post_process_editor(ui, state, &path);
            }
            show_screen_shaders(ui, state, id, &camera.screen_shaders);
            egui::CollapsingHeader::new("Cursor")
                .id_salt("camera_cursor")
                .default_open(false)
                .show(ui, |ui| {
                    let mut cursor = camera.cursor;
                    let hide = ui
                        .checkbox(&mut cursor.hide_on_start, "Hide on start")
                        .on_hover_text(CAMERA_CURSOR_TIP);
                    let lock = ui
                        .checkbox(&mut cursor.lock_on_start, "Lock on start")
                        .on_hover_text(CAMERA_CURSOR_TIP);
                    if hide.changed() || lock.changed() {
                        state.edit_scene(|scene| scene.set_camera_cursor(id, cursor));
                    }
                });
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_camera(id));
    }
    run_menu_action(state, id, ComponentKind::Camera, action);
}

const CAMERA_CURSOR_TIP: &str = "Действует, если камера активна при старте сцены";

const ANTI_ALIASING_OPTIONS: [(AntiAliasing, &str); 5] = [
    (AntiAliasing::Fxaa, "FXAA"),
    (AntiAliasing::None, "None"),
    (AntiAliasing::Taa, "TAA"),
    (AntiAliasing::Fsr, "FSR"),
    (AntiAliasing::Dlss, "DLSS"),
];

const fn anti_aliasing_label(value: AntiAliasing) -> &'static str {
    match value {
        AntiAliasing::Fxaa => "FXAA",
        AntiAliasing::None => "None",
        AntiAliasing::Taa => "TAA",
        AntiAliasing::Fsr => "FSR",
        AntiAliasing::Dlss => "DLSS",
    }
}

/// Выбор ресурса из выпадающего списка или перетаскиванием из Project.
fn asset_picker(
    ui: &mut egui::Ui,
    salt: &str,
    current: &str,
    options: &[String],
) -> Option<String> {
    let mut chosen = None;
    let response = egui::ComboBox::from_id_salt(salt)
        .selected_text(current)
        .width(ui.available_width().min(220.0))
        .show_ui(ui, |ui| {
            for option in options {
                if ui.selectable_label(option == current, option).clicked() {
                    chosen = Some(option.clone());
                }
            }
        })
        .response;
    if let Some(asset) = response.dnd_release_payload::<AssetDrag>() {
        chosen = Some(asset.0.clone());
    }
    chosen
}

fn parse_path(state: &mut EditorState, path: &str) -> Option<VirtualPath> {
    match VirtualPath::parse(path) {
        Ok(path) => Some(path),
        Err(_) => {
            state
                .log
                .error(format!("{path}: некорректный путь ресурса"));
            None
        }
    }
}

fn show_mesh(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let Some(mesh) = &object.mesh_renderer else {
        return;
    };
    let id = object.id.as_str();
    let models = state.assets_of_kind(AssetKind::Model);
    let mut textures = vec![String::new()];
    textures.extend(state.assets_of_kind(AssetKind::Texture));
    let mut model_choice = None;
    let mut texture_choice = None;
    let material_assets = state.assets_of_kind(AssetKind::Material);
    let part_count = state
        .models
        .get(mesh.model.as_str())
        .map_or(0, |model| model.meshes.len());
    let slot_count = part_count.max(mesh.materials.len());
    let mut slot_choice = None;
    let field = OverrideField::Mesh;
    let menu = component_menu(state, id, ComponentKind::MeshRenderer);
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Mesh Renderer",
            icon: component_icon(state, ComponentKind::MeshRenderer),
        },
        id,
        mark(state, id, field.clone()),
        Some(menu),
        true,
        |ui| {
            egui::Grid::new("mesh").num_columns(2).show(ui, |ui| {
                ui.label("Model");
                model_choice = asset_picker(ui, "mesh_model", mesh.model.as_str(), &models);
                ui.end_row();
                ui.label("Texture");
                let current = mesh.texture.as_ref().map_or("", |texture| texture.as_str());
                texture_choice = asset_picker(ui, "mesh_texture", current, &textures);
                ui.end_row();
            });
            ui.separator();
            ui.label("Materials");
            if slot_count == 0 {
                ui.weak("FBX part slots appear after the model loads.");
            }
            for index in 0..slot_count {
                ui.horizontal(|ui| {
                    ui.label(format!("Element {index}"));
                    let current = mesh
                        .materials
                        .get(index)
                        .and_then(Option::as_ref)
                        .map_or_else(String::new, |path| path.as_str().to_owned());
                    let mut next = current.clone();
                    egui::ComboBox::from_id_salt(("mesh-material-slot", id, index))
                        .selected_text(if current.is_empty() {
                            "None (FBX)"
                        } else {
                            &current
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut next, String::new(), "None (FBX)");
                            for path in &material_assets {
                                ui.selectable_value(&mut next, path.clone(), path);
                            }
                        });
                    // Запись только по выбору пользователя: отображение не меняет документ.
                    if next != current {
                        slot_choice = Some((index, next));
                    }
                });
            }
            ui.separator();
            show_material(ui, state, id, mesh.material.as_ref(), &textures);
        },
    );
    if model_choice.is_some() || texture_choice.is_some() {
        let model_changed = model_choice.is_some();
        let model = model_choice.map_or(Some(mesh.model.clone()), |path| parse_path(state, &path));
        let texture = match texture_choice {
            None => Some(mesh.texture.clone()),
            Some(path) if path.is_empty() => Some(None),
            Some(path) => parse_path(state, &path).map(Some),
        };
        if let (Some(model), Some(texture)) = (model, texture) {
            let changed = state.edit_scene(|scene| scene.set_mesh(id, model, texture));
            if changed.is_some() && model_changed {
                state.pending_frame = Some(id.to_owned());
            }
        }
    }
    if let Some((index, path)) = slot_choice {
        let slot = if path.is_empty() {
            Some(None)
        } else {
            parse_path(state, &path).map(Some)
        };
        if let Some(slot) = slot {
            let materials = materials_with_choice(&mesh.materials, slot_count, index, slot);
            state.edit_scene(|scene| scene.set_mesh_materials(id, materials));
        }
    }
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_mesh_renderer(id));
    }
    run_menu_action(state, id, ComponentKind::MeshRenderer, action);
}

/// Список слотов после выбора в части `index`. Дополнение до числа частей происходит только здесь,
/// поэтому отображение без выбора не записывает пустые слоты в сцену.
fn materials_with_choice(
    current: &[Option<VirtualPath>],
    slot_count: usize,
    index: usize,
    slot: Option<VirtualPath>,
) -> Vec<Option<VirtualPath>> {
    let mut materials = current.to_vec();
    materials.resize(slot_count, None);
    materials[index] = slot;
    materials
}

/// Поле override: флажок включает замену значения FBX, без него поле не показывается.
/// Возвращает `true`, если флажок переключён.
fn optional_value<T: Copy>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut Option<T>,
    default: T,
    field: impl FnOnce(&mut egui::Ui, &mut T) -> Option<egui::Response>,
    response: &mut Option<egui::Response>,
) -> bool {
    ui.label(label);
    let toggled = ui
        .horizontal(|ui| {
            let mut enabled = value.is_some();
            let toggled = ui.checkbox(&mut enabled, "").changed();
            if toggled {
                *value = enabled.then_some(default);
            }
            if let Some(value) = value
                && let Some(field) = field(ui, value)
            {
                merge_response(response, field);
            }
            toggled
        })
        .inner;
    ui.end_row();
    toggled
}

/// Scene material override: заданные поля заменяют значения материалов FBX.
fn show_material(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    current: Option<&MaterialOverride>,
    textures: &[String],
) {
    let mut material = current.cloned().unwrap_or(MaterialOverride {
        base_color: None,
        metallic: None,
        roughness: None,
        emissive: None,
        opacity: None,
        alpha_mode: None,
        alpha_cutoff: None,
        metallic_roughness_texture: None,
        normal_texture: None,
        emissive_texture: None,
        occlusion_texture: None,
        normal_scale: None,
        occlusion_strength: None,
        double_sided: None,
    });
    let mut toggled = false;
    let mut picked = false;
    let mut alpha_mode_picked = false;
    let mut response = None;
    let mut texture_choices = [None, None, None, None];
    ui.label("Material Override");
    egui::Grid::new("material").num_columns(2).show(ui, |ui| {
        toggled |= optional_value(
            ui,
            "Base Color",
            &mut material.base_color,
            [1.0; 3],
            |ui, color| {
                let mut shown = *color;
                if ui.color_edit_button_rgb(&mut shown).changed() {
                    *color = shown.map(|channel| channel.clamp(0.0, 1.0));
                    picked = true;
                }
                let mut combined = None;
                for channel in color.iter_mut() {
                    let field = egui::DragValue::new(channel).speed(0.01).range(0.0..=1.0);
                    merge_response(&mut combined, ui.add(field));
                }
                combined
            },
            &mut response,
        );
        let factor =
            |ui: &mut egui::Ui, value: &mut f32| Some(ui.add(egui::Slider::new(value, 0.0..=1.0)));
        toggled |= optional_value(
            ui,
            "Metallic",
            &mut material.metallic,
            0.0,
            factor,
            &mut response,
        );
        toggled |= optional_value(
            ui,
            "Roughness",
            &mut material.roughness,
            0.5,
            factor,
            &mut response,
        );
        toggled |= optional_value(
            ui,
            "Emissive",
            &mut material.emissive,
            [0.0; 3],
            |ui, color| {
                let mut combined = None;
                for channel in color.iter_mut() {
                    let field = egui::DragValue::new(channel)
                        .speed(0.1)
                        .range(0.0..=f32::MAX);
                    merge_response(&mut combined, ui.add(field));
                }
                combined
            },
            &mut response,
        );
        toggled |= optional_value(
            ui,
            "Opacity",
            &mut material.opacity,
            1.0,
            factor,
            &mut response,
        );
        toggled |= optional_value(
            ui,
            "Occlusion Strength",
            &mut material.occlusion_strength,
            1.0,
            factor,
            &mut response,
        );
        toggled |= optional_value(
            ui,
            "Alpha Mode",
            &mut material.alpha_mode,
            MaterialAlphaMode::Opaque,
            |ui, mode| {
                let label = match mode {
                    MaterialAlphaMode::Opaque => "Opaque",
                    MaterialAlphaMode::Mask => "Mask",
                    MaterialAlphaMode::Blend => "Blend",
                };
                let response = egui::ComboBox::from_id_salt(format!("material_alpha_mode_{id}"))
                    .selected_text(label)
                    .show_ui(ui, |ui| {
                        alpha_mode_picked |= ui
                            .selectable_value(mode, MaterialAlphaMode::Opaque, "Opaque")
                            .changed();
                        alpha_mode_picked |= ui
                            .selectable_value(mode, MaterialAlphaMode::Mask, "Mask")
                            .changed();
                        alpha_mode_picked |= ui
                            .selectable_value(mode, MaterialAlphaMode::Blend, "Blend")
                            .changed();
                    })
                    .response;
                Some(response)
            },
            &mut response,
        );
        if material.alpha_mode != Some(MaterialAlphaMode::Mask)
            && material.alpha_cutoff.take().is_some()
        {
            toggled = true;
        }
        if material.alpha_mode == Some(MaterialAlphaMode::Mask) {
            toggled |= optional_value(
                ui,
                "Alpha Cutoff",
                &mut material.alpha_cutoff,
                0.5,
                factor,
                &mut response,
            );
        }
        toggled |= optional_value(
            ui,
            "Normal Scale",
            &mut material.normal_scale,
            1.0,
            |ui, value| Some(ui.add(egui::DragValue::new(value).speed(0.01))),
            &mut response,
        );
        let maps = [
            ("Metal/Rough Map", &material.metallic_roughness_texture),
            ("Normal Map", &material.normal_texture),
            ("Emissive Map", &material.emissive_texture),
            ("Occlusion Map", &material.occlusion_texture),
        ];
        for (index, (label, path)) in maps.into_iter().enumerate() {
            ui.label(label);
            let current = path.as_ref().map_or("", |path| path.as_str());
            texture_choices[index] =
                asset_picker(ui, label, current, textures).filter(|choice| choice != current);
            ui.end_row();
        }
    });

    let mut discrete = toggled || alpha_mode_picked;
    for (choice, slot) in texture_choices.into_iter().zip([
        &mut material.metallic_roughness_texture,
        &mut material.normal_texture,
        &mut material.emissive_texture,
        &mut material.occlusion_texture,
    ]) {
        let Some(choice) = choice else {
            continue;
        };
        if choice.is_empty() {
            *slot = None;
        } else if let Some(path) = parse_path(state, &choice) {
            *slot = Some(path);
        } else {
            continue;
        }
        discrete = true;
    }
    let picker_key = format!("{id}/material-picker");
    if discrete {
        state.edit_scene(|scene| scene.set_material(id, Some(material), None));
        if let Some(scene) = &mut state.scene {
            scene.end_continuous_edit();
        }
    } else if picked {
        state.edit_scene(|scene| scene.set_material(id, Some(material), Some(&picker_key)));
    } else if let Some(response) = &response {
        if response.changed() {
            let key = format!("{id}/material");
            state.edit_scene(|scene| scene.set_material(id, Some(material), Some(&key)));
        }
        finish_edit(state, response);
    }
    if !ui.input(|input| input.pointer.any_down())
        && let Some(scene) = &mut state.scene
    {
        scene.end_continuous_edit_for(&picker_key);
    }
}

fn merge_response(combined: &mut Option<egui::Response>, response: egui::Response) {
    *combined = Some(match combined.take() {
        Some(previous) => previous | response,
        None => response,
    });
}

fn show_lod_group(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let Some(mut group) = object.lod_group.clone() else {
        return;
    };
    let original = group.clone();
    let id = object.id.as_str();
    let base_model = object
        .mesh_renderer
        .as_ref()
        .expect("LOD Group requires Mesh Renderer")
        .model
        .as_str()
        .to_owned();
    let models = state.assets_of_kind(AssetKind::Model);
    let mut textures = vec![String::new()];
    textures.extend(state.assets_of_kind(AssetKind::Texture));
    let mut asset_changed = false;
    let mut structure_changed = false;
    let mut response = None;
    let locked = state
        .scene
        .as_ref()
        .is_some_and(|scene| scene.is_prefab_part(id));
    let menu = component_menu(state, id, ComponentKind::LodGroup);
    let action = component_header(
        ui,
        HeaderTitle {
            text: "LOD Group",
            icon: component_icon(state, ComponentKind::LodGroup),
        },
        id,
        Mark {
            locked,
            overridden: false,
        },
        Some(menu),
        true,
        |ui| {
            ui.add_enabled_ui(!locked, |ui| {
                let can_remove_level = group.levels.len() > 1;
                let mut remove_level = None;
                for (index, level) in group.levels.iter_mut().enumerate() {
                    ui.push_id(("lod_level", index), |ui| {
                        ui.label(format!("LOD {}", index + 1));
                        egui::Grid::new("lod_level_fields")
                            .num_columns(2)
                            .show(ui, |ui| {
                                ui.label("Model");
                                let current = level.model.strip_prefix('/').unwrap_or(&level.model);
                                if let Some(choice) =
                                    asset_picker(ui, "lod_model", current, &models)
                                    && choice != current
                                    && let Some(path) = parse_path(state, &choice)
                                {
                                    level.model = path.as_str().to_owned();
                                    asset_changed = true;
                                }
                                ui.end_row();

                                ui.label("Texture");
                                let current = level
                                    .texture
                                    .as_deref()
                                    .map(|path| path.strip_prefix('/').unwrap_or(path))
                                    .unwrap_or("");
                                if let Some(choice) =
                                    asset_picker(ui, "lod_texture", current, &textures)
                                    && choice != current
                                {
                                    level.texture = if choice.is_empty() {
                                        None
                                    } else {
                                        parse_path(state, &choice)
                                            .map(|path| path.as_str().to_owned())
                                    };
                                    asset_changed = true;
                                }
                                ui.end_row();

                                ui.label("Distance");
                                let distance =
                                    ui.add(egui::DragValue::new(&mut level.distance).speed(0.1));
                                merge_response(&mut response, distance);
                                ui.end_row();
                            });
                        let remove =
                            ui.add_enabled(can_remove_level, egui::Button::new("Remove Level"));
                        if remove.clicked() {
                            remove_level = Some(index);
                        }
                        ui.separator();
                    });
                }
                if let Some(index) = remove_level {
                    group.levels.remove(index);
                    structure_changed = true;
                }

                let add = ui.add_enabled(
                    group.levels.len() < MAX_LOD_LEVELS,
                    egui::Button::new("Add Level"),
                );
                if add.clicked() {
                    let distance = group.levels.last().unwrap().distance * 2.0;
                    group.levels.push(LodLevel {
                        model: base_model.clone(),
                        texture: None,
                        distance,
                    });
                    structure_changed = true;
                }

                ui.separator();
                let last_distance = group.levels.last().unwrap().distance;
                let mut cull_enabled = group.cull_distance.is_some();
                egui::Grid::new("lod_cull_fields")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("Cull Distance");
                        let cull = ui.checkbox(&mut cull_enabled, "");
                        if cull.changed() {
                            group.cull_distance = cull_enabled.then_some(last_distance * 2.0);
                        }
                        merge_response(&mut response, cull);
                        ui.end_row();
                        if let Some(mut distance) = group.cull_distance {
                            ui.label("Distance");
                            let value = ui.add(egui::DragValue::new(&mut distance).speed(0.1));
                            group.cull_distance = Some(distance);
                            merge_response(&mut response, value);
                            ui.end_row();
                        }
                    });
            });
        },
    );
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.set_lod_group(id, None));
        return;
    }
    if action != HeaderAction::None {
        run_menu_action(state, id, ComponentKind::LodGroup, action);
        return;
    }
    if group != original {
        if asset_changed || structure_changed {
            state.edit_scene(|scene| scene.set_lod_group(id, Some(group)));
            if let Some(scene) = &mut state.scene {
                scene.end_continuous_edit();
            }
        } else if let Some(response) = &response {
            commit(state, id, "lod_group", response, |object| {
                object.lod_group = Some(group);
            });
        }
    } else if let Some(response) = &response {
        finish_edit(state, response);
    }
}

fn show_light(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let Some(light) = object.directional_light else {
        return;
    };
    let id = object.id.as_str();
    let field = OverrideField::Light;
    let menu = component_menu(state, id, ComponentKind::DirectionalLight);
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Directional Light",
            icon: component_icon(state, ComponentKind::DirectionalLight),
        },
        id,
        mark(state, id, field.clone()),
        Some(menu),
        true,
        |ui| {
            egui::Grid::new("light").num_columns(2).show(ui, |ui| {
                let mut value = light;
                let (color_picked, color) = color_row(ui, "Color", &mut value.color);
                let (ambient_picked, ambient) = color_row(ui, "Ambient", &mut value.ambient);
                ui.label("Shadows");
                let shadows = ui.checkbox(&mut value.shadows, "");
                ui.end_row();
                ui.label("Shadow Distance");
                let mut distance = value.shadow_distance;
                let distance_response = ui.add(
                    egui::DragValue::new(&mut distance)
                        .speed(0.1)
                        .range(0.01..=f32::MAX),
                );
                value.shadow_distance = if distance.is_finite() {
                    distance.max(0.01)
                } else {
                    100.0
                };
                ui.end_row();
                ui.label("Shadow Cascades");
                let mut cascades = u32::from(value.shadow_cascades);
                let cascades_response =
                    ui.add(egui::DragValue::new(&mut cascades).speed(1.0).range(1..=4));
                value.shadow_cascades = cascades as u8;
                ui.end_row();
                let response = color | ambient | shadows | distance_response | cascades_response;
                let picked = color_picked || ambient_picked;
                commit_picker(state, ui, id, "light", picked, |object| {
                    object.directional_light = Some(value);
                });
                if !picked {
                    commit(state, id, "light", &response, |object| {
                        object.directional_light = Some(value);
                    });
                }
            });
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.directional_light = None;
                Ok(())
            })
        });
    }
    run_menu_action(state, id, ComponentKind::DirectionalLight, action);
}

fn show_point_light(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    light: PointLightComponent,
) {
    let field = OverrideField::PointLight;
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Point Light",
            icon: component_icon(state, ComponentKind::PointLight),
        },
        id,
        mark(state, id, field.clone()),
        Some(component_menu(state, id, ComponentKind::PointLight)),
        true,
        |ui| {
            egui::Grid::new("point_light")
                .num_columns(2)
                .show(ui, |ui| {
                    let mut value = light;
                    let (picked, color) = color_row(ui, "Color", &mut value.color);
                    ui.label("Intensity");
                    let intensity = ui.add(
                        egui::DragValue::new(&mut value.intensity)
                            .speed(0.05)
                            .range(0.0..=f32::MAX),
                    );
                    ui.end_row();
                    ui.label("Range");
                    let range = ui.add(
                        egui::DragValue::new(&mut value.range)
                            .speed(0.05)
                            .range(f32::MIN_POSITIVE..=f32::MAX),
                    );
                    ui.end_row();
                    ui.label("Shadows");
                    let shadows = ui.checkbox(&mut value.shadows, "");
                    ui.end_row();
                    let response = color | intensity | range | shadows;
                    commit_picker(state, ui, id, "point_light", picked, |object| {
                        object.point_light = Some(value);
                    });
                    if !picked {
                        commit(state, id, "point_light", &response, |object| {
                            object.point_light = Some(value);
                        });
                    }
                });
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.point_light = None;
                Ok(())
            })
        });
    }
    run_menu_action(state, id, ComponentKind::PointLight, action);
}

/// Секция аниматора: поля компонента, клипы из FBX и одномерные blend tree.
///
/// Каждое изменение идёт через `commit`, поэтому правки попадают в историю и откатываются
/// при ошибке валидации, как у остальных компонентов.
fn show_animator(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    animator: &engine_scene::AnimatorComponent,
) {
    let field = OverrideField::Animator;
    // Клип из поля Add clip объявляется в [[assets]] сцены через set_component, а не правкой.
    let mut added_clip = None;
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Animator",
            icon: component_icon(state, ComponentKind::Animator),
        },
        id,
        mark(state, id, field.clone()),
        Some(component_menu(state, id, ComponentKind::Animator)),
        true,
        |ui| {
            let mut value = animator.clone();
            let mut response = ui.allocate_response(egui::Vec2::ZERO, egui::Sense::hover());
            egui::Grid::new("animator").num_columns(2).show(ui, |ui| {
                ui.label("Default motion");
                let mut motion = value.default_motion.clone().unwrap_or_default();
                let edit = ui.text_edit_singleline(&mut motion);
                if edit.changed() {
                    value.default_motion = (!motion.is_empty()).then_some(motion);
                }
                response |= edit;
                ui.end_row();

                ui.label("Play on start");
                response |= ui.checkbox(&mut value.play_on_start, "");
                ui.end_row();

                ui.label("Loop");
                response |= ui.checkbox(&mut value.looping, "");
                ui.end_row();

                ui.label("Speed");
                response |= ui.add(
                    egui::DragValue::new(&mut value.speed)
                        .speed(0.01)
                        .range(0.0..=f32::MAX),
                );
                ui.end_row();
            });

            ui.separator();
            ui.label("Clips");
            let mut remove_clip = None;
            for (index, clip) in value.clips.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.monospace(format!("/{}", clip.as_str()));
                    if ui.small_button("×").clicked() {
                        remove_clip = Some(index);
                    }
                });
            }
            if let Some(index) = remove_clip {
                value.clips.remove(index);
                response.mark_changed();
            }
            let input_id = ui.make_persistent_id(("animator_clip_input", id));
            let mut input =
                ui.data_mut(|data| data.get_temp::<String>(input_id).unwrap_or_default());
            ui.horizontal(|ui| {
                ui.text_edit_singleline(&mut input);
                if ui.button("Add clip").clicked() && !input.is_empty() {
                    if let Ok(clip) = VirtualPath::parse(input.trim_start_matches('/')) {
                        added_clip = Some(clip);
                        input.clear();
                    } else {
                        state
                            .log
                            .error(format!("{input}: некорректный путь ресурса"));
                    }
                }
            });
            ui.data_mut(|data| data.insert_temp(input_id, input));

            ui.separator();
            ui.label("Blend trees");
            let mut remove_tree = None;
            for (tree_index, tree) in value.blend_trees.iter_mut().enumerate() {
                ui.push_id(("blend_tree", tree_index), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        response |= ui.text_edit_singleline(&mut tree.name);
                        ui.label("Parameter");
                        response |= ui.text_edit_singleline(&mut tree.parameter);
                        if ui.small_button("Remove tree").clicked() {
                            remove_tree = Some(tree_index);
                        }
                    });
                    let mut remove_point = None;
                    for (point_index, point) in tree.points.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label("Clip");
                            response |= ui.text_edit_singleline(&mut point.clip);
                            response |=
                                ui.add(egui::DragValue::new(&mut point.threshold).speed(0.01));
                            if ui.small_button("×").clicked() {
                                remove_point = Some(point_index);
                            }
                        });
                    }
                    if let Some(index) = remove_point {
                        tree.points.remove(index);
                        response.mark_changed();
                    }
                    if ui.small_button("Add point").clicked() {
                        tree.points.push(engine_scene::BlendTreePoint {
                            clip: String::new(),
                            threshold: 0.0,
                        });
                        response.mark_changed();
                    }
                });
            }
            if let Some(index) = remove_tree {
                value.blend_trees.remove(index);
                response.mark_changed();
            }
            if ui.button("Add blend tree").clicked() {
                value.blend_trees.push(engine_scene::BlendTree {
                    name: format!("tree{}", value.blend_trees.len() + 1),
                    parameter: "speed".to_owned(),
                    points: Vec::new(),
                });
                response.mark_changed();
            }

            if !state.is_playing() {
                ui.separator();
                show_animation_preview(ui, state, id, &value.blend_trees);
            }

            commit(state, id, "animator", &response, |object| {
                object.animator = Some(value.clone());
            });
        },
    );
    if let Some(clip) = added_clip {
        let mut next = animator.clone();
        next.clips.push(clip);
        state.edit_scene(|scene| scene.set_component(id, Component::Animator(next)));
    }
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_animator(id));
    }
    run_menu_action(state, id, ComponentKind::Animator, action);
}

/// Превью аниматора выбранного объекта: проигрывание, выбор движения, перемотка и параметры
/// blend tree. Меняет только настройки превью и не попадает в документ и историю.
fn show_animation_preview(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    blend_trees: &[engine_scene::BlendTree],
) {
    ui.label("Preview");
    if state.animation_status.object.as_deref() != Some(id) {
        // Кадр ещё не пересчитал превью для выбранного объекта.
        return;
    }
    let motions = state.animation_status.motions.clone();
    let (duration, time) = (state.animation_status.duration, state.animation_status.time);
    ui.horizontal(|ui| {
        let label = if state.animation_preview.playing {
            "Pause"
        } else {
            "Play"
        };
        if ui.button(label).clicked() {
            state.animation_preview.playing = !state.animation_preview.playing;
        }
        let selected_text = state
            .animation_preview
            .motion
            .clone()
            .unwrap_or_else(|| "Bind pose".to_owned());
        egui::ComboBox::from_id_salt(("animation_preview_motion", id))
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(state.animation_preview.motion.is_none(), "Bind pose")
                    .clicked()
                {
                    state.animation_preview.motion = None;
                }
                for motion in &motions {
                    let current = state.animation_preview.motion.as_deref() == Some(motion);
                    if ui.selectable_label(current, motion).clicked() {
                        state.animation_preview.motion = Some(motion.clone());
                    }
                }
            });
    });
    if state.animation_preview.motion.is_some() && duration > 0.0 {
        let mut scrub = time;
        let response = ui.add(
            egui::Slider::new(&mut scrub, 0.0..=duration)
                .text("Time")
                .suffix(" s"),
        );
        if response.changed() {
            state.animation_preview.seek = Some(scrub);
        }
    }
    let parameters: std::collections::BTreeSet<&str> = blend_trees
        .iter()
        .map(|tree| tree.parameter.as_str())
        .collect();
    for name in parameters {
        let mut value = state
            .animation_preview
            .parameters
            .get(name)
            .copied()
            .unwrap_or(0.0);
        ui.horizontal(|ui| {
            ui.label(name);
            if ui
                .add(egui::DragValue::new(&mut value).speed(0.01))
                .changed()
            {
                state
                    .animation_preview
                    .parameters
                    .insert(name.to_owned(), value);
            }
        });
    }
}

fn show_audio_source(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    audio: &AudioSourceComponent,
) {
    let field = OverrideField::AudioSource;
    let clips = state.assets_of_kind(AssetKind::Audio);
    let mut clip_choice = None;
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Audio Source",
            icon: component_icon(state, ComponentKind::AudioSource),
        },
        id,
        mark(state, id, field.clone()),
        Some(component_menu(state, id, ComponentKind::AudioSource)),
        true,
        |ui| {
            egui::Grid::new("audio_source")
                .num_columns(2)
                .show(ui, |ui| {
                    let mut value = audio.clone();
                    ui.label("Clip");
                    clip_choice = asset_picker(ui, "audio_clip", audio.clip.as_str(), &clips);
                    ui.end_row();
                    ui.label("Group");
                    let group = egui::ComboBox::from_id_salt("audio_group")
                        .selected_text(audio_group_label(value.group))
                        .show_ui(ui, |ui| {
                            for option in
                                [AudioGroup::Music, AudioGroup::Effects, AudioGroup::Ambient]
                            {
                                ui.selectable_value(
                                    &mut value.group,
                                    option,
                                    audio_group_label(option),
                                );
                            }
                        })
                        .response;
                    ui.end_row();
                    ui.label("Volume");
                    let volume = ui.add(
                        egui::DragValue::new(&mut value.volume)
                            .speed(0.01)
                            .range(0.0..=engine_scene::MAX_AUDIO_VOLUME),
                    );
                    ui.end_row();
                    ui.label("Loop");
                    let looping = ui.checkbox(&mut value.looping, "");
                    ui.end_row();
                    ui.label("Play on start");
                    let play_on_start = ui.checkbox(&mut value.play_on_start, "");
                    ui.end_row();
                    ui.label("Spatial");
                    let spatial = ui.checkbox(&mut value.spatial, "");
                    ui.end_row();
                    ui.label("Min distance");
                    let min = ui.add(
                        egui::DragValue::new(&mut value.min_distance)
                            .speed(0.05)
                            .range(f32::MIN_POSITIVE..=f32::MAX),
                    );
                    ui.end_row();
                    ui.label("Max distance");
                    let max = ui.add(
                        egui::DragValue::new(&mut value.max_distance)
                            .speed(0.05)
                            .range(f32::MIN_POSITIVE..=f32::MAX),
                    );
                    ui.end_row();
                    let response = group | volume | looping | play_on_start | spatial | min | max;
                    commit(state, id, "audio_source", &response, |object| {
                        object.audio_source = Some(value.clone());
                    });
                });
        },
    );
    if let Some(path) = clip_choice
        && let Some(clip) = parse_path(state, &path)
    {
        // set_component объявляет клип в [[assets]] сцены; прямая правка этого не делает.
        state.edit_scene(|scene| {
            scene.set_component(
                id,
                Component::AudioSource(AudioSourceComponent {
                    clip: clip.clone(),
                    ..audio.clone()
                }),
            )
        });
    }
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_audio_source(id));
    }
    run_menu_action(state, id, ComponentKind::AudioSource, action);
}

fn show_network_identity(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    identity: NetworkIdentityComponent,
) {
    let field = OverrideField::NetworkIdentity;
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Network Identity",
            icon: component_icon(state, ComponentKind::NetworkIdentity),
        },
        id,
        mark(state, id, field.clone()),
        Some(component_menu(state, id, ComponentKind::NetworkIdentity)),
        true,
        |ui| {
            egui::Grid::new("network_identity")
                .num_columns(2)
                .show(ui, |ui| {
                    let mut value = identity;
                    ui.label("Authority");
                    let authority = egui::ComboBox::from_id_salt("network_authority")
                        .selected_text(net_authority_label(value.authority))
                        .show_ui(ui, |ui| {
                            for option in [NetAuthority::Server, NetAuthority::Owner] {
                                ui.selectable_value(
                                    &mut value.authority,
                                    option,
                                    net_authority_label(option),
                                );
                            }
                        })
                        .response;
                    ui.end_row();
                    commit(state, id, "network_identity", &authority, |object| {
                        object.network_identity = Some(value);
                    });
                });
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    // Идентификатор без трансформации не бывает: удаление уносит и `network_transform`.
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_network_identity(id));
    }
    run_menu_action(state, id, ComponentKind::NetworkIdentity, action);
}

fn show_network_transform(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    transform: NetworkTransformComponent,
) {
    let field = OverrideField::NetworkTransform;
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Network Transform",
            icon: component_icon(state, ComponentKind::NetworkTransform),
        },
        id,
        mark(state, id, field.clone()),
        Some(component_menu(state, id, ComponentKind::NetworkTransform)),
        true,
        |ui| {
            egui::Grid::new("network_transform")
                .num_columns(2)
                .show(ui, |ui| {
                    let mut value = transform;
                    ui.label("Sync position");
                    let position = ui.checkbox(&mut value.sync_position, "");
                    ui.end_row();
                    ui.label("Sync rotation");
                    let rotation = ui.checkbox(&mut value.sync_rotation, "");
                    ui.end_row();
                    ui.label("Sync scale");
                    let scale = ui.checkbox(&mut value.sync_scale, "");
                    ui.end_row();
                    ui.label("Send rate");
                    let rate = ui.add(
                        egui::DragValue::new(&mut value.send_rate_hz)
                            .range(1..=engine_scene::MAX_SEND_RATE_HZ)
                            .suffix(" Hz"),
                    );
                    ui.end_row();
                    ui.label("Interpolate");
                    let interpolate = ui.checkbox(&mut value.interpolate, "");
                    ui.end_row();
                    ui.label("Interpolation delay");
                    let delay = ui.add(
                        egui::DragValue::new(&mut value.interpolation_delay_ms)
                            .range(0..=engine_scene::MAX_INTERPOLATION_DELAY_MS)
                            .suffix(" ms"),
                    );
                    ui.end_row();
                    ui.label("Snap distance");
                    let snap = ui.add(
                        egui::DragValue::new(&mut value.snap_distance)
                            .speed(0.05)
                            .range(f32::MIN_POSITIVE..=f32::MAX),
                    );
                    ui.end_row();
                    let response = position | rotation | scale | rate | interpolate | delay | snap;
                    commit(state, id, "network_transform", &response, |object| {
                        object.network_transform = Some(value);
                    });
                });
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_network_transform(id));
    }
    run_menu_action(state, id, ComponentKind::NetworkTransform, action);
}

fn show_ui_canvas(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    canvas: &engine_scene::UiCanvasComponent,
) {
    let action = component_header(
        ui,
        HeaderTitle {
            text: "UI Canvas",
            icon: component_icon(state, ComponentKind::UiCanvas),
        },
        id,
        mark(state, id, OverrideField::UiCanvas),
        Some(component_menu(state, id, ComponentKind::UiCanvas)),
        true,
        |ui| {
            let mut value = canvas.clone();
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.label("Document");
                let selected = value.document.as_str().to_owned();
                egui::ComboBox::from_id_salt(("ui_canvas_document", id))
                    .selected_text(&selected)
                    .show_ui(ui, |ui| {
                        for path in state.assets_of_kind(AssetKind::Ui) {
                            if let Some(document) = parse_path(state, &path)
                                && ui
                                    .selectable_value(&mut value.document, document, &path)
                                    .changed()
                            {
                                changed = true;
                            }
                        }
                    });
            });
            changed |= ui.checkbox(&mut value.visible, "Visible").changed();
            ui.horizontal(|ui| {
                ui.label("Layer");
                let mut layer = value.layer.unwrap_or(0);
                if ui.add(egui::DragValue::new(&mut layer)).changed() {
                    value.layer = Some(layer);
                    changed = true;
                }
            });
            if changed {
                state.edit_scene(|scene| {
                    scene.edit_object(id, Some(&format!("{id}/ui_canvas")), |object| {
                        object.ui_canvas = Some(value);
                        Ok(())
                    })
                });
            }
        },
    );
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.ui_canvas = None;
                Ok(())
            })
        });
    }
    run_menu_action(state, id, ComponentKind::UiCanvas, action);
}

fn net_authority_label(authority: NetAuthority) -> &'static str {
    match authority {
        NetAuthority::Server => "Server",
        NetAuthority::Owner => "Owner",
    }
}

fn show_particle_emitter(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    emitter: &engine_scene::ParticleEmitterComponent,
) {
    let field = OverrideField::ParticleEmitter;
    let textures = state.assets_of_kind(AssetKind::Texture);
    let mut texture_choice = None;
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Particle Emitter",
            icon: component_icon(state, ComponentKind::ParticleEmitter),
        },
        id,
        mark(state, id, field.clone()),
        Some(component_menu(state, id, ComponentKind::ParticleEmitter)),
        true,
        |ui| {
            egui::Grid::new("particle_emitter")
                .num_columns(2)
                .show(ui, |ui| {
                    let mut value = emitter.clone();
                    let mut responses = Vec::new();
                    ui.label("Blend");
                    responses.push(
                        egui::ComboBox::from_id_salt("particle_blend")
                            .selected_text(particle_blend_label(value.blend))
                            .show_ui(ui, |ui| {
                                for option in [
                                    engine_scene::ParticleBlendMode::Additive,
                                    engine_scene::ParticleBlendMode::Alpha,
                                ] {
                                    ui.selectable_value(
                                        &mut value.blend,
                                        option,
                                        particle_blend_label(option),
                                    );
                                }
                            })
                            .response,
                    );
                    ui.end_row();
                    ui.label("Texture");
                    texture_choice = asset_picker(
                        ui,
                        "particle_texture",
                        value.texture.as_ref().map_or("", |path| path.as_str()),
                        &textures,
                    );
                    ui.end_row();
                    ui.label("Capacity");
                    responses.push(
                        ui.add(
                            egui::DragValue::new(&mut value.capacity)
                                .range(1..=engine_scene::MAX_ADDITIVE_PARTICLE_CAPACITY),
                        ),
                    );
                    ui.end_row();
                    ui.label("Rate");
                    responses.push(
                        ui.add(
                            egui::DragValue::new(&mut value.rate)
                                .speed(0.5)
                                .range(0.0..=f32::MAX),
                        ),
                    );
                    ui.end_row();
                    ui.label("Duration");
                    responses.push(
                        ui.add(
                            egui::DragValue::new(&mut value.duration)
                                .speed(0.05)
                                .range(0.001..=f32::MAX),
                        ),
                    );
                    ui.end_row();
                    ui.label("Loop");
                    responses.push(ui.checkbox(&mut value.looping, ""));
                    ui.end_row();
                    responses.push(range_row(ui, "Lifetime", &mut value.lifetime, 0.05));
                    responses.push(range_row(ui, "Speed", &mut value.speed, 0.05));
                    responses.push(range_row(ui, "Size", &mut value.size, 0.01));
                    responses.push(range_row(ui, "Rotation", &mut value.rotation, 0.05));
                    responses.push(range_row(
                        ui,
                        "Angular velocity",
                        &mut value.angular_velocity,
                        0.05,
                    ));
                    ui.label("Gravity");
                    ui.horizontal(|ui| {
                        for axis in &mut value.gravity {
                            responses.push(ui.add(egui::DragValue::new(axis).speed(0.05)));
                        }
                    });
                    ui.end_row();
                    ui.label("Drag");
                    responses.push(
                        ui.add(
                            egui::DragValue::new(&mut value.drag)
                                .speed(0.01)
                                .range(0.0..=f32::MAX),
                        ),
                    );
                    ui.end_row();
                    ui.label("Soft distance");
                    responses.push(
                        ui.add(
                            egui::DragValue::new(&mut value.soft_distance)
                                .speed(0.01)
                                .range(0.0..=f32::MAX),
                        ),
                    );
                    ui.end_row();
                    ui.label("Shape");
                    responses.push(
                        egui::ComboBox::from_id_salt("particle_shape")
                            .selected_text(particle_shape_label(&value.shape))
                            .show_ui(ui, |ui| {
                                for option in particle_shape_options() {
                                    let label = particle_shape_label(&option);
                                    if ui
                                        .selectable_label(
                                            particle_shape_label(&value.shape) == label,
                                            label,
                                        )
                                        .clicked()
                                    {
                                        value.shape = option;
                                    }
                                }
                            })
                            .response,
                    );
                    ui.end_row();
                    match &mut value.shape {
                        engine_scene::ParticleShapeComponent::Point {} => {}
                        engine_scene::ParticleShapeComponent::Sphere { radius } => {
                            ui.label("Radius");
                            responses.push(ui.add(egui::DragValue::new(radius).speed(0.01)));
                            ui.end_row();
                        }
                        engine_scene::ParticleShapeComponent::Cone { angle, radius } => {
                            ui.label("Angle");
                            responses.push(
                                ui.add(
                                    egui::DragValue::new(angle)
                                        .speed(0.01)
                                        .range(0.0..=std::f32::consts::FRAC_PI_2),
                                ),
                            );
                            ui.end_row();
                            ui.label("Radius");
                            responses.push(ui.add(egui::DragValue::new(radius).speed(0.01)));
                            ui.end_row();
                        }
                        engine_scene::ParticleShapeComponent::Box { half_extents } => {
                            ui.label("Half extents");
                            ui.horizontal(|ui| {
                                for axis in half_extents.iter_mut() {
                                    responses.push(
                                        ui.add(
                                            egui::DragValue::new(axis)
                                                .speed(0.01)
                                                .range(0.0..=f32::MAX),
                                        ),
                                    );
                                }
                            });
                            ui.end_row();
                        }
                    }
                    ui.label("Flipbook");
                    ui.horizontal(|ui| {
                        responses.push(
                            ui.add(egui::DragValue::new(&mut value.flipbook.columns).range(1..=16)),
                        );
                        responses.push(
                            ui.add(egui::DragValue::new(&mut value.flipbook.rows).range(1..=16)),
                        );
                        responses.push(
                            ui.add(
                                egui::DragValue::new(&mut value.flipbook.frames_per_second)
                                    .speed(0.1)
                                    .range(0.0..=f32::MAX),
                            ),
                        );
                    });
                    ui.end_row();
                    let mut collision_enabled = value.collision.is_some();
                    ui.label("Collision");
                    responses.push(ui.checkbox(&mut collision_enabled, ""));
                    ui.end_row();
                    if collision_enabled {
                        let collision = value.collision.get_or_insert_with(Default::default);
                        ui.label("Restitution");
                        responses.push(
                            ui.add(
                                egui::DragValue::new(&mut collision.restitution)
                                    .speed(0.01)
                                    .range(0.0..=1.0),
                            ),
                        );
                        ui.end_row();
                        ui.label("Friction");
                        responses.push(
                            ui.add(
                                egui::DragValue::new(&mut collision.friction)
                                    .speed(0.01)
                                    .range(0.0..=1.0),
                            ),
                        );
                        ui.end_row();
                        ui.label("Thickness");
                        responses.push(
                            ui.add(
                                egui::DragValue::new(&mut collision.thickness)
                                    .speed(0.005)
                                    .range(0.0..=f32::MAX),
                            ),
                        );
                        ui.end_row();
                    } else {
                        value.collision = None;
                    }
                    let response = responses
                        .into_iter()
                        .reduce(|left, right| left | right)
                        .expect("particle emitter has fields");
                    commit(state, id, "particle_emitter", &response, |object| {
                        object.particle_emitter = Some(value.clone());
                    });
                });
        },
    );
    if let Some(path) = texture_choice
        && let Some(texture) = parse_path(state, &path)
    {
        // set_component объявляет текстуру в [[assets]] сцены; прямая правка этого не делает.
        state.edit_scene(|scene| {
            scene.set_component(
                id,
                Component::ParticleEmitter(engine_scene::ParticleEmitterComponent {
                    texture: Some(texture.clone()),
                    ..emitter.clone()
                }),
            )
        });
    }
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_particle_emitter(id));
    }
    run_menu_action(state, id, ComponentKind::ParticleEmitter, action);
}

/// Строка диапазона `[min, max]` в сетке инспектора.
fn range_row(ui: &mut egui::Ui, label: &str, range: &mut [f32; 2], speed: f64) -> egui::Response {
    ui.label(label);
    let response = ui
        .horizontal(|ui| {
            let low = ui.add(egui::DragValue::new(&mut range[0]).speed(speed));
            let high = ui.add(egui::DragValue::new(&mut range[1]).speed(speed));
            low | high
        })
        .inner;
    ui.end_row();
    response
}

fn particle_blend_label(blend: engine_scene::ParticleBlendMode) -> &'static str {
    match blend {
        engine_scene::ParticleBlendMode::Additive => "Additive",
        engine_scene::ParticleBlendMode::Alpha => "Alpha",
    }
}

fn particle_shape_options() -> [engine_scene::ParticleShapeComponent; 4] {
    [
        engine_scene::ParticleShapeComponent::Point {},
        engine_scene::ParticleShapeComponent::Sphere { radius: 0.5 },
        engine_scene::ParticleShapeComponent::Cone {
            angle: 0.3,
            radius: 0.1,
        },
        engine_scene::ParticleShapeComponent::Box {
            half_extents: [0.5; 3],
        },
    ]
}

fn particle_shape_label(shape: &engine_scene::ParticleShapeComponent) -> &'static str {
    match shape {
        engine_scene::ParticleShapeComponent::Point {} => "Point",
        engine_scene::ParticleShapeComponent::Sphere { .. } => "Sphere",
        engine_scene::ParticleShapeComponent::Cone { .. } => "Cone",
        engine_scene::ParticleShapeComponent::Box { .. } => "Box",
    }
}

fn audio_group_label(group: AudioGroup) -> &'static str {
    match group {
        AudioGroup::Master => "Master",
        AudioGroup::Music => "Music",
        AudioGroup::Effects => "Effects",
        AudioGroup::Ambient => "Ambient",
    }
}

fn show_spot_light(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    light: SpotLightComponent,
) {
    let field = OverrideField::SpotLight;
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Spot Light",
            icon: component_icon(state, ComponentKind::SpotLight),
        },
        id,
        mark(state, id, field.clone()),
        Some(component_menu(state, id, ComponentKind::SpotLight)),
        true,
        |ui| {
            egui::Grid::new("spot_light").num_columns(2).show(ui, |ui| {
                let mut value = light;
                let (picked, color) = color_row(ui, "Color", &mut value.color);
                ui.label("Intensity");
                let intensity = ui.add(
                    egui::DragValue::new(&mut value.intensity)
                        .speed(0.05)
                        .range(0.0..=f32::MAX),
                );
                ui.end_row();
                ui.label("Range");
                let range = ui.add(
                    egui::DragValue::new(&mut value.range)
                        .speed(0.05)
                        .range(f32::MIN_POSITIVE..=f32::MAX),
                );
                ui.end_row();
                ui.label("Inner Angle");
                let inner = ui.add(
                    egui::DragValue::new(&mut value.inner_angle_deg)
                        .speed(0.5)
                        .range(0.01..=value.outer_angle_deg),
                );
                ui.end_row();
                ui.label("Outer Angle");
                let outer = ui.add(
                    egui::DragValue::new(&mut value.outer_angle_deg)
                        .speed(0.5)
                        .range(value.inner_angle_deg..=90.0),
                );
                ui.end_row();
                ui.label("Shadows");
                let shadows = ui.checkbox(&mut value.shadows, "");
                ui.end_row();
                let response = color | intensity | range | inner | outer | shadows;
                commit_picker(state, ui, id, "spot_light", picked, |object| {
                    object.spot_light = Some(value);
                });
                if !picked {
                    commit(state, id, "spot_light", &response, |object| {
                        object.spot_light = Some(value);
                    });
                }
            });
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.spot_light = None;
                Ok(())
            })
        });
    }
    run_menu_action(state, id, ComponentKind::SpotLight, action);
}

const COLLIDER_SHAPES: [(ColliderShape, &str); 5] = [
    (ColliderShape::Box, "Box"),
    (ColliderShape::Sphere, "Sphere"),
    (ColliderShape::Capsule, "Capsule"),
    (ColliderShape::ConvexMesh, "Convex Mesh"),
    (ColliderShape::TriangleMesh, "Triangle Mesh"),
];

const BODY_KINDS: [(RigidBodyKind, &str); 3] = [
    (RigidBodyKind::Static, "Static"),
    (RigidBodyKind::Kinematic, "Kinematic"),
    (RigidBodyKind::Dynamic, "Dynamic"),
];

/// Одна строка метки и числового поля; диапазон совпадает с проверкой документа.
fn drag_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    speed: f64,
    range: std::ops::RangeInclusive<f32>,
) -> egui::Response {
    ui.label(label);
    let response = ui.add(egui::DragValue::new(value).speed(speed).range(range));
    ui.end_row();
    response
}

/// Вектор размеров: все компоненты должны быть больше нуля.
fn size_row(ui: &mut egui::Ui, label: &str, value: &mut [f32; 3]) -> egui::Response {
    ui.label(label);
    let response = ui
        .horizontal(|ui| {
            let mut combined: Option<egui::Response> = None;
            for component in value.iter_mut() {
                let field = egui::DragValue::new(component)
                    .speed(0.02)
                    .range(f32::MIN_POSITIVE..=f32::MAX);
                merge_response(&mut combined, ui.add(field));
            }
            combined.expect("вектор содержит три компонента")
        })
        .inner;
    ui.end_row();
    response
}

/// Выбор варианта перечисления. Это дискретная правка: применяется сразу, без объединения.
fn choice_row<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    label: &str,
    current: T,
    options: &[(T, &str)],
    change: impl FnOnce(&mut SceneObject, T),
) {
    ui.label(label);
    let mut chosen = current;
    let current_text = options
        .iter()
        .find(|(value, _)| *value == current)
        .map_or("", |(_, text)| *text);
    egui::ComboBox::from_id_salt((id, label))
        .selected_text(current_text)
        .show_ui(ui, |ui| {
            for (value, text) in options {
                ui.selectable_value(&mut chosen, *value, *text);
            }
        });
    ui.end_row();
    if chosen != current {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                change(object, chosen);
                Ok(())
            })
        });
    }
}

fn show_collider(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    object: &SceneObject,
    collider: ColliderComponent,
) {
    let id = object.id.as_str();
    let field = OverrideField::Collider;
    let menu = component_menu(state, id, ComponentKind::Collider);
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Collider",
            icon: component_icon(state, ComponentKind::Collider),
        },
        id,
        mark(state, id, field.clone()),
        Some(menu),
        true,
        |ui| {
            egui::Grid::new("collider").num_columns(2).show(ui, |ui| {
                choice_row(
                    ui,
                    state,
                    id,
                    "Shape",
                    collider.shape,
                    &COLLIDER_SHAPES,
                    |object, shape| {
                        object.collider = Some(ColliderComponent {
                            shape,
                            ..collider.clone()
                        })
                    },
                );
                let mut value = collider.clone();
                let mut combined: Option<egui::Response> = None;
                match collider.shape {
                    ColliderShape::Box => {
                        let response = size_row(ui, "Half Extents", &mut value.half_extents);
                        merge_response(&mut combined, response);
                    }
                    ColliderShape::Sphere => {
                        let response = drag_row(
                            ui,
                            "Radius",
                            &mut value.radius,
                            0.01,
                            f32::MIN_POSITIVE..=f32::MAX,
                        );
                        merge_response(&mut combined, response);
                    }
                    ColliderShape::Capsule => {
                        let radius = drag_row(
                            ui,
                            "Radius",
                            &mut value.radius,
                            0.01,
                            f32::MIN_POSITIVE..=f32::MAX,
                        );
                        let height = drag_row(
                            ui,
                            "Height",
                            &mut value.height,
                            0.01,
                            f32::MIN_POSITIVE..=f32::MAX,
                        );
                        merge_response(&mut combined, radius | height);
                    }
                    ColliderShape::ConvexMesh | ColliderShape::TriangleMesh => {}
                }
                let offset = vec3_row(ui, "Offset", &mut value.offset, 0.02);
                merge_response(&mut combined, offset);
                ui.label("Trigger");
                let trigger = ui.checkbox(&mut value.trigger, "");
                ui.end_row();
                merge_response(&mut combined, trigger);
                ui.label("Material").on_hover_text(
                    "Physics material asset (.rvephysmat). When assigned, it replaces Friction and Restitution.",
                );
                let mut options = vec!["Inline".to_owned()];
                options.extend(state.assets_of_kind(AssetKind::PhysicsMaterial));
                let current = collider
                    .material
                    .as_ref()
                    .map(|path| path.as_str())
                    .unwrap_or("Inline");
                if let Some(chosen) = asset_picker(ui, "collider_material", current, &options) {
                    let material = if chosen == "Inline" {
                        Some(None)
                    } else {
                        parse_path(state, &chosen).map(Some)
                    };
                    if let Some(material) = material {
                        state.edit_scene(|scene| scene.set_collider_material(id, material));
                    }
                }
                ui.end_row();
                if value.material.is_none() {
                    let friction =
                        drag_row(ui, "Friction", &mut value.friction, 0.01, 0.0..=f32::MAX);
                    merge_response(&mut combined, friction);
                    let restitution =
                        drag_row(ui, "Restitution", &mut value.restitution, 0.01, 0.0..=1.0);
                    merge_response(&mut combined, restitution);
                }
                if let Some(response) = combined {
                    commit(state, id, "collider", &response, |object| {
                        object.collider = Some(value);
                    });
                }
            });
            if let Some(error) = mesh_geometry_error(state, object, collider.shape) {
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("Геометрия коллайдера не строится: {error}"),
                );
            }
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_physics_component(id, ComponentKind::Collider));
    }
    run_menu_action(state, id, ComponentKind::Collider, action);
}

/// Ошибка построения выпуклой оболочки или trimesh по модели; `None`, если форма строится
/// или модель ещё загружается.
fn mesh_geometry_error(
    state: &mut EditorState,
    object: &SceneObject,
    shape: ColliderShape,
) -> Option<String> {
    if !matches!(
        shape,
        ColliderShape::ConvexMesh | ColliderShape::TriangleMesh
    ) {
        return None;
    }
    let path = object.mesh_renderer.as_ref()?.model.as_str().to_owned();
    let model = state.models.get(&path).cloned()?;
    let built = if shape == ColliderShape::ConvexMesh {
        state.collider_cache.hull(&path, &model).map(|_| ())
    } else {
        state.collider_cache.mesh(&path, &model).map(|_| ())
    };
    built.err()
}

fn show_rigid_body(ui: &mut egui::Ui, state: &mut EditorState, id: &str, body: RigidBodyComponent) {
    let field = OverrideField::RigidBody;
    let menu = component_menu(state, id, ComponentKind::RigidBody);
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Rigid Body",
            icon: component_icon(state, ComponentKind::RigidBody),
        },
        id,
        mark(state, id, field.clone()),
        Some(menu),
        true,
        |ui| {
            egui::Grid::new("rigid_body").num_columns(2).show(ui, |ui| {
                choice_row(
                    ui,
                    state,
                    id,
                    "Kind",
                    body.kind,
                    &BODY_KINDS,
                    |object, kind| object.rigid_body = Some(RigidBodyComponent { kind, ..body }),
                );
                let mut value = body;
                let mut combined: Option<egui::Response> = None;
                let mass = drag_row(
                    ui,
                    "Mass",
                    &mut value.mass,
                    0.05,
                    f32::MIN_POSITIVE..=f32::MAX,
                );
                merge_response(&mut combined, mass);
                let linear = drag_row(
                    ui,
                    "Linear Damping",
                    &mut value.linear_damping,
                    0.01,
                    0.0..=f32::MAX,
                );
                merge_response(&mut combined, linear);
                let angular = drag_row(
                    ui,
                    "Angular Damping",
                    &mut value.angular_damping,
                    0.01,
                    0.0..=f32::MAX,
                );
                merge_response(&mut combined, angular);
                let gravity = drag_row(
                    ui,
                    "Gravity Scale",
                    &mut value.gravity_scale,
                    0.01,
                    -f32::MAX..=f32::MAX,
                );
                merge_response(&mut combined, gravity);
                ui.label("Lock Rotation");
                let lock = ui.checkbox(&mut value.lock_rotation, "");
                ui.end_row();
                merge_response(&mut combined, lock);
                if let Some(response) = combined {
                    commit(state, id, "rigid_body", &response, |object| {
                        object.rigid_body = Some(value);
                    });
                }
            });
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_physics_component(id, ComponentKind::RigidBody));
    }
    run_menu_action(state, id, ComponentKind::RigidBody, action);
}

fn show_character_controller(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    controller: CharacterControllerComponent,
) {
    let field = OverrideField::CharacterController;
    let menu = component_menu(state, id, ComponentKind::CharacterController);
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Character Controller",
            icon: component_icon(state, ComponentKind::CharacterController),
        },
        id,
        mark(state, id, field.clone()),
        Some(menu),
        true,
        |ui| {
            egui::Grid::new("character_controller")
                .num_columns(2)
                .show(ui, |ui| {
                    let mut value = controller;
                    let mut combined: Option<egui::Response> = None;
                    let fields: [(&str, &mut f32, f64, std::ops::RangeInclusive<f32>); 9] = [
                        (
                            "Radius",
                            &mut value.radius,
                            0.01,
                            f32::MIN_POSITIVE..=f32::MAX,
                        ),
                        (
                            "Height",
                            &mut value.height,
                            0.01,
                            f32::MIN_POSITIVE..=f32::MAX,
                        ),
                        ("Step Height", &mut value.step_height, 0.01, 0.0..=f32::MAX),
                        ("Max Slope (°)", &mut value.max_slope_deg, 0.5, 0.01..=89.99),
                        (
                            "Snap Distance",
                            &mut value.snap_distance,
                            0.01,
                            0.0..=f32::MAX,
                        ),
                        ("Skin", &mut value.skin, 0.001, 0.0..=f32::MAX),
                        (
                            "Push Strength",
                            &mut value.push_strength,
                            0.05,
                            0.0..=f32::MAX,
                        ),
                        (
                            "Gravity Scale",
                            &mut value.gravity_scale,
                            0.01,
                            0.0..=f32::MAX,
                        ),
                        ("Jump Height", &mut value.jump_height, 0.05, 0.0..=f32::MAX),
                    ];
                    for (label, field, speed, range) in fields {
                        let response = drag_row(ui, label, field, speed, range);
                        merge_response(&mut combined, response);
                    }
                    ui.label("Use Gravity");
                    let use_gravity = ui.checkbox(&mut value.use_gravity, "");
                    ui.end_row();
                    merge_response(&mut combined, use_gravity);
                    if let Some(response) = combined {
                        commit(state, id, "character_controller", &response, |object| {
                            object.character_controller = Some(value);
                        });
                    }
                });
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| {
            scene.remove_physics_component(id, ComponentKind::CharacterController)
        });
    }
    run_menu_action(state, id, ComponentKind::CharacterController, action);
}

fn show_environment(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    environment: &EnvironmentComponent,
) {
    let field = OverrideField::Environment;
    let action = component_header(
        ui,
        HeaderTitle {
            text: "Environment",
            icon: component_icon(state, ComponentKind::Environment),
        },
        id,
        mark(state, id, field.clone()),
        Some(component_menu(state, id, ComponentKind::Environment)),
        true,
        |ui| {
            let mut value = environment.clone();
            let mut procedural = value.procedural.is_some();
            let mut hdr = value
                .hdr
                .as_ref()
                .map(|path| path.as_str().to_owned())
                .unwrap_or_default();
            let mut changed = false;
            let mut hdr_assets = state.assets_of_kind(AssetKind::Hdr);
            if !hdr.is_empty() && !hdr_assets.contains(&hdr) {
                hdr_assets.push(hdr.clone());
            }
            ui.horizontal(|ui| {
                changed |= ui
                    .radio_value(&mut procedural, true, "Procedural")
                    .changed();
                if ui
                    .add_enabled(
                        !hdr_assets.is_empty(),
                        egui::RadioButton::new(!procedural, "Radiance HDR"),
                    )
                    .clicked()
                {
                    procedural = false;
                    changed = true;
                }
                if !procedural && hdr_assets.is_empty() {
                    ui.label("Нет .hdr ресурсов");
                }
            });
            egui::Grid::new("environment_component")
                .num_columns(2)
                .show(ui, |ui| {
                    ui.label("Intensity");
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut value.intensity)
                                .speed(0.05)
                                .range(0.0..=f32::MAX),
                        )
                        .changed();
                    ui.end_row();
                    ui.label("Skybox");
                    changed |= ui.checkbox(&mut value.skybox, "").changed();
                    ui.end_row();
                    if procedural {
                        let sky = value.procedural.get_or_insert(ProceduralSkyComponent {
                            zenith_color: [0.18, 0.32, 0.58],
                            horizon_color: [0.68, 0.74, 0.82],
                            ground_color: [0.12, 0.11, 0.10],
                            sun_size_deg: 1.0,
                            sun_intensity: 1.0,
                        });
                        let (picked, zenith) = color_row(ui, "Zenith", &mut sky.zenith_color);
                        changed |= picked || zenith.changed();
                        let (picked, horizon) = color_row(ui, "Horizon", &mut sky.horizon_color);
                        changed |= picked || horizon.changed();
                        let (picked, ground) = color_row(ui, "Ground", &mut sky.ground_color);
                        changed |= picked || ground.changed();
                        ui.label("Sun Size");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut sky.sun_size_deg)
                                    .speed(0.05)
                                    .range(0.001..=180.0),
                            )
                            .changed();
                        ui.end_row();
                        ui.label("Sun Intensity");
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut sky.sun_intensity)
                                    .speed(0.05)
                                    .range(0.0..=f32::MAX),
                            )
                            .changed();
                        ui.end_row();
                    } else {
                        ui.label("HDR Panorama");
                        if let Some(path) = asset_picker(ui, "environment_hdr", &hdr, &hdr_assets) {
                            hdr = path;
                            changed = true;
                        }
                        ui.end_row();
                    }
                });
            value.procedural =
                procedural.then_some(value.procedural.unwrap_or(ProceduralSkyComponent {
                    zenith_color: [0.18, 0.32, 0.58],
                    horizon_color: [0.68, 0.74, 0.82],
                    ground_color: [0.12, 0.11, 0.10],
                    sun_size_deg: 1.0,
                    sun_intensity: 1.0,
                }));
            value.hdr = if procedural || hdr.is_empty() {
                None
            } else {
                parse_path(state, &hdr)
            };
            if changed {
                value.intensity = if value.intensity.is_finite() {
                    value.intensity.max(0.0)
                } else {
                    0.0
                };
                if let Some(sky) = &mut value.procedural {
                    for color in [
                        &mut sky.zenith_color,
                        &mut sky.horizon_color,
                        &mut sky.ground_color,
                    ] {
                        for channel in color {
                            *channel = if channel.is_finite() {
                                channel.max(0.0)
                            } else {
                                0.0
                            };
                        }
                    }
                    sky.sun_size_deg = sky.sun_size_deg.clamp(0.001, 180.0);
                    sky.sun_intensity = if sky.sun_intensity.is_finite() {
                        sky.sun_intensity.max(0.0)
                    } else {
                        0.0
                    };
                }
                if procedural || value.hdr.is_some() {
                    let key = format!("{id}/environment");
                    state.edit_scene(|scene| {
                        scene.edit_object(id, Some(&key), |object| {
                            object.environment = Some(value);
                            Ok(())
                        })
                    });
                }
            }
        },
    );
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.environment = None;
                Ok(())
            })
        });
    }
    run_menu_action(state, id, ComponentKind::Environment, action);
}

fn show_lite_script(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    object: &SceneObject,
    index: usize,
) {
    let id = object.id.as_str();
    let script = &object.lite_scripts[index];
    let title = format!("Lua: {}", script.path.as_str());
    let schema = state.lite_schema(script.path.as_str());
    let field = OverrideField::LiteParams(script.path.clone());
    let mut params = script.params.clone();
    let mut params_response = None;
    let mut reset = None;
    let mut enabled = script.enabled;
    let mut enabled_response = None;
    let action = component_header(
        ui,
        HeaderTitle {
            text: &title,
            icon: None,
        },
        (id, index),
        mark(state, id, field.clone()),
        None,
        true,
        |ui| {
            enabled_response = Some(ui.checkbox(&mut enabled, "Enabled"));
            match &schema {
                Ok(Some(schema)) => {
                    params_response = schema_params_ui(
                        ui,
                        state,
                        id,
                        ("lite", id, index),
                        schema,
                        &mut params,
                        &mut reset,
                    );
                }
                Ok(None) => params_response = params_ui(ui, ("lite", id, index), &mut params),
                Err(_) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        "Ошибка схемы properties, см. Console",
                    );
                    params_response = params_ui(ui, ("lite", id, index), &mut params);
                }
            }
        },
    );
    if let Some(response) = &enabled_response {
        let key = format!("lite-enabled-{index}");
        commit(state, id, &key, response, |object| {
            object.lite_scripts[index].enabled = enabled;
        });
    }
    if let Some((name, default)) = reset {
        let base_has_param = default.is_some()
            && prefab_base_object(state, id).is_some_and(|base| {
                base.lite_scripts.get(index).is_some_and(|base_script| {
                    base_script.path == script.path && base_script.params.contains_key(&name)
                })
            });
        reset_schema_param(&mut params, &name, default, base_has_param);
    }
    if let Some(response) = &params_response {
        let key = format!("lite-{index}");
        commit_script_params(state, id, &key, response, index, params, true);
    }
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.lite_scripts.remove(index);
                Ok(())
            })
        });
    }
}

fn show_heavy_script(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    object: &SceneObject,
    index: usize,
) {
    let id = object.id.as_str();
    let script = &object.heavy_scripts[index];
    let title = format!("Rust: {}", script.name);
    let schema = state.heavy_schema(&script.name);
    let field = OverrideField::HeavyParams(script.name.clone());
    let mut params = script.params.clone();
    let mut params_response = None;
    let mut reset = None;
    let mut enabled = script.enabled;
    let mut enabled_response = None;
    let action = component_header(
        ui,
        HeaderTitle {
            text: &title,
            icon: None,
        },
        (id, index),
        mark(state, id, field.clone()),
        None,
        true,
        |ui| {
            enabled_response = Some(ui.checkbox(&mut enabled, "Enabled"));
            match &schema {
                Some(schema) => {
                    params_response = schema_params_ui(
                        ui,
                        state,
                        id,
                        ("heavy", id, index),
                        schema,
                        &mut params,
                        &mut reset,
                    );
                }
                None => params_response = params_ui(ui, ("heavy", id, index), &mut params),
            }
        },
    );
    if let Some(response) = &enabled_response {
        let key = format!("heavy-enabled-{index}");
        commit(state, id, &key, response, |object| {
            object.heavy_scripts[index].enabled = enabled;
        });
    }
    if let Some((name, default)) = reset {
        let base_has_param = default.is_some()
            && prefab_base_object(state, id).is_some_and(|base| {
                base.heavy_scripts.get(index).is_some_and(|base_script| {
                    base_script.name == script.name && base_script.params.contains_key(&name)
                })
            });
        reset_schema_param(&mut params, &name, default, base_has_param);
    }
    if let Some(response) = &params_response {
        let key = format!("heavy-{index}");
        commit_script_params(state, id, &key, response, index, params, false);
    }
    if action == HeaderAction::Revert {
        revert(state, id, field);
        return;
    }
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.heavy_scripts.remove(index);
                Ok(())
            })
        });
    }
}

/// Редактор параметров скрипта. Массивы показываются только для чтения.
fn params_ui(
    ui: &mut egui::Ui,
    salt: impl std::hash::Hash + std::fmt::Debug,
    params: &mut BTreeMap<String, ParamValue>,
) -> Option<egui::Response> {
    let mut combined: Option<egui::Response> = None;
    let mut remove = None;
    egui::Grid::new(("params", salt))
        .num_columns(3)
        .show(ui, |ui| {
            for (name, value) in params.iter_mut() {
                ui.label(name.as_str());
                let response = match value {
                    ParamValue::Bool(value) => ui.checkbox(value, ""),
                    ParamValue::Int(value) => ui.add(egui::DragValue::new(value)),
                    ParamValue::Float(value) => ui.add(egui::DragValue::new(value).speed(0.05)),
                    ParamValue::String(value) => ui.text_edit_singleline(value),
                    ParamValue::Array(values) => ui.label(format!("[{} значений]", values.len())),
                    ParamValue::Object(id) => ui.label(id.as_str()),
                    ParamValue::Asset(path) => ui.label(path.as_str()),
                };
                combined = Some(match combined.take() {
                    Some(previous) => previous | response,
                    None => response,
                });
                if ui.small_button("x").clicked() {
                    remove = Some(name.clone());
                }
                ui.end_row();
            }
        });
    let mut structure_changed = false;
    if let Some(name) = remove {
        params.remove(&name);
        structure_changed = true;
    }
    let add_id = ui.make_persistent_id("new_param_name");
    let mut new_name = ui
        .ctx()
        .data(|data| data.get_temp::<String>(add_id))
        .unwrap_or_default();
    let added = ui
        .horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut new_name)
                    .desired_width(100.0)
                    .hint_text("param"),
            );
            ui.menu_button("Add", |ui| {
                let options: [(&str, ParamValue); 4] = [
                    ("Float", ParamValue::Float(0.0)),
                    ("Int", ParamValue::Int(0)),
                    ("Bool", ParamValue::Bool(false)),
                    ("String", ParamValue::String(String::new())),
                ];
                let mut chosen = None;
                for (label, value) in options {
                    if ui.button(label).clicked() {
                        chosen = Some(value);
                    }
                }
                chosen
            })
            .inner
            .flatten()
        })
        .inner;
    if let Some(value) = added
        && !new_name.is_empty()
    {
        params.insert(std::mem::take(&mut new_name), value);
        structure_changed = true;
    }
    ui.ctx().data_mut(|data| data.insert_temp(add_id, new_name));
    if structure_changed {
        let mut response = combined.unwrap_or_else(|| ui.label(""));
        response.mark_changed();
        return Some(response);
    }
    combined
}

fn schema_params_ui(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    owner: &str,
    salt: impl std::hash::Hash + std::fmt::Debug,
    schema: &ParamSchema,
    params: &mut BTreeMap<String, ParamValue>,
    reset: &mut Option<(String, Option<ParamValue>)>,
) -> Option<egui::Response> {
    let mut combined: Option<egui::Response> = None;
    egui::Grid::new(("schema_params", salt))
        .num_columns(3)
        .show(ui, |ui| {
            for spec in schema.specs() {
                let stored = params.get(&spec.name).cloned();
                let label = spec.label.as_deref().unwrap_or(&spec.name);
                let label = match stored {
                    Some(_) => egui::RichText::new(label),
                    None => egui::RichText::new(label).weak(),
                };
                ui.label(label).on_hover_text(&spec.name);
                let current = stored.as_ref().map(|value| spec.coerce(value));
                let mut value = match &current {
                    Some(Ok(value)) => value.clone(),
                    Some(Err(_)) | None => spec.default.clone(),
                };
                let response = param_widget(ui, state, owner, spec, &mut value);
                if response.changed() {
                    let empty_reference = matches!(
                        (&spec.kind, &value),
                        (
                            ParamKind::Object | ParamKind::Component(_),
                            ParamValue::Object(id)
                        ) if id.is_empty()
                    ) || matches!(
                        (&spec.kind, &value),
                        (ParamKind::Asset(_), ParamValue::String(path)) if path.is_empty()
                    );
                    if empty_reference {
                        params.remove(&spec.name);
                    } else {
                        params.insert(spec.name.clone(), value);
                    }
                }
                merge_response(&mut combined, response);
                ui.horizontal(|ui| {
                    if stored.is_some() && ui.small_button("↺").on_hover_text("Default").clicked()
                    {
                        *reset = Some((spec.name.clone(), Some(spec.default.clone())));
                    }
                    if let Some(Err(error)) = &current {
                        ui.colored_label(ui.visuals().error_fg_color, "⚠")
                            .on_hover_text(error);
                    }
                });
                ui.end_row();
            }
            for name in params.keys() {
                if schema.get(name).is_some() {
                    continue;
                }
                ui.colored_label(ui.visuals().warn_fg_color, format!("⚠ {name}"))
                    .on_hover_text("Параметр не объявлен скриптом");
                ui.label("не объявлен");
                if ui.small_button("x").clicked() {
                    *reset = Some((name.clone(), None));
                }
                ui.end_row();
            }
        });
    if reset.is_some() {
        let mut response = combined.unwrap_or_else(|| ui.label(""));
        response.mark_changed();
        return Some(response);
    }
    combined
}

fn prefab_base_object(state: &EditorState, id: &str) -> Option<SceneObject> {
    let scene = state.scene.as_ref()?;
    let origin = scene.origin(id)?;
    scene.base_object(&origin.instance, id).ok()
}

fn reset_schema_param(
    params: &mut BTreeMap<String, ParamValue>,
    name: &str,
    default: Option<ParamValue>,
    base_has_param: bool,
) {
    if base_has_param && let Some(default) = default {
        params.insert(name.to_owned(), default);
    } else {
        params.remove(name);
    }
}

fn integer_bounds(min: f64, max: f64) -> Option<std::ops::RangeInclusive<i64>> {
    const I64_LOWER: f64 = -9_223_372_036_854_775_808.0;
    const I64_UPPER: f64 = 9_223_372_036_854_775_808.0;

    let lower = if min == f64::NEG_INFINITY {
        i64::MIN
    } else {
        let min = min.ceil();
        if !min.is_finite() || min >= I64_UPPER {
            return None;
        }
        if min <= I64_LOWER {
            i64::MIN
        } else {
            min as i64
        }
    };
    let upper = if max == f64::INFINITY {
        i64::MAX
    } else {
        let max = max.floor();
        if !max.is_finite() || max < I64_LOWER {
            return None;
        }
        if max >= I64_UPPER {
            i64::MAX
        } else {
            max as i64
        }
    };
    (lower <= upper).then_some(lower..=upper)
}

fn param_widget(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    owner: &str,
    spec: &ParamSpec,
    value: &mut ParamValue,
) -> egui::Response {
    match &spec.kind {
        ParamKind::Object | ParamKind::Component(_) | ParamKind::Asset(_) => {
            return ref_field(ui, state, owner, spec, value);
        }
        ParamKind::Enum(values) => {
            let ParamValue::String(current) = value else {
                return ui.label("некорректное enum значение");
            };
            let mut changed = false;
            let mut response = egui::ComboBox::from_id_salt((owner, &spec.name, "enum"))
                .selected_text(current.as_str())
                .show_ui(ui, |ui| {
                    for option in values {
                        if ui.selectable_label(option == current, option).clicked() {
                            *current = option.clone();
                            changed = true;
                        }
                    }
                })
                .response;
            if changed {
                response.mark_changed();
            }
            return response;
        }
        ParamKind::Color => {
            let ParamValue::Array(values) = value else {
                return ui.label("некорректный цвет");
            };
            if values.len() != 3 {
                *values = vec![ParamValue::Float(0.0); 3];
            }
            let mut rgb = std::array::from_fn(|index| match &values[index] {
                ParamValue::Float(value) => *value as f32,
                ParamValue::Int(value) => *value as f32,
                _ => 0.0,
            });
            let mut combined = None;
            let picked = ui.color_edit_button_rgb(&mut rgb).changed();
            if picked {
                for (target, channel) in values.iter_mut().zip(rgb) {
                    *target = ParamValue::Float(channel as f64);
                }
            }
            for (index, channel) in rgb.iter_mut().enumerate() {
                let response = ui.add(egui::DragValue::new(channel).speed(0.01).range(0.0..=1.0));
                if response.changed() {
                    values[index] = ParamValue::Float(*channel as f64);
                }
                merge_response(&mut combined, response);
            }
            let mut response = combined.unwrap_or_else(|| ui.label(""));
            if picked {
                response.mark_changed();
            }
            return response;
        }
        ParamKind::Array(element) => {
            let ParamValue::Array(values) = value else {
                return ui.label("некорректный массив");
            };
            let mut combined = None;
            let mut remove = None;
            ui.vertical(|ui| {
                for (index, value) in values.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        let element_spec = ParamSpec {
                            name: format!("{}[{index}]", spec.name),
                            kind: (**element).clone(),
                            default: element.zero(),
                            min: None,
                            max: None,
                            label: None,
                        };
                        merge_response(
                            &mut combined,
                            param_widget(ui, state, owner, &element_spec, value),
                        );
                        if ui.small_button("−").clicked() {
                            remove = Some(index);
                        }
                    });
                }
                ui.horizontal(|ui| {
                    let mut add = ui.small_button("+").on_hover_text("Добавить элемент");
                    if add.clicked() {
                        values.push(element.zero());
                    }
                    if add.clicked() || remove.is_some() {
                        add.mark_changed();
                    }
                    merge_response(&mut combined, add);
                });
            });
            if let Some(index) = remove {
                values.remove(index);
                if let Some(response) = &mut combined {
                    response.mark_changed();
                }
            }
            return combined.unwrap_or_else(|| ui.label(""));
        }
        _ => {}
    }
    let min = spec.min.unwrap_or(f64::NEG_INFINITY);
    let max = spec.max.unwrap_or(f64::INFINITY);
    match value {
        ParamValue::Bool(value) => ui.checkbox(value, ""),
        ParamValue::Int(value) => match integer_bounds(min, max) {
            Some(bounds) => ui.add(egui::DragValue::new(value).range(bounds)),
            None => ui.label("нет целых значений в диапазоне"),
        },
        ParamValue::Float(value) => {
            ui.add(egui::DragValue::new(value).speed(0.05).range(min..=max))
        }
        ParamValue::String(value) => ui.text_edit_singleline(value),
        ParamValue::Array(values) => {
            ui.horizontal(|ui| {
                let mut combined: Option<egui::Response> = None;
                for value in values.iter_mut() {
                    if let ParamValue::Float(value) = value {
                        merge_response(
                            &mut combined,
                            ui.add(egui::DragValue::new(value).speed(0.05)),
                        );
                    }
                }
                combined.unwrap_or_else(|| ui.label(""))
            })
            .inner
        }
        ParamValue::Object(id) => ui.label(id.as_str()),
        ParamValue::Asset(path) => ui.label(path.as_str()),
    }
}

fn ref_field(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    owner: &str,
    spec: &ParamSpec,
    value: &mut ParamValue,
) -> egui::Response {
    let (current, icon, mut missing) = match (&spec.kind, &*value) {
        (ParamKind::Object | ParamKind::Component(_), ParamValue::Object(id)) => {
            let target = (!id.is_empty())
                .then(|| state.scene.as_ref()?.object(id))
                .flatten();
            let valid = target.is_some_and(|object| reference_matches(&spec.kind, object));
            (Some(id.clone()), "◉", !id.is_empty() && !valid)
        }
        (ParamKind::Asset(_), ParamValue::Asset(path)) => {
            (Some(path.as_str().to_owned()), "▧", false)
        }
        (ParamKind::Asset(_), ParamValue::String(path)) => (Some(path.clone()), "▧", false),
        _ => (None, "?", true),
    };
    let asset_kind = match &spec.kind {
        ParamKind::Asset(kind) => *kind,
        _ => None,
    };
    if matches!(&spec.kind, ParamKind::Asset(_))
        && let Some(path) = current.as_deref().filter(|path| !path.is_empty())
    {
        missing = !all_assets(&state.asset_tree)
            .iter()
            .any(|asset| asset == path);
    }
    let target_text = match &spec.kind {
        ParamKind::Object | ParamKind::Component(_) => current
            .as_deref()
            .filter(|id| !id.is_empty())
            .and_then(|id| state.scene.as_ref()?.object(id))
            .map(|object| object.display_name().to_owned())
            .unwrap_or_else(|| {
                current
                    .as_deref()
                    .filter(|id| !id.is_empty())
                    .map_or_else(|| "None".to_owned(), str::to_owned)
            }),
        ParamKind::Asset(_) => current
            .as_deref()
            .filter(|path| !path.is_empty())
            .map_or_else(|| "None".to_owned(), str::to_owned),
        _ => String::new(),
    };
    let target_text = if missing {
        format!("Missing: {target_text}")
    } else {
        target_text
    };
    let button_text = if missing {
        egui::RichText::new(format!("{icon} {target_text}")).color(ui.visuals().error_fg_color)
    } else {
        egui::RichText::new(format!("{icon} {target_text}"))
    };
    let field = ui.add_sized(
        [
            ui.available_width().min(180.0),
            ui.spacing().interact_size.y,
        ],
        egui::Button::new(button_text).sense(egui::Sense::click_and_drag()),
    );
    if field.clicked() {
        match &spec.kind {
            ParamKind::Object | ParamKind::Component(_) => {
                if let Some(id) = current.as_deref().filter(|id| !id.is_empty())
                    && let Some(scene) = &mut state.scene
                {
                    scene.select(Some(id));
                }
            }
            ParamKind::Asset(_) => {
                if let Some(path) = current.as_deref().filter(|path| !path.is_empty()) {
                    navigate_asset(state, path);
                }
            }
            _ => {}
        }
    }
    let object_payload = field.dnd_hover_payload::<ObjectDrag>().map(|payload| {
        let valid = matches!(&spec.kind, ParamKind::Object | ParamKind::Component(_))
            && state
                .scene
                .as_ref()
                .and_then(|scene| scene.object(&payload.0))
                .is_some_and(|object| reference_matches(&spec.kind, object));
        (payload.0.clone(), valid)
    });
    let asset_payload = field.dnd_hover_payload::<AssetDrag>().map(|payload| {
        let valid = matches!(&spec.kind, ParamKind::Asset(_))
            && asset_kind.is_none_or(|kind| AssetKind::of(&payload.0) == kind);
        (payload.0.clone(), valid)
    });
    if let Some((_, valid)) = object_payload.as_ref().or(asset_payload.as_ref()) {
        ui.painter().rect_stroke(
            field.rect,
            3.0,
            egui::Stroke::new(
                1.5,
                if *valid {
                    ui.visuals().selection.stroke.color
                } else {
                    ui.visuals().error_fg_color
                },
            ),
            egui::StrokeKind::Inside,
        );
    }

    let mut selected = None;
    ui.push_id((owner, &spec.name), |ui| {
        ui.menu_button("⊙", |ui| match &spec.kind {
            ParamKind::Object | ParamKind::Component(_) => {
                let objects = state
                    .scene
                    .as_ref()
                    .map(|scene| scene.view().objects.clone())
                    .unwrap_or_default();
                for object in objects
                    .iter()
                    .filter(|object| reference_matches(&spec.kind, object))
                {
                    if ui
                        .selectable_label(
                            current.as_deref() == Some(object.id.as_str()),
                            object.display_name(),
                        )
                        .on_hover_text(&object.id)
                        .clicked()
                    {
                        selected = Some(ParamValue::Object(object.id.clone()));
                    }
                }
            }
            ParamKind::Asset(_) => {
                let assets = asset_kind.map_or_else(
                    || all_assets(&state.asset_tree),
                    |kind| state.assets_of_kind(kind),
                );
                for path in assets {
                    if ui
                        .selectable_label(current.as_deref() == Some(path.as_str()), &path)
                        .clicked()
                    {
                        selected = VirtualPath::parse(&path).ok().map(ParamValue::Asset);
                    }
                }
            }
            _ => {}
        })
    });
    let clear = ui.small_button("✕").on_hover_text("Очистить ссылку");
    if clear.clicked() {
        selected = Some(match &spec.kind {
            ParamKind::Asset(_) => ParamValue::String(String::new()),
            _ => ParamValue::Object(String::new()),
        });
    }
    if let Some((path, true)) = asset_payload {
        selected = VirtualPath::parse(&path).ok().map(ParamValue::Asset);
    }
    if let Some((id, true)) = object_payload {
        selected = Some(ParamValue::Object(id));
    }
    let mut response = field | clear;
    if let Some(selected) = selected {
        if let ParamValue::Asset(path) = &selected {
            navigate_asset(state, path.as_str());
        }
        *value = selected;
        response.mark_changed();
    }
    response
}

fn reference_matches(kind: &ParamKind, object: &SceneObject) -> bool {
    let Some(component) = (match kind {
        ParamKind::Object => return true,
        ParamKind::Component(component) => Some(component),
        _ => None,
    }) else {
        return false;
    };
    match component {
        ScriptComponentKind::Transform => true,
        ScriptComponentKind::Camera => object.camera.is_some(),
        ScriptComponentKind::MeshRenderer => object.mesh_renderer.is_some(),
        ScriptComponentKind::LodGroup => object.lod_group.is_some(),
        ScriptComponentKind::DirectionalLight => object.directional_light.is_some(),
        ScriptComponentKind::PointLight => object.point_light.is_some(),
        ScriptComponentKind::SpotLight => object.spot_light.is_some(),
        ScriptComponentKind::Environment => object.environment.is_some(),
        ScriptComponentKind::Collider => object.collider.is_some(),
        ScriptComponentKind::RigidBody => object.rigid_body.is_some(),
        ScriptComponentKind::CharacterController => object.character_controller.is_some(),
        ScriptComponentKind::AudioSource => object.audio_source.is_some(),
        ScriptComponentKind::Animator => object.animator.is_some(),
        ScriptComponentKind::ParticleEmitter => object.particle_emitter.is_some(),
        ScriptComponentKind::NetworkIdentity => object.network_identity.is_some(),
        ScriptComponentKind::NetworkTransform => object.network_transform.is_some(),
        ScriptComponentKind::UiCanvas => object.ui_canvas.is_some(),
        ScriptComponentKind::Script(script) => {
            object
                .lite_scripts
                .iter()
                .any(|item| item.path.as_str() == script)
                || object.heavy_scripts.iter().any(|item| item.name == *script)
        }
    }
}

fn all_assets(entries: &[crate::assets_fs::AssetEntry]) -> Vec<String> {
    let mut paths = Vec::new();
    for entry in entries {
        if entry.is_dir {
            paths.extend(all_assets(&entry.children));
        } else {
            paths.push(entry.path.clone());
        }
    }
    paths
}

fn navigate_asset(state: &mut EditorState, path: &str) {
    state.project_selection = Some(path.to_owned());
    state.current_folder = path
        .rsplit_once('/')
        .map_or_else(String::new, |(folder, _)| folder.to_owned());
}

fn show_add_component(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let id = object.id.as_str();
    ui.menu_button("Add Component", |ui| {
        if object.camera.is_none() && ui.button("Camera").clicked() {
            state.edit_scene(|scene| scene.add_camera(id));
        }
        if object.mesh_renderer.is_none() {
            ui.menu_button("Mesh Renderer", |ui| {
                for model in state.assets_of_kind(AssetKind::Model) {
                    if ui.button(&model).clicked()
                        && let Some(path) = parse_path(state, &model)
                        && state
                            .edit_scene(|scene| scene.set_mesh(id, path, None))
                            .is_some()
                    {
                        state.pending_frame = Some(id.to_owned());
                    }
                }
            });
        }
        if object.mesh_renderer.is_some() && object.lod_group.is_none() {
            ui.menu_button("LOD Group", |ui| {
                for model in state.assets_of_kind(AssetKind::Model) {
                    if ui.button(&model).clicked()
                        && let Some(path) = parse_path(state, &model)
                    {
                        state.edit_scene(|scene| {
                            scene.set_lod_group(
                                id,
                                Some(LodGroup {
                                    levels: vec![LodLevel {
                                        model: path.as_str().to_owned(),
                                        texture: None,
                                        distance: 10.0,
                                    }],
                                    cull_distance: None,
                                }),
                            )
                        });
                    }
                }
            });
        }
        if object.directional_light.is_none() && ui.button("Directional Light").clicked() {
            state.edit_scene(|scene| {
                scene.edit_object(id, None, |object| {
                    object.directional_light = Some(default_light());
                    Ok(())
                })
            });
        }
        // Тело требует коллайдер, а контроллер — отсутствия коллайдера и тела; документ проверяет
        // это же, меню просто не предлагает заведомо неверные варианты.
        if object.collider.is_none()
            && object.character_controller.is_none()
            && ui.button("Collider").clicked()
        {
            state.edit_scene(|scene| {
                scene.set_component(id, Component::Collider(ColliderComponent::default()))
            });
        }
        if object.collider.is_some()
            && object.rigid_body.is_none()
            && ui.button("Rigid Body").clicked()
        {
            state.edit_scene(|scene| {
                scene.set_component(id, Component::RigidBody(RigidBodyComponent::default()))
            });
        }
        if object.collider.is_none()
            && object.rigid_body.is_none()
            && object.character_controller.is_none()
            && ui.button("Character Controller").clicked()
        {
            state.edit_scene(|scene| {
                scene.set_component(
                    id,
                    Component::CharacterController(CharacterControllerComponent::default()),
                )
            });
        }
        if object.particle_emitter.is_none() && ui.button("Particle Emitter").clicked() {
            state.edit_scene(|scene| {
                scene.set_component(
                    id,
                    Component::ParticleEmitter(engine_scene::ParticleEmitterComponent::default()),
                )
            });
        }
        if object.audio_source.is_none() {
            ui.menu_button("Audio Source", |ui| {
                let clips = state.assets_of_kind(AssetKind::Audio);
                if clips.is_empty() {
                    ui.label("Нет .wav или .ogg в проекте");
                }
                for clip in clips {
                    if ui.button(&clip).clicked()
                        && let Some(path) = parse_path(state, &clip)
                    {
                        state.edit_scene(|scene| {
                            scene.set_component(
                                id,
                                Component::AudioSource(AudioSourceComponent::with_clip(path)),
                            )
                        });
                    }
                }
            });
        }
        if object.network_identity.is_none() && ui.button("Network Identity").clicked() {
            state.edit_scene(|scene| {
                scene.set_component(
                    id,
                    Component::NetworkIdentity(NetworkIdentityComponent::default()),
                )
            });
        }
        if object.network_transform.is_none() {
            let has_identity = object.network_identity.is_some();
            if ui
                .add_enabled(has_identity, egui::Button::new("Network Transform"))
                .on_disabled_hover_text("Сначала добавьте Network Identity")
                .clicked()
            {
                state.edit_scene(|scene| {
                    scene.set_component(
                        id,
                        Component::NetworkTransform(NetworkTransformComponent::default()),
                    )
                });
            }
        }
        if object.ui_canvas.is_none() {
            ui.menu_button("UI Canvas", |ui| {
                let paths = state.assets_of_kind(AssetKind::Ui);
                if paths.is_empty() {
                    ui.label("Нет .rveui в проекте");
                }
                for path in paths {
                    if ui.button(&path).clicked()
                        && let Some(document) = parse_path(state, &path)
                    {
                        state.edit_scene(|scene| {
                            scene.set_component(
                                id,
                                Component::UiCanvas(engine_scene::UiCanvasComponent {
                                    document,
                                    visible: true,
                                    layer: None,
                                }),
                            )
                        });
                    }
                }
            });
        }
        if object.mesh_renderer.is_some()
            && object.animator.is_none()
            && ui.button("Animator").clicked()
        {
            state.edit_scene(|scene| {
                scene.set_component(
                    id,
                    Component::Animator(engine_scene::AnimatorComponent::new()),
                )
            });
        }
        if object.point_light.is_none() && ui.button("Point Light").clicked() {
            state.edit_scene(|scene| {
                scene.set_component(id, Component::PointLight(PointLightComponent::default()))
            });
        }
        if object.spot_light.is_none() && ui.button("Spot Light").clicked() {
            state.edit_scene(|scene| {
                scene.set_component(id, Component::SpotLight(SpotLightComponent::default()))
            });
        }
        if object.environment.is_none() {
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
                state.edit_scene(|scene| {
                    scene.set_component(id, Component::Environment(default_environment()))
                });
            }
        }
        ui.menu_button("Lua Script", |ui| {
            for script in state.assets_of_kind(AssetKind::Script) {
                if ui.button(&script).clicked()
                    && let Some(path) = parse_path(state, &script)
                {
                    state.edit_scene(|scene| scene.add_lite_script(id, path));
                }
            }
        });
        ui.menu_button("Rust Script", |ui| {
            let name_id = ui.make_persistent_id("heavy_script_name");
            let mut name = ui
                .ctx()
                .data(|data| data.get_temp::<String>(name_id))
                .unwrap_or_default();
            ui.text_edit_singleline(&mut name);
            if ui.button("Add").clicked() {
                let script = std::mem::take(&mut name);
                state.edit_scene(|scene| {
                    scene.edit_object(id, None, |object| {
                        object.heavy_scripts.push(HeavyScript {
                            name: script,
                            params: BTreeMap::new(),
                            enabled: true,
                        });
                        Ok(())
                    })
                });
            }
            ui.ctx().data_mut(|data| data.insert_temp(name_id, name));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_edit::{EditorScene, NewObject};

    fn state() -> EditorState {
        let mut state = EditorState::new(None);
        state.scene = Some(EditorScene::new_untitled().unwrap());
        state
    }

    fn camera(state: &EditorState, id: &str) -> CameraComponent {
        state
            .scene
            .as_ref()
            .unwrap()
            .object(id)
            .unwrap()
            .camera
            .clone()
            .unwrap()
    }

    #[test]
    fn copied_component_pastes_only_into_the_same_kind_and_reset_restores_defaults() {
        let mut state = state();
        state.edit_scene(|scene| {
            scene.set_camera_settings(
                "camera",
                Some(engine_files::VirtualPath::parse("/cameras/a.camerasettings").unwrap()),
            )
        });
        let other = state
            .edit_scene(|scene| scene.create_object(&NewObject::Camera, None))
            .unwrap();
        assert!(!component_menu(&state, &other, ComponentKind::Camera).paste);
        run_menu_action(
            &mut state,
            "camera",
            ComponentKind::Camera,
            HeaderAction::Copy,
        );
        assert!(component_menu(&state, &other, ComponentKind::Camera).paste);
        assert!(!component_menu(&state, &other, ComponentKind::Transform).paste);

        run_menu_action(
            &mut state,
            &other,
            ComponentKind::Camera,
            HeaderAction::Paste,
        );
        assert_eq!(
            camera(&state, &other)
                .settings
                .as_ref()
                .map(|path| path.as_str()),
            Some("cameras/a.camerasettings")
        );
        assert!(!camera(&state, &other).active);
        // Буфер другого вида не меняет компонент.
        run_menu_action(
            &mut state,
            &other,
            ComponentKind::Transform,
            HeaderAction::Paste,
        );
        run_menu_action(
            &mut state,
            &other,
            ComponentKind::Camera,
            HeaderAction::Reset,
        );
        assert!(camera(&state, &other).settings.is_none());
        assert!(state.scene.as_mut().unwrap().undo());
        assert!(camera(&state, &other).settings.is_some());
    }

    #[test]
    fn camera_antialiasing_picker_covers_every_serialized_mode() {
        assert_eq!(
            ANTI_ALIASING_OPTIONS.map(|(value, _)| value),
            [
                AntiAliasing::Fxaa,
                AntiAliasing::None,
                AntiAliasing::Taa,
                AntiAliasing::Fsr,
                AntiAliasing::Dlss,
            ]
        );
        assert_eq!(anti_aliasing_label(AntiAliasing::Dlss), "DLSS");
    }

    fn material_path(value: &str) -> Option<VirtualPath> {
        Some(VirtualPath::parse(value).unwrap())
    }

    #[test]
    fn picking_a_slot_pads_the_list_to_the_part_count_and_keeps_other_slots() {
        let current = [None, material_path("/materials/a.rvemat")];
        let picked = materials_with_choice(&current, 3, 0, material_path("/materials/b.rvemat"));
        assert_eq!(
            picked,
            [
                material_path("/materials/b.rvemat"),
                material_path("/materials/a.rvemat"),
                None
            ]
        );
    }

    #[test]
    fn choosing_none_restores_the_fbx_material_of_that_part() {
        let current = [material_path("/materials/a.rvemat")];
        assert_eq!(
            materials_with_choice(&current, 1, 0, None),
            [None::<VirtualPath>]
        );
    }
}

#[cfg(test)]
mod schema_tests {
    use super::{integer_bounds, reset_schema_param};
    use engine_scene::ParamValue;
    use std::collections::BTreeMap;

    #[test]
    fn integer_bounds_round_fractional_limits_inward_and_handle_empty_ranges() {
        assert_eq!(integer_bounds(1.2, 2.2), Some(2..=2));
        assert_eq!(integer_bounds(-2.2, -1.2), Some(-2..=-2));
        assert_eq!(integer_bounds(1.2, 1.8), None);
        assert_eq!(
            integer_bounds(f64::NEG_INFINITY, f64::INFINITY),
            Some(i64::MIN..=i64::MAX)
        );
    }

    #[test]
    fn reset_keeps_schema_default_when_base_prefab_has_the_key() {
        let mut params = BTreeMap::from([("speed".to_owned(), ParamValue::Int(12))]);
        reset_schema_param(&mut params, "speed", Some(ParamValue::Int(30)), true);
        assert_eq!(params["speed"], ParamValue::Int(30));

        reset_schema_param(&mut params, "speed", Some(ParamValue::Int(30)), false);
        assert!(!params.contains_key("speed"));
    }
}

/// Экранные шейдеры камеры: до четырёх `.glsl` с порядком, стадией, включением и параметрами.
/// Каждая правка идёт через `edit_scene`, поэтому откатывается отменой; ошибка компиляции из
/// горячей перезагрузки показывается под шейдером.
fn show_screen_shaders(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    id: &str,
    shaders: &[engine_scene::ScreenShaderRef],
) {
    egui::CollapsingHeader::new("Screen Shaders")
        .id_salt("camera_screen_shaders")
        .default_open(false)
        .show(ui, |ui| {
            let options = state.assets_of_kind(AssetKind::ScreenShader);
            let mut next = shaders.to_vec();
            let mut other_change = false;
            let mut param_key: Option<String> = None;
            let mut remove = None;
            let mut swap = None;
            for (index, shader) in shaders.iter().enumerate() {
                ui.push_id(index, |ui| {
                    ui.horizontal(|ui| {
                        let current = shader.shader.as_str().to_owned();
                        if let Some(chosen) =
                            asset_picker(ui, "screen_shader_asset", &current, &options)
                            && let Some(path) = parse_path(state, &chosen)
                        {
                            next[index].shader = path;
                            other_change = true;
                        }
                        let mut stage = shader.stage;
                        egui::ComboBox::from_id_salt("screen_shader_stage")
                            .selected_text(screen_stage_label(stage))
                            .show_ui(ui, |ui| {
                                for value in [
                                    engine_scene::ScreenShaderStage::BeforePost,
                                    engine_scene::ScreenShaderStage::AfterPost,
                                ] {
                                    ui.selectable_value(
                                        &mut stage,
                                        value,
                                        screen_stage_label(value),
                                    );
                                }
                            });
                        if stage != shader.stage {
                            next[index].stage = stage;
                            other_change = true;
                        }
                        let mut enabled = shader.enabled;
                        if ui.checkbox(&mut enabled, "On").changed() {
                            next[index].enabled = enabled;
                            other_change = true;
                        }
                        if ui.add_enabled(index > 0, egui::Button::new("Up")).clicked() {
                            swap = Some((index, index - 1));
                        }
                        if ui
                            .add_enabled(index + 1 < shaders.len(), egui::Button::new("Down"))
                            .clicked()
                        {
                            swap = Some((index, index + 1));
                        }
                        if ui.button("Remove").clicked() {
                            remove = Some(index);
                        }
                    });
                    if let Some(error) = state.screen_shader_errors.get(shader.shader.as_str()) {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                    egui::CollapsingHeader::new("Parameters")
                        .id_salt("screen_shader_params")
                        .default_open(false)
                        .show(ui, |ui| {
                            egui::Grid::new("screen_shader_grid")
                                .num_columns(4)
                                .show(ui, |ui| {
                                    for parameter in 0..engine_scene::MAX_SCREEN_SHADER_PARAMS {
                                        ui.label(format!("p{parameter}"));
                                        let mut value =
                                            shader.params.get(parameter).copied().unwrap_or(0.0);
                                        let response =
                                            ui.add(egui::DragValue::new(&mut value).speed(0.01));
                                        if response.changed() {
                                            let params = &mut next[index].params;
                                            params.resize(
                                                engine_scene::MAX_SCREEN_SHADER_PARAMS,
                                                0.0,
                                            );
                                            params[parameter] = value;
                                            param_key = Some(format!(
                                                "screen-shader-{id}-{index}-p{parameter}"
                                            ));
                                        }
                                        finish_edit(state, &response);
                                        if parameter % 4 == 3 {
                                            ui.end_row();
                                        }
                                    }
                                });
                        });
                });
            }
            if let Some(index) = remove {
                next.remove(index);
                other_change = true;
            }
            if let Some((from, to)) = swap {
                next.swap(from, to);
                other_change = true;
            }
            if shaders.len() < engine_scene::MAX_SCREEN_SHADERS {
                match options.first().and_then(|path| parse_path(state, path)) {
                    Some(path) => {
                        if ui.button("Add Screen Shader").clicked() {
                            next.push(engine_scene::ScreenShaderRef {
                                shader: path,
                                stage: engine_scene::ScreenShaderStage::AfterPost,
                                enabled: true,
                                params: Vec::new(),
                            });
                            other_change = true;
                        }
                    }
                    None => {
                        ui.label("Add a .glsl asset to the project to use screen shaders.");
                    }
                }
            }
            if other_change || param_key.is_some() {
                // Только правка одного параметра объединяется с соседними шагами перетаскивания.
                let coalesce = if other_change {
                    None
                } else {
                    param_key.as_deref()
                };
                state.edit_scene(|scene| scene.set_camera_screen_shaders(id, next, coalesce));
            }
        });
}

fn screen_stage_label(stage: engine_scene::ScreenShaderStage) -> &'static str {
    match stage {
        engine_scene::ScreenShaderStage::BeforePost => "Before post",
        engine_scene::ScreenShaderStage::AfterPost => "After post",
    }
}

/// Редактор `.postprocess` под камерой: секции по эффектам, черновик хранится до кнопки Save.
fn show_post_process_editor(ui: &mut egui::Ui, state: &mut EditorState, path: &VirtualPath) {
    let Some(cached) = state
        .project
        .as_ref()
        .and_then(|project| project.cached_post_process(path.as_str()))
    else {
        ui.label("Файл постобработки не загружен");
        return;
    };
    let draft_id = egui::Id::new(("post_process_draft", path.as_str()));
    let mut draft = ui
        .data(|data| data.get_temp::<engine_scene::PostProcessSettings>(draft_id))
        .unwrap_or(cached);
    let mut save = false;
    let mut revert = false;
    egui::CollapsingHeader::new("Post Process")
        .id_salt(draft_id)
        .default_open(false)
        .show(ui, |ui| {
            post_process_sections(ui, &mut draft);
            ui.horizontal(|ui| {
                save = ui
                    .add_enabled(draft != cached, egui::Button::new("Save"))
                    .clicked();
                revert = ui
                    .add_enabled(draft != cached, egui::Button::new("Revert"))
                    .clicked();
            });
        });
    if save {
        if state.save_post_process(path, draft) {
            ui.data_mut(|data| data.remove::<engine_scene::PostProcessSettings>(draft_id));
        }
    } else if revert {
        ui.data_mut(|data| data.remove::<engine_scene::PostProcessSettings>(draft_id));
    } else {
        ui.data_mut(|data| data.insert_temp(draft_id, draft));
    }
}

/// Все секции эффектов; поля связаны с полями `PostProcessSettings`.
fn post_process_sections(ui: &mut egui::Ui, post: &mut engine_scene::PostProcessSettings) {
    use engine_scene::{AmbientOcclusionQuality, ExposureMode, Tonemapper};
    egui::CollapsingHeader::new("Exposure")
        .id_salt("pp_exposure")
        .show(ui, |ui| {
            ui.checkbox(&mut post.exposure.enabled, "Enabled");
            ui.horizontal(|ui| {
                ui.label("Mode");
                egui::ComboBox::from_id_salt("pp_exposure_mode")
                    .selected_text(match post.exposure.mode {
                        ExposureMode::Manual => "Manual",
                        ExposureMode::Auto => "Auto",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut post.exposure.mode,
                            ExposureMode::Manual,
                            "Manual",
                        );
                        ui.selectable_value(&mut post.exposure.mode, ExposureMode::Auto, "Auto");
                    });
            });
            drag_f32(ui, "EV", &mut post.exposure.ev, -16.0..=16.0);
            drag_f32(ui, "Min EV", &mut post.exposure.min_ev, -16.0..=16.0);
            drag_f32(ui, "Max EV", &mut post.exposure.max_ev, -16.0..=16.0);
            drag_f32(ui, "Speed up", &mut post.exposure.speed_up, 0.01..=20.0);
            drag_f32(ui, "Speed down", &mut post.exposure.speed_down, 0.01..=20.0);
            drag_f32(
                ui,
                "Low percent",
                &mut post.exposure.low_percent,
                0.0..=100.0,
            );
            drag_f32(
                ui,
                "High percent",
                &mut post.exposure.high_percent,
                0.0..=100.0,
            );
        });
    egui::CollapsingHeader::new("Tonemapping")
        .id_salt("pp_tonemapping")
        .show(ui, |ui| {
            ui.checkbox(&mut post.tonemapping.enabled, "Enabled");
            ui.horizontal(|ui| {
                ui.label("Tonemapper");
                egui::ComboBox::from_id_salt("pp_tonemapper")
                    .selected_text(match post.tonemapping.tonemapper {
                        Tonemapper::Aces => "ACES",
                        Tonemapper::Neutral => "Neutral",
                        Tonemapper::None => "None",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut post.tonemapping.tonemapper,
                            Tonemapper::Aces,
                            "ACES",
                        );
                        ui.selectable_value(
                            &mut post.tonemapping.tonemapper,
                            Tonemapper::Neutral,
                            "Neutral",
                        );
                        ui.selectable_value(
                            &mut post.tonemapping.tonemapper,
                            Tonemapper::None,
                            "None",
                        );
                    });
            });
        });
    egui::CollapsingHeader::new("Bloom")
        .id_salt("pp_bloom")
        .show(ui, |ui| {
            ui.checkbox(&mut post.bloom.enabled, "Enabled");
            drag_f32(ui, "Intensity", &mut post.bloom.intensity, 0.0..=16.0);
            drag_f32(ui, "Threshold", &mut post.bloom.threshold, 0.0..=16.0);
            drag_f32(ui, "Knee", &mut post.bloom.knee, 0.0..=1.0);
            drag_f32(ui, "Scatter", &mut post.bloom.scatter, 0.0..=1.0);
            ui.horizontal(|ui| {
                ui.label("Tint");
                ui.color_edit_button_rgb(&mut post.bloom.tint);
            });
        });
    egui::CollapsingHeader::new("Ambient occlusion")
        .id_salt("pp_ao")
        .show(ui, |ui| {
            ui.checkbox(&mut post.ambient_occlusion.enabled, "Enabled");
            drag_f32(
                ui,
                "Radius",
                &mut post.ambient_occlusion.radius,
                0.001..=100.0,
            );
            drag_f32(
                ui,
                "Intensity",
                &mut post.ambient_occlusion.intensity,
                0.0..=4.0,
            );
            drag_f32(
                ui,
                "Falloff",
                &mut post.ambient_occlusion.falloff,
                0.0..=1.0,
            );
            ui.horizontal(|ui| {
                ui.label("Quality");
                egui::ComboBox::from_id_salt("pp_ao_quality")
                    .selected_text(match post.ambient_occlusion.quality {
                        AmbientOcclusionQuality::Low => "Low",
                        AmbientOcclusionQuality::Medium => "Medium",
                        AmbientOcclusionQuality::High => "High",
                    })
                    .show_ui(ui, |ui| {
                        let quality = &mut post.ambient_occlusion.quality;
                        ui.selectable_value(quality, AmbientOcclusionQuality::Low, "Low");
                        ui.selectable_value(quality, AmbientOcclusionQuality::Medium, "Medium");
                        ui.selectable_value(quality, AmbientOcclusionQuality::High, "High");
                    });
            });
        });
    egui::CollapsingHeader::new("Color grading")
        .id_salt("pp_grading")
        .show(ui, |ui| {
            ui.checkbox(&mut post.color_grading.enabled, "Enabled");
            drag_f32(
                ui,
                "Temperature",
                &mut post.color_grading.temperature,
                -1.0..=1.0,
            );
            drag_f32(ui, "Tint", &mut post.color_grading.tint, -1.0..=1.0);
            drag_f32(ui, "Contrast", &mut post.color_grading.contrast, 0.0..=2.0);
            drag_f32(
                ui,
                "Saturation",
                &mut post.color_grading.saturation,
                0.0..=2.0,
            );
            drag_rgb(ui, "Lift", &mut post.color_grading.lift, -1.0..=1.0);
            drag_rgb(ui, "Gamma", &mut post.color_grading.gamma, 0.001..=4.0);
            drag_rgb(ui, "Gain", &mut post.color_grading.gain, 0.0..=4.0);
        });
    egui::CollapsingHeader::new("Vignette")
        .id_salt("pp_vignette")
        .show(ui, |ui| {
            ui.checkbox(&mut post.vignette.enabled, "Enabled");
            drag_f32(ui, "Intensity", &mut post.vignette.intensity, 0.0..=1.0);
            drag_f32(ui, "Smoothness", &mut post.vignette.smoothness, 0.0..=1.0);
            drag_f32(ui, "Roundness", &mut post.vignette.roundness, 0.0..=1.0);
            ui.horizontal(|ui| {
                ui.label("Color");
                ui.color_edit_button_rgb(&mut post.vignette.color);
            });
        });
    egui::CollapsingHeader::new("Chromatic aberration")
        .id_salt("pp_aberration")
        .show(ui, |ui| {
            ui.checkbox(&mut post.chromatic_aberration.enabled, "Enabled");
            drag_f32(
                ui,
                "Intensity",
                &mut post.chromatic_aberration.intensity,
                0.0..=1.0,
            );
        });
    egui::CollapsingHeader::new("Film grain")
        .id_salt("pp_grain")
        .show(ui, |ui| {
            ui.checkbox(&mut post.film_grain.enabled, "Enabled");
            drag_f32(ui, "Intensity", &mut post.film_grain.intensity, 0.0..=1.0);
            drag_f32(ui, "Response", &mut post.film_grain.response, 0.0..=1.0);
        });
    egui::CollapsingHeader::new("Depth of field")
        .id_salt("pp_dof")
        .show(ui, |ui| {
            ui.checkbox(&mut post.depth_of_field.enabled, "Enabled");
            drag_f32(
                ui,
                "Focus distance",
                &mut post.depth_of_field.focus_distance,
                0.001..=10_000.0,
            );
            drag_f32(
                ui,
                "Aperture",
                &mut post.depth_of_field.aperture,
                0.1..=64.0,
            );
            drag_f32(
                ui,
                "Focal length (mm)",
                &mut post.depth_of_field.focal_length_mm,
                1.0..=1000.0,
            );
            drag_f32(
                ui,
                "Max blur (px)",
                &mut post.depth_of_field.max_blur_px,
                0.0..=256.0,
            );
        });
    egui::CollapsingHeader::new("Motion blur")
        .id_salt("pp_motion")
        .show(ui, |ui| {
            ui.checkbox(&mut post.motion_blur.enabled, "Enabled");
            drag_f32(
                ui,
                "Shutter angle",
                &mut post.motion_blur.shutter_angle_deg,
                0.1..=360.0,
            );
            ui.horizontal(|ui| {
                ui.label("Samples");
                ui.add(egui::DragValue::new(&mut post.motion_blur.samples).range(4..=32));
            });
            drag_f32(
                ui,
                "Max blur (px)",
                &mut post.motion_blur.max_blur_px,
                0.0..=256.0,
            );
        });
}

fn drag_f32(ui: &mut egui::Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.add(egui::DragValue::new(value).range(range).speed(0.01));
    });
}

fn drag_rgb(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut [f32; 3],
    range: std::ops::RangeInclusive<f32>,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        for channel in value.iter_mut() {
            ui.add(
                egui::DragValue::new(channel)
                    .range(range.clone())
                    .speed(0.01),
            );
        }
    });
}
