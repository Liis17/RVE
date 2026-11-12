//! Экранный ambient occlusion: GTAO по глубине цели сцены и bilateral denoise.
//!
//! Вход — depth цели сцены после prepass непрозрачных мешей; результат — R8-изображение,
//! которое mesh-шейдер читает через binding 8 набора кадра и применяет к непрямому свету.
//! Сырой AO считается в разрешении `post::ao_resolution` (половинном для `low`/`medium`);
//! денойз читает его вместе с глубиной полного разрешения и пишет отфильтрованный буфер размера
//! render-цели, поэтому денойз одновременно выполняет depth-aware upsample. Буферы живут в цели
//! сцены (per slot), создаются лениво и пересоздаются вместе с ней или при смене разрешения AO.

use super::post::AoParams;
use super::{
    DeviceImage, GpuMarks, GpuPass, OwnedDescriptorPool, RenderError, color_subresource_range,
    create_compute_pipeline, float_bytes, image_layout_barrier, mark_pass, pipeline_barrier,
};
use ash::vk;

const GTAO_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gtao.comp.spv"));
const GTAO_DENOISE_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/gtao_denoise.comp.spv"));

/// Формат AO: одна нормализованная компонента.
pub(crate) const AO_FORMAT: vk::Format = vk::Format::R8_UNORM;
const GROUP_SIZE: u32 = 8;
/// Размер push constants GTAO: три `vec4`. Денойз использует первые 16 байт.
const AO_PUSH_CONSTANTS_SIZE: usize = 48;

/// Пайплайны AO: binding 0 — depth, binding 1 — сырой AO (только денойз), binding 2 — storage.
#[derive(Clone, Copy)]
pub(crate) struct AoPipelines {
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    gtao: vk::Pipeline,
    denoise: vk::Pipeline,
}

impl AoPipelines {
    pub(crate) fn new(device: &ash::Device, cache: vk::PipelineCache) -> Result<Self, RenderError> {
        let bindings = [
            sampled_binding(0),
            sampled_binding(1),
            vk::DescriptorSetLayoutBinding::default()
                .binding(2)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let set_layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: the bindings match gtao.comp and gtao_denoise.comp.
        let set_layout = unsafe { device.create_descriptor_set_layout(&set_layout_info, None) }?;
        let set_layouts = [set_layout];
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(AO_PUSH_CONSTANTS_SIZE as u32)];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the set layout and 48-byte compute range match both shaders.
        let layout = match unsafe { device.create_pipeline_layout(&layout_info, None) } {
            Ok(layout) => layout,
            Err(error) => {
                // SAFETY: the set layout was created above and nothing references it.
                unsafe { device.destroy_descriptor_set_layout(set_layout, None) };
                return Err(error.into());
            }
        };
        let gtao = create_compute_pipeline(device, cache, layout, GTAO_SHADER);
        let denoise = gtao
            .as_ref()
            .ok()
            .map(|_| create_compute_pipeline(device, cache, layout, GTAO_DENOISE_SHADER));
        match (gtao, denoise) {
            (Ok(gtao), Some(Ok(denoise))) => Ok(Self {
                set_layout,
                layout,
                gtao,
                denoise,
            }),
            (gtao, denoise) => {
                for pipeline in [gtao.ok(), denoise.and_then(Result::ok)]
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
                Err(RenderError::InvalidState("пайплайны AO не созданы"))
            }
        }
    }

    /// Освобождает пайплайны и layout; вызывается после ожидания простоя устройства.
    pub(crate) fn destroy(self, device: &ash::Device) {
        // SAFETY: the renderer waits for device idle before destroying AO resources.
        unsafe {
            device.destroy_pipeline(self.gtao, None);
            device.destroy_pipeline(self.denoise, None);
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
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
}

/// Цель AO одной цели сцены: сырой и отфильтрованный буферы с наборами дескрипторов.
pub(crate) struct AoTarget {
    /// Разрешение сырого GTAO; отфильтрованный буфер совпадает с render-целью.
    pub(crate) raw_extent: vk::Extent2D,
    pub(crate) raw: DeviceImage,
    pub(crate) filtered: DeviceImage,
    raw_layout: vk::ImageLayout,
    filtered_layout: vk::ImageLayout,
    _pool: OwnedDescriptorPool,
    gtao_set: vk::DescriptorSet,
    denoise_set: vk::DescriptorSet,
}

impl AoTarget {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        extent: vk::Extent2D,
        raw_extent: vk::Extent2D,
        depth_view: vk::ImageView,
        set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RenderError> {
        let usage = vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED;
        let raw = DeviceImage::new(
            instance,
            device,
            physical_device,
            raw_extent,
            AO_FORMAT,
            usage,
            color_subresource_range(),
        )?;
        let filtered = DeviceImage::new(
            instance,
            device,
            physical_device,
            extent,
            AO_FORMAT,
            usage,
            color_subresource_range(),
        )?;
        // Оба набора используют общий layout с двумя sampled-биндингами (depth и сырой AO).
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(4),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
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
        // GTAO: depth (0) и storage сырого AO (2). Денойз: depth (0), сырой AO (1), storage (2).
        write_ao_set(device, sets[0], depth_view, None, raw.view);
        write_ao_set(device, sets[1], depth_view, Some(raw.view), filtered.view);
        Ok(Self {
            raw_extent,
            raw,
            filtered,
            raw_layout: vk::ImageLayout::UNDEFINED,
            filtered_layout: vk::ImageLayout::UNDEFINED,
            _pool: pool,
            gtao_set: sets[0],
            denoise_set: sets[1],
        })
    }
}

fn write_ao_set(
    device: &ash::Device,
    set: vk::DescriptorSet,
    depth: vk::ImageView,
    sampled_raw: Option<vk::ImageView>,
    storage: vk::ImageView,
) {
    let depth_info = [vk::DescriptorImageInfo::default()
        .image_view(depth)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let raw_info = sampled_raw.map(|view| {
        [vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)]
    });
    let storage_info = [vk::DescriptorImageInfo::default()
        .image_view(storage)
        .image_layout(vk::ImageLayout::GENERAL)];
    let mut writes = vec![
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&depth_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(2)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .image_info(&storage_info),
    ];
    if let Some(raw_info) = raw_info.as_ref() {
        writes.push(
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(1)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .image_info(raw_info),
        );
    }
    // SAFETY: the set and views are alive; the set is written before any command uses it.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

/// Параметры записи AO для цели: пайплайны, push GTAO и денойза.
pub(crate) struct AoRecord<'a> {
    pub(crate) target: &'a mut AoTarget,
    pub(crate) pipelines: AoPipelines,
    pub(crate) gtao: AoParams,
    pub(crate) near_far: [f32; 2],
}

/// Записывает GTAO в разрешении сырого буфера и денойз с upsample в `extent` render-цели.
/// Глубина должна быть в `SHADER_READ_ONLY_OPTIMAL`.
pub(crate) fn record_ambient_occlusion(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    extent: vk::Extent2D,
    record: AoRecord<'_>,
    timestamps: Option<GpuMarks>,
) {
    let AoRecord {
        target,
        pipelines,
        gtao,
        near_far,
    } = record;
    let raw_groups = (
        target.raw_extent.width.div_ceil(GROUP_SIZE),
        target.raw_extent.height.div_ceil(GROUP_SIZE),
    );
    let groups = (
        extent.width.div_ceil(GROUP_SIZE),
        extent.height.div_ceil(GROUP_SIZE),
    );
    let gtao_push = gtao.to_bytes();
    let denoise_push = float_bytes::<AO_PUSH_CONSTANTS_SIZE>(&[
        near_far[0],
        near_far[1],
        extent.width as f32,
        extent.height as f32,
    ]);
    transition_image(
        device,
        command_buffer,
        target.raw.image,
        &mut target.raw_layout,
        vk::ImageLayout::GENERAL,
    );
    dispatch(
        device,
        command_buffer,
        pipelines.gtao,
        pipelines.layout,
        target.gtao_set,
        &gtao_push,
        raw_groups,
    );
    transition_image(
        device,
        command_buffer,
        target.raw.image,
        &mut target.raw_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
    );
    transition_image(
        device,
        command_buffer,
        target.filtered.image,
        &mut target.filtered_layout,
        vk::ImageLayout::GENERAL,
    );
    dispatch(
        device,
        command_buffer,
        pipelines.denoise,
        pipelines.layout,
        target.denoise_set,
        &denoise_push,
        groups,
    );
    transition_image(
        device,
        command_buffer,
        target.filtered.image,
        &mut target.filtered_layout,
        vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL,
    );
    mark_pass(
        device,
        command_buffer,
        timestamps,
        GpuPass::AmbientOcclusion,
    );
}

fn dispatch(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    pipeline: vk::Pipeline,
    layout: vk::PipelineLayout,
    set: vk::DescriptorSet,
    push: &[u8],
    groups: (u32, u32),
) {
    let sets = [set];
    // SAFETY: the pipeline, layout, set and push range match the AO shader interface.
    unsafe {
        device.cmd_bind_pipeline(command_buffer, vk::PipelineBindPoint::COMPUTE, pipeline);
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            layout,
            0,
            &sets,
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            push,
        );
        device.cmd_dispatch(command_buffer, groups.0, groups.1, 1);
    }
}

/// Переход изображения AO и барьер, который публикует запись compute-шейдера следующему чтению.
fn transition_image(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    image: vk::Image,
    layout: &mut vk::ImageLayout,
    next: vk::ImageLayout,
) {
    let previous = *layout;
    let (source_stage, source_access) = if previous == vk::ImageLayout::UNDEFINED {
        (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE)
    } else {
        (
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_STORAGE_WRITE | vk::AccessFlags2::SHADER_SAMPLED_READ,
        )
    };
    let (destination_stage, destination_access) = if next == vk::ImageLayout::GENERAL {
        (
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
        )
    } else {
        (
            vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        )
    };
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            image,
            previous,
            next,
            source_stage,
            source_access,
            destination_stage,
            destination_access,
            color_subresource_range(),
        ),
    );
    *layout = next;
}
