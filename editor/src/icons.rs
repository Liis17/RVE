//! Иконки панелей Project и Hierarchy и компонентов Inspector: растеризованы из `IconsPack` при сборке (`build.rs`).

use crate::assets_fs::AssetKind;
use crate::scene_edit::ComponentKind;
use engine_render::{RenderError, Renderer};
use engine_scene::SceneObject;
use engine_ui::render::UiBridge;
use std::collections::HashMap;

/// Иконка из `IconsPack`: RGBA8 128×128 с предумноженным цветом (premultiplied), строки сверху вниз.
struct SourceIcon {
    name: &'static str,
    width: u32,
    height: u32,
    rgba: &'static [u8],
}

static SOURCES: &[SourceIcon] = include!(concat!(env!("OUT_DIR"), "/icons_generated.rs"));

/// Иконка папки в плитке. `.git` и `settings` (без учёта регистра) имеют свои рисунки;
/// остальные папки различаются только наличием содержимого.
pub fn folder_icon(name: &str, has_children: bool) -> &'static str {
    match (name, has_children) {
        (".git", true) => "Folders/folder-git",
        (".git", false) => "Folders/folder-git-empty",
        (_, true) if name.eq_ignore_ascii_case("settings") => "Folders/folder-settings",
        (_, false) if name.eq_ignore_ascii_case("settings") => "Folders/folder-settings-empty",
        (_, true) => "Folders/folder-files",
        (_, false) => "Folders/folder-empty",
    }
}

/// Иконка плитки файла; `None` — плитка показывает превью или заглушку по типу.
pub fn file_icon(kind: AssetKind) -> Option<&'static str> {
    match kind {
        AssetKind::CameraSettings => Some("camera-settings"),
        _ => None,
    }
}

/// Иконка в заголовке компонента Inspector; `None` — заголовок без иконки.
pub fn component_icon(kind: ComponentKind) -> Option<&'static str> {
    match kind {
        ComponentKind::Transform => Some("rve-gizmo"),
        ComponentKind::Camera => Some("camera"),
        ComponentKind::MeshRenderer => Some("mesh"),
        ComponentKind::DirectionalLight => Some("directional-light"),
        ComponentKind::PointLight => Some("point-light"),
        ComponentKind::SpotLight => Some("spot-light"),
        ComponentKind::Collider => Some("collider"),
        _ => None,
    }
}

/// Иконка строки Hierarchy по компонентам объекта. При нескольких компонентах
/// побеждает первый: камера, свет (направленный, точечный, прожектор), Mesh Renderer.
/// Объект только с Transform получает `empty-object`; с другими компонентами — `None`.
pub fn object_icon(object: &SceneObject) -> Option<&'static str> {
    // Полное деструктурирование: новый компонент SceneObject не скомпилируется,
    // пока не решено, делает ли он объект непустым.
    let SceneObject {
        id: _,
        name: _,
        parent: _,
        transform: _,
        camera,
        mesh_renderer,
        lod_group,
        directional_light,
        point_light,
        spot_light,
        environment,
        collider,
        rigid_body,
        character_controller,
        lite_scripts,
        heavy_scripts,
    } = object;
    if camera.is_some() {
        Some("camera")
    } else if directional_light.is_some() {
        Some("directional-light")
    } else if point_light.is_some() {
        Some("point-light")
    } else if spot_light.is_some() {
        Some("spot-light")
    } else if mesh_renderer.is_some() {
        Some("cube")
    } else if lod_group.is_none()
        && environment.is_none()
        && collider.is_none()
        && rigid_body.is_none()
        && character_controller.is_none()
        && lite_scripts.is_empty()
        && heavy_scripts.is_empty()
    {
        Some("empty-object")
    } else {
        None
    }
}

/// UI-текстуры иконок. Загружаются один раз и живут до конца работы редактора:
/// renderer создаётся вместе с окном, а набор иконок фиксирован сборкой.
#[derive(Default)]
pub struct IconCache {
    textures: HashMap<&'static str, egui::TextureId>,
    loaded: bool,
}

impl IconCache {
    /// Загружает все иконки в GPU при первом вызове; вызывается до построения UI.
    pub fn upload(
        &mut self,
        renderer: &mut Renderer,
        bridge: &mut UiBridge,
    ) -> Result<(), RenderError> {
        if self.loaded {
            return Ok(());
        }
        for icon in SOURCES {
            let texture = renderer.create_ui_texture(icon.width, icon.height, icon.rgba)?;
            self.textures
                .insert(icon.name, bridge.register_texture(texture));
        }
        self.loaded = true;
        Ok(())
    }

    /// Текстура иконки по имени из `IconsPack` без расширения; до загрузки — `None`.
    pub fn get(&self, name: &str) -> Option<egui::TextureId> {
        self.textures.get(name).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene_edit::default_light;
    use engine_files::VirtualPath;
    use engine_scene::MeshRenderer;
    use std::path::Path;

    fn mesh_renderer() -> MeshRenderer {
        MeshRenderer {
            model: VirtualPath::parse("/models/cube.fbx").unwrap(),
            texture: None,
            material: None,
        }
    }

    #[test]
    fn folder_icon_follows_git_settings_and_content_rules() {
        assert_eq!(folder_icon(".git", true), "Folders/folder-git");
        assert_eq!(folder_icon(".git", false), "Folders/folder-git-empty");
        for name in ["settings", "Settings", "SETTINGS"] {
            assert_eq!(folder_icon(name, true), "Folders/folder-settings");
            assert_eq!(folder_icon(name, false), "Folders/folder-settings-empty");
        }
        assert_eq!(folder_icon("Models", true), "Folders/folder-files");
        assert_eq!(folder_icon("Models", false), "Folders/folder-empty");
        assert_eq!(folder_icon("git", true), "Folders/folder-files");
    }

    #[test]
    fn every_folder_icon_choice_is_in_the_generated_list() {
        for name in [".git", "settings", "Models"] {
            for has_children in [true, false] {
                let icon = folder_icon(name, has_children);
                assert!(SOURCES.iter().any(|source| source.name == icon), "{icon}");
            }
        }
    }

    #[test]
    fn component_and_file_icons_are_in_the_generated_list() {
        assert_eq!(component_icon(ComponentKind::Transform), Some("rve-gizmo"));
        assert_eq!(component_icon(ComponentKind::Camera), Some("camera"));
        assert_eq!(component_icon(ComponentKind::MeshRenderer), Some("mesh"));
        assert_eq!(
            component_icon(ComponentKind::DirectionalLight),
            Some("directional-light")
        );
        assert_eq!(
            component_icon(ComponentKind::PointLight),
            Some("point-light")
        );
        assert_eq!(component_icon(ComponentKind::SpotLight), Some("spot-light"));
        assert_eq!(component_icon(ComponentKind::Collider), Some("collider"));
        assert_eq!(component_icon(ComponentKind::RigidBody), None);
        assert_eq!(
            file_icon(AssetKind::CameraSettings),
            Some("camera-settings")
        );
        assert_eq!(file_icon(AssetKind::Scene), None);
        let components = [
            ComponentKind::Transform,
            ComponentKind::Camera,
            ComponentKind::MeshRenderer,
            ComponentKind::LodGroup,
            ComponentKind::DirectionalLight,
            ComponentKind::PointLight,
            ComponentKind::SpotLight,
            ComponentKind::Environment,
            ComponentKind::Collider,
            ComponentKind::RigidBody,
            ComponentKind::CharacterController,
        ];
        let icons = components
            .into_iter()
            .filter_map(component_icon)
            .chain(file_icon(AssetKind::CameraSettings))
            .chain(object_icon(&SceneObject {
                mesh_renderer: Some(mesh_renderer()),
                ..SceneObject::default()
            }));
        for icon in icons {
            assert!(SOURCES.iter().any(|source| source.name == icon), "{icon}");
        }
    }

    #[test]
    fn only_transform_objects_get_the_empty_object_icon() {
        let empty = SceneObject {
            id: "empty".to_owned(),
            name: Some("Empty".to_owned()),
            parent: Some("root".to_owned()),
            ..SceneObject::default()
        };
        assert_eq!(object_icon(&empty), Some("empty-object"));
        assert!(SOURCES.iter().any(|source| source.name == "empty-object"));
        let with_collider = SceneObject {
            collider: Some(Default::default()),
            ..empty.clone()
        };
        assert_eq!(object_icon(&with_collider), None);
        let with_script = SceneObject {
            lite_scripts: vec![engine_scene::LiteScript {
                path: VirtualPath::parse("/scripts/spin.lua").unwrap(),
                params: Default::default(),
            }],
            ..empty
        };
        assert_eq!(object_icon(&with_script), None);
    }

    #[test]
    fn object_icon_prefers_camera_then_lights_then_mesh() {
        let mut object = SceneObject::default();
        assert_eq!(object_icon(&object), Some("empty-object"));
        object.mesh_renderer = Some(mesh_renderer());
        assert_eq!(object_icon(&object), Some("cube"));
        object.spot_light = Some(Default::default());
        assert_eq!(object_icon(&object), Some("spot-light"));
        object.point_light = Some(Default::default());
        assert_eq!(object_icon(&object), Some("point-light"));
        object.directional_light = Some(default_light());
        assert_eq!(object_icon(&object), Some("directional-light"));
        object.camera = Some(Default::default());
        assert_eq!(object_icon(&object), Some("camera"));
    }

    #[test]
    fn generated_list_contains_every_folder_svg_as_premultiplied_rgba() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../IconsPack/Folders");
        let mut expected = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "svg"))
            .map(|path| format!("Folders/{}", path.file_stem().unwrap().to_string_lossy()))
            .collect::<Vec<_>>();
        expected.sort();
        let mut names = SOURCES
            .iter()
            .map(|source| source.name)
            .filter(|name| name.starts_with("Folders/"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, expected);

        for source in SOURCES {
            assert_eq!(
                source.rgba.len(),
                (source.width * source.height * 4) as usize
            );
            let (pixels, _) = source.rgba.as_chunks::<4>();
            assert!(
                pixels
                    .iter()
                    .all(|pixel| pixel[..3].iter().all(|channel| *channel <= pixel[3])),
                "{}: цвет больше альфы, premultiplied нарушен",
                source.name
            );
            assert!(
                pixels.iter().any(|pixel| pixel[3] > 0),
                "{}: изображение полностью прозрачно",
                source.name
            );
        }
    }
}
