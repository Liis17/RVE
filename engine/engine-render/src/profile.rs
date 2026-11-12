//! GPU-проходы, CPU/GPU-тайминги кадра, статистика отрисовки и timestamp-запросы.

use crate::*;

/// Проход, границы которого записываются в GPU-профайлер; порядок совпадает с выполнением в кадре.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GpuPass {
    /// Compute кластеризации света.
    Clusters,
    /// Каскады directional shadow.
    ShadowDirectional,
    /// Локальные теневые слои.
    ShadowLocal,
    /// Depth prepass непрозрачных мешей вместе с Hi-Z и тестом occlusion culling;
    /// отсутствует, если выключены и AO, и occlusion culling.
    DepthPrepass,
    /// GTAO и denoise; отсутствует, если AO выключен.
    AmbientOcclusion,
    /// Непрозрачные и маскированные меши.
    Opaque,
    /// Skybox.
    Skybox,
    /// Полупрозрачные меши.
    Transparent,
    /// Отладочный wireframe.
    Wireframe,
    /// Частицы: spawn, симуляция и аддитивные billboards.
    Particles,
    /// Адаптация экспозиции по гистограмме HDR цели сцены до temporal resolve; отсутствует при
    /// ручной экспозиции.
    Exposure,
    /// TAA, FSR или DLSS; отсутствует, если временное сглаживание выключено.
    TemporalResolve,
    /// Экранные шейдеры до motion blur и DoF; отсутствуют без пользовательских шейдеров.
    ScreenShaderBefore,
    /// Motion blur и depth of field; отсутствуют, если эффекты выключены.
    Filters,
    /// Bloom: prefilter, downsample и upsample; отсутствует, если эффект выключен.
    Bloom,
    /// Composite: tonemap до вывода на swapchain.
    Tonemap,
    /// Экранные шейдеры после composite, до output-прохода; отсутствуют без пользовательских шейдеров.
    ScreenShaderAfter,
}

impl GpuPass {
    /// Число проходов, для которых записываются границы.
    pub const COUNT: usize = 17;
    /// Все проходы в порядке выполнения.
    pub const ALL: [Self; Self::COUNT] = [
        Self::Clusters,
        Self::ShadowDirectional,
        Self::ShadowLocal,
        Self::DepthPrepass,
        Self::AmbientOcclusion,
        Self::Opaque,
        Self::Skybox,
        Self::Transparent,
        Self::Wireframe,
        Self::Particles,
        Self::Exposure,
        Self::TemporalResolve,
        Self::ScreenShaderBefore,
        Self::Filters,
        Self::Bloom,
        Self::Tonemap,
        Self::ScreenShaderAfter,
    ];

    /// Короткое имя для отчётов.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Clusters => "Clusters",
            Self::ShadowDirectional => "Shadow directional",
            Self::ShadowLocal => "Shadow local",
            Self::DepthPrepass => "Depth prepass",
            Self::AmbientOcclusion => "Ambient occlusion",
            Self::Opaque => "Opaque",
            Self::Skybox => "Skybox",
            Self::Transparent => "Transparent",
            Self::Wireframe => "Wireframe",
            Self::Particles => "Particles",
            Self::TemporalResolve => "Temporal resolve",
            Self::ScreenShaderBefore => "Screen shaders before post",
            Self::Filters => "Motion blur and DoF",
            Self::Exposure => "Exposure",
            Self::Bloom => "Bloom",
            Self::Tonemap => "Tonemap",
            Self::ScreenShaderAfter => "Screen shaders after post",
        }
    }
}

/// Вид, которому принадлежат GPU-границы: основной экран или viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfiledView {
    Screen,
    Viewport(ViewportId),
}

/// CPU-фазы одного вызова `render_frame*`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderCpuTimings {
    /// От начала вызова до ожидания fence: проверки, подготовка кадра и окружения.
    pub prepare: Duration,
    pub fence_wait: Duration,
    /// Опрос и отправка загрузок ресурсов.
    pub uploads: Duration,
    pub acquire: Duration,
    /// Запись command buffer.
    pub record: Duration,
    /// Сброс fence и `vkQueueSubmit2`.
    pub submit: Duration,
    /// `vkQueuePresentKHR`.
    pub present: Duration,
}

/// GPU-время одного вида. `passes[pass as usize]` — `None`, если проход не выполнялся в кадре.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GpuViewTimings {
    pub key: ProfiledView,
    pub passes: [Option<Duration>; GpuPass::COUNT],
    /// От начала вида до последней записанной границы. Сумма `passes` совпадает с ним
    /// с округлением до наносекунды на каждый проход.
    pub total: Duration,
}

/// GPU-время кадра: виды в порядке записи и вывод на swapchain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpuFrameTimings {
    pub views: [Option<GpuViewTimings>; MAX_PROFILED_VIEWS],
    /// Вывод на swapchain (Output и UI). В `total` видов не входит.
    pub output_ui: Option<Duration>,
}

impl GpuFrameTimings {
    /// Total первого записанного вида: основного экрана или первого viewport.
    pub(crate) fn first_view_total(&self) -> Option<Duration> {
        self.views[0].map(|view| view.total)
    }
}

/// Профиль последнего вызова `render_frame*`. GPU-часть прочитана в начале вызова
/// из прошлой отправки того же слота, поэтому относится к более раннему кадру.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameProfile {
    pub cpu: RenderCpuTimings,
    pub gpu: Option<GpuFrameTimings>,
    /// Draw calls и отсечение мешей, записанные этим вызовом.
    pub draws: DrawStats,
}

/// Счётчики mesh draw'ов одного вызова `render_frame*`, суммарно по видам и проходам.
///
/// Draw одного [`DrawCommand`] считается в каждом проходе, который его записал или отсёк:
/// depth prepass, opaque/mask, blend и wireframe — в `scene_*`, каждый каскад и слой
/// локальной тени — в `shadow_*`. Skinned draw'ы не отсекаются и попадают только в `*_draw_calls`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawStats {
    /// Записанные `vkCmdDrawIndexed` мешей в проходах сцены.
    pub scene_draw_calls: u32,
    /// Draw'ы, пропущенные проходами сцены по frustum камеры.
    pub scene_culled: u32,
    /// Записанные `vkCmdDrawIndexed` в directional и local shadow проходах.
    pub shadow_draw_calls: u32,
    /// Shadow casters, пропущенные по frustum источника.
    pub shadow_culled: u32,
}

impl DrawStats {
    /// Подписи значений [`DrawStats::values`] для профайлеров.
    pub const LABELS: [&'static str; 4] = [
        "scene draw calls",
        "scene culled",
        "shadow draw calls",
        "shadow culled",
    ];

    /// Значения в порядке [`DrawStats::LABELS`].
    pub fn values(&self) -> [u32; 4] {
        [
            self.scene_draw_calls,
            self.scene_culled,
            self.shadow_draw_calls,
            self.shadow_culled,
        ]
    }
}

/// Запросы одного вида или вывода в пуле слота: `first` — начало, далее границы проходов.
#[derive(Clone, Copy)]
pub(crate) struct GpuMarks {
    pub(crate) pool: vk::QueryPool,
    pub(crate) first: u32,
}

impl GpuMarks {
    /// Пишет граничный timestamp `first + query` с стадией `ALL_COMMANDS`.
    /// Вызывать только вне `cmd_begin_rendering`…`cmd_end_rendering`.
    pub(crate) fn write(self, device: &ash::Device, command_buffer: vk::CommandBuffer, query: u32) {
        // SAFETY: the slot pool was reset when this command buffer began, and the index lies in this view's range.
        unsafe {
            device.cmd_write_timestamp2(
                command_buffer,
                vk::PipelineStageFlags2::ALL_COMMANDS,
                self.pool,
                self.first + query,
            );
        }
    }
}

/// Записывает границу после прохода `pass`, если вид его выполняет.
pub(crate) fn mark_pass(
    device: &ash::Device,
    command_buffer: vk::CommandBuffer,
    marks: Option<GpuMarks>,
    pass: GpuPass,
) {
    if let Some(marks) = marks {
        marks.write(device, command_buffer, 1 + pass as u32);
    }
}

/// Виды слота в порядке записи; индекс вида задаёт диапазон его запросов.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct GpuProfileLayout {
    pub(crate) views: [Option<ProfiledView>; MAX_PROFILED_VIEWS],
    pub(crate) count: usize,
}

impl GpuProfileLayout {
    /// Выделяет запросы следующему виду; `None`, если лимит видов кадра исчерпан.
    pub(crate) fn add_view(&mut self, view: ProfiledView) -> Option<u32> {
        if self.count == MAX_PROFILED_VIEWS {
            return None;
        }
        self.views[self.count] = Some(view);
        let first = self.count as u32 * VIEW_QUERY_COUNT;
        self.count += 1;
        Some(first)
    }

    /// Разбирает прочитанные запросы слота. `None` — запрос не записан в кадр или не готов.
    pub(crate) fn decode(
        &self,
        queries: &[Option<u64>],
        valid_bits: u32,
        period_ns: f32,
    ) -> GpuFrameTimings {
        let mut timings = GpuFrameTimings::default();
        for (index, view) in self.views[..self.count].iter().enumerate() {
            let Some(view) = *view else {
                continue;
            };
            let first = index * VIEW_QUERY_COUNT as usize;
            let marks = &queries[first..first + VIEW_QUERY_COUNT as usize];
            timings.views[index] = decode_view(view, marks, valid_bits, period_ns);
        }
        let output = OUTPUT_QUERY_FIRST as usize;
        timings.output_ui = decode_output(&queries[output..output + 2], valid_bits, period_ns);
        timings
    }
}

/// Разбирает границы вида: `marks[0]` — начало, `marks[1 + pass]` — граница после прохода.
/// Пропущенная граница означает, что проход не выполнялся: его время попадает в следующий записанный.
pub(crate) fn decode_view(
    view: ProfiledView,
    marks: &[Option<u64>],
    valid_bits: u32,
    period_ns: f32,
) -> Option<GpuViewTimings> {
    let start = marks[0]?;
    let mut passes = [None; GpuPass::COUNT];
    let mut previous = start;
    for (pass, mark) in passes.iter_mut().zip(&marks[1..]) {
        if let Some(tick) = *mark {
            *pass = Some(timestamp_duration(previous, tick, valid_bits, period_ns));
            previous = tick;
        }
    }
    Some(GpuViewTimings {
        key: view,
        passes,
        total: timestamp_duration(start, previous, valid_bits, period_ns),
    })
}

/// Разбирает границы вывода на swapchain: начало и конец.
pub(crate) fn decode_output(
    marks: &[Option<u64>],
    valid_bits: u32,
    period_ns: f32,
) -> Option<Duration> {
    Some(timestamp_duration(
        marks[0]?, marks[1]?, valid_bits, period_ns,
    ))
}

/// Разбирает результаты `TYPE_64 | WITH_AVAILABILITY`: на запрос значение и флаг доступности.
/// Недоступный запрос даёт `None`, даже если в буфере осталось значение прошлой записи.
pub(crate) fn available_timestamps<const N: usize>(results: &[[u64; 2]; N]) -> [Option<u64>; N] {
    results.map(|[tick, available]| (available != 0).then_some(tick))
}

/// Длительность между timestamp с учётом значащих бит и переполнения счётчика.
pub(crate) fn timestamp_duration(
    start: u64,
    end: u64,
    valid_bits: u32,
    period_ns: f32,
) -> Duration {
    let mask = if valid_bits >= 64 {
        u64::MAX
    } else {
        (1_u64 << valid_bits) - 1
    };
    let ticks = end.wrapping_sub(start) & mask;
    Duration::from_nanos((ticks as f64 * f64::from(period_ns)) as u64)
}
