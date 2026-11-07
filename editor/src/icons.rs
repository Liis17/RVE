//! Иконки папок для панели Project: растеризованы из `IconsPack` при сборке (`build.rs`).

use engine_render::{RenderError, Renderer};
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
    use std::path::Path;

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
