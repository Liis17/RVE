//! Компонент `ui_canvas`: объект сцены ссылается на игровой UI-документ `.rveui`.
//! Формат документа разбирает `engine-gui`; здесь проверяются только ссылка и её расширение.

use crate::document::{SceneError, require_asset_path, require_extension, virtual_path_serde};
use engine_files::VirtualPath;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Игровой UI, связанный с объектом сцены.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiCanvasComponent {
    /// Виртуальный путь `.rveui`; обязан быть объявлен в `[[assets]]`.
    #[serde(with = "virtual_path_serde")]
    pub document: VirtualPath,
    /// Скрытый канвас не рисуется, но его документ загружен и скрипты могут его показать.
    #[serde(default = "default_visible", skip_serializing_if = "is_true")]
    pub visible: bool,
    /// Слой отрисовки; без значения используется слой из `[canvas]` документа.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<i32>,
}

fn default_visible() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

/// Проверяет ссылку на документ: путь объявлен в `[[assets]]`, расширение `.rveui` без учёта регистра.
pub(crate) fn validate_ui_canvas(
    source: &str,
    object_id: &str,
    assets: &HashSet<VirtualPath>,
    canvas: &UiCanvasComponent,
) -> Result<(), SceneError> {
    require_asset_path(source, object_id, assets, &canvas.document, "UI-документ")?;
    require_extension(source, object_id, &canvas.document, "rveui", "UI-документ")
}

#[cfg(test)]
mod tests {
    use crate::{SceneDocument, World};

    const CAMERA: &str = "[[objects]]\nid = \"camera\"\n[objects.camera]\n";
    const ASSETS: &str = "[[assets]]\npath = \"/ui/hud.rveui\"\n";

    fn parse(input: &str) -> Result<SceneDocument, crate::SceneError> {
        SceneDocument::parse("scenes/test.rvescene", input)
    }

    fn source(fields: &str) -> String {
        format!("{ASSETS}{CAMERA}[[objects]]\nid = \"hud\"\n[objects.ui_canvas]\n{fields}\n")
    }

    #[test]
    fn minimal_component_is_visible_and_uses_the_document_layer() {
        let document = parse(&source("document = \"/ui/hud.rveui\"")).unwrap();
        let canvas = document.objects[1].ui_canvas.as_ref().unwrap();
        assert_eq!(canvas.document.as_str(), "ui/hud.rveui");
        assert!(canvas.visible);
        assert_eq!(canvas.layer, None);
    }

    #[test]
    fn full_component_round_trips_and_defaults_are_not_written() {
        let document = parse(&source(
            "document = \"/ui/hud.rveui\"\nvisible = false\nlayer = -2",
        ))
        .unwrap();
        let canvas = document.objects[1].ui_canvas.clone().unwrap();
        assert!(!canvas.visible);
        assert_eq!(canvas.layer, Some(-2));

        let text = toml::to_string_pretty(&document).unwrap();
        let again = parse(&text).unwrap();
        assert_eq!(again.objects[1].ui_canvas, Some(canvas));

        let defaults = parse(&source("document = \"/ui/hud.rveui\"")).unwrap();
        let text = toml::to_string_pretty(&defaults).unwrap();
        assert!(!text.contains("visible"), "{text}");
        assert!(!text.contains("layer"), "{text}");
    }

    #[test]
    fn document_must_be_declared_and_have_rveui_extension_in_any_case() {
        let undeclared = format!(
            "{CAMERA}[[objects]]\nid = \"hud\"\n[objects.ui_canvas]\ndocument = \"/ui/hud.rveui\"\n"
        );
        let error = parse(&undeclared).unwrap_err();
        assert!(error.message.contains("[[assets]]"), "{error}");

        let wrong_extension = format!(
            "[[assets]]\npath = \"/ui/hud.toml\"\n{CAMERA}[[objects]]\nid = \"hud\"\n[objects.ui_canvas]\ndocument = \"/ui/hud.toml\"\n"
        );
        let error = parse(&wrong_extension).unwrap_err();
        assert!(error.message.contains(".rveui"), "{error}");

        let upper_case = format!(
            "[[assets]]\npath = \"/ui/HUD.RVEUI\"\n{CAMERA}[[objects]]\nid = \"hud\"\n[objects.ui_canvas]\ndocument = \"/ui/HUD.RVEUI\"\n"
        );
        assert!(parse(&upper_case).is_ok());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(parse(&source("document = \"/ui/hud.rveui\"\nscale = 2.0")).is_err());
    }

    #[test]
    fn world_exposes_the_component_of_its_object() {
        let world = World::from_document(
            &parse(&source("document = \"/ui/hud.rveui\"\nlayer = 4")).unwrap(),
        )
        .unwrap();
        let hud = world.handle("hud").unwrap();
        let canvas = world.ui_canvas(hud).unwrap();
        assert_eq!(canvas.layer, Some(4));
        assert!(world.ui_canvas(world.handle("camera").unwrap()).is_none());
    }
}
