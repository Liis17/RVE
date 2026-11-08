//! Bloom камеры: prefilter по порогу, цепочка downsample 13-tap и upsample с аддитивным
//! смешиванием. Цепочка хранит свечение без экспозиции; composite добавляет его до экспозиции
//! (см. `tonemap.frag`). Уровни и наборы дескрипторов живут в кадровом слоте цели и
//! пересоздаются вместе с ней.

use super::{
    DeviceImage, GpuMarks, GpuPass, OFFSCREEN_FORMAT, OwnedDescriptorPool, PostProcess,
    RenderError, color_subresource_range, create_post_pipeline, float_bytes, image_layout_barrier,
    mark_pass, offscreen_previous_use, pipeline_barrier, write_image_set,
};
use ash::vk;

const BLOOM_PREFILTER_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/bloom_prefilter.frag.spv"));
const BLOOM_DOWNSAMPLE_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/bloom_downsample.frag.spv"));
const BLOOM_UPSAMPLE_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/bloom_upsample.frag.spv"));

/// Размер push constants bloom-проходов: два `vec4`.
pub(crate) const BLOOM_PUSH_CONSTANTS_SIZE: usize = 32;

/// Пайплайны bloom на общем layout (один sampled image и sampler, push constants fragment).
#[derive(Clone, Copy)]
pub(crate) struct BloomPipelines {
    layout: vk::PipelineLayout,
    prefilter: vk::Pipeline,
    downsample: vk::Pipeline,
    upsample: vk::Pipeline,
}

impl BloomPipelines {
    pub(crate) fn new(
        device: &ash::Device,
        cache: vk::PipelineCache,
        set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RenderError> {
        let set_layouts = [set_layout];
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(BLOOM_PUSH_CONSTANTS_SIZE as u32)];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the set layout and 32-byte fragment range match the bloom shaders.
        let layout = unsafe { device.create_pipeline_layout(&layout_info, None) }?;
        let mut created = Vec::with_capacity(3);
        let result = (|| -> Result<(), RenderError> {
            for (shader, additive) in [
                (BLOOM_PREFILTER_SHADER, false),
                (BLOOM_DOWNSAMPLE_SHADER, false),
                (BLOOM_UPSAMPLE_SHADER, true),
            ] {
                created.push(create_post_pipeline(
                    device,
                    cache,
                    layout,
                    OFFSCREEN_FORMAT,
                    shader,
                    additive,
                )?);
            }
            Ok(())
        })();
        if let Err(error) = result {
            for pipeline in created {
                // SAFETY: the partially created pipelines belong to this device and are unused.
                unsafe { device.destroy_pipeline(pipeline, None) };
            }
            // SAFETY: the layout was created above and no pipeline references it.
            unsafe { device.destroy_pipeline_layout(layout, None) };
            return Err(error);
        }
        Ok(Self {
            layout,
            prefilter: created[0],
            downsample: created[1],
            upsample: created[2],
        })
    }

    /// Освобождает пайплайны и layout; вызывается после ожидания простоя устройства.
    pub(crate) fn destroy(self, device: &ash::Device) {
        // SAFETY: the renderer waits for device idle before destroying bloom resources.
        unsafe {
            device.destroy_pipeline(self.prefilter, None);
            device.destroy_pipeline(self.downsample, None);
            device.destroy_pipeline(self.upsample, None);
            device.destroy_pipeline_layout(self.layout, None);
        }
    }
}

/// Значения push constants bloom-прохода prefilter; остальные проходы используют только размер.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BloomPush {
    pub(crate) threshold: f32,
    pub(crate) knee: f32,
    pub(crate) exposure_ev: f32,
    /// `intensity * tint`: применяется в prefilter, поэтому composite не знает о масштабе.
    pub(crate) tint_intensity: [f32; 3],
    pub(crate) scatter: f32,
}

impl BloomPush {
    pub(crate) fn from_post(post: &PostProcess) -> Self {
        let bloom = &post.bloom;
        Self {
            threshold: bloom.threshold,
            knee: bloom.knee,
            exposure_ev: post.exposure.ev,
            tint_intensity: bloom.tint.map(|value| value * bloom.intensity),
            scatter: bloom.scatter,
        }
    }
}

/// Цепочка уровней bloom одной цели. Уровень 0 имеет половину разрешения рендера.
pub(crate) struct BloomChain {
    levels: Vec<DeviceImage>,
    extents: Vec<vk::Extent2D>,
    layouts: Vec<vk::ImageLayout>,
    _pool: OwnedDescriptorPool,
    /// Чтение HDR-источника; привязывается каждый кадр, так как источник меняется с TAA.
    prefilter_set: vk::DescriptorSet,
    /// `downsample_sets[i - 1]` читает уровень `i - 1` и пишет уровень `i`.
    downsample_sets: Vec<vk::DescriptorSet>,
    /// `upsample_sets[i]` читает уровень `i + 1` и добавляет результат в уровень `i`.
    upsample_sets: Vec<vk::DescriptorSet>,
}

/// Уровни, которые bloom строит для разрешения рендера; ноль уровней не создаёт цепочку.
impl BloomChain {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        render_extent: vk::Extent2D,
        levels: u32,
        set_layout: vk::DescriptorSetLayout,
        sampler: vk::Sampler,
    ) -> Result<Self, RenderError> {
        let level_count = levels.max(1) as usize;
        let mut extents = Vec::with_capacity(level_count);
        let mut extent = half_extent(render_extent);
        for _ in 0..level_count {
            extents.push(extent);
            extent = half_extent(extent);
        }
        let mut images = Vec::with_capacity(level_count);
        for &extent in &extents {
            images.push(DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                OFFSCREEN_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?);
        }
        let set_count = 1 + 2 * (level_count - 1);
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(set_count as u32),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(set_count as u32),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(set_count as u32)
            .pool_sizes(&pool_sizes);
        let pool = OwnedDescriptorPool::new(device, &pool_info)?;
        let set_layouts = vec![set_layout; set_count];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool.handle)
            .set_layouts(&set_layouts);
        // SAFETY: the pool was sized for exactly `set_count` sets of this layout.
        let sets = unsafe { device.allocate_descriptor_sets(&allocate_info) }?;
        let downsample_sets = sets[1..level_count].to_vec();
        let upsample_sets = sets[level_count..].to_vec();
        for (index, set) in downsample_sets.iter().enumerate() {
            write_image_set(device, *set, images[index].view, sampler);
        }
        for (index, set) in upsample_sets.iter().enumerate() {
            write_image_set(device, *set, images[index + 1].view, sampler);
        }
        Ok(Self {
            layouts: vec![vk::ImageLayout::UNDEFINED; level_count],
            levels: images,
            extents,
            _pool: pool,
            prefilter_set: sets[0],
            downsample_sets,
            upsample_sets,
        })
    }
}

fn half_extent(extent: vk::Extent2D) -> vk::Extent2D {
    vk::Extent2D {
        width: (extent.width / 2).max(1),
        height: (extent.height / 2).max(1),
    }
}

/// Bloom, который записывается в текущий кадр вида.
pub(crate) struct BloomRecord<'a> {
    pub(crate) chain: &'a mut BloomChain,
    pub(crate) pipelines: BloomPipelines,
    pub(crate) sampler: vk::Sampler,
    pub(crate) push: BloomPush,
}

/// Собирает запись bloom, если эффект запланирован и цепочка для цели создана.
pub(crate) fn bloom_record<'a>(
    chain: Option<&'a mut BloomChain>,
    pipelines: Option<BloomPipelines>,
    bloom_levels: u32,
    sampler: vk::Sampler,
    post: &PostProcess,
) -> Option<BloomRecord<'a>> {
    match (chain, pipelines) {
        (Some(chain), Some(pipelines)) if bloom_levels > 0 => Some(BloomRecord {
            chain,
            pipelines,
            sampler,
            push: BloomPush::from_post(post),
        }),
        _ => None,
    }
}

/// Переводит изображение в `SHADER_READ_ONLY_OPTIMAL` для чтения проходами постобработки.
pub(crate) fn ensure_sampled(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    layout: &mut vk::ImageLayout,
) {
    if *layout == vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL {
        return;
    }
    let (stage, access) = offscreen_previous_use(*layout);
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            image,
            *layout,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            stage,
            access,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    *layout = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

/// Записывает prefilter, downsample и upsample; возвращает view итогового уровня 0.
///
/// `source` должен быть в `SHADER_READ_ONLY_OPTIMAL` либо переводится сюда; `source_layout`
/// обновляется, чтобы последующий composite не повторял барьер.
pub(crate) fn record_bloom(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source: &DeviceImage,
    source_layout: &mut vk::ImageLayout,
    record: BloomRecord<'_>,
    timestamps: Option<GpuMarks>,
) -> vk::ImageView {
    ensure_sampled(device, command_buffer, source.image, source_layout);
    let BloomRecord {
        chain,
        pipelines,
        sampler,
        push,
    } = record;
    write_image_set(device, chain.prefilter_set, source.view, sampler);
    let level_count = chain.levels.len();
    let layout = pipelines.layout;

    let prefilter_push = float_bytes::<BLOOM_PUSH_CONSTANTS_SIZE>(&[
        push.threshold,
        push.knee,
        push.exposure_ev,
        0.0,
        push.tint_intensity[0],
        push.tint_intensity[1],
        push.tint_intensity[2],
        0.0,
    ]);
    to_attachment(device, command_buffer, chain, 0);
    draw_fullscreen(
        device,
        command_buffer,
        chain.levels[0].view,
        chain.extents[0],
        false,
        pipelines.prefilter,
        layout,
        chain.prefilter_set,
        &prefilter_push,
    );
    to_sampled(device, command_buffer, chain, 0);

    for level in 1..level_count {
        let source_extent = chain.extents[level - 1];
        let texel = texel_push(source_extent, 0.0);
        to_attachment(device, command_buffer, chain, level);
        draw_fullscreen(
            device,
            command_buffer,
            chain.levels[level].view,
            chain.extents[level],
            false,
            pipelines.downsample,
            layout,
            chain.downsample_sets[level - 1],
            &texel,
        );
        to_sampled(device, command_buffer, chain, level);
    }

    for level in (0..level_count.saturating_sub(1)).rev() {
        let source_extent = chain.extents[level + 1];
        let texel = texel_push(source_extent, push.scatter);
        to_attachment(device, command_buffer, chain, level);
        draw_fullscreen(
            device,
            command_buffer,
            chain.levels[level].view,
            chain.extents[level],
            true,
            pipelines.upsample,
            layout,
            chain.upsample_sets[level],
            &texel,
        );
        to_sampled(device, command_buffer, chain, level);
    }
    mark_pass(device, command_buffer, timestamps, GpuPass::Bloom);
    chain.levels[0].view
}

/// Привязывает итоговый bloom к входу composite (binding 2 набора постобработки).
pub(crate) fn write_composite_bloom(
    device: &ash::Device,
    set: vk::DescriptorSet,
    view: vk::ImageView,
) {
    let image_info = [vk::DescriptorImageInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(2)
        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
        .image_info(&image_info)];
    // SAFETY: the set and view are alive and the set is not used by in-flight work.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

/// Push constants downsample и upsample: обратный размер источника и scatter.
fn texel_push(source: vk::Extent2D, scatter: f32) -> [u8; BLOOM_PUSH_CONSTANTS_SIZE] {
    float_bytes::<BLOOM_PUSH_CONSTANTS_SIZE>(&[
        1.0 / source.width as f32,
        1.0 / source.height as f32,
        scatter,
        0.0,
    ])
}

fn to_attachment(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    chain: &mut BloomChain,
    level: usize,
) {
    let previous = chain.layouts[level];
    let (stage, access) = offscreen_previous_use(previous);
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            chain.levels[level].image,
            previous,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            stage,
            access,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_READ | vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        ),
    );
    chain.layouts[level] = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
}

fn to_sampled(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    chain: &mut BloomChain,
    level: usize,
) {
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            chain.levels[level].image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    chain.layouts[level] = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_fullscreen(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    view: vk::ImageView,
    extent: vk::Extent2D,
    load: bool,
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    set: vk::DescriptorSet,
    push: &[u8],
) {
    let attachment = [vk::RenderingAttachmentInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
        .load_op(if load {
            vk::AttachmentLoadOp::LOAD
        } else {
            vk::AttachmentLoadOp::DONT_CARE
        })
        .store_op(vk::AttachmentStoreOp::STORE)];
    let area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    };
    let rendering_info = vk::RenderingInfo::default()
        .render_area(area)
        .layer_count(1)
        .color_attachments(&attachment);
    let viewport = [vk::Viewport {
        x: 0.0,
        y: 0.0,
        width: extent.width as f32,
        height: extent.height as f32,
        min_depth: 0.0,
        max_depth: 1.0,
    }];
    // SAFETY: the target view, pipeline, layout and set belong to this device and the level
    // was transitioned to the colour attachment layout above.
    unsafe {
        device.cmd_begin_rendering(command_buffer, &rendering_info);
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::GRAPHICS, pipeline);
        device.cmd_set_viewport(command_buffer, 0, &viewport);
        device.cmd_set_scissor(command_buffer, 0, &[area]);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::GRAPHICS,
            layout,
            0,
            &[set],
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            layout,
            vk::ShaderStageFlags::FRAGMENT,
            0,
            push,
        );
        device.cmd_draw(command_buffer, 3, 1, 0, 0);
        device.cmd_end_rendering(command_buffer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prefilter в CPU-форме шейдера: возвращает цвет до экспозиции. Экспозиция влияет только на
    /// порог, поэтому одинаковый цвет при разных EV даёт разный вклад, но не разный цвет.
    fn prefilter(
        color: [f32; 3],
        threshold: f32,
        knee: f32,
        exposure_ev: f32,
        tint: [f32; 3],
    ) -> [f32; 3] {
        let exposure = exposure_ev.exp2();
        let luminance = (color[0] * exposure) * 0.2126
            + (color[1] * exposure) * 0.7152
            + (color[2] * exposure) * 0.0722;
        let knee = knee.max(1e-4);
        let mut soft = (luminance - (threshold - knee)).clamp(0.0, 2.0 * knee);
        soft = soft * soft * (0.25 / knee);
        let contribution = soft.max(luminance - threshold).max(0.0) / luminance.max(1e-4);
        [
            color[0] * contribution * tint[0],
            color[1] * contribution * tint[1],
            color[2] * contribution * tint[2],
        ]
    }

    #[test]
    fn prefilter_stores_colour_without_exposure_and_thresholds_exposed_luminance() {
        let bright = [4.0, 4.0, 4.0];
        // EV 0: яркость 4 проходит порог 2 с вкладом (4 - 2) / 4, колено нулевое.
        let unexposed = prefilter(bright, 2.0, 0.0, 0.0, [1.0; 3]);
        assert!((unexposed[0] - 2.0).abs() < 1e-3, "{unexposed:?}");
        // EV +1: экспонированная яркость 8, вклад (8 - 2) / 8; сохранённый цвет всё равно 4 * вклад.
        let exposed = prefilter(bright, 2.0, 0.0, 1.0, [1.0; 3]);
        assert!((exposed[0] - 3.0).abs() < 1e-3, "{exposed:?}");
    }

    #[test]
    fn prefilter_drops_dark_pixels_below_the_knee() {
        assert_eq!(prefilter([0.1; 3], 1.0, 0.5, 0.0, [1.0; 3]), [0.0; 3]);
    }

    #[test]
    fn prefilter_tint_and_intensity_scale_output_linearly() {
        let base = prefilter([3.0; 3], 1.0, 0.0, 0.0, [1.0; 3]);
        let tinted = prefilter([3.0; 3], 1.0, 0.0, 0.0, [2.0, 0.5, 1.0]);
        assert!((tinted[0] - 2.0 * base[0]).abs() < 1e-5);
        assert!((tinted[1] - 0.5 * base[1]).abs() < 1e-5);
        assert!((tinted[2] - base[2]).abs() < 1e-5);
    }

    #[test]
    fn push_constants_fold_intensity_and_tint_and_keep_exposure_separate() {
        let mut post = PostProcess::default();
        post.bloom.intensity = 2.0;
        post.bloom.tint = [0.5, 1.0, 0.25];
        post.bloom.threshold = 1.5;
        post.bloom.knee = 0.25;
        post.exposure.ev = -1.0;
        let push = BloomPush::from_post(&post);
        assert_eq!(push.tint_intensity, [1.0, 2.0, 0.5]);
        assert_eq!(push.threshold, 1.5);
        assert_eq!(push.knee, 0.25);
        assert_eq!(push.exposure_ev, -1.0);
        assert_eq!(push.scatter, post.bloom.scatter);
    }

    #[test]
    fn texel_push_uses_reciprocal_source_size_and_scatter() {
        let bytes = texel_push(
            vk::Extent2D {
                width: 4,
                height: 8,
            },
            0.5,
        );
        let values: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_ne_bytes(*chunk))
            .collect();
        assert_eq!(values, vec![0.25, 0.125, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn half_extent_never_drops_below_one_pixel() {
        assert_eq!(
            half_extent(vk::Extent2D {
                width: 1,
                height: 9
            }),
            vk::Extent2D {
                width: 1,
                height: 4
            }
        );
        assert_eq!(
            half_extent(vk::Extent2D {
                width: 1280,
                height: 720
            }),
            vk::Extent2D {
                width: 640,
                height: 360
            }
        );
    }
}
