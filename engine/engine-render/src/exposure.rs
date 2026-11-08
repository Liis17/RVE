//! Auto exposure: гистограмма log2-яркости HDR-источника и адаптация EV вида.
//!
//! Буфер экспозиции (гистограмма и адаптированный EV) принадлежит цели вида и создаётся
//! вместе с ней. Композит читает адаптированное значение через binding 3 набора постобработки.
//! Гистограмма очищается в начале каждого кадра, а кадры двух слотов упорядочиваются семафором
//! экспозиции в submit (см. `Renderer::submit_and_present`), поэтому адаптация не гоняется
//! между кадрами, которые выполняются одновременно.

use super::bloom::ensure_sampled;
use super::post::{EXPOSURE_BINS, ExposureAverageParams, exposure_average_params};
use super::{
    DeviceBuffer, DeviceImage, GpuMarks, GpuPass, OwnedDescriptorPool, PostProcess, RenderError,
    create_compute_pipeline, mark_pass,
};
use ash::vk;

const HISTOGRAM_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/exposure_histogram.comp.spv"));
const AVERAGE_SHADER: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/exposure_average.comp.spv"));

/// Размер гистограммы в байтах; очищается перед каждым кадром.
const HISTOGRAM_BYTES: usize = EXPOSURE_BINS * size_of::<u32>();
/// Буфер: гистограмма, адаптированный EV и три слова резерва (совпадает с шейдерами).
pub(crate) const EXPOSURE_BUFFER_SIZE: usize = HISTOGRAM_BYTES + 4 * size_of::<f32>();
/// Размер workgroup histogram-прохода: 16x16 пикселей, 256 бинов на группу.
const HISTOGRAM_GROUP_SIZE: u32 = 16;

/// Пайплайны экспозиции и их layout: binding 0 — HDR-источник, binding 1 — буфер экспозиции.
#[derive(Clone, Copy)]
pub(crate) struct ExposurePipelines {
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    histogram: vk::Pipeline,
    average: vk::Pipeline,
}

impl ExposurePipelines {
    pub(crate) fn new(device: &ash::Device, cache: vk::PipelineCache) -> Result<Self, RenderError> {
        let bindings = [
            vk::DescriptorSetLayoutBinding::default()
                .binding(0)
                .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
            vk::DescriptorSetLayoutBinding::default()
                .binding(1)
                .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1)
                .stage_flags(vk::ShaderStageFlags::COMPUTE),
        ];
        let set_layout_info = vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings);
        // SAFETY: the bindings match the histogram and average compute shaders.
        let set_layout = unsafe { device.create_descriptor_set_layout(&set_layout_info, None) }?;
        let set_layouts = [set_layout];
        let push_constant_ranges = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .offset(0)
            .size(size_of::<ExposureAverageParams>() as u32)];
        let layout_info = vk::PipelineLayoutCreateInfo::default()
            .set_layouts(&set_layouts)
            .push_constant_ranges(&push_constant_ranges);
        // SAFETY: the set layout and the 32-byte compute range are valid for both shaders.
        let layout = match unsafe { device.create_pipeline_layout(&layout_info, None) } {
            Ok(layout) => layout,
            Err(error) => {
                // SAFETY: the set layout was created above and nothing references it.
                unsafe { device.destroy_descriptor_set_layout(set_layout, None) };
                return Err(error.into());
            }
        };
        let histogram = create_compute_pipeline(device, cache, layout, HISTOGRAM_SHADER);
        let average = histogram
            .as_ref()
            .ok()
            .map(|_| create_compute_pipeline(device, cache, layout, AVERAGE_SHADER));
        match (histogram, average) {
            (Ok(histogram), Some(Ok(average))) => Ok(Self {
                set_layout,
                layout,
                histogram,
                average,
            }),
            (histogram, average) => {
                let pipelines = [histogram.ok(), average.and_then(Result::ok)];
                for pipeline in pipelines.into_iter().flatten() {
                    // SAFETY: the partially created pipelines belong to this device and are unused.
                    unsafe { device.destroy_pipeline(pipeline, None) };
                }
                // SAFETY: no pipeline references the layout or set layout after the cleanup above.
                unsafe {
                    device.destroy_pipeline_layout(layout, None);
                    device.destroy_descriptor_set_layout(set_layout, None);
                }
                Err(RenderError::InvalidState("пайплайны экспозиции не созданы"))
            }
        }
    }

    /// Освобождает пайплайны и layout; вызывается после ожидания простоя устройства.
    pub(crate) fn destroy(self, device: &ash::Device) {
        // SAFETY: the renderer waits for device idle before destroying exposure resources.
        unsafe {
            device.destroy_pipeline(self.histogram, None);
            device.destroy_pipeline(self.average, None);
            device.destroy_pipeline_layout(self.layout, None);
            device.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }

    /// Layout набора постобработки не использует; нужен только для создания наборов экспозиции.
    pub(crate) fn set_layout(&self) -> vk::DescriptorSetLayout {
        self.set_layout
    }
}

/// Буфер экспозиции и набор дескрипторов одной цели вида.
pub(crate) struct ExposureTarget {
    buffer: DeviceBuffer,
    _pool: OwnedDescriptorPool,
    set: vk::DescriptorSet,
    /// Адаптация уже выполнялась: следующий кадр адаптируется, а не делает snap.
    initialized: bool,
}

impl ExposureTarget {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
        set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RenderError> {
        let buffer = DeviceBuffer::new(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            EXPOSURE_BUFFER_SIZE,
        )?;
        let pool_sizes = [
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::SAMPLED_IMAGE)
                .descriptor_count(1),
            vk::DescriptorPoolSize::default()
                .ty(vk::DescriptorType::STORAGE_BUFFER)
                .descriptor_count(1),
        ];
        let pool_info = vk::DescriptorPoolCreateInfo::default()
            .max_sets(1)
            .pool_sizes(&pool_sizes);
        let pool = OwnedDescriptorPool::new(device, &pool_info)?;
        let layouts = [set_layout];
        let allocate_info = vk::DescriptorSetAllocateInfo::default()
            .descriptor_pool(pool.handle)
            .set_layouts(&layouts);
        // SAFETY: the pool is sized for exactly one set of this layout.
        let set = unsafe { device.allocate_descriptor_sets(&allocate_info) }?[0];
        let buffer_info = [vk::DescriptorBufferInfo::default()
            .buffer(buffer.buffer)
            .offset(0)
            .range(EXPOSURE_BUFFER_SIZE as u64)];
        let writes = [vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&buffer_info)];
        // SAFETY: the buffer and set are alive and the set is not used by in-flight work.
        unsafe { device.update_descriptor_sets(&writes, &[]) };
        Ok(Self {
            buffer,
            _pool: pool,
            set,
            initialized: false,
        })
    }

    pub(crate) fn buffer(&self) -> vk::Buffer {
        self.buffer.buffer
    }
}

/// Запись экспозиции для вида: существует, только если кадр адаптирует экспозицию.
pub(crate) struct ExposureRecord<'a> {
    pub(crate) target: &'a mut ExposureTarget,
    pub(crate) pipelines: ExposurePipelines,
    pub(crate) params: ExposureAverageParams,
}

/// Строит запись адаптации для вида; `None` — вид использует ручную экспозицию.
pub(crate) fn exposure_record<'a>(
    target: &'a mut ExposureTarget,
    pipelines: Option<ExposurePipelines>,
    auto: bool,
    post: &PostProcess,
    frame_delta_ms: f32,
    explicit_reset: bool,
) -> Option<ExposureRecord<'a>> {
    let pipelines = pipelines?;
    if !auto {
        return None;
    }
    let snap = explicit_reset || !target.initialized;
    Some(ExposureRecord {
        params: exposure_average_params(&post.exposure, frame_delta_ms, snap),
        target,
        pipelines,
    })
}

/// Записывает гистограмму и адаптацию; после записи буфер готов для composite.
pub(crate) fn record_exposure(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source: &DeviceImage,
    source_layout: &mut vk::ImageLayout,
    source_extent: vk::Extent2D,
    record: ExposureRecord<'_>,
    timestamps: Option<GpuMarks>,
) {
    ensure_sampled(device, command_buffer, source.image, source_layout);
    let ExposureRecord {
        target,
        pipelines,
        params,
    } = record;
    write_source_binding(device, target.set, source.view);
    let buffer = target.buffer.buffer;
    let set = [target.set];
    // SAFETY: the buffer is alive, the set and pipelines belong to this device and the
    // command buffer is in the recording state.
    unsafe {
        device.cmd_fill_buffer(command_buffer, buffer, 0, HISTOGRAM_BYTES as u64, 0);
    }
    buffer_barrier(
        device,
        command_buffer,
        buffer,
        vk::PipelineStageFlags2::ALL_TRANSFER,
        vk::AccessFlags2::TRANSFER_WRITE,
        vk::PipelineStageFlags2::COMPUTE_SHADER,
        vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
    );
    // SAFETY: the histogram pipeline and set use the same layout; bindings are written above.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            pipelines.histogram,
        );
        device.cmd_bind_descriptor_sets(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            pipelines.layout,
            0,
            &set,
            &[],
        );
        device.cmd_dispatch(
            command_buffer,
            source_extent.width.div_ceil(HISTOGRAM_GROUP_SIZE),
            source_extent.height.div_ceil(HISTOGRAM_GROUP_SIZE),
            1,
        );
    }
    buffer_barrier(
        device,
        command_buffer,
        buffer,
        vk::PipelineStageFlags2::COMPUTE_SHADER,
        vk::AccessFlags2::SHADER_STORAGE_WRITE,
        vk::PipelineStageFlags2::COMPUTE_SHADER,
        vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
    );
    let push = params.to_bytes();
    // SAFETY: the average pipeline uses the same layout and its 32-byte push range is written.
    unsafe {
        device.cmd_bind_pipeline(
            command_buffer,
            vk::PipelineBindPoint::COMPUTE,
            pipelines.average,
        );
        device.cmd_push_constants(
            command_buffer,
            pipelines.layout,
            vk::ShaderStageFlags::COMPUTE,
            0,
            &push,
        );
        device.cmd_dispatch(command_buffer, 1, 1, 1);
    }
    buffer_barrier(
        device,
        command_buffer,
        buffer,
        vk::PipelineStageFlags2::COMPUTE_SHADER,
        vk::AccessFlags2::SHADER_STORAGE_WRITE,
        vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_STORAGE_READ,
    );
    mark_pass(device, command_buffer, timestamps, GpuPass::Exposure);
    target.initialized = true;
}

/// Привязывает HDR-источник к binding 0 набора экспозиции; меняется каждый кадр.
fn write_source_binding(device: &ash::Device, set: vk::DescriptorSet, view: vk::ImageView) {
    let image_info = [vk::DescriptorImageInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(0)
        .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
        .image_info(&image_info)];
    // SAFETY: the set and view are alive and the set is not used by in-flight work.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

/// Привязывает буфер экспозиции к binding 3 набора постобработки (читает composite).
pub(crate) fn write_composite_exposure(
    device: &ash::Device,
    set: vk::DescriptorSet,
    buffer: vk::Buffer,
) {
    let buffer_info = [vk::DescriptorBufferInfo::default()
        .buffer(buffer)
        .offset(0)
        .range(EXPOSURE_BUFFER_SIZE as u64)];
    let writes = [vk::WriteDescriptorSet::default()
        .dst_set(set)
        .dst_binding(3)
        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
        .buffer_info(&buffer_info)];
    // SAFETY: the set and buffer are alive and the set is not used by in-flight work.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

fn buffer_barrier(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    buffer: vk::Buffer,
    source_stage: vk::PipelineStageFlags2,
    source_access: vk::AccessFlags2,
    destination_stage: vk::PipelineStageFlags2,
    destination_access: vk::AccessFlags2,
) {
    let barriers = [vk::BufferMemoryBarrier2::default()
        .src_stage_mask(source_stage)
        .src_access_mask(source_access)
        .dst_stage_mask(destination_stage)
        .dst_access_mask(destination_access)
        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
        .buffer(buffer)
        .offset(0)
        .size(vk::WHOLE_SIZE)];
    let dependency = vk::DependencyInfo::default().buffer_memory_barriers(&barriers);
    // SAFETY: the command buffer is recording and the barrier references a live buffer.
    unsafe { device.cmd_pipeline_barrier2(command_buffer, &dependency) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_layout_matches_histogram_and_adapted_value_offsets() {
        assert_eq!(HISTOGRAM_BYTES, 1024);
        assert_eq!(EXPOSURE_BUFFER_SIZE, 1040);
        // adapted_ev лежит сразу после гистограммы: композит читает его по этому смещению.
        assert_eq!(HISTOGRAM_BYTES, EXPOSURE_BUFFER_SIZE - 16);
    }
}
