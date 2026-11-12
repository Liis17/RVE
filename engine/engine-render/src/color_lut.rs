//! 3D LUT цветокоррекции: проверка данных, упаковка в RGBA16F и привязка к composite.
//!
//! LUT хранится как 3D-изображение `size³`: x — красный, y — зелёный, z — синий вход, так что
//! красный меняется быстрее всего (порядок данных `.cube`). Composite (`tonemap.frag`) применяет
//! LUT после tonemapping и lift/gamma/gain к sRGB-кодированному цвету в `[0, 1]` и смешивает
//! результат с исходным по интенсивности. Без LUT к binding 4 привязана встроенная identity LUT.

use super::{RenderError, environment::float_to_half};
use ash::vk;

/// Наименьшая сторона LUT: интерполяции нужно хотя бы два узла на ось.
pub const MIN_COLOR_LUT_SIZE: u32 = 2;
/// Наибольшая сторона LUT; `128³` RGBA16F занимает 16 МиБ.
pub const MAX_COLOR_LUT_SIZE: u32 = 128;
/// Формат LUT: линейная фильтрация RGBA16F обязательна в Vulkan.
pub(crate) const COLOR_LUT_FORMAT: vk::Format = vk::Format::R16G16B16A16_SFLOAT;
/// Сторона встроенной identity LUT.
pub(crate) const IDENTITY_LUT_SIZE: u32 = 2;

/// Проверяет сторону LUT и число узлов RGB.
pub(crate) fn validate_color_lut(size: u32, texels: &[[f32; 3]]) -> Result<(), RenderError> {
    if !(MIN_COLOR_LUT_SIZE..=MAX_COLOR_LUT_SIZE).contains(&size) {
        return Err(RenderError::InvalidTexture(format!(
            "сторона LUT {size} вне [{MIN_COLOR_LUT_SIZE}, {MAX_COLOR_LUT_SIZE}]"
        )));
    }
    let expected = (size as usize).pow(3);
    if texels.len() != expected {
        return Err(RenderError::InvalidTexture(format!(
            "LUT {size}³ требует {expected} узлов, получено {}",
            texels.len()
        )));
    }
    if texels.iter().flatten().any(|value| !value.is_finite()) {
        return Err(RenderError::InvalidTexture(
            "LUT содержит NaN или бесконечность".to_owned(),
        ));
    }
    Ok(())
}

/// Байты RGBA16F в порядке узлов; значения насыщаются диапазоном half, альфа равна 1.
pub(crate) fn color_lut_bytes(texels: &[[f32; 3]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(texels.len() * 8);
    for texel in texels {
        for channel in texel {
            bytes.extend_from_slice(
                &float_to_half(channel.clamp(-65_504.0, 65_504.0)).to_ne_bytes(),
            );
        }
        bytes.extend_from_slice(&0x3c00_u16.to_ne_bytes());
    }
    bytes
}

/// Тождественная LUT стороны `size`: узел `(r, g, b)` хранит свои координаты в `[0, 1]`.
pub(crate) fn identity_color_lut(size: u32) -> Vec<[f32; 3]> {
    let scale = (size - 1) as f32;
    let mut texels = Vec::with_capacity((size as usize).pow(3));
    for blue in 0..size {
        for green in 0..size {
            for red in 0..size {
                texels.push([
                    red as f32 / scale,
                    green as f32 / scale,
                    blue as f32 / scale,
                ]);
            }
        }
    }
    texels
}

/// Привязывает LUT вида к binding 4 набора постобработки (читает composite).
pub(crate) fn write_composite_lut(
    device: &ash::Device,
    set: vk::DescriptorSet,
    view: vk::ImageView,
) {
    let image_info = [vk::DescriptorImageInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(4)
        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
        .image_info(&image_info)];
    // SAFETY: the set and view are alive and the set is not used by in-flight work.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_matching_texel_count_within_size_limits() {
        assert!(validate_color_lut(2, &identity_color_lut(2)).is_ok());
        assert!(validate_color_lut(17, &identity_color_lut(17)).is_ok());
    }

    #[test]
    fn rejects_sizes_outside_the_limits() {
        assert!(validate_color_lut(1, &[[0.0; 3]]).is_err());
        let size = MAX_COLOR_LUT_SIZE + 1;
        assert!(validate_color_lut(size, &[]).is_err());
    }

    #[test]
    fn rejects_wrong_texel_count_and_non_finite_values() {
        let mut texels = identity_color_lut(2);
        texels.pop();
        assert!(validate_color_lut(2, &texels).is_err());
        let mut texels = identity_color_lut(2);
        texels[3][1] = f32::NAN;
        assert!(validate_color_lut(2, &texels).is_err());
    }

    #[test]
    fn identity_lut_varies_red_fastest_like_cube_files() {
        let lut = identity_color_lut(3);
        assert_eq!(lut.len(), 27);
        assert_eq!(lut[0], [0.0, 0.0, 0.0]);
        assert_eq!(lut[1], [0.5, 0.0, 0.0]);
        assert_eq!(lut[3], [0.0, 0.5, 0.0]);
        assert_eq!(lut[9], [0.0, 0.0, 0.5]);
        assert_eq!(lut[26], [1.0, 1.0, 1.0]);
    }

    #[test]
    fn bytes_are_rgba16f_with_opaque_alpha() {
        let bytes = color_lut_bytes(&[[0.0, 1.0, 2.0], [1.0e9, -1.0e9, 0.5]]);
        assert_eq!(bytes.len(), 16);
        let halves: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_ne_bytes(*pair))
            .collect();
        assert_eq!(
            halves,
            [
                0x0000, 0x3c00, 0x4000, 0x3c00, 0x7bff, 0xfbff, 0x3800, 0x3c00
            ]
        );
    }
}
