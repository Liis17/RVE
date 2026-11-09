//! Мост между выводом egui и UI pass в `engine-render`.

pub use crate::game::UiTextures;
use egui::epaint::{ClippedPrimitive, Primitive, Vertex};
use egui::{ImageData, Rect, TextureId, TexturesDelta};
use engine_render::{RenderError, UiImage, UiPrimitive, UiTextureId, UiVertex, ViewportId};
use std::collections::HashMap;

/// Текстуры, которыми управляет сам egui: шрифты и загруженные через него изображения.
pub struct ManagedTextures<T> {
    textures: HashMap<u64, T>,
}

impl<T> Default for ManagedTextures<T> {
    fn default() -> Self {
        Self {
            textures: HashMap::new(),
        }
    }
}

impl<T: Copy> ManagedTextures<T> {
    /// Применяет новые текстуры и обновления egui; вызывается до отрисовки кадра.
    pub fn apply_updates(
        &mut self,
        delta: &TexturesDelta,
        backend: &mut impl UiTextures<Id = T>,
    ) -> Result<(), RenderError> {
        for (id, deltas) in &delta.set {
            let TextureId::Managed(id) = *id else {
                continue;
            };
            for image_delta in deltas {
                let ImageData::Color(image) = &image_delta.image;
                let [width, height] = image.size.map(|side| side as u32);
                let rgba = image.as_raw();
                match image_delta.pos {
                    Some([x, y]) => {
                        let texture = *self.textures.get(&id).ok_or_else(|| {
                            RenderError::InvalidTexture(format!(
                                "egui обновляет неизвестную текстуру {id}"
                            ))
                        })?;
                        backend.update(texture, [x as u32, y as u32, width, height], rgba)?;
                    }
                    // Полная замена может менять размер, поэтому текстура создаётся заново.
                    None => {
                        let texture = backend.create(width, height, rgba)?;
                        if let Some(previous) = self.textures.insert(id, texture) {
                            backend.free(previous)?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Освобождает текстуры egui; вызывается после отрисовки кадра.
    pub fn apply_frees(
        &mut self,
        delta: &TexturesDelta,
        backend: &mut impl UiTextures<Id = T>,
    ) -> Result<(), RenderError> {
        for id in &delta.free {
            if let TextureId::Managed(id) = *id
                && let Some(texture) = self.textures.remove(&id)
            {
                backend.free(texture)?;
            }
        }
        Ok(())
    }

    /// Releases all egui-managed textures before the rendering backend is destroyed.
    pub fn clear(&mut self, backend: &mut impl UiTextures<Id = T>) -> Result<(), RenderError> {
        for (_, texture) in self.textures.drain() {
            backend.free(texture)?;
        }
        Ok(())
    }

    fn get(&self, id: u64) -> Option<T> {
        self.textures.get(&id).copied()
    }
}

/// Сетка egui с вершинами, переведёнными в физические пиксели.
pub struct ConvertedMesh<'a> {
    clip_rect: [f32; 4],
    image: UiImage,
    vertices: Vec<UiVertex>,
    indices: &'a [u32],
}

impl ConvertedMesh<'_> {
    pub fn primitive(&self) -> UiPrimitive<'_> {
        UiPrimitive {
            clip_rect: self.clip_rect,
            image: self.image,
            vertices: &self.vertices,
            indices: self.indices,
        }
    }
}

/// Соответствие идентификаторов egui ресурсам renderer.
#[derive(Default)]
pub struct UiBridge {
    pub managed: ManagedTextures<UiTextureId>,
    user: HashMap<u64, UiImage>,
    next_user: u64,
}

impl UiBridge {
    /// Регистрирует viewport; результат можно передавать в виджеты egui как текстуру.
    pub fn register_viewport(&mut self, viewport: ViewportId) -> TextureId {
        self.register(UiImage::Viewport(viewport))
    }

    /// Регистрирует UI-текстуру, которой владеет приложение.
    pub fn register_texture(&mut self, texture: UiTextureId) -> TextureId {
        self.register(UiImage::Texture(texture))
    }

    pub fn unregister(&mut self, id: TextureId) {
        if let TextureId::User(id) = id {
            self.user.remove(&id);
        }
    }

    fn register(&mut self, image: UiImage) -> TextureId {
        let id = self.next_user;
        self.next_user += 1;
        self.user.insert(id, image);
        TextureId::User(id)
    }

    /// Переводит примитивы egui в физические пиксели.
    ///
    /// `Callback` и сетки с неизвестной текстурой не имеют смысла для этого renderer и пропускаются.
    pub fn convert<'a>(
        &self,
        primitives: &'a [ClippedPrimitive],
        pixels_per_point: f32,
    ) -> Vec<ConvertedMesh<'a>> {
        primitives
            .iter()
            .filter_map(|clipped| {
                let Primitive::Mesh(mesh) = &clipped.primitive else {
                    return None;
                };
                let image = match mesh.texture_id {
                    TextureId::Managed(id) => self.managed.get(id).map(UiImage::Texture),
                    TextureId::User(id) => self.user.get(&id).copied(),
                }?;
                Some(ConvertedMesh {
                    clip_rect: physical_clip_rect(clipped.clip_rect, pixels_per_point),
                    image,
                    vertices: physical_vertices(&mesh.vertices, pixels_per_point),
                    indices: &mesh.indices,
                })
            })
            .collect()
    }
}

fn physical_clip_rect(rect: Rect, pixels_per_point: f32) -> [f32; 4] {
    [
        rect.min.x * pixels_per_point,
        rect.min.y * pixels_per_point,
        rect.max.x * pixels_per_point,
        rect.max.y * pixels_per_point,
    ]
}

fn physical_vertices(vertices: &[Vertex], pixels_per_point: f32) -> Vec<UiVertex> {
    vertices
        .iter()
        .map(|vertex| UiVertex {
            position: [
                vertex.pos.x * pixels_per_point,
                vertex.pos.y * pixels_per_point,
            ],
            uv: [vertex.uv.x, vertex.uv.y],
            color: vertex.color.to_array(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::ConsoleOverlay;
    use egui::epaint::{ImageDelta, Mesh, PaintCallback};
    use egui::{Color32, ColorImage, Context, RawInput, TextureOptions, pos2, vec2};
    use engine_console::{CommandCatalog, ConsoleLevel, ConsoleLine};
    use std::sync::Arc;

    #[derive(Default)]
    struct FakeBackend {
        next: u32,
        created: Vec<(u32, u32, u32)>,
        updated: Vec<(u32, [u32; 4])>,
        freed: Vec<u32>,
    }

    impl UiTextures for FakeBackend {
        type Id = u32;

        fn create(&mut self, width: u32, height: u32, _rgba: &[u8]) -> Result<u32, RenderError> {
            self.next += 1;
            self.created.push((self.next, width, height));
            Ok(self.next)
        }

        fn update(&mut self, id: u32, region: [u32; 4], _rgba: &[u8]) -> Result<(), RenderError> {
            self.updated.push((id, region));
            Ok(())
        }

        fn free(&mut self, id: u32) -> Result<(), RenderError> {
            self.freed.push(id);
            Ok(())
        }
    }

    fn image(width: usize, height: usize) -> Arc<ColorImage> {
        Arc::new(ColorImage::filled([width, height], Color32::WHITE))
    }

    fn set_delta(id: u64, pos: Option<[usize; 2]>, width: usize, height: usize) -> TexturesDelta {
        let mut delta = TexturesDelta::default();
        let image_delta = ImageDelta {
            image: ImageData::Color(image(width, height)),
            options: TextureOptions::LINEAR,
            pos,
        };
        delta
            .set
            .entry(TextureId::Managed(id))
            .or_default()
            .push(image_delta);
        delta
    }

    /// `TexturesDelta` требует явной очистки перед удалением.
    fn apply(
        textures: &mut ManagedTextures<u32>,
        mut delta: TexturesDelta,
        backend: &mut FakeBackend,
    ) {
        textures.apply_updates(&delta, backend).unwrap();
        delta.clear();
    }

    #[test]
    fn full_delta_creates_and_patch_updates_the_same_texture() {
        let mut backend = FakeBackend::default();
        let mut textures = ManagedTextures::default();
        apply(&mut textures, set_delta(7, None, 8, 8), &mut backend);
        apply(
            &mut textures,
            set_delta(7, Some([2, 3]), 4, 1),
            &mut backend,
        );
        assert_eq!(backend.created, [(1, 8, 8)]);
        assert_eq!(backend.updated, [(1, [2, 3, 4, 1])]);
        assert!(backend.freed.is_empty());
    }

    #[test]
    fn full_replacement_recreates_the_texture_and_frees_the_previous_one() {
        let mut backend = FakeBackend::default();
        let mut textures = ManagedTextures::default();
        apply(&mut textures, set_delta(7, None, 8, 8), &mut backend);
        apply(&mut textures, set_delta(7, None, 16, 16), &mut backend);
        assert_eq!(backend.created, [(1, 8, 8), (2, 16, 16)]);
        assert_eq!(backend.freed, [1]);
    }

    #[test]
    fn patch_of_an_unknown_texture_is_an_error() {
        let mut textures = ManagedTextures::default();
        let mut delta = set_delta(7, Some([0, 0]), 1, 1);
        let error = textures
            .apply_updates(&delta, &mut FakeBackend::default())
            .unwrap_err();
        delta.clear();
        assert!(matches!(error, RenderError::InvalidTexture(_)));
    }

    #[test]
    fn frees_release_only_known_managed_textures() {
        let mut backend = FakeBackend::default();
        let mut textures = ManagedTextures::default();
        apply(&mut textures, set_delta(7, None, 1, 1), &mut backend);
        let mut delta = TexturesDelta::default();
        delta.free.insert(TextureId::Managed(7));
        delta.free.insert(TextureId::Managed(8));
        delta.free.insert(TextureId::User(7));
        textures.apply_frees(&delta, &mut backend).unwrap();
        delta.clear();
        assert_eq!(backend.freed, [1]);
    }

    #[test]
    fn clear_releases_every_managed_texture() {
        let mut backend = FakeBackend::default();
        let mut textures = ManagedTextures::default();
        apply(&mut textures, set_delta(7, None, 1, 1), &mut backend);
        apply(&mut textures, set_delta(8, None, 2, 2), &mut backend);

        textures.clear(&mut backend).unwrap();

        let mut freed = backend.freed;
        freed.sort_unstable();
        assert_eq!(freed, [1, 2]);
    }

    #[test]
    fn console_context_recreated_after_close_reuploads_font_atlas_before_glyph_patches() {
        fn frame(
            context: &Context,
            overlay: &mut ConsoleOverlay,
            lines: &[ConsoleLine],
        ) -> egui::FullOutput {
            let rect = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
            context.run_ui(
                RawInput {
                    screen_rect: Some(rect),
                    ..Default::default()
                },
                |_ui| {
                    overlay.show(context, rect, lines, &CommandCatalog::default());
                },
            )
        }

        let first_line = ConsoleLine {
            level: ConsoleLevel::Info,
            source: "console".to_owned(),
            text: "first".to_owned(),
        };
        let context = Context::default();
        let mut overlay = ConsoleOverlay::default();
        overlay.set_open(true);
        let mut textures = ManagedTextures::default();
        let mut backend = FakeBackend::default();

        let mut first = frame(&context, &mut overlay, std::slice::from_ref(&first_line));
        assert!(
            first
                .textures_delta
                .set
                .values()
                .flatten()
                .any(|delta| delta.pos.is_none())
        );
        textures
            .apply_updates(&first.textures_delta, &mut backend)
            .unwrap();
        first.textures_delta.clear();

        overlay.set_open(false);
        textures.clear(&mut backend).unwrap();
        assert_eq!(backend.freed.len(), backend.created.len());
        drop(context);
        overlay.set_open(true);
        let context = Context::default();
        let mut reopened = frame(&context, &mut overlay, std::slice::from_ref(&first_line));
        assert!(
            reopened
                .textures_delta
                .set
                .values()
                .flatten()
                .any(|delta| delta.pos.is_none())
        );
        textures
            .apply_updates(&reopened.textures_delta, &mut backend)
            .unwrap();
        reopened.textures_delta.clear();

        let later_line = ConsoleLine {
            level: ConsoleLevel::Info,
            source: "console".to_owned(),
            text: "новый ёж".to_owned(),
        };
        let mut later = frame(&context, &mut overlay, &[first_line, later_line]);
        assert!(
            later
                .textures_delta
                .set
                .values()
                .flatten()
                .any(|delta| delta.pos.is_some())
        );
        textures
            .apply_updates(&later.textures_delta, &mut backend)
            .unwrap();
        later.textures_delta.clear();
    }

    #[test]
    fn points_are_scaled_to_physical_pixels() {
        let vertices = [Vertex {
            pos: pos2(10.0, 20.0),
            uv: pos2(0.25, 0.5),
            color: Color32::from_rgba_premultiplied(1, 2, 3, 4),
        }];
        let converted = physical_vertices(&vertices, 2.0);
        assert_eq!(converted[0].position, [20.0, 40.0]);
        assert_eq!(converted[0].uv, [0.25, 0.5]);
        assert_eq!(converted[0].color, [1, 2, 3, 4]);
        assert_eq!(
            physical_clip_rect(Rect::from_min_max(pos2(1.0, 2.0), pos2(3.0, 4.0)), 1.5),
            [1.5, 3.0, 4.5, 6.0]
        );
    }

    #[test]
    fn callbacks_and_unknown_textures_are_not_drawn() {
        let bridge = UiBridge::default();
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        let primitives = [
            ClippedPrimitive {
                clip_rect: rect,
                primitive: Primitive::Callback(PaintCallback {
                    rect,
                    callback: Arc::new(()),
                }),
            },
            ClippedPrimitive {
                clip_rect: rect,
                primitive: Primitive::Mesh(Mesh::with_texture(TextureId::Managed(1))),
            },
        ];
        assert!(bridge.convert(&primitives, 1.0).is_empty());
    }

    #[test]
    fn unregistered_user_textures_are_not_drawn() {
        let mut bridge = UiBridge::default();
        let rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        let id = TextureId::User(0);
        bridge.unregister(id);
        let primitives = [ClippedPrimitive {
            clip_rect: rect,
            primitive: Primitive::Mesh(Mesh::with_texture(id)),
        }];
        assert!(bridge.convert(&primitives, 1.0).is_empty());
    }
}
