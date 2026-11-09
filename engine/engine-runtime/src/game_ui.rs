//! Игровой UI рантайма: соединение `.rveui` с ресурсами проекта.
//!
//! Загрузка CPU-ассетов, регистрация в скриптах и ввод живут в `engine-script`, GPU-ресурсы —
//! в `engine-ui::game`. Здесь остаются чтение ресурсов проекта и выбор владельца префабов.

use crate::prefabs::PrefabToken;
use crate::scene::read_utf8;
use engine_assets::Assets;
use engine_files::VirtualPath;
use engine_gui::paint::UiPaint;
use engine_resources::ResourceHandle;
use std::sync::Arc;

pub(crate) use engine_gui::paint::UiImageData;
pub(crate) use engine_script::{PreparedUi, UiInputBridge, UiRegistry, UiTick};

/// GPU-ресурсы игрового UI рантайма; картинки префабов владеют своим токеном загрузки.
pub(crate) type GameUi = engine_ui::game::GameUi<PrefabToken>;

/// Пустой снимок UI для кадра без канвасов.
pub(crate) fn empty_paint() -> UiPaint {
    UiPaint {
        draw_list: Arc::default(),
        atlas: Arc::default(),
    }
}

/// Ресурсы проекта для загрузчика `.rveui`: тексты и шрифты через `ResourceHandle`, картинки через
/// `Assets` с декодированием PNG.
struct ProjectUiAssets<'a> {
    resources: &'a ResourceHandle,
    assets: &'a Assets,
}

impl engine_script::UiAssetSource for ProjectUiAssets<'_> {
    fn read_text(&self, label: &str) -> Result<String, String> {
        read_utf8(self.resources, label)
    }

    fn read_bytes(&self, label: &str) -> Result<Vec<u8>, String> {
        self.resources
            .read_all(label)
            .map_err(|error| format!("{label}: {error}"))?
            .wait()
            .map_err(|error| format!("{label}: {error}"))
    }

    fn read_image(&self, label: &str) -> Result<UiImageData, String> {
        let texture = self
            .assets
            .load_texture(label)
            .map_err(|error| format!("{label}: {error}"))?;
        Ok(UiImageData::premultiplied(
            [texture.width, texture.height],
            texture.rgba,
        ))
    }
}

/// Загружает документы из путей и все их ассеты с ресурсов проекта. См. `engine_script::load_ui`.
pub(crate) fn load_ui<'a>(
    resources: &ResourceHandle,
    assets: &Assets,
    paths: impl IntoIterator<Item = &'a VirtualPath>,
) -> Result<PreparedUi, String> {
    engine_script::load_ui(&ProjectUiAssets { resources, assets }, paths)
}
