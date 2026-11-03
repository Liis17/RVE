//! Панель Game: сцена глазами камеры из документа.

use super::scene_view::feed_play_pointer;
use crate::state::EditorState;
use egui::{Color32, Rect, Sense};
use engine_scene::SceneDocument;

/// Камеры документа в его порядке: id и отображаемое имя.
fn scene_cameras(document: &SceneDocument) -> Vec<(String, String)> {
    document
        .objects
        .iter()
        .filter(|object| object.camera.is_some())
        .map(|object| (object.id.clone(), object.display_name().to_owned()))
        .collect()
}

/// Выбранная камера, если она ещё есть в сцене, иначе первая: она же камера игры.
fn resolve_camera<'a>(
    cameras: &'a [(String, String)],
    selected: Option<&str>,
) -> Option<&'a (String, String)> {
    cameras
        .iter()
        .find(|(id, _)| Some(id.as_str()) == selected)
        .or_else(|| cameras.first())
}

pub fn show(ui: &mut egui::Ui, state: &mut EditorState) {
    if state
        .scene
        .as_ref()
        .is_some_and(|scene| scene.document().is_prefab())
    {
        state.game_view.visible = false;
        ui.label("Game недоступен для префаба");
        return;
    }
    let Some(scene) = state.scene.as_ref() else {
        ui.label("Сцена не открыта");
        return;
    };
    let cameras = scene_cameras(scene.view());
    let Some((current_id, current_name)) =
        resolve_camera(&cameras, state.game_camera.as_deref()).cloned()
    else {
        return;
    };
    let mut chosen = current_id;
    ui.horizontal(|ui| {
        ui.label("Camera");
        if cameras.len() > 1 {
            egui::ComboBox::from_id_salt("game_camera")
                .selected_text(current_name.as_str())
                .show_ui(ui, |ui| {
                    for (id, name) in &cameras {
                        ui.selectable_value(&mut chosen, id.clone(), name);
                    }
                });
        } else {
            ui.label(current_name.as_str());
        }
    });
    state.game_camera = Some(chosen);

    let rect = ui.available_rect_before_wrap();
    let response = ui.allocate_rect(rect, Sense::hover());
    let pixels_per_point = ui.ctx().pixels_per_point();
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }
    state.game_view.visible = true;
    state.game_view.hovered = response.contains_pointer();
    state.game_view.size = [
        (rect.width() * pixels_per_point).round() as u32,
        (rect.height() * pixels_per_point).round() as u32,
    ];
    if let Some(texture) = state.game_viewport_texture {
        ui.painter().image(
            texture,
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    }
    if state.is_playing() {
        feed_play_pointer(ui, state, rect, pixels_per_point);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_edit::{EditorScene, NewObject};

    fn two_cameras() -> EditorScene {
        let mut scene = EditorScene::new_untitled().unwrap();
        scene.create_object(&NewObject::Camera, None).unwrap();
        scene
    }

    #[test]
    fn cameras_are_listed_in_document_order_with_their_names() {
        let cameras = scene_cameras(two_cameras().document());
        assert_eq!(cameras.len(), 2);
        assert_eq!(cameras[0], ("camera".to_owned(), "Camera".to_owned()));
        assert_ne!(cameras[1].0, "camera");
    }

    #[test]
    fn selection_falls_back_to_the_first_camera_when_it_is_gone() {
        let cameras = scene_cameras(two_cameras().document());
        let second = cameras[1].0.as_str();
        assert_eq!(
            resolve_camera(&cameras, Some(second)).unwrap().0,
            cameras[1].0
        );
        assert_eq!(
            resolve_camera(&cameras, Some("deleted")).unwrap().0,
            "camera"
        );
        assert_eq!(resolve_camera(&cameras, None).unwrap().0, "camera");
    }
}
