//! Игровой UI на GPU: атлас шрифтов и картинки, которые загружает renderer, и вершины батчей.
//!
//! Модуль не зависит от egui: снимок `engine-gui` переводится в `UiPrimitive` и рисуется в
//! `Frame.ui`. Атлас загружается только при новой версии, картинки считаются по владельцам
//! (префаб, сессия Play) и по путям сцен: общая картинка живёт, пока её держит хотя бы один
//! владелец или загрузившая её сцена.

use engine_gui::paint::{UiDrawList, UiImageData, UiPaint, UiTexture};
use engine_render::{RenderError, Renderer, UiImage, UiPrimitive, UiTextureId, UiVertex};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::sync::Arc;

/// Операции с UI-текстурами; отделены от `Renderer`, чтобы проверять учёт без GPU.
pub trait UiTextures {
    type Id: Copy;

    fn create(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<Self::Id, RenderError>;
    fn update(
        &mut self,
        texture: Self::Id,
        region: [u32; 4],
        rgba: &[u8],
    ) -> Result<(), RenderError>;
    fn free(&mut self, texture: Self::Id) -> Result<(), RenderError>;
}

impl UiTextures for Renderer {
    type Id = UiTextureId;

    fn create(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<UiTextureId, RenderError> {
        self.create_ui_texture(width, height, rgba)
    }

    fn update(
        &mut self,
        texture: UiTextureId,
        region: [u32; 4],
        rgba: &[u8],
    ) -> Result<(), RenderError> {
        self.update_ui_texture(texture, region, rgba)
    }

    fn free(&mut self, texture: UiTextureId) -> Result<(), RenderError> {
        self.free_ui_texture(texture)
    }
}

/// GPU-ресурсы игрового UI: атлас шрифтов по версии, картинки по пути с числом ссылок и буфер
/// вершин, пересобираемый только при новом draw list. `O` — владелец картинок (префаб, сессия).
pub struct GameUi<O> {
    /// Загруженная версия атласа и её текстура; `None` — атлас пустой.
    atlas: Option<(u64, Option<UiTextureId>)>,
    images: HashMap<String, (UiTextureId, usize)>,
    /// Пути, которые держит хотя бы одна сцена: её ссылка не снимается до `release_all`.
    scene_paths: HashSet<String>,
    owned: HashMap<O, Vec<String>>,
    vertices: Vec<UiVertex>,
    converted: Option<Arc<UiDrawList>>,
    missing_warned: HashSet<String>,
}

impl<O> Default for GameUi<O> {
    fn default() -> Self {
        Self {
            atlas: None,
            images: HashMap::new(),
            scene_paths: HashSet::new(),
            owned: HashMap::new(),
            vertices: Vec::new(),
            converted: None,
            missing_warned: HashSet::new(),
        }
    }
}

impl<O: Eq + Hash + Clone> GameUi<O> {
    /// Принимает картинки сцены по путям. Путь, который уже держит сцена, не меняется, а новый
    /// id этого пути освобождается сразу: он не попал ни в один снимок. Путь, уже загруженный
    /// префабом, получает ссылку сцены; текстура живёт, пока держит хотя бы один владелец.
    pub fn adopt_scene_images<G: UiTextures<Id = UiTextureId>>(
        &mut self,
        gpu: &mut G,
        images: HashMap<String, UiTextureId>,
    ) {
        for (path, id) in images {
            if !self.scene_paths.insert(path.clone()) {
                free_texture(gpu, id);
                continue;
            }
            match self.images.get_mut(&path) {
                Some((_, users)) => {
                    *users += 1;
                    free_texture(gpu, id);
                }
                None => {
                    self.images.insert(path, (id, 1));
                }
            }
        }
    }

    /// Картинки сцены, созданные при загрузке: каждая держится сценой до `release_all`.
    pub fn with_scene_images(images: HashMap<String, UiTextureId>) -> Self {
        Self {
            scene_paths: images.keys().cloned().collect(),
            images: images
                .into_iter()
                .map(|(path, id)| (path, (id, 1)))
                .collect(),
            ..Self::default()
        }
    }

    /// Загружает картинки владельца или добавляет ссылку к уже загруженным. Ошибка создания
    /// откатывает уже добавленные картинки этого вызова.
    pub fn attach<G: UiTextures<Id = UiTextureId>>(
        &mut self,
        gpu: &mut G,
        owner: O,
        images: &BTreeMap<String, Arc<UiImageData>>,
    ) -> Result<(), RenderError> {
        let mut attached: Vec<String> = Vec::with_capacity(images.len());
        for (path, image) in images {
            if let Err(error) = self.add_image(gpu, path, image) {
                for path in &attached {
                    self.release_image(gpu, path);
                }
                return Err(error);
            }
            attached.push(path.clone());
        }
        if let Some(previous) = self.owned.insert(owner, attached) {
            for path in previous {
                self.release_image(gpu, &path);
            }
        }
        Ok(())
    }

    /// Снимает ссылки владельца; картинка без ссылок освобождается.
    pub fn detach<G: UiTextures<Id = UiTextureId>>(&mut self, gpu: &mut G, owner: &O) {
        for path in self.owned.remove(owner).unwrap_or_default() {
            self.release_image(gpu, &path);
        }
    }

    /// Освобождает все UI-текстуры: атлас, картинки и буфер; вызывается до уничтожения renderer.
    pub fn release_all<G: UiTextures<Id = UiTextureId>>(&mut self, gpu: &mut G) {
        if let Some((_, Some(atlas))) = self.atlas.take() {
            free_texture(gpu, atlas);
        }
        for (_, (id, _)) in self.images.drain() {
            free_texture(gpu, id);
        }
        self.scene_paths.clear();
        self.owned.clear();
        self.vertices.clear();
        self.converted = None;
        self.missing_warned.clear();
    }

    /// Готовит кадр к `primitives`: загружает атлас новой версии и переводит вершины нового draw
    /// list. Картинку батча, которой нет среди загруженных, предупреждает один раз на путь.
    pub fn sync<G: UiTextures<Id = UiTextureId>>(
        &mut self,
        gpu: &mut G,
        paint: &UiPaint,
    ) -> Result<(), RenderError> {
        let loaded = self.atlas.map(|(version, _)| version);
        if atlas_upload_needed(loaded, paint.atlas.version) {
            let atlas = &paint.atlas;
            let texture = if atlas.size[0] > 0 && atlas.size[1] > 0 && !atlas.pixels.is_empty() {
                let rgba: Vec<u8> = atlas.pixels.iter().flatten().copied().collect();
                Some(gpu.create(atlas.size[0] as u32, atlas.size[1] as u32, &rgba)?)
            } else {
                None
            };
            // Старая текстура освобождается только после создания новой; при ошибке выше прежний
            // атлас остаётся, и следующий кадр повторит загрузку.
            if let Some((_, Some(old))) = self.atlas.replace((paint.atlas.version, texture)) {
                free_texture(gpu, old);
            }
        }
        self.convert(&paint.draw_list);
        self.warn_missing(paint);
        Ok(())
    }

    /// Примитивы кадра: батч draw list с текстурой renderer и отсечением. Нужен `sync` с тем же
    /// снимком; без него или при другом draw list результат пуст. Батч с картинкой, которой нет
    /// среди загруженных (например, префаб уже освобождён), пропускается.
    pub fn primitives<'a>(&'a self, paint: &'a UiPaint) -> Vec<UiPrimitive<'a>> {
        if !self.is_converted(&paint.draw_list) {
            return Vec::new();
        }
        let atlas = self.atlas.and_then(|(_, texture)| texture);
        let mut primitives = Vec::with_capacity(paint.draw_list.batches.len());
        for batch in &paint.draw_list.batches {
            let Some(texture) = batch_texture(&batch.texture, atlas, &self.images) else {
                continue;
            };
            let vertices = batch.vertices.start as usize..batch.vertices.end as usize;
            let indices = batch.indices.start as usize..batch.indices.end as usize;
            primitives.push(UiPrimitive {
                clip_rect: batch.clip_rect,
                image: UiImage::Texture(texture),
                vertices: &self.vertices[vertices],
                indices: &paint.draw_list.indices[indices],
            });
        }
        primitives
    }

    fn add_image<G: UiTextures<Id = UiTextureId>>(
        &mut self,
        gpu: &mut G,
        path: &str,
        image: &UiImageData,
    ) -> Result<(), RenderError> {
        if let Some((_, users)) = self.images.get_mut(path) {
            *users += 1;
            return Ok(());
        }
        let id = gpu.create(image.size[0], image.size[1], &image.rgba)?;
        self.images.insert(path.to_owned(), (id, 1));
        Ok(())
    }

    fn release_image<G: UiTextures<Id = UiTextureId>>(&mut self, gpu: &mut G, path: &str) {
        let Some((_, users)) = self.images.get_mut(path) else {
            return;
        };
        *users -= 1;
        if *users == 0
            && let Some((id, _)) = self.images.remove(path)
        {
            free_texture(gpu, id);
        }
    }

    /// Переводит вершины draw list в формат renderer, только если список новый.
    fn convert(&mut self, list: &Arc<UiDrawList>) {
        if self.is_converted(list) {
            return;
        }
        self.vertices.clear();
        self.vertices
            .extend(list.vertices.iter().map(|vertex| UiVertex {
                position: vertex.pos,
                uv: vertex.uv,
                color: vertex.color,
            }));
        self.converted = Some(Arc::clone(list));
    }

    fn is_converted(&self, list: &Arc<UiDrawList>) -> bool {
        self.converted
            .as_ref()
            .is_some_and(|converted| Arc::ptr_eq(converted, list))
    }

    fn warn_missing(&mut self, paint: &UiPaint) {
        let atlas = self.atlas.and_then(|(_, texture)| texture);
        for batch in &paint.draw_list.batches {
            if let UiTexture::Image(path) = &batch.texture
                && batch_texture(&batch.texture, atlas, &self.images).is_none()
                && self.missing_warned.insert(path.clone())
            {
                log::warn!("UI-картинка {path:?} не загружена: батч пропущен");
            }
        }
    }
}

/// Решает, нужна ли загрузка атласа: только когда версия отличается от последней загруженной.
pub fn atlas_upload_needed(loaded: Option<u64>, version: u64) -> bool {
    loaded != Some(version)
}

/// Текстура батча: атлас шрифтов или картинка по пути. `None` — картинки нет среди загруженных.
fn batch_texture(
    texture: &UiTexture,
    atlas: Option<UiTextureId>,
    images: &HashMap<String, (UiTextureId, usize)>,
) -> Option<UiTextureId> {
    match texture {
        UiTexture::FontAtlas => atlas,
        UiTexture::Image(path) => images.get(path).map(|(id, _)| *id),
    }
}

fn free_texture<G: UiTextures<Id = UiTextureId>>(gpu: &mut G, texture: UiTextureId) {
    if let Err(error) = gpu.free(texture) {
        log::error!("UI-текстура не освобождена: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_gui::paint::{UiBatch, UiVertex as GuiVertex};

    #[test]
    fn atlas_is_uploaded_only_when_its_version_changes() {
        assert!(atlas_upload_needed(None, 0), "первый снимок грузит атлас");
        assert!(
            !atlas_upload_needed(Some(3), 3),
            "неизменная версия без загрузки"
        );
        assert!(
            atlas_upload_needed(Some(3), 4),
            "новая версия заменяет атлас"
        );
        // Выброшенный снимок с версией 4 не меняет загруженную: следующий снимок с версией 5
        // снова решает по сравнению с загруженной версией.
        assert!(atlas_upload_needed(Some(3), 5));
    }

    fn paint_with_image(path: &str) -> UiPaint {
        UiPaint {
            draw_list: Arc::new(UiDrawList {
                vertices: vec![GuiVertex {
                    pos: [0.0; 2],
                    uv: [0.0; 2],
                    color: [255; 4],
                }],
                indices: vec![0, 0, 0],
                batches: vec![UiBatch {
                    texture: UiTexture::Image(path.to_owned()),
                    clip_rect: [0.0; 4],
                    vertices: 0..1,
                    indices: 0..3,
                }],
            }),
            atlas: Arc::default(),
        }
    }

    #[test]
    fn unknown_image_batch_is_warned_once_and_skipped() {
        let mut ui = GameUi::<u8>::default();
        let paint = paint_with_image("ui/gone.png");
        ui.convert(&paint.draw_list);
        ui.warn_missing(&paint);
        ui.warn_missing(&paint);
        assert_eq!(ui.missing_warned.len(), 1);
        assert!(ui.primitives(&paint).is_empty());
    }

    #[test]
    fn primitives_need_the_draw_list_of_the_same_snapshot() {
        let mut ui = GameUi::<u8>::default();
        let paint = paint_with_image("ui/gone.png");
        assert!(ui.primitives(&paint).is_empty(), "до sync вершин нет");
        ui.convert(&paint.draw_list);
        let other = paint_with_image("ui/gone.png");
        assert!(
            ui.primitives(&other).is_empty(),
            "чужой draw list не рисуется"
        );
    }

    /// Фейковые UI-текстуры: id — обёртка над usize, освобождения записываются.
    #[derive(Default)]
    struct FakeTextures {
        created: usize,
        freed: Vec<UiTextureId>,
    }

    impl UiTextures for FakeTextures {
        type Id = UiTextureId;

        fn create(
            &mut self,
            _width: u32,
            _height: u32,
            _rgba: &[u8],
        ) -> Result<UiTextureId, RenderError> {
            self.created += 1;
            Ok(fake_texture(self.created))
        }

        fn update(
            &mut self,
            _texture: UiTextureId,
            _region: [u32; 4],
            _rgba: &[u8],
        ) -> Result<(), RenderError> {
            Ok(())
        }

        fn free(&mut self, texture: UiTextureId) -> Result<(), RenderError> {
            self.freed.push(texture);
            Ok(())
        }
    }

    fn fake_texture(value: usize) -> UiTextureId {
        // SAFETY: UiTextureId — обёртка над одним usize; любое значение допустимо для теста.
        unsafe { std::mem::transmute(value) }
    }

    fn owned_image(path: &str) -> BTreeMap<String, Arc<UiImageData>> {
        BTreeMap::from([(
            path.to_owned(),
            Arc::new(UiImageData::premultiplied([1, 1], vec![0; 4])),
        )])
    }

    #[test]
    fn scene_image_keeps_a_loaded_path_after_its_owner_leaves_and_frees_duplicate_ids_once() {
        let mut gpu = FakeTextures::default();
        let mut ui = GameUi::<u8>::default();
        ui.attach(&mut gpu, 1, &owned_image("ui/p.png")).unwrap();
        assert_eq!(gpu.created, 1);

        ui.adopt_scene_images(
            &mut gpu,
            HashMap::from([("ui/p.png".to_owned(), fake_texture(90))]),
        );
        assert_eq!(
            gpu.freed,
            vec![fake_texture(90)],
            "дубликат освобождён сразу"
        );

        ui.detach(&mut gpu, &1);
        assert_eq!(gpu.freed.len(), 1, "сцена держит путь: текстура жива");

        ui.release_all(&mut gpu);
        assert_eq!(
            gpu.freed,
            vec![fake_texture(90), fake_texture(1)],
            "каждая текстура освобождена ровно один раз"
        );
    }

    #[test]
    fn repeated_scene_adoptions_of_one_path_keep_one_texture_and_free_the_rest_once() {
        let mut gpu = FakeTextures::default();
        let mut ui = GameUi::<u8>::default();
        for id in 10..14 {
            ui.adopt_scene_images(
                &mut gpu,
                HashMap::from([("ui/p.png".to_owned(), fake_texture(id))]),
            );
        }
        assert_eq!(
            gpu.freed,
            vec![fake_texture(11), fake_texture(12), fake_texture(13)],
            "каждый повтор пути освобождает только свой новый id"
        );

        ui.release_all(&mut gpu);
        assert_eq!(gpu.freed.len(), 4);
        assert_eq!(gpu.freed.last(), Some(&fake_texture(10)));
    }

    #[test]
    fn same_draw_list_is_not_reconverted() {
        let mut ui = GameUi::<u8>::default();
        let list = Arc::new(UiDrawList {
            vertices: vec![GuiVertex {
                pos: [1.0, 2.0],
                uv: [0.0; 2],
                color: [255; 4],
            }],
            indices: Vec::new(),
            batches: Vec::new(),
        });
        ui.convert(&list);
        assert_eq!(ui.vertices.len(), 1);
        ui.vertices.clear();
        ui.convert(&list);
        assert!(ui.vertices.is_empty(), "тот же Arc не пересчитывается");
        ui.convert(&Arc::new((*list).clone()));
        assert_eq!(ui.vertices.len(), 1, "новый список пересчитывается");
    }
}
