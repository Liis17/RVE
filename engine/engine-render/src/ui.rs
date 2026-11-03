//! Публичные типы UI pass и его чистая логика, не требующая GPU.

use crate::RenderError;
use ash::vk;

/// Вершина UI pass.
///
/// `position` — физические пиксели от левого верхнего угла окна, `uv` — координаты
/// изображения, `color` — предумноженный RGBA в гамма-пространстве sRGB.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct UiVertex {
    pub position: [f32; 2],
    pub uv: [f32; 2],
    pub color: [u8; 4],
}

/// Идентификатор UI-текстуры; после освобождения значение не используется повторно.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct UiTextureId(pub(crate) usize);

/// Идентификатор viewport target; после освобождения значение не используется повторно.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ViewportId(pub(crate) usize);

/// Изображение, которым закрашиваются треугольники примитива.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiImage {
    Texture(UiTextureId),
    /// Результат scene pass этого же кадра; другой viewport использовать нельзя.
    Viewport(ViewportId),
}

/// Индексированные треугольники с общим изображением и прямоугольником отсечения.
///
/// `clip_rect` — `[min_x, min_y, max_x, max_y]` в физических пикселях.
#[derive(Clone, Copy, Debug)]
pub struct UiPrimitive<'a> {
    pub clip_rect: [f32; 4],
    pub image: UiImage,
    pub vertices: &'a [UiVertex],
    pub indices: &'a [u32],
}

/// Один вызов отрисовки внутри упакованных буферов UI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct UiDraw {
    pub image: UiImage,
    pub scissor: vk::Rect2D,
    pub first_index: u32,
    pub vertex_offset: i32,
    pub index_count: u32,
}

/// Вершины и индексы всех примитивов кадра для копирования в буферы слота.
#[derive(Debug, Default)]
pub(crate) struct PackedUi {
    pub vertices: Vec<u8>,
    pub indices: Vec<u8>,
    pub draws: Vec<UiDraw>,
}

/// Проверяет примитивы и складывает видимые в общие буферы.
///
/// Примитив без геометрии или с пустым после обрезки `clip_rect` не попадает в кадр.
pub(crate) fn pack_ui(
    primitives: &[UiPrimitive<'_>],
    extent: vk::Extent2D,
) -> Result<PackedUi, RenderError> {
    let mut packed = PackedUi::default();
    let mut vertex_count = 0_usize;
    let mut index_count = 0_usize;
    for primitive in primitives {
        validate_ui_geometry(primitive.vertices.len(), primitive.indices)?;
        if primitive.indices.is_empty() {
            continue;
        }
        let Some(scissor) = clip_to_extent(primitive.clip_rect, extent) else {
            continue;
        };
        let vertex_offset = i32::try_from(vertex_count)
            .map_err(|_| RenderError::InvalidMesh("слишком много вершин UI"))?;
        let first_index = u32::try_from(index_count)
            .map_err(|_| RenderError::InvalidMesh("слишком много индексов UI"))?;
        packed.draws.push(UiDraw {
            image: primitive.image,
            scissor,
            first_index,
            vertex_offset,
            // Длина среза не может превысить u32: индексы проверены и их сумма тоже.
            index_count: primitive.indices.len() as u32,
        });
        packed
            .vertices
            .extend_from_slice(ui_vertex_bytes(primitive.vertices));
        packed
            .indices
            .extend_from_slice(crate::index_bytes(primitive.indices));
        vertex_count += primitive.vertices.len();
        index_count += primitive.indices.len();
    }
    u32::try_from(index_count)
        .map_err(|_| RenderError::InvalidMesh("слишком много индексов UI"))?;
    Ok(packed)
}

fn validate_ui_geometry(vertex_count: usize, indices: &[u32]) -> Result<(), RenderError> {
    if !indices.len().is_multiple_of(3) {
        return Err(RenderError::InvalidMesh(
            "число индексов UI должно быть кратным трём",
        ));
    }
    if u32::try_from(indices.len()).is_err() {
        return Err(RenderError::InvalidMesh("слишком много индексов UI"));
    }
    if indices.iter().any(|&index| index as usize >= vertex_count) {
        return Err(RenderError::InvalidMesh(
            "индекс UI выходит за пределы вершин",
        ));
    }
    Ok(())
}

/// Обрезает прямоугольник отсечения до изображения; пустой или неконечный даёт `None`.
pub(crate) fn clip_to_extent(clip_rect: [f32; 4], extent: vk::Extent2D) -> Option<vk::Rect2D> {
    if !clip_rect.iter().all(|value| value.is_finite()) {
        return None;
    }
    let min_x = clip_rect[0].floor().clamp(0.0, extent.width as f32) as u32;
    let min_y = clip_rect[1].floor().clamp(0.0, extent.height as f32) as u32;
    let max_x = clip_rect[2].ceil().clamp(0.0, extent.width as f32) as u32;
    let max_y = clip_rect[3].ceil().clamp(0.0, extent.height as f32) as u32;
    if max_x <= min_x || max_y <= min_y {
        return None;
    }
    Some(vk::Rect2D {
        offset: vk::Offset2D {
            x: min_x as i32,
            y: min_y as i32,
        },
        extent: vk::Extent2D {
            width: max_x - min_x,
            height: max_y - min_y,
        },
    })
}

/// Проверяет, что обновляемая область лежит внутри текстуры и данные соответствуют её размеру.
pub(crate) fn validate_texture_region(
    region: [u32; 4],
    texture: vk::Extent2D,
    byte_count: usize,
) -> Result<(), RenderError> {
    let [x, y, width, height] = region;
    if width == 0 || height == 0 {
        return Err(RenderError::InvalidTexture(
            "размеры области должны быть больше нуля".to_owned(),
        ));
    }
    let fits = x
        .checked_add(width)
        .is_some_and(|right| right <= texture.width)
        && y.checked_add(height)
            .is_some_and(|bottom| bottom <= texture.height);
    if !fits {
        return Err(RenderError::InvalidTexture(format!(
            "область {width}x{height} в ({x}, {y}) выходит за текстуру {}x{}",
            texture.width, texture.height
        )));
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|texels| texels.checked_mul(4));
    if expected != Some(byte_count) {
        return Err(RenderError::InvalidTexture(format!(
            "для области {width}x{height} передано {byte_count} байт"
        )));
    }
    Ok(())
}

/// Проверяет размер viewport; ноль допустим и означает пропуск scene pass.
pub(crate) fn validate_viewport_size(
    width: u32,
    height: u32,
    max_size: u32,
) -> Result<(), RenderError> {
    if width > max_size || height > max_size {
        return Err(RenderError::InvalidConfig(
            "размер viewport превышает предел GPU",
        ));
    }
    Ok(())
}

/// В sRGB-формат запись идёт в линейном пространстве, поэтому UI-шейдеру нужен перевод.
pub(crate) fn is_srgb_format(format: vk::Format) -> bool {
    matches!(
        format,
        vk::Format::B8G8R8A8_SRGB | vk::Format::R8G8B8A8_SRGB | vk::Format::A8B8G8R8_SRGB_PACK32
    )
}

pub(crate) fn ui_vertex_bytes(vertices: &[UiVertex]) -> &[u8] {
    // SAFETY: UiVertex is #[repr(C)], contains only plain data and has no padding.
    unsafe { std::slice::from_raw_parts(vertices.as_ptr().cast(), size_of_val(vertices)) }
}

/// Ресурсы, которые ещё могут читать отправленные кадры.
///
/// Запись освобождается после завершения каждого ожидающего кадрового слота и связанного upload serial.
pub(crate) struct RetireQueue<T> {
    all_slots: u32,
    completed_upload_serial: u64,
    entries: Vec<(u32, u64, T)>,
}

impl<T> RetireQueue<T> {
    pub fn new(slot_count: usize) -> Self {
        Self {
            all_slots: (1_u32 << slot_count) - 1,
            completed_upload_serial: 0,
            entries: Vec::new(),
        }
    }

    pub fn retire(&mut self, resource: T, upload_serial: u64) {
        self.entries.push((self.all_slots, upload_serial, resource));
        self.collect();
    }

    /// Отмечает завершённый frame slot и собирает ресурсы, которые больше ничего не ожидают.
    pub fn slot_completed(&mut self, slot: usize) {
        for (pending, _, _) in &mut self.entries {
            *pending &= !(1_u32 << slot);
        }
        self.collect();
    }

    /// Отмечает завершённые GPU uploads и собирает ресурсы, не ожидающие кадры.
    pub fn upload_completed(&mut self, serial: u64) {
        self.completed_upload_serial = self.completed_upload_serial.max(serial);
        self.collect();
    }

    fn collect(&mut self) {
        self.entries.retain(|(pending, upload_serial, _)| {
            *pending != 0 || *upload_serial > self.completed_upload_serial
        });
    }

    /// Уничтожает всё сразу; допустимо только после `device_wait_idle`.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    const EXTENT: vk::Extent2D = vk::Extent2D {
        width: 100,
        height: 50,
    };

    fn vertices(count: usize) -> Vec<UiVertex> {
        vec![UiVertex::default(); count]
    }

    fn primitive<'a>(vertices: &'a [UiVertex], indices: &'a [u32]) -> UiPrimitive<'a> {
        UiPrimitive {
            clip_rect: [0.0, 0.0, 100.0, 50.0],
            image: UiImage::Texture(UiTextureId(0)),
            vertices,
            indices,
        }
    }

    #[test]
    fn ui_vertex_layout_matches_shader_inputs() {
        assert_eq!(size_of::<UiVertex>(), 20);
        assert_eq!(std::mem::offset_of!(UiVertex, position), 0);
        assert_eq!(std::mem::offset_of!(UiVertex, uv), 8);
        assert_eq!(std::mem::offset_of!(UiVertex, color), 16);
    }

    #[test]
    fn packing_concatenates_primitives_with_offsets() {
        let first = vertices(3);
        let second = vertices(4);
        let packed = pack_ui(
            &[
                primitive(&first, &[0, 1, 2]),
                primitive(&second, &[0, 1, 2, 1, 2, 3]),
            ],
            EXTENT,
        )
        .unwrap();
        assert_eq!(packed.vertices.len(), 7 * size_of::<UiVertex>());
        assert_eq!(packed.indices.len(), 9 * size_of::<u32>());
        assert_eq!(packed.draws.len(), 2);
        assert_eq!(packed.draws[1].first_index, 3);
        assert_eq!(packed.draws[1].vertex_offset, 3);
        assert_eq!(packed.draws[1].index_count, 6);
    }

    #[test]
    fn packing_skips_empty_and_fully_clipped_primitives() {
        let triangle = vertices(3);
        let mut clipped = primitive(&triangle, &[0, 1, 2]);
        clipped.clip_rect = [200.0, 0.0, 300.0, 50.0];
        let packed = pack_ui(&[clipped, primitive(&triangle, &[])], EXTENT).unwrap();
        assert!(packed.draws.is_empty());
        assert!(packed.vertices.is_empty());
        assert!(packed.indices.is_empty());
    }

    #[test]
    fn packing_rejects_partial_triangles_and_out_of_range_indices() {
        let triangle = vertices(3);
        assert!(matches!(
            pack_ui(&[primitive(&triangle, &[0, 1])], EXTENT),
            Err(RenderError::InvalidMesh(_))
        ));
        assert!(matches!(
            pack_ui(&[primitive(&triangle, &[0, 1, 3])], EXTENT),
            Err(RenderError::InvalidMesh(_))
        ));
    }

    #[test]
    fn clip_rect_expands_to_whole_pixels_and_stays_inside_extent() {
        assert_eq!(
            clip_to_extent([-5.0, 1.5, 10.2, 500.0], EXTENT),
            Some(vk::Rect2D {
                offset: vk::Offset2D { x: 0, y: 1 },
                extent: vk::Extent2D {
                    width: 11,
                    height: 49,
                },
            })
        );
    }

    #[test]
    fn empty_inverted_and_non_finite_clip_rects_are_rejected() {
        assert_eq!(clip_to_extent([5.0, 5.0, 5.0, 10.0], EXTENT), None);
        assert_eq!(clip_to_extent([9.0, 5.0, 5.0, 10.0], EXTENT), None);
        assert_eq!(clip_to_extent([f32::NAN, 0.0, 5.0, 5.0], EXTENT), None);
        assert_eq!(clip_to_extent([0.0, 0.0, f32::INFINITY, 5.0], EXTENT), None);
    }

    #[test]
    fn texture_region_must_fit_and_match_byte_count() {
        let texture = vk::Extent2D {
            width: 8,
            height: 8,
        };
        assert!(validate_texture_region([6, 6, 2, 2], texture, 16).is_ok());
        assert!(validate_texture_region([7, 0, 2, 1], texture, 8).is_err());
        assert!(validate_texture_region([0, 0, 1, 1], texture, 3).is_err());
        assert!(validate_texture_region([0, 0, 0, 1], texture, 0).is_err());
        assert!(validate_texture_region([u32::MAX, 0, 2, 1], texture, 8).is_err());
    }

    #[test]
    fn viewport_size_allows_zero_but_not_more_than_the_gpu_limit() {
        assert!(validate_viewport_size(0, 0, 16).is_ok());
        assert!(validate_viewport_size(16, 16, 16).is_ok());
        assert!(validate_viewport_size(17, 1, 16).is_err());
    }

    #[test]
    fn srgb_formats_are_recognized() {
        assert!(is_srgb_format(vk::Format::B8G8R8A8_SRGB));
        assert!(!is_srgb_format(vk::Format::B8G8R8A8_UNORM));
    }

    #[test]
    fn retired_resource_lives_until_every_slot_has_completed() {
        let resource = Rc::new(());
        let mut queue = RetireQueue::new(2);
        queue.retire(Rc::clone(&resource), 0);
        queue.slot_completed(0);
        queue.slot_completed(0);
        assert_eq!(Rc::strong_count(&resource), 2);
        queue.slot_completed(1);
        assert_eq!(Rc::strong_count(&resource), 1);
    }

    #[test]
    fn retired_resources_are_dropped_by_clear() {
        let resource = Rc::new(());
        let mut queue = RetireQueue::new(2);
        queue.retire(Rc::clone(&resource), 0);
        queue.clear();
        assert_eq!(Rc::strong_count(&resource), 1);
    }

    #[test]
    fn retired_resource_waits_for_frame_slots_after_its_upload_finishes() {
        let resource = Rc::new(());
        let mut queue = RetireQueue::new(2);
        queue.retire(Rc::clone(&resource), 7);
        queue.upload_completed(7);
        queue.slot_completed(0);
        assert_eq!(Rc::strong_count(&resource), 2);
        queue.slot_completed(1);
        assert_eq!(Rc::strong_count(&resource), 1);
    }

    #[test]
    fn retired_resource_keeps_waiting_for_upload_after_all_frame_slots() {
        let resource = Rc::new(());
        let mut queue = RetireQueue::new(2);
        queue.retire(Rc::clone(&resource), 3);
        queue.slot_completed(0);
        queue.slot_completed(1);
        assert_eq!(Rc::strong_count(&resource), 2);
        queue.upload_completed(2);
        assert_eq!(Rc::strong_count(&resource), 2);
        queue.upload_completed(3);
        assert_eq!(Rc::strong_count(&resource), 1);
    }
}
