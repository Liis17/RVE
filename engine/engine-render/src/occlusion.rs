//! GPU occlusion culling вида по Hi-Z пирамиде глубины (two-phase).
//!
//! Для включённого вида depth prepass делится на две фазы. Первая рисует непрозрачные draw'ы,
//! видимые в прошлом кадре этой цели; из её глубины строится Hi-Z (максимум глубины по
//! блокам 2x2), и `occlusion.comp` проверяет мировую AABB каждого draw'а. Вторая фаза
//! дорисовывает в prepass draw'ы, ставшие видимыми, а основной и прозрачный проходы рисуют
//! видимые и уже записанные в глубину первой фазой. Каждый instanced-батч остаётся одним
//! multi-draw `vkCmdDrawIndexedIndirect`: команда draw'а несёт `instanceCount` 0/1 и
//! `firstInstance` его экземпляра. Ошибочная
//! история влияет только на число окклюдеров первой фазы, но не на корректность: итоговый
//! тест консервативен.
//!
//! Draw без локальной AABB, skinned draw и draw с неконечной матрицей не проверяются и
//! считаются видимыми. Ресурсы живут в цели сцены (per slot) и пересоздаются вместе с ней.

use super::culling::LocalBounds;
use super::{
    DeviceBuffer, DeviceImage, DrawCommand, HostBuffer, OwnedDescriptorPool, RenderError,
    color_mip_subresource_range, create_compute_pipeline, image_layout_barrier, pipeline_barrier,
};
use ash::vk;

const HIZ_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hiz.comp.spv"));
const OCCLUSION_SHADER: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/occlusion.comp.spv"));

const HIZ_FORMAT: vk::Format = vk::Format::R32_SFLOAT;
const HIZ_GROUP_SIZE: u32 = 8;
const CULL_GROUP_SIZE: u32 = 64;
const HIZ_PUSH_CONSTANTS_SIZE: usize = 16;
const CULL_PUSH_CONSTANTS_SIZE: usize = 96;
/// Размер `DrawInput` в std430: mat4, две vec4 и uvec4.
const DRAW_INPUT_SIZE: usize = 112;
/// Размер `VkDrawIndexedIndirectCommand`.
pub(crate) const INDIRECT_COMMAND_SIZE: u32 = 20;
const FLAG_TEST: u32 = 1;
const MODE_EARLY: u32 = 0;
const MODE_CULL: u32 = 1;

/// Пайплайны построения Hi-Z и теста видимости.
#[derive(Clone, Copy)]
pub(crate) struct OcclusionPipelines {
    hiz_set_layout: vk::DescriptorSetLayout,
    hiz_layout: vk::PipelineLayout,
    hiz: vk::Pipeline,
    cull_set_layout: vk::DescriptorSetLayout,
    cull_layout: vk::PipelineLayout,
    cull: vk::Pipeline,
}

impl OcclusionPipelines {
    pub(crate) fn new(device: &ash::Device, cache: vk::PipelineCache) -> Result<Self, RenderError> {
        let mut created = PartialPipelines::default();
        match created.build(device, cache) {
            Ok(pipelines) => Ok(pipelines),
            Err(error) => {
                created.destroy(device);
                Err(error)
            }
        }
    }

    /// Освобождает пайплайны и layout; вызывается после ожидания простоя устройства.
    pub(crate) fn destroy(self, device: &ash::Device) {
        PartialPipelines {
            hiz_set_layout: Some(self.hiz_set_layout),
            hiz_layout: Some(self.hiz_layout),
            hiz: Some(self.hiz),
            cull_set_layout: Some(self.cull_set_layout),
            cull_layout: Some(self.cull_layout),
            cull: Some(self.cull),
        }
        .destroy(device);
    }
}

/// Частично созданные объекты пайплайнов; освобождаются при ошибке создания.
#[derive(Default)]
struct PartialPipelines {
    hiz_set_layout: Option<vk::DescriptorSetLayout>,
    hiz_layout: Option<vk::PipelineLayout>,
    hiz: Option<vk::Pipeline>,
    cull_set_layout: Option<vk::DescriptorSetLayout>,
    cull_layout: Option<vk::PipelineLayout>,
    cull: Option<vk::Pipeline>,
}

impl PartialPipelines {
    fn build(
        &mut self,
        device: &ash::Device,
        cache: vk::PipelineCache,
    ) -> Result<OcclusionPipelines, RenderError> {
        let hiz_bindings = [
            binding(0, vk::DescriptorType::SAMPLED_IMAGE),
            binding(1, vk::DescriptorType::STORAGE_IMAGE),
        ];
        let hiz_set_layout = create_set_layout(device, &hiz_bindings)?;
        self.hiz_set_layout = Some(hiz_set_layout);
        let hiz_layout = create_layout(device, hiz_set_layout, HIZ_PUSH_CONSTANTS_SIZE)?;
        self.hiz_layout = Some(hiz_layout);
        let hiz = create_compute_pipeline(device, cache, hiz_layout, HIZ_SHADER)?;
        self.hiz = Some(hiz);

        let cull_bindings = [
            binding(0, vk::DescriptorType::STORAGE_BUFFER),
            binding(1, vk::DescriptorType::STORAGE_BUFFER),
            binding(2, vk::DescriptorType::STORAGE_BUFFER),
            binding(3, vk::DescriptorType::SAMPLED_IMAGE),
            binding(4, vk::DescriptorType::STORAGE_BUFFER),
        ];
        let cull_set_layout = create_set_layout(device, &cull_bindings)?;
        self.cull_set_layout = Some(cull_set_layout);
        let cull_layout = create_layout(device, cull_set_layout, CULL_PUSH_CONSTANTS_SIZE)?;
        self.cull_layout = Some(cull_layout);
        let cull = create_compute_pipeline(device, cache, cull_layout, OCCLUSION_SHADER)?;
        self.cull = Some(cull);
        Ok(OcclusionPipelines {
            hiz_set_layout,
            hiz_layout,
            hiz,
            cull_set_layout,
            cull_layout,
            cull,
        })
    }

    fn destroy(&mut self, device: &ash::Device) {
        // SAFETY: the objects belong to this device and no command buffer references them
        // (creation failed or the renderer waited for device idle).
        unsafe {
            for pipeline in [self.hiz.take(), self.cull.take()].into_iter().flatten() {
                device.destroy_pipeline(pipeline, None);
            }
            for layout in [self.hiz_layout.take(), self.cull_layout.take()]
                .into_iter()
                .flatten()
            {
                device.destroy_pipeline_layout(layout, None);
            }
            for layout in [self.hiz_set_layout.take(), self.cull_set_layout.take()]
                .into_iter()
                .flatten()
            {
                device.destroy_descriptor_set_layout(layout, None);
            }
        }
    }
}

fn binding(index: u32, ty: vk::DescriptorType) -> vk::DescriptorSetLayoutBinding<'static> {
    vk::DescriptorSetLayoutBinding::default()
        .binding(index)
        .descriptor_type(ty)
        .descriptor_count(1)
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
}

fn create_set_layout(
    device: &ash::Device,
    bindings: &[vk::DescriptorSetLayoutBinding<'_>],
) -> Result<vk::DescriptorSetLayout, RenderError> {
    let info = vk::DescriptorSetLayoutCreateInfo::default().bindings(bindings);
    // SAFETY: the bindings match hiz.comp or occlusion.comp.
    Ok(unsafe { device.create_descriptor_set_layout(&info, None) }?)
}

fn create_layout(
    device: &ash::Device,
    set_layout: vk::DescriptorSetLayout,
    push_size: usize,
) -> Result<vk::PipelineLayout, RenderError> {
    let set_layouts = [set_layout];
    let ranges = [vk::PushConstantRange::default()
        .stage_flags(vk::ShaderStageFlags::COMPUTE)
        .offset(0)
        .size(push_size as u32)];
    let info = vk::PipelineLayoutCreateInfo::default()
        .set_layouts(&set_layouts)
        .push_constant_ranges(&ranges);
    // SAFETY: the set layout is alive and the push range fits the guaranteed 128-byte minimum.
    Ok(unsafe { device.create_pipeline_layout(&info, None) }?)
}

/// Размеры уровней Hi-Z для depth заданного размера. Нулевой уровень — половина степени
/// двойки, покрывающей depth, поэтому каждый следующий уровень делится ровно пополам, как
/// mip-цепочка Vulkan, а texel уровня L покрывает пиксели depth `[x << (L + 1), (x + 1) << (L + 1))`.
/// Последний уровень — 1x1.
pub(crate) fn hiz_mip_extents(depth: vk::Extent2D) -> Vec<vk::Extent2D> {
    let half_power = |size: u32| (size.max(2).next_power_of_two() / 2).max(1);
    let mut extent = vk::Extent2D {
        width: half_power(depth.width),
        height: half_power(depth.height),
    };
    let mut extents = vec![extent];
    while extent.width > 1 || extent.height > 1 {
        extent = vk::Extent2D {
            width: (extent.width / 2).max(1),
            height: (extent.height / 2).max(1),
        };
        extents.push(extent);
    }
    extents
}

/// Входные данные теста одного draw'а в layout `DrawInput` шейдера.
///
/// `mesh` возвращает число индексов и локальную AABB меша draw'а.
pub(crate) fn pack_draw_inputs(
    bytes: &mut Vec<u8>,
    draws: &[DrawCommand],
    mut mesh: impl FnMut(&DrawCommand) -> Option<(u32, Option<LocalBounds>)>,
) {
    bytes.clear();
    bytes.reserve(draws.len() * DRAW_INPUT_SIZE);
    for draw in draws {
        let (index_count, bounds) = mesh(draw).unwrap_or((0, None));
        let finite_model = draw.model.into_iter().flatten().all(f32::is_finite);
        let tested = bounds.filter(|_| draw.skin.is_none() && finite_model);
        for column in draw.model {
            for value in column {
                bytes.extend_from_slice(&value.to_ne_bytes());
            }
        }
        let (min, max) = tested.map_or(([0.0; 3], [0.0; 3]), |bounds| (bounds.min, bounds.max));
        for corner in [min, max] {
            for value in corner {
                bytes.extend_from_slice(&value.to_ne_bytes());
            }
            bytes.extend_from_slice(&0.0_f32.to_ne_bytes());
        }
        let flags = if tested.is_some() { FLAG_TEST } else { 0 };
        for value in [index_count, flags, 0, 0] {
            bytes.extend_from_slice(&value.to_ne_bytes());
        }
    }
}

/// Буферы теста одной цели; вместимость в draw'ах растёт до степени двойки.
struct OcclusionBuffers {
    capacity: usize,
    inputs: HostBuffer,
    visible: DeviceBuffer,
    late: DeviceBuffer,
    /// Видимость прошлого кадра цели по индексу draw'а (u32).
    history: DeviceBuffer,
}

/// Hi-Z пирамида, буферы и наборы дескрипторов одной цели сцены.
pub(crate) struct OcclusionTarget {
    device: ash::Device,
    depth_extent: vk::Extent2D,
    hiz: DeviceImage,
    hiz_layout: vk::ImageLayout,
    mip_extents: Vec<vk::Extent2D>,
    mip_views: Vec<vk::ImageView>,
    _pool: OwnedDescriptorPool,
    hiz_sets: Vec<vk::DescriptorSet>,
    cull_set: vk::DescriptorSet,
    buffers: Option<OcclusionBuffers>,
    /// Повторно используемая упаковка входов кадра.
    staging: Vec<u8>,
    draw_count: u32,
}

impl OcclusionTarget {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        depth_extent: vk::Extent2D,
        depth_view: vk::ImageView,
        pipelines: OcclusionPipelines,
    ) -> Result<Self, RenderError> {
        let mip_extents = hiz_mip_extents(depth_extent);
        let levels = mip_extents.len() as u32;
        let hiz = DeviceImage::new(
            instance,
            device,
            physical_device,
            mip_extents[0],
            HIZ_FORMAT,
            vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::SAMPLED,
            color_mip_subresource_range(0, levels),
        )?;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(levels + 1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_IMAGE)
                .descriptor_count(levels),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(4),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(levels + 1)
            .pool_sizes(&pool_sizes);
        let pool = OwnedDescriptorPool::new(device, &pool_info)?;
        let mut target = Self {
            device: device.clone(),
            depth_extent,
            hiz,
            hiz_layout: vk::ImageLayout::UNDEFINED,
            mip_extents,
            mip_views: Vec::with_capacity(levels as usize),
            _pool: pool,
            hiz_sets: Vec::new(),
            cull_set: vk::DescriptorSet::null(),
            buffers: None,
            staging: Vec::new(),
            draw_count: 0,
        };
        for level in 0..levels {
            let view_info = vk::ImageViewCreateInfo::default()
                .image(target.hiz.image)
                .view_type(vk::ImageViewType::TYPE_2D)
                .format(HIZ_FORMAT)
                .subresource_range(color_mip_subresource_range(level, 1));
            // SAFETY: the view addresses one existing mip of the Hi-Z image; Drop destroys it.
            target
                .mip_views
                .push(unsafe { device.create_image_view(&view_info, None) }?);
        }
        let mut layouts = vec![pipelines.hiz_set_layout; levels as usize];
        layouts.push(pipelines.cull_set_layout);
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(target._pool.handle)
            .set_layouts(&layouts);
        // SAFETY: the pool was sized for these sets.
        let mut sets = unsafe { device.allocate_descriptor_sets(&allocate_info) }?;
        target.cull_set = sets.pop().ok_or(RenderError::InvalidState(
            "набор occlusion culling не создан",
        ))?;
        target.hiz_sets = sets;
        for (level, set) in target.hiz_sets.iter().enumerate() {
            let (source, source_layout) = if level == 0 {
                (depth_view, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            } else {
                (target.mip_views[level - 1], vk::ImageLayout::GENERAL)
            };
            write_hiz_set(device, *set, source, source_layout, target.mip_views[level]);
        }
        let hiz_info = [vk::DescriptorImageInfo::default()
            .image_view(target.hiz.view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(target.cull_set)
            .dst_binding(3)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&hiz_info)];
        // SAFETY: the set and the full-chain view are alive and not used by any submission yet.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(target)
    }

    /// Пишет входы теста кадра. Вызывается после ожидания fence слота цели: GPU уже не
    /// читает её буферы, поэтому их можно заменить при росте.
    pub(crate) fn prepare(
        &mut self,
        instance: &ash::Instance,
        physical_device: vk::PhysicalDevice,
        draws: &[DrawCommand],
        mesh: impl FnMut(&DrawCommand) -> Option<(u32, Option<LocalBounds>)>,
    ) -> Result<(), RenderError> {
        let count = draws.len().max(1);
        if self
            .buffers
            .as_ref()
            .is_none_or(|buffers| buffers.capacity < count)
        {
            // Старые буферы освобождаются до выделения новых: слот уже не использует их.
            self.buffers = None;
            let capacity = count.next_power_of_two();
            let usage =
                vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::INDIRECT_BUFFER;
            let args_size = capacity * INDIRECT_COMMAND_SIZE as usize;
            let buffers = OcclusionBuffers {
                capacity,
                inputs: HostBuffer::new_empty(
                    instance,
                    &self.device,
                    physical_device,
                    vk::BufferUsageFlags::STORAGE_BUFFER,
                    capacity * DRAW_INPUT_SIZE,
                )?,
                visible: DeviceBuffer::new(
                    instance,
                    &self.device,
                    physical_device,
                    usage,
                    args_size,
                )?,
                late: DeviceBuffer::new(instance, &self.device, physical_device, usage, args_size)?,
                history: DeviceBuffer::new(
                    instance,
                    &self.device,
                    physical_device,
                    vk::BufferUsageFlags::STORAGE_BUFFER,
                    capacity * std::mem::size_of::<u32>(),
                )?,
            };
            write_buffer_set(&self.device, self.cull_set, &buffers);
            self.buffers = Some(buffers);
        }
        pack_draw_inputs(&mut self.staging, draws, mesh);
        let buffers = self.buffers.as_ref().ok_or(RenderError::InvalidState(
            "буферы occlusion culling отсутствуют",
        ))?;
        buffers.inputs.write(&self.staging)?;
        self.draw_count = draws.len() as u32;
        Ok(())
    }

    /// Indirect-буфер итоговой видимости (первая фаза prepass, основной и прозрачный проходы).
    pub(crate) fn visible_buffer(&self) -> Option<vk::Buffer> {
        self.buffers.as_ref().map(|buffers| buffers.visible.buffer)
    }

    /// Indirect-буфер второй фазы prepass.
    pub(crate) fn late_buffer(&self) -> Option<vk::Buffer> {
        self.buffers.as_ref().map(|buffers| buffers.late.buffer)
    }
}

impl Drop for OcclusionTarget {
    fn drop(&mut self) {
        for view in self.mip_views.drain(..) {
            // SAFETY: the target is dropped after the submissions using it completed; the mip
            // views are destroyed before the Hi-Z image field.
            unsafe { self.device.destroy_image_view(view, None) };
        }
    }
}

fn write_hiz_set(
    device: &ash::Device,
    set: vk::DescriptorSet,
    source: vk::ImageView,
    source_layout: vk::ImageLayout,
    destination: vk::ImageView,
) {
    let source_info = [vk::DescriptorImageInfo::default()
        .image_view(source)
        .image_layout(source_layout)];
    let destination_info = [vk::DescriptorImageInfo::default()
        .image_view(destination)
        .image_layout(vk::ImageLayout::GENERAL)];
    let writes = [
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&source_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
            .image_info(&destination_info),
    ];
    // SAFETY: the set and views are alive and the set is not used by in-flight work.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

fn write_buffer_set(device: &ash::Device, set: vk::DescriptorSet, buffers: &OcclusionBuffers) {
    let infos = [
        (0, buffers.inputs.buffer),
        (1, buffers.visible.buffer),
        (2, buffers.late.buffer),
        (4, buffers.history.buffer),
    ]
    .map(|(binding, buffer)| {
        (
            binding,
            [vk::DescriptorBufferInfo::default()
                .buffer(buffer)
                .offset(0)
                .range(vk::WHOLE_SIZE)],
        )
    });
    let writes: Vec<_> = infos
        .iter()
        .map(|(binding, info)| {
            vk::WriteDescriptorSet::default()
                .dst_set(set)
                .dst_binding(*binding)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .buffer_info(info)
        })
        .collect();
    // SAFETY: the set belongs to this target; its previous buffers are no longer used by the GPU.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

/// Готовит аргументы первой фазы prepass из видимости прошлого кадра цели.
pub(crate) fn record_early(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    pipelines: OcclusionPipelines,
    target: &OcclusionTarget,
    instance_base: u32,
) {
    // Прошлый кадр цели читал аргументы как indirect и писал их и историю compute-шейдером.
    memory_barrier(
        device,
        command_buffer,
        vk::PipelineStageFlags2::DRAW_INDIRECT | vk::PipelineStageFlags2::COMPUTE_SHADER,
        vk::AccessFlags2::INDIRECT_COMMAND_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
        vk::PipelineStageFlags2::COMPUTE_SHADER,
        vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
    );
    dispatch_cull(
        device,
        command_buffer,
        pipelines,
        target,
        [[0.0; 4]; 4],
        MODE_EARLY,
        instance_base,
    );
    publish_indirect(device, command_buffer);
}

/// Строит Hi-Z из depth первой фазы и пишет итоговые аргументы и аргументы второй фазы.
/// Depth цели должен быть в `SHADER_READ_ONLY_OPTIMAL`. `instance_base` — слот экземпляра
/// первого draw'а вида в буфере экземпляров кадра.
pub(crate) fn record_cull(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    pipelines: OcclusionPipelines,
    target: &mut OcclusionTarget,
    view_projection: [[f32; 4]; 4],
    instance_base: u32,
) {
    let (source_stage, source_access) = if target.hiz_layout == vk::ImageLayout::UNDEFINED {
        (vk::PipelineStageFlags2::NONE, vk::AccessFlags2::NONE)
    } else {
        (
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
        )
    };
    pipeline_barrier(
        device,
        command_buffer,
        image_layout_barrier(
            target.hiz.image,
            target.hiz_layout,
            vk::ImageLayout::GENERAL,
            source_stage,
            source_access,
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_STORAGE_WRITE,
            color_mip_subresource_range(0, target.mip_extents.len() as u32),
        ),
    );
    target.hiz_layout = vk::ImageLayout::GENERAL;
    let mut source = target.depth_extent;
    for (level, extent) in target.mip_extents.iter().enumerate() {
        let push = int_bytes([
            source.width as i32,
            source.height as i32,
            extent.width as i32,
            extent.height as i32,
        ]);
        // SAFETY: the pipeline, layout and set match hiz.comp; the source level was published
        // by the barrier after the previous dispatch.
        unsafe {
            device.cmd_bind_pipeline(
                command_buffer,
                vk::PipelineBindPoint::COMPUTE,
                pipelines.hiz,
            );
            device.cmd_bind_descriptor_sets(
                command_buffer,
                vk::PipelineBindPoint::COMPUTE,
                pipelines.hiz_layout,
                0,
                &[target.hiz_sets[level]],
                &[],
            );
            device.cmd_push_constants(
                command_buffer,
                pipelines.hiz_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &push,
            );
            device.cmd_dispatch(
                command_buffer,
                extent.width.div_ceil(HIZ_GROUP_SIZE),
                extent.height.div_ceil(HIZ_GROUP_SIZE),
                1,
            );
        }
        memory_barrier(
            device,
            command_buffer,
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_STORAGE_WRITE,
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_SAMPLED_READ,
        );
        source = *extent;
    }
    dispatch_cull(
        device,
        command_buffer,
        pipelines,
        target,
        view_projection,
        MODE_CULL,
        instance_base,
    );
    publish_indirect(device, command_buffer);
}

fn dispatch_cull(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    pipelines: OcclusionPipelines,
    target: &OcclusionTarget,
    view_projection: [[f32; 4]; 4],
    mode: u32,
    instance_base: u32,
) {
    let mut push = [0_u8; CULL_PUSH_CONSTANTS_SIZE];
    let floats = view_projection.into_iter().flatten().chain([
        target.depth_extent.width as f32,
        target.depth_extent.height as f32,
        target.mip_extents[0].width as f32,
        target.mip_extents[0].height as f32,
    ]);
    for (chunk, value) in push.as_chunks_mut::<4>().0.iter_mut().zip(floats) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    let params = [
        target.draw_count,
        mode,
        target.mip_extents.len() as u32,
        instance_base,
    ];
    for (chunk, value) in push[80..].as_chunks_mut::<4>().0.iter_mut().zip(params) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    // SAFETY: the pipeline, layout and set match occlusion.comp; prepare() bound the buffers.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            pipelines.cull,
        );
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            pipelines.cull_layout,
            0,
            &[target.cull_set],
            &[],
        );
        device.cmd_push_constants(
            command_buffer,
            pipelines.cull_layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &push,
        );
        device.cmd_dispatch(
            command_buffer,
            target.draw_count.div_ceil(CULL_GROUP_SIZE),
            1,
            1,
        );
    }
}

/// Публикует записанные compute-шейдером аргументы для indirect-чтения.
fn publish_indirect(device: &ash::Device, command_buffer: vk::CommandBuffer) {
    memory_barrier(
        device,
        command_buffer,
        vk::PipelineStageFlags2::COMPUTE_SHADER,
        vk::AccessFlags2::SHADER_STORAGE_WRITE,
        vk::PipelineStageFlags2::DRAW_INDIRECT | vk::PipelineStageFlags2::COMPUTE_SHADER,
        vk::AccessFlags2::INDIRECT_COMMAND_READ | vk::AccessFlags2::SHADER_STORAGE_READ,
    );
}

fn memory_barrier(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source_stage: vk::PipelineStageFlags2,
    source_access: vk::AccessFlags2,
    destination_stage: vk::PipelineStageFlags2,
    destination_access: vk::AccessFlags2,
) {
    let barriers = [vk::MemoryBarrier2::default()
        .src_stage_mask(source_stage)
        .src_access_mask(source_access)
        .dst_stage_mask(destination_stage)
        .dst_access_mask(destination_access)];
    let dependency = vk::DependencyInfo::default().memory_barriers(&barriers);
    // SAFETY: the barrier only orders work recorded in this command buffer.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };
}

fn int_bytes(values: [i32; 4]) -> [u8; HIZ_PUSH_CONSTANTS_SIZE] {
    let mut bytes = [0; HIZ_PUSH_CONSTANTS_SIZE];
    for (chunk, value) in bytes.as_chunks_mut::<4>().0.iter_mut().zip(values) {
        chunk.copy_from_slice(&value.to_ne_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MeshId, SkinBinding};

    fn extent(width: u32, height: u32) -> vk::Extent2D {
        vk::Extent2D { width, height }
    }

    #[test]
    fn hiz_chain_matches_vulkan_mip_sizes() {
        let sizes: Vec<_> = hiz_mip_extents(extent(5, 3))
            .into_iter()
            .map(|extent| (extent.width, extent.height))
            .collect();
        assert_eq!(sizes, [(4, 2), (2, 1), (1, 1)]);
        for (width, height) in [(1, 1), (640, 360), (1920, 1080), (1366, 767), (3, 4000)] {
            let chain = hiz_mip_extents(extent(width, height));
            let base = chain[0];
            // Vulkan: размер уровня — max(1, base >> L), уровней — floor(log2(max)) + 1.
            assert_eq!(chain.len() as u32, base.width.max(base.height).ilog2() + 1);
            for (level, mip) in chain.iter().enumerate() {
                assert_eq!(mip.width, (base.width >> level).max(1), "{width}x{height}");
                assert_eq!(
                    mip.height,
                    (base.height >> level).max(1),
                    "{width}x{height}"
                );
            }
            assert_eq!(chain.last().copied(), Some(extent(1, 1)));
        }
        assert_eq!(hiz_mip_extents(extent(640, 360))[0], extent(512, 256));
    }

    #[test]
    fn hiz_texels_cover_power_of_two_pixel_blocks() {
        // Шейдер теста адресует texel уровня L как pixel >> (L + 1); цепочка должна вмещать
        // индекс последнего пикселя depth на каждом уровне.
        for (width, height) in [(1, 1), (7, 3), (640, 360), (1920, 1080), (1366, 767)] {
            for (level, mip) in hiz_mip_extents(extent(width, height)).iter().enumerate() {
                let shift = level + 1;
                assert!(
                    ((width - 1) >> shift) < mip.width,
                    "{width}x{height} L{level}"
                );
                assert!(
                    ((height - 1) >> shift) < mip.height,
                    "{width}x{height} L{level}"
                );
            }
        }
    }

    fn draw(model: [[f32; 4]; 4]) -> DrawCommand {
        DrawCommand {
            mesh: MeshId(0),
            material: None,
            model,
            previous_model: None,
            lod_level: 0,
            casts_shadow: false,
            skin: None,
        }
    }

    const IDENTITY: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    const BOUNDS: LocalBounds = LocalBounds {
        min: [-1.0, -2.0, -3.0],
        max: [1.0, 2.0, 3.0],
    };

    fn u32_at(bytes: &[u8], draw: usize, offset: usize) -> u32 {
        let start = draw * DRAW_INPUT_SIZE + offset;
        u32::from_ne_bytes(bytes[start..start + 4].try_into().unwrap())
    }

    fn f32_at(bytes: &[u8], draw: usize, offset: usize) -> f32 {
        f32::from_bits(u32_at(bytes, draw, offset))
    }

    #[test]
    fn packed_inputs_match_shader_layout() {
        let mut moved = IDENTITY;
        moved[3] = [4.0, 5.0, 6.0, 1.0];
        let mut bytes = Vec::new();
        pack_draw_inputs(&mut bytes, &[draw(moved)], |_| Some((36, Some(BOUNDS))));
        assert_eq!(bytes.len(), DRAW_INPUT_SIZE);
        assert_eq!(f32_at(&bytes, 0, 48), 4.0);
        assert_eq!(f32_at(&bytes, 0, 56), 6.0);
        assert_eq!(
            [0, 4, 8].map(|offset| f32_at(&bytes, 0, 64 + offset)),
            BOUNDS.min
        );
        assert_eq!(
            [0, 4, 8].map(|offset| f32_at(&bytes, 0, 80 + offset)),
            BOUNDS.max
        );
        assert_eq!(u32_at(&bytes, 0, 96), 36);
        assert_eq!(u32_at(&bytes, 0, 100), FLAG_TEST);
    }

    #[test]
    fn draws_without_reliable_bounds_are_never_tested() {
        let mut skinned = draw(IDENTITY);
        skinned.skin = Some(SkinBinding {
            instance: crate::skin::SkinId(0),
            palette: 0,
        });
        let mut broken = IDENTITY;
        broken[0][0] = f32::NAN;
        let draws = [skinned, draw(IDENTITY), draw(broken), draw(IDENTITY)];
        let mut meshes = [
            Some((6, Some(BOUNDS))),
            Some((6, None)),
            Some((6, Some(BOUNDS))),
            None,
        ]
        .into_iter();
        let mut bytes = Vec::new();
        pack_draw_inputs(&mut bytes, &draws, |_| meshes.next().flatten());
        assert_eq!(bytes.len(), 4 * DRAW_INPUT_SIZE);
        // Skinned, без AABB, с NaN-матрицей и отсутствующий меш: проверка отключена.
        for index in 0..4 {
            assert_eq!(u32_at(&bytes, index, 100), 0, "draw {index}");
        }
        assert_eq!(u32_at(&bytes, 3, 96), 0);
    }

    #[test]
    fn packing_reuses_the_buffer_for_a_new_frame() {
        let mut bytes = Vec::new();
        pack_draw_inputs(&mut bytes, &[draw(IDENTITY); 3], |_| {
            Some((3, Some(BOUNDS)))
        });
        pack_draw_inputs(&mut bytes, &[draw(IDENTITY)], |_| Some((3, Some(BOUNDS))));
        assert_eq!(bytes.len(), DRAW_INPUT_SIZE);
    }
}
