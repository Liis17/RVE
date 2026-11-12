//! Auto exposure: гистограмма log2-яркости HDR-источника и адаптация EV вида.
//!
//! Буфер экспозиции (гистограмма и адаптированный EV) принадлежит виду, а не кадровому слоту:
//! адаптация идёт по кадрам вида подряд, даже когда кадры двух слотов перекрываются на GPU.
//! Порядок между кадрами одного вида задаёт барьер в начале записи экспозиции: его первая область
//! синхронизации охватывает команды, отправленные раньше на ту же очередь (все submit renderer
//! идут через одну graphics queue). Поэтому запись предыдущего кадра вида (compute-адаптация и
//! чтение composite/bloom) завершается до заполнения гистограммы. Состояние адаптации (`ExposureHistory`)
//! меняется только после успешного submit, поэтому неотправленный кадр не помечает вид адаптированным.
//! Гистограмма считается по HDR цели сцены в render-разрешении до temporal resolve, поэтому FSR/DLSS
//! получают экспозицию текущего кадра: усреднение пишет в буфер множитель `exp2(ev + adapted_ev)`,
//! который копируется в 1×1 изображение экспозиции апскейлера. Композит читает адаптированное
//! значение через binding 3 набора постобработки только в auto-режиме.

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
/// Буфер: гистограмма, адаптированный EV, множитель экспозиции для апскейлеров и два слова
/// резерва (совпадает с шейдерами).
pub(crate) const EXPOSURE_BUFFER_SIZE: usize = HISTOGRAM_BYTES + 4 * size_of::<f32>();
/// Смещение множителя `exp2(ev + adapted_ev)`; кратно 4, как требует копирование в R32_SFLOAT.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) const EXPOSURE_SCALE_OFFSET: u64 = (HISTOGRAM_BYTES + size_of::<f32>()) as u64;
/// Интервал между кадрами, после которого кадр считается паузой (свёрнутое окно, загрузка,
/// модальный цикл resize), а не обычным шагом: адаптация берёт шаг предыдущего кадра.
const MAX_ADAPTATION_STEP_MS: f32 = 250.0;
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
        // SAFETY: the set layout and the ExposureAverageParams compute range are valid for both shaders.
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
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub(crate) struct ExposureHistory {
    /// Последний отправленный кадр вида, если он адаптировал экспозицию; иначе следующий
    /// auto-кадр делает snap.
    last: Option<ExposureFrame>,
}

/// Решение адаптации для auto-кадра вида.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ExposureFrame {
    /// Адаптация стартует с текущей экспозиции, а не с прошлого значения буфера.
    pub(crate) snap: bool,
    /// Render-разрешение кадра, по которому считается гистограмма.
    extent: (u32, u32),
    /// Шаг адаптации в миллисекундах.
    pub(crate) delta_ms: f32,
}

impl ExposureHistory {
    /// Решение для кадра: `None`, если вид в ручном режиме и буфер не адаптируется.
    ///
    /// Snap происходит при явном `Frame::reset_history`, при первом адаптирующем кадре вида и
    /// после ручного кадра. Resize и паузы адаптацию не сбрасывают и не ускоряют: если
    /// render-разрешение изменилось или интервал `measured_delta_ms` длиннее
    /// `MAX_ADAPTATION_STEP_MS`, кадр берёт шаг предыдущего адаптирующего кадра, чтобы время
    /// простоя не превращалось в скачок экспозиции.
    pub(crate) fn plan_frame(
        self,
        auto: bool,
        explicit_reset: bool,
        extent: (u32, u32),
        measured_delta_ms: f32,
    ) -> Option<ExposureFrame> {
        if !auto {
            return None;
        }
        let measured_delta_ms = measured_delta_ms.max(0.0);
        let frame = match self.last {
            None => ExposureFrame {
                snap: true,
                extent,
                delta_ms: measured_delta_ms,
            },
            Some(last) => {
                let interrupted =
                    last.extent != extent || measured_delta_ms > MAX_ADAPTATION_STEP_MS;
                ExposureFrame {
                    snap: explicit_reset,
                    extent,
                    delta_ms: if interrupted {
                        last.delta_ms
                    } else {
                        measured_delta_ms
                    },
                }
            }
        };
        Some(frame)
    }

    /// Фиксирует отправленный кадр вида; вызывается только после успешного `queue_submit2`.
    /// `frame` — решение этого кадра, `None` для ручного кадра.
    pub(crate) fn submitted(&mut self, frame: Option<ExposureFrame>) {
        self.last = frame;
    }
}

/// Буфер экспозиции вида; создаётся при первом кадре вида.
pub(crate) struct ExposureResources {
    buffer: DeviceBuffer,
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
            // TRANSFER_SRC — копирование множителя в изображение экспозиции FSR/DLSS.
            vk::BufferUsageFlags::STORAGE_BUFFER
                | vk::BufferUsageFlags::TRANSFER_DST
                | vk::BufferUsageFlags::TRANSFER_SRC,
            EXPOSURE_BUFFER_SIZE,
        )?;
        Ok(Self { buffer })
    }
}

/// Экспозиция вида для одного кадра: буфер для composite и адаптация, если кадр auto.
#[derive(Clone, Copy)]
pub(crate) struct ViewExposure {
    pub(crate) buffer: vk::Buffer,
    /// `None` — вид в ручном режиме; буфер в этом кадре не записывается и не читается.
    pub(crate) adaptation: Option<ExposureFrame>,
}

/// Готовит экспозицию вида: создаёт его ресурсы при первом кадре и решает адаптацию.
/// `render_extent` — разрешение HDR цели сцены, `measured_delta_ms` — интервал с прошлого кадра вида.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_view_exposure(
    instance: &ash::Instance,
    device: &ash::Device,
    physical_device: vk::PhysicalDevice,
    history: ExposureHistory,
    resources: &mut Option<ExposureResources>,
    auto: bool,
    explicit_reset: bool,
    render_extent: vk::Extent2D,
    measured_delta_ms: f32,
) -> Result<ViewExposure, RenderError> {
    if resources.is_none() {
        *resources = Some(ExposureResources::new(instance, device, physical_device)?);
    }
    let Some(resources) = resources.as_ref() else {
        return Err(RenderError::InvalidState(
            "ресурсы экспозиции вида отсутствуют",
        ));
    };
    Ok(ViewExposure {
        buffer: resources.buffer.buffer,
        adaptation: history.plan_frame(
            auto,
            explicit_reset,
            (render_extent.width, render_extent.height),
            measured_delta_ms,
        ),
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
) -> Option<ExposureRecord> {
    let pipelines = pipelines?;
    let adaptation = view.adaptation?;
    Some(ExposureRecord {
        set: target.set,
        buffer: view.buffer,
        pipelines,
        params: exposure_average_params(&post.exposure, adaptation.delta_ms, adaptation.snap),
    })
}

/// Записывает гистограмму и адаптацию; после записи буфер готов для composite и bloom
/// (fragment-чтение). Копирование множителя для апскейлера ставит собственный барьер.
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
    // Предыдущий кадр вида мог писать буфер в compute и читать его в fragment-стадиях; барьер
    // упорядочивает заполнение гистограммы после них (см. описание модуля).
    buffer_barrier(
        device,
        command_buffer,
        buffer,
        vk::PipelineStageFlags2::COMPUTE_SHADER | vk::PipelineStageFlags2::FRAGMENT_SHADER,
        vk::AccessFlags2::SHADER_STORAGE_READ | vk::AccessFlags2::SHADER_STORAGE_WRITE,
        vk::PipelineStageFlags2::ALL_TRANSFER,
        vk::AccessFlags2::TRANSFER_WRITE,
    );
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
    // SAFETY: the average pipeline uses the same layout and its push range is written in full.
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

pub(crate) fn buffer_barrier(
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

    const EXTENT: (u32, u32) = (1280, 720);
    const STEP_MS: f32 = 1000.0 / 60.0;

    fn plan(
        history: ExposureHistory,
        reset: bool,
        extent: (u32, u32),
        delta: f32,
    ) -> ExposureFrame {
        history
            .plan_frame(true, reset, extent, delta)
            .expect("auto frame plans adaptation")
    }

    /// Адаптирующий кадр, отправленный успешно: вид адаптирован.
    fn submitted_auto(history: &mut ExposureHistory, extent: (u32, u32), delta: f32) {
        let frame = plan(*history, false, extent, delta);
        history.submitted(Some(frame));
    }

    #[test]
    fn upscaler_scale_follows_adapted_value_and_is_texel_aligned() {
        assert_eq!(EXPOSURE_SCALE_OFFSET, 1028);
        assert_eq!(EXPOSURE_SCALE_OFFSET % 4, 0);
        assert!(EXPOSURE_SCALE_OFFSET + 4 <= EXPOSURE_BUFFER_SIZE as u64);
    }

    #[test]
    fn first_auto_frame_of_view_snaps() {
        let frame = plan(ExposureHistory::default(), false, EXTENT, STEP_MS);
        assert!(frame.snap);
        assert_eq!(frame.delta_ms, STEP_MS);
    }

    #[test]
    fn manual_frame_does_not_plan_adaptation() {
        let mut history = ExposureHistory::default();
        assert_eq!(history.plan_frame(false, false, EXTENT, STEP_MS), None);
        history.submitted(None);
        assert_eq!(history.plan_frame(false, true, EXTENT, STEP_MS), None);
    }

    #[test]
    fn next_auto_frame_adapts_from_previous_value_with_measured_step() {
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history, EXTENT, STEP_MS);
        let frame = plan(history, false, EXTENT, 33.0);
        assert!(!frame.snap);
        assert_eq!(frame.delta_ms, 33.0);
    }

    #[test]
    fn explicit_reset_snaps_even_after_adaptation() {
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history, EXTENT, STEP_MS);
        assert!(plan(history, true, EXTENT, STEP_MS).snap);
    }

    #[test]
    fn switching_from_manual_to_auto_snaps() {
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history, EXTENT, STEP_MS);
        history.submitted(None);
        assert!(plan(history, false, EXTENT, STEP_MS).snap);
    }

    #[test]
    fn failed_submit_keeps_view_unadapted_so_next_frame_snaps() {
        let history = ExposureHistory::default();
        // Кадр спланирован, но submit не выполнен: commit не вызывается.
        assert!(plan(history, false, EXTENT, STEP_MS).snap);
        assert!(plan(history, false, EXTENT, STEP_MS).snap);
    }

    #[test]
    fn resize_continues_adaptation_with_previous_step_instead_of_the_stall() {
        // Регрессия: после resize первый кадр получал интервал простоя (до 1 с) и почти целиком
        // прыгал к новой цели. Теперь адаптация продолжается без snap и с прежним шагом.
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history, EXTENT, STEP_MS);
        submitted_auto(&mut history, EXTENT, STEP_MS);
        let resized = plan(history, false, (1920, 1080), 1000.0);
        assert!(!resized.snap);
        assert_eq!(resized.delta_ms, STEP_MS);
        history.submitted(Some(resized));
        let next = plan(history, false, (1920, 1080), 20.0);
        assert_eq!(next.delta_ms, 20.0);
    }

    #[test]
    fn resize_with_a_normal_interval_still_uses_previous_step() {
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history, EXTENT, STEP_MS);
        let resized = plan(history, false, (640, 360), 40.0);
        assert!(!resized.snap);
        assert_eq!(resized.delta_ms, STEP_MS);
    }

    #[test]
    fn pause_longer_than_the_limit_uses_previous_step() {
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history, EXTENT, 20.0);
        assert_eq!(
            plan(history, false, EXTENT, MAX_ADAPTATION_STEP_MS).delta_ms,
            MAX_ADAPTATION_STEP_MS
        );
        assert_eq!(plan(history, false, EXTENT, 900.0).delta_ms, 20.0);
    }

    #[test]
    fn consistently_slow_view_keeps_its_own_step_after_a_snap() {
        // Вид, который стабильно рендерится медленнее лимита, адаптируется по своему шагу.
        let mut history = ExposureHistory::default();
        submitted_auto(&mut history, EXTENT, 400.0);
        assert_eq!(plan(history, false, EXTENT, 400.0).delta_ms, 400.0);
    }
}
