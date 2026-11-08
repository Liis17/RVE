//! Inspector for `.rvemat` and `.rveshader` project assets.

use crate::assets_fs::AssetKind;
use crate::materials::{material_to_toml_value, shader_schema, sorted_paths, surface_source};
use crate::state::EditorState;
use engine_files::VirtualPath;
use engine_scene::{
    MaterialAlphaMode, MaterialAsset, ShaderProperty, ShaderPropertyDefault, ShaderPropertyKind,
};
use std::collections::BTreeSet;

pub fn show(ui: &mut egui::Ui, state: &mut EditorState, path: &str) {
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| match AssetKind::of(path) {
            AssetKind::Material => show_material(ui, state, path),
            AssetKind::Shader => show_shader(ui, state, path),
            _ => {
                ui.label("Ассет не выбран");
            }
        });
}

fn show_material(ui: &mut egui::Ui, state: &mut EditorState, path: &str) {
    ui.heading("Material");
    ui.label(path);
    if let Some(cached) = state.materials.material(path).cloned() {
        if let Some(error) = &cached.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        let Some(mut material) = cached.effective_asset().cloned() else {
            return;
        };
        let mut changed = false;
        ui.horizontal(|ui| {
            if ui.button("Open in Editor").clicked() {
                open_external(state, path);
            }
            if ui.button("Preview").clicked() {
                request_preview(state, path);
            }
        });
        ui.separator();
        egui::Grid::new(("material-header", path))
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Shader");
                let shader_paths = sorted_paths(state.materials.shader_paths().map(str::to_owned));
                let mut shader = material.shader.clone();
                egui::ComboBox::from_id_salt(("material-shader", path))
                    .selected_text(
                        shader
                            .as_ref()
                            .map_or("Lit (built-in)", |value| value.as_str()),
                    )
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut shader, None, "Lit (built-in)");
                        for shader_path in shader_paths {
                            if let Ok(shader_path) = VirtualPath::parse(&shader_path) {
                                ui.selectable_value(
                                    &mut shader,
                                    Some(shader_path.clone()),
                                    shader_path.as_str(),
                                );
                            }
                        }
                    });
                if shader != material.shader {
                    material.shader = shader;
                    changed = true;
                }
                ui.end_row();
                ui.label("Alpha Mode");
                let before = material.alpha_mode;
                egui::ComboBox::from_id_salt(("material-alpha", path))
                    .selected_text(alpha_label(&material.alpha_mode))
                    .show_ui(ui, |ui| {
                        for mode in [
                            MaterialAlphaMode::Opaque,
                            MaterialAlphaMode::Mask,
                            MaterialAlphaMode::Blend,
                        ] {
                            ui.selectable_value(&mut material.alpha_mode, mode, alpha_label(&mode));
                        }
                    });
                changed |= before != material.alpha_mode;
                ui.end_row();
                if material.alpha_mode == MaterialAlphaMode::Mask {
                    ui.label("Alpha Cutoff");
                    changed |= ui
                        .add(
                            egui::DragValue::new(&mut material.alpha_cutoff)
                                .range(0.0..=1.0)
                                .speed(0.01),
                        )
                        .changed();
                    ui.end_row();
                }
                ui.label("Double Sided");
                changed |= ui.checkbox(&mut material.double_sided, "").changed();
                ui.end_row();
            });

        let selected_shader = material
            .shader
            .as_ref()
            .and_then(|path| state.materials.shader(path.as_str()));
        if let Some(shader) = selected_shader
            && let Some(error) = &shader.error
        {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        let schema = shader_schema(selected_shader.and_then(|shader| shader.asset.as_ref()));
        ui.separator();
        ui.heading("Properties");
        for (index, layout) in schema.properties.iter().enumerate() {
            let property = &layout.property;
            ui.push_id((path, "property", index), |ui| {
                let key = property.name.as_str();
                let default = material_to_toml_value(property);
                let current = material
                    .properties
                    .get(key)
                    .cloned()
                    .or(default.clone())
                    .unwrap_or(toml::Value::Float(0.0));
                let mut value = current;
                let mut reset = false;
                ui.horizontal(|ui| {
                    ui.label(&property.label);
                    if property_widget(ui, property, &mut value, state) {
                        changed = true;
                        if property.kind == ShaderPropertyKind::Texture
                            && value.as_str().is_some_and(str::is_empty)
                        {
                            material.properties.remove(key);
                        } else {
                            material.properties.insert(key.to_owned(), value.clone());
                        }
                    }
                    if ui
                        .small_button("↺")
                        .on_hover_text("Reset to default")
                        .clicked()
                    {
                        reset = true;
                    }
                });
                if reset {
                    match default {
                        Some(value) => {
                            material.properties.insert(key.to_owned(), value);
                        }
                        None => {
                            material.properties.remove(key);
                        }
                    }
                    changed = true;
                }
            });
        }
        let known = schema
            .properties
            .iter()
            .map(|property| property.property.name.as_str())
            .collect::<BTreeSet<_>>();
        let unused = material
            .properties
            .keys()
            .filter(|key| !known.contains(key.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        if !unused.is_empty() {
            ui.horizontal(|ui| {
                ui.label(format!("Unused properties: {}", unused.len()));
                if ui.button("Remove unused").clicked() {
                    for key in &unused {
                        material.properties.remove(key);
                    }
                    changed = true;
                }
            });
        }
        for warning in material.resolve(&schema).warnings {
            ui.colored_label(ui.visuals().warn_fg_color, warning);
        }
        if changed {
            state.materials.set_material_draft(path, material.clone());
        }
        let pointer_down =
            ui.input(|input| input.pointer.button_down(egui::PointerButton::Primary));
        if !pointer_down
            && state
                .materials
                .material(path)
                .is_some_and(|cached| cached.draft.is_some())
        {
            save_material(state, path, material);
        }
    } else {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "Материал ещё не загружен в библиотеку.",
        );
    }
}

fn property_widget(
    ui: &mut egui::Ui,
    property: &ShaderProperty,
    value: &mut toml::Value,
    state: &EditorState,
) -> bool {
    match property.kind {
        ShaderPropertyKind::Texture => {
            let textures = sorted_paths(state.assets_of_kind(AssetKind::Texture).into_iter());
            let current = value.as_str().unwrap_or_default().to_owned();
            let mut next = current.clone();
            egui::ComboBox::from_id_salt(("material-texture", &property.name))
                .selected_text(if next.is_empty() { "None" } else { &next })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut next, String::new(), "None");
                    for path in textures {
                        ui.selectable_value(&mut next, path.clone(), path);
                    }
                });
            if next == current {
                false
            } else if next.is_empty() {
                *value = toml::Value::String(String::new());
                true
            } else {
                *value = toml::Value::String(next);
                true
            }
        }
        kind => {
            let count = kind.components().unwrap_or(1);
            let mut values =
                numeric_values(value, count).unwrap_or_else(|| match &property.default {
                    ShaderPropertyDefault::Numeric(default) => default.clone(),
                    ShaderPropertyDefault::Texture(_) => vec![0.0; count],
                });
            values.resize(count, 0.0);
            values.truncate(count);
            let old = values.clone();
            if kind == ShaderPropertyKind::Color && count == 3 {
                let mut color = [values[0], values[1], values[2]];
                egui::color_picker::color_edit_button_rgb(ui, &mut color);
                values.copy_from_slice(&color);
            } else {
                for value in &mut values {
                    let mut drag = egui::DragValue::new(value).speed(0.01);
                    if count == 1 {
                        if let Some(min) = property.min {
                            drag = drag.range(min..=property.max.unwrap_or(f32::MAX));
                        } else if let Some(max) = property.max {
                            drag = drag.range(f32::MIN..=max);
                        }
                    }
                    ui.add(drag);
                }
            }
            if values == old {
                false
            } else {
                *value = if count == 1 {
                    toml::Value::Float(f64::from(values[0]))
                } else {
                    toml::Value::Array(
                        values
                            .into_iter()
                            .map(|value| toml::Value::Float(f64::from(value)))
                            .collect(),
                    )
                };
                true
            }
        }
    }
}

fn numeric_values(value: &toml::Value, count: usize) -> Option<Vec<f32>> {
    if count == 1 {
        return value
            .as_float()
            .or_else(|| value.as_integer().map(|number| number as f64))
            .map(|number| vec![number as f32]);
    }
    value
        .as_array()?
        .iter()
        .map(|value| {
            value
                .as_float()
                .or_else(|| value.as_integer().map(|number| number as f64))
                .map(|number| number as f32)
        })
        .collect()
}

fn show_shader(ui: &mut egui::Ui, state: &mut EditorState, path: &str) {
    ui.heading("Surface Shader");
    ui.label(path);
    let Some(shader) = state.materials.shader(path).cloned() else {
        ui.colored_label(
            ui.visuals().warn_fg_color,
            "Шейдер ещё не загружен в библиотеку.",
        );
        return;
    };
    if let Some(error) = shader.error {
        ui.colored_label(ui.visuals().error_fg_color, error);
    }
    ui.horizontal(|ui| {
        if ui.button("Open in Editor").clicked() {
            open_external(state, path);
        }
        if ui.button("Preview").clicked() {
            request_preview(state, path);
        }
    });
    let Some(asset) = shader.asset else {
        return;
    };
    ui.label(format!("Lighting: {:?}", asset.lighting));
    match engine_render::compile_surface_shader(&surface_source(&asset)) {
        Ok(_) => {
            ui.colored_label(egui::Color32::GREEN, "Compile status: OK");
        }
        Err(error) => {
            let location = error.line.map_or_else(String::new, |line| {
                format!(
                    " (file line {})",
                    asset.code_first_line + line.saturating_sub(1)
                )
            });
            ui.colored_label(
                ui.visuals().error_fg_color,
                format!("Compile status: {error}{location}"),
            );
        }
    }
    ui.separator();
    egui::Grid::new(("shader-properties", path))
        .striped(true)
        .show(ui, |ui| {
            ui.strong("Name");
            ui.strong("Type");
            ui.strong("Default");
            ui.strong("Range");
            ui.end_row();
            for property in asset.properties {
                ui.label(property.label);
                ui.label(format!("{:?}", property.kind));
                ui.label(format_default(&property.default));
                ui.label(match (property.min, property.max) {
                    (Some(min), Some(max)) => format!("{min} … {max}"),
                    (Some(min), None) => format!("≥ {min}"),
                    (None, Some(max)) => format!("≤ {max}"),
                    (None, None) => String::new(),
                });
                ui.end_row();
            }
        });
}

fn format_default(default: &ShaderPropertyDefault) -> String {
    match default {
        ShaderPropertyDefault::Numeric(values) => format!("{values:?}"),
        ShaderPropertyDefault::Texture(path) => path
            .as_ref()
            .map_or_else(|| "None".to_owned(), |path| path.as_str().to_owned()),
    }
}

fn alpha_label(mode: &MaterialAlphaMode) -> &'static str {
    match mode {
        MaterialAlphaMode::Opaque => "Opaque",
        MaterialAlphaMode::Mask => "Mask",
        MaterialAlphaMode::Blend => "Blend",
    }
}

fn save_material(state: &mut EditorState, path: &str, material: MaterialAsset) {
    let Some(project) = state.project.as_ref() else {
        return;
    };
    let Some(path_value) = VirtualPath::parse(path).ok() else {
        state
            .log
            .error(format!("{path}: некорректный путь материала"));
        return;
    };
    match project.save_material(&path_value, &material) {
        Ok(()) => {
            state.materials.mark_material_saved(path, material);
        }
        Err(error) => state.log.error(error),
    }
}

fn open_external(state: &mut EditorState, path: &str) {
    if let Some(project) = state.project.as_ref()
        && let Err(error) = state.external_editor.open(project, path)
    {
        state.log.error(error);
    }
}

fn request_preview(state: &mut EditorState, path: &str) {
    state.material_preview_asset = Some(path.to_owned());
    state.preview_requested = true;
}
