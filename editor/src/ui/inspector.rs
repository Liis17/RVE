//! Панель Inspector: поля и компоненты выбранного объекта.

use crate::assets_fs::AssetKind;
use crate::scene_edit::{Component, ComponentKind, default_light};
use crate::state::{AssetDrag, EditorState};
use engine_files::VirtualPath;
use engine_scene::document::MaterialOverride;
use engine_scene::{
    CameraComponent, HeavyScript, LodGroup, LodLevel, MAX_LOD_LEVELS, ParamValue, SceneObject,
    SceneTransform,
};
use std::collections::BTreeMap;

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
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

fn show_object(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let id = object.id.as_str();
    egui::Grid::new("object_fields")
        .num_columns(2)
        .show(ui, |ui| {
            ui.label("Name");
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
            let response = ui.text_edit_singleline(&mut text);
            if response.lost_focus() {
                ui.ctx().data_mut(|data| data.remove::<String>(editor_id));
                state.edit_scene(|scene| scene.change_id(id, &text));
            } else if response.has_focus() {
                ui.ctx().data_mut(|data| data.insert_temp(editor_id, text));
            }
            ui.end_row();
        });

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
    for index in 0..object.lite_scripts.len() {
        show_lite_script(ui, state, object, index);
    }
    for index in 0..object.heavy_scripts.len() {
        show_heavy_script(ui, state, object, index);
    }
    ui.separator();
    show_add_component(ui, state, object);
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
    let menu = component_menu(state, id, ComponentKind::Transform);
    let action = component_header(ui, "Transform", id, Some(menu), false, |ui| {
        egui::Grid::new("transform").num_columns(2).show(ui, |ui| {
            let mut transform: SceneTransform = object.transform;
            let position = vec3_row(ui, "Position", &mut transform.position, 0.05);
            let rotation = vec3_row(ui, "Rotation", &mut transform.rotation, 0.5);
            let scale = vec3_row(ui, "Scale", &mut transform.scale, 0.02);
            let response = position | rotation | scale;
            commit(state, id, "transform", &response, |object| {
                object.transform = transform;
            });
        });
    });
    run_menu_action(state, id, ComponentKind::Transform, action);
}

/// Действие из заголовка компонента.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HeaderAction {
    None,
    Remove,
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
    ComponentMenu {
        reset: object.is_some_and(|object| Component::reset(object, kind).is_some()),
        paste: state
            .component_clipboard
            .as_ref()
            .is_some_and(|copied| copied.kind() == kind),
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
        HeaderAction::None | HeaderAction::Remove => None,
    };
    if let Some(component) = component {
        state.edit_scene(|scene| scene.set_component(id, component));
    }
}

/// Заголовок компонента: кнопка удаления и контекстное меню Reset/Copy/Paste по щелчку ПКМ.
fn component_header(
    ui: &mut egui::Ui,
    title: &str,
    salt: impl std::hash::Hash + std::fmt::Debug,
    menu: Option<ComponentMenu>,
    removable: bool,
    body: impl FnOnce(&mut egui::Ui),
) -> HeaderAction {
    let mut action = HeaderAction::None;
    let id = ui.make_persistent_id(("component", title, salt));
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
        .show_header(ui, |ui| {
            let label = ui.add(egui::Label::new(title).sense(egui::Sense::click()));
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
            if removable && ui.small_button("Remove").clicked() {
                action = HeaderAction::Remove;
            }
        })
        .body(body);
    action
}

fn show_camera(ui: &mut egui::Ui, state: &mut EditorState, id: &str, camera: &CameraComponent) {
    let menu = component_menu(state, id, ComponentKind::Camera);
    let action = component_header(ui, "Camera", id, Some(menu), true, |ui| {
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
            ui.label("Active");
            let mut active = camera.active;
            let response = ui.checkbox(&mut active, "");
            if response.changed() {
                state.edit_scene(|scene| scene.set_camera_active(id, active));
            }
            finish_edit(state, &response);
        });
    });
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_camera(id));
    }
    run_menu_action(state, id, ComponentKind::Camera, action);
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
    let menu = component_menu(state, id, ComponentKind::MeshRenderer);
    let action = component_header(ui, "Mesh Renderer", id, Some(menu), true, |ui| {
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
        show_material(ui, state, id, mesh.material.as_ref(), &textures);
    });
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
    if action == HeaderAction::Remove {
        state.edit_scene(|scene| scene.remove_mesh_renderer(id));
    }
    run_menu_action(state, id, ComponentKind::MeshRenderer, action);
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
        metallic_roughness_texture: None,
        normal_texture: None,
        normal_scale: None,
    });
    let mut toggled = false;
    let mut picked = false;
    let mut response = None;
    let mut texture_choices = [None, None];
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
            "Normal Scale",
            &mut material.normal_scale,
            1.0,
            |ui, value| Some(ui.add(egui::DragValue::new(value).speed(0.01))),
            &mut response,
        );
        let maps = [
            ("Metal/Rough Map", &material.metallic_roughness_texture),
            ("Normal Map", &material.normal_texture),
        ];
        for (index, (label, path)) in maps.into_iter().enumerate() {
            ui.label(label);
            let current = path.as_ref().map_or("", |path| path.as_str());
            texture_choices[index] =
                asset_picker(ui, label, current, textures).filter(|choice| choice != current);
            ui.end_row();
        }
    });

    let mut discrete = toggled;
    for (choice, slot) in texture_choices.into_iter().zip([
        &mut material.metallic_roughness_texture,
        &mut material.normal_texture,
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
    let menu = component_menu(state, id, ComponentKind::LodGroup);
    let action = component_header(ui, "LOD Group", id, Some(menu), true, |ui| {
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
                        if let Some(choice) = asset_picker(ui, "lod_model", current, &models)
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
                        if let Some(choice) = asset_picker(ui, "lod_texture", current, &textures)
                            && choice != current
                        {
                            level.texture = if choice.is_empty() {
                                None
                            } else {
                                parse_path(state, &choice).map(|path| path.as_str().to_owned())
                            };
                            asset_changed = true;
                        }
                        ui.end_row();

                        ui.label("Distance");
                        let distance = ui.add(egui::DragValue::new(&mut level.distance).speed(0.1));
                        merge_response(&mut response, distance);
                        ui.end_row();
                    });
                let remove = ui.add_enabled(can_remove_level, egui::Button::new("Remove Level"));
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
    let menu = component_menu(state, id, ComponentKind::DirectionalLight);
    let action = component_header(ui, "Directional Light", id, Some(menu), true, |ui| {
        egui::Grid::new("light").num_columns(2).show(ui, |ui| {
            let mut value = light;
            let (color_picked, color) = color_row(ui, "Color", &mut value.color);
            let (ambient_picked, ambient) = color_row(ui, "Ambient", &mut value.ambient);
            ui.label("Shadows");
            let shadows = ui.checkbox(&mut value.shadows, "");
            ui.end_row();
            let center = vec3_row(ui, "Shadow Center", &mut value.shadow_center, 0.05);
            ui.label("Shadow Radius");
            let mut radius = value.shadow_radius.unwrap_or(0.0);
            let radius_response = ui.add(egui::DragValue::new(&mut radius).speed(0.1));
            value.shadow_radius = (radius > 0.0).then_some(radius);
            ui.end_row();
            let response = color | ambient | shadows | center | radius_response;
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
    });
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

fn show_lite_script(
    ui: &mut egui::Ui,
    state: &mut EditorState,
    object: &SceneObject,
    index: usize,
) {
    let id = object.id.as_str();
    let script = &object.lite_scripts[index];
    let title = format!("Lua: {}", script.path.as_str());
    let mut params = script.params.clone();
    let mut params_response = None;
    let action = component_header(ui, &title, (id, index), None, true, |ui| {
        params_response = params_ui(ui, ("lite", id, index), &mut params);
    });
    if let Some(response) = &params_response {
        let key = format!("lite-{index}");
        commit(state, id, &key, response, |object| {
            object.lite_scripts[index].params = params;
        });
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
    let mut params = script.params.clone();
    let mut params_response = None;
    let action = component_header(ui, &title, (id, index), None, true, |ui| {
        params_response = params_ui(ui, ("heavy", id, index), &mut params);
    });
    if let Some(response) = &params_response {
        let key = format!("heavy-{index}");
        commit(state, id, &key, response, |object| {
            object.heavy_scripts[index].params = params;
        });
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
            scene.edit_object("camera", None, |object| {
                object.camera.as_mut().unwrap().active = true;
                Ok(())
            })
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
        assert_eq!(camera(&state, &other).active, true);
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
        assert_eq!(camera(&state, &other).active, false);
        assert!(state.scene.as_mut().unwrap().undo());
        assert_eq!(camera(&state, &other).active, true);
    }
}
