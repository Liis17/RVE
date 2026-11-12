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
    let has_asset = state.material_preview_asset.is_some() || state.inspected_asset.is_some();
    if let (true, Some(texture)) = (has_asset, state.material_preview_texture) {
        ui.painter().image(
            texture,
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    } else {
        // Без ресурса картинка не рисуется, поэтому старый кадр не остаётся на экране.
        let text = if has_asset {
            "Material viewport is starting"
        } else {
            "Select a material or shader"
        };
        ui.painter().text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(14.0),
            ui.visuals().weak_text_color(),
        );
    }
    if response.dragged() {
        (state.material_preview_yaw, state.material_preview_pitch) = orbit_preview(
            state.material_preview_yaw,
            state.material_preview_pitch,
            response.drag_delta(),
        );
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

/// Перетаскивание вправо поворачивает модель вправо, как в Scene: камера орбиты уходит на -yaw.
fn orbit_preview(yaw: f32, pitch: f32, drag: egui::Vec2) -> (f32, f32) {
    (
        yaw - drag.x * 0.01,
        (pitch + drag.y * 0.01).clamp(-1.35, 1.35),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewport::camera::EditorCamera;

    #[test]
    fn horizontal_drag_orbits_in_the_same_direction_as_scene_camera() {
        let (yaw, pitch) = orbit_preview(0.5, 0.25, egui::vec2(20.0, 0.0));
        let scene = EditorCamera {
            yaw: 0.5,
            pitch: 0.25,
            ..EditorCamera::default()
        };
        let mut scene_moved = scene;
        scene_moved.orbit(20.0, 0.0);
        assert!(scene_moved.yaw < scene.yaw);
        assert!(
            yaw < 0.5,
            "drag right must decrease yaw like the scene camera"
        );
        assert_eq!(pitch, 0.25);
    }

    #[test]
    fn vertical_drag_keeps_its_direction_and_pitch_is_clamped() {
        let (_, pitch) = orbit_preview(0.5, 0.25, egui::vec2(0.0, 20.0));
        assert!(pitch > 0.25);
        let (_, pitch) = orbit_preview(0.5, 1.34, egui::vec2(0.0, 1_000.0));
        assert_eq!(pitch, 1.35);
    }
}
