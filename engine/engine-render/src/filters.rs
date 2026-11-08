//! Motion blur и depth of field: фильтры HDR-цвета по векторам скорости и глубине сцены.
//!
//! Проходы читают цвет, velocity и depth из цели сцены (уже в `SHADER_READ_ONLY_OPTIMAL`) и
//! пишут в два промежуточных HDR-изображения кадрового слота. Порядок ping-pong: motion blur
//! пишет первый буфер, DoF читает его и пишет второй; без motion blur DoF пишет первый.
//! Промежуточные изображения создаются под разрешение источника цвета, поэтому выборка по UV
//! совпадает по масштабу с velocity и depth.

use super::bloom::{draw_fullscreen, ensure_sampled};
use super::{
    DeviceImage, GpuMarks, GpuPass, OFFSCREEN_FORMAT, OwnedDescriptorPool, RenderError,
    color_subresource_range, create_post_pipeline, float_bytes, image_layout_barrier, mark_pass,
    offscreen_previous_use, pipeline_barrier,
};
use ash::vk;

const MOTION_BLUR_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/motion_blur.frag.spv"));
const DOF_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/dof.frag.spv"));

/// Размер push constants фильтров: два `vec4`.
pub(crate) const FILTER_PUSH_CONSTANTS_SIZE: usize = 32;

/// Пайплайны фильтров на общем layout: color (0), sampler (1), velocity (2), depth (3).
#[derive(Clone, Copy)]
pub(crate) struct FilterPipelines {
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    motion: vk::Pipeline,
    dof: vk::Pipeline,
}

impl FilterPipelines {
    pub(crate) fn new(device: &ash::Device, cache: vk::PipelineCache) -> Result<Self, RenderError> {
        let bindings = [
            sampled_binding(0),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::FRAGMENT),
            sampled_binding(2),
            sampled_binding(3),
        ];
        let set_layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: the bindings match motion_blur.frag and dof.frag.
        let set_layout = unsafe { device.create_descriptor_set_layout(&set_layout_info, None) }?;
        let set_layouts = [set_layout];
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::FRAGMENT)
            .offset(0)
            .size(FILTER_PUSH_CONSTANTS_SIZE as u32)];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the set layout and the 32-byte fragment range match both shaders.
        let layout = match unsafe { device.create_pipeline_layout(&layout_info, None) } {
            Ok(layout) => layout,
            Err(error) => {
                // SAFETY: the set layout was created above and nothing references it.
                unsafe { device.destroy_descriptor_set_layout(set_layout, None) };
                return Err(error.into());
            }
        };
        let motion = create_post_pipeline(
            device,
            cache,
            layout,
            OFFSCREEN_FORMAT,
            MOTION_BLUR_SHADER,
            false,
        );
        let dof = motion.as_ref().ok().map(|_| {
            create_post_pipeline(device, cache, layout, OFFSCREEN_FORMAT, DOF_SHADER, false)
        });
        match (motion, dof) {
            (Ok(motion), Some(Ok(dof))) => Ok(Self {
                set_layout,
                layout,
                motion,
                dof,
            }),
            (motion, dof) => {
                for pipeline in [motion.ok(), dof.and_then(Result::ok)]
                    .into_iter()
                    .flatten()
                {
                    // SAFETY: the partially created pipelines belong to this device and are unused.
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
                // SAFETY: no pipeline references the layouts after the cleanup above.
                unsafe {
                    device.destroy_pipeline_layout(layout, None);
                    device.destroy_descriptor_set_layout(set_layout, None);
                }
                Err(RenderError::InvalidState("пайплайны фильтров не созданы"))
            }
        }
    }

    /// Освобождает пайплайны и layout; вызывается после ожидания простоя устройства.
    pub(crate) fn destroy(self, device: &ash::Device) {
        // SAFETY: the renderer waits for device idle before destroying filter resources.
        unsafe {
            device.destroy_pipeline(self.motion, None);
            device.destroy_pipeline(self.dof, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }

    pub(crate) fn set_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }
}

fn sampled_binding(binding: u32) -> vk::DescriptorSetLayoutBinding<'static> {
    vk::DescriptorSetLayoutBinding::default()
        .binding(binding)
        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::FRAGMENT)
}

/// Два промежуточных HDR-изображения и наборы дескрипторов кадрового слота для одного
/// разрешения источника. Пересоздаются, когда меняется разрешение источника.
pub(crate) struct FilterScratch {
    pub(crate) extent: vk::Extent2D,
    images: [DeviceImage; 2],
    layouts: [vk::ImageLayout; 2],
    _pool: OwnedDescriptorPool,
    motion_set: vk::DescriptorSet,
    dof_set: vk::DescriptorSet,
}

impl FilterScratch {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RenderError> {
        let images = [
            DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                OFFSCREEN_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
            DeviceImage::new(
                instance,
                device,
                physical_device,
                extent,
                OFFSCREEN_FORMAT,
                vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
                color_subresource_range(),
            )?,
        ];
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(6),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLER)
                .descriptor_count(2),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(2)
            .pool_sizes(&pool_sizes);
        let pool = OwnedDescriptorPool::new(device, &pool_info)?;
        let layouts = [set_layout; 2];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool.handle)
            .set_layouts(&layouts);
        // SAFETY: the pool was sized for exactly two sets of this layout.
        let sets = unsafe { device.allocate_descriptor_sets(&allocate_info) }?;
        Ok(Self {
            extent,
            images,
            layouts: [vk::ImageLayout::UNDEFINED; 2],
            _pool: pool,
            motion_set: sets[0],
            dof_set: sets[1],
        })
    }

    /// Изображение результата: `index` из `record_filters`.
    pub(crate) fn image(&self, index: usize) -> &DeviceImage {
        &self.images[index]
    }
}

/// Параметры фильтров вида; данные эффектов уже пересчитаны в `post.rs`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FilterParams {
    pub(crate) motion: Option<[f32; 4]>,
    pub(crate) dof: Option<[f32; 4]>,
    pub(crate) near_far: [f32; 2],
}

/// Запись фильтров кадра: источник цвета и представления velocity и depth.
pub(crate) struct FilterRecord<'a> {
    pub(crate) scratch: &'a mut FilterScratch,
    pub(crate) pipelines: FilterPipelines,
    pub(crate) sampler: vk::Sampler,
    pub(crate) velocity: vk::ImageView,
    pub(crate) depth: vk::ImageView,
    pub(crate) params: FilterParams,
}

/// Записывает включённые фильтры; возвращает индекс промежуточного изображения с результатом
/// или `None`, если фильтров нет и результат — исходный цвет.
pub(crate) fn record_filters(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source: &DeviceImage,
    source_layout: &mut vk::ImageLayout,
    record: &mut FilterRecord<'_>,
    timestamps: Option<GpuMarks>,
) -> Option<usize> {
    if record.params.motion.is_none() && record.params.dof.is_none() {
        return None;
    }
    ensure_sampled(device, command_buffer, source.image, source_layout);
    let extent = record.scratch.extent;
    let texel = [1.0 / extent.width as f32, 1.0 / extent.height as f32];
    let mut input_view = source.view;
    let mut result = None;
    if let Some(motion) = record.params.motion {
        let set = record.scratch.motion_set;
        write_filter_set(device, set, source.view, record);
        let push = motion_push(texel, motion);
        transition_to_attachment(device, command_buffer, record.scratch, 0);
        draw_fullscreen(
            device,
            command_buffer,
            record.scratch.images[0].view,
            extent,
            false,
            record.pipelines.motion,
            record.pipelines.layout,
            set,
            &push,
        );
        transition_to_sampled(device, command_buffer, record.scratch, 0);
        input_view = record.scratch.images[0].view;
        result = Some(0);
    }
    if let Some(dof) = record.params.dof {
        let destination = if result.is_some() { 1 } else { 0 };
        let set = record.scratch.dof_set;
        write_filter_set(device, set, input_view, record);
        // Масштаб CoC задан на единицу высоты; умножаем на высоту источника.
        let dof = [dof[0], dof[1] * extent.height as f32, dof[2], dof[3]];
        let push = dof_push(texel, dof, record.params.near_far);
        transition_to_attachment(device, command_buffer, record.scratch, destination);
        draw_fullscreen(
            device,
            command_buffer,
            record.scratch.images[destination].view,
            extent,
            false,
            record.pipelines.dof,
            record.pipelines.layout,
            set,
            &push,
        );
        transition_to_sampled(device, command_buffer, record.scratch, destination);
        result = Some(destination);
    }
    mark_pass(device, command_buffer, timestamps, GpuPass::Filters);
    result
}

/// Motion blur: v0 = (texel, shutter, max px), v1 = (samples, 0, 0, 0).
fn motion_push(texel: [f32; 2], motion: [f32; 4]) -> [u8; FILTER_PUSH_CONSTANTS_SIZE] {
    float_bytes::<FILTER_PUSH_CONSTANTS_SIZE>(&[
        texel[0], texel[1], motion[0], motion[1], motion[2], 0.0, 0.0, 0.0,
    ])
}

/// DoF: v0 = (texel, near, far), v1 = (focus, масштаб CoC, максимум CoC, 0).
fn dof_push(
    texel: [f32; 2],
    dof: [f32; 4],
    near_far: [f32; 2],
) -> [u8; FILTER_PUSH_CONSTANTS_SIZE] {
    float_bytes::<FILTER_PUSH_CONSTANTS_SIZE>(&[
        texel[0],
        texel[1],
        near_far[0],
        near_far[1],
        dof[0],
        dof[1],
        dof[2],
        0.0,
    ])
}

/// Записывает набор фильтра: цвет (0), sampler (1), velocity (2) и depth (3) из цели сцены.
fn write_filter_set(
    device: &ash::Device,
    set: vk::DescriptorSet,
    color: vk::ImageView,
    record: &FilterRecord<'_>,
) {
    let color_info = [vk::DescriptorImageInfo::default()
        .image_view(color)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let sampler_info = [vk::DescriptorImageInfo::default().sampler(record.sampler)];
    let velocity_info = [vk::DescriptorImageInfo::default()
        .image_view(record.velocity)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let depth_info = [vk::DescriptorImageInfo::default()
        .image_view(record.depth)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let writes = [
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&color_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::SAMPLER)
            .image_info(&sampler_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(2)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&velocity_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(3)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&depth_info),
    ];
    // SAFETY: the set and views are alive and the set is not used by in-flight work.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

fn transition_to_attachment(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scratch: &mut FilterScratch,
    index: usize,
) {
    let previous = scratch.layouts[index];
    let (stage, access) = offscreen_previous_use(previous);
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            scratch.images[index].image,
            previous,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            stage,
            access,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            color_subresource_range(),
        ),
    );
    scratch.layouts[index] = vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL;
}

fn transition_to_sampled(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    scratch: &mut FilterScratch,
    index: usize,
) {
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            scratch.images[index].image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
            vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT,
            vk::AccessFlags2::COLOR_ATTACHMENT_WRITE,
            vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
            color_subresource_range(),
        ),
    );
    scratch.layouts[index] = vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL;
}
