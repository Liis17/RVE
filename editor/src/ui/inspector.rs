//! Панель Inspector: поля и компоненты выбранного объекта.

use crate::assets_fs::AssetKind;
use crate::scene_edit::default_light;
use crate::state::{AssetDrag, EditorState};
use engine_files::VirtualPath;
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
    if let Some(camera) = object.camera {
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

fn show_transform(ui: &mut egui::Ui, state: &mut EditorState, object: &SceneObject) {
    let id = object.id.as_str();
    egui::CollapsingHeader::new("Transform")
        .default_open(true)
        .show(ui, |ui| {
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
}

/// Заголовок компонента с кнопкой удаления; возвращает `true` при её нажатии.
fn component_header(
    ui: &mut egui::Ui,
    title: &str,
    salt: impl std::hash::Hash + std::fmt::Debug,
    body: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut remove = false;
    let id = ui.make_persistent_id(("component", title, salt));
    egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true)
        .show_header(ui, |ui| {
            ui.label(title);
            remove = ui.small_button("Remove").clicked();
        })
        .body(body);
    remove
}

fn show_camera(ui: &mut egui::Ui, state: &mut EditorState, id: &str, camera: CameraComponent) {
    let remove = component_header(ui, "Camera", id, |ui| {
        egui::Grid::new("camera").num_columns(2).show(ui, |ui| {
            let mut value = camera;
            ui.label("FOV");
            let fov = ui.add(
                egui::DragValue::new(&mut value.fov_deg)
                    .speed(0.25)
                    .suffix("°"),
            );
            ui.end_row();
            ui.label("Near");
            let near = ui.add(egui::DragValue::new(&mut value.near).speed(0.01));
            ui.end_row();
            ui.label("Far");
            let far = ui.add(egui::DragValue::new(&mut value.far).speed(1.0));
            ui.end_row();
            commit(state, id, "camera", &(fov | near | far), |object| {
                object.camera = Some(value);
            });
        });
    });
    if remove {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.camera = None;
                Ok(())
            })
        });
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
    let remove = component_header(ui, "Mesh Renderer", id, |ui| {
        egui::Grid::new("mesh").num_columns(2).show(ui, |ui| {
            ui.label("Model");
            model_choice = asset_picker(ui, "mesh_model", mesh.model.as_str(), &models);
            ui.end_row();
            ui.label("Texture");
            let current = mesh.texture.as_ref().map_or("", |texture| texture.as_str());
            texture_choice = asset_picker(ui, "mesh_texture", current, &textures);
            ui.end_row();
        });
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
    if remove {
        state.edit_scene(|scene| scene.remove_mesh_renderer(id));
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
    let remove_component = component_header(ui, "LOD Group", id, |ui| {
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

    if remove_component {
        state.edit_scene(|scene| scene.set_lod_group(id, None));
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
    let remove = component_header(ui, "Directional Light", id, |ui| {
        egui::Grid::new("light").num_columns(2).show(ui, |ui| {
            let mut value = light;
            let color = vec3_row(ui, "Color", &mut value.color, 0.01);
            let ambient = vec3_row(ui, "Ambient", &mut value.ambient, 0.01);
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
            commit(state, id, "light", &response, |object| {
                object.directional_light = Some(value);
            });
        });
    });
    if remove {
        state.edit_scene(|scene| {
            scene.edit_object(id, None, |object| {
                object.directional_light = None;
                Ok(())
            })
        });
    }
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
    let remove = component_header(ui, &title, (id, index), |ui| {
        params_response = params_ui(ui, ("lite", id, index), &mut params);
    });
    if let Some(response) = &params_response {
        let key = format!("lite-{index}");
        commit(state, id, &key, response, |object| {
            object.lite_scripts[index].params = params;
        });
    }
    if remove {
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
    let remove = component_header(ui, &title, (id, index), |ui| {
        params_response = params_ui(ui, ("heavy", id, index), &mut params);
    });
    if let Some(response) = &params_response {
        let key = format!("heavy-{index}");
        commit(state, id, &key, response, |object| {
            object.heavy_scripts[index].params = params;
        });
    }
    if remove {
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
            state.edit_scene(|scene| {
                scene.edit_object(id, None, |object| {
                    object.camera = Some(CameraComponent {
                        fov_deg: 60.0,
                        near: 0.1,
                        far: 1000.0,
                    });
                    Ok(())
                })
            });
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
