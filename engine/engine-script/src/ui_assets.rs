//! CPU-ассеты игрового UI: `.rveui` с картинками и шрифтами и их регистрация в скриптах.
//!
//! Загрузчик не знает о ресурсах проекта: runtime и редактор передают свой `UiAssetSource`.
//! Каждый ключ регистрируется один раз: повторная регистрация шрифта пересоздаёт все шрифты UI.

use crate::host::ScriptHost;
use engine_files::VirtualPath;
use engine_gui::document::{AssetKind, UiDocument, asset_kind};
use engine_gui::paint::UiImageData;
use engine_gui::system::UiSystem;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// `.rveui` и их ассеты, загруженные на CPU. Ключи — канонические пути без ведущего слеша.
#[derive(Clone, Debug, Default)]
pub struct PreparedUi {
    pub documents: BTreeMap<String, Arc<UiDocument>>,
    pub fonts: BTreeMap<String, Arc<Vec<u8>>>,
    pub images: BTreeMap<String, Arc<UiImageData>>,
}

/// Источник текстов, шрифтов и картинок для `load_ui`. Метки — `/путь` внутри проекта.
pub trait UiAssetSource {
    /// Текст документа `.rveui`.
    fn read_text(&self, label: &str) -> Result<String, String>;
    /// Байты шрифта TTF/OTF.
    fn read_bytes(&self, label: &str) -> Result<Vec<u8>, String>;
    /// Картинка PNG, уже premultiplied (см. [`UiImageData::premultiplied`]).
    fn read_image(&self, label: &str) -> Result<UiImageData, String>;
}

pub fn is_ui_document(path: &VirtualPath) -> bool {
    path.as_str().ends_with(".rveui")
}

/// Загружает документы из путей и все их ассеты. Ошибка разбора, отсутствующий или нечитаемый
/// ассет — ошибка загрузки с путём. Шрифты и размеры картинок проверяются теми же правилами,
/// что и при регистрации в скриптах. Пути, не `.rveui`, пропускаются.
pub fn load_ui<'a>(
    source: &impl UiAssetSource,
    paths: impl IntoIterator<Item = &'a VirtualPath>,
) -> Result<PreparedUi, String> {
    let mut ui = PreparedUi::default();
    let mut probe = UiSystem::default();
    for path in paths {
        if !is_ui_document(path) || ui.documents.contains_key(path.as_str()) {
            continue;
        }
        let label = format!("/{}", path.as_str());
        let text = source.read_text(&label)?;
        let document =
            UiDocument::parse(label.clone(), &text).map_err(|error| format!("{label}: {error}"))?;
        for asset in document.assets() {
            let key = asset.path.as_str().to_owned();
            let asset_label = format!("/{key}");
            match asset_kind(&key) {
                Some(AssetKind::Image) => {
                    if ui.images.contains_key(&key) {
                        continue;
                    }
                    let image = source.read_image(&asset_label)?;
                    probe
                        .register_image(&key, image.size)
                        .map_err(|error| format!("{asset_label}: {error}"))?;
                    ui.images.insert(key, Arc::new(image));
                }
                Some(AssetKind::Font) => {
                    if ui.fonts.contains_key(&key) {
                        continue;
                    }
                    let bytes = source.read_bytes(&asset_label)?;
                    probe
                        .register_font(&key, bytes.clone())
                        .map_err(|error| format!("{asset_label}: {error}"))?;
                    ui.fonts.insert(key, Arc::new(bytes));
                }
                None => {
                    return Err(format!("{label}: неподдерживаемый ассет {key:?}"));
                }
            }
        }
        ui.documents
            .insert(path.as_str().to_owned(), Arc::new(document));
    }
    Ok(ui)
}

/// Что уже зарегистрировано в скриптах. Один регистратор на сцену или сессию Play.
#[derive(Default)]
pub struct UiRegistry {
    documents: HashSet<String>,
    fonts: HashSet<String>,
    images: HashSet<String>,
}

impl UiRegistry {
    /// Регистрирует документы для `open` и канвасов компонентов, шрифты и размеры картинок.
    pub fn register(&mut self, scripts: &mut ScriptHost, ui: &PreparedUi) -> Result<(), String> {
        for (path, document) in &ui.documents {
            if self.documents.insert(path.clone()) {
                let path = VirtualPath::parse(path)
                    .map_err(|error| format!("UI-документ {path}: {error}"))?;
                scripts.register_ui_document(path, Arc::clone(document));
            }
        }
        let mut system = scripts.ui_mut();
        for (path, bytes) in &ui.fonts {
            if self.fonts.insert(path.clone()) {
                system
                    .register_font(path, bytes.to_vec())
                    .map_err(|error| error.to_string())?;
            }
        }
        for (path, image) in &ui.images {
            if self.images.insert(path.clone()) {
                system
                    .register_image(path, image.size)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUD: &str = "[canvas]\n[[elements]]\nid = \"title\"\nrect = { anchor_min = [0.0, 0.0], anchor_max = [0.0, 0.0], pivot = [0.0, 0.0], offset_min = [0.0, 0.0], offset_max = [100.0, 20.0] }\ntext = { string = \"Hi\", size = 20.0, color = [255, 255, 255, 255] }\n";
    const PICTURE: &str = "[[assets]]\npath = \"ui/icon.png\"\n[canvas]\n";

    /// Источник в памяти: тексты и картинки по меткам, шрифтов нет.
    #[derive(Default)]
    struct Memory {
        texts: BTreeMap<String, String>,
        images: BTreeMap<String, UiImageData>,
    }

    impl UiAssetSource for Memory {
        fn read_text(&self, label: &str) -> Result<String, String> {
            self.texts
                .get(label)
                .cloned()
                .ok_or_else(|| format!("{label}: нет файла"))
        }

        fn read_bytes(&self, label: &str) -> Result<Vec<u8>, String> {
            Err(format!("{label}: шрифтов в тесте нет"))
        }

        fn read_image(&self, label: &str) -> Result<UiImageData, String> {
            self.images
                .get(label)
                .cloned()
                .ok_or_else(|| format!("{label}: нет картинки"))
        }
    }

    fn path(text: &str) -> VirtualPath {
        VirtualPath::parse(text).unwrap()
    }

    #[test]
    fn documents_load_once_and_other_paths_are_skipped() {
        let source = Memory {
            texts: BTreeMap::from([("/ui/hud.rveui".to_owned(), HUD.to_owned())]),
            ..Memory::default()
        };
        let hud = path("ui/hud.rveui");
        let ui = load_ui(&source, [&hud, &hud, &path("models/cube.fbx")]).unwrap();
        assert_eq!(ui.documents.keys().collect::<Vec<_>>(), ["ui/hud.rveui"]);
        assert!(ui.fonts.is_empty());
        assert!(ui.images.is_empty());
    }

    #[test]
    fn image_asset_is_read_once_from_the_source_and_kept_premultiplied() {
        let image = UiImageData::premultiplied([1, 1], vec![200, 100, 0, 128]);
        let source = Memory {
            texts: BTreeMap::from([("/ui/panel.rveui".to_owned(), PICTURE.to_owned())]),
            images: BTreeMap::from([("/ui/icon.png".to_owned(), image.clone())]),
        };
        let panel = path("ui/panel.rveui");
        let ui = load_ui(&source, [&panel]).unwrap();
        assert_eq!(
            ui.images.get("ui/icon.png").map(|image| image.as_ref()),
            Some(&image)
        );
    }

    #[test]
    fn missing_or_broken_document_is_an_error_with_its_label() {
        let missing = load_ui(&Memory::default(), [&path("ui/hud.rveui")]).unwrap_err();
        assert!(missing.contains("/ui/hud.rveui"), "{missing}");

        let source = Memory {
            texts: BTreeMap::from([("/ui/bad.rveui".to_owned(), "[[elements".to_owned())]),
            ..Memory::default()
        };
        let error = load_ui(&source, [&path("ui/bad.rveui")]).unwrap_err();
        assert!(error.starts_with("/ui/bad.rveui"), "{error}");
    }

    #[test]
    fn missing_image_stops_loading_with_the_asset_label() {
        let source = Memory {
            texts: BTreeMap::from([("/ui/panel.rveui".to_owned(), PICTURE.to_owned())]),
            ..Memory::default()
        };
        let error = load_ui(&source, [&path("ui/panel.rveui")]).unwrap_err();
        assert!(error.contains("/ui/icon.png"), "{error}");
    }
}
