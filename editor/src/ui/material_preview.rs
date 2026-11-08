//! Dedicated material preview viewport controls.

use crate::state::{EditorState, MaterialPreviewPrimitive};

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    ui.horizontal(|ui| {
        ui.label("Preview");
        for (primitive, label) in [
            (MaterialPreviewPrimitive::Plane, "Plane"),
            (MaterialPreviewPrimitive::Cube, "Cube"),
            (MaterialPreviewPrimitive::Sphere, "Sphere"),
        ] {
            ui.selectable_value(&mut state.material_preview_primitive, primitive, label);
        }
        ui.separator();
        ui.label(
            state
                .material_preview_asset
                .as_deref()
                .or(state.inspected_asset.as_deref())
                .unwrap_or("Select a material or shader"),
        );
    });
    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, egui::Sense::drag());
    let pixels_per_point = ui.ctx().pixels_per_point();
    state.material_preview.visible = rect.width() >= 1.0 && rect.height() >= 1.0;
    state.material_preview.size = [
        (rect.width() * pixels_per_point).round().max(0.0) as u32,
        (rect.height() * pixels_per_point).round().max(0.0) as u32,
    ];
    if let Some(texture) = state.material_preview_texture {
        ui.painter().image(
            texture,
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    } else {
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Material viewport is starting",
            egui::FontId::proportional(14.0),
            ui.visuals().weak_text_color(),
        );
    }
    if response.dragged() {
        let delta = response.drag_delta();
        state.material_preview_yaw += delta.x * 0.01;
        state.material_preview_pitch =
            (state.material_preview_pitch + delta.y * 0.01).clamp(-1.35, 1.35);
        ui.ctx().request_repaint();
    }
    if response.contains_pointer() {
        let scroll = ui.input(|input| input.smooth_scroll_delta.y);
        if scroll != 0.0 {
            state.material_preview_zoom =
                (state.material_preview_zoom * (-scroll * 0.002).exp()).clamp(1.5, 12.0);
            ui.ctx().request_repaint();
        }
    }
}
