//! Auto exposure: гистограмма log2-яркости HDR-источника и адаптация EV вида.
//!
//! Буфер экспозиции (гистограмма и адаптированный EV) и семафор принадлежат виду, а не
//! кадровому слоту: адаптация идёт по кадрам вида подряд, даже когда кадры двух слотов
//! перекрываются на GPU. Внутри кадра порядок задают барьеры. Между submit барьер не задаёт
//! порядок, поэтому кадр вида, адаптирующий экспозицию, ждёт семафор предыдущей адаптации этого
//! вида и сигналит свой (`Renderer::submit_and_present`). Состояние адаптации (`ExposureHistory`)
//! меняется только после успешного submit, поэтому неотправленный кадр не помечает вид адаптированным.
//! Композит читает адаптированное значение через binding 3 набора постобработки только в auto-режиме.

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

/// Состояние адаптации вида между кадрами. Меняется только после успешного submit кадра.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExposureHistory {
    /// Последний отправленный кадр вида адаптировал экспозицию; иначе следующий auto-кадр делает snap.
    adapted: bool,
    /// Семафор вида уже сигналился: следующий адаптирующий кадр обязан его дождаться.
    signaled: bool,
}

/// Решение адаптации для auto-кадра вида.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExposureFrame {
    /// Адаптация стартует с текущей экспозиции, а не с прошлого значения буфера.
    pub(crate) snap: bool,
    /// Кадр ждёт семафор предыдущей адаптации этого вида.
    pub(crate) wait: bool,
}

impl ExposureHistory {
    /// Решение для кадра: `None`, если вид в ручном режиме и буфер не адаптируется.
    ///
    /// Snap происходит при явном `Frame::reset_history`, при первом адаптирующем кадре вида и
    /// после ручного кадра. Изменение размера цели сюда не входит, поэтому адаптация его не сбрасывает.
    pub(crate) fn plan_frame(self, auto: bool, explicit_reset: bool) -> Option<ExposureFrame> {
        auto.then_some(ExposureFrame {
            snap: explicit_reset || !self.adapted,
            wait: self.signaled,
        })
    }

    /// Фиксирует отправленный кадр вида; вызывается только после успешного `queue_submit2`.
    pub(crate) fn submitted(&mut self, auto: bool) {
        self.adapted = auto;
        self.signaled |= auto;
    }
}

/// Буфер экспозиции вида и семафор, которым упорядочивается адаптация его кадров.
pub(crate) struct ExposureResources {
    device: ash::Device,
    buffer: DeviceBuffer,
    semaphore: vk::Semaphore,
}

impl ExposureResources {
    pub(crate) fn new(
        instance: &ash::Instance,
        device: &ash::Device,
        physical_device: vk::PhysicalDevice,
    ) -> Result<Self, RenderError> {
        let buffer = DeviceBuffer::new(
            instance,
            device,
            physical_device,
            vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST,
            EXPOSURE_BUFFER_SIZE,
        )?;
        // SAFETY: a default create info is valid for a new binary semaphore.
        let semaphore =
            unsafe { device.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?;
        Ok(Self {
            device: device.clone(),
            buffer,
            semaphore,
        })
    }
}

impl Drop for ExposureResources {
    fn drop(&mut self) {
        // SAFETY: the owner destroys these resources after device idle or after the view's
        // submissions completed, so no pending submission waits on or signals the semaphore.
        unsafe { self.device.destroy_semaphore(self.semaphore, None) };
    }
}

/// Семафор адаптации вида для одного submit: кадр ждёт предыдущий и сигналит свой.
#[derive(Clone, Copy)]
pub(crate) struct ExposureSync {
    pub(crate) semaphore: vk::Semaphore,
    pub(crate) wait: bool,
}

/// Адаптация кадра вида: решение по `ExposureHistory` и синхронизация submit.
#[derive(Clone, Copy)]
pub(crate) struct ExposureAdaptation {
    pub(crate) snap: bool,
    pub(crate) sync: ExposureSync,
}

/// Экспозиция вида для одного кадра: буфер для composite и адаптация, если кадр auto.
#[derive(Clone, Copy)]
pub(crate) struct ViewExposure {
    pub(crate) buffer: vk::Buffer,
    /// `None` — вид в ручном режиме; буфер в этом кадре не записывается и не читается.
    pub(crate) adaptation: Option<ExposureAdaptation>,
}

/// Готовит экспозицию вида: создаёт его ресурсы при первом кадре и решает адаптацию.
pub(crate) fn prepare_view_exposure(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    history: ExposureHistory,
    resources: &mut Option<ExposureResources>,
    auto: bool,
    explicit_reset: bool,
) -> Result<ViewExposure, RenderError> {
    if resources.is_none() {
        *resources = Some(ExposureResources::new(instance, device, physical_device)?);
    }
    let Some(resources) = resources.as_ref() else {
        return Err(RenderError::InvalidState(
            "ресурсы экспозиции вида отсутствуют",
        ));
    };
    let adaptation = history
        .plan_frame(auto, explicit_reset)
        .map(|frame| ExposureAdaptation {
            snap: frame.snap,
            sync: ExposureSync {
                semaphore: resources.semaphore,
                wait: frame.wait,
            },
        });
    Ok(ViewExposure {
        buffer: resources.buffer.buffer,
        adaptation,
    })
}

/// Набор дескрипторов одного кадрового слота цели вида; буфер подставляется при записи кадра.
pub(crate) struct ExposureTarget {
    _pool: OwnedDescriptorPool,
    set: vk::DescriptorSet,
}

impl ExposureTarget {
    pub(crate) fn new(
        device: &ash::Device,
        set_layout: vk::DescriptorSetLayout,
    ) -> Result<Self, RenderError> {
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
        Ok(Self { _pool: pool, set })
    }
}

/// Запись адаптации для кадра: пайплайны, набор слота, буфер вида и параметры.
pub(crate) struct ExposureRecord {
    set: vk::DescriptorSet,
    buffer: vk::Buffer,
    pipelines: ExposurePipelines,
    params: ExposureAverageParams,
}

/// Строит запись адаптации; `None` — кадр ручной или пайплайны отсутствуют.
pub(crate) fn exposure_record(
    target: &ExposureTarget,
    pipelines: Option<ExposurePipelines>,
    view: ViewExposure,
    post: &PostProcess,
    frame_delta_ms: f32,
) -> Option<ExposureRecord> {
    let pipelines = pipelines?;
    let adaptation = view.adaptation?;
    Some(ExposureRecord {
        set: target.set,
        buffer: view.buffer,
        pipelines,
        params: exposure_average_params(&post.exposure, frame_delta_ms, adaptation.snap),
    })
}

/// Записывает гистограмму и адаптацию; после записи буфер готов для composite.
///
/// Вызывается только при успешной записи кадра; `ExposureHistory` обновляется после submit.
pub(crate) fn record_exposure(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    source: &DeviceImage,
    source_layout: &mut vk::ImageLayout,
    source_extent: vk::Extent2D,
    record: ExposureRecord,
    timestamps: Option<GpuMarks>,
) {
    ensure_sampled(device, command_buffer, source.image, source_layout);
    let ExposureRecord {
        set,
        buffer,
        pipelines,
        params,
    } = record;
    write_exposure_bindings(device, set, source.view, buffer);
    let set = [set];
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
}

/// Привязывает HDR-источник (binding 0) и буфер вида (binding 1) к набору слота; меняется каждый кадр.
fn write_exposure_bindings(
    device: &ash::Device,
    set: vk::DescriptorSet,
    view: vk::ImageView,
    buffer: vk::Buffer,
) {
    let image_info = [vk::DescriptorImageInfo::default()
        .image_view(view)
        .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
    let buffer_info = [vk::DescriptorBufferInfo::default()
        .buffer(buffer)
        .offset(0)
        .range(EXPOSURE_BUFFER_SIZE as u64)];
    let writes = [
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::SAMPLED_IMAGE)
            .image_info(&image_info),
        vk::WriteDescriptorSet::default()
            .dst_set(set)
            .dst_binding(1)
            .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
            .buffer_info(&buffer_info),
    ];
    // SAFETY: the set, view and buffer are alive and the set is not used by in-flight work.
    unsafe { device.update_descriptor_sets(&writes, &[]) };
}

/// Привязывает буфер экспозиции вида к binding 3 набора постобработки (читает composite).
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

    /// Адаптирующий кадр, отправленный успешно: вид адаптирован и семафор сигналится.
    fn submitted_auto(history: &mut ExposureHistory) {
        assert!(history.plan_frame(true, false).is_some());
        history.submitted(true);
    }

    #[test]
    fn first_auto_frame_of_view_snaps_without_waiting() {
        let history = ExposureHistory::default();
        let frame = history
            .plan_frame(true, false)
            .expect("auto frame plans adaptation");
        assert!(frame.snap);
        assert!(!frame.wait);
    }

    #[test]
    fn manual_frame_does_not_plan_adaptation() {
        let mut history = ExposureHistory::default();
        assert_eq!(history.plan_frame(false, false), None);
        history.submitted(false);
        assert_eq!(history.plan_frame(false, true), None);
    }

    #[test]
    fn next_auto_frame_adapts_from_previous_value_and_waits_for_it() {
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history);
        let frame = history
            .plan_frame(true, false)
            .expect("auto frame plans adaptation");
        assert!(!frame.snap);
        assert!(frame.wait);
    }

    #[test]
    fn explicit_reset_snaps_even_after_adaptation() {
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history);
        let frame = history
            .plan_frame(true, true)
            .expect("auto frame plans adaptation");
        assert!(frame.snap);
        assert!(frame.wait);
    }

    #[test]
    fn switching_from_manual_to_auto_snaps_but_still_waits_for_pending_signal() {
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history);
        history.submitted(false);
        let frame = history
            .plan_frame(true, false)
            .expect("auto frame plans adaptation");
        assert!(frame.snap);
        assert!(frame.wait);
    }

    #[test]
    fn failed_submit_keeps_view_unadapted_so_next_frame_snaps() {
        let history = ExposureHistory::default();
        // Кадр спланирован, но submit не выполнен: commit не вызывается.
        let planned = history
            .plan_frame(true, false)
            .expect("auto frame plans adaptation");
        assert!(planned.snap);
        let retried = history
            .plan_frame(true, false)
            .expect("auto frame plans adaptation");
        assert!(retried.snap);
        assert!(!retried.wait);
    }

    #[test]
    fn resize_does_not_reset_adaptation_because_it_is_not_an_input() {
        // Пересоздание цели не передаёт сюда ничего: без явного reset адаптация продолжается.
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history);
        submitted_auto(&mut history);
        let frame = history
            .plan_frame(true, false)
            .expect("auto frame plans adaptation");
        assert!(!frame.snap);
    }
}
